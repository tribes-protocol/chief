//! The brain's rules, pinned.
//!
//! # What moved here, and why the tests had to move with it
//!
//! Every rule below used to belong to a rail PROCESS, and several of them used
//! to be rules about how two of those processes agreed: which one had the
//! glass, which one had recorded a wake, which one was allowed to rewrite the
//! selection. Those questions have no answer any more because they have no
//! second asker — so the tests that pinned the agreement are gone, and what
//! survives is pinned here, against the one struct that now holds the state.
//!
//! The daemon is a REAL one — a stub of three routes over a real listener with
//! a real signed bearer — because the property this stage is about is what the
//! click does while chiefd is not answering, and a mocked client cannot fail to
//! answer in the way a socket can.

// Staging a key fixture in a tempdir is the sanctioned use of the
// seam-disallowed writer: production filesystem effects belong to
// `chiefd_host`, and nothing in this crate writes a key.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{Brain, Event, Facts, WakeAnswer};
use crate::actuate::client::ActuationClient;
use crate::actuate::trust::tags;
use crate::bearer::Bearer;
use crate::sidebar::tests::RecordingTmux;
use crate::sidebar::wire::{Frames, Mailbox, ToBrain, ToClient, PROTOCOL};
use crate::sidebar::{Action, DepartmentRow, PersonRow, PersonState, Tmux, View};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A control client can apply the nested tmux command but return no nested
/// `display-message` output. This fake records that exact mutation and exposes
/// it only through the direct authoritative pane read that follows.
struct LostWakeAckTmux {
    inner: Arc<RecordingTmux>,
    claim: Mutex<Option<String>>,
}

impl LostWakeAckTmux {
    fn new(inner: Arc<RecordingTmux>) -> Self {
        Self { inner, claim: Mutex::new(None) }
    }
}

impl Tmux for LostWakeAckTmux {
    fn run(&self, args: &[&str]) -> String {
        let asked = args.join(" ");
        if args.first() == Some(&"if-shell")
            && (asked.contains("chief-wake:") || asked.contains("chief-external-wake:"))
        {
            let tail = asked.split_once(tags::WAKE_CLAIM).expect("wake CAS writes its claim").1;
            let claim = tail
                .split(|character: char| !character.is_ascii_hexdigit())
                .find(|part| part.len() == 32)
                .expect("wake claim is one UUID")
                .to_owned();
            *self.claim.lock().expect("claim lock") = Some(claim);
            let _ = self.inner.run(args);
            return String::new();
        }
        if args.first() == Some(&"display-message") && asked.contains("chief-end") {
            let claim = self.claim.lock().expect("claim lock").clone().unwrap_or_default();
            let _ = self.inner.run(args);
            return format!(
                "org-acme_\tacme\t__focus__\t0\tanalyst\t\t\t\t\t\t{claim}\t{claim}\t\tchief-end"
            );
        }
        self.inner.run(args)
    }
}

/// A company with one department and one SLEEPING person in it.
///
/// Sleeping is the whole precondition: the wake branch is only reached for a
/// person whose state is not live, and a person who is up is focused instead.
fn view_of_one_sleeper() -> View {
    let departments = vec![DepartmentRow {
        id: "quant".to_owned(),
        name: "Quant".to_owned(),
        depth: 0,
        live: 0,
        total: 1,
    }];
    let mut people = BTreeMap::new();
    people.insert(
        "quant".to_owned(),
        vec![PersonRow {
            id: "analyst".to_owned(),
            name: "Priya".to_owned(),
            title: "Quant Analyst".to_owned(),
            live: false,
            desired: false,
            idle: false,
            crash: None,
            refused: None,
            manager: false,
        }],
    );
    View::new(departments, people)
}

