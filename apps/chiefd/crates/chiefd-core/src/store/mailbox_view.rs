//! Read/settle helpers that finish the mailbox row port started by
//! [`crate::store::mailbox_rows`] (org-data-normalization P0, N-mailbox).
//!
//! The retired TypeScript mailbox store carried its read/mutate surface on the
//! same object as its primitives; here the two are split. This module owns the
//! part [`crate::store::mailbox`] (the durable enqueue/archive/wake primitives,
//! operating over the in-memory `Ledgers` snapshot) and
//! [`crate::store::mailbox_rows`] (whole-company reconstruct/publish, and the
//! fence-free per-person `delta`) do not: locating one envelope by its logical
//! message id across a person's buckets, settling one or many pending rows by
//! row id, dropping a departed person's whole mailbox, and a read-only
//! five-bucket VIEW projection. Every function here is a thin `&Transaction`
//! composition of [`mailbox_rows::reconstruct_person`] and
//! [`mailbox_rows::delta`] — no new raw SQL against the `mailbox` table, because
//! the row-level primitives already say everything these operations need.
//!
//! # Row identity, not a file key
//!
//! Row identity is `<envelopeId>@<personId>`
//! ([`crate::store::mailbox::MailboxEnvelope::row_id`],
//! [`mailbox_rows::MailboxEntry::envelope_id`]), not the TypeScript store's
//! `<createdAt-with-colons-replaced>-<id>.json` file key. Accordingly this
//! module deliberately does NOT port (Mandate 0 — no shim, no compatibility
//! layer, no fallback):
//!
//! * the `<safeCreatedAt>-<id>.json` file-key builder (TS `mailboxEnvelopeKey`) —
//!   a row's PRIMARY KEY already is `<id>@<person>`;
//! * `mailboxKeyMessageId` (the key-suffix parser that recovered a message id
//!   from that file key) — moot once the message id is a plain column
//!   (`MailboxEntry::envelope::id`), never encoded into a derived string;
//! * the `rawState` write-preservation side-map `applyDelta` carried so a
//!   whole-document rewrite would not clobber a `delivered` row — a per-call
//!   read via [`mailbox_rows::reconstruct_person`] immediately before every
//!   write here makes that side-map unnecessary: the write only ever touches
//!   rows this call itself decided to move;
//! * the whole-document `MailboxDocument` publish/mutate path
//!   (`mutateMailboxDocument`/`commitMailboxDocument`) — [`mailbox_rows::delta`]
//!   is already the fence-free, per-envelope write path every function in this
//!   module composes; there is no whole-document aggregate to publish.

use std::collections::BTreeMap;

use rusqlite::Transaction;

use crate::store::mailbox::MailboxState;
use crate::store::mailbox_rows::{self, MailboxEntry};
use crate::ChiefdError;

/// More than one `pending` (or fence-archived `Delivered`, which the search
/// treats as `pending` — see [`view`]) row matches the same message id for one
/// person. Under the row model's insert-if-absent `enqueue`
/// ([`crate::store::mailbox::enqueue`]) this should never arise going forward —
/// a row's identity is deterministic in `(message id, person)` — but
/// [`find_by_message_id`] refuses loudly rather than silently picking one,
/// matching the TypeScript `findMailboxEntryByMessageId`'s "multiple durable
/// copies" throw.
pub const MAILBOX_MULTIPLE_DURABLE_COPIES: &str = "mailbox-multiple-durable-copies";

/// The read-only five-bucket VIEW of one person's mailbox: live `pending`
/// (the `Delivered` fence-archive state — Fable ruling #5 — collapses into it,
/// exactly like the TypeScript `readMailboxDocument`'s `viewBucket`) plus the
/// four pane-drain terminals. Entries in each bucket are `envelope_id`-sorted
/// (the order [`mailbox_rows::reconstruct_person`] returns them in).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailboxView {
    /// Awaiting drain by the recipient's live pane, including fence-archived
    /// (`Delivered`) rows.
    pub pending: Vec<MailboxEntry>,
    /// The recipient accepted it.
    pub accepted: Vec<MailboxEntry>,
    /// Overtaken by a later envelope before it was accepted.
    pub superseded: Vec<MailboxEntry>,
    /// The recipient rejected it.
    pub rejected: Vec<MailboxEntry>,
    /// A health-incident alert whose incident was resolved.
    pub resolved: Vec<MailboxEntry>,
}

