//! The rail's rules, pinned. Every test here is about a RULE the product
//! states, not about the code running.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use crate::actuate::trust::tags;
use crate::placement;

use super::{
    click as click_at, contrast_foreground, effects, for_session, person_border_format,
    person_display_role, person_first_name, person_short_identity, rail_border_format, Action,
    DepartmentRow, PersonRow, PersonState, RailRefusal, Tmux, View, CEO_DISPLAY_ROLE,
    CONTRAST_ON_DARK, CONTRAST_ON_LIGHT, NO_ACCENT_BACKGROUND, ROOT_DEPARTMENT_DISPLAY_NAME,
    TEAM_MEMBER_DISPLAY_ROLE,
};

/// Existing row-only assertions click the row body. Coordinate-specific tests
/// below call `click_at` directly for the disclosure cell.
fn click(view: &View, height: usize, row: usize) -> Action {
    click_at(view, height, usize::MAX, row)
}

/// A tmux that records every command and answers from a script.
///
/// This is the SIMULATED TMUX COVERAGE `chief/CLAUDE.md` requires for a change
/// to placement: it asserts the exact verbs, in order, that a click turns into.
/// The live half — that these verbs do what this file assumes against a real
/// tmux server — is `a_real_tmux_lays_a_rail_out_beside_its_people` below.
/// `pub(super)` so `rail::tests` can drive `Rail::perform` with the same
/// harness. Both modules are `cfg(test)`; nothing production-side can see it.
pub(super) struct RecordingTmux {
    replies: Mutex<Vec<String>>,
    /// Answers matched by what was ASKED rather than by position.
    ///
    /// A positional script has to be re-counted every time the code under test
    /// grows a read, and one inserted call silently shifts every later answer
    /// onto the wrong question — which is a test that fails for a reason that
    /// has nothing to do with its own claim. These match the first pattern that
    /// is a substring of `verb arg arg …`, so a sequence stays readable as the
    /// facts it states about tmux.
    answers: Mutex<Vec<(String, String)>>,
    calls: Mutex<Vec<Vec<String>>>,
    record_viewport_authority: bool,
}

impl RecordingTmux {
    pub(super) fn new(replies: &[&str]) -> Self {
        Self {
            replies: Mutex::new(replies.iter().rev().map(|s| (*s).to_owned()).collect()),
            answers: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            record_viewport_authority: false,
        }
    }

    /// A tmux that answers BY QUESTION: the first pattern found in the command
    /// wins, and anything unmatched answers empty.
    pub(super) fn answering(answers: &[(&str, &str)]) -> Self {
        Self {
            replies: Mutex::new(Vec::new()),
            answers: Mutex::new(
                answers
                    .iter()
                    .map(|(ask, reply)| ((*ask).to_owned(), (*reply).to_owned()))
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
            record_viewport_authority: false,
        }
    }

    fn recording_viewport_authority(mut self) -> Self {
        self.record_viewport_authority = true;
        self
    }

    /// Every command issued, as `verb arg arg …`, in order.
    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("not poisoned").iter().map(|call| call.join(" ")).collect()
    }

    pub(super) fn answer(&self, pattern: &str, reply: &str) {
        self.answers
            .lock()
            .expect("not poisoned")
            .insert(0, (pattern.to_owned(), reply.to_owned()));
    }

    fn verbs(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("not poisoned")
            .iter()
            .filter_map(|call| call.first().cloned())
            .collect()
    }
}

impl Tmux for RecordingTmux {
    fn run(&self, args: &[&str]) -> String {
        let asked = args.join(" ");
        if args.first() != Some(&"if-shell")
            && asked.contains("@chief_viewport_topology_epoch")
            && asked.contains("display-message")
        {
            if self.record_viewport_authority {
                self.calls
                    .lock()
                    .expect("not poisoned")
                    .push(args.iter().map(|arg| (*arg).to_owned()).collect());
            }
            return "1".to_owned();
        }
        if args.first() == Some(&"run-shell") && asked.contains("@chief_viewport_refresh_command") {
            if self.record_viewport_authority {
                self.calls
                    .lock()
                    .expect("not poisoned")
                    .push(args.iter().map(|arg| (*arg).to_owned()).collect());
            }
            return String::new();
        }
        let matched = self
            .answers
            .lock()
            .expect("not poisoned")
            .iter()
            .find(|(pattern, _)| asked.contains(pattern.as_str()))
            .map(|(_, reply)| reply.clone());
        let matched = matched.or_else(|| {
            (args.first() == Some(&"if-shell"))
                .then(|| {
                    asked
                        .split_whitespace()
                        .find(|word| {
                            word.contains("chief-wake:")
                                || word.contains("chief-external-wake:")
                                || word.contains("chief-retire-card:")
                        })
                        .map(|word| {
                            word.trim_matches(|character: char| {
                                matches!(character, '\'' | '"' | ';')
                            })
                            .to_owned()
                        })
                })
                .flatten()
        });
        if matched.is_none()
            && (asked.contains("#{window_width}\t#{window_height}\t#{@organization_window_id}")
                || asked.contains(
                    "#{window_index}\t#{window_width}\t#{window_height}\t#{@organization_window_id}",
                ))
        {
            return String::new();
        }
        self.calls
            .lock()
            .expect("not poisoned")
            .push(args.iter().map(|a| (*a).to_owned()).collect());
        matched
            .unwrap_or_else(|| self.replies.lock().expect("not poisoned").pop().unwrap_or_default())
    }
}

#[derive(Default)]
struct EmptyInvalidationTmux {
    calls: Mutex<Vec<Vec<String>>>,
}

impl Tmux for EmptyInvalidationTmux {
    fn run(&self, args: &[&str]) -> String {
        self.calls
            .lock()
            .expect("not poisoned")
            .push(args.iter().map(|arg| (*arg).to_owned()).collect());
        String::new()
    }
}

impl EmptyInvalidationTmux {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("not poisoned").iter().map(|call| call.join(" ")).collect()
    }
}

#[test]
fn a_lost_wake_reply_never_accepts_a_foreign_or_stale_claim() {
    for (name, fields) in [
        (
            "foreign claim",
            [
                "org-acme_",
                "acme",
                "__focus__",
                "0",
                "nia",
                "",
                "",
                "",
                "",
                "",
                "other-claim",
                "other-claim",
                "",
                "chief-end",
            ],
        ),
        (
            "wrong session",
            [
                "org-other_",
                "acme",
                "__focus__",
                "0",
                "nia",
                "",
                "",
                "",
                "",
                "",
                "this-claim",
                "this-claim",
                "",
                "chief-end",
            ],
        ),
        (
            "sidebar pane",
            [
                "org-acme_",
                "acme",
                "__focus__",
                "0",
                "nia",
                "",
                "",
                "1",
                "",
                "",
                "this-claim",
                "this-claim",
                "",
                "chief-end",
            ],
        ),
        (
            "CAS did not apply",
            [
                "org-acme_",
                "acme",
                "__focus__",
                "0",
                "",
                "",
                "nia",
                "",
                "",
                "",
                "this-claim",
                "this-claim",
                "",
                "chief-end",
            ],
        ),
    ] {
        let post_state = fields.join("\t");
        let tmux =
            RecordingTmux::answering(&[("if-shell -F -t %80", ""), ("chief-end", &post_state)]);
        assert!(
            !effects::activate_sleeping_focus_with_claim(
                &tmux,
                "org-acme_",
                "acme",
                "%80",
                "nia",
                "this-claim",
            ),
            "{name} must not authorize a wake"
        );
        let calls = tmux.calls();
        let guarded =
            calls.iter().filter(|call| call.starts_with("if-shell -F -t %80")).collect::<Vec<_>>();
        assert_eq!(guarded.len(), 2, "one CAS and one guarded recovery: {calls:?}");
        assert!(
            guarded[1].contains(tags::SLEEPING_PERSON)
                && guarded[1].contains(tags::WAKING_PERSON)
                && guarded[1].contains(tags::WAKE_CLAIM),
            "a failed action restores only its own exact claim so the card stays actionable: {calls:?}"
        );
    }
}

/// What `list-panes` answers for a company with the CEO and one analyst up, in
/// two windows, plus a pane whose person died.
const PANES: &str = "%1\t@0\tchief\t0\n%4\t@1\tanalyst\t0\n%5\t@1\tghost\t1";
/// The same session through the liveness format (`person\tdead`).
const LIVENESS: &str = "chief\t0\nanalyst\t0\nghost\t1\n\t0";
/// The same session through the pane-title format (`pane\tperson\taccent`).
const TITLE_PANES: &str = "%9\t\t\n%4\tanalyst\t#e5c07b\n%1\tchief\t#2b3a55";

fn dept(id: &str, depth: usize, live: usize) -> DepartmentRow {
    DepartmentRow { id: id.to_owned(), name: id.to_owned(), depth, live, total: live }
}

/// A person who is up (`live`) or parked. `desired` follows `live` so the
/// fixture's states read WORKING and SLEEPING, which is the ordinary pair.
fn person(id: &str, live: bool) -> PersonRow {
    PersonRow {
        id: id.to_owned(),
        name: id.to_owned(),
        title: format!("{id} role"),
        live,
        desired: live,
        idle: false,
        crash: None,
        refused: None,
        manager: false,
    }
}

/// A company: an executive root with the CEO up, a quant unit with two people
/// (one still starting), and an empty unit nobody is running.
fn company() -> View {
    let departments = vec![
        dept("executive", 0, 1),
        DepartmentRow {
            id: "quant".to_owned(),
            name: "quant".to_owned(),
            depth: 1,
            live: 1,
            total: 2,
        },
        dept("empty", 1, 0),
    ];
    let mut people = BTreeMap::new();
    people.insert("executive".to_owned(), vec![person("chief", true)]);
    people.insert("quant".to_owned(), vec![person("quant-head", true), person("analyst", false)]);
    people.insert("empty".to_owned(), Vec::new());
    View::new(departments, people)
}

fn department_click_row(view: &View, id: &str) -> usize {
    view.tree_rows()
        .iter()
        .position(
            |row| matches!(row, super::TreeRow::Department(department) if department.id == id),
        )
        .expect("department is in the tree")
        .saturating_sub(view.scroll_offset())
}

fn person_click_rows(view: &View, id: &str) -> [usize; 2] {
    let first = view
        .tree_rows()
        .iter()
        .position(|row| matches!(row, super::TreeRow::Person(_, person) if person.id == id))
        .expect("person is disclosed in the tree")
        .saturating_sub(view.scroll_offset());
    [first, first + 1]
}

// --- where the rail may exist ------------------------------------------------

/// The directory these fence tests stand in, and the session its company
/// projects onto — composed through the production namer so the fence and the
/// actuator cannot drift apart.
fn here() -> &'static std::path::Path {
    std::path::Path::new("/work/acme")
}

fn session_here(slug: &str) -> String {
    crate::placement::session_name_for(slug, &host_primitives::rendezvous::company_key(here()))
}

#[test]
fn the_rail_draws_only_in_its_own_companys_operator_session() {
    assert!(for_session(&session_here("acme"), here()).is_ok());
}

/// THE COMPANY IS THE DIRECTORY, so the fence follows the KEY and not the name.
///
/// Two directories may hold companies called `acme`, and their sessions differ
/// only in the key. The retired fence compared a session against a SLUG it was
/// handed, so a rail standing in `/work/acme` accepted `/elsewhere/acme`'s
/// session — and then read that company with the operator credential, whose
/// scope is unconditional. That is the disclosure this whole fence exists to
/// prevent, aimed at exactly the case the directory made possible.
#[test]
fn a_same_named_companys_session_in_another_directory_is_refused() {
    let elsewhere = std::path::Path::new("/elsewhere/acme");
    let theirs = crate::placement::session_name_for(
        "acme",
        &host_primitives::rendezvous::company_key(elsewhere),
    );
    assert_ne!(theirs, session_here("acme"), "fixture: two directories, two sessions");
    assert!(
        for_session(&theirs, here()).is_err(),
        "a company with the same NAME somewhere else is a different company"
    );
    // And the fence is symmetric: neither may draw in the other's session.
    assert!(for_session(&session_here("acme"), elsewhere).is_err());
}

/// The SLUG is a display word, so the fence is indifferent to it.
///
/// A rename would change every session name under the old comparison and lock
/// the operator out of their own rail until something re-derived it. The key
/// does not change, so the fence does not care.
#[test]
fn the_same_directory_is_accepted_whatever_the_company_is_called() {
    for slug in ["acme", "acme-corp", "renamed-yesterday"] {
        assert!(for_session(&session_here(slug), here()).is_ok(), "{slug}");
    }
}

#[test]
fn the_rail_refuses_every_session_that_is_not_the_operators_company_session() {
    // THE DISCLOSURE FENCE. The rail reads with the OPERATOR bearer, whose
    // scope is unconditional — no subtree narrowing. That is sound only while
    // the viewer IS the operator, which is true in exactly one session. Each
    // refusal below is a place the operator credential would otherwise have
    // shown a whole company's tree to somebody who is not the operator.
    let mine = session_here("acme");
    let ending =
        crate::placement::session_key_suffix(&host_primitives::rendezvous::company_key(here()));
    for session in [
        "chiefd-actuator-box17-4242".to_owned(), // the headless actuator: nobody is attached
        "bash".to_owned(),                       // a bare shell
        crate::placement::session_name_for(
            "acme",
            &host_primitives::rendezvous::company_key(std::path::Path::new("/elsewhere/acme")),
        ), // a DIFFERENT company's session
        mine.trim_end_matches('_').to_owned(),   // the name WITHOUT the terminator
        format!("{mine}x"),                      // and one with anything after it
        String::new(),
    ] {
        let refused = for_session(&session, here()).expect_err("{session} must be refused");
        let RailRefusal::NotACompanySession { session: named, expected } = refused;
        assert_eq!(named, session);
        assert_eq!(expected, ending, "the refusal names the ending it would have accepted");
    }
}

#[test]
fn the_refusal_says_why_rather_than_only_that() {
    let refused = for_session("bash", here()).expect_err("refused");
    let text = refused.to_string();
    assert!(text.contains("operator credential"), "the reason is the credential: {text}");
    assert!(text.contains("unconditional"), "and that its scope is not narrowed: {text}");
}

// --- what the sections contain -----------------------------------------------

#[test]
fn the_people_section_shows_every_person_of_the_department_with_their_state() {
    // It used to show only the LIVE ones, with the rest dimmed beneath and
    // unclickable. The operator ruled that out: a company's sleeping people are
    // the ones you most need to see, and a rail that hides them cannot wake
    // anybody. The states are what tell them apart now.
    let mut view = company();
    view.select("quant");
    let rows: Vec<(&str, PersonState)> =
        view.people().iter().map(|row| (row.id.as_str(), row.state())).collect();
    assert_eq!(
        rows,
        vec![("quant-head", PersonState::Working), ("analyst", PersonState::Sleeping)],
        "everybody is drawn, in canonical order, each saying what it is"
    );
}

#[test]
fn a_department_with_no_live_people_selects_and_shows_nothing_rather_than_refusing() {
    let mut view = company();
    view.select("empty");
    assert_eq!(view.selected(), Some("empty"), "an empty department is still selectable");
    assert!(view.people().is_empty(), "nobody works here at all");
}

#[test]
fn selecting_a_department_that_is_not_in_the_tree_changes_nothing() {
    let mut view = company();
    view.select("no-such-unit");
    assert_eq!(view.selected(), Some("executive"), "the first department stays selected");
}

// --- what happens when the company changes under the rail --------------------

#[test]
fn a_person_who_dies_while_selected_stays_in_the_list_and_stops_claiming_to_be_up() {
    // THE CLAIM MOVED. This test used to assert that a dead person LEFT the
    // list, which was true while `View::people()` returned the live rows only.
    // The operator ruled that out: the people you most need to act on are the
    // ones who are NOT up, and a rail that deletes them cannot be used to wake
    // anybody. A death now moves the row's STATE and never the row.
    let mut view = company();
    view.select("quant");
    assert_eq!(view.people().len(), 2, "every person of the unit is drawn, live or not");

    let mut people = BTreeMap::new();
    people.insert("quant".to_owned(), vec![person("quant-head", false), person("analyst", false)]);
    view.refresh("Acme".to_owned(), vec![dept("quant", 1, 0)], people);

    assert_eq!(
        view.people().iter().map(|row| (row.id.as_str(), row.state())).collect::<Vec<_>>(),
        vec![("quant-head", PersonState::Sleeping), ("analyst", PersonState::Sleeping)],
        "the pane is gone AND chiefd no longer wants them, so both rows read sleeping"
    );
    assert_eq!(view.selected(), Some("quant"), "the department the operator chose is kept");
}

#[test]
fn a_selection_whose_department_left_the_tree_falls_back_rather_than_showing_nothing_forever() {
    let mut view = company();
    view.select("quant");
    view.refresh("Acme".to_owned(), vec![dept("executive", 0, 1)], BTreeMap::new());
    assert_eq!(
        view.selected(),
        Some("executive"),
        "a deleted department must not strand the rail on an id nothing describes"
    );
}

#[test]
fn a_refresh_reclamps_both_scroll_offsets_against_the_lists_that_replaced_them() {
    let departments: Vec<DepartmentRow> = (0..20).map(|i| dept(&format!("d{i}"), 0, 1)).collect();
    let mut view = View::new(departments, BTreeMap::new());
    view.scroll(15);
    assert_eq!(view.scroll_offset(), 15);

    view.refresh("Acme".to_owned(), vec![dept("d0", 0, 1), dept("d1", 0, 1)], BTreeMap::new());
    assert_eq!(
        view.scroll_offset(),
        3,
        "an offset into a list that shrank would draw a blank rail"
    );
}

// --- scrolling ---------------------------------------------------------------

#[test]
fn each_section_scrolls_on_its_own_and_neither_runs_off_either_end() {
    let departments: Vec<DepartmentRow> = (0..10).map(|i| dept(&format!("d{i}"), 0, 1)).collect();
    let mut people = BTreeMap::new();
    people.insert(
        "d0".to_owned(),
        (0..4).map(|i| person(&format!("p{i}"), true)).collect::<Vec<_>>(),
    );
    let mut view = View::new(departments, people);

    view.scroll(3);
    assert_eq!(view.scroll_offset(), 3);

    view.scroll(100);
    assert_eq!(view.scroll_offset(), 27, "clamped at the last tree line");
    view.scroll(-100);
    assert_eq!(view.scroll_offset(), 0, "and at the first");
}

#[test]
fn department_body_and_disclosure_clicks_keep_nonzero_scroll_and_visible_rows() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut people = BTreeMap::new();
    people.insert(
        "d0".to_owned(),
        vec![PersonRow {
            id: "alpha".to_owned(),
            name: "Alpha".to_owned(),
            title: "Alpha role".to_owned(),
            live: true,
            desired: true,
            idle: false,
            crash: None,
            refused: None,
            manager: false,
        }],
    );
    people.insert("d2".to_owned(), vec![person("charlie", true)]);
    let mut view = View::new(
        vec![dept("d0", 0, 1), dept("d1", 1, 0), dept("d2", 1, 1), dept("d3", 1, 0)],
        people,
    );
    view.scroll(4);

    const WIDTH: u16 = 30;
    const HEIGHT: u16 = 5;
    let visible_keys = |view: &View| {
        view.tree_rows()
            .into_iter()
            .skip(view.scroll_offset())
            .take(usize::from(HEIGHT.saturating_sub(1)))
            .map(|row| match row {
                super::TreeRow::DepartmentSpacer(department) => {
                    format!("spacer:{}", department.id)
                }
                super::TreeRow::Department(department) => format!("department:{}", department.id),
                super::TreeRow::Person(_, person) => format!("person:{}", person.id),
                super::TreeRow::Role(_, person) => format!("role:{}", person.id),
            })
            .collect::<Vec<_>>()
    };
    let rendered_keys = |view: &View| {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, view, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..HEIGHT.saturating_sub(1))
            .map(|row| {
                let text =
                    (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>();
                if text.trim().is_empty() {
                    "spacer"
                } else if text.contains("d1") {
                    "department:d1"
                } else if text.contains("d2") {
                    "department:d2"
                } else {
                    panic!("unexpected visible row {row}: {text:?}")
                }
            })
            .collect::<Vec<_>>()
    };
    let expected = vec![
        "spacer:d1".to_owned(),
        "department:d1".to_owned(),
        "spacer:d2".to_owned(),
        "department:d2".to_owned(),
    ];
    let assert_place = |view: &View| {
        assert_eq!(view.scroll_offset(), 4, "a click must not own the scroll offset");
        assert_eq!(visible_keys(view), expected, "the same logical rows stay in view");
        assert_eq!(rendered_keys(view), ["spacer", "department:d1", "spacer", "department:d2"]);
    };
    assert_place(&view);

    // `d2` is a top-level department (depth one), and the ROOT COSTS NO LEVEL,
    // so its disclosure shares the root's column one and a body click must land
    // past it, at column five.
    let body = click_at(&view, usize::from(HEIGHT), 5, 3);
    assert_eq!(body, Action::SelectDepartment("d2".to_owned()));
    view.select("d2");
    assert!(view.is_expanded("d2"), "a collapsed body click still expands");
    assert_place(&view);

    assert_eq!(click_at(&view, usize::from(HEIGHT), 5, 3), body);
    view.select("d2");
    assert!(view.is_expanded("d2"), "an expanded body click keeps the branch open");
    assert_place(&view);

    let disclosure = Action::ToggleDepartmentDisclosure("d2".to_owned());
    assert_eq!(click_at(&view, usize::from(HEIGHT), 1, 3), disclosure);
    view.toggle_department_disclosure("d2");
    assert!(!view.is_expanded("d2"));
    assert_place(&view);

    assert_eq!(click_at(&view, usize::from(HEIGHT), 1, 3), disclosure);
    view.toggle_department_disclosure("d2");
    assert!(view.is_expanded("d2"));
    assert_place(&view);

    view.scroll(1);
    assert_eq!(view.scroll_offset(), 5, "explicit scroll input still changes the offset");
}

// --- a person who died while the rail was not looking ------------------------

#[test]
fn a_person_whose_pane_died_stays_clickable_and_stops_claiming_to_be_running() {
    // THE REPORTED BUG. Liveness is TMUX's fact and chiefd emits no event for
    // it, so the changefeed CANNOT wake the rail when a pane dies. The row
    // stayed drawn, stayed clickable, and every click resolved to no pane and
    // did nothing — which the operator read as "it kicks me back to CEO",
    // because doing nothing leaves the screen wherever tmux already was.
    let mut view = company();
    view.select("quant");
    let first_person_row = person_click_rows(&view, "quant-head")[0];
    assert_eq!(
        click(&view, 21, first_person_row),
        Action::FocusPerson {
            department_id: "quant".to_owned(),
            person_id: "quant-head".to_owned()
        },
        "drawn live, so clickable"
    );

    // tmux now says that pane is gone. Nobody told chiefd, and chiefd will
    // never tell the rail.
    view.set_live(&BTreeSet::new());
    assert_eq!(
        click(&view, 21, first_person_row),
        Action::FocusPerson {
            department_id: "quant".to_owned(),
            person_id: "quant-head".to_owned()
        },
        "the row is still THERE — every person is drawn now — so it stays a target"
    );
    assert_eq!(
        view.people()[0].state(),
        PersonState::Starting,
        "but it no longer claims to be running, which is the lie that was fixed. \
         STARTING and not SLEEPING: chiefd still WANTS this person up, and a pane that \
         died is exactly what the next converge pass is about to replace. `PersonRow::state` \
         decides this from the pane, never from the settle clock — a clock-derived state \
         would call a corpse WORKING for the whole 300s activity window"
    );
}

/// A person the ACTUATOR has given up on stops claiming to be on their way.
///
/// # The lie this ends
///
/// `starting` is a promise that something is coming. When the actuator has hit
/// the crash-loop limit it has dropped that person from placement entirely, so
/// nothing is coming and nothing ever will until an operator acts. On the
/// operator's own company thirteen people wore `starting` for twenty minutes
/// while the actuator printed `this plan asked for NOTHING` once a second —
/// the rail reporting a state that could not advance, which is the worst of
/// both worlds: no diagnosis and no progress.
///
/// The three inputs are unchanged and so is their precedence. HELD is read
/// only where STARTING used to be the only answer: desired, and no pane.
#[test]
fn a_person_whose_boot_keeps_dying_reads_crashing_and_not_starting() {
    let roster = roster();
    let desired: BTreeSet<String> = ["chief"].map(str::to_owned).into_iter().collect();
    let held: BTreeMap<String, super::CrashNotice> = [(
        "chief".to_owned(),
        super::CrashNotice {
            failures: 11,
            elapsed: "4m 12s".to_owned(),
            retry_in: "10s".to_owned(),
            last_error: Some("pi exited during extension bind".to_owned()),
        },
    )]
    .into_iter()
    .collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &held,
        &BTreeMap::new(),
    );
    let row = &people["executive"][0];
    assert_eq!(
        row.state(),
        PersonState::Crashing,
        "chiefd still wants them and there is still no pane — but the actuator has stopped \
         trying, and that is the difference the operator is looking at the row for"
    );
    assert_eq!(row.state().tag(), "crashing", "and it says so in the notices too");

    // THE SAME PERSON, NOT HELD. One input changed, and the row is a promise
    // again.
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(
        people["executive"][0].state(),
        PersonState::Starting,
        "held must not swallow the ordinary case: a desired, paneless person the actuator is \
         still working on is STARTING exactly as before"
    );

    // AND LIVENESS STILL WINS. A held person whose pane came back is up; the
    // hold is the actuator's intention, never a claim about tmux.
    let live: BTreeSet<String> = ["chief"].map(str::to_owned).into_iter().collect();
    let (_, people) =
        super::project(&roster, &desired, &live, &BTreeSet::new(), &held, &BTreeMap::new());
    assert_eq!(
        people["executive"][0].state(),
        PersonState::Working,
        "a pane settles WORKING vs IDLE and nothing else is consulted"
    );
}

/// The one reason a refusal fixture uses, verbatim from chiefd's gate.
///
/// Two filenames and a home, which is what makes a refusal actionable rather
/// than merely accurate. The test asserts the WHOLE string reaches the row: the
/// rail must not summarize the only part the operator can act on.
const GATE_REFUSAL: &str =
    "required files 'settings.json' and 'agent.md' are missing from home '/companies/acme/chief'";

/// A person CHIEFD'S LAUNCH GATE has refused never claims to be on their way.
///
/// # The lie this ends
///
/// `starting` is a promise: chiefd wants this person and something is coming
/// for them. A person the gate has declined is one nobody is going to try —
/// the daemon re-derives the refusal from the disk on every single pass, and it
/// does not clear because time passed. The row said `starting` about them
/// anyway, on every pass, for ever, because the desired set is built with no
/// launch gate at all and `desired && !live` had nowhere else to go.
///
/// A refusal is now its own cell, and it carries the GATE'S OWN SENTENCE.
#[test]
fn a_person_the_launch_gate_refused_reads_refused_and_never_starting() {
    let roster = roster();
    let desired: BTreeSet<String> = ["chief"].map(str::to_owned).into_iter().collect();
    let refusals: BTreeMap<String, String> =
        [("chief".to_owned(), GATE_REFUSAL.to_owned())].into_iter().collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &refusals,
    );
    let row = &people["executive"][0];
    assert_eq!(
        row.state(),
        PersonState::Refused,
        "chiefd wants them and its own gate will not publish them a launch spec; that is not          a person on their way"
    );
    assert_eq!(row.state().tag(), "refused", "and it says so in one word an operator can act on");
    assert_eq!(
        row.refused.as_deref(),
        Some(GATE_REFUSAL),
        "the gate's reason travels verbatim — chiefd is the only process that can see the disk          it is about, and a rewrite would drop the two filenames and the home"
    );

    // THE SAME PERSON, NOT REFUSED. One input changed, and the row is a
    // promise again — refused must not swallow the ordinary case.
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(people["executive"][0].state(), PersonState::Starting);

    // AND LIVENESS STILL WINS. A refusal is chiefd's answer about launching
    // somebody, never a claim about a pane that already exists.
    let live: BTreeSet<String> = ["chief"].map(str::to_owned).into_iter().collect();
    let (_, people) =
        super::project(&roster, &desired, &live, &BTreeSet::new(), &BTreeMap::new(), &refusals);
    assert_eq!(people["executive"][0].state(), PersonState::Working);
}

/// A refused person is WANTED and BLOCKED, which is neither of the two states
/// the rail could say before.
///
/// This is the shape decision, pinned. Excluding refused people from the
/// desired set would have made this row `sleeping` — the word for somebody the
/// operator parked — and sent them to un-park a person nobody had parked, while
/// the actuator quietly stopped planning for them. The row therefore keeps
/// `desired`, and the refusal sits beside it.
///
/// NOTE: the `!= Sleeping` half of this passes on the reverted tree as well. It
/// pins the design against the OTHER available fix, not the fix itself.
#[test]
fn a_refused_person_is_still_wanted_and_never_reads_asleep() {
    let roster = roster();
    let desired: BTreeSet<String> = ["chief"].map(str::to_owned).into_iter().collect();
    let refusals: BTreeMap<String, String> =
        [("chief".to_owned(), GATE_REFUSAL.to_owned())].into_iter().collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &refusals,
    );
    let row = &people["executive"][0];
    assert!(row.desired, "chiefd still wants this person, and the rail must keep saying so");
    assert_ne!(
        row.state(),
        PersonState::Sleeping,
        "a blocked person is not a parked one; `sleeping` invites the wrong repair"
    );
}

/// A REFUSAL OUTRANKS A CRASH-LOOP HOLD, because it names the repair.
///
/// Both mean "chiefd wants them and nothing is coming". They differ in which
/// one is the LIVE cause: a hold is this actuator's verdict about boots it
/// attempted, and no boot is attempted at all for a person the gate refuses, so
/// the hold is a record of an older question. The refusal is re-derived this
/// very pass and says what to fix.
#[test]
fn a_refusal_outranks_a_crash_report_because_only_one_of_them_names_a_repair() {
    let roster = roster();
    let desired: BTreeSet<String> = ["chief"].map(str::to_owned).into_iter().collect();
    let held: BTreeMap<String, super::CrashNotice> = [(
        "chief".to_owned(),
        super::CrashNotice {
            failures: 11,
            elapsed: "4m 12s".to_owned(),
            retry_in: "10s".to_owned(),
            last_error: Some("pi exited during extension bind".to_owned()),
        },
    )]
    .into_iter()
    .collect();
    let refusals: BTreeMap<String, String> =
        [("chief".to_owned(), GATE_REFUSAL.to_owned())].into_iter().collect();
    let (_, people) =
        super::project(&roster, &desired, &BTreeSet::new(), &BTreeSet::new(), &held, &refusals);
    assert_eq!(people["executive"][0].state(), PersonState::Refused);

    // AND THE HOLD IS UNTOUCHED WHERE IT IS THE ONLY ANSWER. A crash-held
    // person the gate is happy with still reads `held`.
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &held,
        &BTreeMap::new(),
    );
    assert_eq!(people["executive"][0].state(), PersonState::Crashing);
}

#[test]
fn re_reading_liveness_revives_a_person_whose_pane_came_back() {
    // The same fact in the other direction: a person tmux has STARTED becomes
    // live without waiting for chiefd to say anything, because chiefd never
    // will.
    let mut view = company();
    view.select("quant");
    view.set_live(&BTreeSet::new());
    assert!(view.people().iter().all(|row| !row.state().is_live()));

    let back: BTreeSet<String> = ["quant-head", "analyst"].map(str::to_owned).into_iter().collect();
    view.set_live(&back);
    assert_eq!(
        view.people().iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["quant-head", "analyst"],
        "both are live now, in canonical order"
    );
    assert!(
        view.people().iter().all(|row| row.state().is_live()),
        "and both say so: a re-read moves the STATE, not just a flag"
    );
}

#[test]
fn re_reading_liveness_recounts_every_departments_live_column() {
    // The department rows carry a LIVE count, and a stale count is the same
    // class of lie as a stale row.
    let mut view = company();
    assert_eq!(view.departments()[1].live, 1, "quant starts with one live person");
    assert_eq!(view.departments()[1].total, 2, "both rostered quant people count in total");
    view.set_live(&BTreeSet::new());
    assert!(
        view.departments().iter().all(|dept| dept.live == 0),
        "every count follows the same re-read: {:?}",
        view.departments()
    );
    assert_eq!(
        view.departments().iter().map(|dept| dept.total).collect::<Vec<_>>(),
        vec![1, 2, 0],
        "a tmux liveness refresh changes no roster-derived total"
    );
}

#[test]
fn re_reading_liveness_reclamps_a_scroll_offset_the_shorter_list_no_longer_has() {
    let mut people = BTreeMap::new();
    people
        .insert("quant".to_owned(), vec![person("a", true), person("b", true), person("c", true)]);
    let mut view = View::new(vec![dept("quant", 0, 3)], people);
    view.scroll(2);
    assert_eq!(view.scroll_offset(), 2, "scrolled to the last tree line");
    // The people list no longer shrinks when somebody parks — every person is
    // drawn — so the offset SURVIVES a liveness re-read. That is the point:
    // clicking a row must not scroll the list out from under the operator.
    let one: BTreeSet<String> = ["a"].map(str::to_owned).into_iter().collect();
    view.set_live(&one);
    assert_eq!(view.scroll_offset(), 2, "the tree is the same length");
    // A REFRESH that genuinely drops people is what re-clamps it.
    view.refresh("Acme".to_owned(), vec![dept("quant", 0, 1)], BTreeMap::new());
    assert_eq!(view.scroll_offset(), 1, "the remaining department is the final tree row");
}

// --- one selection, however many rails --------------------------------------
//
// THE DEFECT, in the operator's words: "I have to click twice on the department
// to move the purple `>`." The first click always moved a marker — in the rail
// they clicked, which `show_department` then switched the glass AWAY from. What
// they were left looking at was a DIFFERENT rail process, with its own `View`,
// still marking whatever it had last been told. Clicking the same row again
// moved that one, which is the second click.
//
// The selection is the operator's, not the window's, so it is recorded in a
// session option every rail reads.

// --- what is marked as selected ----------------------------------------------

#[test]
fn selecting_a_department_marks_it_and_leaves_the_people_unmarked() {
    // The operator's rule: "If I select the department, the selection is on the
    // department side; the people are clear and there's no selection."
    let mut view = company();
    view.select_person("chief");
    assert_eq!(view.selected_person(), Some("chief"));

    view.select("quant");
    assert_eq!(view.selected(), Some("quant"));
    assert_eq!(
        view.selected_person(),
        None,
        "picking a department clears the person mark, so only one row is ever marked"
    );
}

#[test]
fn clicking_a_person_moves_the_mark_to_that_person() {
    let mut view = company();
    view.select("quant");
    assert_eq!(view.selected_person(), None, "a fresh department selection marks no person");
    view.select_person("quant-head");
    assert_eq!(view.selected_person(), Some("quant-head"));
    assert_eq!(view.selected(), Some("quant"), "the department stays the filter");
}

#[test]
fn the_company_tree_places_two_clickable_lines_under_each_expanded_department() {
    let mut view = company();
    view.select("quant");
    let rows = view.tree_rows();
    let quant = rows
        .iter()
        .position(
            |row| matches!(row, super::TreeRow::Department(department) if department.id == "quant"),
        )
        .expect("quant is in the tree");
    assert!(
        matches!(rows[quant + 1], super::TreeRow::Person(_, person) if person.id == "quant-head")
    );
    assert!(
        matches!(rows[quant + 2], super::TreeRow::Role(_, person) if person.id == "quant-head")
    );
    assert_eq!(
        click(&view, 21, quant + 1 - view.scroll_offset()),
        Action::FocusPerson {
            department_id: "quant".to_owned(),
            person_id: "quant-head".to_owned()
        }
    );
    assert_eq!(
        click(&view, 21, quant + 2 - view.scroll_offset()),
        Action::FocusPerson {
            department_id: "quant".to_owned(),
            person_id: "quant-head".to_owned()
        }
    );
}