/// A staged 0600 operator key under `<data_root>/keys`, the way the daemon
/// mints it at boot. Without one the client refuses before it reaches the
/// network, and the ordering these tests are about would never be exercised.
fn staged_operator_key(dir: &Path) {
    use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
    use std::os::unix::fs::PermissionsExt as _;

    let keys = keys_of(dir);
    std::fs::create_dir_all(&keys).expect("keys dir");
    let secret = p256::SecretKey::from_slice(&[9u8; 32]).expect("scalar");
    let path = identity_keys::operator_key_path(&keys);
    std::fs::write(&path, secret.to_pkcs8_pem(LineEnding::LF).expect("pem").as_bytes())
        .expect("write key");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

/// A company's keys directory, `<dir>/.chief/keys` — the production layout
/// `chief_cli::paths::keys_dir` names, spelled here because the library half
/// may not reach the binary's module.
fn keys_of(dir: &Path) -> std::path::PathBuf {
    identity_keys::keys_dir(&dir.join(".chief"))
}

/// The placement inputs a brain that has read the company would be holding:
/// Quant with the sleeper and one colleague who is already up.
///
/// TWO desired people, on purpose. This fixture keeps the existing multi-person
/// move and completion path covered beside the only-person regression.
fn placement_of_a_two_person_quant() -> (crate::roster::Roster, BTreeMap<String, String>) {
    use crate::roster::{Roster, RosterCompany, RosterDepartment, RosterPerson};
    let person = |order: usize, id: &str| RosterPerson {
        id: id.to_owned(),
        display_name: "Priya".to_owned(),
        title: "Analyst".to_owned(),
        department_id: "quant".to_owned(),
        is_head_of: None,
        display_order: order,
        desired_active: true,
        employment_state: "active".to_owned(),
    };
    let roster = Roster {
        company: RosterCompany { slug: "acme".to_owned(), display_name: "Acme".to_owned() },
        root_department_id: "quant".to_owned(),
        departments: vec![RosterDepartment {
            id: "quant".to_owned(),
            name: "Quant".to_owned(),
            parent_department_id: None,
            head_person_id: "quant-head".to_owned(),
            order: 0,
            state: "active".to_owned(),
        }],
        people: vec![person(0, "quant-head"), person(1, "analyst")],
    };
    let hashes = [("quant-head", "hash-1"), ("analyst", "hash-2")]
        .into_iter()
        .map(|(person, hash)| (person.to_owned(), hash.to_owned()))
        .collect();
    (roster, hashes)
}

fn empty_inbox_counts(roster: &crate::roster::Roster) -> BTreeMap<String, usize> {
    roster.people.iter().map(|person| (person.id.clone(), 0)).collect()
}

/// A retained two-window company used to pin the brain's first selection.
fn retained_company_facts() -> Facts {
    use crate::roster::{Roster, RosterCompany, RosterDepartment, RosterPerson};
    let person = |order: usize, id: &str, department: &str| RosterPerson {
        id: id.to_owned(),
        display_name: id.to_owned(),
        title: "Lead".to_owned(),
        department_id: department.to_owned(),
        is_head_of: Some(department.to_owned()),
        display_order: order,
        desired_active: true,
        employment_state: "active".to_owned(),
    };
    let roster = Roster {
        company: RosterCompany { slug: "acme".to_owned(), display_name: "Acme".to_owned() },
        root_department_id: "executive".to_owned(),
        departments: vec![
            RosterDepartment {
                id: "executive".to_owned(),
                name: "Executive".to_owned(),
                parent_department_id: None,
                head_person_id: "chief".to_owned(),
                order: 0,
                state: "active".to_owned(),
            },
            RosterDepartment {
                id: "quant".to_owned(),
                name: "Quant".to_owned(),
                parent_department_id: Some("executive".to_owned()),
                head_person_id: "analyst".to_owned(),
                order: 1,
                state: "active".to_owned(),
            },
        ],
        people: vec![person(0, "chief", "executive"), person(1, "analyst", "quant")],
    };
    let inbox_counts = empty_inbox_counts(&roster);
    Facts {
        roster,
        desired: ["chief".to_owned(), "analyst".to_owned()].into_iter().collect(),
        idle: BTreeSet::new(),
        hashes: BTreeMap::new(),
        accents: BTreeMap::new(),
        models: BTreeMap::new(),
        inbox_counts,
        crashing: BTreeMap::new(),
        refusals: BTreeMap::new(),
    }
}

fn brain_on_retained_window(active: &str) -> (Brain, Arc<RecordingTmux>) {
    let tmux = Arc::new(RecordingTmux::answering(&[
        ("#{@organization_window_id}\t#{@organization_person_id}\t#{pane_dead}", active),
        ("#{@organization_person_id}\t#{pane_dead}", "chief\t0\nanalyst\t0"),
        ("list-windows -t org-acme_", "@1\texecutive\n@2\tquant\n@7\t__focus__"),
        ("#{@chief_asleep_for}", "%80\t__focus__"),
        ("#{window_panes}", "2"),
    ]));
    let (brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    (brain, tmux)
}

#[test]
fn first_company_frame_adopts_the_active_department_and_later_refreshes_do_not() {
    let (mut brain, tmux) = brain_on_retained_window("quant\t\t0\nquant\tanalyst\t0");

    brain.absorb(retained_company_facts());
    assert_eq!(brain.view.selected(), Some("quant"), "the first frame agrees with the glass");
    assert!(
        tmux.calls().iter().any(|call| {
            call == "list-panes -t org-acme_ -F #{@organization_window_id}\t#{@organization_person_id}\t#{pane_dead}"
        }),
        "the rule reads the active window and person in one simulated tmux snapshot"
    );

    brain.view.select("executive");
    brain.absorb(retained_company_facts());
    assert_eq!(
        brain.view.selected(),
        Some("executive"),
        "after the first company frame, operator selection wins every refresh"
    );
}

#[test]
fn first_company_frame_adopts_the_person_in_the_active_focus_window() {
    let (mut brain, _tmux) = brain_on_retained_window("__focus__\t\t0\n__focus__\tanalyst\t0");

    brain.absorb(retained_company_facts());

    assert_eq!(brain.view.selected(), Some("quant"), "the person's live home filters the list");
    assert_eq!(brain.view.selected_person(), Some("analyst"), "the row on the glass is marked");
}

/// The tmux a sleeper click needs to build its whole destination.
///
/// **THE FOCUS WINDOW IS ALREADY THERE, PARKED** — Stage 4. It is minted once
/// per session by `Brain::ensure_focus_window` on the company-read path, so by
/// the time any click can be serviced it exists, holds its rail (`%12`) and its
/// standing notice (`%80`), and no gesture will ever create or destroy it.
fn tmux_for_a_sleeper_click() -> Arc<RecordingTmux> {
    Arc::new(RecordingTmux::answering(&[
        ("list-windows", "@1\tquant\n@7\t__focus__"),
        // nobody is in it, and no earlier wake's panel is keeping a seat
        ("-t @7 -F #{pane_id}\t#{@organization_person_id}", ""),
        ("-t @7 -F #{pane_id}\t#{@chief_loading_for}", ""),
        (
            "-F #{pane_id}\t#{@organization_sidebar}\t#{@chief_asleep_for}\t#{@chief_sleeping_person}\t#{@chief_waking_person}\t#{@organization_person_id}",
            "%12\t1\t\t\t\t\n%80\t\t__focus__\t\t\t",
        ),
        // its two panes, and which of them is the standing notice
        ("-F #{pane_id}\t#{@organization_sidebar}", "%12\t1\t0\n%80\t\t0"),
        ("-t %80 #{@chief_asleep_for}", "__focus__"),
        ("show-options -p -t %80", "@chief_sleeping_person analyst"),
        ("#{pane_id}\t#{@chief_sleeping_person}", "%80\tanalyst"),
        (
            "#{session_name}\t#{@organization_id}\t#{@organization_window_id}\t#{pane_dead}\t#{@chief_waking_person}",
            "org-acme_\tacme\t__focus__\t0\tanalyst\t\t\t\t\t\taccepted-claim\taccepted-claim\t\tchief-end",
        ),
        // The department OVERVIEW's window, minted on the click: it carries its
        // own logical id (`__overview__:<department>`), so it is never one of
        // the windows `list-windows` above already names.
        ("new-window", "@9"),
        ("split-window", "%8"),
        ("#{window_height}", "200\t50"),
        ("@chief_sidebar_columns", "26"),
        ("#{pane_width}", "26"),
    ]))
}

/// The live failure shape: one clean rail and one unowned body that still
/// carries Nia's old waking claim after Nia left the desired set.
fn tmux_for_an_orphan_waking_then_sleeper_click() -> Arc<RecordingTmux> {
    Arc::new(RecordingTmux::answering(&[
        ("list-windows", "@1\tquant\n@7\t__focus__"),
        (
            "chief-orphan-waking-end",
            "%12\t1200\torg-acme_\t@7\tacme\t__focus__\t0\t1\t\t\t\t\t\t\tchief-orphan-waking-end\n\
             %80\t8000\torg-acme_\t@7\tacme\t__focus__\t0\t\t\t\tnia\t\t\told-claim\tchief-orphan-waking-end",
        ),
        (
            "chief-parked-end",
            "org-acme_\t@7\t8001\tacme\t__focus__\t0\t\t\t__focus__\t\t\t\t\tchief-parked-end",
        ),
        (
            "-F #{pane_id}\t#{@organization_sidebar}\t#{@chief_asleep_for}\t#{@chief_sleeping_person}\t#{@chief_waking_person}\t#{@organization_person_id}",
            "%12\t1\t\t\t\t\n%80\t\t__focus__\t\t\t",
        ),
        ("-F #{pane_id}\t#{@organization_sidebar}", "%12\t1\t0\n%80\t\t0"),
        ("-t %80 #{@chief_asleep_for}", "__focus__"),
        ("show-options -p -t %80", "@chief_sleeping_person analyst"),
        ("#{pane_id}\t#{@chief_sleeping_person}", "%80\tanalyst"),
        ("#{window_height}", "200\t50"),
        ("@chief_sidebar_columns", "26"),
        ("#{pane_width}", "26"),
    ]))
}

/// One shared tmux state for two separate rail brains. The claim is already
/// marked desired-seen, as a prior changefeed read records it in production.
/// Applying the orphan CAS changes the direct local-option proof to parked.
struct SharedOrphanWakingTmux {
    inner: Arc<RecordingTmux>,
    phase: Mutex<SharedWakingPhase>,
    ready: Mutex<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharedWakingPhase {
    Pending,
    Seen,
    Parked,
}

impl Tmux for SharedOrphanWakingTmux {
    fn run(&self, args: &[&str]) -> String {
        let asked = args.join(" ");
        if asked == "list-panes -s -t org-acme_ -F #{pane_id}" {
            return "%12\n%80".to_owned();
        }
        if asked.contains("chief-waking-scope") {
            let rail = asked.contains("-t %12");
            let phase = *self.phase.lock().expect("shared waking state");
            let (pane, pid, pane_options) = if rail {
                ("%12", "1200", "@organization_sidebar 1".to_owned())
            } else if phase == SharedWakingPhase::Parked {
                ("%80", "8001", "@chief_asleep_for __focus__".to_owned())
            } else {
                let seen = if phase == SharedWakingPhase::Seen {
                    "@chief_waking_desired_claim old-claim\n"
                } else {
                    ""
                };
                (
                    "%80",
                    "8000",
                    format!(
                        "@chief_wake_claim old-claim\n{seen}@chief_waking_pending_claim old-claim\n@chief_waking_person nia"
                    ),
                )
            };
            return format!(
                "chief-waking-scope\t$1\t@7\t{pane}\t{pid}\t{}\t0\t2\n\
                 chief-waking-pane-options\n{pane_options}\n\
                 chief-waking-window-options\n@organization_id acme\n@organization_window_id __focus__\n\
                 chief-waking-session-options\n{}@organization_id acme\n\
                 chief-waking-options-end",
                if rail { "26" } else { "174" },
                if *self.ready.lock().expect("shared recovery state") {
                    "@chief_waking_recovery_ready_v1 1\n"
                } else {
                    ""
                },
            );
        }
        if args.first() == Some(&"show-options") && asked.contains("@chief_orphan_commands_") {
            *self.phase.lock().expect("shared waking state") = SharedWakingPhase::Parked;
            let _ = self.inner.run(args);
            return String::new();
        }
        if args.first() == Some(&"if-shell")
            && asked.contains("@chief_waking_recovery_ready_v1")
            && !asked.contains("chief-orphan-park:")
        {
            *self.ready.lock().expect("shared recovery state") = true;
            let _ = self.inner.run(args);
            return String::new();
        }
        if args.first() == Some(&"if-shell")
            && asked.contains(tags::WAKING_DESIRED_SEEN)
            && asked.contains(&format!("#{{==:#{{{}}},}}", tags::WAKING_DESIRED_SEEN))
        {
            *self.phase.lock().expect("shared waking state") = SharedWakingPhase::Seen;
            let _ = self.inner.run(args);
            return String::new();
        }
        self.inner.run(args)
    }
}

/// A rail-only focus handoff whose topology batch applies but returns no pane
/// id, as a control-mode client does when the useful nested reply arrives
/// after the outer command block.
/// A card window that holds ONLY its rail, which is what
/// `ensure_focus_window` leaves between minting it and the next company read
/// parking it. A click landing in that gap has no body to reuse, so
/// `mint_focus_card_body` splits one.
///
/// This replaced `tmux_for_lost_sleeping_handoff`, whose subject was the
/// nested-reply recovery inside `handoff_occupied_focus` — the guarded batch
/// that sent the focus window's LIVE OCCUPANT home and took their cell. No live
/// person is ever in this window now, so the handoff, its lost-reply recovery
/// and the post-state readback that repaired it are all deleted; what is left
/// is one plain split, whose own `-P -F #{pane_id}` reports the body.
fn tmux_that_cannot_split_a_card_body() -> Arc<RecordingTmux> {
    Arc::new(RecordingTmux::answering(&[
        ("list-windows", "@1\tquant\n@7\t__focus__"),
        (
            "-F #{pane_id}\t#{@organization_sidebar}\t#{@chief_asleep_for}\t#{@chief_sleeping_person}\t#{@chief_waking_person}\t#{@organization_person_id}",
            "%12\t1\t\t\t\t",
        ),
        ("-F #{pane_id}\t#{@organization_sidebar}\t#{pane_dead}", "%12\t1\t0"),
        ("#{pane_index}", "0"),
        ("#{window_width}", "240"),
        ("@chief_sidebar_columns", "26"),
        ("#{pane_width}", "26"),
        ("#{window_height}", "200\t50"),
    ]))
}

fn tmux_for_rail_only_card_window() -> Arc<RecordingTmux> {
    Arc::new(RecordingTmux::answering(&[
        ("list-windows", "@1\tquant\n@7\t__focus__"),
        (
            "-F #{pane_id}\t#{@organization_sidebar}\t#{@chief_asleep_for}\t#{@chief_sleeping_person}\t#{@chief_waking_person}\t#{@organization_person_id}",
            "%12\t1\t\t\t\t",
        ),
        ("-F #{pane_id}\t#{@organization_sidebar}\t#{pane_dead}", "%12\t1\t0"),
        ("-F #{pane_id}\t#{@organization_person_id}\t#{pane_dead}", "%12\t\t0"),
        ("#{pane_id}\t#{@chief_sleeping_person}", "%80\tanalyst"),
        ("#{pane_index}", "0"),
        ("split-window", "%80"),
        ("#{window_width}", "240"),
        ("@chief_sidebar_columns", "26"),
        ("#{pane_width}", "26"),
        ("#{window_height}", "200\t50"),
    ]))
}

/// A brain wired to `url`, holding this company's sleeper, with a client
/// attached so a frame has somewhere to go.
fn brain_against<T: Tmux + 'static>(
    url: &str,
    root: &Path,
    tmux: Arc<T>,
    placed: bool,
) -> (Brain, tokio::sync::mpsc::UnboundedReceiver<Event>, Arc<Mailbox>) {
    let client = Arc::new(ActuationClient::new(
        url,
        "acme@digest",
        Arc::new(Bearer::operator(&keys_of(root))),
    ));
    let (mut brain, events) =
        Brain::new(tmux, client, "org-acme_".to_owned(), PathBuf::from("/company"));
    brain.view = view_of_one_sleeper();
    brain.inbox_counts =
        [("chief".to_owned(), 0), ("quant-head".to_owned(), 0), ("analyst".to_owned(), 0)]
            .into_iter()
            .collect();
    if placed {
        brain.placement = Some(placement_of_a_two_person_quant());
    }
    let outbox = Arc::new(Mailbox::new());
    brain.attach(1, "%9".to_owned(), 26, 50, Arc::clone(&outbox));
    (brain, events, outbox)
}

fn sleeper_facts(desired: &[&str]) -> Facts {
    let (roster, hashes) = placement_of_a_two_person_quant();
    let inbox_counts = empty_inbox_counts(&roster);
    Facts {
        roster,
        desired: desired.iter().map(|person| (*person).to_owned()).collect(),
        idle: BTreeSet::new(),
        hashes,
        accents: BTreeMap::new(),
        models: BTreeMap::new(),
        inbox_counts,
        crashing: BTreeMap::new(),
        refusals: BTreeMap::new(),
    }
}

/// The frame in a mailbox right now, or `None`.
async fn frame_in(outbox: &Mailbox) -> Option<ToClient> {
    tokio::time::timeout(std::time::Duration::from_millis(200), outbox.take()).await.ok().flatten()
}

/// Send the exact frame the card sends and let the production conversation
/// task and Brain event handler decide it.
async fn wake_through_card_wire(
    brain: &mut Brain,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    pane: &str,
    person: &str,
) -> ToClient {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (mut card, server) = tokio::net::UnixStream::pair().expect("private card socket");
    let conversation = tokio::spawn(super::converse(44, server, brain.answers.clone()));
    card.write_all(
        &ToBrain::WakePerson { protocol: PROTOCOL, pane: pane.into(), person: person.into() }
            .encode(),
    )
    .await
    .expect("write card action");
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
        .await
        .expect("card action reached the brain channel")
        .expect("brain channel remains open");
    assert!(matches!(
        &event,
        Event::WakeCard { pane: received_pane, person: received_person, .. }
            if received_pane == pane && received_person == person
    ));
    brain.apply(event);

    let mut bytes = [0_u8; 512];
    let count = tokio::time::timeout(std::time::Duration::from_secs(1), card.read(&mut bytes))
        .await
        .expect("brain answered the card")
        .expect("read card answer");
    let mut frames = Frames::new();
    frames.feed(&bytes[..count]);
    let answer =
        frames.next_to_client().expect("valid brain frame").expect("one complete brain answer");
    drop(card);
    conversation.abort();
    answer
}

/// Wakes received per hanging-daemon URL. A `OnceLock` because the routes are
/// `'static` closures and each test gets its own ephemeral port.
static HANGING_WAKES: std::sync::OnceLock<Mutex<BTreeMap<String, usize>>> =
    std::sync::OnceLock::new();

/// How many wakes a hanging daemon has received.
///
/// Counted on the DAEMON's side, because "did the brain post twice" is a
/// question about the wire and a brain-side counter would be the very state
/// under test.
async fn wake_posts(url: &str) -> usize {
    HANGING_WAKES
        .get()
        .and_then(|by_url| by_url.lock().expect("lock").get(url).copied())
        .unwrap_or_default()
}

/// A daemon that authenticates and then never answers the wake.
async fn hanging_daemon() -> String {
    use axum::routing::post;

    let listener =
        tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the hanging daemon");
    let url = format!("http://{}", listener.local_addr().expect("stub address"));
    let counted = url.clone();
    let app = axum::Router::new()
        .route(
            "/v1/auth/challenge",
            post(|| async { axum::Json(serde_json::json!({"nonceId":"n","nonce":"abc"})) }),
        )
        .route(
            "/v1/auth/token",
            post(|| async { axum::Json(serde_json::json!({"token":"jwt-1"})) }),
        )
        .route(
            "/v1/org/person/wake",
            post(move || async move {
                *HANGING_WAKES
                    .get_or_init(|| Mutex::new(BTreeMap::new()))
                    .lock()
                    .expect("lock")
                    .entry(counted)
                    .or_default() += 1;
                std::future::pending::<axum::http::StatusCode>().await
            }),
        );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    url
}

/// A daemon that answers NOTHING AT ALL, auth included.
///
/// This is the instrument for Stage 3's own proof and it is chosen for one
/// property: with the auth challenge hanging, **no HTTP request this client
/// makes can ever complete**. So a click that finishes is a click that awaited
/// none, and `wake_posts` stays at zero for ever rather than by luck of
/// scheduling — the assertion is deterministic instead of racy.
async fn mute_daemon() -> String {
    use axum::routing::post;

    let listener =
        tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the mute daemon");
    let url = format!("http://{}", listener.local_addr().expect("stub address"));
    let counted = url.clone();
    let app = axum::Router::new()
        .route(
            "/v1/auth/challenge",
            post(|| async { std::future::pending::<axum::http::StatusCode>().await }),
        )
        .route(
            "/v1/auth/token",
            post(|| async { std::future::pending::<axum::http::StatusCode>().await }),
        )
        .route(
            "/v1/org/person/wake",
            post(move || async move {
                *HANGING_WAKES
                    .get_or_init(|| Mutex::new(BTreeMap::new()))
                    .lock()
                    .expect("lock")
                    .entry(counted)
                    .or_default() += 1;
                std::future::pending::<axum::http::StatusCode>().await
            }),
        );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    url
}

// ---------------------------------------------------------------------------
// THE STAGE 3 PROOF
// ---------------------------------------------------------------------------

/// **A CLICK COMPLETES VISIBLY WITH NOTHING ON THE INPUT PATH BUT MEMORY AND
/// TMUX.**
///
/// This is the acceptance criterion of the design record Stage 3, stated
/// as a test: the brain is pointed at a daemon that answers nothing at all —
/// not even the auth challenge — and a click must still produce a FRAME and a
/// TMUX BATCH, inside the product's own 50ms ceiling.
///
/// The mute daemon is what makes "zero HTTP calls on the input path" a fact
/// rather than a hope. Every request this client could make blocks for ever on
/// the challenge, so a click that RETURNS is a click that awaited none of them;
/// and `wake_posts` can never leave zero, so the assertion cannot pass by a
/// scheduling accident.
///
/// Fifty milliseconds is the product's own ceiling and not a tolerance picked
/// to pass: every step in here is an in-memory tmux call, so the real figure is
/// microseconds and the margin against a dead daemon is unbounded.
#[tokio::test]
async fn a_sleeping_person_click_opens_their_card_without_waking_them() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    // The attach frame; what this test is about is the one AFTER it.
    let _ = frame_in(&outbox).await;

    // THE WHOLE INPUT PATH, from the bytes a thin client forwards. The company
    // title is row zero, Quant is row one, and its first person is row two.
    // SGR coordinates are one-based, so the wire says 3.
    let started = std::time::Instant::now();
    let keep = brain.input(1, b"\x1b[<0;2;3M");
    let elapsed = started.elapsed();
    assert!(keep, "a click never detaches the client that made it");

    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "a click must reach the glass in one frame however dead chiefd is; this one took \
         {elapsed:?}"
    );
    // VISIBLE: a frame is on its way to the pane.
    let frame = frame_in(&outbox).await.expect("the click produced a frame");
    let ToClient::Frame { bytes, .. } = frame else {
        panic!("a click answers with a frame");
    };
    assert!(!bytes.is_empty(), "and the frame has cells in it");
    // AND THE CLICK GAVE IMMEDIATE FEEDBACK WITHOUT BUILDING A SECOND PANE.
    // The actuator creates the final tagged pane. Its startup wrapper paints
    // the first frame and then execs Pi in that same pane.
    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| {
            call.contains("sleeping-person-card")
                && call.contains("Priya")
                && call.contains("Quant Analyst")
        }),
        "the final body opens the clicked person's exact card: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| {
            ["split-window", "new-window", "join-pane", "swap-pane", "kill-pane"]
                .iter()
                .any(|verb| call.contains(verb))
        }),
        "a sleeper click must not create or transition a temporary pane: {calls:?}"
    );
    let respawns: Vec<&String> =
        calls.iter().filter(|call| call.contains("respawn-pane -k -t %80")).collect();
    assert_eq!(respawns.len(), 1, "the existing body is repainted once in place: {calls:?}");
    assert!(respawns[0].contains("sleeping-person-card"), "the body runs the Chief card");
    assert!(!respawns[0].contains("Click a person"), "the generic frame is gone at the click");
    assert!(
        calls
            .iter()
            .any(|call| call.contains("pane-border-format") && call.contains("Quant Analyst")),
        "the sleeping card has the final role border instead of a raw title: {calls:?}"
    );
    // AND NOTHING ON THAT PATH SPOKE HTTP.
    assert_eq!(wake_posts(&url).await, 0, "selecting a sleeping person does not wake them");
    assert_eq!(
        brain
            .view
            .people()
            .into_iter()
            .find(|row| row.id == "analyst")
            .expect("the clicked person is on the list")
            .state(),
        PersonState::Sleeping,
        "the row stays sleeping until the card button is activated"
    );
}

