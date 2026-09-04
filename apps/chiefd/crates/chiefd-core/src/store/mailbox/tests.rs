//! Mailbox store + wake-scan tests. Real `Ledgers`, an injected clock, a
//! recording fake waker — no mocks of the store itself and no sleeps.

use std::cell::RefCell;

use super::*;
use crate::clock::WallMillis;
use crate::ledger::Ledgers;

const EPOCH: i64 = 1_784_116_800_000; // 2026-07-15T12:00:00.000Z

fn ledgers() -> Ledgers {
    Ledgers::empty(WallMillis(EPOCH))
}

fn envelope(id: &str, created_at: &str, recipients: &[&str]) -> MailboxEnvelope {
    let recipients: Vec<String> = recipients.iter().map(|s| (*s).to_string()).collect();
    MailboxEnvelope {
        schema_version: MAILBOX_ENVELOPE_SCHEMA_VERSION,
        id: id.to_string(),
        organization: "cobalt".to_string(),
        from_person_id: "launcher".to_string(),
        to: recipients.first().cloned().unwrap_or_default(),
        recipients,
        body: format!("body-{id}"),
        urgency: Urgency::Normal,
        reply_to: None,
        health_incident: None,
        created_at: created_at.to_string(),
    }
}

/// A waker that records every call and can be told to fail.
#[derive(Default)]
struct RecordingWaker {
    wake_calls: RefCell<Vec<Vec<String>>>,
    wake_fails: bool,
}

impl RuntimeWaker for RecordingWaker {
    fn wake(&self, recipients: &[String]) -> Vec<String> {
        self.wake_calls.borrow_mut().push(recipients.to_vec());
        if self.wake_fails {
            Vec::new()
        } else {
            recipients.to_vec()
        }
    }
}

/// A decider that coalesces exactly the named people.
struct InFlight(std::collections::HashSet<String>);

impl WakeDecider for InFlight {
    fn reconcile_in_flight(&self, person_id: &str) -> bool {
        self.0.contains(person_id)
    }
}

#[test]
fn inbox_visibility_and_launch_demand_are_different_typed_rules() {
    assert!(MailboxState::Pending.is_inbox_message());
    assert!(MailboxState::Pending.supplies_launch_demand());
    assert!(MailboxState::Delivered.is_inbox_message());
    assert!(!MailboxState::Delivered.supplies_launch_demand());
    for state in [
        MailboxState::Accepted,
        MailboxState::Superseded,
        MailboxState::Rejected,
        MailboxState::Resolved,
    ] {
        assert!(!state.is_inbox_message(), "{state:?} is settled");
        assert!(!state.supplies_launch_demand(), "{state:?} cannot launch a person");
    }
}

// --- enqueue: durable, idempotent, content-fenced ---------------------------

#[test]
fn enqueue_stages_one_pending_row_per_recipient() {
    let mut l = ledgers();
    let recipients =
        enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob", "carol"]))
            .expect("enqueue");
    assert_eq!(recipients, vec!["bob".to_string(), "carol".to_string()]);
    assert_eq!(pending_for(&l, "bob").len(), 1);
    assert_eq!(pending_for(&l, "carol").len(), 1);
    assert_eq!(pending_for(&l, "bob")[0].id, "e1");
    // A recipient named twice still gets exactly one row.
    let mut l2 = ledgers();
    enqueue(&mut l2, &envelope("e2", "2026-07-15T12:00:00.000Z", &["bob", "bob"])).expect("dedup");
    assert_eq!(pending_for(&l2, "bob").len(), 1);
}

#[test]
fn a_repeated_publish_of_the_same_envelope_is_an_idempotent_no_op() {
    let mut l = ledgers();
    let env = envelope("e1", "2026-07-15T12:00:00.000Z", &["bob"]);
    enqueue(&mut l, &env).expect("first");
    // Re-publishing the exact envelope (a crash-and-retry) writes no new row and
    // still reports the recipient as pending (so the wake set is complete).
    let again = enqueue(&mut l, &env).expect("replay");
    assert_eq!(again, vec!["bob".to_string()]);
    assert_eq!(pending_for(&l, "bob").len(), 1, "no duplicate row");
}

#[test]
fn a_replay_with_a_different_created_at_is_still_one_idempotent_row() {
    // THE crux crash-safety property. The two commits (the sink's stage, then the
    // scheduler's mark_delivered) are not atomic, so a crash between them makes
    // the next wake pass re-present the SAME effect id — rendered at a LATER time,
    // hence a different createdAt. Because the row identity is time-independent
    // (`id@person`, not `createdAt`-`id@person`), this is a no-op success, never a
    // duplicate and never a conflict.
    let mut l = ledgers();
    enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob"])).expect("first");
    let replay = enqueue(&mut l, &envelope("e1", "2026-07-15T12:30:00.000Z", &["bob"]))
        .expect("a later-time replay is a no-op success, never a conflict");
    assert_eq!(replay, vec!["bob".to_string()], "still pending, so still wakeable");
    assert_eq!(pending_for(&l, "bob").len(), 1, "one row across the retry, no duplicate");
    // The row was left exactly as-is — the original createdAt survives, never
    // rewritten by the replay.
    assert_eq!(pending_for(&l, "bob")[0].created_at, "2026-07-15T12:00:00.000Z");
}

#[test]
fn an_already_accepted_row_is_not_resurrected_by_a_replay() {
    let mut l = ledgers();
    let env = envelope("e1", "2026-07-15T12:00:00.000Z", &["bob"]);
    enqueue(&mut l, &env).expect("first");
    assert!(archive(&mut l, &env.row_id("bob"), MailboxState::Accepted));
    // Bob's pane already drained it. Re-presenting the same effect must NOT
    // move it back to pending, and must NOT report bob as needing a wake.
    let replay = enqueue(&mut l, &env).expect("replay after accept");
    assert!(replay.is_empty(), "an accepted envelope is not re-woken");
    assert!(pending_for(&l, "bob").is_empty());
}