#[test]
fn every_root_nested_and_sibling_department_has_one_non_clickable_blank_row() {
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};
    use ratatui::Terminal;

    let departments = vec![
        dept("executive", 0, 1),
        dept("research", 1, 0),
        dept("lab", 2, 1),
        dept("sales", 1, 0),
    ];
    let mut people = BTreeMap::new();
    people.insert("executive".to_owned(), vec![person("chief", true)]);
    people.insert("lab".to_owned(), vec![person("chemist", true)]);
    let mut view = View::new(departments, people);
    view.select("lab");

    let keys = view
        .tree_rows()
        .iter()
        .map(|row| match row {
            super::TreeRow::DepartmentSpacer(department) => format!("blank:{}", department.id),
            super::TreeRow::Department(department) => format!("department:{}", department.id),
            super::TreeRow::Person(department, person) => {
                format!("person:{}:{}", department.id, person.id)
            }
            super::TreeRow::Role(department, person) => {
                format!("role:{}:{}", department.id, person.id)
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "blank:executive",
            "department:executive",
            "person:executive:chief",
            "role:executive:chief",
            "blank:research",
            "department:research",
            "blank:lab",
            "department:lab",
            "person:lab:chemist",
            "role:lab:chemist",
            "blank:sales",
            "department:sales",
        ],
        "each department owns exactly one blank row, after the prior branch and before its card"
    );

    const WIDTH: u16 = 30;
    const HEIGHT: u16 = 13;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("test terminal");
    terminal.draw(|frame| super::render::draw_with_appearance(frame, &view, true)).expect("draw");
    let buffer = terminal.backend().buffer();
    for (row, key) in keys.iter().enumerate() {
        let row = u16::try_from(row).expect("small row");
        let text = (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>();
        if key.starts_with("blank:") {
            assert!(text.trim().is_empty(), "{key} must draw no text: {text:?}");
            for column in 0..WIDTH {
                assert_eq!(buffer[(column, row)].bg, Color::Reset, "{key} has no container");
                assert_eq!(buffer[(column, row)].modifier, Modifier::empty(), "{key} has no style");
            }
            assert_eq!(click_at(&view, usize::from(HEIGHT), 1, usize::from(row)), Action::Ignored);
            assert_eq!(click_at(&view, usize::from(HEIGHT), 8, usize::from(row)), Action::Ignored);
        } else if let Some(id) = key.strip_prefix("department:") {
            assert!(text.contains(id), "{key} must draw its department card: {text:?}");
            // The disclosure indents with the department, so it lives at the
            // depth-derived column, never a fixed one — and the ROOT COSTS NO
            // LEVEL, so a top-level department shares the root's column one.
            // Column 8 is a label cell for all of these (the deepest disclosure
            // is column 3), so it is navigation everywhere.
            let disclosure_column = match id {
                "executive" | "research" | "sales" => 1,
                "lab" => 3,
                other => panic!("unexpected department {other}"),
            };
            assert_eq!(
                click_at(&view, usize::from(HEIGHT), 8, usize::from(row)),
                Action::SelectDepartment(id.to_owned())
            );
            assert_eq!(
                click_at(&view, usize::from(HEIGHT), disclosure_column, usize::from(row)),
                Action::ToggleDepartmentDisclosure(id.to_owned())
            );
        }
    }
}

#[test]
fn both_person_lines_carry_their_owning_department_when_another_department_is_selected() {
    let mut view = company();
    view.select("quant");
    view.select("executive");
    assert_eq!(view.selected(), Some("executive"));
    let expected = Action::FocusPerson {
        department_id: "quant".to_owned(),
        person_id: "quant-head".to_owned(),
    };
    let [identity, role] = person_click_rows(&view, "quant-head");
    assert_eq!(click(&view, 21, identity), expected);
    assert_eq!(click(&view, 21, role), expected);
}

#[test]
fn department_row_selection_always_expands_and_only_disclosure_toggles() {
    let mut view = company();
    view.select("quant");
    assert!(view.is_expanded("quant"));
    view.select_person("quant-head");
    view.select("quant");
    assert_eq!(view.selected(), Some("quant"));
    assert_eq!(view.selected_person(), None, "a department row returns to its grid");
    assert!(view.is_expanded("quant"));
    view.select("quant");
    assert!(view.is_expanded("quant"), "a repeated row click never collapses the branch");

    view.toggle_department_disclosure("quant");
    assert!(!view.is_expanded("quant"));
    view.refresh(
        "Acme".to_owned(),
        vec![dept("executive", 0, 1), dept("quant", 1, 1), dept("empty", 1, 0)],
        view.everybody().clone(),
    );
    assert!(!view.is_expanded("quant"), "a refresh does not undo a human collapse");
    view.toggle_department_disclosure("quant");
    assert!(view.is_expanded("quant"));
}

#[test]
fn only_the_disclosure_cell_toggles_a_department() {
    let mut view = company();
    let executive = department_click_row(&view, "executive");
    assert_eq!(
        click_at(&view, 21, 1, executive),
        Action::ToggleDepartmentDisclosure("executive".to_owned()),
        "the root disclosure is in column one"
    );

    view.select("quant");
    let quant = department_click_row(&view, "quant");
    assert_eq!(
        click_at(&view, 21, 1, quant),
        Action::ToggleDepartmentDisclosure("quant".to_owned()),
        "a top-level department's disclosure shares the root's column one"
    );
    for column in [0, 2, 3, 4, 20] {
        assert_eq!(
            click_at(&view, 21, column, quant),
            Action::SelectDepartment("quant".to_owned()),
            "column {column} is department navigation, not disclosure"
        );
    }

    view.toggle_department_disclosure("quant");
    assert!(!view.is_expanded("quant"));
    view.select("quant");
    assert!(view.is_expanded("quant"), "a row click reopens a collapsed department");
}

#[test]
fn the_selection_palette_tracks_explicit_and_automatic_terminal_appearance() {
    use ratatui::style::Color;

    assert!(crate::appearance::is_light(None, Some("light"), Some("15;0")));
    assert!(!crate::appearance::is_light(None, Some("dark"), Some("0;15")));
    assert!(
        crate::appearance::is_light(None, Some("auto"), Some("0;15")),
        "Automatic uses the actual terminal background"
    );
    assert!(
        !crate::appearance::is_light(Some("dark\n"), Some("auto"), Some("0;15")),
        "the live browser theme overrides a frozen Light-looking process environment"
    );
    assert!(
        crate::appearance::is_light(Some("light"), Some("dark"), Some("0;0")),
        "the live browser theme also overrides an explicit stale Dark environment"
    );
    assert!(
        crate::appearance::is_light(Some("invalid"), Some("auto"), Some("0;15")),
        "an invalid bridge value uses the portable environment fallback"
    );
    for invalid in [Some(""), Some("auto"), Some("LIGHT"), Some("unknown"), None] {
        assert!(
            crate::appearance::is_light(invalid, Some("light"), Some("0;0")),
            "invalid or missing live theme {invalid:?} falls back to the Light environment"
        );
        assert!(
            !crate::appearance::is_light(invalid, Some("dark"), Some("0;15")),
            "invalid or missing live theme {invalid:?} falls back to the Dark environment"
        );
    }
    let light = super::render::selection_style_for(true);
    assert_eq!(light.bg, Some(Color::Rgb(0xed, 0xe7, 0xf6)));
    assert_eq!(light.fg, Some(Color::Rgb(0x5b, 0x21, 0xb6)));
    let dark = super::render::selection_style_for(false);
    assert_eq!(dark.bg, Some(Color::Rgb(0x2e, 0x10, 0x65)));
    assert_eq!(dark.fg, Some(Color::Rgb(0xd8, 0xb4, 0xfe)));
}

#[test]
fn selected_person_container_has_equal_edge_padding_and_exact_palettes_on_both_lines() {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    for (light, foreground, background) in [
        (true, Color::Rgb(0x5b, 0x21, 0xb6), Color::Rgb(0xed, 0xe7, 0xf6)),
        (false, Color::Rgb(0xd8, 0xb4, 0xfe), Color::Rgb(0x2e, 0x10, 0x65)),
    ] {
        let mut view = company();
        view.select("quant");
        view.select_person("quant-head");
        let mut terminal = Terminal::new(TestBackend::new(30, 14)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, &view, light))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let identity = (0..14)
            .find(|row| {
                (0..30)
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("quant-head")
            })
            .expect("person identity line");
        for row in [identity, identity + 1] {
            assert_eq!(buffer[(0, row)].symbol(), " ", "one left padding cell on row {row}");
            assert_eq!(buffer[(29, row)].symbol(), " ", "one right padding cell on row {row}");
            for column in 0..30 {
                assert_eq!(buffer[(column, row)].bg, background);
                // `quant` is a TOP-LEVEL department and the root costs no
                // level, so its people are flush: the status cell sits at
                // column 1, the same column the root's people use.
                let expected = if row == identity && column == 1 {
                    if light {
                        Color::Rgb(0x00, 0x5e, 0x00)
                    } else {
                        Color::Rgb(0x00, 0xc5, 0x00)
                    }
                } else {
                    foreground
                };
                assert_eq!(buffer[(column, row)].fg, expected);
            }
        }
        assert_eq!(buffer[(1, identity)].symbol(), "\u{25cf}", "the selected status stays visible");
        assert_eq!(buffer[(3, identity)].symbol(), "q", "the name follows the status column");
        assert_eq!(buffer[(3, identity + 1)].symbol(), "q", "the title aligns with the name");

        view.select("quant");
        let mut terminal = Terminal::new(TestBackend::new(30, 14)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, &view, light))
            .expect("draw department reset");
        let buffer = terminal.backend().buffer();
        let department = (0..14)
            .find(|row| {
                (0..30)
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("− quant")
            })
            .expect("selected department line");
        for column in 0..30 {
            assert_eq!(buffer[(column, department)].bg, background);
            assert_eq!(buffer[(column, department)].fg, foreground);
            assert_ne!(buffer[(column, identity)].bg, background);
            assert_ne!(buffer[(column, identity + 1)].bg, background);
        }
    }
}

#[test]
fn selected_department_container_uses_each_exact_palette_across_the_full_width() {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    for (light, foreground, background) in [
        (true, Color::Rgb(0x5b, 0x21, 0xb6), Color::Rgb(0xed, 0xe7, 0xf6)),
        (false, Color::Rgb(0xd8, 0xb4, 0xfe), Color::Rgb(0x2e, 0x10, 0x65)),
    ] {
        let view = company();
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, &view, light))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let selected_row = (0..12)
            .find(|row| {
                (0..30)
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("executive")
            })
            .expect("selected department");
        assert_eq!(buffer[(0, selected_row)].symbol(), " ");
        assert_eq!(buffer[(29, selected_row)].symbol(), " ");
        for column in 0..30 {
            assert_eq!(buffer[(column, selected_row)].bg, background);
            assert_eq!(buffer[(column, selected_row)].fg, foreground);
        }
    }
}

#[test]
fn a_department_badge_draws_live_over_total_in_light_and_dark() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let view = View::new(
        vec![DepartmentRow {
            id: "research".to_owned(),
            name: "Research".to_owned(),
            depth: 0,
            live: 1,
            total: 5,
        }],
        BTreeMap::new(),
    );
    for light in [true, false] {
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, &view, light))
            .expect("draw department count");
        let buffer = terminal.backend().buffer();
        let screen = (0..6)
            .map(|row| (0..30).map(|column| buffer[(column, row)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Research"), "the department label is visible:\n{screen}");
        assert!(screen.contains("1/5"), "the badge shows active over total:\n{screen}");
    }
}

#[test]
fn one_blank_row_precedes_normal_order_and_only_department_labels_gain_bold() {
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::Terminal;

    for light in [true, false] {
        let view = company();
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, &view, light))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let row_text =
            |row: u16| (0..30).map(|column| buffer[(column, row)].symbol()).collect::<String>();
        let named_row = |name: &str| {
            (0..12).find(|row| row_text(*row).contains(name)).expect("named row is visible")
        };
        let label_columns = |row: u16, label: &str| {
            let text = row_text(row);
            let byte = text.find(label).expect("label is visible");
            let start = text[..byte].chars().count();
            start..start + label.chars().count()
        };

        assert!(row_text(0).trim().is_empty(), "row zero is exact blank padding");
        assert_eq!(named_row("executive"), 1, "Executive follows normal tree order at row one");
        assert_eq!(click(&view, 12, 1), Action::SelectDepartment("executive".to_owned()));

        for label in ["executive", "quant", "empty"] {
            let row = named_row(label);
            for column in label_columns(row, label) {
                assert!(
                    buffer[(u16::try_from(column).expect("small column"), row)]
                        .modifier
                        .contains(Modifier::BOLD),
                    "{label} is bold in the selected or unselected {light:?} frame"
                );
            }
        }

        let quant = named_row("quant");
        assert!(
            !buffer[(1, quant)].modifier.contains(Modifier::BOLD),
            "an unselected disclosure keeps its prior style"
        );
        assert!(
            !buffer[(28, quant)].modifier.contains(Modifier::BOLD),
            "an unselected live count keeps its prior style"
        );
        let chief = named_row("chief");
        for column in label_columns(chief, "chief") {
            assert!(
                !buffer[(u16::try_from(column).expect("small column"), chief)]
                    .modifier
                    .contains(Modifier::BOLD),
                "person names do not gain department emphasis"
            );
        }
    }
}

/// The icon is now the whole status label. Its shape, colour, fixed column,
/// and selected-card behavior are one product rule, so this checks the cells
/// rather than only the text that produced them.
#[test]
fn every_person_state_uses_the_fixed_status_column_without_a_right_side_word() {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    let departments =
        vec![DepartmentRow { id: "team".into(), name: "Team".into(), depth: 2, live: 2, total: 4 }];
    let states = [
        ("worker", "Worker", true, true, false, "\u{25cf}"),
        ("idler", "Idler", true, true, true, "\u{25ce}"),
        ("starter", "Starter", false, true, false, "\u{25cc}"),
        ("sleeper", "Sleeper", false, false, false, "\u{25cf}"),
    ];
    let mut people = BTreeMap::new();
    people.insert(
        "team".to_owned(),
        states
            .iter()
            .map(|(id, name, live, desired, idle, _)| PersonRow {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                title: format!("{name} title"),
                live: *live,
                desired: *desired,
                idle: *idle,
                crash: None,
                refused: None,
                manager: false,
            })
            .collect(),
    );
    let mut view = View::new(departments, people);
    view.select("team");

    const WIDTH: u16 = 30;
    const HEIGHT: u16 = 16;
    let luminance = |color: Color| {
        let Color::Rgb(red, green, blue) = color else {
            panic!("card colors must be resolved truecolor values: {color:?}");
        };
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    };
    let contrast = |left: Color, right: Color| {
        let left = luminance(left);
        let right = luminance(right);
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    };
    let draw = |view: &View, light: bool| {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, view, light))
            .expect("draw");
        terminal.backend().buffer().clone()
    };
    let row_text = |buffer: &ratatui::buffer::Buffer, row: u16| {
        (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>()
    };
    let person_row = |buffer: &ratatui::buffer::Buffer, name: &str| {
        (0..HEIGHT).find(|row| row_text(buffer, *row).contains(name)).expect("person row")
    };

    // `team` sits at depth two and the ROOT COSTS NO LEVEL, so the branch
    // indents two columns: the disclosure and every person's status share
    // column 3, and the name and title begin at column 5. The status column is
    // FIXED for the branch — it is the department's own indented column, the
    // same for every state.
    const STATUS: u16 = 3;
    const NAME: u16 = 5;
    let ordinary = draw(&view, true);
    let department =
        (0..HEIGHT).find(|row| row_text(&ordinary, *row).contains("Team")).expect("department row");
    assert_eq!(ordinary[(STATUS, department)].symbol(), "\u{2212}");
    for (id, name, _, _, _, glyph) in states {
        let row = person_row(&ordinary, name);
        let light_colour = match id {
            "worker" => Color::Rgb(0x00, 0x5e, 0x00),
            "idler" => Color::Rgb(0x53, 0x53, 0x00),
            "starter" => Color::Rgb(0x00, 0x5a, 0x5a),
            _ => Color::Rgb(0xff, 0x00, 0x00),
        };
        assert_eq!(ordinary[(0, row)].symbol(), " ", "{id}: one outer padding cell");
        assert_eq!(ordinary[(STATUS, row)].symbol(), glyph, "{id}: icon aligns with disclosure");
        assert_eq!(ordinary[(STATUS, row)].fg, light_colour, "{id}: icon colour");
        assert_eq!(ordinary[(STATUS + 1, row)].symbol(), " ", "{id}: one gap after icon");
        assert_eq!(ordinary[(NAME, row)].symbol(), name.chars().next().unwrap().to_string());
        assert_eq!(ordinary[(NAME, row + 1)].symbol(), name.chars().next().unwrap().to_string());
        for state_word in ["working", "idle", "starting", "sleeping"] {
            // The identity itself may contain these letters -- `@idler` holds
            // "idle" -- so this asks the question the rule actually asks: is
            // the word repeated at the RIGHT end of the row, where the old
            // right-side status column used to draw it?
            assert!(
                !row_text(&ordinary, row).trim_end().ends_with(state_word),
                "{id}: {state_word} must not be duplicated at the right"
            );
        }

        view.select_person(id);
        for (light, foreground, background) in [
            (true, Color::Rgb(0x5b, 0x21, 0xb6), Color::Rgb(0xed, 0xe7, 0xf6)),
            (false, Color::Rgb(0xd8, 0xb4, 0xfe), Color::Rgb(0x2e, 0x10, 0x65)),
        ] {
            let selected = draw(&view, light);
            let row = person_row(&selected, name);
            let colour = match (id, light) {
                ("worker", true) => Color::Rgb(0x00, 0x5e, 0x00),
                ("idler", true) => Color::Rgb(0x53, 0x53, 0x00),
                ("starter", true) => Color::Rgb(0x00, 0x5a, 0x5a),
                ("sleeper", true) => Color::Rgb(0xff, 0x00, 0x00),
                ("worker", false) => Color::Rgb(0x00, 0xc5, 0x00),
                ("idler", false) => Color::Rgb(0xaf, 0xaf, 0x00),
                ("starter", false) => Color::Rgb(0x00, 0xbd, 0xbd),
                _ => Color::Rgb(0xff, 0x00, 0x00),
            };
            for selected_row in [row, row + 1] {
                for column in 0..WIDTH {
                    assert_eq!(selected[(column, selected_row)].bg, background);
                }
            }
            assert_eq!(
                selected[(STATUS, row)].fg,
                colour,
                "{id}: selected icon keeps state colour"
            );
            assert!(
                contrast(selected[(STATUS, row)].fg, selected[(STATUS, row)].bg) >= 3.0,
                "{id}: the status glyph meets graphical contrast in the {light:?} card"
            );
            assert_eq!(selected[(NAME, row)].fg, foreground, "{id}: selected name uses theme ink");
            assert!(
                contrast(selected[(NAME, row)].fg, selected[(NAME, row)].bg) >= 4.5,
                "{id}: selected card text meets AA in the {light:?} card"
            );
            assert_eq!(
                selected[(NAME, row + 1)].fg,
                foreground,
                "{id}: selected title uses theme ink"
            );
        }
    }
}

// --- clicks ------------------------------------------------------------------

#[test]
fn clicking_a_department_selects_it() {
    let view = company();
    assert_eq!(
        click(&view, 21, department_click_row(&view, "executive")),
        Action::SelectDepartment("executive".to_owned())
    );
    assert_eq!(
        click(&view, 21, department_click_row(&view, "quant")),
        Action::SelectDepartment("quant".to_owned())
    );
}

#[test]
fn clicking_a_person_focuses_them() {
    let mut view = company();
    view.select("quant");
    let first_person_row = person_click_rows(&view, "quant-head")[0];
    assert_eq!(
        click(&view, 21, first_person_row),
        Action::FocusPerson {
            department_id: "quant".to_owned(),
            person_id: "quant-head".to_owned()
        }
    );
}

#[test]
fn a_click_on_a_person_who_is_not_live_resolves_to_their_card() {
    // THE CLAIM MOVED. This used to resolve to `Ignored`, because the People
    // section drew the live rows only and the second row was blank. Every
    // person is drawn now, so the row is a TARGET. `Brain::perform` turns this
    // action into the focused sleeping card; only that card's button may wake.
    let mut view = company();
    view.select("quant");
    let second_person_row = person_click_rows(&view, "analyst")[0];
    assert_eq!(
        click(&view, 21, second_person_row),
        Action::FocusPerson { department_id: "quant".to_owned(), person_id: "analyst".to_owned() },
        "a sleeper is a click target; refusing here is the silence that read as broken"
    );
    assert_eq!(
        view.people()[1].state(),
        PersonState::Sleeping,
        "and the state on the row is what tells the brain to show the sleeping card"
    );
}

#[test]
fn the_titles_and_the_blank_rows_are_not_click_targets() {
    let view = company();
    assert_eq!(click(&view, 21, 0), Action::Ignored, "the one blank padding row");
    assert_eq!(click(&view, 21, view.tree_rows().len() + 1), Action::Ignored, "a blank tree row");
}

#[test]
fn the_control_row_toggles_and_a_collapsed_rail_has_no_other_targets() {
    let mut view = company();
    assert_eq!(click(&view, 21, 20), Action::ToggleCollapsed);
    view.toggle_collapsed();
    assert!(view.collapsed());
    assert_eq!(click(&view, 21, 20), Action::ToggleCollapsed, "and toggles back");
    assert_eq!(
        click(&view, 21, 1),
        Action::Ignored,
        "a collapsed rail draws no rows, so a stray click selects nothing"
    );
}

#[test]
fn a_click_beyond_the_last_department_of_a_short_company_selects_nothing() {
    let view = company();
    assert_eq!(click(&view, 21, view.tree_rows().len() + 1), Action::Ignored, "past the tree");
}

// --- the row -> entity map, exhaustively -------------------------------------

/// EVERY row of a known rail, in one table.
///
/// The operator reported that every department click resolved to the FIRST
/// department and every person click to the first person. The individual
/// mapping assertions above already covered rows 1 and 2, so a spot check could
/// not tell a correct map from a broken one — this walks the whole pane and
/// pins what each row is, including the rows that must resolve to NOTHING.
///
/// If this passes and the operator still sees the wrong row selected, the map
/// is not the defect: the ROW the rail was handed is. That is why
/// `sidebar.click` logs the raw row and height.
#[test]
fn every_row_of_the_rail_maps_to_exactly_the_entity_drawn_on_it() {
    // Three departments, and a department with three live people, so the middle
    // and last rows of both sections are distinct from the first.
    let departments = vec![dept("executive", 0, 1), dept("engineering", 1, 3), dept("empty", 1, 0)];
    let mut people = BTreeMap::new();
    people.insert("executive".to_owned(), vec![person("chief", true)]);
    people.insert(
        "engineering".to_owned(),
        vec![person("carlos", true), person("priya", true), person("tom", true)],
    );
    let mut view = View::new(departments, people);
    view.select("engineering");

    let height = 21;
    assert_eq!(click(&view, height, 0), Action::Ignored, "row 0 is blank padding");
    for (index, row) in view.tree_rows().iter().skip(view.scroll_offset()).enumerate() {
        let expected = match row {
            super::TreeRow::DepartmentSpacer(_) => Action::Ignored,
            super::TreeRow::Department(department) => {
                Action::SelectDepartment(department.id.clone())
            }
            super::TreeRow::Person(department, person)
            | super::TreeRow::Role(department, person) => Action::FocusPerson {
                department_id: department.id.clone(),
                person_id: person.id.clone(),
            },
        };
        assert_eq!(click(&view, height, index), expected, "tree line {index}");
    }
    assert_eq!(click(&view, height, view.tree_rows().len()), Action::Ignored);
    assert_eq!(click(&view, height, height - 1), Action::ToggleCollapsed);
}

#[test]
fn an_out_of_range_row_is_refused_and_never_clamped_onto_the_first_entity() {
    // The failure pattern the operator described — everything landing on the
    // first row — is what a CLAMP produces. There is none: an index past the
    // end resolves to nothing at all, so a wrong computation can never quietly
    // become "the first one".
    let view = company();
    let height = 21;
    for row in [view.tree_rows().len() + 1, view.tree_rows().len() + 2] {
        assert_eq!(click(&view, height, row), Action::Ignored, "row {row}");
    }
}

#[test]
fn a_scrolled_section_maps_rows_through_its_own_offset() {
    // The other way a row can resolve to the wrong entity: the section is
    // scrolled and the click does not account for it. Row 1 is whatever the
    // offset put there, not always the first department.
    let mut view = company();
    view.scroll(1);
    assert_eq!(view.scroll_offset(), 1);
    let offset = view.scroll_offset();
    assert_eq!(
        click(&view, 21, 1),
        Action::FocusPerson {
            department_id: "executive".to_owned(),
            person_id: "chief".to_owned()
        },
        "the first drawn row is tree[offset], and the click must agree"
    );
    assert_eq!(view.scroll_offset(), offset, "hit-testing never moves the scrolled tree");
}

// --- folding the three authorities into two lists ----------------------------

/// A head whose `department_id` is the PARENT, which is the shape that made
/// `engineering-head` vanish from the operator's People list.
///
/// A free fn rather than a closure inside [`roster`]: a second test builds one
/// of these too, and a closure scoped to one function cannot be called from
/// another (which is exactly how this stopped compiling).
/// An ordinary direct member of a department: not its head, and asleep.
///
/// `desired_active: false` is the point of this helper. A sleeping person is
/// still employed, so they stay in their department's TOTAL while being absent
/// from its live count -- which is the whole rule the `active/total` badge
/// states, and it needs a person who is one and not the other to pin it.
fn member(id: &str, homed: &str, order: usize) -> crate::roster::RosterPerson {
    crate::roster::RosterPerson {
        id: id.to_owned(),
        display_name: id.to_owned(),
        title: "t".to_owned(),
        department_id: homed.to_owned(),
        is_head_of: None,
        display_order: order,
        desired_active: false,
        employment_state: "active".to_owned(),
    }
}

fn head(id: &str, homed: &str, heads: &str, order: usize) -> crate::roster::RosterPerson {
    crate::roster::RosterPerson {
        id: id.to_owned(),
        display_name: id.to_owned(),
        title: "t".to_owned(),
        department_id: homed.to_owned(),
        is_head_of: Some(heads.to_owned()),
        display_order: order,
        desired_active: true,
        employment_state: "active".to_owned(),
    }
}

/// A roster with an executive root, a quant child, and an empty child of quant.
fn roster() -> crate::roster::Roster {
    use crate::roster::{RosterCompany, RosterDepartment, RosterPerson};
    let department = |id: &str, parent: Option<&str>, head: &str, order: usize| RosterDepartment {
        id: id.to_owned(),
        name: id.to_owned(),
        parent_department_id: parent.map(str::to_owned),
        head_person_id: head.to_owned(),
        order,
        state: "active".to_owned(),
    };
    let member = |id: &str, department: &str, order: usize| RosterPerson {
        id: id.to_owned(),
        display_name: id.to_owned(),
        title: "t".to_owned(),
        department_id: department.to_owned(),
        is_head_of: None,
        display_order: order,
        desired_active: true,
        employment_state: "active".to_owned(),
    };
    crate::roster::Roster {
        company: RosterCompany { slug: "acme".to_owned(), display_name: "Acme".to_owned() },
        root_department_id: "executive".to_owned(),
        // Deliberately OUT of array order, with `order` carrying the truth:
        // reading the array position instead of the field is the defect
        // `roster.rs` warns about, and it only shows once a tree is nested.
        departments: vec![
            department("deep", Some("quant"), "nobody", 2),
            department("executive", None, "chief", 0),
            department("quant", Some("executive"), "quant-head", 1),
        ],
        people: vec![
            member("analyst", "quant", 2),
            member("chief", "executive", 0),
            member("quant-head", "quant", 1),
        ],
    }
}

#[test]
fn the_departments_keep_structural_depth_without_using_it_as_display_indentation() {
    let roster = roster();
    let desired = ["chief", "quant-head", "analyst"].map(str::to_owned).into_iter().collect();
    let live = ["chief"].map(str::to_owned).into_iter().collect();
    let (departments, _) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert_eq!(
        departments.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["executive", "quant", "deep"],
        "the canonical `order` field decides, never the array position"
    );
    assert_eq!(
        departments.iter().map(|row| row.depth).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "depth is the structural fact the rail indents each department by"
    );
    assert_eq!(
        departments.iter().map(|row| row.live).collect::<Vec<_>>(),
        vec![1, 0, 0],
        "the count is LIVE people, which is tmux's answer and not chiefd's"
    );
    assert!(
        departments.iter().any(|row| row.id == "deep"),
        "a department with nobody in it still gets a ROW; what it does not get is a window"
    );
}

/// THE REPORTED BUG. The store nests a sub-department under its parent
/// (`commodities.parent_id = trading`, itself under the executive root), the
/// roster carries that parent chain, and [`super::project`] derives the right
/// `depth` for each row — but the rail drew every department at one fixed
/// column, so the owner's photographed tree (Commodities/Securities/Crypto
/// UNDER Trading Strategy) rendered as flat siblings. The rail must indent each
/// department one step per level of real depth, its people aligned under it, so
/// the glass matches the store's tree.
///
/// This drives the WHOLE derivation, not a hand-set `depth`: it builds a roster
/// with genuine `parent_department_id` links, projects it, and reads the drawn
/// columns — a flat rail fails it and a nested one passes it.
/// THE ROOT COSTS NO LEVEL (operator ruling, 2026-08-19).
///
/// #1172 made the rail draw the store's nesting, and it spent one indentation
/// level on the executive root — so EVERY department started two columns in and
/// a top-level department read as if it were a sub-department of something. The
/// operator's words: "level one is the root, level two is the root's children,
/// those should not have any indentation, and then the other ones start
/// indenting."
///
/// So the drawn step is `depth - 1`, floored at zero: depth 0 and depth 1 share
/// the flush-left column, and the first real step belongs to depth 2 — a
/// department INSIDE another department, which is the only case where the
/// indent carries information. This pins the arithmetic itself, so a future
/// change to `DEPARTMENT_INDENT_STEP` or to the render cannot quietly restore
/// the root's level.
#[test]
fn the_root_and_its_top_level_departments_share_the_flush_left_column() {
    use super::{
        department_disclosure_column, department_indent, DEPARTMENT_INDENT_STEP, TREE_GUTTER,
    };

    assert_eq!(department_indent(0), 0, "the root is flush left");
    assert_eq!(department_indent(1), 0, "a top-level department is flush with the root");
    assert_eq!(
        department_indent(2),
        DEPARTMENT_INDENT_STEP,
        "the first step belongs to a department inside a department"
    );
    assert_eq!(
        department_indent(3),
        2 * DEPARTMENT_INDENT_STEP,
        "every level below that adds one more step"
    );

    // Hit-testing reads the same geometry as the render, so the disclosure a
    // click toggles is the disclosure that was drawn — at depth 0 and depth 1
    // that is the SAME column, which is exactly the case a fixed-column
    // hit-test would have got wrong.
    assert_eq!(department_disclosure_column(0), TREE_GUTTER);
    assert_eq!(department_disclosure_column(1), TREE_GUTTER);
    assert_eq!(department_disclosure_column(2), TREE_GUTTER + DEPARTMENT_INDENT_STEP);
}

#[test]
fn a_nested_sub_department_renders_indented_under_its_parent() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::roster::{Roster, RosterCompany, RosterDepartment, RosterPerson};
    let department =
        |id: &str, name: &str, parent: Option<&str>, head: &str, order: usize| RosterDepartment {
            id: id.to_owned(),
            name: name.to_owned(),
            parent_department_id: parent.map(str::to_owned),
            head_person_id: head.to_owned(),
            order,
            state: "active".to_owned(),
        };
    let head = |id: &str, name: &str, department: &str, order: usize| RosterPerson {
        id: id.to_owned(),
        display_name: name.to_owned(),
        title: "t".to_owned(),
        department_id: department.to_owned(),
        is_head_of: Some(department.to_owned()),
        display_order: order,
        desired_active: true,
        employment_state: "active".to_owned(),
    };
    let roster = Roster {
        company: RosterCompany { slug: "taperoom".to_owned(), display_name: "Taperoom".to_owned() },
        root_department_id: "executive".to_owned(),
        // Preorder: root, then Trading, then Trading's child Commodities.
        departments: vec![
            department("executive", "Executive", None, "chief", 0),
            department("trading", "Trading Strategy", Some("executive"), "sage", 1),
            department("commodities", "Commodities Strategy", Some("trading"), "ore", 2),
        ],
        people: vec![
            head("chief", "Chief", "executive", 0),
            head("sage", "Sage", "trading", 1),
            head("ore", "Ore", "commodities", 2),
        ],
    };
    let desired = ["chief", "sage", "ore"].map(str::to_owned).into_iter().collect();
    let live = ["chief", "sage", "ore"].map(str::to_owned).into_iter().collect();
    let (departments, people) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    // The derivation is what makes this non-vacuous: the depths are computed
    // from the parent chain, not asserted into the fixture.
    assert_eq!(
        departments.iter().map(|row| (row.id.as_str(), row.depth)).collect::<Vec<_>>(),
        vec![("executive", 0), ("trading", 1), ("commodities", 2)],
        "project derives depth from the store's parent chain"
    );

    let mut view = View::new(departments, people);
    view.select("executive");
    view.select("trading");
    view.select("commodities");

    const WIDTH: u16 = 40;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 24)).expect("test terminal");
    terminal.draw(|frame| super::render::draw_with_appearance(frame, &view, true)).expect("draw");
    let buffer = terminal.backend().buffer();
    let rows = (0..24)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let row_index = |needle: &str| {
        rows.iter().position(|row| row.contains(needle)).expect("named row is drawn")
    };
    let column_of = |needle: &str| {
        let row = &rows[row_index(needle)];
        let byte = row.find(needle).expect("needle is on named row");
        row[..byte].chars().count()
    };

    // Each department below a top-level one steps one indentation level deeper
    // than its parent. The rail's step is two columns and the ROOT COSTS NO
    // LEVEL, so a department at depth `d` puts its disclosure at column
    // `1 + 2*(d-1)` and its label at `3 + 2*(d-1)`, floored at depth 1.
    assert_eq!(column_of("Executive"), 3, "the root department is not indented");
    assert_eq!(
        column_of("Trading Strategy"),
        3,
        "a top-level department is flush with the root, not indented under it"
    );
    assert_eq!(
        column_of("Commodities Strategy"),
        5,
        "a sub-department indents one step under its parent department, not flat beside it"
    );

    // The nesting is also structural order: the sub-department is drawn AFTER
    // the parent it belongs to, never as a sibling before or unrelated to it.
    assert!(
        row_index("Trading Strategy") < row_index("Commodities Strategy"),
        "the sub-department follows its parent in the tree"
    );

    // A department's people align under it, one status cell at the department's
    // own indented disclosure column, so the head reads as living in the
    // sub-department rather than out-dented from its own header.
    assert_eq!(column_of("Sage"), 3, "Trading's head aligns under Trading");
    assert_eq!(column_of("Ore"), 5, "Commodities' head aligns under Commodities");

    // The disclosure control tracks the indentation: it is not stranded in a
    // fixed left gutter away from the label it opens.
    let disclosure_column = |label: &str| {
        let row = &rows[row_index(label)];
        row.chars().position(|c| c == '+' || c == '\u{2212}').expect("a disclosure marker")
    };
    assert_eq!(disclosure_column("Executive"), 1, "root disclosure in column one");
    assert_eq!(
        disclosure_column("Trading Strategy"),
        1,
        "a top-level department's disclosure shares the root's column"
    );
    assert_eq!(
        disclosure_column("Commodities Strategy"),
        3,
        "sub-department disclosure indents with its label"
    );
}

#[test]
fn a_departments_people_are_its_head_plus_its_workers() {
    // THE REPORTED BUG. `engineering-head` was live in tmux and present in the
    // rail's own `live_people` field, and the operator never saw them in the
    // People list. A head is a person, and the department they HEAD must list
    // them — whichever department their `department_id` happens to name.
    let mut roster = roster();
    roster.people.push(head("quant-boss", "executive", "quant", 3));

    let desired =
        ["chief", "quant-head", "analyst", "quant-boss"].map(str::to_owned).into_iter().collect();
    let live = ["quant-boss"].map(str::to_owned).into_iter().collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let quant: Vec<&str> =
        people.get("quant").expect("quant has people").iter().map(|r| r.id.as_str()).collect();
    assert!(
        quant.contains(&"quant-boss"),
        "the HEAD of a department is one of its people: {quant:?}"
    );
    assert!(quant.contains(&"analyst"), "and so are its workers: {quant:?}");

    // And they are still listed where they are HOMED, because that is the one
    // department a person belongs to and where `placement::pane_department_id`
    // puts their pane. This roster disagrees with itself — a real one homes a
    // head in the unit they head — and the union means neither field can drop
    // the person off the rail.
    let executive: Vec<&str> = people
        .get("executive")
        .expect("executive has people")
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    assert!(executive.contains(&"quant-boss"), "{executive:?}");
}

#[test]
fn a_department_lists_neither_a_childs_head_nor_a_childs_plain_members() {
    // THE OPERATOR'S REPORT, 2026-08-14: "head of engineering should not show
    // up when I select Executives. He should show up when I click Engineering
    // (which he does)." A one-level head roll-up used to list a child's head
    // under the parent as well; a department's People list is now its OWN
    // members and nothing else, so a head appears under exactly one department.
    let mut roster = roster();
    // The fixture's `quant-head` is a plain member; make them the head the
    // department record already names, which is what a real roster publishes.
    roster.people.retain(|person| person.id != "quant-head");
    roster.people.push(head("quant-head", "quant", "quant", 1));
    let desired = ["chief", "quant-head", "analyst"].map(str::to_owned).into_iter().collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let executive: Vec<&str> = people
        .get("executive")
        .expect("executive has people")
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(
        executive,
        vec!["chief"],
        "the CEO alone — neither `quant-head`, who heads the child unit, nor `analyst`, \
         who is a plain member of it: {executive:?}"
    );
    let quant: Vec<&str> =
        people.get("quant").expect("quant has people").iter().map(|r| r.id.as_str()).collect();
    assert!(
        quant.contains(&"quant-head"),
        "and the head is still listed by the unit they head: {quant:?}"
    );
}