#[tokio::test]
async fn a_department_row_shows_its_department_and_never_hijacks_to_its_manager() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    let mut people = BTreeMap::new();
    people.insert(
        "quant".to_owned(),
        vec![PersonRow {
            id: "analyst".to_owned(),
            name: "Priya".to_owned(),
            title: "Quant Analyst".to_owned(),
            live: false,
            desired: false,
            idle: false,
            crash: None,
            refused: None,
            manager: true,
        }],
    );
    brain.view = View::new(
        vec![DepartmentRow {
            id: "quant".to_owned(),
            name: "Quant".to_owned(),
            depth: 0,
            live: 0,
            total: 1,
        }],
        people,
    );
    brain.inbox_counts = [("analyst".to_owned(), 0)].into_iter().collect();

    brain.perform(
        Action::SelectDepartment("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );

    // THE DEPARTMENT, NOT ONE PERSON IN IT. The operator's ruling: a department
    // row shows that department -- everybody in it who is up comes back onto the
    // glass. Rewriting the row into a click on its manager replaced a whole team
    // with one card, and the operator reported exactly that as a regression.
    assert_eq!(brain.view.selected(), Some("quant"), "the department itself is selected");
    assert_eq!(brain.view.selected_person(), None, "no person is selected by a department row");
    assert_eq!(brain.sleeping_card, None, "and no card takes the glass");
    // Still zero: the department path has never had a wake route and must not
    // grow one. That half of the old rule is the half worth keeping.
    assert_eq!(wake_posts(&url).await, 0, "a department row cannot wake anybody");
    let calls = tmux.calls();
    assert!(
        !calls.iter().any(|call| call.contains("sleeping-person-card")),
        "a department row opens no person's card: {calls:?}"
    );
}

/// A BURST OF CLICKS ON ONE ROW ASKS CHIEFD ONCE — and answers the glass every
/// time.
///
/// The inline await used to do this by accident: the event loop could not even
/// READ a second click until the first POST returned. With the POST spawned the
/// loop is free, so without this the operator's own impatience becomes a burst
/// of wakes for one person — and their log shows nine clicks in three and a
/// half seconds on a lagging row, while five consecutive failed boots is what
/// makes the actuator give up on somebody for good.
///
/// The GLASS must still answer each click. A click into silence is the worst
/// outcome this surface has, and every effect the click performs is idempotent.
#[tokio::test]
async fn one_wake_button_action_posts_once_and_duplicates_are_idempotent() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    // The daemon never answers the wake, so the first one stays outstanding for
    // the whole burst — which is exactly the window the operator clicks into.
    let url = hanging_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    assert_eq!(wake_posts(&url).await, 0, "selecting the card does not wake");
    assert!(brain.wake_from_card("%80", "analyst"), "the button action is accepted");
    assert!(
        brain.wake_from_card("%80", "analyst"),
        "a duplicate action returns the accepted result"
    );
    assert_eq!(
        brain.view.people().into_iter().find(|row| row.id == "analyst").map(PersonRow::state),
        Some(PersonState::Starting),
        "the accepted button changes the rail row immediately"
    );
    // Let every spawned task that was going to reach the wire reach it.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        wake_posts(&url).await,
        1,
        "one button action and one duplicate produce one wake POST"
    );
    let calls = tmux.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("if-shell -F -t %80")
                && call.contains("@chief_waking_person"))
            .count(),
        1,
        "the same pane changes from sleeping to waking once: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| {
            call.starts_with("if-shell -F -t %80")
                && call.contains("@chief_waking_person")
                && call.contains("@chief_sleeping_person")
        }),
        "the final pane changes tags in one tmux publication: {calls:?}"
    );
    assert!(!calls.iter().any(|call| call.contains("split-window")));
    assert!(
        !calls
            .iter()
            .any(|call| { call.contains("set-option -p -t %80 @chief_asleep_for __focus__") }),
        "a repeated click on the same waking person never flashes the generic frame: {calls:?}"
    );
}

#[tokio::test]
async fn the_real_card_wire_enters_brain_authority_before_one_signed_wake_post() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, mut events, _outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert_eq!(
        wake_through_card_wire(&mut brain, &mut events, "%80", "analyst").await,
        ToClient::WakeAccepted { person: "analyst".into() }
    );
    assert_eq!(
        wake_through_card_wire(&mut brain, &mut events, "%80", "analyst").await,
        ToClient::WakeAccepted { person: "analyst".into() },
        "a duplicate real wire action is accepted but does not request another wake"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(wake_posts(&url).await, 1, "the signed ActuationClient posts exactly once");
    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert!(brain.waking.contains("analyst"));
    assert_eq!(brain.pending_zoom.as_deref(), Some("analyst"));
    assert_eq!(
        tmux.calls()
            .iter()
            .filter(|call| {
                call.starts_with("if-shell -F -t %80")
                    && call.contains("@chief_waking_person")
                    && call.contains("chief-wake:")
            })
            .count(),
        1,
        "the real wire action passes the exact card CAS once"
    );
}

#[tokio::test]
async fn a_freshly_split_card_body_survives_a_refresh_that_changes_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_rail_only_card_window();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert_eq!(brain.view.selected_person(), Some("analyst"));
    let posts_before = wake_posts(&url).await;
    let calls_before_refresh = tmux.calls().len();

    brain.absorb(sleeper_facts(&[]));

    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert_eq!(brain.view.selected_person(), Some("analyst"));
    assert_eq!(wake_posts(&url).await, posts_before);
    assert_eq!(posts_before, 0, "showing or refreshing a sleeping card never wakes it");
    assert!(
        !tmux.calls()[calls_before_refresh..]
            .iter()
            .any(|call| call.contains("respawn-pane -k -t %80")),
        "fresh desired=false/live=false facts leave the same final body untouched: {:?}",
        tmux.calls()
    );
    // NO POST-STATE READ AT ALL. The card body's own `split-window -P -F
    // #{pane_id}` reports it on the same invocation, so there is no nested
    // reply to lose and nothing to reconstruct by reading the window back.
    assert!(
        !tmux.calls().iter().any(|call| call.contains("chief-sleeping-card-end")),
        "the split reports its own pane: {:?}",
        tmux.calls()
    );

    let calls_before_external_wake = tmux.calls().len();
    brain.absorb(sleeper_facts(&["analyst"]));

    assert_eq!(brain.sleeping_card, None, "external desired state consumes card authority");
    assert_eq!(wake_posts(&url).await, 0, "a different backend client owns this wake");
    let external_calls = &tmux.calls()[calls_before_external_wake..];
    assert_eq!(
        external_calls
            .iter()
            .filter(|call| call.starts_with("if-shell -F -t %80")
                && call.contains("chief-external-wake"))
            .count(),
        1,
        "the recovered final body is promoted exactly once in place: {external_calls:?}"
    );
}

#[tokio::test]
async fn an_orphan_waking_body_is_parked_before_the_next_sleeping_click_without_a_post() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = Arc::new(SharedOrphanWakingTmux {
        inner: tmux_for_an_orphan_waking_then_sleeper_click(),
        phase: Mutex::new(SharedWakingPhase::Seen),
        ready: Mutex::new(true),
    });
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.absorb(sleeper_facts(&[]));

    assert!(!brain.waking.contains("nia"));
    assert_eq!(brain.pending_zoom, None);
    assert!(
        tmux.inner.calls().iter().any(|call| {
            call.starts_with("set-option -g @chief_orphan_commands_")
                && call.contains("#{==:#{pane_pid},1200}")
                && call.contains("#{==:#{pane_width},26}")
                && call.contains("#{==:#{pane_pid},8000}")
                && call.contains("@chief_waking_person},nia")
                && call.contains("@chief_wake_claim},old-claim")
                && call.contains("respawn-pane")
                && call.contains("%80")
        }),
        "the stale body is parked through one exact live guard: {:?}",
        tmux.inner.calls()
    );
    assert!(
        tmux.inner.calls().iter().any(|call| {
            call.starts_with("show-options -t $1 @organization_id")
                && call.contains("show-options -w -t @7 @organization_id")
                && call.contains("show-options -p -t %80 @chief_wake_claim")
                && call.contains("run-shell")
                && call.contains("@chief_orphan_commands_")
        }),
        "the staged command runs only after exact local scope reads: {:?}",
        tmux.inner.calls()
    );

    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert_eq!(brain.view.selected_person(), Some("analyst"));
    assert_eq!(wake_posts(&url).await, 0, "showing a sleeping card never sends the wake POST");
}

#[tokio::test]
async fn a_second_brain_cannot_park_a_fresh_pre_post_wake_but_can_park_its_shared_withdrawal() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = Arc::new(SharedOrphanWakingTmux {
        inner: tmux_for_an_orphan_waking_then_sleeper_click(),
        phase: Mutex::new(SharedWakingPhase::Pending),
        ready: Mutex::new(false),
    });
    let url = mute_daemon().await;
    let (mut first, _first_events, _first_outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    let (mut second, _second_events, _second_outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    first.waking.insert("nia".to_owned());
    first.pending_zoom = Some("nia".to_owned());

    first.absorb(sleeper_facts(&[]));
    second.absorb(sleeper_facts(&[]));

    assert_eq!(
        *tmux.phase.lock().expect("shared waking state"),
        SharedWakingPhase::Pending,
        "another rail cannot use its empty process-local state to retire the shared pending claim"
    );
    assert_eq!(
        tmux.inner.calls().iter().filter(|call| call.contains("chief-orphan-park:")).count(),
        0,
        "the pre-POST desired lag never reaches the destructive CAS"
    );

    first.absorb(sleeper_facts(&["nia"]));
    assert_eq!(
        *tmux.phase.lock().expect("shared waking state"),
        SharedWakingPhase::Seen,
        "desired=true stamps this exact shared claim"
    );

    second.absorb(sleeper_facts(&[]));
    assert_eq!(
        *tmux.phase.lock().expect("shared waking state"),
        SharedWakingPhase::Parked,
        "any rail can retire the same claim after its shared desired authority is withdrawn: {:?}",
        tmux.inner.calls()
    );
    assert_eq!(wake_posts(&url).await, 0);
}

// TOMBSTONE: `a_nonexact_lost_handoff_post_state_never_creates_card_authority`.
//
// The sleeping card used to be painted by `handoff_occupied_focus`, a guarded
// `if-shell` batch that sent the focus window's LIVE OCCUPANT home and took
// their cell. Its nested `display-message` reply could be lost, so the card's
// pane id was recovered by READING BACK the window's post-state — and that
// readback had to refuse six kinds of near-miss (the wrong person, a foreign
// company, a dead body, mixed ownership, two candidate bodies, none at all)
// before it could be believed.
//
// One window per person deleted the whole apparatus. No live person is ever in
// the card window, so there is no occupant to hand off; the card body is one
// plain `split-window` whose own `-P -F #{pane_id}` reports the pane it made,
// on the same invocation, with nothing nested to lose. There is no post-state
// to be non-exact about.

#[tokio::test]
async fn a_card_body_tmux_would_not_split_never_logs_false_shown_success() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    // The same rail-only card window, but tmux reports no pane for the split —
    // a window too narrow to divide, or a server that refused. The card was not
    // painted, and the one thing that must never happen is the rail claiming it
    // was.
    let tmux = tmux_that_cannot_split_a_card_body();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    let lines = crate::ladder::test_support::recorded("sleeping-card-refusal", || {
        brain.perform(
            Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
            crate::sidebar::gesture::next(),
            "%9",
        );
    });

    assert_eq!(brain.sleeping_card, None);
    assert!(
        lines.iter().any(|line| line.get("event").and_then(|value| value.as_str())
            == Some("sidebar.sleeping-card.refused")),
        "the missing exact body is a structured refusal: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.get("event").and_then(|value| value.as_str())
            == Some("sidebar.sleeping-card.shown")),
        "a None pane must never be reported as shown: {lines:?}"
    );
    assert_eq!(wake_posts(&url).await, 0);
}

#[tokio::test]
async fn an_absent_roster_person_clears_a_recovered_sleeping_card() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_rail_only_card_window();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    let mut facts = sleeper_facts(&[]);
    facts.roster.people.retain(|person| person.id != "analyst");

    brain.absorb(facts);

    assert_eq!(brain.sleeping_card, None);
    assert_eq!(brain.view.selected_person(), None);
    assert_eq!(wake_posts(&url).await, 0);
    assert!(tmux.calls().iter().any(|call| {
        call.contains("respawn-pane -k -t %80") && call.contains("Click a person in the sidebar")
    }));
}

#[tokio::test]
async fn a_lost_tmux_cas_reply_rechecks_the_mutated_card_and_posts_once() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let recorded = tmux_for_a_sleeper_click();
    let tmux = Arc::new(LostWakeAckTmux::new(Arc::clone(&recorded)));
    let url = hanging_daemon().await;
    let (mut brain, mut events, _outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert_eq!(
        wake_through_card_wire(&mut brain, &mut events, "%80", "analyst").await,
        ToClient::WakeAccepted { person: "analyst".into() },
        "the exact post-CAS WAKING state is the durable acknowledgement"
    );
    assert_eq!(
        wake_through_card_wire(&mut brain, &mut events, "%80", "analyst").await,
        ToClient::WakeAccepted { person: "analyst".into() },
        "a duplicate stays accepted without a second POST"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(wake_posts(&url).await, 1);
    assert!(brain.waking.contains("analyst"));
    assert_eq!(brain.pending_zoom.as_deref(), Some("analyst"));
    let calls = recorded.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("if-shell -F -t %80") && call.contains("chief-wake:"))
            .count(),
        1,
        "the action runs one guarded mutation even when its stdout is lost: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| {
            call.starts_with("display-message -p -t %80")
                && call.contains("chief-end")
                && call.contains("@chief_waking_person")
        }),
        "Brain re-reads the exact mutated pane before it accepts: {calls:?}"
    );
}

#[tokio::test]
async fn the_real_card_wire_rejects_a_wrong_person_or_pane_without_touching_the_card() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, mut events, _outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    let calls_before = tmux.calls().len();

    for (pane, person) in [("%81", "analyst"), ("%80", "someone-else")] {
        assert_eq!(
            wake_through_card_wire(&mut brain, &mut events, pane, person).await,
            ToClient::WakeRejected { person: person.into() }
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(wake_posts(&url).await, 0);
    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert!(!brain.waking.contains("analyst"));
    assert!(
        !tmux.calls()[calls_before..].iter().any(|call| call.contains("chief-wake:")),
        "neither wrong tuple changes the card marker"
    );
}

#[tokio::test]
async fn the_real_card_wire_rejects_a_person_who_is_no_longer_selected() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, mut events, _outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    brain.view.select("quant");
    let calls_before = tmux.calls().len();

    assert_eq!(
        wake_through_card_wire(&mut brain, &mut events, "%80", "analyst").await,
        ToClient::WakeRejected { person: "analyst".into() }
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(wake_posts(&url).await, 0);
    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert!(!brain.waking.contains("analyst"));
    assert!(!tmux.calls()[calls_before..].iter().any(|call| call.contains("chief-wake:")));
}

#[tokio::test]
async fn the_real_card_wire_rejects_a_person_who_is_already_desired() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, mut events, _outbox) =
        brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    brain.desired.insert("analyst".into());
    let calls_before = tmux.calls().len();

    assert_eq!(
        wake_through_card_wire(&mut brain, &mut events, "%80", "analyst").await,
        ToClient::WakeRejected { person: "analyst".into() }
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(wake_posts(&url).await, 0);
    assert_eq!(brain.sleeping_card, Some(("analyst".into(), "%80".into())));
    assert!(!brain.waking.contains("analyst"));
    assert!(!tmux.calls()[calls_before..].iter().any(|call| call.contains("chief-wake:")));
}