// --- pending_for ordering ---------------------------------------------------

#[test]
fn pending_for_is_ordered_by_creation_and_ignores_other_recipients() {
    let mut l = ledgers();
    enqueue(&mut l, &envelope("late", "2026-07-15T12:05:00.000Z", &["bob"])).expect("late");
    enqueue(&mut l, &envelope("early", "2026-07-15T12:00:00.000Z", &["bob"])).expect("early");
    enqueue(&mut l, &envelope("other", "2026-07-15T12:01:00.000Z", &["carol"])).expect("carol");
    let ids: Vec<String> = pending_for(&l, "bob").into_iter().map(|e| e.id).collect();
    assert_eq!(ids, vec!["early".to_string(), "late".to_string()], "oldest first, bob only");
}

// --- archive: pending -> terminal, idempotent -------------------------------

#[test]
fn archive_moves_pending_to_terminal_once_and_is_idempotent() {
    let mut l = ledgers();
    let env = envelope("e1", "2026-07-15T12:00:00.000Z", &["bob"]);
    enqueue(&mut l, &env).expect("enqueue");
    let row_id = env.row_id("bob");
    assert!(archive(&mut l, &row_id, MailboxState::Accepted), "first archive moves it");
    assert!(pending_for(&l, "bob").is_empty(), "no longer pending");
    assert!(!archive(&mut l, &row_id, MailboxState::Accepted), "already terminal → no-op");
    // Archiving to a non-terminal target is rejected outright.
    let env2 = envelope("e2", "2026-07-15T12:00:00.000Z", &["bob"]);
    enqueue(&mut l, &env2).expect("enqueue");
    assert!(
        !archive(&mut l, &env2.row_id("bob"), MailboxState::Pending),
        "cannot archive to pending"
    );
    assert_eq!(pending_for(&l, "bob").len(), 1, "still pending");
}

#[test]
fn archiving_an_absent_row_is_false_not_a_panic() {
    let mut l = ledgers();
    assert!(!archive(&mut l, "does-not-exist@bob", MailboxState::Accepted));
}

// --- pending_recipients -----------------------------------------------------

#[test]
fn pending_recipients_deduplicates_and_drops_the_drained() {
    let mut l = ledgers();
    enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob", "carol"])).expect("e1");
    enqueue(&mut l, &envelope("e2", "2026-07-15T12:01:00.000Z", &["bob"])).expect("e2");
    let mut recips: Vec<String> = pending_recipients(&l).into_iter().collect();
    recips.sort();
    assert_eq!(recips, vec!["bob".to_string(), "carol".to_string()]);
    // Drain carol; bob still has two pending, carol has none.
    archive(
        &mut l,
        &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob", "carol"]).row_id("carol"),
        MailboxState::Accepted,
    );
    assert_eq!(pending_recipients(&l), ["bob".to_string()].into_iter().collect());
}

// --- the wake scan (duty #8) -----------------------------------------------

#[test]
fn the_wake_scan_wakes_everyone_pending_by_default() {
    let mut l = ledgers();
    enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob", "carol"])).expect("e1");
    let plan = pending_mailbox_wake_scan(&l, &WakeEveryone);
    assert_eq!(plan.wake, vec!["bob".to_string(), "carol".to_string()]);
    assert!(plan.coalesced.is_empty());
}

#[test]
fn a_recipient_with_an_in_flight_reconcile_is_coalesced_not_re_woken() {
    let mut l = ledgers();
    enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob", "carol"])).expect("e1");
    let plan =
        pending_mailbox_wake_scan(&l, &InFlight(["carol"].into_iter().map(String::from).collect()));
    assert_eq!(plan.wake, vec!["bob".to_string()], "bob still needs waking");
    assert_eq!(plan.coalesced, vec!["carol".to_string()], "carol coalesced onto the reconcile");
}

#[test]
fn running_the_wake_pass_wakes_the_pending_and_reports_the_woken() {
    let mut l = ledgers();
    enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob"])).expect("e1");
    let waker = RecordingWaker::default();
    let outcome = run_pending_mailbox_wake(&l, &WakeEveryone, &waker);
    assert_eq!(outcome.woken, vec!["bob".to_string()]);
    assert_eq!(*waker.wake_calls.borrow(), vec![vec!["bob".to_string()]], "exactly one wake call");
}

#[test]
fn an_empty_mailbox_wakes_nobody_and_makes_no_host_call() {
    let l = ledgers();
    let waker = RecordingWaker::default();
    let outcome = run_pending_mailbox_wake(&l, &WakeEveryone, &waker);
    assert!(outcome.woken.is_empty());
    assert!(waker.wake_calls.borrow().is_empty(), "no pending mail → no host wake");
}

#[test]
fn a_failed_wake_is_absent_from_woken_but_never_an_error() {
    let mut l = ledgers();
    enqueue(&mut l, &envelope("e1", "2026-07-15T12:00:00.000Z", &["bob"])).expect("e1");
    let waker = RecordingWaker { wake_fails: true, ..RecordingWaker::default() };
    let outcome = run_pending_mailbox_wake(&l, &WakeEveryone, &waker);
    assert!(outcome.woken.is_empty(), "the wake failed, so bob is not reported woken");
    // The envelope is untouched and still pending — the scan will re-drive it.
    assert_eq!(pending_for(&l, "bob").len(), 1, "a failed wake never touches durable mail");
}