#[test]
fn a_departments_row_counts_only_the_people_it_lists() {
    // THE THREE THINGS THAT MUST AGREE: the number beside a department row, the
    // people listed when it is selected, and — once the right-hand side follows
    // a department click — the panes shown for it. The count is computed from
    // the SAME map the list is read out of, so a live child head counted in the
    // parent's number without being in the parent's list is impossible.
    let mut roster = roster();
    roster.people.retain(|person| person.id != "quant-head");
    roster.people.push(head("quant-head", "quant", "quant", 1));
    roster.people.push(member("sleeper-a", "quant", 3));
    roster.people.push(member("sleeper-b", "quant", 4));
    roster.people.push(member("sleeper-c", "quant", 5));
    let desired = ["chief", "quant-head", "analyst"].map(str::to_owned).into_iter().collect();
    let live = ["chief", "quant-head"].map(str::to_owned).into_iter().collect();
    let (departments, people) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let executive = people.get("executive").expect("executive has people");
    assert_eq!(
        executive.iter().filter(|row| row.live).count(),
        1,
        "the CEO only; the live head of the child unit is counted by that child"
    );
    assert_eq!(
        departments.iter().find(|row| row.id == "executive").map(|row| row.live),
        Some(1),
        "and the ROW says the same number the list under it would show"
    );
    assert_eq!(
        departments.iter().find(|row| row.id == "quant").map(|row| row.live),
        Some(1),
        "the head is counted once, by the unit they head"
    );
    assert_eq!(
        departments.iter().map(|row| (row.id.as_str(), row.live, row.total)).collect::<Vec<_>>(),
        vec![("executive", 1, 1), ("quant", 1, 5), ("deep", 0, 0)],
        "the CEO stays in root, while sleeping direct members count only in quant's total"
    );
}

#[test]
fn a_head_is_listed_by_the_unit_they_head_and_by_no_ancestor() {
    // No level, never the subtree. `deep` nests under `quant`, so its head
    // belongs to `deep`'s list and to neither `quant`'s nor the root's — a row
    // that borrowed people from below would make the top row the whole company.
    let mut roster = roster();
    roster.people.push(head("deep-head", "deep", "deep", 4));
    let desired = ["deep-head"].map(str::to_owned).into_iter().collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let deep: Vec<&str> =
        people.get("deep").expect("deep has people").iter().map(|r| r.id.as_str()).collect();
    assert_eq!(deep, vec!["deep-head"], "the unit they head: {deep:?}");
    for ancestor in ["quant", "executive"] {
        let rows: Vec<&str> = people
            .get(ancestor)
            .expect("the ancestor has people")
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert!(
            !rows.contains(&"deep-head"),
            "but no ancestor lists them — not the parent, not the grandparent: {rows:?}"
        );
    }
}

#[test]
fn the_head_of_the_listed_department_is_the_only_manager_on_its_list() {
    // WHO RUNS THIS DEPARTMENT, as a fact of the ROW rather than of the person:
    // the same person heads one department and is a plain member of none other,
    // and the flag is read off the roster's `is_head_of` — never off a title,
    // which is prose nobody may gate on.
    let mut roster = roster();
    roster.people.retain(|person| person.id != "quant-head" && person.id != "chief");
    roster.people.push(head("quant-head", "quant", "quant", 1));
    roster.people.push(head("chief", "executive", "executive", 0));
    let (_, people) = super::project(
        &roster,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let managers = |department: &str| -> Vec<String> {
        people
            .get(department)
            .expect("the department has people")
            .iter()
            .filter(|row| row.manager)
            .map(|row| row.id.clone())
            .collect()
    };
    assert_eq!(managers("quant"), vec!["quant-head".to_owned()], "the head of quant, and only it");
    assert!(
        people.get("quant").expect("quant").iter().any(|row| row.id == "analyst" && !row.manager),
        "a worker beside them is not marked"
    );
    assert_eq!(managers("executive"), vec!["chief".to_owned()], "and every department has its own");
}

/// THE OPERATOR ASKED FOR THIS COLOUR BY NAME: "put in purple who the manager
/// is, like the same purple we use for the `>` sign — that's a nice purple".
/// So the assertion is on the CELLS, not on the string: a badge rendered in the
/// default colour would read identically in a text-only test.
#[test]
fn the_manager_badge_uses_its_own_purple_without_a_selection_arrow() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let departments = vec![DepartmentRow {
        id: "quant".to_owned(),
        name: "Quant".to_owned(),
        depth: 0,
        live: 1,
        total: 2,
    }];
    let mut people = BTreeMap::new();
    people.insert(
        "quant".to_owned(),
        vec![
            PersonRow {
                id: "ada".to_owned(),
                name: "Ada".to_owned(),
                title: "Head of Quant".to_owned(),
                live: true,
                desired: true,
                idle: true,
                crash: None,
                refused: None,
                manager: true,
            },
            PersonRow {
                id: "milo".to_owned(),
                name: "Milo".to_owned(),
                title: "Quant Analyst".to_owned(),
                live: true,
                desired: true,
                idle: true,
                crash: None,
                refused: None,
                manager: false,
            },
        ],
    );
    let mut view = View::new(departments, people);
    view.select("quant");
    let mut terminal = Terminal::new(TestBackend::new(30, 12)).expect("a test terminal");
    terminal
        .draw(|frame| super::render::draw_with_appearance(frame, &view, true))
        .expect("the Light rail draws without reading host theme state");

    let buffer = terminal.backend().buffer().clone();
    let row_text = |row: u16| -> String {
        (0..30)
            .map(|column| buffer[(column, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let ada = (0..12).find(|row| row_text(*row).contains("Ada")).expect("Ada is drawn");
    // The status occupies the same fixed cell as department disclosure. The
    // name and title begin in one flat text column after it.
    assert!(
        row_text(ada).starts_with(" \u{25ce} Ada (manager)"),
        "the name and then who they are: {:?}",
        row_text(ada)
    );
    assert!(
        !row_text(ada).contains("idle"),
        "the icon replaces the duplicate right-side state word: {:?}",
        row_text(ada)
    );
    assert!(
        !row_text(ada + 2).contains("(manager)"),
        "and a plain member carries no badge: {:?}",
        row_text(ada + 1)
    );

    let ada_text = row_text(ada);
    let badge_byte = ada_text.find("(manager)").expect("the badge is on the row");
    let badge = ada_text[..badge_byte].chars().count();
    let colour_at =
        |column: usize| buffer[(u16::try_from(column).expect("a small number"), ada)].fg;
    assert_eq!(
        colour_at(1),
        ratatui::style::Color::Rgb(0x53, 0x53, 0x00),
        "the idle half-circle uses the Light yellow"
    );
    let manager_colour = ratatui::style::Color::Rgb(0x5b, 0x21, 0xb6);
    for offset in 0.."(manager)".len() {
        assert_eq!(
            colour_at(badge + offset),
            manager_colour,
            "every cell of the badge uses the manager purple"
        );
    }
    assert_ne!(colour_at(badge - 2), manager_colour, "the NAME is not repainted");
}

/// THE RULE: A RAIL PERSON ROW CARRIES THE NAME, NEVER THE `@handle`.
///
/// The operator's words: "no need to show the username `@name`, the `Name` is
/// good enough. We do not need `Name @name`, it is redundant."
///
/// It is redundant ALWAYS, not only when the two words match. The rail handle
/// was [`super::person_short_identity`] of the SAME `person.name` the row
/// already prints, so it could never carry a fact the name did not — a
/// two-word "Nadia Okonkwo" simply gave `@nadia`. So the row drops it
/// unconditionally, with no equality test and no fallback.
///
/// The handle is not deleted from the product: it still names the person's
/// pane border and tmux window, where it identifies ONE person instead of
/// listing many, and it remains their reference id everywhere.
#[test]
fn a_rail_person_row_shows_the_name_without_the_redundant_handle() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let departments = vec![DepartmentRow {
        id: "desk".to_owned(),
        name: "Desk".to_owned(),
        depth: 0,
        live: 2,
        total: 2,
    }];
    let mut people = BTreeMap::new();
    people.insert(
        "desk".to_owned(),
        vec![
            PersonRow {
                id: "evan".to_owned(),
                name: "Evan".to_owned(),
                title: "Execution Lead".to_owned(),
                live: true,
                desired: true,
                idle: true,
                crash: None,
                refused: None,
                manager: true,
            },
            PersonRow {
                id: "nadia".to_owned(),
                name: "Nadia Okonkwo".to_owned(),
                title: "Trade Validator".to_owned(),
                live: true,
                desired: true,
                idle: true,
                crash: None,
                refused: None,
                manager: false,
            },
        ],
    );
    let mut view = View::new(departments, people);
    view.select("desk");
    const WIDTH: u16 = 26;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 12)).expect("a test terminal");
    terminal
        .draw(|frame| super::render::draw_with_appearance(frame, &view, true))
        .expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..12)
        .map(|row| {
            (0..WIDTH)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();

    let evan = rows.iter().find(|row| row.contains("Evan")).expect("Evan is drawn");
    assert_eq!(evan, " \u{25ce} Evan (manager)", "name, then the manager marker, and nothing else");

    // A name that does NOT match its handle drops it just the same: the handle
    // was derived from this name, so it never told the reader anything new.
    let nadia = rows.iter().find(|row| row.contains("Nadia")).expect("Nadia is drawn");
    assert_eq!(nadia, " \u{25ce} Nadia Okonkwo", "the two-word name also stands alone");

    assert!(
        rows.iter().all(|row| !row.contains('@')),
        "no rail row prints a handle at all: {rows:?}"
    );

    // The title still follows on its own second line.
    assert!(
        rows.iter().any(|row| row.trim() == "Execution Lead"),
        "the role keeps its own row: {rows:?}"
    );
}

#[test]
fn a_departed_person_is_drawn_nowhere_at_all() {
    // THE OPERATOR'S REPORT: "when I fire someone, they show up as sleeping in
    // the sidebar. They should be completely hidden. We never see fired
    // employees." Sleeping is a state the operator ACTS on — it is the invitation
    // to wake somebody — so drawing a fired person that way offers a gesture the
    // company will refuse.
    //
    // They stay in the ROSTER, and that is deliberate: the reap tells this
    // company's own leaked pane from a stranger's by finding the person in it.
    // Only the LIST drops them.
    let mut roster = roster();
    roster.people.push(crate::roster::RosterPerson {
        id: "fired".to_owned(),
        display_name: "fired".to_owned(),
        title: "t".to_owned(),
        department_id: "quant".to_owned(),
        is_head_of: None,
        display_order: 9,
        desired_active: false,
        employment_state: crate::roster::DEPARTED.to_owned(),
    });
    // Live in tmux, because a departed person can still be finishing a handoff.
    let live = ["fired"].map(str::to_owned).into_iter().collect();
    let (departments, people) = super::project(
        &roster,
        &BTreeSet::new(),
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert!(
        people.values().flatten().all(|row| row.id != "fired"),
        "a fired person is on no department's list"
    );
    assert_eq!(
        departments.iter().find(|row| row.id == "quant").map(|row| row.live),
        Some(0),
        "and their live pane is counted by nobody, or the row would promise a person the \
         list does not have"
    );
    assert_eq!(
        departments.iter().find(|row| row.id == "quant").map(|row| row.total),
        Some(2),
        "the departed roster row does not inflate the operational total"
    );
}

#[test]
fn a_benched_person_is_still_drawn_because_they_are_coming_back() {
    // The boundary of the rule above. Benched is not fired: that person is on
    // the roster, undesired for now, and the operator wakes them by clicking
    // them. Hiding everybody who is merely undesired would empty the rail of
    // exactly the people it exists to act on.
    let mut roster = roster();
    roster.people.push(crate::roster::RosterPerson {
        id: "benched".to_owned(),
        display_name: "benched".to_owned(),
        title: "t".to_owned(),
        department_id: "quant".to_owned(),
        is_head_of: None,
        display_order: 9,
        desired_active: false,
        employment_state: "benched".to_owned(),
    });
    let (_, people) = super::project(
        &roster,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let quant: Vec<&str> =
        people.get("quant").expect("quant has people").iter().map(|r| r.id.as_str()).collect();
    assert!(quant.contains(&"benched"), "{quant:?}");
}

#[test]
fn the_state_tags_are_lowercase() {
    // The operator asked for lowercase, and for a smaller font if possible.
    // There is no smaller font: a terminal cell is one size. Lowercase is the
    // whole of the available answer, and it is what they asked for as fallback.
    for state in
        [PersonState::Working, PersonState::Idle, PersonState::Starting, PersonState::Sleeping]
    {
        let tag = state.tag();
        assert_eq!(tag, tag.to_lowercase(), "{tag} must be lowercase");
        assert!(!tag.is_empty());
    }
}

#[test]
fn the_root_department_is_called_executive_and_never_the_company_name() {
    // THE REGRESSION. Genesis writes the COMPANY's name into the root
    // department's `name`, so the Departments list opened with "Tribes Capital"
    // pretending to be a department. The company name belongs on the rail's own
    // border, once, not in a row that impersonates a department.
    let mut roster = roster();
    for unit in &mut roster.departments {
        if unit.id == roster.root_department_id {
            unit.name = roster.company.display_name.clone();
        }
    }
    let desired = ["chief"].map(str::to_owned).into_iter().collect();
    let live = ["chief"].map(str::to_owned).into_iter().collect();
    let (departments, _) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let root = departments.first().expect("the root is the first department");
    assert_eq!(root.id, "executive");
    assert_eq!(root.name, ROOT_DEPARTMENT_DISPLAY_NAME);
    assert!(
        !departments.iter().any(|row| row.name == "Acme"),
        "the company name is never a department row: {departments:?}"
    );
    assert_eq!(
        departments.get(1).map(|row| row.name.as_str()),
        Some("quant"),
        "and every other department keeps the name chiefd published"
    );
}

#[test]
fn the_unique_ceo_role_ignores_stale_roster_titles_in_light_and_dark() {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    // This is the existing-company shape: the durable roster still carries an
    // old abbreviated title, while the root department remains the authority
    // for the one CEO identity.
    let mut roster = roster();
    let chief = roster
        .people
        .iter_mut()
        .find(|person| person.id == "chief")
        .expect("the existing fixture has its CEO");
    chief.title = "Chief".to_owned();
    chief.display_name = "Avery".to_owned();
    chief.is_head_of = Some("executive".to_owned());
    let analyst = roster
        .people
        .iter_mut()
        .find(|person| person.id == "analyst")
        .expect("the existing fixture has a non-CEO control");
    analyst.title = "Market Analyst".to_owned();

    let desired = ["chief", "analyst"].map(str::to_owned).into_iter().collect();
    let live = ["chief", "analyst"].map(str::to_owned).into_iter().collect();
    let (departments, people) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let ceo = people
        .get("executive")
        .and_then(|rows| rows.iter().find(|person| person.id == "chief"))
        .expect("the root head is listed in Executive");
    assert_eq!(ceo.name, "Avery", "the CEO display name stays roster-owned");
    assert_eq!(ceo.title, "Chief Executive Officer", "the CEO role is product-owned");
    assert!(ceo.manager, "the CEO keeps the root manager badge");
    assert_eq!(
        people
            .get("quant")
            .and_then(|rows| rows.iter().find(|person| person.id == "analyst"))
            .map(|person| person.title.as_str()),
        Some("Market Analyst"),
        "a non-CEO keeps the exact roster role"
    );

    let mut view = View::new(departments, people);
    view.select("executive");
    view.select_person("chief");
    for (light, foreground, background) in [
        (true, Color::Rgb(0x5b, 0x21, 0xb6), Color::Rgb(0xed, 0xe7, 0xf6)),
        (false, Color::Rgb(0xd8, 0xb4, 0xfe), Color::Rgb(0x2e, 0x10, 0x65)),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(32, 16)).expect("test terminal");
        terminal
            .draw(|frame| super::render::draw_with_appearance(frame, &view, light))
            .expect("draw CEO card");
        let buffer = terminal.backend().buffer();
        let row_text =
            |row: u16| (0..32).map(|column| buffer[(column, row)].symbol()).collect::<String>();
        let identity = (0..16)
            .find(|row| row_text(*row).contains("Avery (manager)"))
            .expect("the CEO identity and manager badge are visible");
        let role = identity + 1;
        assert!(row_text(role).contains("Chief Executive Officer"));
        assert_ne!(row_text(role).trim(), "Chief", "the stale stored title is not drawn");
        for row in [identity, role] {
            for column in 0..32 {
                assert_eq!(buffer[(column, row)].bg, background, "theme ground at {column},{row}");
                let expected = if row == identity && column == 1 {
                    if light {
                        Color::Rgb(0x00, 0x5e, 0x00)
                    } else {
                        Color::Rgb(0x00, 0xc5, 0x00)
                    }
                } else {
                    foreground
                };
                assert_eq!(buffer[(column, row)].fg, expected, "theme ink at {column},{row}");
            }
        }
    }
}

#[test]
fn a_person_who_is_neither_wanted_nor_running_is_drawn_sleeping() {
    // THE CLAIM MOVED. `project` used to DROP these people, so the assertion
    // was `people.get("quant") == None`. Nobody is filtered now: a person
    // chiefd does not want and tmux does not have is SLEEPING, which is a state
    // the operator asked to see and to click, not an absence to hide.
    let roster = roster();
    let desired = ["chief"].map(str::to_owned).into_iter().collect();
    let live = ["chief"].map(str::to_owned).into_iter().collect();
    let (departments, people) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(
        people
            .get("quant")
            .map(|rows| rows.iter().map(|row| (row.id.as_str(), row.state())).collect::<Vec<_>>()),
        Some(vec![("quant-head", PersonState::Sleeping), ("analyst", PersonState::Sleeping)]),
        "a departed person is drawn asleep, in canonical order — not hidden"
    );
    assert_eq!(
        departments.iter().find(|row| row.id == "quant").map(|row| row.live),
        Some(0),
        "and the department's LIVE count is still tmux's answer, so the row says nobody is up"
    );
    assert_eq!(
        departments.iter().find(|row| row.id == "quant").map(|row| row.total),
        Some(2),
        "both sleeping people remain in the roster-derived denominator"
    );
    assert_eq!(
        people.get("executive").map(|rows| rows.iter().map(PersonRow::state).collect::<Vec<_>>()),
        Some(vec![PersonState::Working]),
        "while the one who is both wanted and up reads working"
    );
}

#[test]
fn a_person_chiefd_wants_but_tmux_has_not_started_is_kept_and_marked_not_live() {
    let roster = roster();
    let desired = ["chief", "quant-head", "analyst"].map(str::to_owned).into_iter().collect();
    let live = ["quant-head"].map(str::to_owned).into_iter().collect();
    let (_, people) = super::project(
        &roster,
        &desired,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let quant = people.get("quant").expect("the quant unit has people");
    assert_eq!(
        quant.iter().map(|row| (row.id.as_str(), row.live)).collect::<Vec<_>>(),
        vec![("quant-head", true), ("analyst", false)],
        "canonical person order, and starting is told apart from up — \
         which is how a department that is booting reads as booting, not as empty"
    );
}

// --- simulated tmux ----------------------------------------------------------
//
// tmux placement is a product invariant, so the verbs a click turns into are
// asserted as a SEQUENCE and not merely as a set.

#[test]
fn live_people_are_read_from_the_pane_tags_and_a_dead_pane_is_not_a_live_person() {
    let tmux = RecordingTmux::new(&[LIVENESS]);
    let live = effects::live_person_ids(&tmux, "org-acme_");
    assert_eq!(
        live.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["analyst", "chief"],
        "a dead pane is not a placement, and an untagged pane is not a person"
    );
    assert_eq!(
        tmux.calls(),
        vec!["list-panes -s -t org-acme_ -F #{@organization_person_id}\t#{pane_dead}"],
        "one read, of the tag that IS the live placement record"
    );
}

#[test]
fn a_rails_own_department_is_read_from_its_windows_tag_in_one_call() {
    // `@organization_window_id` IS the logical department id
    // (`placement::Window::logical_id`), tagged by the same converge pass that
    // minted the window. The rail reads the authority, never a cache of it —
    // the same rule `pane_of` follows and the same reason `placement.rs`
    // refuses `last_pane_department_id`.
    let tmux = RecordingTmux::new(&["engineering\n"]);
    assert_eq!(effects::window_department_id(&tmux, "%9"), Some("engineering".to_owned()));
    assert_eq!(
        tmux.calls(),
        vec!["display-message -p -t %9 -F #{@organization_window_id}"],
        "one read, of the tag the converge pass writes"
    );
}

#[test]
fn an_untagged_window_gives_the_rail_no_department_rather_than_an_empty_one() {
    // A rail somewhere this company did not mint. An empty string is not a
    // department id, and treating it as one would select a tree node for
    // a department that does not exist.
    let tmux = RecordingTmux::new(&["   \n"]);
    assert_eq!(effects::window_department_id(&tmux, "%9"), None);
}

// --- clicking a person: the move, and the arms that do not move ------------
//
// Operator ruling, 2026-08-14: "If I click on a person, move him into a new
// window so I can see him alone. If I click back to the department, move him
// back."
//
// TOMBSTONES, both retired by that ruling and neither coming back:
//
// * `clicking_a_person_lays_them_full_screen_beside_the_rail_and_never_zooms`
//   asserted no call ever contained `-Z`, because the rail must never
//   disappear, and laid a focused LAYOUT with every bystander at 24 columns
//   instead. A layout string enumerates every pane, so it can narrow a
//   bystander and never hide one — "the person I selected gets merged with the
//   CEO". That closes the whole layout family.
// * `clicking_a_person_zooms_their_pane_and_takes_everybody_else_off_the_glass`
//   and `clicking_a_person_in_an_already_zoomed_window_lands_the_zoom_on_them`
//   replaced it with `resize-pane -Z`, which is true full screen and takes the
//   RAIL off the glass with everybody else. The operator rejected that in turn:
//   the rail must stay.
//
// What survives from both is the pair of rules underneath them, re-pinned below
// against the new verbs: the person's OWN window is selected first (a pane put
// on the glass in a window nobody is looking at is a click that did nothing),
// and a click that resolves to no live pane moves nothing at all.

/// One person click.
///
/// It used to carry `homes` — `person_id -> the department window they belong
/// in` — because the gesture MOVED a pane into the focus window and had to send
/// the previous occupant back where placement wanted them. Nothing moves now:
/// every person is already alone in a window of their own, and the click is a
/// `select-window`.
fn person_click<'a>(person_id: &'a str, display_name: &'a str) -> effects::PersonClick<'a> {
    effects::PersonClick { person_id, display_name }
}

/// What a minted rail is told to run in a TEST.
///
/// NEVER this test binary. `mint_rail` used to read `std::env::current_exe()`
/// itself, and under `cargo test` that is the test harness — so the live test
/// below minted a rail by spawning the whole suite inside a tmux pane, which
/// reached this test again and spawned it again. Measured before it was caught:
/// 31 tmux servers and a rising tree of test processes from a single run. The
/// program is an INPUT now (`PersonClick::rail_program`), and this is the inert
/// value the SIMULATED tests pass — nothing executes it, they only assert the
/// argv. The live tests stage a real one; see [`staged_rail_program`].
const RAIL_PROGRAM: &str = "/nonexistent/chief-test-rail";

/// A real program a LIVE test can mint a rail with: it ignores its arguments and
/// blocks forever.
///
/// Ignoring the arguments is the whole requirement, and it is not obvious. The
/// gesture always appends `sidebar <organization>` to whatever program it is
/// given, so an ordinary blocking command will not do — `/bin/cat sidebar acme`
/// treats both as FILENAMES, fails to open them, and exits, and tmux then
/// destroys the pane the test is about to measure. A rail that vanishes on birth
/// looks exactly like a layout bug.
///
/// Staged inside the caller's tempdir, which takes it away with the test.
//
// Staging a fixture in a tempdir is the sanctioned use of the seam-disallowed
// writer — production filesystem effects belong to `chiefd_host`, and nothing in
// this crate writes a file. Same allow `sidebar/rail/tests.rs` carries for its
// staged operator key.
#[allow(clippy::disallowed_methods)]
fn staged_rail_program(dir: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;

    let path = dir.join("fake-rail");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 3600\n").expect("stage the fake rail");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path.display().to_string()
}

/// Every call that MUTATES, in order — reads dropped.
///
/// Stage 4's claim is about what a gesture WRITES, and a gesture reads freely:
/// it must, because every decision it makes is about the world as it is right
/// now. Counting reads into "one select-window and nothing else" would make the
/// assertion untestable and would also make it wrong.
fn writes(calls: &[String]) -> Vec<String> {
    calls
        .iter()
        .filter(|call| {
            !call.starts_with("list-panes")
                && !call.starts_with("list-windows")
                && !call.starts_with("display-message -p")
                && !call.starts_with("show-options")
        })
        .cloned()
        .collect()
}

/// The session as Stage 4 leaves it: two department windows and the one
/// PERMANENT focus window, appended last.
const WINDOWS_WITH_FOCUS: &str = "@0\texecutive\n@1\tquant\n@7\t__focus__";

/// `@chief_sidebar_columns`, named through the constant rather than spelled out,
/// so a rename of the option does not silently stop matching.
const COLUMNS_OPTION: &str = crate::actuate::trust::sidebar_options::COLUMNS;

/// A fresh sleeping department is not published as one full-width, untagged
/// shell pane. All construction is one detached tmux sequence, and the only
/// `select-window` occurs after the final layout exists.
#[test]
fn a_new_department_has_one_final_hidden_construction_before_one_publication() {
    for (expanded, collapsed, effective) in [(26, "0", 26), (31, "0", 31), (31, "1", 4)] {
        let expanded = expanded.to_string();
        let effective = effective.to_string();
        let tmux = RecordingTmux::answering(&[
            ("display-message -p -t org-acme_ -F #{window_width}", "240\t55\texecutive"),
            ("list-windows -t org-acme_", "@0\texecutive"),
            (COLUMNS_OPTION, &expanded),
            ("@chief_sidebar_collapsed", collapsed),
            ("new-window -d", "@7"),
        ]);
        let sleeping = effects::Overview {
            card: None,
            organization: "acme",
            department_id: "engineering",
            department_name: "Engineering",
            asleep: 4,
            rail_program: Some(RAIL_PROGRAM),
            company_dir: std::path::Path::new("/company"),
        };

        assert!(effects::show_department_overview(&tmux, "org-acme_", &sleeping).shown);
        let writes = writes(&tmux.calls());
        assert_eq!(writes.len(), 2, "one hidden construction and one publication: {writes:?}");
        let built = &writes[0];
        let published = &writes[1];
        assert!(built.starts_with("new-window -d -a"), "the mint is detached: {built}");
        for required in [
            "@organization_id acme",
            "@organization_window_id engineering",
            "@chief_asleep_for engineering",
            "resize-window",
            "@organization_sidebar 1",
            "select-layout",
        ] {
            assert!(built.contains(required), "final construction contains `{required}`: {built}");
        }
        assert!(
            built.contains(&format!("split-window -h -b -l {effective}")),
            "the rail starts at its exact open or collapsed width: {built}"
        );
        assert!(!built.contains("select-window"), "construction never publishes itself: {built}");
        assert_eq!(published, "select-window -t @7", "one publication, after final state");
        let tag = built.find("@organization_window_id engineering").expect("window tag");
        let rail = built.find("@organization_sidebar 1").expect("rail tag");
        let layout = built.find("select-layout").expect("final layout");
        assert!(tag < rail && rail < layout, "identity, rail, then final layout: {built}");
    }
}

#[test]
fn a_stale_focus_mint_uses_the_active_window_and_keeps_the_rail_at_26() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows -t org-acme_", "@0\texecutive"),
        ("display-message -p -t org-acme_ -F #{window_width}", "240\t55\texecutive"),
        ("'new-window' '-d'", "@7"),
        ("list-panes -t @7", "%80\t\t0"),
        (COLUMNS_OPTION, "26"),
        ("split-window -h -b -l 26", "%90"),
        ("display-message -p -t @7 #{window_width}", "166\t42"),
    ]);
    let parked = effects::Parked {
        organization: "acme",
        rail_program: Some(RAIL_PROGRAM),
        company_dir: std::path::Path::new("/company"),
    };

    assert_eq!(effects::ensure_focus_window(&tmux, "org-acme_", &parked), Some("@7".into()));
    let calls = tmux.calls();
    assert_eq!(calls.iter().filter(|call| call.contains("new-window")).count(), 1);
    assert_eq!(calls.iter().filter(|call| call.starts_with("split-window")).count(), 1);
    assert!(calls.iter().any(|call| call.contains("split-window -h -b -l 26")));
    let repair = calls
        .iter()
        .find(|call| call.starts_with("resize-window"))
        .expect("the stale 166-column mint is repaired");
    assert!(repair.contains("-x 240 -y 55"), "active source geometry: {repair}");
    assert!(repair.ends_with("set-option -w -t @7 window-size manual"), "managed sizing: {repair}");
    assert!(
        !calls.iter().any(|call| call.starts_with("select-window")),
        "the detached mint never flashes on the glass"
    );
}

#[test]
fn a_waking_focus_body_is_already_furnished_and_refresh_writes_nothing() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows", "@7\t__focus__"),
        ("list-panes -t @7 -F #{pane_id}\t#{pane_dead}", "%79\t0\t1\t\t\t\t\n%80\t0\t\t\t\teli\t"),
    ]);
    let parked = effects::Parked {
        organization: "acme",
        rail_program: Some(RAIL_PROGRAM),
        company_dir: std::path::Path::new("/company"),
    };

    assert_eq!(effects::ensure_focus_window(&tmux, "org-acme_", &parked), Some("@7".into()));
    assert!(
        writes(&tmux.calls()).is_empty(),
        "a refresh must not put generic furniture beside the body reserved by the click"
    );
}

/// **THE RULE FOR A PERSON CLICK: IT WRITES NOTHING BUT A SELECTION.**
///
/// Three designs stood here before this one, each pinned by a test of its own.
/// `clicking_a_person_moves_them_into_a_window_of_their_own_beside_a_rail`
/// broke the pane into a freshly minted window and booted a rail into it, on
/// the click path. Stage 4 replaced that with a MOVE into a permanent focus
/// window — no mint, no reap, but still a `join-pane`. The operator recorded
/// what a `join-pane` costs on 2026-08-21: their agent's pane went from `42x17`
/// or `64x17` inside a tiled department window to the `129x35` focus body, and
/// the Pi inside repainted its whole scrollback at the new width.
///
/// A pane has exactly one size. Every person is placed alone from the start, so
/// the click is `select-window` + `select-pane` and touches no pane at all.
/// **A CLICK THAT DID NOT MOVE THE GLASS MUST NOT LOG THAT IT DID.**
///
/// The operator reported "everything jumped to the Chief" FOUR times, and the
/// first two fixes — the sidebar's CEO landing (#1211) and the actuator's
/// watched-window chokepoint (#1228) — were both correct and both aimed at
/// something else. Nothing stole the glass. The click's own navigation failed:
/// `Batch::run` discards tmux's result, and the caller logged
/// `sidebar.person.selected` — "the operator was taken to the window" —
/// unconditionally.
///
/// Measured on the box: three clicks on the same person in thirteen seconds,
/// each logging "taken to @2" with the right window id and a live pane, while
/// the active window was @1 throughout. Rail says Reid, glass shows Chief, and
/// ZERO destructive events, because nothing was destroyed.
///
/// **The lying log is why it took four reports**: every investigation started
/// from "something moved the glass", because the one readable surface said the
/// click had worked.
#[test]
fn a_click_whose_select_window_does_not_land_reports_failure_not_success() {
    // tmux NAMES a different active window, before and after the retry: this is
    // positive evidence of divergence, not an unreadable probe.
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
        ("display-message -p -t org-acme_ #{window_id}", "@0"),
    ]);

    let shown = effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst"));

    // NOT `navigated()`. The caller's own answer is what the brain reads, and a
    // click that did not land is not a navigation.
    assert!(!shown.shown, "a click that did not move the glass is not shown: {:?}", tmux.calls());
    // AND IT RETRIED ONCE — a transient control-client hiccup is the observed
    // cause, so one re-select is the remedy; a loop would be a fight with tmux.
    let selects = tmux
        .calls()
        .iter()
        .filter(|call| call.contains("select-window") && call.contains("@1"))
        .count();
    assert_eq!(selects, 2, "one batched select and exactly one retry: {:?}", tmux.calls());
}

/// AND AN UNREADABLE PROBE IS NOT A FAILURE. Inventing a failure from a read
/// nobody took would be the same class of lie pointing the other way, so only
/// tmux NAMING a different window counts.
#[test]
fn a_click_whose_active_window_tmux_will_not_name_is_still_a_success() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);

    assert!(
        effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst")).shown,
        "a probe that answers nothing means 'I could not tell', never 'it failed': {:?}",
        tmux.calls()
    );
}

#[test]
fn a_person_click_moves_no_pane_and_mints_no_window() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);

    assert!(effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst")).shown);

    let calls = tmux.calls();
    for forbidden in ["break-pane", "new-window", "kill-window", "join-pane", "kill-pane"] {
        assert!(
            !calls.iter().any(|call| call.contains(forbidden)),
            "a person click must not touch a pane, but it ran `{forbidden}`: {calls:?}"
        );
    }
    assert!(
        !calls.iter().any(|call| call.starts_with("split-window")),
        "and it boots no rail: every window has had one since converge minted it: {calls:?}"
    );
    let shown = calls
        .iter()
        .find(|call| call.contains("select-window -t @1"))
        .expect("the operator is taken to the window this person is already alone in");
    assert!(
        shown.contains("select-pane -t %4"),
        "selecting the window and the pane are ONE tmux command sequence, so no frame \
         exists between them: {shown}"
    );
}

/// A CLICK REPORTS NAVIGATION, NEVER A MOVE.
///
/// `Shown::moved_geometry` is what arms the brain's settle pass — the window in
/// which size changes are read as OUR transit rather than as the operator's
/// hand on the rail border. A gesture that moves nothing must not arm it, or
/// the brain withholds 300ms of resizes the operator actually asked for.
#[test]
fn a_person_click_reports_no_moved_geometry() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);
    let shown = effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst"));
    assert!(shown.shown);
    assert!(!shown.moved_geometry, "navigation moves no geometry");
}

/// THE FOCUS WINDOW IS MINTED ONCE, PARKED, AND THE MINT IS OFF THE CLICK PATH.
///
/// `-a -t '<session>:$'` puts it last in the window list, which is where
/// `desired_topology` appends it, so no department window's index ever shuffles
/// under the operator. It carries the two window tags converge audits by, its
/// one pane is tagged as a NOTICE rather than as a person, and it gets a rail.
#[test]
fn a_session_with_no_focus_window_mints_exactly_one_parked_one() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows", WINDOWS),
        ("new-window", "@7"),
        ("list-panes -t @7", "%80\t\t0"),
    ]);

    let window = effects::ensure_focus_window(
        &tmux,
        "org-acme_",
        &effects::Parked {
            organization: "acme",
            rail_program: Some(RAIL_PROGRAM),
            company_dir: std::path::Path::new("/company"),
        },
    );

    assert_eq!(window.as_deref(), Some("@7"));
    let calls = tmux.calls();
    let minted = calls.iter().find(|call| call.contains("new-window")).expect("the mint");
    assert!(
        minted.starts_with("if-shell -F -t org-acme_")
            && minted.contains("#{W:#{?#{==:#{@organization_window_id},__focus__},1,}}")
            && minted.contains("#{==:#{W:")
            && minted.contains("'new-window'")
            && minted.contains("'@organization_window_id' '__focus__'")
            && minted.contains("'@chief_asleep_for' '__focus__'"),
        "absence is checked by tmux and the complete identity is published in that same queue: {minted}"
    );
    assert!(
        minted.contains("-d") && minted.contains("-a") && minted.contains("org-acme_:$"),
        "DETACHED, so the glass never moves, and appended LAST so no index shuffles: \
         {minted}"
    );
    assert!(
        calls.iter().any(|call| call.contains("'@organization_window_id' '__focus__'")),
        "the window carries the logical id converge audits it by: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call.contains("'@chief_asleep_for' '__focus__'")),
        "and its one pane is tagged as the rail's own FURNITURE — never as a person, \
         which converge would adopt: {calls:?}"
    );
    let rail = calls.iter().find(|call| call.starts_with("split-window")).expect("its rail");
    // The split is the first command of the batch; the tag is the second. Read
    // the split alone to make the claim about the PROGRAM, and the whole call to
    // make the claim about atomicity below.
    let split = rail.split(" ; ").next().expect("the split leads the batch");
    assert!(
        split.contains("-c /company") && split.ends_with(&format!("{RAIL_PROGRAM} sidebar")),
        "the rail runs the program the CALLER named. THIS IS A SAFETY RULE, NOT A STYLE \
         ONE: `mint_rail` used to read `std::env::current_exe()`, which under `cargo \
         test` is the TEST BINARY — so minting a rail spawned the whole suite inside a \
         tmux pane, which reached this test and spawned it again. Measured before it was \
         caught: 31 tmux servers from ONE run. It also runs bare in the exact company \
         directory, so company discovery cannot read the target person's agent home. {split}"
    );
    let harness = std::env::current_exe().expect("a test binary has a path");
    assert!(!split.contains(&harness.display().to_string()), "and never this process: {split}");
    // THE SECOND SAFETY RULE ON THIS CALL, and the reason a window once drew the
    // company twice: the pane must never be observable before it is tagged.
    // `mint_rail` used to split, read back the new pane id, and tag it in a
    // SECOND `tmux.run`. The rail's own `chief sidebar` attached to the brain
    // inside 25ms — well before that second call landed — and a converge pass
    // that read the window in the gap counted zero panes carrying
    // `@organization_sidebar`, decided the window had lost its sidebar, and
    // split a second rail into it. The repair's rail took the tag, so every
    // guard downstream (all of which count the TAG) saw one rail and stayed
    // silent, while the first one kept painting from the far edge.
    assert!(
        rail.contains(&format!("; set-option -p -t @7 {}", tags::SIDEBAR)),
        "the tag rides in the SAME tmux command sequence as the split, so no observer can \
         ever catch this pane untagged: {rail}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("set-option") && call.contains(tags::SIDEBAR)),
        "and no standalone tagging call survives — that call IS the gap: {calls:?}"
    );
}