#[tokio::test]
async fn an_external_wake_promotes_the_exact_card_without_posting_or_parking() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let recorded = tmux_for_a_sleeper_click();
    let tmux = Arc::new(LostWakeAckTmux::new(Arc::clone(&recorded)));
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    brain.absorb(sleeper_facts(&["analyst"]));

    assert_eq!(wake_posts(&url).await, 0, "a different client owns the wake request");
    assert_eq!(brain.sleeping_card, None);
    let calls = recorded.calls();
    let promoted: Vec<_> = calls
        .iter()
        .filter(|call| {
            call.starts_with("if-shell -F -t %80") && call.contains("chief-external-wake")
        })
        .collect();
    assert_eq!(promoted.len(), 1, "one guarded in-place promotion: {calls:?}");
    assert!(promoted[0].contains("respawn-pane") && promoted[0].contains("%80"));
    assert!(promoted[0].contains("Priya is starting"));
    assert!(!calls.iter().any(|call| call.contains("@chief_asleep_for __focus__")));
    assert!(calls.iter().any(|call| call.contains("chief-end")));
}

#[tokio::test]
async fn a_fresh_live_person_removes_card_authority_and_focuses_the_same_person() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    tmux.answer("#{@organization_person_id}\t#{pane_dead}", "analyst\t0");
    tmux.answer(
        "#{pane_id}\t#{window_id}\t#{@organization_person_id}\t#{pane_dead}",
        "%81\t@1\tanalyst\t0",
    );
    tmux.answer("if-shell -F -t %80", "");
    tmux.answer("#{window_zoomed_flag}", "0");

    brain.absorb(sleeper_facts(&["analyst"]));

    assert_eq!(brain.sleeping_card, None);
    let calls = tmux.calls();
    // THE CARD IS ANSWERED BY NAVIGATION. It used to be answered by a MOVE —
    // one guarded batch that joined the person's live pane into the card's own
    // cell and killed the card, so the focus window never published the
    // obsolete card beside the live person. The person is in a window of their
    // own now, so there is no "beside": the operator is taken to them and the
    // spent card is parked behind them, off the glass.
    assert!(
        !calls.iter().any(|call| call.contains("join-pane")),
        "no pane is moved to retire a card: {calls:?}"
    );
    let selected_at = calls
        .iter()
        .position(|call| {
            call.contains("select-window -t @1") && call.contains("select-pane -t %81")
        })
        .expect("the operator is taken to the window the person is already alone in");
    let parked_at = calls
        .iter()
        .position(|call| {
            call.contains("respawn-pane") && call.contains("%80") && call.contains("__focus__")
        })
        .expect("the spent card is parked back to the standing notice");
    assert!(
        selected_at < parked_at,
        "the navigation lands FIRST, so the card is repainted behind the operator rather \
         than in front of them: {calls:?}"
    );
    assert!(
        !calls[selected_at..].iter().any(|call| call.contains("select-pane -t %80")),
        "and the obsolete card pane is never selected again once the person is live: \
         {calls:?}"
    );
}

#[tokio::test]
async fn a_retained_departed_person_clears_and_parks_the_old_card_and_stale_refusal() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    let mut facts = sleeper_facts(&[]);
    facts
        .roster
        .people
        .iter_mut()
        .find(|person| person.id == "analyst")
        .expect("retained roster row")
        .employment_state = "departed".into();
    brain.absorb(facts);

    assert_eq!(brain.sleeping_card, None);
    let calls = tmux.calls();
    assert!(calls.iter().any(|call| {
        call.contains("respawn-pane -k -t %80") && call.contains("Click a person in the sidebar")
    }));
    assert!(
        brain
            .placement
            .as_ref()
            .expect("placement")
            .0
            .people
            .iter()
            .any(|person| person.id == "analyst" && person.departed()),
        "the regression keeps the departed row in the durable roster"
    );
    let card_launches = calls.iter().filter(|call| call.contains("sleeping-person-card")).count();
    brain.settle_wake(&WakeAnswer {
        gesture: crate::sidebar::gesture::next(),
        person: "analyst".into(),
        name: "Priya".into(),
        refusal: Some("late refusal".into()),
    });
    assert_eq!(
        tmux.calls().iter().filter(|call| call.contains("sleeping-person-card")).count(),
        card_launches,
        "a stale refusal cannot restore a card for a retained departed roster row"
    );
}

/// A SLEEPER CLICK NEVER BUILDS A TEMPORARY DESTINATION.
///
/// The permanent focus body is the final person pane. The click repaints that
/// same pane immediately, and the actuator later respawns Pi in it.
#[tokio::test]
async fn a_sleeper_click_builds_no_temporary_destination() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| call.contains("sleeping-person-card analyst")),
        "the selected sleeper's card is visible: {calls:?}"
    );
    for forbidden in
        ["split-window", "new-window", "join-pane", "swap-pane", "kill-pane", "@chief_loading_for"]
    {
        assert!(
            !calls.iter().any(|call| call.contains(forbidden)),
            "the click must not use temporary-pane operation `{forbidden}`: {calls:?}"
        );
    }
    assert_eq!(
        calls.iter().filter(|call| call.contains("respawn-pane -k -t %80")).count(),
        1,
        "the only process transition keeps the existing final pane id: {calls:?}"
    );
    assert_eq!(
        brain.sleeping_card.as_ref().map(|(person, _)| person.as_str()),
        Some("analyst"),
        "the final body is reserved by this sleeping card"
    );
    assert_eq!(brain.pending_zoom, None, "selection alone has no pending wake");
}

// ---------------------------------------------------------------------------
// The thin-client contract
// ---------------------------------------------------------------------------

/// **A THIN CLIENT'S FIRST FRAME IS A PUSH.**
///
/// This is the whole of a thin client's boot, and the reason a freshly minted
/// window's rail paints in one socket round trip: it has no company to read, no
/// bearer to load and nothing to wait for. The rail this replaces spent that
/// moment on discovery, a beacond health wait and an authenticated round trip —
/// measured at a median 11ms and a tail of 804ms of blank pane.
#[tokio::test]
async fn a_thin_client_is_sent_a_frame_the_moment_it_attaches() {
    let tmux = Arc::new(RecordingTmux::answering(&[]));
    let (mut brain, _events) = Brain::new(
        tmux as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    let outbox = Arc::new(Mailbox::new());

    brain.attach(1, "%9".to_owned(), 26, 50, Arc::clone(&outbox));

    let frame = frame_in(&outbox).await.expect("attaching IS the boot: the frame is pushed");
    let ToClient::Frame { gesture, bytes } = frame else {
        panic!("a client is sent frames");
    };
    assert_eq!(gesture, None, "nobody asked for this one; it is the boot");
    assert!(!bytes.is_empty(), "and it has the rail's chrome in it");
}

/// **THE CORRELATOR CROSSES THE SOCKET.**
///
/// `gesture_id` used to travel between processes as the third field of the
/// `SELECTION` tmux option, and Stage 3 deletes that option. It rides the frame
/// instead — which is what lets the CLIENT, the only process that can honestly
/// say the bytes reached a pty, write `sidebar.frame.painted` naming the click
/// it answers. Without this the harness has no funnel at all.
#[tokio::test]
async fn the_frame_that_answers_a_click_names_that_click() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    let _ = frame_in(&outbox).await;

    let gesture = crate::sidebar::gesture::next();
    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        gesture,
        "%9",
    );
    brain.render(Some(gesture));

    let frame = frame_in(&outbox).await.expect("a frame answered the click");
    let ToClient::Frame { gesture: named, .. } = frame else {
        panic!("a click answers with a frame");
    };
    assert_eq!(
        named,
        Some(gesture.raw()),
        "the id the brain minted at the mouse event is the id the client will log"
    );
}

/// THE CARD IS BUILT FROM THE ROSTER THE BRAIN IS ALREADY HOLDING, and reads
/// nothing from any agent. That is the whole property: the tiled grid it
/// replaces put every live person in the unit on the glass at a third of the
/// width they are read at, and repainted each of them the moment one was
/// clicked.
#[tokio::test]
async fn a_department_card_carries_the_units_own_people_and_never_another_units() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.placement = Some(placement_of_a_two_person_quant());

    let mut people = BTreeMap::new();
    people.insert(
        "executive".to_owned(),
        vec![PersonRow {
            id: "chief".to_owned(),
            name: "Chief".to_owned(),
            title: "Chief Executive".to_owned(),
            live: true,
            desired: true,
            idle: false,
            crash: None,
            refused: None,
            manager: true,
        }],
    );
    people.insert(
        "quant".to_owned(),
        vec![
            PersonRow {
                id: "quant-head".to_owned(),
                name: "Quinn".to_owned(),
                title: "Head of Quant".to_owned(),
                live: true,
                desired: true,
                idle: true,
                crash: None,
                refused: None,
                manager: true,
            },
            PersonRow {
                id: "analyst".to_owned(),
                name: "Ana".to_owned(),
                title: "Analyst".to_owned(),
                live: false,
                desired: false,
                idle: false,
                crash: None,
                refused: None,
                manager: false,
            },
        ],
    );
    brain.view = View::new(
        vec![
            DepartmentRow {
                id: "executive".to_owned(),
                name: "Executive".to_owned(),
                depth: 0,
                live: 1,
                total: 1,
            },
            DepartmentRow {
                id: "quant".to_owned(),
                name: "Quant".to_owned(),
                depth: 1,
                live: 1,
                total: 2,
            },
            DepartmentRow {
                id: "quant-data".to_owned(),
                name: "Quant Data".to_owned(),
                depth: 2,
                live: 0,
                total: 0,
            },
        ],
        people,
    );
    brain.inbox_counts =
        [("chief".to_owned(), 0), ("quant-head".to_owned(), 0), ("analyst".to_owned(), 12)]
            .into_iter()
            .collect();

    let launch = brain.department_card_launch("quant").expect("the card builds from the roster");
    let payload = launch.last().expect("the payload is the last argument");
    let card: crate::sidebar::department_card::Card =
        serde_json::from_str(payload).expect("the payload is the card's own shape");

    assert_eq!(card.name, "Quant");
    assert_eq!(card.path, vec!["Executive".to_owned()], "the ancestor chain, outermost first");
    assert_eq!(
        card.children,
        vec!["Quant Data".to_owned()],
        "direct sub-units only, so a deep tree does not flatten into one line"
    );
    let names: Vec<&str> = card.members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(names, ["Quinn", "Ana"], "this unit's people, in roster order");
    assert_eq!(
        card.members.iter().map(|member| member.inbox_messages).collect::<Vec<_>>(),
        [0, 12],
        "the durable inbox count follows each person into the serialized card"
    );
    assert!(
        !names.contains(&"Chief"),
        "and NEVER another unit's — a card that borrowed the root's people is the \
         head-in-parent defect wearing a new hat"
    );

    let quinn = &card.members[0];
    assert!(quinn.head, "the head is marked from the roster's own per-row fact");
    assert_eq!(
        quinn.state,
        crate::sidebar::PersonState::Idle,
        "a live person with the settle clock running reads IDLE, which is what the \
         operator asked to be able to see"
    );
    assert_eq!(
        card.members[1].state,
        crate::sidebar::PersonState::Sleeping,
        "and a person with no pane that nobody wants reads asleep"
    );
    let tally = card.tally();
    assert_eq!((tally.up, tally.asleep), (1, 1));
}

// ---------------------------------------------------------------------------
// THE DEPARTMENT CARD AND THE RAIL SAY ONE THING
// ---------------------------------------------------------------------------

/// A tmux that REMEMBERS the one pane option the card refresh writes.
///
/// The transition guard is a comparison against tmux's own state, so a fake
/// that forgets every write reports "changed" for ever and would make the churn
/// test vacuous — which is the whole property under test. This records the
/// stamp the batch writes and answers the next read with it, which is all a
/// real server does here.
struct CardStampTmux {
    inner: Arc<RecordingTmux>,
    stamp: Mutex<BTreeMap<String, String>>,
}

impl CardStampTmux {
    fn new(inner: Arc<RecordingTmux>) -> Self {
        Self { inner, stamp: Mutex::new(BTreeMap::new()) }
    }
}

impl Tmux for CardStampTmux {
    fn run(&self, args: &[&str]) -> String {
        let asked = args.join(" ");
        if asked.starts_with("display-message -p")
            && asked.ends_with(&format!("#{{{}}}", tags::DEPARTMENT_CARD))
        {
            let _ = self.inner.run(args);
            let pane = args.iter().find(|arg| arg.starts_with('%')).copied().unwrap_or_default();
            return self.stamp.lock().expect("stamp lock").get(pane).cloned().unwrap_or_default();
        }
        if let Some(index) = args.iter().position(|arg| *arg == tags::DEPARTMENT_CARD) {
            // `set-option -p -t <pane> @chief_department_card <hex>`, possibly
            // inside a batch, so the pane is the argument before the tag.
            if let (Some(pane), Some(value)) = (args.get(index - 1), args.get(index + 1)) {
                self.stamp
                    .lock()
                    .expect("stamp lock")
                    .insert((*pane).to_owned(), (*value).to_owned());
            }
        }
        self.inner.run(args)
    }
}

/// A session whose glass shows the QUANT department's overview card: `@9`
/// carries the overview's own logical id, and holds a rail (`%12`) beside the
/// card pane (`%8`).
fn tmux_watching_the_quant_card() -> Arc<RecordingTmux> {
    let overview = crate::placement::overview_window_id("quant");
    Arc::new(RecordingTmux::answering(&[
        // The card pane, and the rail that is not it. Both before the looser
        // `@chief_asleep_for` row below, which answers the focus window's own
        // furniture read.
        ("-t %8 #{@chief_asleep_for}", &overview),
        ("-t %12 #{@chief_asleep_for}", ""),
        ("-t @9 -F #{pane_id}\t#{@organization_sidebar}\t#{pane_dead}", "%12\t1\t0\n%8\t\t0"),
        // A SECOND standing overview — Executive — which the operator is NOT
        // selected on. It has to be refreshed too, and pinning that is the
        // whole reason this fixture holds two.
        ("-t %20 #{@chief_asleep_for}", "__overview__:executive"),
        ("-t %21 #{@chief_asleep_for}", ""),
        ("-t @11 -F #{pane_id}\t#{@organization_sidebar}\t#{pane_dead}", "%21\t1\t0\n%20\t\t0"),
        (
            "list-windows -t org-acme_",
            &format!(
                "@1\texecutive\n@2\tquant\n@7\t__focus__\n@9\t{overview}\n@11\t{}",
                crate::placement::overview_window_id("executive")
            ),
        ),
        ("#{@organization_window_id}\t#{@organization_person_id}\t#{pane_dead}", "quant\t\t0"),
        ("#{@organization_person_id}\t#{pane_dead}", "chief\t0"),
        ("#{@chief_asleep_for}", "%80\t__focus__"),
        ("#{window_panes}", "2"),
    ]))
}