/// Reconstruct one person's mailbox as the five-bucket VIEW.
///
/// # Errors
/// Propagates [`mailbox_rows::reconstruct_person`]'s fail-closed store error.
pub fn view(tx: &Transaction<'_>, slug: &str, person: &str) -> Result<MailboxView, ChiefdError> {
    let snapshot = mailbox_rows::reconstruct_person(tx, slug, person)?;
    let mut out = MailboxView::default();
    for entry in snapshot.entries {
        match MailboxState::parse(&entry.state) {
            Some(state) if state.is_inbox_message() => out.pending.push(entry),
            Some(MailboxState::Accepted) => out.accepted.push(entry),
            Some(MailboxState::Superseded) => out.superseded.push(entry),
            Some(MailboxState::Rejected) => out.rejected.push(entry),
            Some(MailboxState::Resolved) => out.resolved.push(entry),
            // Exhaustive even though the guard above has already handled both
            // states. If its rule changes, these rows stay out of a terminal
            // bucket rather than being silently misclassified.
            Some(MailboxState::Pending | MailboxState::Delivered) => {}
            // `reconstruct_person` already fails closed (a store error)
            // on an unparseable state, so this arm is unreachable in practice;
            // it is kept exhaustive rather than a wildcard so a state that is
            // ever ADDED to the vocab fails to compile here, not silently drops.
            None => {}
        }
    }
    Ok(out)
}

/// Locate one envelope by its logical message id
/// ([`crate::store::mailbox::MailboxEnvelope::id`], NOT a file key — see the
/// module doc) among `person`'s rows.
///
/// Searches `pending` first (`Pending` and `Delivered` rows together, since the
/// VIEW collapses them — see [`view`]), then the pane-drain terminals in the
/// order accepted, superseded, rejected, resolved. Within any one state, the
/// lowest row id (`envelope_id`) wins. More than one `pending`-bucket match is
/// a loud, typed refusal — never a silent pick.
///
/// # Errors
/// [`MAILBOX_MULTIPLE_DURABLE_COPIES`] when more than one pending/delivered row
/// matches; propagates [`mailbox_rows::reconstruct_person`]'s
/// A fail-closed store error.
pub fn find_by_message_id(
    tx: &Transaction<'_>,
    slug: &str,
    person: &str,
    message_id: &str,
) -> Result<Option<MailboxEntry>, ChiefdError> {
    let snapshot = mailbox_rows::reconstruct_person(tx, slug, person)?;
    let mut matches: Vec<MailboxEntry> =
        snapshot.entries.into_iter().filter(|e| e.envelope.id == message_id).collect();
    matches.sort_by_key(MailboxEntry::envelope_id);

    let is_pending_bucket = |e: &MailboxEntry| {
        matches!(
            MailboxState::parse(&e.state),
            Some(MailboxState::Pending | MailboxState::Delivered)
        )
    };
    let pending_count = matches.iter().filter(|e| is_pending_bucket(e)).count();
    if pending_count > 1 {
        return Err(ChiefdError::refused(
            MAILBOX_MULTIPLE_DURABLE_COPIES,
            format!("Message id '{message_id}' has multiple durable copies for '{person}'"),
        ));
    }
    if let Some(entry) = matches.iter().find(|e| is_pending_bucket(e)) {
        return Ok(Some(entry.clone()));
    }
    for bucket in [
        MailboxState::Accepted,
        MailboxState::Superseded,
        MailboxState::Rejected,
        MailboxState::Resolved,
    ] {
        if let Some(entry) = matches.iter().find(|e| MailboxState::parse(&e.state) == Some(bucket))
        {
            return Ok(Some(entry.clone()));
        }
    }
    Ok(None)
}

/// One settle decision within a [`settle_many`] batch: move `row_id` (an
/// `<envelopeId>@<personId>` row identity, [`MailboxEntry::envelope_id`]) to
/// the terminal bucket `to`, if and only if it is currently `Pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleDecision {
    /// The row identity to settle.
    pub row_id: String,
    /// The terminal bucket to move it to.
    pub to: MailboxState,
}

/// One row [`settle_many`] actually moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The row identity that moved.
    pub row_id: String,
    /// The terminal bucket it moved to.
    pub to: MailboxState,
    /// The entry after the move (state already updated).
    pub entry: MailboxEntry,
}

/// A settle target that is not a terminal state — reuses
/// [`mailbox_rows::MAILBOX_INVALID`] (the same refusal a raw publish/delta
/// would hit trying to write an unrepresentable state).
fn require_terminal(to: MailboxState) -> Result<(), ChiefdError> {
    if to.is_terminal() {
        return Ok(());
    }
    Err(ChiefdError::refused(
        mailbox_rows::MAILBOX_INVALID,
        format!("mailbox settle target '{}' is not a terminal state", to.as_str()),
    ))
}