/// A CLIENT THAT CANNOT NAME ITS OWN EXECUTABLE MINTS NO RAIL, and says so. The
/// window is still there holding its notice — a rail-less window for one
/// converge pass is worse than a rail, and far better than a pane running
/// something nobody chose.
#[test]
fn a_focus_window_whose_rail_program_is_unknown_is_still_minted() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows", WINDOWS),
        ("new-window", "@7"),
        ("list-panes -t @7", "%80\t\t0"),
    ]);

    let window = effects::ensure_focus_window(
        &tmux,
        "org-acme_",
        &effects::Parked {
            organization: "acme",
            rail_program: None,
            company_dir: std::path::Path::new("/company"),
        },
    );

    assert_eq!(window.as_deref(), Some("@7"));
    assert!(
        !tmux.calls().iter().any(|call| call.starts_with("split-window")),
        "no rail is minted rather than one running a guessed program: {:?}",
        tmux.calls()
    );
}

/// THE PARKED FOCUS WINDOW CANNOT GO BLANK, BY CONSTRUCTION.
///
/// tmux destroys a window only when its LAST pane goes, so a focus window with
/// nobody in it is a window holding its rail — and tmux gives a lone pane the
/// whole window, which is exactly the "the side panel is full screen and the
/// right side is blank" the operator reported. The standing notice is what makes
/// that state unreachable, and it is why `never_blank` no longer has to reason
/// about this window at all.
#[test]
fn a_focus_window_holding_only_its_rail_gets_its_standing_notice_back() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows", WINDOWS_WITH_FOCUS),
        ("list-panes -t @7 -F #{pane_id}\t#{pane_dead}", "%79\t0\t1\t\t\t\t"),
        // The RAIL's own window is 200 columns wide, and the operator chose 26.
        ("-t %79 #{window_width}", "200"),
        ("-t %79 #{pane_index}", "0"),
        ("if-shell -F -t %79", "%80\t7"),
        ("#{window_width}\t#{window_height}", "200\t50"),
        ("#{pane_width}", "26"),
        (COLUMNS_OPTION, "26"),
    ]);

    effects::ensure_focus_window(
        &tmux,
        "org-acme_",
        &effects::Parked {
            organization: "acme",
            rail_program: Some(RAIL_PROGRAM),
            company_dir: std::path::Path::new("/company"),
        },
    );

    let calls = tmux.calls();
    let notice = calls
        .iter()
        .find(|call| call.starts_with("if-shell -F -t %79"))
        .unwrap_or_else(|| panic!("the standing notice is guarded and put back: {calls:?}"));
    assert!(
        notice.contains("split-window") && notice.contains("-t %79"),
        "there is only the rail left to split, so it is split; every other case prefers \
         the non-rail pane: {notice}"
    );
    assert!(
        notice.contains("'-l' '173'"),
        "and it is SIZED, so the rail keeps exactly the 26 columns the operator chose out \
         of its 200-column window. An unsized split off the rail halves the sidebar, and a \
         halved sidebar is a width the rail then RECORDS: {notice}"
    );
    let tagged = calls
        .iter()
        .find(|call| call.contains("'@chief_asleep_for' '__focus__'"))
        .expect("the notice is tagged as furniture");
    assert!(
        tagged.contains("'set-option' '-p' '-t' '@7.1' '@chief_asleep_for' '__focus__'")
            && !tagged.contains("'set-option' '-p' '-t' '@7.0' '@chief_asleep_for'")
            && tagged.contains("'rename-window' '-t' '@7' 'Person'"),
        "and the window takes its parked name back in the same message, because converge \
         has no rename step and a window left holding the previous occupant's name keeps \
         it for ever: {tagged}"
    );
}

/// A person can publish after the refresh snapshot but before its write. The
/// write-time tmux predicate is the final authority: its false arm changes no
/// option, creates no pane and tags no generic notice.
#[test]
fn a_focus_body_published_after_the_snapshot_cancels_the_generic_write() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows", WINDOWS_WITH_FOCUS),
        ("list-panes -t @7 -F #{pane_id}\t#{pane_dead}", "%79\t0\t1\t\t\t\t"),
        ("-t %79 #{window_width}", "200"),
        ("-t %79 #{pane_index}", "0"),
        // Empty means tmux evaluated the predicate after the person appeared.
        ("if-shell -F -t %79", ""),
        (COLUMNS_OPTION, "26"),
    ]);
    let parked = effects::Parked {
        organization: "acme",
        rail_program: Some(RAIL_PROGRAM),
        company_dir: std::path::Path::new("/company"),
    };

    assert_eq!(effects::ensure_focus_window(&tmux, "org-acme_", &parked), Some("@7".into()));
    let writes = writes(&tmux.calls());
    assert_eq!(writes.len(), 1, "the CAS is the only mutation boundary: {writes:?}");
    assert!(
        writes[0].starts_with("if-shell -F -t %79")
            && writes[0].contains("#{window_panes},1")
            && writes[0].contains("@organization_sidebar")
            && writes[0].contains("split-window"),
        "tmux rechecks the exact rail-only topology before it can split: {}",
        writes[0]
    );
}

#[test]
fn a_dead_or_mixed_focus_body_still_blocks_duplicate_furniture() {
    for body in ["%79\t0\t1\t\t\t\t\n%80\t1\t\teli\t\t\t", "%79\t0\t1\teli\t\t\t"] {
        let tmux = RecordingTmux::answering(&[
            ("list-windows", WINDOWS_WITH_FOCUS),
            ("list-panes -t @7 -F #{pane_id}\t#{pane_dead}", body),
        ]);
        let parked = effects::Parked {
            organization: "acme",
            rail_program: Some(RAIL_PROGRAM),
            company_dir: std::path::Path::new("/company"),
        };
        assert_eq!(effects::ensure_focus_window(&tmux, "org-acme_", &parked), Some("@7".into()));
        assert!(writes(&tmux.calls()).is_empty(), "unknown topology fails closed: {body}");
    }
}

/// AN EFFECT REACHED FROM THE REFRESH PATH MAY ONLY FIRE ON A TRANSITION.
///
/// This runs on every company read, and a company with one chatty agent wakes
/// that path many times a second. A focus window that is already furnished must
/// therefore cost exactly its reads.
#[test]
fn a_furnished_focus_window_is_read_and_never_touched() {
    let tmux = RecordingTmux::answering(&[
        ("list-windows", WINDOWS_WITH_FOCUS),
        (
            "list-panes -t @7 -F #{pane_id}\t#{pane_dead}",
            "%79\t0\t1\t\t\t\t\n%4\t0\t\tanalyst\t\t\t",
        ),
    ]);

    effects::ensure_focus_window(
        &tmux,
        "org-acme_",
        &effects::Parked {
            organization: "acme",
            rail_program: Some(RAIL_PROGRAM),
            company_dir: std::path::Path::new("/company"),
        },
    );

    assert!(
        writes(&tmux.calls()).is_empty(),
        "a focus window that is already showing somebody is left completely alone: {:?}",
        tmux.calls()
    );
}

/// A DEPARTMENT'S ONLY WORKER IS ALREADY ALONE, so their click is the same
/// click as everybody else's.
///
/// This used to pin the ordering INSIDE the move — `select-window` before the
/// `join-pane` that emptied the source — so no rendered frame could show the
/// active source after its last content pane left. There is no source to empty
/// now, which is the strongest form of that rule: the frame it guarded against
/// cannot be constructed.
#[test]
fn clicking_a_person_who_is_alone_writes_one_selection_and_nothing_else() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);
    assert!(effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst")).shown);
    let calls = tmux.calls();
    assert_eq!(
        writes(&calls),
        vec!["select-window -t @1 ; select-pane -t %4".to_owned()],
        "one write, and it is a selection: {calls:?}"
    );
}

/// CLICKING A SECOND PERSON LEAVES THE FIRST EXACTLY WHERE THEY ARE.
///
/// The retarget used to be a two-`join-pane` swap: the clicked person IN before
/// the previous occupant went OUT, so the focus window never stood rail-only.
/// Both of those panes changed width, so the operator saw BOTH agents reflow —
/// the one they clicked and the one they had just been reading.
///
/// Nobody is displaced now. The person who was on the glass keeps their window,
/// their width and their scrollback, and the operator simply looks somewhere
/// else.
#[test]
fn clicking_a_second_person_leaves_the_first_exactly_where_they_are() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);
    assert!(effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst")).shown);
    let calls = tmux.calls();
    assert!(
        !calls.iter().any(|call| call.contains("join-pane")),
        "nobody is moved in and nobody is sent home: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("rename-window")),
        "and no window is renamed — a window carries the name converge minted it with, \
         which is its own person's: {calls:?}"
    );
    // The CEO's pane, in the window the operator was looking at, is untouched.
    assert!(
        !calls.iter().any(|call| call.contains("%1")),
        "the person the operator was reading is not addressed at all: {calls:?}"
    );
}

/// THE IDEMPOTENT CLICK. Clicking the person who is already out re-selects their
/// window and repairs it; a second `break-pane` would be aimed at a pane that is
/// already where it belongs.
#[test]
fn clicking_the_person_who_is_already_out_moves_nothing() {
    let tmux = RecordingTmux::new(&[
        "%4\t@7\tanalyst\t0",           // pane_of: they are in @7 already
        "@0\texecutive\n@7\t__focus__", // which IS the focus window
        "0",                            // not zoomed
        "",                             // …and the ONE batched write
    ]);
    assert!(effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst"),).shown);
    let calls = tmux.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("break-pane") || call.starts_with("join-pane")),
        "nothing moves for a person who is already in their own window: {calls:?}"
    );
    // THE WHOLE DISPLAY GESTURE IS ONE INVOCATION. It used to be up to five,
    // and tmux renders at the end of each — so the operator saw the window
    // between them. The reads are hoisted above it and the writes ride
    // together; see `effects::Batch`.
    let shown = calls
        .iter()
        .find(|call| call.contains("select-window -t @7"))
        .expect("the window is shown");
    assert!(
        shown.contains("select-pane"),
        "selecting the window and the pane are the SAME tmux command sequence, so no frame \
         exists between them: {shown:?}"
    );
}

#[test]
fn clicking_a_person_in_another_window_switches_to_that_window_first() {
    // THE RULE, named because it was doubted from a live tmux dump: every
    // window has its own rail (#1137), and the rail the operator clicks is in
    // window @0 while the person they clicked is in window @1.
    //
    // The resolver is SESSION-scoped (`list-panes -s -t <session>`), not
    // window-scoped, so it sees every window's panes and returns the window the
    // person is actually in. A window-scoped resolver would find nothing and the
    // click would silently do nothing — which is what a rail listing the whole
    // company but only able to show its own window would feel like.
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);
    assert!(effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst")).shown);
    let calls = tmux.calls();
    assert!(
        calls.iter().any(|call| call.starts_with("list-panes -s -t org-acme_")),
        "SESSION scope, so a person one window over is still found: {}",
        calls.join(" | ")
    );
    assert!(
        calls.iter().any(|call| call.contains("select-window -t @1")),
        "the window that person is already alone in, not the rail's own: {calls:?}"
    );
}

#[test]
fn a_click_that_resolves_to_nobody_moves_nothing() {
    let tmux = RecordingTmux::new(&[PANES, LIVENESS]);
    assert!(!effects::show_person(&tmux, "org-acme_", &person_click("ghost", "Ghost")).shown);
    assert!(
        !tmux.verbs().iter().any(|verb| verb == "break-pane" || verb == "select-window"),
        "moving a pane that is not there would put a stranger on the glass"
    );
}

// --- clicking somebody who is not up ------------------------------------------

#[test]
fn clicking_a_person_who_is_not_up_says_so_instead_of_doing_nothing() {
    // 17 of 25 clicks on the operator's box ended `focus.unresolved`, because
    // the two-minute settle parked the staff WHILE they were clicking them. The
    // rail did nothing and said nothing, so it read as broken rather than as a
    // company that had gone to sleep. Silence is the worst possible answer from
    // a control.
    let tmux = RecordingTmux::new(&[""]);
    effects::announce(&tmux, "%9", &effects::asleep_notice("Priya", "sleeping"));
    let calls = tmux.calls();
    assert_eq!(calls.len(), 1, "one message, on the rail's own pane: {calls:?}");
    assert!(calls[0].starts_with("display-message -t %9 "), "{}", calls[0]);
    assert!(calls[0].contains("Priya"), "it names the PERSON: {}", calls[0]);
    assert!(calls[0].contains("sleeping"), "and the state, which is the actionable half");
}

#[test]
fn the_notice_never_pretends_the_click_worked() {
    let notice = effects::asleep_notice("Priya", "sleeping");
    assert!(
        !notice.contains("Focus") && !notice.contains("focused"),
        "a person with no pane was not focused, and saying so would be a lie: {notice}"
    );
}

/// THE PLACEHOLDER IS GONE. The notice used to end "Click-to-wake is not wired
/// yet", which was honest and useless. The click now wakes them, and the
/// sentence says so — in the CONTINUOUS tense, because the wake is a round trip
/// plus a converge pass plus a pane spawn and a notice claiming it had already
/// happened would be the same lie in the other direction.
#[test]
fn the_notice_says_the_wake_is_under_way_and_no_longer_says_it_is_unwired() {
    let notice = effects::asleep_notice("Priya", "sleeping");
    assert!(notice.contains("waking"), "the click acts, and the notice says so: {notice}");
    assert!(
        !notice.contains("not wired") && !notice.contains("yet"),
        "the placeholder outlived the defect it described: {notice}"
    );
    assert!(
        !notice.contains("woken") && !notice.contains("awake"),
        "a completed tense would claim a pane that is not there yet: {notice}"
    );
}

/// AND THE REFUSAL REACHES THE GLASS TOO. A wake chiefd declined is a fact
/// about the company — benched, paused, out of scope — and the operator is the
/// only one who can act on it. Summarizing it away would leave a click that
/// silently did nothing, which is the exact defect the notice exists for.
#[test]
fn a_refused_wake_puts_the_daemons_own_reason_in_front_of_the_operator() {
    let tmux = RecordingTmux::new(&[""]);
    let notice = effects::wake_refused_notice(
        "Priya",
        "person-not-staffed: that person is not asleep, they are off the roster",
    );
    effects::announce(&tmux, "%9", &notice);
    let calls = tmux.calls();
    assert_eq!(calls.len(), 1, "one message, on the rail's own pane: {calls:?}");
    assert!(notice.contains("Priya"), "it names the PERSON: {notice}");
    assert!(
        notice.contains("off the roster"),
        "and carries the daemon's own words, which are the only actionable half: {notice}"
    );
}

// --- the pane border titles --------------------------------------------------

#[test]
fn user_facing_identity_uses_first_name_while_internal_id_and_role_stay_authoritative() {
    assert_eq!(person_first_name("Vera Jones"), "Vera");
    assert_eq!(person_short_identity("Vera Jones"), "@vera");
    assert_eq!(person_display_role("Vera Jones", "Test Engineer", false), "Test Engineer");
    assert_eq!(person_display_role("Vera Jones", "", false), TEAM_MEMBER_DISPLAY_ROLE);
    assert_eq!(person_display_role("Vera Jones", "Vera Jones", false), TEAM_MEMBER_DISPLAY_ROLE);
    assert_eq!(person_display_role("Avery", "Chief", true), CEO_DISPLAY_ROLE);

    let mut roster = roster();
    let analyst = roster.people.iter_mut().find(|person| person.id == "analyst").expect("analyst");
    analyst.id = "execution-desk-vera".to_owned();
    analyst.display_name = "Vera Jones".to_owned();
    analyst.title.clear();
    let (_, people) = super::project(
        &roster,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let vera = people
        .values()
        .flatten()
        .find(|person| person.id == "execution-desk-vera")
        .expect("the internal person id remains the row authority");
    assert_eq!(vera.name, "Vera");
    assert_eq!(person_short_identity(&vera.name), "@vera");
    assert_eq!(vera.title, TEAM_MEMBER_DISPLAY_ROLE);
}

#[test]
fn a_persons_border_says_their_role_and_nothing_else() {
    let format = person_border_format("Vera Jones", "Chief of Staff", "#e5c07b");
    assert!(format.contains("@vera"), "{format}");
    assert!(format.contains("Chief of Staff"), "{format}");
    for noise in ["pi", "π", "workspace", "Tribes Capital", "#{pane_title}"] {
        assert!(
            !format.contains(noise),
            "the operator asked for the ROLE and nothing else; '{noise}' is in {format}"
        );
    }
}

#[test]
fn a_chip_carries_its_ground_alongside_the_identity_and_the_role() {
    // The identity line and the filled ground are ONE format string, and the
    // commit that added `{identity}` dropped the ground argument off the end of
    // it -- `bg={}` with nothing left to fill it. Every field is asserted here
    // together, so adding a fifth span can never again silently cost the chip
    // the colour that makes it a chip.
    let format = person_border_format("Vera Jones", "Chief of Staff", "#e5c07b");
    assert!(format.contains("@vera"), "{format}");
    assert!(format.contains("Chief of Staff"), "{format}");
    assert!(format.contains("bg=#e5c07b"), "the chip lost its ground: {format}");
    assert!(format.contains(&format!("fg={CONTRAST_ON_LIGHT}")), "{format}");
    assert!(!format.contains("bg= "), "an empty ground is an unfilled chip: {format}");
}

#[test]
fn a_person_role_is_literal_text_and_cannot_inject_a_tmux_format() {
    let format = person_border_format("Vera", "Lead #{pane_title} #[fg=red]", "#e5c07b");
    assert!(format.contains("Lead ##{pane_title} ##[fg=red]"));
    assert!(!format.contains(" Lead #{pane_title}"));
}

#[test]
fn a_border_never_puts_white_text_on_a_light_ground() {
    // The operator's rule, verbatim: "not a yellow background with white text.
    // It should be black text." Computed from the ground rather than looked up,
    // so a theme colour nobody has allocated yet cannot produce unreadable text.
    assert_eq!(contrast_foreground("#e5c07b"), CONTRAST_ON_LIGHT, "the yellow they named");
    assert_eq!(contrast_foreground("#ffffff"), CONTRAST_ON_LIGHT);
    assert_eq!(contrast_foreground("#000000"), CONTRAST_ON_DARK);
    assert_eq!(contrast_foreground("#2b3a55"), CONTRAST_ON_DARK, "a deep navy takes white");
    let yellow = person_border_format("Vera", "Chief", "#e5c07b");
    assert!(yellow.contains(&format!("fg={CONTRAST_ON_LIGHT}")), "{yellow}");
    assert!(yellow.contains("bg=#e5c07b"), "the ground is that pane's own theme colour: {yellow}");
}

#[test]
fn the_black_white_choice_flips_at_the_wcag_equal_contrast_boundary() {
    // WCAG contrast against black is `(L + 0.05) / 0.05`; against white it is
    // `1.05 / (L + 0.05)`. Their equal point is L ~= 0.179. These adjacent
    // sRGB greys sit on opposite sides of it and pin the real ratio choice,
    // rather than the old and incorrect `L > 0.5` shortcut.
    assert_eq!(contrast_foreground("#757575"), CONTRAST_ON_DARK);
    assert_eq!(contrast_foreground("#767676"), CONTRAST_ON_LIGHT);
}

#[test]
fn a_person_with_no_accent_gets_a_definite_chip_rather_than_no_chip() {
    // THE REPORTED BUG. "Absent is not a colour" left the chip unfilled, which
    // rendered as the operator's white-ground/grey-text, unreadable. A chip is
    // a filled shape or it is not a chip. Nothing invents an ACCENT here; this
    // is an explicit no-accent ground, and the ink comes off it by the same
    // luminance rule every other chip uses. The case that used to reach it was
    // a standard identity with no theme file; the case that reaches it now is
    // an exhausted palette, and the answer must be the same either way.
    for absent in ["", "default", "not-a-colour", "#12345"] {
        let format = person_border_format("Vera", "Chief", absent);
        assert!(format.contains(&format!("bg={NO_ACCENT_BACKGROUND}")), "{absent} -> {format}");
        assert!(format.contains(&format!("fg={CONTRAST_ON_DARK}")), "{absent} -> {format}");
        assert!(!format.contains("bg=default"), "an unfilled chip is the bug: {format}");
    }
    assert_eq!(
        contrast_foreground(NO_ACCENT_BACKGROUND),
        CONTRAST_ON_DARK,
        "the fallback ground is measured like any other, not special-cased"
    );
}

#[test]
fn the_rails_own_border_is_the_company_name_in_white_on_black() {
    let format = rail_border_format("Tribes Capital");
    assert!(format.contains("Tribes Capital"), "{format}");
    assert!(format.contains("fg=white"), "{format}");
    assert!(format.contains("bg=black"), "{format}");
}

#[test]
fn writing_the_titles_turns_the_border_on_and_replaces_every_pane_title() {
    let mut roles = BTreeMap::new();
    roles.insert("analyst".to_owned(), "Analyst".to_owned());
    roles.insert("chief".to_owned(), "Chief".to_owned());
    let names = BTreeMap::from([
        ("analyst".to_owned(), "Vera".to_owned()),
        ("chief".to_owned(), "Chief".to_owned()),
    ]);
    let mut accents = BTreeMap::new();
    accents.insert("analyst".to_owned(), "#c75e00".to_owned());
    let tmux = RecordingTmux::new(&["", "", "", TITLE_PANES, "", ""]);
    effects::write_pane_titles(
        &tmux,
        "org-acme_",
        &["%9".to_owned()],
        "Tribes Capital",
        effects::PersonChips { names: &names, roles: &roles, accents: &accents },
        &BTreeMap::new(),
    );
    let calls = tmux.calls();
    assert_eq!(
        calls[0], "set-option -g pane-border-status top",
        "tmux draws no border title at all until this is on"
    );
    // AND ITS GLOBAL DEFAULT, immediately, because tmux's own default draws
    // `#{pane_title}` — the machine's hostname for a pane nothing has titled.
    // The enabling site owns the fallback; see `SAFE_BORDER_DEFAULT`.
    assert_eq!(
        calls[1],
        format!("set-option -g pane-border-format {}", crate::sidebar::SAFE_BORDER_DEFAULT),
        "the global enable is paired with a global format: {}",
        calls[1]
    );
    assert!(
        calls[2].starts_with("set-option -p -t %9 pane-border-format "),
        "the rail's own border is written next, and per PANE: {}",
        calls[2]
    );
    assert!(calls[2].contains("Tribes Capital"), "{}", calls[2]);
    assert!(
        calls.iter().any(|call| call.contains("-t %4") && call.contains("Analyst")),
        "each live person's border is their role: {calls:?}"
    );
    assert!(calls.iter().any(|call| call.contains("-t %1") && call.contains("Chief")), "{calls:?}");
    assert!(
        !calls.iter().any(|call| call.contains("#{pane_title}")),
        "the pane title is whatever the program inside set; the rail renders \
         its own string instead: {calls:?}"
    );
    // THE REGRESSION THE OPERATOR REPORTED. The chip must be FILLED with the
    // accent. A format that names no `bg=` leaves the title inheriting the
    // border's own style, which is the plain-grey text they screenshotted.
    let analyst = calls
        .iter()
        .find(|call| call.contains("-t %4"))
        .cloned()
        .unwrap_or_else(|| panic!("no border written for %4: {calls:?}"));
    assert!(analyst.contains("bg=#b35400"), "the mode-safe accent is the GROUND: {analyst}");
    assert_eq!(
        analyst,
        "set-option -p -t %4 pane-border-format #[fg=#ffffff,bg=#b35400] @vera · Analyst #[default]",
        "the exact tmux argv carries the normalized accent and readable truecolor ink"
    );
    assert!(
        !analyst.contains("#e5c07b"),
        "chiefd's own allocation wins over the pane's `@accent`, which nothing \
         on this tree writes any more: {analyst}"
    );
}

#[test]
fn a_person_chiefd_allocated_no_accent_for_keeps_the_terminals_own_ground() {
    // The rail neither allocates a colour nor guesses one: a person absent from
    // the accent map is drawn on the explicit no-accent ground. This used to be
    // the CEO's ordinary state (no theme file, by the standard-identity split);
    // it is now only an exhausted palette, and the answer is unchanged.
    let mut roles = BTreeMap::new();
    roles.insert("chief".to_owned(), "Chief".to_owned());
    let names = BTreeMap::from([("chief".to_owned(), "Chief".to_owned())]);
    let tmux = RecordingTmux::new(&["", "", "", "%1\tchief\t", ""]);
    effects::write_pane_titles(
        &tmux,
        "org-acme_",
        &["%9".to_owned()],
        "Acme",
        effects::PersonChips { names: &names, roles: &roles, accents: &BTreeMap::new() },
        &BTreeMap::new(),
    );
    let written = tmux
        .calls()
        .into_iter()
        .find(|call| call.contains("-t %1"))
        .unwrap_or_else(|| panic!("no border written for %1"));
    assert!(written.contains("Chief"), "{written}");
    assert!(
        written.contains(&format!("bg={NO_ACCENT_BACKGROUND}")),
        "an unallocated person still gets a READABLE chip, just not an accented one: {written}"
    );
}

// TOMBSTONE: `an_accent_is_read_from_the_generated_theme_file` and
// `a_theme_that_carries_no_usable_accent_is_a_person_with_no_colour`, deleted
// 2026-08-16 with `sidebar::accent_from_theme`. They pinned a JSON parse of
// `pi-home/themes/organization-<id>*.json`, which chief no longer writes. The
// accent arrives on the launch catalog as a hex now
// (`LaunchEntry::accent`), so what is left to pin is that the rail carries the
// colour chiefd published rather than one of its own — and that is pinned on
// the wire, in `actuate::launch_catalog`'s
// `the_rail_paints_the_accent_the_catalog_carried_and_never_one_of_its_own`.

#[test]
fn every_shipped_roster_accent_gets_ink_that_reads_on_it() {
    // The ten curated identity accents (`ORGANIZATION_PERSON_ACCENTS`, the ONE
    // place accents are allocated). The operator's constraint is that none of
    // these may ever end up as white-on-light, and the luminance split is what
    // holds it for the derived hues beyond the palette too.
    for accent in [
        "#e24033", "#c75e00", "#a27400", "#2c8e46", "#00899a", "#3c7adf", "#6977c5", "#a74ef5",
        "#d83d98", "#c05e68",
    ] {
        let chip = person_border_format("Vera", "Engineer", accent);
        let background = chip
            .split("bg=")
            .nth(1)
            .and_then(|value| value.split(']').next())
            .expect("resolved chip background");
        let foreground = chip
            .split("fg=")
            .nth(1)
            .and_then(|value| value.split(',').next())
            .expect("resolved chip foreground");
        let foreground_luminance = super::relative_luminance(foreground).expect("foreground");
        let background_luminance = super::relative_luminance(background).expect("background");
        let ratio = (foreground_luminance.max(background_luminance) + 0.05)
            / (foreground_luminance.min(background_luminance) + 0.05);
        assert!(ratio >= 4.5, "{accent} resolved to {foreground} on {background}: {ratio}");
        assert_eq!(foreground, CONTRAST_ON_DARK, "curated identity chips use white ink");
    }
    let screenshot = person_border_format("Vera", "Test Engineer", "#6977c5");
    assert_eq!(
        screenshot, "#[fg=#ffffff,bg=#5e6ab0] @vera · Test Engineer #[default]",
        "the live screenshot's #2e3436 on #6977c5 pair was only 3.04:1"
    );
    // And the case the operator named by hand, which is NOT in that palette and
    // is exactly why the ink is computed rather than fixed at white.
    assert_eq!(contrast_foreground("#e5c07b"), CONTRAST_ON_LIGHT, "yellow takes BLACK text");
}

#[test]
fn the_chiefs_title_bar_is_purple_with_white_ink() {
    // Operator ruling, 2026-08-24. The Chief used to take palette slot 0 (the
    // red `#e24033`) by being the oldest roster row, and the bar read white on
    // red. `chiefd-host`'s allocator now answers a FIXED purple for the CEO —
    // `accent::CHIEF_EXECUTIVE_ACCENT`, which this crate cannot link and so
    // repeats by value, the same way every other accent here is repeated.
    //
    // The value is the operator's `#9076c7` rebalanced INTO the identity band,
    // which is the half that is easy to get wrong: raw `#9076c7` sits at
    // luminance ~0.230, outside [0.19, 0.21], so `person_chip_background`
    // would hand it back undarkened and `contrast_foreground` would correctly
    // answer BLACK on it. In the band it darkens like every other identity
    // colour and the ink comes out white by measurement.
    let chip = person_border_format("Chief", "Chief Executive Officer", "#896dc3");
    assert_eq!(
        chip, "#[fg=#ffffff,bg=#7b61ae] @chief · Chief Executive Officer #[default]",
        "the Chief's bar is purple with white text, never the old red and never black ink"
    );
    assert!(!chip.contains("#e24033"), "and never the palette red it used to wear");
    // The rule, not just the bytes: the raw hue would have gone the other way.
    assert_eq!(contrast_foreground("#9076c7"), CONTRAST_ON_LIGHT, "raw, it takes BLACK ink");
    assert_eq!(contrast_foreground("#7b61ae"), CONTRAST_ON_DARK, "darkened, it takes WHITE");
}

#[test]
fn hue_wrapped_roster_accents_use_the_same_contrast_rule() {
    // The first two 37-degree wrap cycles produced by chiefd-host's allocator.
    // Hue rotation does not preserve relative luminance, so this set includes
    // both foreground answers. The title code must measure the final derived
    // hex, not infer an answer from its base palette slot.
    for accent in [
        "#9f7517", "#788300", "#5c8900", "#2b8a7e", "#3c72ff", "#8566e6", "#9468c5", "#e10dc2",
        "#da4a45", "#a37240",
    ] {
        let chip = person_border_format("Vera", "Analyst", accent);
        assert!(chip.contains("fg=#ffffff"), "{accent}: {chip}");
        assert!(
            !chip.contains(&format!("bg={accent}")),
            "the raw L=.202 ground is normalized: {chip}"
        );
    }
}

#[test]
fn a_pane_whose_person_the_roster_does_not_name_is_left_alone() {
    // Another company's pane, or a pane that is not a person at all. Writing a
    // border on it would be this rail relabelling something it does not own.
    let tmux = RecordingTmux::new(&["", "", "", TITLE_PANES]);
    let none = BTreeMap::new();
    effects::write_pane_titles(
        &tmux,
        "org-acme_",
        &["%9".to_owned()],
        "Acme",
        effects::PersonChips { names: &none, roles: &none, accents: &none },
        &BTreeMap::new(),
    );
    assert_eq!(
        tmux.calls().len(),
        4,
        "the option, its global default, the rail's own border, the read — and no person: {:?}",
        tmux.calls()
    );
}

#[test]
fn clicking_a_person_who_died_between_the_draw_and_the_click_shows_nothing() {
    // `ghost` is in the roster and was drawn a moment ago; their pane is dead.
    let tmux = RecordingTmux::new(&[PANES, LIVENESS]);
    assert!(!effects::show_person(&tmux, "org-acme_", &person_click("ghost", "Ghost")).shown);
    assert_eq!(
        tmux.verbs(),
        vec!["list-panes", "list-panes"],
        "the pane is resolved at click time and found absent, so nothing moves — \
         a cached pane id would have shown whoever inherited it. The SECOND \
         read is the diagnostic: the warning names who tmux does have, which is \
         the fact that would otherwise cost an expedition"
    );
}

#[test]
fn clicking_a_person_nobody_has_ever_heard_of_shows_nothing() {
    let tmux = RecordingTmux::new(&[PANES, LIVENESS]);
    assert!(!effects::show_person(&tmux, "org-acme_", &person_click("nobody", "Nobody")).shown);
    assert_eq!(tmux.verbs(), vec!["list-panes", "list-panes"]);
}

/// What `list-windows` answers for a company with two departments up: the
/// window NAMES are sanitized display names and the ids are only in the tags,
/// which is the whole reason the resolver reads a tag.
const WINDOWS: &str = "@0\texecutive\n@1\tquant";

/// **PROOF 1 OF STAGE 4: A DEPARTMENT CLICK WITH NOBODY OUT IS EXACTLY ONE
/// `select-window`, AND NOTHING ELSE AT ALL.**
///
/// This is the assertion the whole stage is for. It used to be
/// `clicking_a_department_shows_its_window_as_a_grid_and_finds_it_by_tag`, which
/// pinned a `select-window` FOLLOWED BY a `select-layout` of the destination —
/// and `select-layout` with an absolute string is a window RESIZE, not an
/// arrangement (`layout-custom.c` `layout_parse` calls `window_resize`, measured
/// on tmux 3.3a). So every navigation SIGWINCHed every Pi in the window it
/// navigated to, and a Pi parked on a synchronous read cannot repaint until it
/// comes back: that is why "department pixels take seconds" even when the click
/// itself returned in a millisecond.
///
/// The re-lay had a real reason — `ApplyLayout` is gated on `layout_dirty`, so
/// converge repairs only windows ITS OWN steps touched — and the reason is gone
/// with its cause. Nothing on the click path mutates a window it is merely
/// navigating to, so there is nothing left for the click to repair.
/// **AND THE FOCUS WINDOW SURVIVES IT.** A department click used to end in
/// `kill-window -t <focus>`, which destroyed a window, killed the rail PROCESS
/// inside it, and shifted every window index after it. The occupant goes home
/// and the window stays, holding the standing notice that was planted BEFORE
/// they left.
/// A focus return is one picture, not a sequence the client can paint.
///
/// The old path published `split-window` by itself while focus was active. It
/// then returned the person, applied two layouts, and selected the department
/// in later tmux invocations. A real browser showed all of those boundaries:
/// first the person's old narrow terminal image, then
/// `{rail, parked notice, person}`, then the parked focus window, and only then
/// the department. One tmux server command sequence is the business rule: the
/// first frame after the gesture is the final department frame.
/// Repeated worker navigation must not turn an unusual multi-occupant focus
/// snapshot into one publication per person. Every occupant returns before the
/// one final department select, in the same server command sequence.
/// THE RAIL-ONLY ZOMBIE WINDOW, measured on a live box (window @4 of
/// `org-tribes-capital_`): a department's last person left, the rail pane kept
/// the window alive at full width, and a click switched the operator onto it —
/// "the side panel is full screen and the right side is blank". Converge reaps
/// such a window within its 30s ceiling, but a click inside that gap must
/// refuse the switch and answer with the same company fact as no window at
/// all: nobody is up in there.
/// A DEPARTMENT VIEW NEVER SHOWS ONE ZOOMED PANE.
///
/// Nothing in this product CREATES zoom state any more — a person click moves a
/// pane, it does not zoom — but `C-M-z` is still bound and tmux still allows it,
/// and zoom is WINDOW state that outlives everything but its own toggle. So this
/// is a REPAIR: clicking the department is the operator's way back to everybody,
/// and it must work for somebody who zoomed by hand.
///
/// It used to be PARTLY redundant with the `select-layout` that followed it —
/// `select-layout` un-zooms the window itself (measured on tmux 3.3a). Stage 4
/// deleted that layout, so this is the only thing left that can clear a hand-made
/// zoom, and it rides the SAME batch as the `select-window` so no frame exists
/// between the two.
/// AN UNZOOMED WINDOW IS NOT TOGGLED. `-Z` is a toggle, so an unconditional
/// un-zoom would ZOOM the department view — the exact opposite of the gesture.
#[test]
fn collapse_records_only_collapse_and_resizes_every_rail() {
    let tmux = RecordingTmux::new(&["", "%9\t1\n%12\t1", ""]);
    effects::set_collapsed_and_resize_all(&tmux, "org-acme_", true, 4);
    assert_eq!(
        tmux.calls(),
        vec![
            "set-option -t org-acme_ @chief_sidebar_collapsed 1".to_owned(),
            "list-panes -s -t org-acme_ -F #{pane_id}\t#{@organization_sidebar}".to_owned(),
            "resize-pane -x 4 -t %9 ; resize-pane -x 4 -t %12".to_owned(),
        ]
    );
    assert!(!tmux.calls().iter().any(|call| call.contains("@chief_sidebar_columns")));
}

#[test]
fn collapse_invalidates_before_width_mutation_and_refreshes_after_final_rail() {
    let tmux = RecordingTmux::new(&["", "%9\t1\n%12\t1", ""]).recording_viewport_authority();
    effects::set_collapsed_and_resize_all(&tmux, "org-acme_", true, 4);
    let calls = tmux.calls();
    assert_eq!(calls.len(), 5, "one fence, three product calls, one refresh: {calls:?}");
    assert!(calls[0].contains("@chief_viewport_topology_epoch"));
    assert_eq!(calls[1], "set-option -t org-acme_ @chief_sidebar_collapsed 1");
    assert_eq!(calls[3], "resize-pane -x 4 -t %9 ; resize-pane -x 4 -t %12");
    assert_eq!(calls[4], "run-shell -b -t org-acme_ #{@chief_viewport_refresh_command} 1");
}

#[test]
fn empty_topology_epoch_refuses_sidebar_width_and_mint_mutations() {
    let tmux = EmptyInvalidationTmux::default();
    effects::set_collapsed_and_resize_all(&tmux, "org-acme_", true, 4);
    let sleeping = effects::Overview {
        card: None,
        organization: "acme",
        department_id: "engineering",
        department_name: "Engineering",
        asleep: 2,
        rail_program: Some(RAIL_PROGRAM),
        company_dir: std::path::Path::new("/company"),
    };
    let _ = effects::show_department_overview(&tmux, "org-acme_", &sleeping);
    let calls = tmux.calls();
    assert!(calls.iter().any(|call| call.contains("@chief_viewport_topology_epoch")));
    for forbidden in [
        "new-window",
        "split-window",
        "join-pane",
        "kill-pane",
        "kill-window",
        "resize-pane",
        "@chief_sidebar_collapsed 1",
    ] {
        assert!(
            !calls.iter().any(|call| call.contains(forbidden)),
            "an empty epoch must refuse `{forbidden}`: {calls:?}"
        );
    }
}

/// THE SNAP THE OPERATOR REPORTED: "clicking on the sidebar and switching
/// between different selections should never resize the sidebar — remember its
/// running size and never resize it by code."
///
/// A rail dragged past a third of the window used to be REPAIRED back to the
/// default width, on every redraw, which is a resize on every click. There is no
/// longer any width the rail refuses to be, and no arm of this rule resizes
/// anything.
/// A RAIL WITH NO NEIGHBOUR IS NOT A RAIL SOMEBODY WIDENED.
///
/// THE MEASURED CASCADE, off a live company: the loading panel died as its
/// person arrived, the rail was the only pane in that window for one frame, it
/// recorded 200 columns as the operator's choice — and every later layout gave
/// the person the ONE column left over. The operator described it as "the
/// sidebar just goes full screen and leaves me with just a little bit of it
/// loading". A border needs two panes; with one, there is nothing to drag.
/// A REPLACED BINARY IS NOT A PROGRAM. Linux answers `/proc/self/exe` for an
/// overwritten executable with the path plus a literal ` (deleted)`, and
/// `current_exe()` hands that back verbatim. Measured on a live company after
/// an upgrade: every rail minted afterwards was spawned as
/// `/path/to/chief (deleted)`, the exec failed, tmux destroyed the pane before
/// its tag landed (`set-option: no such pane: %104`), and the window opened
/// with no sidebar at all — one full-width pane where the rail belongs.
#[test]
fn the_rail_program_is_a_path_that_exists_and_never_a_deleted_one() {
    let program = super::rail_program().expect("this test binary is on disk");
    assert!(
        !program.ends_with(" (deleted)"),
        "the suffix Linux appends to a replaced binary is stripped, never executed: {program}"
    );
    assert!(
        std::path::Path::new(&program).is_file(),
        "and what is left is CHECKED, so a rail is never minted at a path that cannot \
         start: {program}"
    );
}

#[test]
fn expanded_preferences_keep_all_readable_human_widths() {
    for drawn in [12, 31, 80, 106, 200] {
        assert_eq!(super::brain::canonical_columns(drawn), drawn);
    }
    for invalid in [-1, 0, 4, 11] {
        assert_eq!(super::brain::canonical_columns(invalid), 26);
    }
}

#[test]
fn a_dragged_width_is_recorded_without_resizing_anything() {
    let tmux = RecordingTmux::new(&[""]);
    effects::record_columns(&tmux, "org-acme_", 31);
    assert_eq!(
        tmux.calls(),
        vec!["set-option -t org-acme_ @chief_sidebar_columns 31"],
        "tmux already moved the pane; the rail is only writing down what it now is"
    );
    assert!(
        !tmux.verbs().iter().any(|verb| verb == "resize-pane"),
        "resizing here would fight the drag it is meant to preserve"
    );
}

// --- live tmux ---------------------------------------------------------------

/// The railed layout, applied to a REAL tmux server.
///
/// The simulated tests above assert which verbs the rail issues. This asserts
/// the fact all of them rest on and none of them can prove: that the layout
/// string `crate::layout` builds is one tmux accepts, and that the geometry it
/// produces is a full-height rail with the people beside it.
///
/// Skipped where there is no tmux, loud under CI — the same precondition shape
/// `crate::tmux`'s own live tests use.
#[test]
fn a_real_tmux_lays_a_rail_out_beside_its_people() {
    let Some(server) = live_server("sidebar-layout") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "120", "-y", "40"]);
    // A rail pane on the left, then two people beside it.
    let rail_pane = tmux_out(&socket, &["display-message", "-p", "-t", session, "#{pane_id}"]);
    tmux_ok(&socket, &["split-window", "-h", "-t", session]);
    tmux_ok(&socket, &["split-window", "-h", "-t", session]);
    let panes: Vec<String> = tmux_out(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"])
        .lines()
        .map(str::to_owned)
        .collect();
    let people: Vec<&str> =
        panes.iter().map(String::as_str).filter(|id| *id != rail_pane).collect();
    assert_eq!(people.len(), 2, "the fixture must have a rail and two people");

    let layout = crate::layout::organization_tmux_layout(
        120,
        40,
        Some(crate::layout::Rail { pane_id: &rail_pane, columns: 26 }),
        &people,
    )
    .expect("the railed layout builds");
    tmux_ok(&socket, &["select-layout", "-t", session, &layout]);

    // Re-read every time: a geometry captured once would still describe the
    // layout after a resize, which is exactly the assertion that must not pass
    // by accident.
    let geometry = |socket: &str| {
        tmux_out(
            socket,
            &[
                "list-panes",
                "-t",
                session,
                "-F",
                "#{pane_id}\t#{pane_left}\t#{pane_width}\t#{pane_height}",
            ],
        )
    };
    let row = |pane: &str| -> (i64, i64, i64) {
        let geometry = geometry(&socket);
        geometry
            .lines()
            .find_map(|line| {
                let fields: Vec<&str> = line.split('\t').collect();
                if fields.first() != Some(&pane) {
                    return None;
                }
                Some((
                    fields.get(1)?.parse().ok()?,
                    fields.get(2)?.parse().ok()?,
                    fields.get(3)?.parse().ok()?,
                ))
            })
            .unwrap_or_else(|| panic!("tmux never reported {pane} in {geometry:?}"))
    };

    let (left, width, height) = row(&rail_pane);
    assert_eq!(left, 0, "the rail is the leftmost cell");
    assert_eq!(width, 26, "at exactly the width it asked for");
    assert_eq!(height, 40, "and full height — it is a rail, not a box");
    for person in &people {
        let (left, _, height) = row(person);
        assert!(left >= 27, "{person} sits beyond the rail and its divider, not under it");
        assert_eq!(height, 40, "{person} keeps the full height beside the rail");
    }

    // COLLAPSE. A tmux pane cannot be zero columns; four is the floor the
    // control needs, and tmux accepts it.
    tmux_ok(&socket, &["resize-pane", "-x", "4", "-t", &rail_pane]);
    assert_eq!(row(&rail_pane).1, 4, "the collapsed rail is a four-column stub, never absent");

    // ZOOM IS WHAT A PERSON CLICK NOW DOES, proved against a real server rather
    // than asserted from the manual.
    //
    // THE RULE THIS REPLACES: a focused LAYOUT, in which every bystander kept
    // `FOCUS_MIN_READABLE_COLUMNS` (24) because a layout string can only narrow
    // a pane, never hide one. The operator retired that compromise — "when I
    // click on people the person I selected gets merged with the CEO" — and
    // asked for zoom by name. Zoom takes the RAIL off the glass too, which is
    // the price, and `C-M-z` is the way back.
    tmux_ok(&socket, &["resize-pane", "-x", "26", "-t", &rail_pane]);
    tmux_ok(&socket, &["resize-pane", "-Z", "-t", people[1]]);
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", session, "#{window_zoomed_flag}"]),
        "1",
        "the window is zoomed on the clicked person"
    );
    assert_eq!(
        row(people[1]).1,
        120,
        "and they have the WHOLE window — not the width the rail did not want"
    );
    // THE PER-WINDOW TOGGLE, measured: `-Z` aimed at a SECOND pane while the
    // window is already zoomed does not move the zoom, it clears it. That is
    // why the effects read `#{window_zoomed_flag}` and un-zoom before they
    // zooms; a click on a second person would otherwise show nobody.
    tmux_ok(&socket, &["resize-pane", "-Z", "-t", people[0]]);
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", session, "#{window_zoomed_flag}"]),
        "0",
        "aiming -Z at another pane UNZOOMED the window instead of moving the zoom"
    );
    let (left, width, height) = row(&rail_pane);
    assert_eq!((left, width, height), (0, 26, 40), "and the rail is back, exactly where it was");
}