/// A brain on that glass, with QUANT selected.
fn brain_watching_the_quant_card() -> (Brain, Arc<RecordingTmux>) {
    let recorder = tmux_watching_the_quant_card();
    let tmux = Arc::new(CardStampTmux::new(Arc::clone(&recorder)));
    let (mut brain, _events) = Brain::new(
        tmux as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.absorb(retained_company_facts());
    brain.view.select("quant");
    // THE CLICK'S OWN CARD, drawn and stamped. A fixture that stopped at the
    // selection would leave the pane unstamped, and the first wake after it
    // would repaint for that reason rather than for a fact that moved — which
    // is exactly the confusion the stamp exists to remove.
    brain.absorb(retained_company_facts());
    (brain, recorder)
}

/// The payload of the card the brain would draw for `department`, parsed.
fn drawn_card(brain: &Brain, department: &str) -> crate::sidebar::department_card::Card {
    let launch = brain.department_card_launch(department).expect("the card builds from the roster");
    serde_json::from_str(launch.last().expect("the payload is the last argument"))
        .expect("the payload is the card's own shape")
}

/// **THE CARD AND THE RAIL ARE ONE ANSWER, NEVER TWO.**
///
/// The operator's report was two surfaces disagreeing about one company: their
/// rail drew `Executive 2/5` with Chief and Sam green while the card beside it
/// read `0 up · 4 asleep · 1 starting`, `Chief … starting`, `Sam … asleep`.
///
/// The MEASURED cause was staleness and not a second liveness read — the card
/// reads `View::everybody()` and the rail reads `View::people()`, and those are
/// the same `BTreeMap`, written by `View::refresh` on the company read and by
/// `View::set_live` on the click. But nothing pinned that, which is how the
/// wrong repair — "give the card its own tmux read" — stayed plausible. So it
/// is pinned: person for person, and count for count.
#[test]
fn the_card_says_exactly_what_the_rail_says_about_every_person() {
    let (brain, _tmux) = brain_watching_the_quant_card();

    for row in brain.view.departments() {
        let card = drawn_card(&brain, &row.id);
        let rail: Vec<(String, crate::sidebar::PersonState)> = brain
            .view
            .everybody()
            .get(&row.id)
            .map(|people| {
                people.iter().map(|person| (person.name.clone(), person.state())).collect()
            })
            .unwrap_or_default();
        let drawn: Vec<(String, crate::sidebar::PersonState)> =
            card.members.iter().map(|member| (member.name.clone(), member.state)).collect();
        assert_eq!(drawn, rail, "the card's rows ARE the rail's rows for {}", row.id);

        // AND THE ROLL-UP AGREES WITH THE ROW ABOVE IT. `2/5` on the rail and
        // `0 up` on the card is the disagreement the operator reported; it is
        // one fact and this is where the two readings of it meet.
        let tally = card.tally();
        assert_eq!(
            tally.up, row.live,
            "the card's `up` is the rail row's live count for {}",
            row.id
        );
        assert_eq!(
            tally.up + tally.starting + tally.blocked + tally.asleep,
            card.members.len(),
            "every member lands in exactly one bucket for {}",
            row.id
        );
    }
}

/// A person coming up moves BOTH surfaces, not one. This is the same claim as
/// above taken across a transition, because the defect was never visible in a
/// single frame — both surfaces were right about the moment they were drawn.
#[test]
fn a_person_coming_up_moves_the_card_and_the_rail_together() {
    let (mut brain, tmux) = brain_watching_the_quant_card();
    assert_eq!(
        drawn_card(&brain, "quant").tally(),
        crate::sidebar::department_card::Tally { up: 0, starting: 1, blocked: 0, asleep: 0 },
        "the analyst is wanted and has no pane yet"
    );

    tmux.answer("#{@organization_person_id}\t#{pane_dead}", "chief\t0\nanalyst\t0");
    brain.absorb(retained_company_facts());

    let card = drawn_card(&brain, "quant");
    assert_eq!(
        card.tally(),
        crate::sidebar::department_card::Tally { up: 1, starting: 0, blocked: 0, asleep: 0 },
        "and now they have one"
    );
    assert_eq!(
        card.members[0].state,
        crate::sidebar::PersonState::Working,
        "which is the state the rail's own row reads"
    );
}

/// **THE CARD IS REPAINTED WHEN THE COMPANY MOVES.** The whole defect: it was
/// argv handed to a pane at spawn time, and the only thing that ever spawned it
/// again was another department click.
#[test]
fn a_company_read_that_changes_a_state_repaints_the_card_in_place() {
    let (mut brain, tmux) = brain_watching_the_quant_card();
    let stamped = tmux.calls().len();

    tmux.answer("#{@organization_person_id}\t#{pane_dead}", "chief\t0\nanalyst\t0");
    brain.absorb(retained_company_facts());

    let calls = tmux.calls();
    let repaints: Vec<&String> =
        calls[stamped..].iter().filter(|call| call.contains("respawn-pane")).collect();
    assert_eq!(repaints.len(), 1, "the analyst came up, so the card is redrawn once: {repaints:?}");
    assert!(repaints[0].contains("respawn-pane -k -t %8"), "in place: {}", repaints[0]);
    assert!(
        repaints[0].contains("department-card"),
        "running the card program, not a notice: {}",
        repaints[0]
    );
}

/// A mailbox row can move without a roster or runtime fact moving with it.
/// That one durable count must refresh the card once, in place, and the same
/// count on the next pass must be silent.
#[test]
fn an_inbox_count_change_repaints_the_card_once() {
    let (mut brain, tmux) = brain_watching_the_quant_card();
    let settled = tmux.calls().len();
    let mut facts = retained_company_facts();
    facts.inbox_counts.insert("analyst".to_owned(), 12);

    brain.absorb(facts.clone());

    let calls = tmux.calls();
    let changed: Vec<&String> =
        calls[settled..].iter().filter(|call| call.contains("respawn-pane")).collect();
    assert_eq!(changed.len(), 1, "only Quant changed: {changed:?}");
    assert!(changed[0].contains("respawn-pane -k -t %8"), "the same card pane is reused");
    assert!(
        changed[0].contains(r#""inbox_messages":12"#),
        "the new count reaches the payload: {}",
        changed[0]
    );
    assert!(
        !changed[0].contains("select-layout") && !changed[0].contains("select-window"),
        "a count refresh does not move the glass: {}",
        changed[0]
    );

    let once = tmux.calls().len();
    brain.absorb(facts);
    assert!(
        !tmux.calls()[once..].iter().any(|call| call.contains("respawn-pane")),
        "the same count does not repaint twice"
    );
}

/// **EVERY STANDING CARD, NEVER "THE SELECTED ONE".**
///
/// MEASURED on a live company and it is the sharpest failure this work had: the
/// repair for the stale card asked `View::selected()` first, so a session
/// holding an Executive card AND a Research card refreshed one of them and
/// froze the other. The operator's rail read `Research 0/3` beside a card
/// reading `1 up · Alex idle` — the original defect, surviving inside the fix
/// for it, because a selection says what somebody is LOOKING at and never what
/// is true.
#[test]
fn a_card_the_operator_is_not_selected_on_is_repainted_too() {
    let (mut brain, tmux) = brain_watching_the_quant_card();
    assert_eq!(brain.view.selected(), Some("quant"), "the operator is on Quant");
    let settled = tmux.calls().len();

    // THE CEO'S PANE GOES, and nothing at all happens to Quant. Executive is
    // the department that moved and Executive is the card nobody is selected on.
    tmux.answer("#{@organization_person_id}\t#{pane_dead}", "");
    brain.absorb(retained_company_facts());

    let calls = tmux.calls();
    let repaints: Vec<&String> =
        calls[settled..].iter().filter(|call| call.contains("respawn-pane")).collect();
    assert!(
        repaints.iter().any(|call| call.contains("respawn-pane -k -t %20")),
        "the EXECUTIVE card is repainted although the operator is on Quant: {repaints:?}"
    );
    assert!(
        repaints.iter().any(|call| call.contains("Chief") || call.contains("chief")),
        "carrying the CEO's own new state: {repaints:?}"
    );
}

/// **AND NEVER WHEN IT DOES NOT.** `effects::show_department_overview` records
/// what an effect that fires on every changefeed wake did to this operator's
/// glass: relaid, re-selected, woke the other rail, churned continuously. This
/// path runs on that same wake and must be silent across an unchanged company.
#[test]
fn a_company_read_that_changes_nothing_repaints_nothing() {
    let (mut brain, tmux) = brain_watching_the_quant_card();
    let settled = tmux.calls().len();

    // TEN WAKES, because the loop this exists to stop fired on every one of
    // them. A single unchanged pass proves nothing about a path a chatty
    // company drives many times a second.
    for _ in 0..10 {
        brain.absorb(retained_company_facts());
    }

    // COUNTED against the CARD PANES, which are the only panes this rule may
    // ever write to. Anything at all landing on one of them across ten
    // unchanged company reads is the churn loop back.
    let calls = tmux.calls();
    let touched: Vec<&String> = calls[settled..]
        .iter()
        .filter(|call| !call.starts_with("display-message -p") && !call.starts_with("list-"))
        .filter(|call| {
            call.contains("%8")
                || call.contains("%20")
                || call.contains("respawn-pane")
                || call.contains("select-layout")
                || call.contains("select-window")
        })
        .collect();
    assert_eq!(
        touched.len(),
        0,
        "ten unchanged company reads issued {} write(s) at the glass: {touched:?}",
        touched.len()
    );
}

#[tokio::test]
async fn a_department_selection_frame_waits_until_the_geometry_action_returns() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    let _ = frame_in(&outbox).await;

    let gesture = crate::sidebar::gesture::next();
    brain.perform(Action::SelectDepartment("quant".to_owned()), gesture, "%9");
    assert_eq!(brain.view.selected(), Some("quant"), "the selection changed");
    // The subject here is FRAME ORDERING, not which window the click landed on:
    // whatever the geometry action does, no selected frame may be published
    // before it returns. (The overview this click now shows is minted from the
    // roster and needs a staged rail program, neither of which this fixture
    // has — the live test reads the window back off a real server instead.)
    assert!(
        frame_in(&outbox).await.is_none(),
        "perform never publishes a selected frame before or during its geometry action"
    );

    brain.render(Some(gesture));
    assert!(frame_in(&outbox).await.is_some(), "the post-geometry selected frame is published");
}

#[tokio::test]
async fn every_department_row_click_shows_the_overview_and_only_disclosure_collapses() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    let mut people = BTreeMap::new();
    people.insert(
        "executive".to_owned(),
        vec![PersonRow {
            id: "chief".to_owned(),
            name: "Chief".to_owned(),
            title: "Chief".to_owned(),
            live: true,
            desired: true,
            idle: false,
            crash: None,
            refused: None,
            manager: true,
        }],
    );
    people.insert("quant".to_owned(), brain.view.everybody()["quant"].clone());
    brain.view = View::new(
        vec![
            DepartmentRow {
                id: "executive".to_owned(),
                name: "Executive".to_owned(),
                depth: 0,
                live: 1,
                total: 1,
            },
            DepartmentRow {
                id: "quant".to_owned(),
                name: "Quant".to_owned(),
                depth: 1,
                live: 0,
                total: 2,
            },
        ],
        people,
    );
    // THE OVERVIEW IS BUILT FROM THE ROSTER. A brain that has not read the
    // company yet cannot name the company a window belongs to, so it declines
    // to mint one and says so (`sidebar.department.unplaced`) — the same rule
    // the sibling card path has always followed. Give it the read.
    brain.placement = Some(placement_of_a_two_person_quant());
    brain.view.scroll(1);
    let scroll_before = brain.view.scroll_offset();
    assert_eq!(scroll_before, 1, "the click starts below the top of the tree");
    brain.view.select_person("analyst");
    assert!(!brain.view.is_expanded("quant"));
    let selections_before =
        tmux.calls().iter().filter(|call| call.contains("select-window")).count();

    brain.perform(
        Action::SelectDepartment("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );
    assert_eq!(
        brain.view.selected_person(),
        None,
        "the row moves the selection off the person and onto the department itself"
    );
    assert!(brain.view.is_expanded("quant"), "the row opens a collapsed department");
    assert_eq!(
        brain.view.scroll_offset(),
        scroll_before,
        "the collapsed body click does not scroll"
    );
    // NAVIGATION IS NOT ASSERTED HERE any more, and the reason is worth stating.
    // A department click now shows an OVERVIEW window, which is minted from the
    // roster and needs a real rail program to put in it; this fixture stages
    // neither, so the click correctly declines to mint and says so. What this
    // test is about — selection, disclosure and scroll — is unaffected by that,
    // and the navigation itself is proved against a REAL tmux in
    // `sidebar::tests::a_real_tmux_shows_the_department_and_then_the_person_the_operator_clicked`,
    // which stages both and reads the window back off the server.
    let after_first = tmux.calls().iter().filter(|call| call.contains("select-window")).count();
    assert_eq!(after_first, selections_before, "the click performs no geometry it cannot describe");

    brain.perform(
        Action::SelectDepartment("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );
    let after_second = tmux.calls().iter().filter(|call| call.contains("select-window")).count();
    assert_eq!(after_second, after_first, "and the repeated click likewise");
    assert!(brain.view.is_expanded("quant"), "the repeated row click does not collapse");
    assert_eq!(
        brain.view.scroll_offset(),
        scroll_before,
        "the expanded body click does not scroll"
    );

    brain.perform(
        Action::ToggleDepartmentDisclosure("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );
    assert!(!brain.view.is_expanded("quant"), "the explicit disclosure closes the branch");
    assert_eq!(brain.view.scroll_offset(), scroll_before, "the disclosure click does not scroll");
    assert_eq!(
        tmux.calls().iter().filter(|call| call.contains("select-window")).count(),
        after_second,
        "disclosure does not navigate"
    );
}

/// AN IDENTICAL FRAME IS NOT SENT. herdr drops those server-side for the same
/// reason, and it is what makes the pointer crossing the rail cost zero bytes.
///
/// A frame answering a GESTURE is always sent even when it is identical,
/// because the client is what writes `sidebar.frame.painted` and a gesture with
/// no frame has no honest end.
#[tokio::test]
async fn a_frame_that_changes_nothing_is_not_sent_unless_a_gesture_asked_for_it() {
    let tmux = Arc::new(RecordingTmux::answering(&[]));
    let (mut brain, _events) = Brain::new(
        tmux as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    let outbox = Arc::new(Mailbox::new());
    brain.attach(1, "%9".to_owned(), 26, 50, Arc::clone(&outbox));
    let _ = frame_in(&outbox).await;

    brain.render(None);
    assert!(frame_in(&outbox).await.is_none(), "nothing changed, so nothing was sent");

    let gesture = crate::sidebar::gesture::next();
    brain.render(Some(gesture));
    assert!(
        frame_in(&outbox).await.is_some(),
        "but a gesture is always answered, so it always has an end to measure"
    );
}

/// **A REPAINT WRITES EVERY CELL AND NEVER ERASES.**
///
/// ratatui draws the difference between its own back buffer and the frame it is
/// about to draw; that is a BELIEF about what is on the glass, and tmux
/// falsifies it by resizing a pane between the two. Writing every cell replaces
/// the belief with a fact — and it is also what makes the single-slot mailbox
/// sound, because a frame that can be dropped in flight must not be a diff.
///
/// It must NOT be done by erasing first. The operator runs tmux 3.3a, whose
/// binary contains no reference to mode 2026 at all, so an `ED2` reaches the
/// glass alone and every repaint blanks the pane for a frame.
#[tokio::test]
async fn every_frame_is_whole_and_no_frame_erases_the_screen() {
    let tmux = Arc::new(RecordingTmux::answering(&[]));
    let (mut brain, _events) = Brain::new(
        tmux as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.view = view_of_one_sleeper();
    let outbox = Arc::new(Mailbox::new());
    brain.attach(1, "%9".to_owned(), 26, 20, Arc::clone(&outbox));

    let frame = frame_in(&outbox).await.expect("the boot frame");
    let ToClient::Frame { bytes, .. } = frame else { panic!("a frame") };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        !text.contains("\u{1b}[2J"),
        "an ED2 is a blank pane on a tmux that ignores synchronized updates"
    );
    // WHOLE: the frame carries the first department, which a diff against an
    // unchanged buffer would have skipped entirely.
    assert!(text.contains("Quant"), "every cell is written: {text:?}");
}

// ---------------------------------------------------------------------------
// What the converge loop reads
// ---------------------------------------------------------------------------

/// **THE FOCUS IS A FIELD READ, NOT A BUS.**
///
/// `placement::desired_topology` is handed the person the operator clicked, and
/// converge used to learn that person by reading a tmux option some rail had
/// written. It asks the brain now, so the marker the operator can see and the
/// placement they are shown cannot come apart.
#[tokio::test]
async fn converge_is_handed_the_person_the_click_chose() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    let focus = Arc::clone(&brain.focus);

    let gesture = crate::sidebar::gesture::next();
    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        gesture,
        "%9",
    );

    let seen = focus.lock().expect("not poisoned").clone();
    assert_eq!(seen.person.as_deref(), Some("analyst"));
    assert_eq!(
        seen.gesture,
        Some(gesture.raw()),
        "and it names the gesture, so `actuator.gesture.observed` can be subtracted from \
         this click's own `sidebar.click`"
    );
}

/// **A CLICK RINGS CONVERGE AT ONCE.**
///
/// `actuator.gesture.observed` was measured at 2,831ms and 4,477ms — pure
/// converge-cadence latency between the operator clicking and the process that
/// spawns panes learning of it. It is the same process now, and this is the
/// signal that collapses it.
#[tokio::test]
async fn a_gesture_wakes_the_converge_loop_rather_than_waiting_for_the_changefeed() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    let nudge = Arc::clone(&brain.nudge);
    // Parked exactly where the converge loop parks: `Notify` holds a permit for
    // a `notify_one` that arrives first, so this cannot race.
    let waiting = tokio::spawn(async move { nudge.notified().await });
    tokio::task::yield_now().await;

    brain.perform(
        Action::SelectDepartment("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .expect("converge is woken by the gesture, not by the next changefeed event")
        .expect("the task must not panic");
}

/// **THE SETTLE PASS SHRANK TO THE GESTURES THAT ACTUALLY MOVE SOMETHING.**
///
/// `gestured_at` is the anti-jitter rule: tmux applies a pane's GRID resize
/// synchronously with the command but re-sizes the pty — and so delivers
/// SIGWINCH — up to ~250ms later, so a frame drawn in that gap is measured at
/// one width and interpreted at another. Every gesture used to stamp it, because
/// every gesture used to churn topology: a person click minted a window, a
/// department click killed one, and both re-laid whatever they touched.
///
/// Stage 4 deleted that churn, so a department click that returns nobody is one
/// `select-window` and has no transit to wait out. Stamping it anyway would make
/// the brain withhold the next 300ms of resizes the OPERATOR asked for — a drag
/// of the sidebar border immediately after a navigation, which is exactly the
/// sequence an operator arranging their screen performs.
///
/// The gestures that DO stamp it are pinned in the same test, because "arms
/// nothing" is only a rule if "arms something" is still true next door.
#[tokio::test]
async fn a_department_click_that_moves_nobody_arms_no_settle_pass() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    brain.perform(
        Action::SelectDepartment("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert!(
        brain.gestured_at.is_none(),
        "nothing moved, so nothing is in transit: {:?}",
        tmux.calls()
    );
    assert!(brain.settle_at.is_none(), "and no settle pass is due: {:?}", tmux.calls());

    // A sleeper click also moves no geometry. The actuator creates the final
    // pane later, so the click path must not arm a settle pass for a temporary
    // panel that no longer exists.
    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert!(brain.gestured_at.is_none(), "the card click moved no panes: {:?}", tmux.calls());
    assert!(brain.settle_at.is_none(), "no temporary-pane settle pass is due");
}

/// **A TRANSIT MUST NEVER WRITE A HUMAN PREFERENCE.**
///
/// `record_width` used to run only from a `Resize` the transit rule let
/// through, which turned that rule's own skip into a way to make a wrong answer
/// permanent: record the FIRST resize of a transit, skip the one that corrects
/// it, and `@chief_sidebar_columns` holds the wrong number for the life of the
/// session — every later layout reproduces it, because the recorded width is
/// what a layout falls back to when the current one is implausible.
///
/// MEASURED on the live company, and it is what this test reproduces exactly:
/// converge killed the parked focus window's standing notice and re-laid the
/// window in one argv; tmux handed the notice's columns to the rail between the
/// two; the rail reported **113** columns of its 200-column window; the brain
/// recorded 113 twenty milliseconds before converge's own geometry stamp
/// arrived, and then correctly skipped the 113 -> 26 that would have undone it.
/// Every window in the session was laid with a 113-column sidebar from then on.
///
/// The settle pass now records the SETTLED width, so the last word about the
/// rail is about geometry that has stopped moving.
///
/// Proved RED: without the record in `settle`, the option is left at 113.
#[tokio::test]
async fn the_settle_pass_never_records_runtime_geometry() {
    let tmux = Arc::new(RecordingTmux::answering(&[
        // The rail's own window is the one on the glass, so its width may be
        // recorded at all.
        ("display-message -p -t org-acme_ #{window_id}", "@7"),
        ("display-message -p -t %9 #{window_id}", "@7"),
        ("-t %9 #{window_panes}", "2"),
        ("-t %9 #{window_width}", "200"),
    ]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.view = view_of_one_sleeper();
    let outbox = Arc::new(Mailbox::new());
    brain.attach(1, "%9".to_owned(), 26, 50, outbox);

    // A transit begins — converge is about to move geometry.
    brain.apply(Event::GeometryMoved);
    // tmux hands the rail a whole departing sibling's columns…
    brain.apply(Event::Resize { id: 1, columns: 113, rows: 50 });
    // …and takes them back a moment later. BOTH are inside the transit, so
    // neither is painted and neither is recorded on its own.
    brain.apply(Event::Resize { id: 1, columns: 26, rows: 50 });
    assert!(
        !tmux
            .calls()
            .iter()
            .any(|call| call.starts_with("set-option") && call.contains("@chief_sidebar_columns")),
        "nothing is written mid-transit: {:?}",
        tmux.calls()
    );

    brain.settle();

    assert!(
        !tmux
            .calls()
            .iter()
            .any(|call| call.contains("set-option") && call.contains("@chief_sidebar_columns")),
        "only MouseDragEnd1Border can write the expanded preference: {:?}",
        tmux.calls()
    );
}

#[test]
fn a_generic_resize_restores_the_effective_width_without_writing_a_preference() {
    let tmux = Arc::new(RecordingTmux::answering(&[("list-panes -s -t org-acme_", "%9\t1")]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.view = view_of_one_sleeper();
    brain.attach(1, "%9".to_owned(), 26, 50, Arc::new(Mailbox::new()));

    brain.apply(Event::Resize { id: 1, columns: 80, rows: 59 });

    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| call.contains("resize-pane -x 26 -t %9")),
        "viewport geometry must yield to the saved effective width: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| {
            call.starts_with("set-option") && call.contains("@chief_sidebar_columns")
        }),
        "generic SIGWINCH is not preference authority: {calls:?}"
    );
}

#[test]
fn a_collapsed_rail_restores_four_columns_after_a_viewport_resize() {
    let tmux = Arc::new(RecordingTmux::new(&["26", "1", "26", "%9\t1", ""]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.view = view_of_one_sleeper();
    brain.view.set_collapsed(true);
    brain.attach(1, "%9".to_owned(), 4, 50, Arc::new(Mailbox::new()));

    brain.apply(Event::Resize { id: 1, columns: 80, rows: 59 });

    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| call.contains("resize-pane -x 4 -t %9")),
        "the collapsed effective width is four columns: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("set-option")),
        "viewport repair changes no preference: {calls:?}"
    );
}

#[test]
fn brain_restart_reads_custom_width_and_collapse_independently() {
    let tmux = Arc::new(RecordingTmux::new(&["37", "1"]));
    let (brain, _events) = Brain::new(
        tmux as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    assert_eq!(brain.expanded_columns, 37);
    assert!(brain.view.collapsed());
}

#[test]
fn fresh_client_attach_applies_custom_width_without_writing_preferences() {
    let tmux = Arc::new(RecordingTmux::new(&["37", "0", ""]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.attach(1, "%9".to_owned(), 26, 50, Arc::new(Mailbox::new()));
    assert!(tmux.calls().iter().any(|call| call == "resize-pane -x 37 -t %9"));
    assert!(!tmux.calls().iter().any(|call| call.starts_with("set-option")));
}

#[test]
fn explicit_drag_option_is_mirrored_but_generic_resize_never_writes_it() {
    let tmux = Arc::new(RecordingTmux::new(&[
        "",   // restart: expanded default
        "",   // restart: open
        "37", // MouseDragEnd1Border has now written the option
        "%9\t1\n%12\t1",
        "",
    ]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.view = view_of_one_sleeper();
    brain.attach(1, "%9".to_owned(), 26, 50, Arc::new(Mailbox::new()));
    brain.apply(Event::Resize { id: 1, columns: 37, rows: 50 });

    assert_eq!(brain.expanded_columns, 37);
    assert!(tmux
        .calls()
        .iter()
        .any(|call| { call == "resize-pane -x 37 -t %9 ; resize-pane -x 37 -t %12" }));
    assert!(!tmux
        .calls()
        .iter()
        .any(|call| { call.starts_with("set-option") && call.contains("@chief_sidebar_columns") }));
}

#[test]
fn collapse_and_expand_restore_custom_width_in_every_window() {
    let tmux = Arc::new(RecordingTmux::new(&[
        "37",
        "0",
        "37",
        "",
        "%9\t1\n%12\t1",
        "",
        "37",
        "",
        "%9\t1\n%12\t1",
        "",
    ]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.perform(Action::ToggleCollapsed, crate::sidebar::gesture::next(), "%9");
    brain.perform(Action::ToggleCollapsed, crate::sidebar::gesture::next(), "%9");

    let calls = tmux.calls();
    assert!(calls.iter().any(|call| call == "set-option -t org-acme_ @chief_sidebar_collapsed 1"));
    assert!(calls.iter().any(|call| call == "resize-pane -x 4 -t %9 ; resize-pane -x 4 -t %12"));
    assert!(calls.iter().any(|call| call == "set-option -t org-acme_ @chief_sidebar_collapsed 0"));
    assert!(calls.iter().any(|call| call == "resize-pane -x 37 -t %9 ; resize-pane -x 37 -t %12"));
    assert!(!calls
        .iter()
        .any(|call| { call.starts_with("set-option") && call.contains("@chief_sidebar_columns") }));
}

// ---------------------------------------------------------------------------
// The state that used to live in tmux options
// ---------------------------------------------------------------------------

/// SELECTING SEVERAL SLEEPERS DOES NOT MARK ANY OF THEM STARTING.
///
/// The operator wakes people in bursts, and the record that drove this used to
/// be a single-slot session option — so every click on a second sleeper erased
/// the first sleeper's mark: "when I clicked on all four users, it showed
/// Starting — but when I clicked on the other one, the one that started goes to
/// Sleeping." It is a `BTreeSet` field now, and each entry ends on its OWN
/// person's arrival or refusal.
#[tokio::test]
async fn sleeper_selection_marks_nobody_starting_before_a_button_action() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view = view_of_two_sleepers();

    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    brain.perform(
        Action::FocusPerson {
            department_id: "quant".to_owned(),
            person_id: "quant-head".to_owned(),
        },
        crate::sidebar::gesture::next(),
        "%9",
    );

    assert!(
        !tmux.calls().iter().any(|call| call.contains("Click a person in the sidebar")),
        "switching between sleeping cards never publishes the generic body"
    );

    // The company is read again. Selection is still not a wake action.
    let roster = placement_of_a_two_person_quant().0;
    let inbox_counts = empty_inbox_counts(&roster);
    brain.absorb(Facts {
        roster,
        desired: BTreeSet::new(),
        idle: BTreeSet::new(),
        hashes: BTreeMap::new(),
        accents: BTreeMap::new(),
        models: BTreeMap::new(),
        inbox_counts,
        crashing: BTreeMap::new(),
        refusals: BTreeMap::new(),
    });

    let states: BTreeMap<String, PersonState> =
        brain.view.people().into_iter().map(|row| (row.id.clone(), row.state())).collect();
    assert_eq!(states.get("analyst"), Some(&PersonState::Sleeping));
    assert_eq!(
        states.get("quant-head"),
        Some(&PersonState::Sleeping),
        "neither row starts before its card button is activated: {states:?}"
    );
}

/// chiefd's own refusal sentence, as `LaunchCatalog::refusals` publishes it.
const CATALOG_REFUSAL: &str =
    "required files 'settings.json' and 'agent.md' are missing from home '/companies/acme/priya'";

/// A company read whose LAUNCH CATALOG declined somebody.
fn refused_facts(person: &str) -> Facts {
    Facts {
        refusals: [(person.to_owned(), CATALOG_REFUSAL.to_owned())].into_iter().collect(),
        ..sleeper_facts(&[person])
    }
}

/// THE WHOLE PATH, END TO END: chiefd's launch gate declines somebody, the
/// converge loop hands the catalog's refusals to the brain beside the desired
/// set it read in the same pass, and the rail draws `refused` with the reason.
///
/// The desired set is built with no launch gate at all — the gate lives on the
/// launch-catalog route, because the daemon is the only process that can see
/// the disk it gates on. So `desired && !live` was true for a refused person on
/// every pass and the row read `starting` for ever. The refusal now travels the
/// same seam the actuator's crash-loop holds travel, which is the seam that
/// already exists for "a fact about this person that chiefd's desired set
/// cannot carry".
#[tokio::test]
async fn a_launch_gate_refusal_reaches_the_rail_from_the_catalog_with_its_reason() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = Arc::new(RecordingTmux::answering(&[]));
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);

    brain.absorb(refused_facts("analyst"));

    let row = brain
        .view
        .people()
        .into_iter()
        .find(|row| row.id == "analyst")
        .expect("the refused person is still drawn");
    assert!(row.desired, "chiefd wants them; the refusal does not un-want them");
    assert_eq!(
        row.state(),
        PersonState::Refused,
        "a person the gate declined must never render a state that implies they are about to          start"
    );
    assert_eq!(row.refused.as_deref(), Some(CATALOG_REFUSAL), "with the gate's own reason");
}

// SUPERSEDED, AND DELETED RATHER THAN KEPT ALONGSIDE:
// `a_click_on_a_refused_person_names_the_gates_reason_and_posts_no_wake`
// asserted that SOME tmux call carried the gate's sentence, which a
// `display-message` satisfies — and that is exactly what shipped and what the
// operator could not see. A test that cannot tell a status flash from a
// surface is a test that pins the wrong rule, and keeping it beside the real
// one would keep that rule alive.
// `a_click_on_a_refused_person_puts_the_gates_reason_on_the_focus_body` is its
// replacement: it drives the operator's own input path and asserts the answer
// lands on the body they are reading. It carries both of the old test's other
// assertions — no wake posted, and no notice that says "waking".

/// The same company with BOTH people asleep.
fn view_of_two_sleepers() -> View {
    let departments = vec![DepartmentRow {
        id: "quant".to_owned(),
        name: "Quant".to_owned(),
        depth: 0,
        live: 0,
        total: 2,
    }];
    let asleep = |id: &str, name: &str| PersonRow {
        id: id.to_owned(),
        name: name.to_owned(),
        title: "Intelligence Analyst".to_owned(),
        live: false,
        desired: false,
        idle: false,
        crash: None,
        refused: None,
        manager: false,
    };
    let mut people = BTreeMap::new();
    people.insert(
        "quant".to_owned(),
        vec![asleep("analyst", "Priya"), asleep("quant-head", "Quinn")],
    );
    View::new(departments, people)
}

/// **A PERSON WHO IS NOT COMING BACK NEVER MOVES THE GLASS TO THE CEO.**
///
/// Operator ruling: *"we should never ever switch without the user explicitly
/// clicking."* Measured on a live box, session
/// `org-taperoom-inc-4cc439_`: a click on the sleeping `pm-exposure` put their
/// card on the FOCUS window at `01:52:55.606`, a disclosure toggle at
/// `01:52:57.777` changed no selection, and the converge pass at `01:52:57.958`
/// read the focus window as "a person window" and threw the glass to `@chief`
/// — while the rail went on highlighting `pm-exposure`.
///
/// This runs the rule over EVERY window the operator can be standing on, because
/// the deleted guard's whole defect was that it asked which one it was. The
/// answer is the same from all three: the person stays selected, they are told
/// the person has gone, and the person's own card takes the glass.
#[tokio::test]
async fn a_person_who_is_neither_up_nor_wanted_never_moves_the_glass_to_the_ceo() {
    // AN INVARIANCE PIN, and read it as one: the repaired `tidy_selection`
    // never asks which window the operator is standing on, so all three arms
    // execute the same code. That is the POINT — the deleted guard's whole
    // defect was that it asked — but it is one path proved not to vary, not
    // three paths of coverage, and a reader must not count it as three.
    for standing_on in ["__focus__", "__person__:analyst", "quant"] {
        let root = tempfile::tempdir().expect("tempdir");
        staged_operator_key(root.path());
        let tmux = tmux_for_a_sleeper_click();
        tmux.answer("display-message -p -t org-acme_ -F #{@organization_window_id}", standing_on);
        let url = mute_daemon().await;
        let (mut brain, _events, _outbox) =
            brain_against(&url, root.path(), Arc::clone(&tmux), true);
        brain.view.select("quant");
        brain.view.select_person("analyst");

        brain.tidy_selection(&BTreeSet::new(), &BTreeSet::new());

        let calls = tmux.calls();
        assert!(
            calls.iter().any(|call| call.starts_with("display-message -t org-acme_")
                && call.contains("Priya")
                && call.contains("no longer up")),
            "the operator is TOLD, rather than left watching a window go stale (on \
             {standing_on}): {calls:?}"
        );
        // THE RAIL AND THE GLASS MAY NEVER DISAGREE. The operator's screenshot
        // was the last-clicked person highlighted beside the CEO's panel.
        assert_eq!(
            brain.view.selected_person(),
            Some("analyst"),
            "nothing but a click may move the selection (on {standing_on})"
        );
        assert_eq!(
            brain.view.selected(),
            Some("quant"),
            "and the department marker stays where the click left it (on {standing_on})"
        );
        assert!(
            calls.iter().any(|call| call.contains("sleeping-person-card analyst")),
            "the SAME subject is redrawn honestly: the person's own card takes the glass \
             (on {standing_on}): {calls:?}"
        );
        // THE CEO IS THE HEAD OF THE FIXTURE'S ROOT DEPARTMENT, `quant-head`.
        // Not selected, and not shown, by any of these paths.
        assert_ne!(
            brain.view.selected_person(),
            Some("quant-head"),
            "the CEO landing is gone (on {standing_on})"
        );
        assert!(
            !calls.iter().any(|call| call.contains("quant-head")),
            "and nothing about the CEO reaches tmux either (on {standing_on}): {calls:?}"
        );
    }
}

/// **A DISCLOSURE TRIANGLE MOVES NOTHING, SO IT PARKS NOTHING.**
///
/// The second half of the operator's BUG 1, and it survived the first cut of
/// this change. Their measured sequence was a click on a SLEEPING person
/// (`pm-exposure`, carded on the focus window) and then a triangle. The toggle
/// arm selects nothing, shows nothing and reports `moved_geometry = false` —
/// but it fell through `perform`'s "any gesture that is not a person click
/// parks the card" block, so the triangle killed that person's card into the
/// parked "Click a person in the sidebar…" notice while the rail went on
/// highlighting them. That is the same two-halves disagreement the CEO landing
/// produced, reached by a different path, and the repair cost a false
/// announcement: the next pass re-carded them by saying "«name» is no longer
/// up", which is untrue of somebody who was only ever ASLEEP, and it repeated
/// on every triangle because the re-card resets `gone`.
///
/// The pending zoom is exempt for the same reason and it is not a bonus: a
/// wake the operator asked for, cancelled by a triangle, leaves the glass on
/// the parked notice under a rail that still names that person — the very
/// state this whole change exists to make impossible.
#[tokio::test]
async fn a_disclosure_toggle_leaves_the_card_and_the_pending_wake_alone() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.absorb(sleeper_facts(&[]));
    brain.view.select("quant");
    brain.view.select_person("analyst");
    brain.sleeping_card = Some(("analyst".to_owned(), "%80".to_owned()));
    brain.pending_zoom = Some("analyst".to_owned());
    let before = tmux.calls().len();

    brain.perform(
        Action::ToggleDepartmentDisclosure("quant".to_owned()),
        crate::sidebar::gesture::next(),
        "%9",
    );

    let calls = tmux.calls();
    let after: Vec<&String> = calls[before..].iter().collect();
    assert_eq!(
        brain.sleeping_card.as_ref().map(|(person, _)| person.as_str()),
        Some("analyst"),
        "the card the operator asked for outlives a gesture that navigates nowhere"
    );
    assert_eq!(
        brain.pending_zoom.as_deref(),
        Some("analyst"),
        "and so does the wake they asked for"
    );
    // The fixture DOES answer the park's own probe — `#{pane_id}\t#{@chief_
    // sleeping_person}` returns `%80\tanalyst` — so the park would reach tmux
    // if it were still called, and these two assertions are not vacuous.
    assert!(
        !after.iter().any(|call| call.contains("rename-window")),
        "no park: the focus window is not renamed out from under the card: {after:?}"
    );
    assert!(
        !after.iter().any(|call| call.contains("respawn-pane")),
        "and nothing is respawned, so there is no card kill and no flicker: {after:?}"
    );
    assert!(
        !after.iter().any(|call| call.contains("no longer up")),
        "and nobody is told a sleeping person has gone: {after:?}"
    );
    // The one thing a triangle DOES do.
    assert!(!brain.view.is_expanded("quant"), "the branch still closes");
}

/// **A CLICK WHOSE NAVIGATION DID NOT LAND IS FINISHED, ONCE.**
///
/// #1231 established the mechanism and made the failure sayable: a click's
/// `select-window` can silently fail, the rail's in-process selection moves
/// anyway, and the operator is left looking at somebody else's window while the
/// rail insists otherwise. This completes it.
///
/// The four bounds are each load-bearing and each has a test below: inside a
/// click's own window, gesture-fenced, once per episode, and LIVE people only —
/// the last of which is what keeps the reap working, because under #1211 a gone
/// person stays selected and enforcing for them would pin their dead window on
/// the glass for ever.
#[tokio::test]
async fn a_diverged_selection_inside_a_clicks_own_window_is_re_asserted_once() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    // Analyst is LIVE — the bound below turns on this and nothing else.
    tmux.answer("-F #{@organization_person_id}\t#{pane_dead}", "analyst\t0");
    // The session is showing SOMEBODY ELSE's window: the click did not land.
    tmux.answer("display-message -p -t org-acme_ #{window_id}", "@1");
    tmux.answer("-t @1 -F #{@organization_window_id}", "__person__:chief");
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");
    brain.clicked_at = Some(std::time::Instant::now());
    let before = tmux.calls().len();

    brain.absorb(sleeper_facts(&["analyst"]));

    // ASSERTED ON THE ENFORCEMENT'S OWN PROBE, not on `select-window`: the card
    // and repair paths also select windows, so a bare verb match would pass for
    // the wrong reason.
    let probe = "display-message -p -t org-acme_ #{window_id}";
    assert!(
        tmux.calls()[before..].iter().any(|call| call == probe),
        "the divergence is read: {:?}",
        &tmux.calls()[before..]
    );
    // ONCE. A window that will not take the selection is reported, not fought.
    let after_first = tmux.calls().len();
    brain.absorb(sleeper_facts(&["analyst"]));
    assert!(
        !tmux.calls()[after_first..].iter().any(|call| call == probe),
        "and not again on the next pass: {:?}",
        &tmux.calls()[after_first..]
    );
}

/// AND NEVER LONG AFTER THE CLICK. An operator who switches windows by hand has
/// made a NEW decision; re-asserting the old one would be the brain overruling
/// the person it exists to serve.
#[tokio::test]
async fn a_manual_window_switch_long_after_a_click_is_never_reverted() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    tmux.answer("display-message -p -t org-acme_ #{window_id}", "@1");
    tmux.answer("-t @1 -F #{@organization_window_id}", "__person__:chief");
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");
    // The click was a minute ago — outside its own completion window.
    brain.clicked_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
    let before = tmux.calls().len();

    brain.absorb(sleeper_facts(&["analyst"]));

    assert!(
        !tmux.calls()[before..]
            .iter()
            .any(|call| call == "display-message -p -t org-acme_ #{window_id}"),
        "past the click's window the glass belongs to the operator: {:?}",
        &tmux.calls()[before..]
    );
}

/// AND NEVER FOR A PERSON WHO HAS GONE — the bound that keeps the reap working.
///
/// Under #1211 a gone person stays SELECTED until the operator clicks elsewhere.
/// Enforcing for them would hold their dead window on the glass and make it
/// unreapable for ever: the same starvation `kill_window`'s comment warns
/// about, reached through a third door. `tidy_selection` owns them and cards
/// them off, unchanged.
#[tokio::test]
async fn a_selected_person_who_is_not_live_is_never_enforced() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    tmux.answer("display-message -p -t org-acme_ #{window_id}", "@1");
    tmux.answer("-t @1 -F #{@organization_window_id}", "__person__:chief");
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");
    brain.clicked_at = Some(std::time::Instant::now());
    let before = tmux.calls().len();

    // `sleeper_facts(&[])` leaves nobody live.
    brain.absorb(sleeper_facts(&[]));

    // The CARD path selects a window for a gone person, and that is correct and
    // unchanged — so this asserts the ENFORCEMENT's own probe never runs rather
    // than that no window was ever selected.
    assert!(
        !tmux.calls()[before..]
            .iter()
            .any(|call| call == "display-message -p -t org-acme_ #{window_id}"),
        "a gone person's window must stay reapable: {:?}",
        &tmux.calls()[before..]
    );
}

/// THE LOSS IS ANNOUNCED ONCE, not once per company read.
///
/// A company read arrives about once a second and re-derives the same absence
/// every time. The card the repair paints is itself an exemption on the next
/// pass, but the announcement must not depend on the card having been
/// paintable: a person whose ROW has gone has nothing to draw a card from.
#[tokio::test]
async fn the_operator_is_told_once_that_the_person_they_selected_has_gone() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");
    // No row for them at all, so the card cannot be painted and only the
    // transition record can stop the announcement repeating.
    brain.view = View::new(
        vec![DepartmentRow {
            id: "quant".to_owned(),
            name: "Quant".to_owned(),
            depth: 0,
            live: 0,
            total: 0,
        }],
        BTreeMap::new(),
    );
    brain.view.select("quant");
    brain.view.select_person("analyst");

    brain.tidy_selection(&BTreeSet::new(), &BTreeSet::new());
    brain.tidy_selection(&BTreeSet::new(), &BTreeSet::new());
    brain.tidy_selection(&BTreeSet::new(), &BTreeSet::new());

    let announcements = tmux.calls().iter().filter(|call| call.contains("no longer up")).count();
    assert_eq!(announcements, 1, "one loss is one sentence: {:?}", tmux.calls());
}

/// **THE BLANK-WINDOW REPAIR AGREES WITH THE RAIL.**
///
/// `2026-08-23T15:30:39`, on the operator's own box: `sidebar.focus.minted` →
/// `sidebar.window.blank` (department `executive`) → `sidebar.department.sleeping`
/// for `__overview__:executive`. The seed had just selected `@chief` and their
/// home `executive`, and the repair read only the department. The operator typed
/// `chief`, the rail said `@chief`, and the glass said Executive.
#[tokio::test]
async fn a_blank_window_is_repaired_with_the_selected_person_not_their_department() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    tmux.answer("display-message -p -t org-acme_ #{window_panes}", "1");
    tmux.answer("display-message -p -t org-acme_ -F #{@organization_window_id}", "quant");
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");

    brain.never_blank();

    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| call.contains("sleeping-person-card analyst")),
        "the panel beside the rail says what the rail says is selected: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("department-card")),
        "and NOT the department overview the rail is not pointing at: {calls:?}"
    );
    assert_eq!(brain.view.selected_person(), Some("analyst"), "a repair selects nothing");
}

/// The same repair for a person who IS up: their own window, not a card.
#[tokio::test]
async fn a_blank_window_is_repaired_with_the_selected_person_s_own_pane_when_they_are_up() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    tmux.answer("display-message -p -t org-acme_ #{window_panes}", "1");
    tmux.answer("display-message -p -t org-acme_ -F #{@organization_window_id}", "quant");
    tmux.answer("-F #{@organization_person_id}\t#{pane_dead}", "analyst\t0");
    tmux.answer(
        "#{pane_id}\t#{window_id}\t#{@organization_person_id}\t#{pane_dead}",
        "%44\t@1\tanalyst\t0",
    );
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");

    brain.never_blank();

    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| call.contains("select-pane -t %44")),
        "the operator is taken to the window this person is already alone in: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("sleeping-person-card")),
        "a person who is up is shown, not carded: {calls:?}"
    );
    assert_eq!(
        brain.noticed, None,
        "and the department branch of the repair is never reached: {calls:?}"
    );
}