/// Move one row from `Pending` to a terminal bucket — the drain/archive step a
/// recipient's pane performs when it accepts (or supersedes, rejects,
/// resolves) its mail. `row_id` is the row identity (`<envelopeId>@<personId>`,
/// [`MailboxEntry::envelope_id`]); `updated_at_ms` is the epoch-millis stamp
/// written to the moved row and used as the `org_events` `at` value.
///
/// Only a `Pending` row moves. A row that does not exist for this person, that
/// another writer already settled, or that was fence-archived to `Delivered`
/// (chiefd-converge-owned, never a caller's to settle) is reported as `false`
/// — never an error: settling is idempotent by row identity, so a concurrent
/// settle of the same row is a harmless race, not a conflict.
///
/// # Errors
/// [`mailbox_rows::MAILBOX_INVALID`] when `to` is not a terminal state;
/// propagates [`mailbox_rows::reconstruct_person`]/[`mailbox_rows::delta`]'s
/// A fail-closed store error.
pub fn settle(
    tx: &Transaction<'_>,
    slug: &str,
    person: &str,
    row_id: &str,
    to: MailboxState,
    updated_at_ms: i64,
) -> Result<bool, ChiefdError> {
    require_terminal(to)?;
    let snapshot = mailbox_rows::reconstruct_person(tx, slug, person)?;
    let Some(current) = snapshot.entries.into_iter().find(|e| e.envelope_id() == row_id) else {
        return Ok(false);
    };
    if MailboxState::parse(&current.state) != Some(MailboxState::Pending) {
        return Ok(false);
    }
    let mut next = current;
    next.state = to.as_str().to_string();
    next.updated_at = updated_at_ms;
    mailbox_rows::delta(
        tx,
        slug,
        person,
        &[next],
        &[],
        &updated_at_ms.to_string(),
        mailbox_rows::IN_PROCESS_ACTOR,
    )?;
    Ok(true)
}

/// Settle every decision in `decisions` for one person as ONE per-person
/// [`mailbox_rows::delta`] upsert batch — one transaction for the whole batch
/// (Mandate 4), not one `delta` call per decision. Only currently-`Pending`
/// rows move; a decision naming a row that does not exist for this person, or
/// that is not `Pending` (already settled by a concurrent writer, or
/// fence-archived), is silently absent from the result — never an error,
/// matching [`settle`]'s idempotent-by-identity contract. A `row_id` repeated
/// across decisions only settles once (the first decision for it wins).
///
/// # Errors
/// [`mailbox_rows::MAILBOX_INVALID`] when ANY decision's `to` is not a
/// terminal state — checked before touching a row, so a batch with one bad
/// decision writes nothing; propagates
/// [`mailbox_rows::reconstruct_person`]/[`mailbox_rows::delta`]'s
/// A fail-closed store error.
pub fn settle_many(
    tx: &Transaction<'_>,
    slug: &str,
    person: &str,
    decisions: &[SettleDecision],
    updated_at_ms: i64,
) -> Result<Vec<Settled>, ChiefdError> {
    for decision in decisions {
        require_terminal(decision.to)?;
    }
    if decisions.is_empty() {
        return Ok(Vec::new());
    }
    let snapshot = mailbox_rows::reconstruct_person(tx, slug, person)?;
    let mut by_id: BTreeMap<String, MailboxEntry> =
        snapshot.entries.into_iter().map(|e| (e.envelope_id(), e)).collect();

    let mut upserts = Vec::new();
    let mut settled = Vec::new();
    for decision in decisions {
        let Some(current) = by_id.remove(&decision.row_id) else { continue };
        if MailboxState::parse(&current.state) != Some(MailboxState::Pending) {
            continue;
        }
        let mut next = current;
        next.state = decision.to.as_str().to_string();
        next.updated_at = updated_at_ms;
        settled.push(Settled {
            row_id: decision.row_id.clone(),
            to: decision.to,
            entry: next.clone(),
        });
        upserts.push(next);
    }
    if !upserts.is_empty() {
        mailbox_rows::delta(
            tx,
            slug,
            person,
            &upserts,
            &[],
            &updated_at_ms.to_string(),
            mailbox_rows::IN_PROCESS_ACTOR,
        )?;
    }
    Ok(settled)
}