/// The two live facts the completion layer rests on: that a window minted after
/// the fact can be given the same first-cell rail, and that the prefix-free
/// unzoom binding is one tmux accepts.
///
/// Skipped where there is no tmux, loud under CI — same precondition as above.
#[test]
fn a_real_tmux_rails_a_window_minted_after_the_attach_and_binds_the_way_back() {
    let Some(server) = live_server("sidebar-completion") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "120", "-y", "40"]);

    // THE BINDING. Root table, no prefix — the way back from a zoom for
    // somebody who does not know tmux.
    //
    // It is WRITTEN `C-M-z` and tmux READS IT BACK as `M-C-z`: modifiers are
    // normalised into tmux's own canonical order, so `list-keys` never echoes
    // the spelling the configuration used. Asserted in the read-back spelling
    // on purpose, and said out loud here, because the obvious check — grep
    // `list-keys` for the string you bound — finds nothing and looks exactly
    // like a binding that failed to take.
    tmux_ok(&socket, &["bind-key", "-n", "C-M-z", "resize-pane", "-Z"]);
    let bound = tmux_out(&socket, &["list-keys", "-T", "root"]);
    assert!(
        bound.lines().any(|line| line.contains("M-C-z") && line.contains("resize-pane -Z")),
        "tmux accepted the chord and it toggles zoom: {bound}"
    );

    // THE GATE, derived rather than stored. Before `attach` rails anything, no
    // pane in the session is a rail, so the company is not railed and the
    // actuator mints nothing.
    let probe = ["list-panes", "-s", "-t", session, "-F", "#{@organization_sidebar}"];
    assert!(
        !tmux_out(&socket, &probe).lines().any(|line| line.trim() == "1"),
        "a company nobody has attached to is not railed"
    );
    // `attach` rails the first window; that is the whole decision, and it is
    // now readable from the panes themselves with nothing to keep in step.
    let first = tmux_out(&socket, &["display-message", "-p", "-t", session, "#{pane_id}"]);
    tmux_ok(&socket, &["set-option", "-p", "-t", &first, "@organization_sidebar", "1"]);
    assert!(
        tmux_out(&socket, &probe).lines().any(|line| line.trim() == "1"),
        "one railed window is what makes the company a railed one"
    );

    // A window minted afterwards — what the actuator does when a department
    // starts, and the case the operator would otherwise hit as a bug.
    tmux_ok(&socket, &["new-window", "-d", "-t", session, "-n", "quant"]);
    let window = tmux_out(
        &socket,
        &["display-message", "-p", "-t", &format!("{session}:quant"), "#{window_id}"],
    );

    // The rail, minted into that fresh window exactly as the actuator does it.
    let rail_pane = tmux_out(
        &socket,
        &["split-window", "-h", "-b", "-l", "26", "-t", &window, "-P", "-F", "#{pane_id}"],
    );
    assert!(rail_pane.starts_with('%'), "tmux minted a rail pane: {rail_pane}");
    tmux_ok(&socket, &["set-option", "-p", "-t", &rail_pane, "@organization_sidebar", "1"]);

    // And it is discoverable by exactly the read `observe_rail` performs.
    let discovered = tmux_out(
        &socket,
        &["list-panes", "-t", &window, "-F", "#{pane_id}\t#{@organization_sidebar}"],
    );
    let found = discovered
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(_, marker)| marker.trim() == "1")
        .map(|(pane, _)| pane.trim().to_owned());
    assert_eq!(
        found.as_deref(),
        Some(rail_pane.as_str()),
        "the converge pass finds the rail by its tag and reserves its column: {discovered}"
    );

    let geometry = tmux_out(
        &socket,
        &["list-panes", "-t", &window, "-F", "#{pane_id}\t#{pane_left}\t#{pane_width}"],
    );
    let rail_row = geometry
        .lines()
        .find(|line| line.starts_with(&rail_pane))
        .unwrap_or_else(|| panic!("tmux never reported {rail_pane} in {geometry:?}"));
    let fields: Vec<&str> = rail_row.split('\t').collect();
    assert_eq!(fields.get(1).copied(), Some("0"), "the new window's rail is leftmost too");
}

/// A live tmux server that DIES WITH THE TEST, however the test ends.
///
/// # Why this is a guard and not a line at the bottom of each test
///
/// It was a trailing `kill-server`, and a trailing anything is skipped by the
/// one path that matters: a FAILING assertion panics, the line never runs, and
/// the server survives with every pane it minted. Measured on this box — one
/// afternoon of iterating on the live tests left 17 running tmux servers behind,
/// and a leaked server holding live panes is how a workstation gets burned.
///
/// `Drop` runs while unwinding, so the server is reaped whether the test passes,
/// fails, or panics somewhere in the middle. The kill stays best-effort: a
/// server that is already gone is the outcome we wanted anyway, and a teardown
/// that could itself fail a passing test would be worse than the leak.
struct LiveServer {
    socket: String,
}

impl LiveServer {
    /// The socket path, for the `tmux -S` calls the test makes.
    fn socket(&self) -> &str {
        &self.socket
    }
}

impl Drop for LiveServer {
    // Removing a socket this test itself created, in a teardown that must not be
    // skippable. Same sanctioned use of the seam-disallowed writer as the staged
    // fixtures above.
    #[allow(clippy::disallowed_methods)]
    fn drop(&mut self) {
        let _ =
            std::process::Command::new("tmux").args(["-S", &self.socket, "kill-server"]).output();
        // And the socket FILE, which tmux does not always take with the server.
        // Cosmetic on its own; it stops `/tmp` filling with one dead entry per
        // test run, which is what made a REAL leak hard to see among them.
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A live tmux server for one test, or `None` where tmux is absent.
fn live_server(label: &str) -> Option<LiveServer> {
    live_socket(label).map(|socket| LiveServer { socket })
}

/// A private tmux socket for one live test, or `None` where tmux is absent.
///
/// # Panics
/// Under `CI`, where an absent tmux is a broken runner rather than a developer
/// box without the tool.
fn live_socket(label: &str) -> Option<String> {
    let present =
        std::process::Command::new("tmux").arg("-V").output().is_ok_and(|out| out.status.success());
    assert!(
        present || std::env::var("CI").is_err(),
        "CI must have tmux: the sidebar's live layout test cannot be skipped there"
    );
    present.then(|| {
        format!("{}/chiefd-{label}-{}.sock", std::env::temp_dir().display(), std::process::id())
    })
}

fn tmux_ok(socket: &str, args: &[&str]) {
    let out = std::process::Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .expect("tmux runs");
    assert!(out.status.success(), "tmux {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn tmux_out(socket: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .expect("tmux runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn assert_valid_wake_claim(socket: &str, pane: &str) {
    let claim = tmux_out(socket, &["show-options", "-p", "-v", "-t", pane, tags::WAKE_CLAIM]);
    assert_eq!(claim.len(), 32, "every WAKING producer publishes one bounded claim");
    assert!(claim.bytes().all(|byte| byte.is_ascii_hexdigit()), "invalid wake claim: {claim:?}");
}

// --- the operator's gestures, driven against a REAL tmux server -------------
//
// Operator ruling, 2026-08-14: "If I click on a person, move him into a new
// window so I can see him alone. If I click back to the department, move him
// back."
//
// It IS a placement fact: the person the brain holds as selected is what
// `placement::desired_topology` is handed, and it puts that person in a window
// of their own. `a1a7aca9f` deleted an ancestor of exactly this and was right to —
// moving the person emptied their department window, the undesired-window reap
// killed the window the operator was looking at, and tmux fell back to the CEO.
// Three things are different: a person ALONE in their department is never moved
// (so no window is ever emptied), `interpret::kill_window` defers on the
// session's active window, and the RAIL performs the move and selects the
// destination itself. See the design record
// before proposing anything else.
//
// Everything above this line is a RECORDING tmux: it pins which verbs a click
// turns into. Nothing above it can tell whether those verbs, run in that order
// against a real server, put the right thing on the glass — which is exactly
// how five source-only fixes in one night each shipped a rail the operator
// still had to report. The tests below drive `effects::show_department` and
// `effects::show_person` against a live tmux and assert the OBSERVED state:
// which window is active, which pane is active, and how wide everybody ended
// up.
//
// # Driving a REAL click, when somebody needs the whole loop
//
// These tests call the effects directly, which is the right level for the
// layout rules — no rail process, no daemon, no Pi. The rung above them is a
// real mouse click into a running `chief sidebar`, and the one thing that
// looks impossible about it is not: a click is just bytes, and tmux will
// deliver them. VERIFIED on 3.3a, 2026-08-14 —
//
//     tmux -S <sock> send-keys -t <pane> -H 1b 5b 3c 30 3b 3c COL 3b ROW 4d
//     tmux -S <sock> send-keys -t <pane> -H 1b 5b 3c 30 3b 3c COL 3b ROW 6d
//
// is `ESC [ < 0 ; col ; row M` (press) then `… m` (release), SGR mouse mode,
// button 0, and it arrives at the pane's stdin byte for byte. Confirmed by
// running `cat -v` in the pane and reading `^[[<0;10;5M^[[<0;10;5m` back.
// crossterm parses exactly that. Two traps: `-H` takes the bytes as hex, and
// `cat` block-buffers, so append a newline or the capture reads empty and the
// delivery looks like it failed when it did not.

/// A tmux that really runs, on one private socket.
///
/// A spawn-per-command tmux, which is what these live tests want: they assert
/// what tmux ends up SHOWING, and the spawn path is the simplest thing that
/// puts a verb in front of a real server. The production transport is
/// `control::ControlTransport`, whose own tests pin that it answers the same
/// bytes as this does.
struct SocketTmux {
    socket: String,
}

impl Tmux for SocketTmux {
    fn run(&self, args: &[&str]) -> String {
        std::process::Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
            .unwrap_or_default()
    }
}

struct FirstWindowReadTmux {
    inner: SocketTmux,
    first: std::sync::atomic::AtomicBool,
    replacement: Option<String>,
    gate: Option<std::sync::Arc<std::sync::Barrier>>,
}

impl Tmux for FirstWindowReadTmux {
    fn run(&self, args: &[&str]) -> String {
        let first_focus_read = args.first() == Some(&"list-windows")
            && args.last().is_some_and(|format| format.contains(tags::WINDOW))
            && self.first.swap(false, std::sync::atomic::Ordering::SeqCst);
        let answer = self.inner.run(args);
        if first_focus_read {
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            return self.replacement.clone().unwrap_or(answer);
        }
        answer
    }
}

struct MutatingDuplicateTmux {
    inner: SocketTmux,
    target: String,
    option: &'static str,
    value: &'static str,
    once: std::sync::atomic::AtomicBool,
}

impl Tmux for MutatingDuplicateTmux {
    fn run(&self, args: &[&str]) -> String {
        if args.first() == Some(&"if-shell")
            && !args.contains(&"-F")
            && args.iter().any(|arg| arg.contains("kill-window"))
            && self.once.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            tmux_ok(
                &self.inner.socket,
                &["set-option", "-w", "-t", &self.target, self.option, self.value],
            );
        }
        self.inner.run(args)
    }
}

/// Two independent rail processes can both observe the first company read.
/// The create decision belongs to tmux's one command queue, so that race still
/// publishes one logical focus window.
#[test]
fn concurrent_real_tmux_focus_ensures_mint_one_window() {
    let Some(server) = live_server("focus-mint-cas") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "200",
            "-y",
            "50",
            "-n",
            "Executive",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-t", session, tags::ORGANIZATION, "acme"]);
    let first_reads = std::sync::Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let socket = socket.clone();
            let first_reads = first_reads.clone();
            handles.push(scope.spawn(move || {
                let tmux = FirstWindowReadTmux {
                    inner: SocketTmux { socket },
                    first: std::sync::atomic::AtomicBool::new(true),
                    replacement: None,
                    gate: Some(first_reads),
                };
                effects::ensure_focus_window(
                    &tmux,
                    session,
                    &effects::Parked {
                        organization: "acme",
                        rail_program: None,
                        company_dir: std::path::Path::new("/company"),
                    },
                )
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("the focus ensure does not panic"))
            .collect::<Vec<_>>()
    });
    assert!(
        results.iter().all(Option::is_some) && results[0] == results[1],
        "both concurrent owners must resolve the same focus window: {results:?}"
    );
    let focus = tmux_out(
        &socket,
        &["list-windows", "-t", session, "-F", &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW)],
    );
    assert_eq!(
        focus.lines().filter(|line| line.ends_with("\t__focus__")).count(),
        1,
        "two simultaneous owners must publish one focus window: {focus:?}"
    );
}

#[test]
fn an_empty_client_census_cannot_duplicate_an_existing_real_focus_window() {
    let Some(server) = live_server("focus-mint-empty-read") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-t", session, tags::ORGANIZATION, "acme"]);
    let (original, _) = tagged_focus_window(&socket, session, true);
    let tmux = FirstWindowReadTmux {
        inner: SocketTmux { socket: socket.clone() },
        first: std::sync::atomic::AtomicBool::new(true),
        replacement: Some(String::new()),
        gate: None,
    };

    assert_eq!(
        effects::ensure_focus_window(
            &tmux,
            session,
            &effects::Parked {
                organization: "acme",
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        ),
        Some(original.clone())
    );
    assert!(
        !tmux.first.load(std::sync::atomic::Ordering::SeqCst),
        "the test must force the first focus census to answer empty"
    );
    assert_eq!(
        tmux_out(
            &socket,
            &[
                "list-windows",
                "-t",
                session,
                "-F",
                &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW),
            ],
        )
        .lines()
        .filter(|line| line.ends_with("\t__focus__"))
        .map(str::to_owned)
        .collect::<Vec<_>>(),
        [format!("{original}\t__focus__")],
        "a failed client read cannot authorize a second focus window"
    );
}

fn tagged_focus_window(socket: &str, session: &str, first: bool) -> (String, String) {
    let window = if first {
        tmux_out(socket, &["display-message", "-p", "-t", session, "#{window_id}"])
    } else {
        tmux_out(
            socket,
            &[
                "new-window",
                "-d",
                "-a",
                "-t",
                &format!("{session}:$"),
                "-P",
                "-F",
                "#{window_id}",
                "/bin/sh",
                "-c",
                "while :; do sleep 3600 & wait $!; done",
            ],
        )
    };
    let pane = tmux_out(socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    let rail = tmux_out(
        socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            "26",
            "-t",
            &pane,
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(
        socket,
        &[
            "set-option",
            "-w",
            "-t",
            &window,
            tags::ORGANIZATION,
            "acme",
            ";",
            "set-option",
            "-w",
            "-t",
            &window,
            tags::WINDOW,
            placement::FOCUS_WINDOW_ID,
            ";",
            "set-option",
            "-p",
            "-t",
            &pane,
            tags::ASLEEP,
            placement::FOCUS_WINDOW_ID,
            ";",
            "set-option",
            "-p",
            "-t",
            &rail,
            tags::SIDEBAR,
            "1",
        ],
    );
    (window, pane)
}

#[test]
fn a_real_inactive_duplicate_made_only_of_focus_furniture_is_repaired() {
    let Some(server) = live_server("focus-duplicate-repair") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-t", session, tags::ORGANIZATION, "acme"]);
    let (older, _) = tagged_focus_window(&socket, session, true);
    let (keeper, keeper_body) = tagged_focus_window(&socket, session, false);
    let ordinary = tmux_out(
        &socket,
        &[
            "new-window",
            "-d",
            "-a",
            "-t",
            &format!("{session}:$"),
            "-P",
            "-F",
            "#{window_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    let tmux = SocketTmux { socket: socket.clone() };
    tmux_ok(
        &socket,
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            &keeper_body,
            tags::ASLEEP,
            ";",
            "set-option",
            "-p",
            "-t",
            &keeper_body,
            tags::SLEEPING_PERSON,
            "nia",
        ],
    );
    tmux_ok(&socket, &["select-window", "-t", &keeper]);
    assert_eq!(
        effects::ensure_focus_window(
            &tmux,
            session,
            &effects::Parked {
                organization: "acme",
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        ),
        Some(keeper.clone()),
        "the newer active legitimate body is the keeper; the older parked duplicate is deleted"
    );
    let windows = tmux_out(
        &socket,
        &["list-windows", "-t", session, "-F", &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW)],
    );
    assert_eq!(
        windows
            .lines()
            .filter(|line| line.ends_with("\t__focus__"))
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        [format!("{keeper}\t__focus__")],
        "the active newer focus survives instead of the lower numeric id: {windows:?}"
    );
    assert!(!windows.lines().any(|line| line.starts_with(&older)));

    tmux_ok(
        &socket,
        &[
            "set-option",
            "-p",
            "-u",
            "-t",
            &keeper_body,
            tags::SLEEPING_PERSON,
            ";",
            "set-option",
            "-p",
            "-t",
            &keeper_body,
            tags::ASLEEP,
            placement::FOCUS_WINDOW_ID,
        ],
    );
    let (newer, _) = tagged_focus_window(&socket, session, false);
    tmux_ok(&socket, &["select-window", "-t", &ordinary]);
    assert_eq!(
        effects::ensure_focus_window(
            &tmux,
            session,
            &effects::Parked {
                organization: "acme",
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        ),
        Some(keeper.clone())
    );
    let windows = tmux_out(
        &socket,
        &["list-windows", "-t", session, "-F", &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW)],
    );
    assert_eq!(
        windows
            .lines()
            .filter(|line| line.ends_with("\t__focus__"))
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        [format!("{keeper}\t__focus__")],
        "with all focus windows inactive and parked, the lower numeric id survives: {windows:?}"
    );
    assert!(!windows.lines().any(|line| line.starts_with(&newer)));
    assert!(windows.lines().any(|line| line.starts_with(&ordinary)));
}

#[test]
fn a_real_duplicate_with_unknown_local_state_is_left_for_fail_closed_planning() {
    let Some(server) = live_server("focus-duplicate-unknown") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-t", session, tags::ORGANIZATION, "acme"]);
    let (active, _) = tagged_focus_window(&socket, session, true);
    let (unknown, _) = tagged_focus_window(&socket, session, false);
    tmux_ok(&socket, &["set-option", "-w", "-t", &unknown, "@chief_unknown", "1"]);
    tmux_ok(&socket, &["select-window", "-t", &active]);
    let tmux = SocketTmux { socket: socket.clone() };

    assert_eq!(
        effects::ensure_focus_window(
            &tmux,
            session,
            &effects::Parked {
                organization: "acme",
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        ),
        None
    );
    assert_eq!(
        tmux_out(&socket, &["list-windows", "-t", session, "-F", "#{window_id}"]).lines().count(),
        2,
        "unknown ownership is never repaired by deletion"
    );
}

#[test]
fn a_duplicate_that_changes_after_snapshot_is_not_deleted() {
    let Some(server) = live_server("focus-duplicate-cas") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-t", session, tags::ORGANIZATION, "acme"]);
    let _ = tagged_focus_window(&socket, session, true);
    let (candidate, _) = tagged_focus_window(&socket, session, false);
    let ordinary = tmux_out(
        &socket,
        &[
            "new-window",
            "-d",
            "-a",
            "-t",
            &format!("{session}:$"),
            "-P",
            "-F",
            "#{window_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["select-window", "-t", &ordinary]);
    let tmux = MutatingDuplicateTmux {
        inner: SocketTmux { socket: socket.clone() },
        target: candidate,
        option: "@chief_unknown",
        value: "1",
        once: std::sync::atomic::AtomicBool::new(true),
    };

    assert_eq!(
        effects::ensure_focus_window(
            &tmux,
            session,
            &effects::Parked {
                organization: "acme",
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        ),
        None
    );
    assert!(
        !tmux.once.load(std::sync::atomic::Ordering::SeqCst),
        "the mutation must land after the snapshot and before the delete CAS"
    );
    let focus = tmux_out(
        &socket,
        &["list-windows", "-t", session, "-F", &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW)],
    );
    assert_eq!(focus.lines().filter(|line| line.ends_with("\t__focus__")).count(), 2);
}

#[test]
fn a_keeper_that_changes_after_snapshot_preserves_the_other_focus_window() {
    let Some(server) = live_server("focus-keeper-cas") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-t", session, tags::ORGANIZATION, "acme"]);
    let (keeper, _) = tagged_focus_window(&socket, session, true);
    let (candidate, _) = tagged_focus_window(&socket, session, false);
    let ordinary = tmux_out(
        &socket,
        &[
            "new-window",
            "-d",
            "-a",
            "-t",
            &format!("{session}:$"),
            "-P",
            "-F",
            "#{window_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["select-window", "-t", &ordinary]);
    let tmux = MutatingDuplicateTmux {
        inner: SocketTmux { socket: socket.clone() },
        target: keeper.clone(),
        option: tags::WINDOW,
        value: "__changed__",
        once: std::sync::atomic::AtomicBool::new(true),
    };

    assert_eq!(
        effects::ensure_focus_window(
            &tmux,
            session,
            &effects::Parked {
                organization: "acme",
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        ),
        None
    );
    assert!(
        !tmux.once.load(std::sync::atomic::Ordering::SeqCst),
        "the keeper mutation must land after selection and before the delete CAS"
    );
    let windows = tmux_out(
        &socket,
        &["list-windows", "-t", session, "-F", &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW)],
    );
    assert!(windows.lines().any(|line| line == format!("{keeper}\t__changed__")));
    assert!(
        windows.lines().any(|line| line == format!("{candidate}\t{}", placement::FOCUS_WINDOW_ID)),
        "a stale repair must not delete the remaining valid focus candidate: {windows:?}"
    );
}

#[derive(Clone, Copy)]
enum OrphanWakingRace {
    Retag,
    ReplaceClaim,
    UnsetWindowOrganization,
    UnsetSessionOrganization,
    SetSessionOrganizationForeign,
    PresentEmptyForbidden,
    Respawn,
    RespawnRail,
    ResizeRail,
    MoveWindow,
    RecreateSession,
}

/// Change the observed waking body at the exact snapshot-to-CAS seam.
struct InterleavingOrphanWakingTmux {
    inner: SocketTmux,
    pane: String,
    rail: String,
    race: OrphanWakingRace,
    fired: Mutex<bool>,
}

impl Tmux for InterleavingOrphanWakingTmux {
    fn run(&self, args: &[&str]) -> String {
        if args.first() == Some(&"show-options")
            && args.join(" ").contains("@chief_orphan_commands_")
            && !*self.fired.lock().expect("orphan race lock")
        {
            *self.fired.lock().expect("orphan race lock") = true;
            match self.race {
                OrphanWakingRace::Retag => tmux_ok(
                    &self.inner.socket,
                    &["set-option", "-p", "-t", &self.pane, tags::PERSON, "nia"],
                ),
                OrphanWakingRace::ReplaceClaim => tmux_ok(
                    &self.inner.socket,
                    &["set-option", "-p", "-t", &self.pane, tags::WAKE_CLAIM, "replacement-claim"],
                ),
                OrphanWakingRace::UnsetWindowOrganization => tmux_ok(
                    &self.inner.socket,
                    &["set-option", "-w", "-u", "-t", &self.pane, tags::ORGANIZATION],
                ),
                OrphanWakingRace::UnsetSessionOrganization => tmux_ok(
                    &self.inner.socket,
                    &["set-option", "-u", "-t", "org-acme_", tags::ORGANIZATION],
                ),
                OrphanWakingRace::SetSessionOrganizationForeign => tmux_ok(
                    &self.inner.socket,
                    &["set-option", "-t", "org-acme_", tags::ORGANIZATION, "foreign"],
                ),
                OrphanWakingRace::PresentEmptyForbidden => tmux_ok(
                    &self.inner.socket,
                    &["set-option", "-p", "-t", &self.pane, tags::PERSON, ""],
                ),
                OrphanWakingRace::Respawn => tmux_ok(
                    &self.inner.socket,
                    &[
                        "respawn-pane",
                        "-k",
                        "-t",
                        &self.pane,
                        "/bin/sh",
                        "-c",
                        "while :; do sleep 3600 & wait $!; done",
                    ],
                ),
                OrphanWakingRace::RespawnRail => tmux_ok(
                    &self.inner.socket,
                    &[
                        "respawn-pane",
                        "-k",
                        "-t",
                        &self.rail,
                        "/bin/sh",
                        "-c",
                        "while :; do sleep 3600 & wait $!; done",
                    ],
                ),
                OrphanWakingRace::ResizeRail => {
                    tmux_ok(&self.inner.socket, &["resize-pane", "-t", &self.rail, "-x", "30"]);
                }
                OrphanWakingRace::MoveWindow => tmux_ok(
                    &self.inner.socket,
                    &["break-pane", "-d", "-s", &self.pane, "-t", "org-acme_:", "-n", "Raced"],
                ),
                OrphanWakingRace::RecreateSession => {
                    tmux_ok(&self.inner.socket, &["kill-session", "-t", "org-acme_"]);
                    tmux_ok(
                        &self.inner.socket,
                        &[
                            "new-session",
                            "-d",
                            "-s",
                            "org-acme_",
                            "/bin/sh",
                            "-c",
                            "while :; do sleep 3600 & wait $!; done",
                        ],
                    );
                }
            }
        }
        self.inner.run(args)
    }
}

fn live_orphan_waking_focus(label: &str) -> Option<(LiveServer, String, String, String)> {
    let server = live_server(label)?;
    let socket = server.socket().to_owned();
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            "org-acme_",
            "-n",
            "Person",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    let window = tmux_out(&socket, &["display-message", "-p", "-t", "org-acme_", "#{window_id}"]);
    let body = tmux_out(&socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    let rail = tmux_out(
        &socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            "26",
            "-t",
            &body,
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(
        &socket,
        &[
            "set-option",
            "-t",
            "org-acme_",
            tags::ORGANIZATION,
            "acme",
            ";",
            "set-option",
            "-w",
            "-t",
            &window,
            tags::ORGANIZATION,
            "acme",
            ";",
            "set-option",
            "-w",
            "-t",
            &window,
            tags::WINDOW,
            placement::FOCUS_WINDOW_ID,
            ";",
            "set-option",
            "-p",
            "-t",
            &rail,
            tags::SIDEBAR,
            "1",
            ";",
            "set-option",
            "-p",
            "-t",
            &body,
            tags::WAKING_PERSON,
            "nia",
            ";",
            "set-option",
            "-p",
            "-t",
            &body,
            tags::WAKE_CLAIM,
            "claim-nia",
        ],
    );
    Some((server, window, rail, body))
}

/// Put a person body into the focus window at the exact seam between the
/// refresh snapshot and its guarded write.
struct InterleavingFocusBodyTmux {
    inner: SocketTmux,
    rail: String,
    injected: Mutex<Option<String>>,
}

impl Tmux for InterleavingFocusBodyTmux {
    fn run(&self, args: &[&str]) -> String {
        if args.first() == Some(&"if-shell")
            && self.injected.lock().expect("interleave lock").is_none()
        {
            let pane = tmux_out(
                &self.inner.socket,
                &[
                    "split-window",
                    "-h",
                    "-d",
                    "-t",
                    &self.rail,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "/bin/sh",
                    "-c",
                    "printf 'Nia is starting'; while :; do sleep 3600 & wait $!; done",
                ],
            );
            tmux_ok(
                &self.inner.socket,
                &[
                    "set-option",
                    "-p",
                    "-t",
                    &pane,
                    tags::PERSON,
                    "nia",
                    ";",
                    "set-option",
                    "-p",
                    "-t",
                    &pane,
                    tags::WINDOW,
                    placement::FOCUS_WINDOW_ID,
                ],
            );
            *self.injected.lock().expect("interleave lock") = Some(pane);
        }
        self.inner.run(args)
    }
}

/// The V4 race is exercised by a real tmux server: refresh reads a rail-only
/// focus window, then a person appears before its generic split reaches tmux.
/// The guarded false arm leaves exactly the rail and that person.
#[test]
fn a_real_tmux_person_published_after_snapshot_prevents_generic_furniture() {
    let Some(server) = live_server("focus-furniture-cas") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "200",
            "-y",
            "50",
            "-n",
            "Person",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    let window = tmux_out(&socket, &["display-message", "-p", "-t", session, "#{window_id}"]);
    let rail = tmux_out(&socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    tmux_ok(
        &socket,
        &[
            "set-option",
            "-w",
            "-t",
            &window,
            tags::WINDOW,
            placement::FOCUS_WINDOW_ID,
            ";",
            "set-option",
            "-p",
            "-t",
            &rail,
            tags::SIDEBAR,
            "1",
            ";",
            "set-option",
            "-t",
            session,
            COLUMNS_OPTION,
            "26",
        ],
    );
    let tmux = InterleavingFocusBodyTmux {
        inner: SocketTmux { socket: socket.clone() },
        rail: rail.clone(),
        injected: Mutex::new(None),
    };
    let parked = effects::Parked {
        organization: "acme",
        rail_program: Some(RAIL_PROGRAM),
        company_dir: std::path::Path::new("/company"),
    };

    assert_eq!(effects::ensure_focus_window(&tmux, session, &parked), Some(window.clone()));
    let injected = tmux.injected.lock().expect("interleave lock").clone().expect("person body");
    let rows = tmux_out(
        &socket,
        &[
            "list-panes",
            "-t",
            &window,
            "-F",
            &format!(
                "#{{pane_id}}\t#{{{}}}\t#{{{}}}\t#{{{}}}",
                tags::SIDEBAR,
                tags::PERSON,
                tags::ASLEEP
            ),
        ],
    );
    assert_eq!(rows.lines().count(), 2, "no third generic pane: {rows:?}");
    assert!(rows.lines().any(|row| row.starts_with(&format!("{rail}\t1\t\t"))));
    assert!(
        rows.lines().any(|row| row.starts_with(&format!("{injected}\t\tnia"))),
        "the injected person is the only body: {rows:?}"
    );
    assert!(rows.lines().all(|row| !row.ends_with("\t__focus__")), "no generic tag: {rows:?}");
}

#[test]
fn a_real_tmux_guarded_rail_only_write_tags_the_new_body_not_the_rail() {
    let Some(server) = live_server("focus-furniture-cas-true") else { return };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "200",
            "-y",
            "50",
            "-n",
            "Person",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(&socket, &["set-option", "-g", "pane-base-index", "7"]);
    let window = tmux_out(&socket, &["display-message", "-p", "-t", session, "#{window_id}"]);
    let rail = tmux_out(&socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    tmux_ok(
        &socket,
        &[
            "set-option",
            "-w",
            "-t",
            &window,
            tags::WINDOW,
            placement::FOCUS_WINDOW_ID,
            ";",
            "set-option",
            "-p",
            "-t",
            &rail,
            tags::SIDEBAR,
            "1",
            ";",
            "set-option",
            "-t",
            session,
            COLUMNS_OPTION,
            "26",
        ],
    );
    let tmux = SocketTmux { socket: socket.clone() };
    let parked = effects::Parked {
        organization: "acme",
        rail_program: Some(RAIL_PROGRAM),
        company_dir: std::path::Path::new("/company"),
    };

    assert_eq!(effects::ensure_focus_window(&tmux, session, &parked), Some(window.clone()));
    let rows = tmux_out(
        &socket,
        &[
            "list-panes",
            "-t",
            &window,
            "-F",
            &format!(
                "#{{pane_id}}\t#{{pane_index}}\t#{{{}}}\t#{{{}}}",
                tags::SIDEBAR,
                tags::ASLEEP
            ),
        ],
    );
    let lines = rows.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "one rail and one notice: {rows:?}");
    assert!(
        lines.iter().any(|row| row.starts_with(&format!("{rail}\t7\t1\t"))),
        "the rail keeps its identity and is never tagged asleep: {rows:?}"
    );
    assert!(
        lines.iter().any(|row| row.split('\t').skip(1).eq(["8", "", "__focus__"])),
        "the appended body at rail index + 1 carries the generic tag: {rows:?}"
    );
}

/// The cold-click frame is proved on a private real tmux server. The same pane
/// cell starts as generic focus furniture and ends as Priya's person-specific
/// startup body; no pane is added, removed, or moved.
#[test]
fn a_real_tmux_cold_focus_repaints_the_same_final_pane_before_pi() {
    let Some(server) = live_server("cold-focus-final-pane") else { return };
    let socket = server.socket();
    tmux_ok(
        socket,
        &[
            "new-session",
            "-d",
            "-s",
            "org-acme_",
            "-n",
            "Person",
            "/bin/sh",
            "-c",
            "printf 'Click a person in the sidebar'; while :; do sleep 3600 & wait $!; done",
        ],
    );
    let window = tmux_out(socket, &["display-message", "-p", "-t", "org-acme_", "#{window_id}"]);
    let pane = tmux_out(socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    let rail = tmux_out(
        socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-t",
            &pane,
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(socket, &["set-option", "-t", "org-acme_", tags::ORGANIZATION, "acme"]);
    tmux_ok(socket, &["set-option", "-w", "-t", &window, tags::ORGANIZATION, "acme"]);
    tmux_ok(socket, &["set-option", "-w", "-t", &window, tags::WINDOW, placement::FOCUS_WINDOW_ID]);
    tmux_ok(socket, &["set-option", "-p", "-t", &pane, tags::ASLEEP, placement::FOCUS_WINDOW_ID]);
    tmux_ok(socket, &["set-option", "-p", "-t", &rail, tags::SIDEBAR, "1"]);
    let pane_ids_before = tmux_out(socket, &["list-panes", "-t", &window, "-F", "#{pane_id}"]);
    let before = tmux_out(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{pane_id}\t#{window_id}\t#{pane_left}\t#{pane_width}",
        ],
    );

    let tmux = SocketTmux { socket: socket.to_owned() };
    let claimed = effects::show_waking_focus(
        &tmux,
        "org-acme_",
        &effects::FocusPerson {
            person_id: "analyst",
            name: "Priya",
            role: "Quant Analyst",
            accent: "#c75e00",
            standing: None,
        },
    );
    assert_eq!(claimed.as_deref(), Some(pane.as_str()));
    assert_valid_wake_claim(socket, &pane);

    let body = (0..100)
        .find_map(|_| {
            let frame = tmux_out(socket, &["capture-pane", "-p", "-t", &pane]);
            if frame.contains("Priya is starting") {
                Some(frame)
            } else {
                std::thread::yield_now();
                None
            }
        })
        .unwrap_or_default();
    let after = tmux_out(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{pane_id}\t#{window_id}\t#{pane_left}\t#{pane_width}",
        ],
    );
    assert_eq!(after, before, "the final pane id, window, position and width stay stable");
    assert_eq!(
        tmux_out(socket, &["list-panes", "-t", &window, "-F", "#{pane_id}"]),
        pane_ids_before,
        "the production rail and body are the same two pane ids after the click"
    );
    assert!(body.contains("Priya is starting"), "first body: {body:?}");
    assert!(!body.contains("Click a person"), "generic body must not survive: {body:?}");
    let border = tmux_out(socket, &["show-options", "-p", "-v", "-t", &pane, "pane-border-format"]);
    assert!(border.contains("Quant Analyst"), "role border: {border:?}");
    assert!(!border.contains("#{pane_title}"), "raw tmux title must not be border authority");
}

#[test]
fn a_real_tmux_parks_one_orphan_waking_body_then_reuses_the_same_pane_for_idas_card() {
    let Some((_server, window, rail, body)) = live_orphan_waking_focus("orphan-waking-success")
    else {
        return;
    };
    let socket = format!(
        "{}/chiefd-orphan-waking-success-{}.sock",
        std::env::temp_dir().display(),
        std::process::id()
    );
    let before_pid = tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]);
    let rail_before =
        tmux_out(&socket, &["display-message", "-p", "-t", &rail, "#{pane_pid}\t#{pane_width}"]);
    let tmux = SocketTmux { socket: socket.clone() };

    let parked = effects::park_orphan_waking_focus(
        &tmux,
        "org-acme_",
        "acme",
        &BTreeSet::new(),
        &BTreeSet::new(),
        false,
    )
    .expect("desired-off orphan waking furniture is parked");
    assert_eq!(parked.pane, body);
    assert_eq!(parked.person, "nia");
    let parked_pid = tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]);
    assert_ne!(parked_pid, before_pid, "parking replaces the process in the same pane");
    assert_eq!(
        tmux_out(&socket, &["show-options", "-p", "-v", "-t", &body, tags::ASLEEP]),
        placement::FOCUS_WINDOW_ID
    );
    assert_eq!(
        tmux_out(&socket, &["show-options", "-p", "-v", "-t", &body, tags::WAKING_PERSON]),
        ""
    );
    assert_eq!(tmux_out(&socket, &["show-options", "-p", "-v", "-t", &body, tags::WAKE_CLAIM]), "");

    let card = effects::show_sleeping_focus(
        &tmux,
        "org-acme_",
        &effects::FocusPerson {
            person_id: "ida",
            name: "Ida",
            role: "Engineer",
            accent: "#c75e00",
            standing: None,
        },
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf 'Ida sleeping'; while :; do sleep 3600 & wait $!; done".to_owned(),
        ],
    )
    .expect("the next sleeping click takes the restored final body");
    assert_eq!(card, body);
    assert_eq!(
        tmux_out(&socket, &["show-options", "-p", "-v", "-t", &body, tags::SLEEPING_PERSON]),
        "ida"
    );
    assert_eq!(
        tmux_out(&socket, &["list-panes", "-t", &window, "-F", "#{pane_id}"])
            .lines()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([rail.as_str(), body.as_str()]),
        "the rail and final body are the only panes before and after recovery"
    );
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &rail, "#{pane_pid}\t#{pane_width}"],),
        rail_before,
        "recovery does not replace or resize the rail"
    );
}

#[test]
fn a_real_tmux_orphan_waking_guard_refuses_desired_retagged_respawned_or_moved_bodies() {
    let Some((_server, _window, _rail, body)) = live_orphan_waking_focus("orphan-waking-desired")
    else {
        return;
    };
    let socket = format!(
        "{}/chiefd-orphan-waking-desired-{}.sock",
        std::env::temp_dir().display(),
        std::process::id()
    );
    let before_pid = tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]);
    let tmux = SocketTmux { socket: socket.clone() };
    assert_eq!(
        effects::park_orphan_waking_focus(
            &tmux,
            "org-acme_",
            "acme",
            &BTreeSet::from(["nia".to_owned()]),
            &BTreeSet::new(),
            false,
        ),
        None,
        "current desired authority keeps the waking body"
    );
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
        before_pid
    );
    assert_eq!(
        effects::park_orphan_waking_focus(
            &tmux,
            "org-acme_",
            "acme",
            &BTreeSet::new(),
            &BTreeSet::from(["nia".to_owned()]),
            false,
        ),
        None,
        "a local pre-grant hand-off keeps the waking body before desired appears"
    );
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
        before_pid
    );

    for (label, race) in [
        ("retag", OrphanWakingRace::Retag),
        ("claim", OrphanWakingRace::ReplaceClaim),
        ("window-org-unset", OrphanWakingRace::UnsetWindowOrganization),
        ("session-org-unset", OrphanWakingRace::UnsetSessionOrganization),
        ("session-org-foreign", OrphanWakingRace::SetSessionOrganizationForeign),
        ("present-empty-forbidden", OrphanWakingRace::PresentEmptyForbidden),
        ("respawn", OrphanWakingRace::Respawn),
        ("rail-respawn", OrphanWakingRace::RespawnRail),
        ("rail-resize", OrphanWakingRace::ResizeRail),
        ("move", OrphanWakingRace::MoveWindow),
        ("session-recreate", OrphanWakingRace::RecreateSession),
    ] {
        let fixture_label = format!("orphan-waking-{label}");
        let Some((_server, _window, raced_rail, raced_body)) =
            live_orphan_waking_focus(&fixture_label)
        else {
            return;
        };
        let raced_socket = format!(
            "{}/chiefd-{}-{}.sock",
            std::env::temp_dir().display(),
            fixture_label,
            std::process::id()
        );
        let raced = InterleavingOrphanWakingTmux {
            inner: SocketTmux { socket: raced_socket.clone() },
            pane: raced_body.clone(),
            rail: raced_rail,
            race,
            fired: Mutex::new(false),
        };
        let raced_before_pid =
            tmux_out(&raced_socket, &["display-message", "-p", "-t", &raced_body, "#{pane_pid}"]);
        assert_eq!(
            effects::park_orphan_waking_focus(
                &raced,
                "org-acme_",
                "acme",
                &BTreeSet::new(),
                &BTreeSet::new(),
                false,
            ),
            None,
            "{label} race invalidates the snapshot authority"
        );
        assert!(*raced.fired.lock().expect("race fired"), "{label} interleave is non-vacuous");
        assert_ne!(
            tmux_out(&raced_socket, &["show-options", "-p", "-v", "-t", &raced_body, tags::ASLEEP],),
            placement::FOCUS_WINDOW_ID,
            "{label} race was not overwritten by stale park authority"
        );
        if matches!(
            race,
            OrphanWakingRace::UnsetWindowOrganization
                | OrphanWakingRace::UnsetSessionOrganization
                | OrphanWakingRace::SetSessionOrganizationForeign
                | OrphanWakingRace::PresentEmptyForbidden
        ) {
            assert_eq!(
                tmux_out(
                    &raced_socket,
                    &["display-message", "-p", "-t", &raced_body, "#{pane_pid}"],
                ),
                raced_before_pid,
                "{label} refuses before the destructive respawn"
            );
        }
        if matches!(race, OrphanWakingRace::PresentEmptyForbidden) {
            assert!(
                tmux_out(&raced_socket, &["show-options", "-p", "-t", &raced_body, tags::PERSON],)
                    .starts_with(tags::PERSON),
                "cleanup preserves the raced present-empty local option"
            );
        }
    }
}

#[test]
fn a_real_tmux_orphan_waking_snapshot_refuses_missing_foreign_and_present_empty_local_scope() {
    #[derive(Clone, Copy)]
    enum Scope {
        Pane,
        Window,
        Session,
    }
    for (label, scope, option, value) in [
        ("pane-claim-missing", Scope::Pane, tags::WAKE_CLAIM, None),
        ("pane-claim-empty", Scope::Pane, tags::WAKE_CLAIM, Some("")),
        ("pane-recovery-ready-mask", Scope::Pane, "@chief_waking_recovery_ready_v1", Some("1")),
        ("window-org-missing", Scope::Window, tags::ORGANIZATION, None),
        ("window-org-foreign", Scope::Window, tags::ORGANIZATION, Some("foreign")),
        ("window-org-empty", Scope::Window, tags::ORGANIZATION, Some("")),
        ("window-logical-missing", Scope::Window, tags::WINDOW, None),
        ("window-logical-foreign", Scope::Window, tags::WINDOW, Some("foreign")),
        ("window-logical-empty", Scope::Window, tags::WINDOW, Some("")),
        ("window-recovery-ready-mask", Scope::Window, "@chief_waking_recovery_ready_v1", Some("1")),
        ("session-org-missing", Scope::Session, tags::ORGANIZATION, None),
        ("session-org-foreign", Scope::Session, tags::ORGANIZATION, Some("foreign")),
        ("session-org-empty", Scope::Session, tags::ORGANIZATION, Some("")),
    ] {
        let fixture_label = format!("orphan-local-scope-{label}");
        let Some((_server, window, _rail, body)) = live_orphan_waking_focus(&fixture_label) else {
            return;
        };
        let socket = format!(
            "{}/chiefd-{}-{}.sock",
            std::env::temp_dir().display(),
            fixture_label,
            std::process::id()
        );
        let target = match scope {
            Scope::Pane => body.as_str(),
            Scope::Window => window.as_str(),
            Scope::Session => "org-acme_",
        };
        let mut command = vec!["set-option"];
        match scope {
            Scope::Pane => command.push("-p"),
            Scope::Window => command.push("-w"),
            Scope::Session => {}
        }
        if value.is_none() {
            command.push("-u");
        }
        command.extend(["-t", target, option]);
        if let Some(value) = value {
            command.push(value);
        }
        tmux_ok(&socket, &command);
        let before_pid = tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]);
        let tmux = SocketTmux { socket: socket.clone() };