/// AND THE DEPARTMENT IS STILL THE ANSWER WHEN NO PERSON IS SELECTED.
///
/// `never_blank` keeps the property it was written for — "the right-hand side
/// should never go blank. It's just impossible." — so the case it always
/// repaired must go on being repaired.
#[tokio::test]
async fn a_blank_window_with_no_person_selected_is_still_repaired_with_the_department() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    tmux.answer("display-message -p -t org-acme_ #{window_panes}", "1");
    tmux.answer("display-message -p -t org-acme_ -F #{@organization_window_id}", "quant");
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    let before = tmux.calls().len();

    brain.never_blank();

    let all = tmux.calls();
    let calls: Vec<&String> = all[before..].iter().collect();
    // THE DEPARTMENT OVERVIEW IS WHAT THE REPAIR REACHED FOR, read from the
    // effect's own probes rather than from a field: `show_department_overview`
    // captures the session's canonical geometry and then asks which window
    // carries the department's logical id. Neither probe is on any other
    // branch of `show_selection` in this fixture.
    assert!(
        calls.iter().any(|call| call.contains("list-windows")
            && call.contains("#{window_index}")
            && call.contains("#{@organization_window_id}")),
        "the department's own overview is what a rail-only window falls back to when the \
         rail marks no person: {calls:?}"
    );
    let overview_probe = "list-windows -t org-acme_ -F #{window_id}\t#{@organization_window_id}";
    assert!(
        calls.iter().any(|call| call.as_str() == overview_probe),
        "and it looked for that department's standing overview window: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("sleeping-person-card")),
        "and no person is invented to fill it: {calls:?}"
    );
    // AND A NOTICE THAT NEVER WENT UP IS NOT RECORDED. This fixture cannot
    // mint the overview window — `mint_sleeping_department_window` answers
    // `None` and `show_department_overview` answers `Shown::nothing()` — so
    // `noticed` must stay empty. Recording it here would tell the next pass's
    // asleep-department transition that this department's overview is already
    // on the glass, and that pass would decline to put up the one nobody has
    // seen. `noticed` names the notice that IS up, never the one that was
    // attempted.
    assert_eq!(
        brain.noticed, None,
        "a repair that showed nothing records nothing, so the next pass retries: {calls:?}"
    );
}