/// Delete every row for `person` in ONE transaction (Mandate 4), returning the
/// count removed. Deletes ALL states, including fence-archived `Delivered`
/// rows: this is a full wipe (the departed-person mailbox clear), not a
/// selective delta, so — unlike an ordinary caller-driven delta, which never
/// deletes a `Delivered` row it did not itself settle — it does not spare them.
///
/// # Errors
/// Propagates [`mailbox_rows::reconstruct_person`]/[`mailbox_rows::delta`]'s
/// A fail-closed store error.
pub fn drop_person_mailbox(
    tx: &Transaction<'_>,
    slug: &str,
    person: &str,
    at: &str,
) -> Result<usize, ChiefdError> {
    let snapshot = mailbox_rows::reconstruct_person(tx, slug, person)?;
    if snapshot.entries.is_empty() {
        return Ok(0);
    }
    let deletes: Vec<String> = snapshot.entries.iter().map(MailboxEntry::envelope_id).collect();
    let count = deletes.len();
    mailbox_rows::delta(tx, slug, person, &[], &deletes, at, mailbox_rows::IN_PROCESS_ACTOR)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    //! Round-trip coverage for the mailbox view/settle helpers, mirroring the
    //! harness `mailbox_rows/tests.rs` uses (a real in-memory
    //! `COMPANY_SCHEMA_SQL` connection, published fixtures, one assertion per
    //! behavior).

    use super::*;
    use rusqlite::Connection;

    use crate::store::mailbox::{MailboxEnvelope, Urgency, MAILBOX_ENVELOPE_SCHEMA_VERSION};
    use crate::store::mailbox_rows::MailboxSnapshot;

    const SLUG: &str = "acme";

    fn open() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(crate::schema::COMPANY_SCHEMA_SQL).expect("apply company schema");
        conn
    }

    fn env(id: &str, to: &str) -> MailboxEnvelope {
        MailboxEnvelope {
            schema_version: MAILBOX_ENVELOPE_SCHEMA_VERSION,
            id: id.to_string(),
            organization: SLUG.to_string(),
            from_person_id: "chief".to_string(),
            to: to.to_string(),
            recipients: vec![to.to_string()],
            body: format!("body of {id}"),
            urgency: Urgency::Normal,
            reply_to: None,
            health_incident: None,
            created_at: "2026-08-01T00:00:00.000Z".to_string(),
        }
    }

    fn entry(id: &str, person: &str, state: &str) -> MailboxEntry {
        MailboxEntry {
            envelope: env(id, person),
            person: person.to_string(),
            state: state.to_string(),
            updated_at: 1_700_000_000_000,
            extra: BTreeMap::new(),
        }
    }

    fn seed(tx: &Transaction<'_>, entries: Vec<MailboxEntry>) {
        mailbox_rows::publish(tx, SLUG, &MailboxSnapshot { entries }).unwrap();
    }

    /// `view` collapses `Delivered` into `pending` and sorts the rest into the
    /// four pane-drain buckets.
    #[test]
    fn view_collapses_delivered_into_pending() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(
            &tx,
            vec![
                entry("p1", "bob", "pending"),
                entry("p2", "bob", "delivered"),
                entry("a1", "bob", "accepted"),
                entry("s1", "bob", "superseded"),
                entry("r1", "bob", "rejected"),
                entry("z1", "bob", "resolved"),
            ],
        );

        let v = view(&tx, SLUG, "bob").unwrap();
        let mut pending_ids: Vec<&str> = v.pending.iter().map(|e| e.envelope.id.as_str()).collect();
        pending_ids.sort_unstable();
        assert_eq!(pending_ids, vec!["p1", "p2"], "pending + delivered collapse into one bucket");
        assert_eq!(v.accepted.len(), 1);
        assert_eq!(v.superseded.len(), 1);
        assert_eq!(v.rejected.len(), 1);
        assert_eq!(v.resolved.len(), 1);
        tx.commit().unwrap();
    }

    /// An empty mailbox reconstructs to an all-empty view, not an error.
    #[test]
    fn view_of_an_absent_person_is_empty() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        let v = view(&tx, SLUG, "nobody").unwrap();
        assert_eq!(v, MailboxView::default());
        tx.commit().unwrap();
    }

    /// A single pending match is found directly.
    #[test]
    fn find_by_message_id_returns_a_single_pending_match() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("msg-1", "bob", "pending")]);
        let found = find_by_message_id(&tx, SLUG, "bob", "msg-1").unwrap().unwrap();
        assert_eq!(found.state, "pending");
        tx.commit().unwrap();
    }

    /// A `Delivered` row satisfies the same "pending" search bucket.
    #[test]
    fn find_by_message_id_treats_delivered_as_pending() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("msg-1", "bob", "delivered")]);
        let found = find_by_message_id(&tx, SLUG, "bob", "msg-1").unwrap().unwrap();
        assert_eq!(found.state, "delivered");
        tx.commit().unwrap();
    }

    /// The `mailbox` table's own `CHECK (envelope_id = id || '@' || person)`
    /// (schema.rs) makes "two rows, same (id, person), both pending"
    /// structurally unrepresentable — the PK `(slug, envelope_id)` collides
    /// before a second row could ever be written. The multiplicity guard in
    /// [`find_by_message_id`] is therefore defensive-but-currently-dead code,
    /// preserved byte-for-byte from the TypeScript `findMailboxEntryByMessageId`
    /// for the day that invariant loosens; this test documents that a single,
    /// ordinary match is never spuriously refused by it.
    #[test]
    fn find_by_message_id_does_not_spuriously_refuse_a_single_match() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("dup", "bob", "pending")]);
        assert!(find_by_message_id(&tx, SLUG, "bob", "dup").unwrap().is_some());
        tx.commit().unwrap();
    }

    /// Terminal buckets are searched in order accepted, superseded, rejected,
    /// resolved once no pending/delivered match exists.
    #[test]
    fn find_by_message_id_falls_through_terminal_buckets_in_order() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(
            &tx,
            vec![
                entry("only-superseded", "bob", "superseded"),
                entry("only-rejected", "bob", "rejected"),
            ],
        );
        assert_eq!(
            find_by_message_id(&tx, SLUG, "bob", "only-superseded").unwrap().unwrap().state,
            "superseded"
        );
        assert_eq!(
            find_by_message_id(&tx, SLUG, "bob", "only-rejected").unwrap().unwrap().state,
            "rejected"
        );
        tx.commit().unwrap();
    }

    /// No match returns `None`, not an error.
    #[test]
    fn find_by_message_id_returns_none_when_absent() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("other", "bob", "pending")]);
        assert!(find_by_message_id(&tx, SLUG, "bob", "missing").unwrap().is_none());
        tx.commit().unwrap();
    }

    /// `settle` moves a pending row to the requested terminal bucket.
    #[test]
    fn settle_moves_a_pending_row_to_a_terminal_bucket() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("m1", "bob", "pending")]);
        let moved =
            settle(&tx, SLUG, "bob", "m1@bob", MailboxState::Accepted, 1_700_000_001_000).unwrap();
        assert!(moved);
        let v = view(&tx, SLUG, "bob").unwrap();
        assert!(v.pending.is_empty());
        assert_eq!(v.accepted.len(), 1);
        assert_eq!(v.accepted[0].updated_at, 1_700_000_001_000);
        tx.commit().unwrap();
    }

    /// Settling an already-settled row is a no-op `false`, never an error —
    /// idempotent by row identity.
    #[test]
    fn settle_is_idempotent_and_reports_not_moved() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("m1", "bob", "pending")]);
        assert!(settle(&tx, SLUG, "bob", "m1@bob", MailboxState::Accepted, 1).unwrap());
        assert!(!settle(&tx, SLUG, "bob", "m1@bob", MailboxState::Accepted, 2).unwrap());
        // A row that never existed is likewise a no-op, not an error.
        assert!(!settle(&tx, SLUG, "bob", "nope@bob", MailboxState::Accepted, 3).unwrap());
        tx.commit().unwrap();
    }

    /// A fence-archived `Delivered` row is chiefd-converge-owned and is never
    /// moved by a caller's settle.
    #[test]
    fn settle_never_moves_a_delivered_row() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("m1", "bob", "delivered")]);
        assert!(!settle(&tx, SLUG, "bob", "m1@bob", MailboxState::Accepted, 1).unwrap());
        tx.commit().unwrap();
    }

    /// A non-terminal settle target is a typed refusal, not a panic or a
    /// silent no-op.
    #[test]
    fn settle_refuses_a_non_terminal_target() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("m1", "bob", "pending")]);
        let err = settle(&tx, SLUG, "bob", "m1@bob", MailboxState::Pending, 1).unwrap_err();
        assert!(matches!(err, ChiefdError::Refused(r) if r.code == mailbox_rows::MAILBOX_INVALID));
        tx.commit().unwrap();
    }

    /// `settle_many` moves every pending decision in one batch and silently
    /// skips a decision on a row that is missing or already settled.
    #[test]
    fn settle_many_batches_and_skips_not_moved() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(
            &tx,
            vec![
                entry("m1", "bob", "pending"),
                entry("m2", "bob", "pending"),
                entry("m3", "bob", "accepted"), // already settled
            ],
        );
        let decisions = vec![
            SettleDecision { row_id: "m1@bob".to_string(), to: MailboxState::Accepted },
            SettleDecision { row_id: "m2@bob".to_string(), to: MailboxState::Rejected },
            SettleDecision { row_id: "m3@bob".to_string(), to: MailboxState::Accepted },
            SettleDecision { row_id: "missing@bob".to_string(), to: MailboxState::Accepted },
        ];
        let settled = settle_many(&tx, SLUG, "bob", &decisions, 1_700_000_002_000).unwrap();
        let mut ids: Vec<&str> = settled.iter().map(|s| s.row_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["m1@bob", "m2@bob"], "only the two genuinely-pending rows moved");
        let v = view(&tx, SLUG, "bob").unwrap();
        assert!(v.pending.is_empty());
        // `accepted` holds TWO rows: m1, which this batch moved, and m3, which
        // was already accepted at seed time and was skipped. "Skipped" means
        // left exactly as it was — not removed, and not restamped.
        let mut accepted: Vec<&str> = v.accepted.iter().map(|e| e.envelope.id.as_str()).collect();
        accepted.sort_unstable();
        assert_eq!(
            accepted,
            vec!["m1", "m3"],
            "the moved row joins the skipped one, not replaces it"
        );
        let m3 = v.accepted.iter().find(|e| e.envelope.id == "m3").expect("m3 is still accepted");
        assert_eq!(m3.updated_at, 1_700_000_000_000, "a skipped row keeps its original stamp");
        let rejected: Vec<&str> = v.rejected.iter().map(|e| e.envelope.id.as_str()).collect();
        assert_eq!(rejected, vec!["m2"]);
        tx.commit().unwrap();
    }

    /// A batch with one non-terminal decision writes nothing at all — checked
    /// up front, before any row is touched.
    #[test]
    fn settle_many_refuses_the_whole_batch_on_one_bad_decision() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("m1", "bob", "pending")]);
        let decisions =
            vec![SettleDecision { row_id: "m1@bob".to_string(), to: MailboxState::Pending }];
        let err = settle_many(&tx, SLUG, "bob", &decisions, 1).unwrap_err();
        assert!(matches!(err, ChiefdError::Refused(r) if r.code == mailbox_rows::MAILBOX_INVALID));
        // Nothing moved: the row is still pending.
        let v = view(&tx, SLUG, "bob").unwrap();
        assert_eq!(v.pending.len(), 1);
        tx.commit().unwrap();
    }

    /// An empty decision batch is a harmless `Ok(vec![])`.
    #[test]
    fn settle_many_with_no_decisions_is_a_no_op() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(&tx, vec![entry("m1", "bob", "pending")]);
        let settled = settle_many(&tx, SLUG, "bob", &[], 1).unwrap();
        assert!(settled.is_empty());
        tx.commit().unwrap();
    }

    /// `drop_person_mailbox` deletes every row for that person, including a
    /// fence-archived `Delivered` row, and leaves other persons untouched.
    #[test]
    fn drop_person_mailbox_deletes_all_states_and_spares_others() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        seed(
            &tx,
            vec![
                entry("m1", "bob", "pending"),
                entry("m2", "bob", "delivered"),
                entry("m3", "bob", "accepted"),
                entry("keep", "ann", "pending"),
            ],
        );
        let removed = drop_person_mailbox(&tx, SLUG, "bob", "2026-08-01T00:01:00.000Z").unwrap();
        assert_eq!(removed, 3);
        assert_eq!(view(&tx, SLUG, "bob").unwrap(), MailboxView::default());
        assert_eq!(view(&tx, SLUG, "ann").unwrap().pending.len(), 1, "ann's mailbox is untouched");
        tx.commit().unwrap();
    }

    /// Dropping an absent person's mailbox is `Ok(0)`, not an error.
    #[test]
    fn drop_person_mailbox_of_an_absent_person_is_zero() {
        let mut conn = open();
        let tx = conn.transaction().unwrap();
        assert_eq!(drop_person_mailbox(&tx, SLUG, "nobody", "t").unwrap(), 0);
        tx.commit().unwrap();
    }
}