        assert_eq!(
            effects::park_orphan_waking_focus(
                &tmux,
                "org-acme_",
                "acme",
                &BTreeSet::new(),
                &BTreeSet::new(),
                false,
            ),
            None,
            "{label} cannot become recovery authority"
        );
        assert_eq!(
            tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
            before_pid,
            "{label} refuses before respawn"
        );
    }
}

#[test]
fn a_real_tmux_claim_that_is_never_seen_is_parked_only_after_the_brain_watched_it() {
    // THE LIVE WEDGE, 2026-08-18. A claim that chiefd never calls desired never
    // gets a pending mark, so on a session that is ALREADY recovery-ready it
    // matched neither the startup case nor the withdrawn-claim case and was
    // refused every round forever. The operator watched `… is starting…` for an
    // hour with no process behind it, and no pass could reclaim the pane.
    //
    // The fence that produced that refusal is not weakened here: this same
    // shape is still refused outright when the caller has not watched it. Only
    // a caller that says it saw THIS pane and THIS claim stay unseen across its
    // own bounded count may park it.
    let Some((_server, _window, _rail, body)) = live_orphan_waking_focus("orphan-never-seen")
    else {
        return;
    };
    let socket = format!(
        "{}/chiefd-orphan-never-seen-{}.sock",
        std::env::temp_dir().display(),
        std::process::id()
    );
    // Recovery-ready, and NO pending or desired-seen mark: exactly the pane the
    // live company was stuck on.
    tmux_ok(&socket, &["set-option", "-t", "org-acme_", "@chief_waking_recovery_ready_v1", "1"]);
    let before_pid = tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]);
    let tmux = SocketTmux { socket: socket.clone() };

    assert_eq!(
        effects::park_orphan_waking_focus(
            &tmux,
            "org-acme_",
            "acme",
            &BTreeSet::new(),
            &BTreeSet::new(),
            false,
        ),
        None,
        "an unseen claim the caller has not watched is still refused"
    );
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
        before_pid,
        "and it refuses BEFORE any respawn"
    );

    let parked = effects::park_orphan_waking_focus(
        &tmux,
        "org-acme_",
        "acme",
        &BTreeSet::new(),
        &BTreeSet::new(),
        true,
    )
    .expect("a claim watched unseen past the bound is finally parked");
    assert_eq!(parked.pane, body);
    assert_ne!(
        tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
        before_pid,
        "the exact body process changes once when the orphan is reclaimed"
    );
}

#[test]
fn a_real_tmux_shared_desired_claim_is_parked_only_after_its_withdrawal() {
    let Some((_server, _window, _rail, body)) = live_orphan_waking_focus("orphan-shared-claim")
    else {
        return;
    };
    let socket = format!(
        "{}/chiefd-orphan-shared-claim-{}.sock",
        std::env::temp_dir().display(),
        std::process::id()
    );
    tmux_ok(
        &socket,
        &[
            "set-option",
            "-p",
            "-t",
            &body,
            tags::WAKING_PENDING,
            "claim-nia",
            ";",
            "set-option",
            "-t",
            "org-acme_",
            "@chief_waking_recovery_ready_v1",
            "1",
        ],
    );
    let before_pid = tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]);
    let tmux = SocketTmux { socket: socket.clone() };

    assert_eq!(
        effects::park_orphan_waking_focus(
            &tmux,
            "org-acme_",
            "acme",
            &BTreeSet::from(["nia".to_owned()]),
            &BTreeSet::new(),
            false,
        ),
        None,
        "desired=true records authority and does not park"
    );
    assert_eq!(
        tmux_out(&socket, &["show-options", "-p", "-v", "-t", &body, tags::WAKING_DESIRED_SEEN],),
        "claim-nia",
        "the desired observation is shared through the exact pane claim"
    );
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
        before_pid
    );

    let parked = effects::park_orphan_waking_focus(
        &tmux,
        "org-acme_",
        "acme",
        &BTreeSet::new(),
        &BTreeSet::new(),
        false,
    )
    .expect("the later desired=false read retires the same shared claim");
    assert_eq!(parked.pane, body);
    assert_ne!(
        tmux_out(&socket, &["display-message", "-p", "-t", &body, "#{pane_pid}"]),
        before_pid,
        "the exact body process changes once after authority withdrawal"
    );
}

/// A sleeping card, its waking animation process, and Pi all own one final
/// body. The button transition changes tags and never changes pane identity.
#[test]
fn a_real_tmux_sleeping_card_wakes_into_fake_pi_in_the_same_pane() {
    let Some(server) = live_server("sleeping-card-same-pane") else { return };
    let socket = server.socket();
    tmux_ok(
        socket,
        &[
            "new-session",
            "-d",
            "-s",
            "org-acme_",
            "-n",
            "Person",
            "/bin/sh",
            "-c",
            "printf 'Click a person'; while :; do sleep 3600 & wait $!; done",
        ],
    );
    let window = tmux_out(socket, &["display-message", "-p", "-t", "org-acme_", "#{window_id}"]);
    let body = tmux_out(socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    let rail = tmux_out(
        socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            "26",
            "-t",
            &body,
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(socket, &["set-option", "-t", "org-acme_", tags::ORGANIZATION, "acme"]);
    tmux_ok(socket, &["set-option", "-w", "-t", &window, tags::WINDOW, placement::FOCUS_WINDOW_ID]);
    tmux_ok(socket, &["set-option", "-p", "-t", &body, tags::ASLEEP, placement::FOCUS_WINDOW_ID]);
    tmux_ok(socket, &["set-option", "-p", "-t", &rail, tags::SIDEBAR, "1"]);
    let before = tmux_out(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            &body,
            "#{pane_id}\t#{window_id}\t#{pane_left}\t#{pane_width}",
        ],
    );
    let ids = tmux_out(socket, &["list-panes", "-t", &window, "-F", "#{pane_id}"]);
    let tmux = SocketTmux { socket: socket.to_owned() };
    let card = effects::show_sleeping_focus(
        &tmux,
        "org-acme_",
        &effects::FocusPerson {
            person_id: "nia",
            name: "Nia",
            role: "Research #{pane_title} #[fg=red]",
            accent: "#c75e00",
            standing: None,
        },
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf 'Nia sleeping — Wake Up'; while :; do sleep 3600 & wait $!; done".to_owned(),
        ],
    )
    .expect("the card takes the final body");
    assert_eq!(card, body);
    let border = tmux_out(socket, &["show-options", "-p", "-v", "-t", &body, "pane-border-format"]);
    assert!(
        border.contains("Research ##{pane_title} ##[fg=red]"),
        "hostile role stays literal: {border:?}"
    );
    assert_eq!(
        tmux_out(socket, &["show-options", "-p", "-v", "-t", &body, tags::SLEEPING_PERSON]),
        "nia"
    );
    assert!(effects::activate_sleeping_focus(&tmux, "org-acme_", "acme", &body, "nia"));
    assert_eq!(
        tmux_out(socket, &["show-options", "-p", "-v", "-t", &body, tags::WAKING_PERSON]),
        "nia"
    );
    assert_eq!(
        tmux_out(socket, &["show-options", "-p", "-v", "-t", &body, tags::SLEEPING_PERSON]),
        ""
    );
    tmux_ok(
        socket,
        &[
            "respawn-pane",
            "-k",
            "-t",
            &body,
            "/bin/sh",
            "-c",
            "printf 'Nia Pi ready'; while :; do sleep 3600 & wait $!; done",
        ],
    );
    assert_eq!(
        tmux_out(
            socket,
            &[
                "display-message",
                "-p",
                "-t",
                &body,
                "#{pane_id}\t#{window_id}\t#{pane_left}\t#{pane_width}"
            ]
        ),
        before,
        "card, waking state and Pi keep one final pane and geometry"
    );
    assert_eq!(tmux_out(socket, &["list-panes", "-t", &window, "-F", "#{pane_id}"]), ids);
    assert_eq!(tmux_out(socket, &["display-message", "-p", "-t", &rail, "#{pane_width}"]), "26");
    let final_frame = tmux_out(socket, &["capture-pane", "-p", "-t", &body]);
    assert!(final_frame.contains("Nia Pi ready"));
    assert!(!final_frame.contains("Click a person"));
}

/// A click during the short rail-only handoff gap mints the one final waking
/// body directly, without first publishing generic furniture.
#[test]
fn a_real_tmux_rail_only_focus_mints_the_final_waking_body_directly() {
    let Some(server) = live_server("cold-focus-rail-only") else { return };
    let socket = server.socket();
    tmux_ok(
        socket,
        &[
            "new-session",
            "-d",
            "-s",
            "org-acme_",
            "-n",
            "Person",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600 & wait $!; done",
        ],
    );
    tmux_ok(socket, &["set-option", "-g", "pane-base-index", "7"]);
    tmux_ok(socket, &["set-option", "-t", "org-acme_", tags::ORGANIZATION, "acme"]);
    tmux_ok(socket, &["set-option", "-t", "org-acme_", "@chief_sidebar_columns", "26"]);
    let focus = tmux_out(socket, &["display-message", "-p", "-t", "org-acme_", "#{window_id}"]);
    let rail = tmux_out(socket, &["display-message", "-p", "-t", &focus, "#{pane_id}"]);
    tmux_ok(socket, &["set-option", "-w", "-t", &focus, tags::ORGANIZATION, "acme"]);
    tmux_ok(socket, &["set-option", "-w", "-t", &focus, tags::WINDOW, placement::FOCUS_WINDOW_ID]);
    tmux_ok(socket, &["set-option", "-p", "-t", &rail, tags::SIDEBAR, "1"]);
    let rail_pid = tmux_out(socket, &["display-message", "-p", "-t", &rail, "#{pane_pid}"]);

    let tmux = SocketTmux { socket: socket.to_owned() };
    let waking = effects::show_waking_focus(
        &tmux,
        "org-acme_",
        &effects::FocusPerson {
            person_id: "nia",
            name: "Nia",
            role: "Research Lead",
            accent: "#c75e00",
            standing: None,
        },
    )
    .expect("rail-only focus creates Nia's final body");

    assert_eq!(tmux_out(socket, &["display-message", "-p", "-t", &rail, "#{pane_pid}"]), rail_pid);
    assert_eq!(
        tmux_out(socket, &["show-options", "-v", "-t", "org-acme_", "@chief_sidebar_columns"]),
        "26",
        "the rail's recorded width does not move"
    );
    assert_eq!(
        tmux_out(socket, &["list-panes", "-t", &focus, "-F", "#{pane_id}"])
            .lines()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([rail.as_str(), waking.as_str()])
    );
    assert_eq!(
        tmux_out(socket, &["show-options", "-p", "-v", "-t", &waking, tags::WAKING_PERSON]),
        "nia"
    );
    assert_valid_wake_claim(socket, &waking);
    assert_eq!(
        tmux_out(socket, &["display-message", "-p", "-t", &waking, "#{pane_index}"]),
        "8",
        "base-index 7 makes the final body addressable in the same batch at index 8"
    );
    assert_eq!(
        tmux_out(socket, &["display-message", "-p", "-t", &rail, "#{pane_width}"]),
        "26",
        "the visible rail keeps its product width"
    );
    let body = (0..100)
        .find_map(|_| {
            let frame = tmux_out(socket, &["capture-pane", "-p", "-t", &waking]);
            if frame.contains("Nia is starting") {
                Some(frame)
            } else {
                std::thread::yield_now();
                None
            }
        })
        .unwrap_or_default();
    assert!(body.contains("Nia is starting"), "first body: {body:?}");
    assert!(!body.contains("Click a person"), "no generic frame: {body:?}");
    let border =
        tmux_out(socket, &["show-options", "-p", "-v", "-t", &waking, "pane-border-format"]);
    assert!(border.contains("Research Lead"), "role border: {border:?}");
    assert!(!border.contains("#{pane_title}"), "raw title is never visible authority");
}

/// The width the rail records, and therefore the width every layout must give
/// it back. Any other number on the glass is the rail losing the operator's
/// drag.
const LIVE_RAIL_COLUMNS: i64 = 26;

/// Mint ONE PERSON'S window, tagged and railed exactly as converge mints it: a
/// rail in the first cell at the recorded width, then the person's own pane
/// filling everything beyond it.
///
/// This replaced `live_department`, which built the retired shape — one window
/// per department, its people tiled into a grid beside the rail. A fixture that
/// still built that would be testing a topology the product no longer produces,
/// and every width assertion below it would be about the wrong thing.
fn live_person_window(socket: &str, session: &str, person: &str) -> PersonFixture {
    let logical = crate::placement::person_window_id(person);
    let window = tmux_out(
        socket,
        &["new-window", "-d", "-t", session, "-n", person, "-P", "-F", "#{window_id}"],
    );
    tmux_ok(socket, &["set-option", "-w", "-t", &window, "@organization_window_id", &logical]);
    tmux_ok(socket, &["set-option", "-w", "-t", &window, "@organization_id", "acme"]);
    // The pane `new-window` made is the person; the rail is split in BEFORE it,
    // which is where converge puts it.
    let pane = tmux_out(socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    let rail = tmux_out(
        socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            &LIVE_RAIL_COLUMNS.to_string(),
            "-t",
            &pane,
            "-P",
            "-F",
            "#{pane_id}",
        ],
    );
    tmux_ok(socket, &["set-option", "-p", "-t", &rail, "@organization_sidebar", "1"]);
    tmux_ok(socket, &["set-option", "-p", "-t", &pane, "@organization_person_id", person]);
    tmux_ok(socket, &["set-option", "-p", "-t", &pane, "@organization_window_id", &logical]);
    tmux_ok(socket, &["set-option", "-p", "-t", &pane, "@organization_id", "acme"]);
    PersonFixture { person: person.to_owned(), window, rail, pane }
}

/// One person's window, as the live tests read it.
struct PersonFixture {
    person: String,
    window: String,
    rail: String,
    pane: String,
}

/// `pane_id -> (left, width)` for one window, read fresh.
fn live_geometry(socket: &str, window: &str) -> BTreeMap<String, (i64, i64)> {
    tmux_out(socket, &["list-panes", "-t", window, "-F", "#{pane_id}\t#{pane_left}\t#{pane_width}"])
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let pane = parts.next()?.to_owned();
            let left = parts.next()?.parse().ok()?;
            let width = parts.next()?.parse().ok()?;
            Some((pane, (left, width)))
        })
        .collect()
}

/// The session's active window id. Active-ness is per SESSION, not per client,
/// which is why this is asked of the session and not of a pane.
fn live_active_window(socket: &str, session: &str) -> String {
    tmux_out(socket, &["display-message", "-p", "-t", session, "#{window_id}"])
}

/// Every window id in list order, which is the order the operator reads.
fn live_windows(socket: &str, session: &str) -> Vec<String> {
    tmux_out(socket, &["list-windows", "-t", session, "-F", "#{window_id}"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The window carrying one logical id, by TAG — never by name.
fn live_tagged_window(socket: &str, session: &str, logical: &str) -> Option<String> {
    tmux_out(
        socket,
        &["list-windows", "-t", session, "-F", "#{window_id}\t#{@organization_window_id}"],
    )
    .lines()
    .filter_map(|line| line.split_once('\t'))
    .find(|(_, tag)| tag.trim() == logical)
    .map(|(window, _)| window.trim().to_owned())
}

/// THE UNSET-OPTION DEFAULT, WHICH IS THE OTHER HALF OF THE SHRINK.
///
/// `interpret::observe_rail` reads `@chief_sidebar_columns` to decide how many
/// columns to reserve, and it used to fall back to the COLLAPSED width when the
/// option could not be read — so a session whose rail had not recorded a width
/// yet had every window laid beside a four-column sidebar reading `Depa` /
/// `Peop`. Nothing wrote a better number back, because the rail declines to
/// record a width it cannot be read at, so the shrink was permanent.
///
/// Read off a REAL server, because the thing under test is what tmux answers
/// for an option nobody has set.
#[test]
fn an_unset_width_option_reads_as_the_open_rail_and_never_the_collapsed_one() {
    let Some(server) = live_server("sidebar-width-default") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);

    // Exactly `observe_rail`'s read, against a session that has recorded
    // nothing.
    let raw = tmux_out(
        &socket,
        &["display-message", "-p", "-t", session, "-F", "#{@chief_sidebar_columns}"],
    );
    let observed = raw.trim().parse::<i64>().unwrap_or(super::brain::RAIL_DEFAULT_COLUMNS);
    assert_eq!(
        observed,
        super::brain::RAIL_DEFAULT_COLUMNS,
        "an unreadable option means we do not know the width, NOT that the operator \
         collapsed their sidebar: {raw:?}"
    );
    assert_ne!(
        super::brain::RAIL_DEFAULT_COLUMNS,
        crate::layout::RAIL_COLLAPSED_COLUMNS,
        "the two are different numbers, which is the whole point of the fallback"
    );

    // And the rail's own reader agrees, because two halves that disagree about
    // this is what produced a sidebar the operator never chose.
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, 31);
    let recorded =
        tmux_out(&socket, &["show-options", "-v", "-t", session, "@chief_sidebar_columns"]);
    assert_eq!(recorded.trim(), "31", "and a recorded width is what both halves then read");
}

/// THE SLEEPER CLICK, OBSERVED ON A REAL SERVER — the flicker, measured.
///
/// The operator drove this by hand and rejected it: "there shouldn't be any
/// flicker when I click a sleeping machine. Immediately on the right-hand side
/// it would be the new window and show a loading UI, and then once the agent
/// starts flip immediately. The sidebar is moving and it keeps shifting back
/// and forth when I do selection."
///
/// Every clause here is read back off tmux, because that is the only place the
/// three complaints are visible: a window that does not exist yet, a rail that
/// is briefly the whole screen, and a geometry that changes twice.
/// The panes of one window carrying a non-empty `tag`.
fn live_panes_with_tag(socket: &str, window: &str, tag: &str) -> Vec<String> {
    tmux_out(socket, &["list-panes", "-t", window, "-F", &format!("#{{pane_id}}\t#{{{tag}}}")])
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(pane, _)| pane.trim().to_owned())
        .collect()
}

/// AN OVERVIEW SURVIVES THE SWEEP THAT RETIRES A NOTICE, and this is the defect
/// that kept the card off the glass entirely.
///
/// The sweeper reads a pane's `@chief_asleep_for` tag and asks whether that
/// department is still in the roster. An overview's tag is a WINDOW id
/// (`__overview__:<department>`), which is in no roster, so the first build
/// killed every card the instant it was drawn — measured on a live box,
/// `sidebar.department.removed … "that department is no longer in the roster"`
/// for `__overview__:research`, once per pass, minted and destroyed in a loop.
///
/// The second half is the other direction: a notice saying "everyone is asleep"
/// becomes FALSE when somebody comes up and must go, but a card reporting
/// "3 asleep" simply reports "1 up" on the next pass. Sweeping it would blank
/// the surface every time a department woke.
#[test]
fn an_overview_is_not_swept_when_its_department_wakes_and_only_dies_with_it() {
    let live: BTreeSet<String> = ["research".to_owned()].into_iter().collect();
    let known: BTreeSet<String> = ["research".to_owned()].into_iter().collect();
    let overview = crate::placement::overview_window_id("research");

    assert_eq!(
        crate::placement::overview_department_id(&overview),
        Some("research"),
        "the sweeper can recover the department an overview is about"
    );
    assert_eq!(
        crate::placement::overview_department_id("research"),
        None,
        "and a plain department id is not mistaken for one"
    );

    // The department is live: a NOTICE about it is stale, an overview is not.
    let tmux = RecordingTmux::answering(&[(
        "#{pane_id}\t#{@chief_asleep_for}",
        &format!("%9\t{overview}\n%10\tresearch"),
    )]);
    effects::close_sleeping_notices(&tmux, "org-acme_", &live, &known);
    let calls = tmux.calls();
    let killed: Vec<&String> = calls.iter().filter(|call| call.contains("kill-")).collect();
    assert!(
        !killed.iter().any(|call| call.contains("%9")),
        "the overview pane survives its department waking: {killed:?}"
    );
}

// ---------------------------------------------------------------------------
// THE DEPARTMENT CARD IS A LIVE REPORT
// ---------------------------------------------------------------------------

/// One card argv whose LAST argument is the payload, exactly as production
/// builds it (`chief department-card <json>`).
///
/// `/bin/sh -c <hold> <payload>` puts the payload in `$0`, so the pane holds
/// open and the fingerprint this module computes is taken from the real thing
/// rather than from a stand-in that happens to differ.
fn card_argv(payload: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "while :; do sleep 30; done".to_owned(),
        payload.to_owned(),
    ]
}