/// **THE FOCUS WINDOW IS NOT THIS SWEEP'S BUSINESS**, and adding the selection
/// to the repair must not change that. `ensure_focus_window` has already put the
/// standing notice back this pass, so a rail-only focus window is a state that
/// no longer exists.
#[tokio::test]
async fn a_rail_only_focus_window_is_left_alone_by_the_blank_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    tmux.answer("display-message -p -t org-acme_ #{window_panes}", "1");
    tmux.answer("display-message -p -t org-acme_ -F #{@organization_window_id}", "__focus__");
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");
    let before = tmux.calls().len();

    brain.never_blank();

    let after: Vec<String> = tmux.calls().into_iter().skip(before).collect();
    assert!(
        !after.iter().any(|call| call.contains("sleeping-person-card")
            || call.starts_with("new-window")
            || call.starts_with("select-window")),
        "the parked focus window repairs itself: {after:?}"
    );
}

/// A PERSON MID-WAKE IS EXEMPT, and that is now one field rather than a
/// session option with a sixty-second grace.
///
/// A woken sleeper is neither live NOR desired for the second or two between
/// the wake POST and chiefd granting it. A rail that read that as "not coming
/// back" threw the operator to the CEO a moment after they clicked — reported
/// as "sometimes when I click a sleeping agent it goes to CEO first".
#[tokio::test]
async fn somebody_the_brain_is_waking_is_never_tidied_off_the_glass() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let watching = Arc::new(RecordingTmux::answering(&[
        ("#{@organization_window_id}", "__focus__"),
        ("#{window_id}", "@1"),
    ]));
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) =
        brain_against(&url, root.path(), Arc::clone(&watching), true);
    brain.view.select("quant");
    brain.view.select_person("analyst");
    brain.pending_zoom = None;
    brain.waking.insert("analyst".to_owned());

    brain.tidy_selection(&BTreeSet::new(), &BTreeSet::new());

    assert_eq!(
        brain.view.selected_person(),
        Some("analyst"),
        "they are seconds from coming up; the selection is not somebody else's to clear"
    );
    assert!(
        !watching.calls().iter().any(|call| call.contains("no longer up")),
        "and nothing is announced about a person who has not gone anywhere: {:?}",
        watching.calls()
    );
}

/// A DEPARTMENT WITH A PERSON STARTING IS NOT FULLY ASLEEP.
///
/// The operator can click a sleeper and immediately click their department.
/// The loading panel then stays hidden in the focus window until arrival. A
/// company refresh in that interval must not add an `ASLEEP` body beside the
/// department body: the brain already knows the person is waking and knows
/// their derived home.
#[test]
fn a_refresh_does_not_call_a_selected_department_asleep_while_its_person_is_waking() {
    let tmux = Arc::new(RecordingTmux::answering(&[("list-windows", "@1\tquant\n@7\t__focus__")]));
    let (mut brain, _events) = Brain::new(
        Arc::clone(&tmux) as Arc<dyn Tmux>,
        unreachable_client(),
        "org-acme_".to_owned(),
        PathBuf::from("/company"),
    );
    brain.view = view_of_one_sleeper();
    brain.view.select("quant");
    brain.placement = Some(placement_of_a_two_person_quant());
    brain.waking.insert("analyst".to_owned());

    brain.tidy_selection(&BTreeSet::new(), &BTreeSet::new());

    assert_eq!(brain.noticed, None, "a starting person makes the department not fully asleep");
    assert!(
        !tmux.calls().iter().any(|call| call.starts_with("split-window")),
        "refresh must not add sleeping furniture beside a wake in progress: {:?}",
        tmux.calls()
    );
}

/// A REFUSED WAKE TAKES BACK EVERYTHING THE CLICK PAINTED, in chiefd's own
/// words.
///
/// Optimism is only sound if it is WITHDRAWN when it turns out to be wrong. The
/// operator has already been shown a window, a loading panel and a `starting`
/// row by the time chiefd says no.
#[tokio::test]
async fn a_refused_wake_is_announced_and_every_mark_it_left_is_withdrawn() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    assert!(brain.wake_from_card("%80", "analyst"));

    brain.settle_wake(&WakeAnswer {
        gesture: crate::sidebar::gesture::next(),
        person: "analyst".to_owned(),
        name: "Priya".to_owned(),
        refusal: Some("benched".to_owned()),
    });

    let calls = tmux.calls();
    assert!(
        calls.iter().filter(|call| call.contains("sleeping-person-card analyst")).count() >= 2,
        "a refusal replaces WAKING with the same actionable card: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call.contains("benched")),
        "the backend's exact refusal stays visible on the replacement card: {calls:?}"
    );
    assert!(!brain.waking.contains("analyst"), "the mark is withdrawn");
    assert_eq!(brain.pending_zoom, None, "and so is the zoom nobody is coming for");
    assert!(
        brain.refused.contains("analyst"),
        "and the stale-selection rule is told, so the operator is not thrown to the CEO a \
         tick later for a person they were just refused"
    );
    assert_eq!(
        brain.sleeping_card.as_ref().map(|(person, pane)| (person.as_str(), pane.as_str())),
        Some(("analyst", "%80")),
        "the refused action can be tried again on the same final pane"
    );
}

#[tokio::test]
async fn a_stale_refusal_cannot_rebuild_a_card_after_the_operator_moves_on() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.perform(
        Action::FocusPerson { department_id: "quant".into(), person_id: "analyst".into() },
        crate::sidebar::gesture::next(),
        "%9",
    );
    assert!(brain.wake_from_card("%80", "analyst"));
    brain.sleeping_card = None;
    let before =
        tmux.calls().iter().filter(|call| call.contains("sleeping-person-card analyst")).count();

    brain.settle_wake(&WakeAnswer {
        gesture: crate::sidebar::gesture::next(),
        person: "analyst".into(),
        name: "Priya".into(),
        refusal: Some("late refusal".into()),
    });

    let after =
        tmux.calls().iter().filter(|call| call.contains("sleeping-person-card analyst")).count();
    assert_eq!(after, before, "stale async work cannot replace the current glass");
}

/// A GRANT RELEASES NOTHING BUT ITS OWN OPTIMISM: the row keeps saying
/// `starting` until the PANE arrives.
///
/// Two mechanisms used to do this — a private set cleared by the ANSWER, and a
/// session option with a sixty-second grace cleared by the ARRIVAL. One process
/// needs one set, and it is the arrival that ends the wait, because no CLOCK
/// may end one.
#[tokio::test]
async fn a_granted_wake_keeps_the_row_starting_until_the_pane_actually_exists() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = Arc::new(RecordingTmux::answering(&[]));
    let url = mute_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.waking.insert("analyst".to_owned());

    brain.settle_wake(&WakeAnswer {
        gesture: crate::sidebar::gesture::next(),
        person: "analyst".to_owned(),
        name: "Priya".to_owned(),
        refusal: None,
    });
    assert!(brain.waking.contains("analyst"), "a grant is not an arrival");

    brain.finish_pending_zoom(&BTreeSet::from(["analyst".to_owned()]));
    assert!(!brain.waking.contains("analyst"), "the PANE is what ends the wait");
}

/// A client that could never reach a daemon, for the tests that never make a
/// request. It is a real client aimed at a port nothing listens on, so nothing
/// here can pass by having been mocked.
fn unreachable_client() -> Arc<ActuationClient> {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let client = Arc::new(ActuationClient::new(
        "http://127.0.0.1:1",
        "acme@digest",
        Arc::new(Bearer::operator(&keys_of(root.path()))),
    ));
    std::mem::forget(root);
    client
}

/// **THE CLICK THE OPERATOR MADE, THROUGH THE PATH THEIR MOUSE TAKES.**
///
/// Not `perform`: the raw SGR bytes a thin rail forwards, decoded by the
/// brain's own decoder, hit-tested against the frame it last pushed. The test
/// this replaces called `perform` directly and asserted that SOME tmux call
/// carried the gate's sentence — which a `display-message` satisfies. It
/// passed, and the operator got nothing: `announce` is one line for
/// `display-time` on a session this product runs with `status off`, and the
/// focus body — the pane every other person click writes its answer to, and
/// the pane they were reading — went on showing somebody else's card.
///
/// So the assertion is where the answer has to LAND, and it is the card: the
/// same surface a sleeping click opens, carrying the gate's own sentence.
#[tokio::test]
async fn a_click_on_a_refused_person_puts_the_gates_reason_on_the_focus_body() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.absorb(refused_facts("analyst"));
    let row = brain
        .view
        .tree_rows()
        .iter()
        .position(|row| matches!(row, crate::sidebar::TreeRow::Person(_, person) if person.id == "analyst"))
        .expect("the refused person has a row to click");

    // SGR mouse coordinates are one-based, so the wire says row + 1.
    let keep = brain.input(1, format!("\x1b[<0;2;{}M", row + 1).as_bytes());
    assert!(keep, "a click never detaches the client that made it");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = tmux.calls();
    let carded: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains("respawn-pane") && call.contains("sleeping-person-card"))
        .collect();
    assert_eq!(carded.len(), 1, "the focus body is repainted once, in place: {calls:?}");
    assert!(
        carded[0].contains(CATALOG_REFUSAL),
        "and it carries the gate's own sentence, which names the repair: {:?}",
        carded[0]
    );
    assert!(
        brain
            .carded_refusal
            .as_ref()
            .is_some_and(|(person, reason)| person == "analyst" && reason == CATALOG_REFUSAL),
        "the brain records what the body now says, so the card is not rebuilt every second"
    );
    // AND NOTHING WAS PROMISED. Unchanged by this fix and pinned with it.
    assert!(
        !calls.iter().any(|call| call.starts_with("display-message -t") && call.contains("waking")),
        "nothing is being woken, so no notice may say so: {calls:?}"
    );
    assert_eq!(wake_posts(&url).await, 0, "a refused person is not a wake chiefd can grant");
    assert!(!brain.waking.contains("analyst"), "and no wake is outstanding for them");
}

/// **A CARD THAT SAID `Waking up…` FOR FIVE MINUTES ABOUT A REFUSED PERSON.**
///
/// Measured: 00:52:31Z to 00:57:50Z, about sixty-four converge rounds, with
/// every waking tag present and a live card process — while the rail row one
/// pane away read `refused` the whole time. It cleared only when the operator
/// selected somebody else.
///
/// The rail's own rule, applied to the card: a state that can never advance
/// must stop promising. The company read that carries the refusal is what ends
/// it, so the operator does not have to click anything to be told.
#[tokio::test]
async fn a_card_promising_a_wake_the_gate_refused_becomes_the_refusal() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    // THE SHAPE THE OPERATOR WAS LOOKING AT: this brain's own click asked for
    // the wake, the card took the spinner, and chiefd's gate then declined the
    // launch the wake was for.
    brain.absorb(sleeper_facts(&[]));
    brain.sleeping_card = Some(("analyst".to_owned(), "%80".to_owned()));
    brain.waking.insert("analyst".to_owned());
    brain.pending_zoom = Some("analyst".to_owned());
    let before = tmux.calls().len();

    brain.absorb(refused_facts("analyst"));

    let calls = tmux.calls();
    let carded: Vec<&String> = calls[before..]
        .iter()
        .filter(|call| call.contains("respawn-pane") && call.contains("sleeping-person-card"))
        .collect();
    assert_eq!(carded.len(), 1, "the spinning body is rebuilt once as the refusal: {calls:?}");
    assert!(
        carded[0].contains(CATALOG_REFUSAL),
        "carrying the gate's own sentence rather than a spinner: {:?}",
        carded[0]
    );
    assert!(
        !brain.waking.contains("analyst"),
        "no wake is outstanding for somebody the gate has declined"
    );
    assert_eq!(brain.pending_zoom, None, "and no zoom is pending on a pane that is not coming");
    assert_eq!(
        brain.view.people().iter().find(|row| row.id == "analyst").map(|row| row.state()),
        Some(PersonState::Refused),
        "the row and the card now say the same thing about the same person"
    );
}

/// A COMPANY READ ARRIVES ABOUT ONCE A SECOND and the gate re-derives the same
/// refusal every time. The card is a PROCESS in a pane, so a sweep that acted
/// on every pass would kill and rebuild the operator's card under them for as
/// long as the refusal stood.
#[tokio::test]
async fn a_refused_card_is_not_rebuilt_by_every_company_read() {
    let root = tempfile::tempdir().expect("tempdir");
    staged_operator_key(root.path());
    let tmux = tmux_for_a_sleeper_click();
    let url = hanging_daemon().await;
    let (mut brain, _events, _outbox) = brain_against(&url, root.path(), Arc::clone(&tmux), true);
    brain.absorb(sleeper_facts(&[]));
    brain.sleeping_card = Some(("analyst".to_owned(), "%80".to_owned()));
    brain.absorb(refused_facts("analyst"));
    let settled = tmux.calls().len();

    for _ in 0..3 {
        brain.absorb(refused_facts("analyst"));
    }

    let calls = tmux.calls();
    assert!(
        !calls[settled..]
            .iter()
            .any(|call| call.contains("respawn-pane") && call.contains("sleeping-person-card")),
        "the standing refusal card is left alone: {:?}",
        &calls[settled..]
    );
}