/// The one overview these unit tests refresh.
fn quant_overview() -> effects::StandingOverview {
    effects::StandingOverview {
        window: "@9".to_owned(),
        overview_id: crate::placement::overview_window_id("quant"),
        department_id: "quant".to_owned(),
    }
}

/// A tmux holding one overview window: a rail (`%12`) and the card (`%8`),
/// where the card pane already draws `drawn`.
fn tmux_for_a_standing_card(drawn: Option<&str>) -> RecordingTmux {
    let overview = crate::placement::overview_window_id("quant");
    let stamped: Option<String> = drawn.map(|payload| {
        use sha2::{Digest as _, Sha256};
        Sha256::digest(payload.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
    });
    RecordingTmux::answering(&[
        ("list-windows", &format!("@9\t{overview}")),
        ("-F #{pane_id}\t#{@organization_sidebar}\t#{pane_dead}", "%12\t1\t0\n%8\t\t0"),
        ("-t %12 #{@chief_asleep_for}", ""),
        ("-t %8 #{@chief_asleep_for}", &overview),
        ("-t %8 #{@chief_department_card}", stamped.as_deref().unwrap_or_default()),
    ])
}

/// THE CHURN GUARD, and it is the reason this verb may exist at all.
///
/// It runs from the company-read path, which a chatty company wakes many times
/// a second. `show_department_overview` records at length what an effect that
/// fires on every one of those wakes did to the glass — relaid, re-selected,
/// woke the other rail, and churned continuously. A pass that changes nothing
/// must therefore issue NO tmux write, not a cheap one.
#[test]
fn an_unchanged_department_issues_no_tmux_mutation_at_all() {
    let payload = r#"{"name":"Quant","path":[],"members":[],"children":[]}"#;
    let tmux = tmux_for_a_standing_card(Some(payload));
    let card = card_argv(payload);

    // TEN CHANGEFEED WAKES. One would pass on a guard that merely happened not
    // to fire; the loop this exists to stop fired on EVERY wake, and a chatty
    // company wakes this path many times a second.
    for _ in 0..10 {
        assert!(!effects::refresh_department_card(
            &tmux,
            "org-acme_",
            &quant_overview(),
            Some(&card)
        ));
    }

    // COUNTED, not shape-matched. The claim is a NUMBER — zero mutations — and
    // a test that asserted the absence of one particular verb would pass while
    // some other write churned the glass. Every tmux command a read may issue
    // is named here, and anything else is a mutation whatever it says.
    let calls = tmux.calls();
    let mutations: Vec<&String> = calls
        .iter()
        .filter(|call| {
            !(call.starts_with("list-windows")
                || call.starts_with("list-panes")
                || call.starts_with("display-message -p"))
        })
        .collect();
    assert_eq!(
        mutations.len(),
        0,
        "ten unchanged wakes issued {} tmux mutation(s): {mutations:?}",
        mutations.len()
    );
    // AND THE READS ARE BOUNDED TOO. A guard that answers "nothing changed" by
    // re-reading the whole session on every wake is cheaper than a relayout and
    // still wrong: this path runs many times a second beside a live tmux
    // server. Four reads per pass — the window list, that window's panes, and
    // the two pane tags — is the whole cost of knowing there is nothing to do.
    assert_eq!(calls.len(), 40, "four reads per unchanged pass, ten passes: {calls:?}");
}

/// THE DEFECT, stated as a test. The card was argv at spawn time and only a
/// department CLICK ever spawned it again, so it froze the instant it was drawn
/// — the operator's rail read `Executive 2/5` with two green dots while the card
/// beside it read `0 up · 4 asleep · 1 starting`.
#[test]
fn a_changed_department_repaints_its_card_in_place_and_moves_no_geometry() {
    let was = r#"{"name":"Quant","members":[{"state":"Sleeping"}]}"#;
    let now = r#"{"name":"Quant","members":[{"state":"Working"}]}"#;
    let tmux = tmux_for_a_standing_card(Some(was));
    let card = card_argv(now);

    assert!(
        effects::refresh_department_card(&tmux, "org-acme_", &quant_overview(), Some(&card),),
        "a state that moved is exactly what this surface exists to show"
    );

    let calls = tmux.calls();
    let repaint = calls
        .iter()
        .find(|call| call.contains("respawn-pane"))
        .expect("the card pane is respawned with the new payload");
    assert!(repaint.contains("respawn-pane -k -t %8"), "the CARD's pane, by its tag: {repaint}");
    assert!(repaint.contains(now), "carrying the payload it is now to draw: {repaint}");
    assert!(
        repaint.contains(&format!("set-option -p -t %8 {}", tags::DEPARTMENT_CARD)),
        "and stamping what it now draws, in the same batch: {repaint}"
    );

    // THE THREE THINGS THIS PATH MAY NEVER DO. Each of them is a churn shape
    // `show_department_overview` documents from the operator's own box.
    for forbidden in ["select-window", "select-layout", "split-window", "select-pane", "kill-pane"]
    {
        assert!(
            !calls.iter().any(|call| call.contains(forbidden)),
            "a refresh-path repaint never issues {forbidden}: {calls:?}"
        );
    }
}

/// A pane that has never been stamped — the one-line notice the card replaces,
/// or a card spawned before this rule existed — is repainted once and then
/// settles.
#[test]
fn an_unstamped_pane_is_repainted_once_and_then_stands_still() {
    let payload = r#"{"name":"Quant","members":[]}"#;
    let tmux = tmux_for_a_standing_card(None);
    let card = card_argv(payload);

    assert!(
        effects::refresh_department_card(&tmux, "org-acme_", &quant_overview(), Some(&card)),
        "a pane drawing something nobody recorded is a pane that must be redrawn"
    );

    let settled = tmux_for_a_standing_card(Some(payload));
    assert!(
        !effects::refresh_department_card(&settled, "org-acme_", &quant_overview(), Some(&card)),
        "and the pass after the repaint is unchanged"
    );
}

/// No window, no card, and no attempt to mint one. Refreshing is not showing:
/// a department the operator is not looking at must not acquire a window from
/// the changefeed.
#[test]
fn a_department_with_no_overview_window_is_left_alone() {
    let tmux = RecordingTmux::answering(&[("list-windows", "@1\tquant\n@7\t__focus__")]);
    assert!(
        effects::standing_overviews(&tmux, "org-acme_").is_empty(),
        "a people window and the focus window are not overviews"
    );
    for call in tmux.calls() {
        assert!(call.starts_with("list-windows"), "nothing but the one read: {call}");
    }
}

/// **EVERY STANDING CARD, NEVER "THE SELECTED ONE".** Measured on the operator's
/// box: a session holds one overview window per department they have clicked,
/// and a refresh keyed on the SELECTION left every other card frozen — the
/// original defect surviving inside its own repair.
#[test]
fn every_standing_overview_is_found_and_a_focus_or_people_window_is_not_one() {
    let tmux = RecordingTmux::answering(&[(
        "list-windows",
        "@1\texecutive\n@3\t__overview__:executive\n@4\t__focus__\n@5\t__overview__:research",
    )]);
    let standing = effects::standing_overviews(&tmux, "org-acme_");
    assert_eq!(
        standing.iter().map(|one| one.department_id.as_str()).collect::<Vec<_>>(),
        ["executive", "research"],
        "both cards on this glass, and neither the people window nor the focus body"
    );
    assert_eq!(standing[1].window, "@5");
    assert_eq!(standing[1].overview_id, "__overview__:research");
}

/// There is nothing to compare a one-line notice against, so the fallback the
/// brain uses before its first company read is never repainted over.
#[test]
fn a_department_with_no_card_to_draw_is_left_alone() {
    let tmux = tmux_for_a_standing_card(None);
    assert!(!effects::refresh_department_card(&tmux, "org-acme_", &quant_overview(), None));
    assert!(tmux.calls().is_empty(), "it does not even look: {:?}", tmux.calls());
}

/// The overview a live test clicks a department onto.
///
/// The CARD's own drawing is unit-tested (`department_card::tests`); what a live
/// server can prove and nothing else can is the SHAPE of the window it lands in
/// — a rail at its recorded width, one pane beside it, and not one person pane
/// moved to make room. So the program here is a stand-in that simply holds the
/// pane open.
fn live_overview<'a>(
    overview_id: &'a str,
    department_name: &'a str,
    card: &'a [String],
    company_dir: &'a std::path::Path,
    rail_program: &'a str,
) -> effects::Overview<'a> {
    effects::Overview {
        organization: "acme",
        // THE OVERVIEW'S OWN LOGICAL ID, never the department's. Passing the
        // raw department id here found the window placement puts that
        // department's PEOPLE in and hung the card beside them — five panes
        // where there should be two, which is the bug this parameter name now
        // makes hard to write.
        department_id: overview_id,
        department_name,
        asleep: 0,
        rail_program: Some(rail_program),
        company_dir,
        card: Some(card),
    }
}

/// **THE INVARIANT THE OPERATOR ASKED FOR, AGAINST A REAL TMUX SERVER: A
/// PERSON'S PANE WIDTH DOES NOT CHANGE WHEN THEY ARE CLICKED.**
///
/// Their words, recorded on 2026-08-21 over a screen recording of the shipping
/// product: *"when I click on an agent I want it should be in the final
/// position, right? Why is it going half screen and growing?"* Frame 020 of
/// that clip shows the Chief filling the content area; frame 031, one click
/// later, shows Sam's text wrapped to half the pane with the right half blank,
/// and then it reflows out to full width.
///
/// The mechanism was measured with `tmux list-panes -a` on the same box: agents
/// sat at `42x17` and `64x17` inside their department's tiled window while the
/// focus body was `129x35`. A click `join-pane`d them from one to the other, so
/// tmux truncated the alternate screen at the new width and the Pi inside
/// repainted its entire scrollback.
///
/// A pane has exactly one size. Every desired person is now placed alone in a
/// window of their own (`placement::desired_topology`), every window is
/// normalized to the same canonical geometry, and a click is `select-window` +
/// `select-pane`. This test samples `#{pane_width}` either side of every click
/// it makes and asserts equality — which is the assertion no simulated tmux can
/// make, because it is a claim about what tmux DOES.
///
/// Skipped where there is no tmux, loud under CI — the same precondition shape
/// every other live test in this file uses.
#[test]
fn a_click_on_a_person_does_not_change_their_pane_width() {
    let staging = tempfile::tempdir().expect("tempdir");
    let Some(server) = live_server("sidebar-gestures") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, LIVE_RAIL_COLUMNS);
    let rail_program = staged_rail_program(staging.path());

    // ONE WINDOW PER PERSON, exactly as converge mints them.
    let chief = live_person_window(&socket, session, "chief");
    let quant_head = live_person_window(&socket, session, "quant-head");
    let analyst = live_person_window(&socket, session, "analyst");
    let trader = live_person_window(&socket, session, "trader");
    tmux_ok(&socket, &["select-window", "-t", &chief.window]);
    let parked = effects::ensure_focus_window(
        &tmux,
        session,
        &effects::Parked {
            organization: "acme",
            rail_program: Some(&rail_program),
            company_dir: staging.path(),
        },
    )
    .expect("the focus window is minted");
    tmux_ok(&socket, &["select-window", "-t", &chief.window]);

    // --- EVERY PERSON IS ALREADY AT THEIR FINAL WIDTH -----------------------
    let width_of = |pane: &str| -> i64 {
        tmux_out(&socket, &["display-message", "-p", "-t", pane, "#{pane_width}"])
            .parse()
            .expect("tmux reports a pane width")
    };
    let everybody = [&chief, &quant_head, &analyst, &trader];
    let before: Vec<i64> = everybody.iter().map(|person| width_of(&person.pane)).collect();
    assert!(
        before.iter().all(|width| *width == before[0]),
        "every person is laid out identically, so there is no width for a click to \
         change them TO: {before:?}"
    );
    assert_eq!(
        before[0],
        200 - LIVE_RAIL_COLUMNS - 1,
        "and that width is the window less the rail and its divider: {before:?}"
    );

    // --- THE CLICK, SAMPLED EITHER SIDE -------------------------------------
    //
    // The analyst is not the window the operator is on and not the first window
    // in the session: a gesture that only worked for the active window, or only
    // for the first, would pass for the wrong reason.
    let windows_before = live_windows(&socket, session);
    let widths_before: BTreeMap<&str, i64> =
        everybody.iter().map(|person| (person.person.as_str(), width_of(&person.pane))).collect();

    assert!(
        effects::show_person(
            &tmux,
            session,
            &effects::PersonClick { person_id: "analyst", display_name: "Ana Lyst" },
        )
        .shown,
        "the person has a live pane, so the operator is taken to it"
    );

    let widths_after: BTreeMap<&str, i64> =
        everybody.iter().map(|person| (person.person.as_str(), width_of(&person.pane))).collect();
    assert_eq!(
        widths_after, widths_before,
        "NOBODY was resized by the click — not the person clicked, and not the person \
         the operator was reading a moment ago"
    );
    assert_eq!(
        live_active_window(&socket, session),
        analyst.window,
        "and the glass is on the window that person is already alone in"
    );
    assert_eq!(
        live_windows(&socket, session),
        windows_before,
        "no window is minted, reaped or reordered by a click"
    );
    let laid = live_geometry(&socket, &analyst.window);
    assert_eq!(laid.len(), 2, "a rail and the person, and nothing else: {laid:?}");
    assert_eq!(
        laid.get(&analyst.rail).copied(),
        Some((0, LIVE_RAIL_COLUMNS)),
        "the rail keeps the exact width the operator dragged it to: {laid:?}"
    );
    assert_eq!(
        laid.get(&analyst.pane).map(|(left, _)| *left),
        Some(LIVE_RAIL_COLUMNS + 1),
        "and the person has every column beyond it: {laid:?}"
    );

    // --- A SECOND PERSON, AND THE FIRST IS LEFT ALONE -----------------------
    let pid_of =
        |pane: &str| tmux_out(&socket, &["display-message", "-p", "-t", pane, "#{pane_pid}"]);
    let analyst_pid = pid_of(&analyst.pane);
    assert!(
        effects::show_person(
            &tmux,
            session,
            &effects::PersonClick { person_id: "trader", display_name: "Tra Der" },
        )
        .shown
    );
    assert_eq!(
        everybody
            .iter()
            .map(|person| (person.person.as_str(), width_of(&person.pane)))
            .collect::<BTreeMap<&str, i64>>(),
        widths_before,
        "a retarget resizes nobody either — the person the operator has just stopped \
         reading keeps their pane exactly as it was"
    );
    assert_eq!(live_active_window(&socket, session), trader.window);
    assert_eq!(
        pid_of(&analyst.pane),
        analyst_pid,
        "and the person left behind was never respawned"
    );
    assert_eq!(
        live_geometry(&socket, &analyst.window).len(),
        2,
        "their window still holds them and their rail"
    );

    // --- THE IDEMPOTENT CLICK ----------------------------------------------
    let steady = live_geometry(&socket, &trader.window);
    assert!(
        effects::show_person(
            &tmux,
            session,
            &effects::PersonClick { person_id: "trader", display_name: "Tra Der" },
        )
        .shown
    );
    assert_eq!(live_geometry(&socket, &trader.window), steady, "clicking them again moves nothing");

    // --- A DEPARTMENT CLICK DRAGS NOBODY BACK -------------------------------
    //
    // The retired ruling was "click the department to move him back", and the
    // move back is what re-rendered him at a third of his width. Nobody moves in
    // either direction now: the department reports ITSELF, in a card window of
    // its own (#1195).
    let card_program =
        vec!["/bin/sh".to_owned(), "-c".to_owned(), "while :; do sleep 30; done".to_owned()];
    assert!(
        effects::show_department_overview(
            &tmux,
            session,
            &live_overview(
                &crate::placement::overview_window_id("quant"),
                "Quant",
                &card_program,
                staging.path(),
                &rail_program
            ),
        )
        .shown,
        "a department always has something to show about itself"
    );
    assert_eq!(
        everybody
            .iter()
            .map(|person| (person.person.as_str(), width_of(&person.pane)))
            .collect::<BTreeMap<&str, i64>>(),
        widths_before,
        "and a department click resizes nobody: {widths_before:?}"
    );
    let card = live_geometry(&socket, &live_active_window(&socket, session));
    assert_eq!(card.len(), 2, "the card window holds a rail and ONE card: {card:?}");
    for person in everybody {
        assert!(
            !card.contains_key(&person.pane),
            "{} was NOT dragged into the card window: {card:?}",
            person.person
        );
    }

    // --- THE CARD WINDOW IS STILL FURNITURE ONLY ----------------------------
    assert!(
        live_windows(&socket, session).contains(&parked),
        "the card window is permanent: minted once per session, destroyed by no gesture"
    );
    assert!(
        live_panes_with_tag(&socket, &parked, "@organization_person_id").is_empty(),
        "and it holds no live person — a person in it is a person converge wants \
         somewhere else, which is a pane about to be moved, which is a resize"
    );

    // --- ONE WINDOW IS ACTIVE PER SESSION -----------------------------------
    //
    // The fact `interpret::kill_window`'s deferral rests on, and no simulated
    // tmux can say: `#{window_active}` read against a live session names the
    // window the operator is actually on.
    tmux_ok(&socket, &["select-window", "-t", &chief.window]);
    let active: Vec<String> =
        tmux_out(&socket, &["list-windows", "-t", session, "-F", "#{window_id}\t#{window_active}"])
            .lines()
            .filter(|line| line.ends_with("\t1"))
            .map(|line| line.split('\t').next().unwrap_or_default().to_owned())
            .collect();
    assert_eq!(active, vec![chief.window.clone()]);

    // --- A PERSON WITH NO PANE ----------------------------------------------
    assert!(
        !effects::show_person(
            &tmux,
            session,
            &effects::PersonClick { person_id: "sleeper", display_name: "Sleeper" },
        )
        .shown,
        "nobody by that name has a live pane, so the glass must not move"
    );
    assert_eq!(
        live_active_window(&socket, session),
        chief.window,
        "and it did not: a click on a sleeper leaves the operator where they were"
    );
}

// TOMBSTONE: `an_emptied_department_hands_the_glass_to_one_that_still_has_somebody`,
// deleted 2026-08-14 with the rule it pinned. Moving the operator off a
// department with nobody live and onto one that had somebody is what made a
// click on a fully-asleep Engineering land on the CEO, and the operator ruled
// it out: "we should never fall back to the CEO department. If everybody's
// sleeping, just show that it's sleeping." The replacement rule — the clicked
// department gets its own window saying so — is pinned by
// `a_department_where_everybody_sleeps_gets_its_own_window_saying_so` in
// `rail/tests.rs`.

/// A SLEEPING DEPARTMENT'S WINDOW KEEPS ITS NOTICE, AND STOPS RE-SELECTING
/// ITSELF ONCE IT HAS ONE.
///
/// # The 409 the operator's log recorded in one session
///
/// **THE CARD IS REDRAWN IN PLACE, AND THE GLASS DOES NOT MOVE.**
///
/// The unit tests above assert which verbs are issued. What only a real server
/// can say is what those verbs DO — that `respawn-pane -k` replaces the process
/// in a pane while its id, its window, its position, its width and the
/// operator's own active window all stay exactly as they were. That is the
/// entire safety argument for putting this effect on the changefeed path, and
/// it is an argument about tmux rather than about this code.
///
/// Skipped where there is no tmux, loud under CI — the same precondition every
/// other live test in this file uses.
#[test]
fn a_real_tmux_repaints_a_department_card_in_place_without_moving_the_glass() {
    let staging = tempfile::tempdir().expect("tempdir");
    let Some(server) = live_server("sidebar-card-refresh") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, LIVE_RAIL_COLUMNS);
    let rail_program = staged_rail_program(staging.path());
    let executive = live_person_window(&socket, session, "chief");
    let overview = crate::placement::overview_window_id("quant");

    let was = r#"{"name":"Quant","path":[],"members":[],"children":[]}"#;
    let now = r#"{"name":"Quant","path":[],"members":[{"name":"Ana"}],"children":[]}"#;

    // --- THE CLICK: the card goes up, carrying what it draws -----------------
    assert!(
        effects::show_department_overview(
            &tmux,
            session,
            &live_overview(&overview, "Quant", &card_argv(was), staging.path(), &rail_program),
        )
        .shown
    );
    let window = live_tagged_window(&socket, session, &overview).expect("the overview's window");
    // ENUMERATED OFF A REAL SERVER, not assumed: this is the read that decides
    // WHICH cards get refreshed, and the whole "every standing card" rule rests
    // on it telling an overview window from the department's people window and
    // from the focus body.
    let standing = effects::standing_overviews(&tmux, session);
    assert_eq!(
        standing.iter().map(|one| one.department_id.as_str()).collect::<Vec<_>>(),
        ["quant"],
        "the overview stands and the executive people window is not one: {standing:?}"
    );
    let standing = standing.into_iter().next().expect("the one standing overview");
    assert_eq!(standing.window, window);
    let panes = live_panes_with_tag(&socket, &window, "@chief_asleep_for");
    assert_eq!(panes.len(), 1, "a rail and one card");
    let card_pane = panes[0].clone();
    assert_eq!(
        tmux_out(
            &socket,
            &["display-message", "-p", "-t", &card_pane, "#{@chief_department_card}"]
        )
        .len(),
        64,
        "the verb that drew it stamped what it drew"
    );

    // --- AN UNCHANGED PASS: not one thing moves ------------------------------
    let before = live_geometry(&socket, &window);
    let pid = tmux_out(&socket, &["display-message", "-p", "-t", &card_pane, "#{pane_pid}"]);
    let active = live_active_window(&socket, session);
    for _ in 0..3 {
        assert!(
            !effects::refresh_department_card(&tmux, session, &standing, Some(&card_argv(was))),
            "the card already draws these facts"
        );
    }
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &card_pane, "#{pane_pid}"]),
        pid,
        "three changefeed wakes did not restart the card's own process"
    );
    assert_eq!(live_geometry(&socket, &window), before, "and laid nothing out");

    // --- THE OPERATOR LOOKS ELSEWHERE, AND THE COMPANY MOVES -----------------
    //
    // A card that is not on the glass is still refreshed, and refreshing it may
    // NOT bring the operator back to it. Navigation is a gesture and this is
    // not one.
    tmux_ok(&socket, &["select-window", "-t", &executive.window]);
    assert!(
        effects::refresh_department_card(&tmux, session, &standing, Some(&card_argv(now))),
        "the department's facts moved, so its card is redrawn"
    );
    assert_eq!(
        live_active_window(&socket, session),
        executive.window,
        "and the operator is left exactly where they were: a repaint never navigates"
    );
    assert_ne!(
        live_active_window(&socket, session),
        active,
        "the fixture really did move them off the card first"
    );
    assert_eq!(
        live_panes_with_tag(&socket, &window, "@chief_asleep_for"),
        vec![card_pane.clone()],
        "THE SAME PANE. A repaint that killed and re-split would hand the card's columns \
         to the rail on the way through, which this module measures elsewhere as a \
         full-width sidebar latched for the rest of the session"
    );
    assert_eq!(live_geometry(&socket, &window), before, "and its geometry is untouched");
    assert_ne!(
        tmux_out(&socket, &["display-message", "-p", "-t", &card_pane, "#{pane_pid}"]),
        pid,
        "but the process inside it is a new one, drawing the new facts"
    );

    // --- AND THE PASS AFTER THE REPAINT IS UNCHANGED AGAIN -------------------
    assert!(
        !effects::refresh_department_card(&tmux, session, &standing, Some(&card_argv(now))),
        "a repaint stamps what it drew, so it settles rather than repeating"
    );
}

/// The reuse arm of [`effects::show_department_overview`] returned early only
/// when the window was active AND still held an `@chief_asleep_for` pane. It
/// assumed that pane, and the notice was a shell that printed and then waited a
/// bounded time — so when it ended the guard fell open, the arm relaid the
/// window, re-selected it and woke the other window's rail on EVERY changefeed
/// wake, while never putting the notice back. The operator was re-selected
/// repeatedly into a window holding nothing but a full-width rail, which is why
/// their first click on a department always seemed to do nothing and the second
/// one worked.
///
/// Both halves are pinned here: the notice is RESTORED when it has gone, and the
/// pass after that touches nothing.
#[test]
fn a_real_tmux_restores_a_lost_sleeping_notice_instead_of_re_selecting_an_empty_window() {
    let staging = tempfile::tempdir().expect("tempdir");
    let Some(server) = live_server("sidebar-sleeping-restore") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, LIVE_RAIL_COLUMNS);
    let rail_program = staged_rail_program(staging.path());
    let _executive = live_person_window(&socket, session, "chief");

    let asleep = effects::Overview {
        card: None,
        organization: "acme",
        department_id: "quant",
        department_name: "Quant",
        asleep: 3,
        rail_program: Some(&rail_program),
        company_dir: staging.path(),
    };

    // --- THE FIRST CLICK: the department gets a window that says so ----------
    assert!(effects::show_department_overview(&tmux, session, &asleep).shown);
    let window =
        live_tagged_window(&socket, session, "quant").expect("its own window, not somebody else's");
    assert_eq!(
        live_panes_with_tag(&socket, &window, "@chief_asleep_for").len(),
        1,
        "and it holds the notice"
    );

    // --- THE NOTICE DIES, as its shell always eventually did -----------------
    let notice = live_panes_with_tag(&socket, &window, "@chief_asleep_for")[0].clone();
    tmux_ok(&socket, &["kill-pane", "-t", &notice]);
    let rail_only = live_panes_with_tag(&socket, &window, "@organization_sidebar");
    assert_eq!(
        rail_only.len(),
        1,
        "a full-width rail, alone — the picture this surface exists to end"
    );

    // --- THE NEXT WAKE PUTS IT BACK ------------------------------------------
    assert!(effects::show_department_overview(&tmux, session, &asleep).shown);
    assert_eq!(
        live_panes_with_tag(&socket, &window, "@chief_asleep_for").len(),
        1,
        "the notice is BACK. Re-selecting the operator into an empty window — which is all \
         this arm used to do — told them nothing and moved the glass for no reason"
    );

    // --- AND THE PASS AFTER THAT TOUCHES NOTHING ------------------------------
    let before = live_geometry(&socket, &window);
    assert!(effects::show_department_overview(&tmux, session, &asleep).shown);
    assert_eq!(
        live_panes_with_tag(&socket, &window, "@chief_asleep_for").len(),
        1,
        "exactly one notice — the restore is not a mint-every-pass"
    );
    assert_eq!(
        live_geometry(&socket, &window),
        before,
        "and nothing was relaid: an effect on the refresh path may only fire on a TRANSITION, \
         and this pass is not one"
    );
}

/// THE WIDTH WAR, as a table. Two processes wrote the same number about once a
/// second, for ever, two columns apart:
///
/// ```text
///   rail  width-recorded 239 | conv  narrowed requested=239 applied=237
///   rail  width-recorded 237 | rail  width-recorded 239 | …
/// ```
///
/// The rail measured itself at the FULL window width in the frame between
/// converge splitting a person in and tmux laying the window out. The pane count
/// was already two, so the `alone` rule did not fire and 239 was written down as
/// the operator's choice. `organization_tmux_layout` cannot honour a rail that
/// leaves nobody any columns, narrows it, and the rail records THAT — so neither
/// number is stable, the plan is never empty, and the window is relaid about
/// once a second (`requested: 2 applied: 2`, round after round).
#[test]
fn a_transient_full_window_resize_does_not_become_a_preference() {
    assert_eq!(super::brain::canonical_columns(31), 31);
    assert_eq!(super::brain::canonical_columns(4), 26);
}

/// **THE RAIL ISSUES NO `join-pane` AT ALL**, and a source sweep is the only way
/// to pin that: each call site was reached by a different gesture, and a new one
/// added would reintroduce the defect silently.
///
/// # What this replaced, and why the rule got stronger
///
/// It used to be `every_join_pane_splits_horizontally_so_the_rail_keeps_its_
/// height`: every join had to carry `-h`, because a bare `join-pane` splits the
/// TARGET vertically and made the rail — a full-height column down the left —
/// that window's top half until the layout put it back. Measured on a live
/// company, on every click, in every rail at once:
///
/// ```text
///   %4  49 -> 24 rows   (width 31, unchanged)
///   … ~200ms …
///   %4  24 -> 49 rows
/// ```
///
/// Then it also had to name its own `-t`, because `join-pane -t <window>` splits
/// that window's ACTIVE pane, which in a parked focus window is the rail: a
/// 26-column sidebar became 13 on every person click.
///
/// Both rules were about making a MOVE cheap. The move is gone — one window per
/// person, so a click selects and never joins — and the strongest available
/// statement is that this file contains no `join-pane` for a rule to apply to.
/// A `join-pane` reappearing here means a pane is being moved between windows
/// again, which means a pane is being resized, which is the whole defect.
#[test]
fn the_rail_moves_no_pane_between_windows() {
    let source = include_str!("effects.rs");
    let joins: Vec<&str> = source
        .match_indices("\"join-pane\"")
        .map(|(at, _)| {
            let rest = &source[at..];
            &rest[..rest.len().min(220)]
        })
        .collect();
    assert!(
        joins.is_empty(),
        "a pane moved between windows is a pane resized, and a Pi whose pane is resized \
         repaints its whole scrollback — which is what the operator recorded on 2026-08-21. \
         Every person is placed alone by `placement::desired_topology`, so a click has \
         nowhere to move anybody to: {joins:?}"
    );
    // AND THE SPLIT THAT REMAINS IS FURNITURE'S. `split-window` still appears —
    // the rail mints its own sidebar and its own card bodies — and every one of
    // those goes through `park_argv`, which is the one place that decides which
    // pane may be split and by how much. None of them touches a person's pane.
    assert!(
        source.contains("fn park_argv("),
        "the one place that sizes and targets a furniture split must still exist"
    );
}

/// A live tmux seam that reads the rail's width BETWEEN commands.
///
/// The halving below is invisible after the fact: the join takes half the rail's
/// columns and the layout that follows gives them back, so a width read at the
/// end of the gesture is 26 whether or not the operator ever saw 13. tmux
/// renders at the END of a command sequence, so the widths between the sequences
/// are exactly the frames that reach the glass — and this records every one of
/// them.
struct WatchRail {
    inner: SocketTmux,
    rail: String,
    seen: std::sync::Mutex<Vec<i64>>,
}

impl Tmux for WatchRail {
    fn run(&self, args: &[&str]) -> String {
        let out = self.inner.run(args);
        // Asked on the RAW socket, never through `self`, so watching cannot
        // become part of what is being watched.
        let width = tmux_out(
            &self.inner.socket,
            &["display-message", "-p", "-t", &self.rail, "#{pane_width}"],
        );
        if let Ok(width) = width.trim().parse::<i64>() {
            self.seen.lock().expect("the watcher's own lock is not poisoned").push(width);
        }
        out
    }
}

/// THE OPERATOR'S SIDEBAR HALVING ON EVERY PERSON CLICK — `{26: 4, 13: 3}`.
///
/// # What was measured
///
/// On the operator's own live company, `sidebar.rail.width-recorded` since the
/// session started reads `{26: 4, 13: 3}` — and 13 is 26 halved, the signature
/// of the 13-column sidebar catastrophe `plausible_rail_width` was built for.
/// Every 13 lands within ~150ms of a `sidebar.person.retargeted` and is undone
/// ~400ms later:
///
/// ```text
///   05:07:59.880  sidebar.person.retargeted  tomas  @2
///   05:07:59.996  sidebar.rail.width-recorded  13
///   05:08:00.425  sidebar.rail.width-recorded  26
/// ```
///
/// # The cause, read straight off their box
///
/// `join-pane -t <window>` does not target a window: tmux resolves a window
/// target to that window's ACTIVE pane and splits it in half. Their focus
/// window answered `%5 pane_active=1 pane_width=26 @organization_sidebar=1` —
/// **the active pane IS the rail** — so every person click halved the sidebar
/// and the layout that followed put it back.
///
/// Worse than a flicker: a width the rail is DRAWN at is a width the rail
/// RECORDS, so this was one `record_width` from becoming the session's
/// remembered sidebar for good.
///
/// # Why this test watches instead of asserting the end state
///
/// The end state was always right. A test that read the width after the gesture
/// passed while the operator was watching the sidebar jump, which is exactly how
/// this survived three rounds of width fixes.
#[test]
fn a_person_arriving_never_takes_the_rails_columns() {
    let staging = tempfile::tempdir().expect("tempdir");
    let Some(server) = live_server("rail-halving") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, LIVE_RAIL_COLUMNS);
    let rail_program = staged_rail_program(staging.path());

    let _quant = live_person_window(&socket, session, "quant-head");
    let analyst = live_person_window(&socket, session, "analyst");
    let parked = effects::ensure_focus_window(
        &tmux,
        session,
        &effects::Parked {
            organization: "acme",
            rail_program: Some(&rail_program),
            company_dir: staging.path(),
        },
    )
    .expect("the focus window is minted");
    // THE OPERATOR'S OWN STATE, ESTABLISHED RATHER THAN HOPED FOR: the focus
    // window's active pane is its RAIL. That is what their box answered, and a
    // test that merely happened to inherit it would stop pinning anything the
    // day the mint order changed.
    let focus_rail = tmux_out(
        &socket,
        &["list-panes", "-t", &parked, "-F", "#{pane_id}\t#{@organization_sidebar}"],
    )
    .lines()
    .find_map(|line| {
        line.split_once('\t').filter(|(_, tag)| !tag.is_empty()).map(|(p, _)| p.to_owned())
    })
    .expect("the focus window has a rail");
    tmux_ok(&socket, &["select-pane", "-t", &focus_rail]);
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", &parked, "#{pane_active}"]).trim(),
        "1",
        "the fixture is only the operator's state if the ACTIVE pane of the focus window is \
         its rail"
    );
    assert_eq!(
        live_geometry(&socket, &parked).get(&focus_rail).map(|(_, width)| *width),
        Some(LIVE_RAIL_COLUMNS),
        "and the rail starts at the width the operator chose"
    );

    // THE CLICK, through a seam that watches a rail between every command.
    //
    // The DESTINATION's rail, which is the one the halving could reach: the
    // click selects the window the analyst is already alone in. The card
    // window's rail is checked afterwards, because a gesture that does not
    // touch that window cannot narrow anything in it.
    let watch =
        WatchRail { inner: tmux, rail: analyst.rail.clone(), seen: std::sync::Mutex::new(vec![]) };
    assert!(
        effects::show_person(
            &watch,
            session,
            &effects::PersonClick { person_id: "analyst", display_name: "Ana Lyst" },
        )
        .shown,
        "the person is shown in the window they are already alone in"
    );

    let seen = watch.seen.lock().expect("not poisoned").clone();
    assert!(!seen.is_empty(), "the watcher saw no command at all, so it is pinning nothing");
    let narrowest = *seen.iter().min().expect("at least one reading");
    assert_eq!(
        narrowest,
        LIVE_RAIL_COLUMNS,
        "the rail is {LIVE_RAIL_COLUMNS} columns at EVERY frame of the gesture. It went to \
         {narrowest} here, and {} is {LIVE_RAIL_COLUMNS} halved — which is what a
         `join-pane -t <window>` did, because tmux resolves a window target to that \
         window's ACTIVE pane and splits it in half. Widths seen: {seen:?}",
        LIVE_RAIL_COLUMNS / 2
    );
    // AND THE CARD WINDOW'S RAIL IS UNTOUCHED, because the click never went
    // near it. This is the assertion that would have failed on every design
    // before one window per person: each of them routed the click THROUGH this
    // window, which is why its rail was the one being halved.
    assert_eq!(
        live_geometry(&socket, &parked).get(&focus_rail).map(|(_, width)| *width),
        Some(LIVE_RAIL_COLUMNS),
        "a person click does not reach the card window at all"
    );
}

/// THE 147-COLUMN SIDEBAR, and the half of the placement rule that was missing.
///
/// # What was measured
///
/// The operator's company, restarted on a binary whose control transport had
/// just been made fast. The actuator now reaches the executive window before the
/// rail is minted into it, puts a sleeping notice there, and 950ms later the CEO
/// comes up and `close_sleeping_notices` sweeps that notice:
///
/// ```text
///   05:52:35.352  sidebar.department.sleeping.restored  executive  %2  @1
///   05:52:35.419  sidebar.rails.minted                             %5  @1
///   05:52:36.300  sidebar.department.awake              executive  %2
///   05:52:36.302  sidebar.rail.width-recorded           147
/// ```
///
/// 147 is 26 plus the notice's 120 plus its divider. The window went in as
/// `{rail, notice, chief}`, tmux hands a dying pane's columns to its PREVIOUS
/// sibling, and every layout for the rest of the session reproduced 147 because
/// the recorded width is what a layout falls back to.
///
/// # The rule
///
/// `Park` had one side and needed two. Splitting in FRONT is right when the
/// SIBLING is the pane that dies — a person leaving the focus window, a notice a
/// loading panel replaces. A sleeping notice is the opposite: IT is the pane
/// that dies, so it goes on the far side and hands its columns back to a pane
/// that is staying. `{rail, chief, notice}`.
#[test]
fn a_sleeping_notice_hands_its_columns_back_to_its_sibling_and_never_to_the_rail() {
    let Some(server) = live_server("sleeping-notice-columns") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, LIVE_RAIL_COLUMNS);

    // THE OPERATOR'S OWN ORDERING, WHICH IS THE WHOLE OF THE BUG. The actuator
    // reached the executive window BEFORE `chief attach` minted a rail into it
    // — 05:52:35.352 against 05:52:35.419 — so the notice was placed among panes
    // that did not yet include a rail, and the rail was split in FRONT of it
    // afterwards. Building the window rail-first passes whichever side the
    // notice takes, because `relay` nests the rail beside a CONTAINER and tmux
    // then hands a dying pane's columns to a sibling inside that container. A
    // fixture that cannot produce the ordering cannot fail on it.
    let window = tmux_out(
        &socket,
        &["new-window", "-d", "-t", session, "-n", "executive", "-P", "-F", "#{window_id}"],
    );
    tmux_ok(&socket, &["set-option", "-w", "-t", &window, "@organization_window_id", "executive"]);
    let person = tmux_out(&socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
    tmux_ok(&socket, &["set-option", "-p", "-t", &person, "@organization_person_id", "chief"]);

    effects::show_department_overview(
        &tmux,
        session,
        &effects::Overview {
            card: None,
            organization: "acme",
            department_id: "executive",
            department_name: "Executive",
            asleep: 1,
            rail_program: None,
            company_dir: std::path::Path::new("/company"),
        },
    );
    let notice = live_panes_with_tag(&socket, &window, "@chief_asleep_for")
        .into_iter()
        .next()
        .expect("the sleeping notice is up");

    // AND ONLY NOW THE RAIL, split in first-cell at the width the operator
    // chose — exactly what `sidebar.rails.minted` does on the attach path.
    let rail = tmux_out(
        &socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            &LIVE_RAIL_COLUMNS.to_string(),
            "-t",
            &person,
            "-P",
            "-F",
            "#{pane_id}",
        ],
    );
    tmux_ok(&socket, &["set-option", "-p", "-t", &rail, "@organization_sidebar", "1"]);
    assert_eq!(
        live_geometry(&socket, &window).get(&rail).map(|(_, w)| *w),
        Some(LIVE_RAIL_COLUMNS),
        "the rail starts at the width the operator chose"
    );

    // AND THEN THE DEPARTMENT WAKES, which is what the notice is swept by.
    let live: BTreeSet<String> = ["executive".to_owned()].into_iter().collect();
    let known: BTreeSet<String> = ["executive".to_owned()].into_iter().collect();
    effects::close_sleeping_notices(&tmux, session, &live, &known);

    let grid = live_geometry(&socket, &window);
    assert!(!grid.contains_key(&notice), "the notice is gone: {grid:?}");
    assert_eq!(
        grid.get(&rail).map(|(_, width)| *width),
        Some(LIVE_RAIL_COLUMNS),
        "AND ITS COLUMNS WENT TO THE PERSON, NOT TO THE RAIL. tmux hands a dying pane's \
         columns to its PREVIOUS SIBLING, so a notice split in FRONT of the person leaves \
         the rail next in line — measured on the operator's box as a 26-column sidebar \
         becoming {} and staying there for the session: {grid:?}",
        LIVE_RAIL_COLUMNS + 120 + 1
    );
    assert!(
        grid.get(&person).map(|(_, width)| *width).unwrap_or_default() > LIVE_RAIL_COLUMNS,
        "and the person got them: {grid:?}"
    );
}

/// THE SIDEBAR FLASHING ACROSS THE WHOLE SCREEN, and the rule that ends it.
///
/// Measured on a live company: clicking a sleeper in a fully-asleep department
/// logged `sidebar.rail.frame-resized 37 -> 239` and back 230ms later. The
/// notice was killed BEFORE the placeholder was split in, which left the rail
/// alone in the window — and tmux hands a lone pane the whole window.
///
/// An intermediate pane count is an intermediate GEOMETRY, and the operator sees
/// every one of them. So the panel is created first, beside the notice, and the
/// notice's columns pass straight to it.
#[test]
fn the_rail_pads_both_edges_truncates_rather_than_clipping_and_draws_the_tree() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let departments = vec![
        DepartmentRow {
            id: "executive".into(),
            name: "Executive".into(),
            depth: 0,
            live: 2,
            total: 2,
        },
        DepartmentRow {
            id: "engineering".into(),
            name: "Engineering".into(),
            depth: 1,
            live: 1,
            total: 2,
        },
        DepartmentRow {
            id: "portfolio-management".into(),
            name: "Portfolio Management".into(),
            depth: 1,
            live: 0,
            total: 0,
        },
    ];
    let mut people = BTreeMap::new();
    people.insert(
        "engineering".to_owned(),
        vec![
            PersonRow {
                id: "rhea".into(),
                name: "Rhea".into(),
                title: "Engineering Lead".into(),
                live: true,
                desired: true,
                idle: true,
                crash: None,
                refused: None,
                manager: false,
            },
            PersonRow {
                id: "tomas".into(),
                name: "Tomas Kowalski-Fitzgerald".into(),
                title: "Platform Engineer".into(),
                live: false,
                desired: true,
                idle: false,
                crash: None,
                refused: None,
                manager: false,
            },
        ],
    );
    let mut view = View::new(departments, people);
    view.select("engineering");
    view.select_person("rhea");

    const WIDTH: u16 = 26;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 16)).expect("a test terminal");
    terminal.draw(|frame| super::render::draw(frame, &view)).expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..16)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect();

    // --- BOTH EDGES ARE RESERVED -------------------------------------------
    for (index, row) in rows.iter().enumerate() {
        assert!(row.starts_with(' '), "row {index} writes in the left gutter: {row:?}");
        assert!(
            row.ends_with(' '),
            "row {index} runs into the right-hand margin — this is the clipped \
             `Portfolio Management (0)` the operator photographed: {row:?}"
        );
    }

    // --- ONE QUIET ROW PRECEDES THE TREE -----------------------------------
    assert!(rows[0].trim().is_empty(), "row zero is blank padding: {:?}", rows[0]);
    assert!(rows[1].contains("Executive"), "normal tree order starts on row one: {rows:?}");
    assert!(rows.iter().all(|row| !row.contains("Company")), "the retired heading is gone");
    assert!(rows.iter().all(|row| !row.contains('\u{2500}')), "the retired rule is gone");

    // --- THE TREE ----------------------------------------------------------
    assert!(rows.iter().any(|row| row.contains(" \u{2212} Engineering")), "expanded: {rows:?}");
    assert!(rows.iter().any(|row| row.contains(" + Portfolio")), "collapsed: {rows:?}");
    assert!(
        rows.iter()
            .all(|row| !row.contains(['\u{251c}', '\u{2514}', '\u{2502}', '\u{25be}', '\u{25b8}'])),
        "the tree has no connectors, triangles, or selection arrows: {rows:?}"
    );
    let rhea = rows.iter().find(|row| row.contains("Rhea")).expect("Rhea identity");
    let title = rows.iter().find(|row| row.contains("Engineering Lead")).expect("exact title");
    let column_of = |row: &str, needle: &str| {
        let byte = row.find(needle).expect("needle is in row");
        row[..byte].chars().count()
    };
    // Engineering is a TOP-LEVEL department, and the root costs no level, so
    // its people are flush: the status sits at column 1 and the name and title
    // at column 3.
    assert_eq!(rhea.chars().nth(1), Some('\u{25ce}'), "the status uses the disclosure column");
    assert_eq!(column_of(rhea, "Rhea"), 3, "the identity nests under Engineering: {rhea:?}");
    assert_eq!(
        column_of(title, "Engineering Lead"),
        3,
        "the role nests under Engineering: {title:?}"
    );
    assert_eq!(
        column_of(rhea, "Rhea"),
        column_of(title, "Engineering Lead"),
        "the exact human title aligns with the person's name"
    );

    // --- TRUNCATION, NOT CLIPPING ------------------------------------------
    let long = rows.iter().find(|row| row.contains("Tomas")).expect("the long name");
    assert!(
        long.contains('\u{2026}'),
        "a name too long for the rail says so rather than being cut mid-word: {long:?}"
    );
    assert_eq!(long.chars().nth(1), Some('\u{25cc}'), "starting keeps its compact dotted icon");
    assert!(!long.contains("starting"), "the icon replaces the right-side state word: {long:?}");

    // --- THE SELECTION IS THE WHOLE ROW ------------------------------------
    let chosen = rows.iter().position(|row| row.contains("Rhea")).expect("the selected person");
    assert!(rows[chosen].starts_with(' '), "selection has no second marker: {:?}", rows[chosen]);
    let name_at = u16::try_from(column_of(&rows[chosen], "Rhea")).expect("a small number");
    assert_eq!(
        buffer[(name_at, u16::try_from(chosen).expect("a small number"))].fg,
        buffer[(0, u16::try_from(chosen).expect("a small number"))].fg,
        "and the row's own TEXT carries the accent, so the selection is findable without \
         knowing which column to look in"
    );
}

/// THE WHOLE VISIBLE GESTURE IS ONE TMUX INVOCATION.
///
/// It used to be "the destination is selected and the pane is moved in one
/// server sequence", so no rendered frame could show the active source after
/// its last content pane left. There is no move left; what survives is the
/// reason the rule existed — tmux renders at the END of a command sequence, so
/// anything split across two invocations is a frame the operator can see.
#[test]
fn the_visible_half_of_a_person_click_is_one_tmux_invocation() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes -s -t org-acme_ -F #{pane_id}", PANES),
        ("#{window_zoomed_flag}", "0"),
        ("#{window_width}\t#{window_height}", "200\t50"),
    ]);
    assert!(effects::show_person(&tmux, "org-acme_", &person_click("analyst", "Ana Lyst")).shown);
    let calls = tmux.calls();
    assert_eq!(writes(&calls).len(), 1, "one write, so one frame: {calls:?}");
    let shown = &writes(&calls)[0];
    assert!(
        shown.find("select-window").expect("a window") < shown.find("select-pane").expect("a pane")
    );
}

#[test]
fn the_unified_tree_draws_one_blank_row_and_no_heading_or_splitter() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let departments = vec![DepartmentRow {
        id: "eng".into(),
        name: "Engineering".into(),
        depth: 0,
        live: 1,
        total: 1,
    }];
    let mut people = BTreeMap::new();
    people.insert(
        "eng".to_owned(),
        vec![PersonRow {
            id: "rhea".into(),
            name: "Rhea".into(),
            title: "Engineering Lead".into(),
            live: true,
            desired: true,
            idle: false,
            crash: None,
            refused: None,
            manager: false,
        }],
    );
    let mut view = View::new(departments, people);
    view.select("eng");

    const WIDTH: u16 = 30;
    const HEIGHT: usize = 14;
    let mut terminal =
        Terminal::new(TestBackend::new(WIDTH, u16::try_from(HEIGHT).expect("small")))
            .expect("a test terminal");
    terminal.draw(|frame| super::render::draw(frame, &view)).expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..u16::try_from(HEIGHT).expect("small"))
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect();

    assert!(rows[0].trim().is_empty(), "one blank row precedes the tree: {rows:?}");
    assert!(rows[1].contains("Engineering"), "the first department starts at row one: {rows:?}");
    assert!(rows.iter().all(|row| !row.contains("Company")), "no company heading: {rows:?}");
    assert!(rows.iter().all(|row| !row.contains('\u{2500}')), "no horizontal rule: {rows:?}");
}

/// A GESTURE LAYS THE RAIL OUT AT THE WIDTH IT ALREADY HAS — except when that
/// width is a split in progress.
///
/// # Why the current width and not the recorded one
///
/// `grid_layout` sized the rail's cell from the width recorded in a session
/// option. Whenever that drifted from the rail's actual width — and converge, a
/// drag and any mid-layout frame all write it — an ordinary department click
/// RESIZED the sidebar as a side effect of arranging the people beside it.
///
/// That resize is the head of the whole corruption chain: tmux applies a pane's
/// grid resize synchronously but its pty up to 250ms later, so the rail then
/// draws a frame measured at one width and interpreted at another. Probed: a
/// mutation leaving the rail's cell byte-identical delivers it ZERO SIGWINCHes.
///
/// # Why there is a band
///
/// Preferring the current width cements it, so a rail caught mid-transit — half
/// the window because a panel was just split off it — would be left there by the
/// very layout meant to arrange the window. At half the glass or wider, a rail
/// is not a width anybody chose; it is a frame of a split, and the recorded
/// width is the better answer.
#[test]
fn a_gesture_restores_the_fixed_width_after_a_border_drag() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes", "%9\t1\t0\n%4\t\t0"),
        ("#{window_width}", "200\t50"),
        // The rail is at 30 because the tmux border was dragged.
        ("#{pane_width}", "30"),
        ("@chief_sidebar_columns", "26"),
    ]);
    effects::lay_equal_grid_for_test(&tmux, "org-acme_", "@1");
    let laid = tmux.calls().iter().find(|c| c.starts_with("select-layout")).cloned();
    assert!(
        laid.as_deref().is_some_and(|call| call.contains("26x50,0,0,9")),
        "layout restores the product width rather than preserving a border drag: {laid:?}"
    );
    assert!(
        laid.as_deref().is_some_and(|call| !call.contains("30x50")),
        "the current tmux pane width is not sidebar product state: {laid:?}"
    );
}

/// THE HALVING THE WINDOW CANNOT SEE, and the drag that must survive it.
///
/// The operator dragged their rail to 37. A split then left it at 18, and 18
/// clears the readable floor and is nowhere near half of a 240-column window —
/// so `plausible_rail_width` passed it, the layout COMPUTED 18, applied it, and
/// the sidebar sat at half width for 5.4 seconds. Verbatim from their box:
///
/// ```text
/// 16:06:15.892  frame-resized %2: 26 -> 37   (the operator's own drag)
/// 16:06:52.831  window.laid columns=18       <- the layout CHOSE 18
/// 16:06:58.869  frame-resized %4: 18 -> 37   (5.4s later, back)
/// ```
///
/// Only the width the rail is KNOWN to have had can answer this, and both
/// directions are pinned here because a fix that refused the halving by
/// refusing everything would take the operator's drag with it.
#[test]
fn every_layout_uses_the_human_width_not_the_panes_current_width() {
    let laid_at = |current: &'static str, recorded: &'static str| -> Option<String> {
        let tmux = RecordingTmux::answering(&[
            ("list-panes", "%9\t1\t0\n%4\t\t0"),
            ("#{window_width}", "240\t50"),
            ("#{pane_width}", current),
            ("@chief_sidebar_columns", recorded),
        ]);
        effects::lay_equal_grid_for_test(&tmux, "org-acme_", "@1");
        tmux.calls().iter().find(|c| c.starts_with("select-layout")).cloned()
    };

    let halved = laid_at("18", "37");
    assert!(
        halved.as_deref().is_some_and(|call| call.contains("37x50,0,0,9")),
        "a split transit cannot replace the human width: {halved:?}"
    );

    let dragged = laid_at("37", "37");
    assert!(
        dragged.as_deref().is_some_and(|call| call.contains("37x50,0,0,9")),
        "the explicit human width remains product state: {dragged:?}"
    );

    let doubled = laid_at("74", "37");
    assert!(
        doubled.as_deref().is_some_and(|call| call.contains("37x50,0,0,9")),
        "a dead neighbour cannot overwrite the human preference: {doubled:?}"
    );
}

#[test]
fn fit_narrowing_never_overwrites_the_human_preference() {
    let tmux = RecordingTmux::answering(&[
        ("list-panes", "%9\t1\t0\n%4\t\t0"),
        ("#{window_width}", "40\t50"),
        ("#{pane_width}", "37"),
        ("@chief_sidebar_columns", "37"),
    ]);
    effects::lay_equal_grid_for_test(&tmux, "org-acme_", "@1");
    assert!(tmux.calls().iter().any(|call| call.starts_with("select-layout")));
    assert!(!tmux
        .calls()
        .iter()
        .any(|call| { call.starts_with("set-option") && call.contains("@chief_sidebar_columns") }));
}

/// BOTH EDGES OF THE BAND, as a table.
///
/// THE RULE: a rail that has not read the company yet says so, and does not
/// state anything about the company it has not read.
///
/// The operator caught this on the very frame added to stop the pane being
/// white: "why does the department list disappear?". It had not disappeared —
/// the rail was drawing an EMPTY roster through the ordinary path, which
/// renders an empty Departments list and the words "Nobody works here". Both
/// are claims about the company, and for the 1.6 seconds the rail spent booting
/// they were false ones.
///
/// "I have not read it" and "I read it and it is empty" are different facts.
/// The renderer must not spell them the same.
#[test]
fn a_rail_that_has_not_read_the_company_claims_nothing_about_it() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const WIDTH: u16 = 30;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 14)).expect("a test terminal");
    terminal.draw(|frame| super::render::draw(frame, &View::unread())).expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let screen: String = (0..14)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !screen.contains("Nobody works here"),
        "a rail that has not asked cannot answer; that line is a statement about the \
         company:\n{screen}"
    );
    assert!(
        screen.contains('…'),
        "and it says the answer is still coming, rather than leaving a blank to be read as \
         an empty company:\n{screen}"
    );
}

/// THE RULE: A RAIL NEVER SITS ON `…` FOR EVER. A read that has been tried and
/// refused is a failure, not a wait, and the glass must say which.
///
/// `…` is a promise that an answer is coming. It is true for the moment between
/// a rail's birth and its first read, and false from the first refusal onward —
/// but the rail had no way to spell the difference, so a company nobody could
/// read looked exactly like one that was about to appear, indefinitely. On the
/// operator's box that is what "the sidebar is stuck on `…` and never fills in"
/// was: the published snapshot was absent or stale, the rail's own reads then
/// failed too, and `refresh` returned without ever marking the view read.
///
/// The honesty rule is unchanged and is asserted here as well: this frame still
/// says nothing about who works at the company, so "Nobody works here" stays
/// reachable only from a company that was actually read.
#[test]
fn a_rail_whose_reads_fail_says_so_instead_of_waiting_for_ever() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const WIDTH: u16 = 30;
    let mut view = View::unread();
    // What `Rail::refresh` does when the snapshot is absent and its own reads
    // are refused: no company arrives, and the failure is recorded.
    view.note_unreadable();

    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 14)).expect("a test terminal");
    terminal.draw(|frame| super::render::draw(frame, &view)).expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let screen: String = (0..14)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        screen.contains("could not read"),
        "a rail that tried and failed must say so; `…` promises an answer that is not \
         coming:\n{screen}"
    );
    assert!(
        !screen.contains('…'),
        "and it must stop promising one — a failure drawn as a wait is the stuck-`…` \
         defect itself:\n{screen}"
    );
    assert!(
        !screen.contains("Nobody works here"),
        "a failed read is still not a reading; the honesty rule holds:\n{screen}"
    );
}

/// THE CONTROL: a rail that recovers stops saying it could not read.
///
/// Without this the rule above is satisfied by a rail that latches the failure
/// for ever, which trades a permanent `…` for a permanent apology.
#[test]
fn a_rail_that_recovers_stops_saying_it_could_not_read() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const WIDTH: u16 = 30;
    let mut view = View::unread();
    view.note_unreadable();
    view.refresh(
        "Acme".to_owned(),
        vec![DepartmentRow {
            id: "executive".to_owned(),
            name: "Executive".to_owned(),
            depth: 0,
            live: 1,
            total: 1,
        }],
        std::collections::BTreeMap::new(),
    );

    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 14)).expect("a test terminal");
    terminal.draw(|frame| super::render::draw(frame, &view)).expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let screen: String = (0..14)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !screen.contains("could not read"),
        "one good read clears the failure; a notice that outlives its cause is its own \
         bug:\n{screen}"
    );
    assert!(screen.contains("Executive"), "and the company it read is on the glass:\n{screen}");
}

/// THE CONTROL: a company that really has been read and really is empty DOES
/// say so.
///
/// Without this the test above passes for a renderer that simply never draws
/// the line, which would lose the placeholder that stops an empty half being
/// read as a broken rail.
#[test]
fn a_rail_that_read_an_empty_company_does_say_nobody_works_here() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const WIDTH: u16 = 30;
    let mut view = View::unread();
    view.refresh("Acme".to_owned(), Vec::new(), std::collections::BTreeMap::new());
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 14)).expect("a test terminal");
    terminal.draw(|frame| super::render::draw(frame, &view)).expect("the rail draws");
    let buffer = terminal.backend().buffer().clone();
    let screen: String = (0..14)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        screen.contains("Nobody works here"),
        "a refresh is what turns 'unknown' into 'known', whatever it found — and an empty \
         answer IS an answer:\n{screen}"
    );
}

#[test]
fn hierarchy_depth_indents_each_department_and_its_people_under_its_parent() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let departments = vec![
        DepartmentRow { id: "root".into(), name: "Root Unit".into(), depth: 0, live: 1, total: 1 },
        DepartmentRow {
            id: "child".into(),
            name: "Child Unit".into(),
            depth: 1,
            live: 1,
            total: 1,
        },
        DepartmentRow { id: "deep".into(), name: "Deep Unit".into(), depth: 2, live: 1, total: 1 },
    ];
    let card = |id: &str, name: &str, title: &str| PersonRow {
        id: id.to_owned(),
        name: name.to_owned(),
        title: title.to_owned(),
        live: true,
        desired: true,
        idle: false,
        crash: None,
        refused: None,
        manager: false,
    };
    let mut people = BTreeMap::new();
    people.insert("root".to_owned(), vec![card("root-person", "RootPerson", "RootTitle")]);
    people.insert("child".to_owned(), vec![card("child-person", "ChildPerson", "ChildTitle")]);
    people.insert("deep".to_owned(), vec![card("deep-person", "DeepPerson", "DeepTitle")]);
    let mut view = View::new(departments, people);
    view.select("child");
    view.select("deep");
    view.scroll(-100);

    const WIDTH: u16 = 32;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 20)).expect("test terminal");
    terminal.draw(|frame| super::render::draw_with_appearance(frame, &view, true)).expect("draw");
    let buffer = terminal.backend().buffer();
    let rows = (0..20)
        .map(|row| (0..WIDTH).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let column_of = |needle: &str| {
        let row = rows.iter().find(|row| row.contains(needle)).expect("named row is drawn");
        let byte = row.find(needle).expect("needle is on named row");
        row[..byte].chars().count()
    };

    // THE ROOT COSTS NO LEVEL. Depth 0 (the root) and depth 1 (a top-level
    // department) share column 3; the step starts at depth 2, the first
    // department that lives INSIDE another department. A person's identity and
    // role sit under their own department, at the same column as the
    // department label, so the whole branch reads as one indented block.
    for (department, column) in [("Root Unit", 3), ("Child Unit", 3), ("Deep Unit", 5)] {
        assert_eq!(column_of(department), column, "the department indents one step per depth");
    }
    for (identity, column) in [("RootPerson", 3), ("ChildPerson", 3), ("DeepPerson", 5)] {
        assert_eq!(column_of(identity), column, "the person aligns under its department");
    }
    for (title, column) in [("RootTitle", 3), ("ChildTitle", 3), ("DeepTitle", 5)] {
        assert_eq!(column_of(title), column, "the role aligns with its identity");
    }
    // The disclosure marker and the person status icon track the same
    // indentation, one cell per level from column one.
    for (label, column) in [("Root Unit", 1), ("Child Unit", 1), ("Deep Unit", 3)] {
        let row = rows.iter().find(|row| row.contains(label)).expect("department row");
        assert!(
            matches!(row.chars().nth(column), Some('+') | Some('\u{2212}')),
            "the disclosure indents with its label: {row:?}"
        );
    }
    for (identity, column) in [("RootPerson", 1), ("ChildPerson", 1), ("DeepPerson", 3)] {
        let row = rows.iter().find(|row| row.contains(identity)).expect("person row");
        assert_eq!(
            row.chars().nth(column),
            Some('\u{25cf}'),
            "the status shares the department's indented disclosure column: {row:?}"
        );
    }
}

/// Is this pane still on the server at all? One `list-panes -a`, because the
/// window that held it may have gone with it.
fn pane_is_live(socket: &str, pane: &str) -> bool {
    tmux_out(socket, &["list-panes", "-a", "-F", "#{pane_id}"]).lines().any(|id| id == pane)
}

/// A NOTICE WHOSE DEPARTMENT WAS REMOVED GOES WITH IT — on a real tmux server.
///
/// # What was measured
///
/// A department was removed from a live company and its
/// `@chief_asleep_for <department>` pane stayed on the glass. `chief topology`
/// did not list its window, because placement is derived from the CURRENT tree
/// and that department was not in it, so no converge pass owned the pane and
/// nothing quarantined it. Only a restart cleared it.
///
/// # Why it survived every sweep
///
/// `close_sleeping_notices` matched AWAKE departments only. A department that
/// does not exist has nobody in it who can come up, so by that test its notice
/// was true forever.
///
/// This drives the real condition rather than the value: a real server, a real
/// notice placed by production's own `show_department_overview`, and a roster
/// that no longer names the department. The three panes it must NOT touch are
/// in the same session and the same sweep — a still-sleeping department, the
/// `__focus__` sentinel, and everything at all when the roster read is empty.
#[test]
fn a_sleeping_notice_dies_with_the_department_it_describes() {
    let Some(server) = live_server("notice-department-removed") else {
        return;
    };
    let socket = server.socket().to_owned();
    let session = "org-acme_";
    tmux_ok(&socket, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]);
    let tmux = SocketTmux { socket: socket.clone() };
    effects::record_columns(&tmux, session, LIVE_RAIL_COLUMNS);

    // Two departments, each with a person pane the rail can survive on, and a
    // sleeping notice placed by production.
    let mut notices = BTreeMap::new();
    for (department, name) in [("research", "Research"), ("engineering", "Engineering")] {
        let window = tmux_out(
            &socket,
            &["new-window", "-d", "-t", session, "-n", name, "-P", "-F", "#{window_id}"],
        );
        tmux_ok(
            &socket,
            &["set-option", "-w", "-t", &window, "@organization_window_id", department],
        );
        // The window holds its rail and its notice, which is what a department
        // with nobody up looks like: the notice is the only CONTENT pane, so
        // the window goes with it rather than leaving the rail full-width.
        let rail = tmux_out(&socket, &["display-message", "-p", "-t", &window, "#{pane_id}"]);
        tmux_ok(&socket, &["set-option", "-p", "-t", &rail, "@organization_sidebar", "1"]);
        effects::show_department_overview(
            &tmux,
            session,
            &effects::Overview {
                card: None,
                organization: "acme",
                department_id: department,
                department_name: name,
                asleep: 2,
                rail_program: None,
                company_dir: std::path::Path::new("/company"),
            },
        );
        let notice = live_panes_with_tag(&socket, &window, "@chief_asleep_for")
            .into_iter()
            .next()
            .expect("the sleeping notice is up");
        notices.insert(department.to_owned(), (window, notice));
    }

    // THE OPERATOR IS LOOKING AT THE DEPARTMENT THAT GETS REMOVED. This is not
    // decoration: it is the branch the first version of this fix failed on,
    // live. `kill_pane_without_stranding_the_rail` protects a WATCHED window by
    // keeping its last content pane, which is right for a department that woke
    // — the next pass replaces it in place — and permanently wrong for one that
    // is gone, because there is no next pass and nothing ever replaces the
    // sentence. Measured on a live company: the rail dropped the department at
    // once and the pane still said `4 people are asleep` for a department that
    // did not exist.
    let (research_window_active, _) = notices["research"].clone();
    tmux_ok(&socket, &["select-window", "-t", &research_window_active]);
    assert_eq!(
        tmux_out(&socket, &["display-message", "-p", "-t", session, "#{window_id}"]),
        research_window_active,
        "the fixture must have the removed department's window ON THE GLASS"
    );

    let asleep: BTreeSet<String> = BTreeSet::new();
    // AN EMPTY ROSTER READ SWEEPS NOTHING. A company always holds at least a
    // root department, so no departments at all is a failed read, not a
    // company with none.
    effects::close_sleeping_notices(&tmux, session, &asleep, &BTreeSet::new());
    let survived_empty_read: Vec<String> = notices
        .iter()
        .filter(|(_, (_, notice))| pane_is_live(&socket, notice))
        .map(|(department, _)| department.clone())
        .collect();

    // AND NOW `research` IS REMOVED FROM THE ROSTER.
    let known: BTreeSet<String> = ["engineering".to_owned()].into_iter().collect();
    effects::close_sleeping_notices(&tmux, session, &asleep, &known);

    let (research_window, research_notice) = notices["research"].clone();
    let (_, engineering_notice) = notices["engineering"].clone();
    let research_left = pane_is_live(&socket, &research_notice);
    let engineering_left = pane_is_live(&socket, &engineering_notice);
    let windows = tmux_out(&socket, &["list-windows", "-t", session, "-F", "#{window_id}"]);

    assert_eq!(
        survived_empty_read.len(),
        2,
        "an empty roster read must sweep nothing; survivors: {survived_empty_read:?}"
    );
    assert!(
        !research_left,
        "the notice outlived the department it describes — nothing else ever sweeps it, because \
         placement derives windows from the CURRENT tree and that department is not in it"
    );
    assert!(
        engineering_left,
        "a department that is merely ASLEEP keeps its notice; that is what the notice is for"
    );
    assert!(
        !windows.lines().any(|window| window == research_window),
        "the notice was that window's last content pane, so the window goes too; windows: \
         {windows}"
    );
}

/// The `__focus__` sentinel is not a department and is never swept as one.
///
/// The permanent focus window parks behind a standing notice carrying
/// `@chief_asleep_for __focus__`. It shares the tag with a department notice on
/// purpose, so everything acting on that tag needs no new case — which means
/// the new "the department is gone" arm would have swept it on every single
/// refresh, and `never_blank` would be handed the rail-only window it exists to
/// prevent.
#[test]
fn the_parked_focus_notice_is_never_swept_as_a_removed_department() {
    let live: BTreeSet<String> = BTreeSet::new();
    let known: BTreeSet<String> = ["engineering".to_owned()].into_iter().collect();
    assert_eq!(effects::notice_stale("__focus__", &live, &known), None);
    assert_eq!(
        effects::notice_stale("research", &live, &known),
        Some(effects::NoticeStale::DepartmentGone)
    );
    let awake: BTreeSet<String> = ["engineering".to_owned()].into_iter().collect();
    assert_eq!(
        effects::notice_stale("engineering", &awake, &known),
        Some(effects::NoticeStale::DepartmentAwake)
    );
    assert_eq!(effects::notice_stale("engineering", &live, &known), None);
}

/// A DEPARTMENT CARD'S PANE CARRIES ITS OWN BORDER TITLE, so tmux never falls
/// back to the default format.
///
/// # The defect, measured on a live company
///
/// `pane-border-status` is turned on GLOBALLY. Every pane that is not given a
/// `pane-border-format` therefore inherits tmux's default,
/// `#{pane_index} "#{pane_title}"` — and `pane_title` for a pane nothing has
/// titled is THE MACHINE'S HOSTNAME.
///
/// The rail is titled and every person pane is titled. The department card was
/// not, so clicking a department — the product's central gesture — drew the
/// operator's hostname above the card, on every box, in every department
/// window. Found while recording the README asset, where it would have
/// published a box name to a public repository.
///
/// This pins the RULE, not the string: the format must not be tmux's default,
/// and it must resolve to the department rather than to anything about the
/// host.
#[test]
fn a_department_card_pane_is_titled_so_the_host_name_never_shows() {
    let format = crate::sidebar::department_border_format();
    assert!(
        format.contains("#{window_name}"),
        "the card's title reads the window chief already named after the department: {format}"
    );
    assert!(
        !format.contains("pane_title"),
        "tmux's default draws `#{{pane_title}}`, which is the hostname for an untitled pane: \
         {format}"
    );
    assert!(
        !format.contains("pane_index"),
        "the default's `#{{pane_index}} \"…\"` shape must not survive here: {format}"
    );
    // Styled like the rail's, because they are one surface rather than two.
    assert!(
        format.contains(crate::sidebar::RAIL_BORDER_FOREGROUND)
            && format.contains(crate::sidebar::RAIL_BORDER_BACKGROUND),
        "the card and the rail share one border style: {format}"
    );
}

/// AND THE PANE ACTUALLY RECEIVES IT. The format existing is not the fix; the
/// card pane being told about it is.
#[test]
fn every_department_card_site_writes_the_border_format() {
    let source = include_str!("effects.rs");
    // The three places a card pane comes into existence or is repainted: the
    // mint, the refresh, and the sleeping-pane restore. A site that stamps
    // `DEPARTMENT_CARD` and does not title the pane is the defect returning.
    let stamps = source.matches("tags::DEPARTMENT_CARD, &fingerprint").count();
    let titles = source.matches("department_border_format()").count();
    assert!(
        titles >= stamps,
        "every site that stamps a pane as a department card must also title it: \
         {stamps} stamp(s), {titles} title(s)"
    );
}

/// TURNING THE BORDER ON GLOBALLY OBLIGES A GLOBAL DEFAULT, because tmux's own
/// default draws the machine's hostname.
///
/// Titling each pane kind fixes the kinds we know about. This pins the
/// fallback for the ones we do not: a pane that is new, stray, or simply
/// early. Measured on a live company after the department cards were titled, a
/// department window's rail still flashed `0 "<hostname>"` for the instant
/// between its mint and the first title pass.
#[test]
fn every_site_that_enables_the_border_also_sets_its_global_default() {
    let source = include_str!("effects.rs");
    let enables = source.matches(r#""pane-border-status", "top""#).count();
    let defaults = source.matches("SAFE_BORDER_DEFAULT").count();
    assert!(enables > 0, "the guard must not pass because the surface vanished");
    assert!(
        defaults >= enables,
        "each of the {enables} site(s) enabling the border must pair a global default with it; \
         found {defaults}. tmux's own default is `#{{pane_index}} \"#{{pane_title}}\"`, whose \
         title for an untitled pane is the hostname."
    );
}

/// And the default itself says nothing about the host.
#[test]
fn the_safe_border_default_reveals_nothing_about_the_machine() {
    let default = crate::sidebar::SAFE_BORDER_DEFAULT;
    assert!(!default.contains("pane_title"), "that is the hostname for an untitled pane");
    assert!(!default.contains("host"), "no host format may appear: {default}");
    assert!(!default.contains("pane_index"), "tmux's default shape must not survive: {default}");
}

/// THE RAIL'S BOTTOM ROW CARRIES THE RUNNING VERSION, RIGHT-ALIGNED.
///
/// The operator asked for this after spending an hour telling two releases
/// apart by hand. The label is only worth having if it is the version of the
/// process actually drawing the rail, so it is the same build-time constant
/// `chief --version` prints — a binary cannot be wrong about which binary it
/// is.
#[test]
fn the_rail_footer_carries_the_running_version_right_aligned() {
    let row = super::render::control_row(24, "<<", "0.5.2");

    assert!(row.starts_with(" <<"), "the control keeps the left edge: {row:?}");
    assert!(row.ends_with("v0.5.2 "), "the version is right-aligned with padding: {row:?}");
    assert_eq!(row.chars().count(), 24, "the row fills its width exactly: {row:?}");
    assert!(
        !row.ends_with("v0.5.2"),
        "and is NOT flush against the edge — the operator asked for a little padding"
    );
}

/// THE TWO NEVER TOUCH. A version that ran into the chevrons would read as one
/// corrupted token rather than two facts.
#[test]
fn the_footer_version_never_collides_with_the_collapse_control() {
    for width in 12..40u16 {
        let row = super::render::control_row(width, "<<", "0.5.2");
        if let Some(rest) = row.strip_prefix(" <<") {
            if rest.contains('v') {
                assert!(
                    rest.starts_with(' '),
                    "width {width}: at least one space must separate them: {row:?}"
                );
            }
        }
        assert!(row.chars().count() <= width as usize, "width {width}: never overflows: {row:?}");
    }
}

/// A RAIL TOO NARROW FOR BOTH KEEPS THE CONTROL AND DROPS THE VERSION.
///
/// Truncating would be worse than dropping: `v0.5` is not a shorter way of
/// writing `v0.5.2`, it is a different release, and a label that can lie about
/// which version is running defeats the reason this exists.
#[test]
fn a_narrow_rail_drops_the_version_rather_than_truncating_it() {
    // A collapsed rail is four columns wide.
    let collapsed = super::render::control_row(4, ">>", "0.5.2");
    assert_eq!(collapsed, " >>", "the control survives at the smallest width");
    assert!(!collapsed.contains('v'), "and no partial version is drawn: {collapsed:?}");

    // The exact boundary: " <<" + "v0.5.2 " is 10 columns, plus one space is 11.
    assert!(!super::render::control_row(10, "<<", "0.5.2").contains('v'));
    assert!(super::render::control_row(11, "<<", "0.5.2").ends_with("v0.5.2 "));
}
