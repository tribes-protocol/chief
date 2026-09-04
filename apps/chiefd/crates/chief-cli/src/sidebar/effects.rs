//! What the rail asks tmux, and what it asks tmux to do.
//!
//! Split out of [`super::brain`] so every one of them can be driven by a
//! recording [`Tmux`] in a test. tmux placement is a product invariant and
//! `chief/CLAUDE.md` requires simulated tmux coverage for it; a sequence that
//! only exists inside an async event loop cannot be asserted as a sequence.
//!
//! Each function here reads or writes the OPERATOR'S OWN TERMINAL and nothing
//! else. None of them can change a company.
//!
//! # Tracing the glass — every event this surface emits
//!
//! All of it lands in `~/.chief/log/chief.jsonl` (`chiefd_log::install`), at
//! `info` — which is the production filter, so these are visible on an
//! operator's own box without a rebuild or a flag. One `event` field per line,
//! so a whole gesture is one `grep`.
//!
//! Everything below is emitted by the ONE process that performs gestures
//! ([`super::brain`]). The exception is `sidebar.frame.painted`, which is
//! written by the thin CLIENT — see below.
//!
//! # Every line of one gesture carries that gesture's id
//!
//! `detail.gesture_id` is on every line below, and on the `sidebar.frame.painted`
//! each thin client writes for the same click. It is minted when the brain
//! decodes the mouse event ([`crate::sidebar::gesture`]), it is the click's own
//! wall clock in microseconds, and it is what makes a funnel a subtraction
//! instead of a guess. Before it existed, `session` was one constant per box
//! (3,664 of 3,715 lines) and the rail's pid was replaced mid-episode, so every
//! client-side number this product has ever quoted was a nearest-next-in-time
//! heuristic.
//!
//! It crosses the process boundary on the FRAME
//! ([`crate::sidebar::wire::ToClient`]). It used to ride the third field of the
//! `SELECTION` tmux option, which Stage 3 deletes with the rest of the
//! cross-process bus.
//!
//! ```text
//! jq -c 'select(.detail.gesture_id == 1755300000000001)' ~/.chief/log/chief.jsonl
//! ```
//!
//! # Which of these means the operator can SEE something
//!
//! Exactly one: `sidebar.frame.painted`, and only for the rail's own cells. It
//! is emitted by the THIN CLIENT, after it has written and flushed the frame to
//! its own pane's pty — which is the last instant any process can honestly
//! claim, and the reason the brain does not write it: the brain composed those
//! bytes but has no way to know they reached a terminal.
//!
//! There is ONE PER RAIL. A session with two windows answers a click with two
//! of these, one per attached client, each naming the same gesture.
//!
//! **`sidebar.window.laid` is NOT a frame** and must never be read as one. It
//! is the layout COMMAND, issued at click time — measured at a median 2ms
//! BEFORE the `sidebar.wake.requested` of the very gesture it was quoted as
//! completing, which is how a 5,636ms cold click came to be reported as 1-37ms.
//! The honest end of a cold click is `sidebar.wake.zoomed`; the honest measure
//! of the whole glass is `chief bench click` ([`crate::bench`]), which reads
//! tmux's grid rather than this process's intentions.
//!
//! | event | when |
//! | --- | --- |
//! | `sidebar.gesture` | a click began (`enter`) and its handler returned (`exit`, with `durationMs`) |
//! | `sidebar.click` | a click arrived, with the row it resolved to |
//! | `sidebar.frame.painted` | **the frame answering a gesture is on the glass**, with `elapsed_us` from the click |
//! | `sidebar.department.selected` | a department is on the glass |
//! | `sidebar.department.sleeping` | a department with nobody up got its own notice window |
//! | `sidebar.department.awake` | that notice was killed because somebody came up |
//! | `sidebar.department.unmatched` | **error** — a click named a department the rail is not drawing |
//! | `sidebar.department.unwindowed` | that department has no window with anybody in it |
//! | `sidebar.department.unminted` | **error** — tmux would not mint the notice window |
//! | `sidebar.focus.minted` | the session's ONE focus window was created — once, ever |
//! | `sidebar.focus.parked` | it held nothing but its rail, so its standing notice went back |
//! | `sidebar.focus.missing` | **error** — a gesture wanted it and this session has none |
//! | `sidebar.wake.requested` / `.zoomed` / `.refused` | the sleeper gesture, start to finish |
//! | `sidebar.loading.shown` / `.closed` | the loading panel's whole life |
//! | `sidebar.person.moved` / `.solo` / `.already` / `.retargeted` | which arm a person click took |
//! | `sidebar.person.unmoved` / `.unplaced` / `.homeless` / `.unhomed` | a person gesture that could not complete |
//! | `sidebar.window.laid` | a layout was applied, with the width the rail got |
//! | `layout.rail.narrowed` | the fit forced a narrower rail than the human preference |
//! | `sidebar.rail.width-ignored` | a frame too narrow to believe; the remembered width stands |
//! | `sidebar.selection.stale` | the person on the glass is gone; where the glass went |
//! | `sidebar.client.attached` / `.detached` | a thin rail joined or left this session's brain |
//! | `sidebar.brain.listening` | the session socket is open and serving rails |
//!
//! A sidebar that changed size on its own is `sidebar.window.laid` plus
//! `layout.rail.narrowed`; a click that went somewhere unexpected is
//! `sidebar.click` followed by whichever `sidebar.department.*` line it reached.

use std::collections::{BTreeMap, BTreeSet};

use super::Tmux;
use crate::actuate::trust::{sidebar_options, tags, viewport_options};
use crate::placement::FOCUS_WINDOW_ID;

const WAKING_RECOVERY_READY: &str = "@chief_waking_recovery_ready_v1";

/// What the active managed window says the operator is looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWindowSelection {
    /// The window's live logical tag.
    pub window: String,
    /// The live person in that window, when it has one.
    pub person: Option<String>,
}

/// Read the active window and its live person in one tmux snapshot.
///
/// This is used only when a new shared sidebar brain reads the company for the
/// first time. A retained tmux session can already be on a non-root department
/// or on a focused person, and the first rail frame must agree with that glass.
pub fn active_window_selection(tmux: &dyn Tmux, session: &str) -> Option<ActiveWindowSelection> {
    let rows = tmux.run(&[
        "list-panes",
        "-t",
        session,
        "-F",
        &format!("#{{{}}}\t#{{{}}}\t#{{pane_dead}}", tags::WINDOW, tags::PERSON),
    ]);
    let mut window = None;
    let mut person = None;
    for row in rows.lines() {
        let mut fields = row.split('\t');
        let logical = fields.next()?.trim();
        let pane_person = fields.next()?.trim();
        let dead = fields.next()?.trim();
        if logical.is_empty() || dead == "1" {
            continue;
        }
        window.get_or_insert_with(|| logical.to_owned());
        if !pane_person.is_empty() {
            person.get_or_insert_with(|| pane_person.to_owned());
        }
    }
    window.map(|window| ActiveWindowSelection { window, person })
}

/// Who is up, according to tmux.
///
/// The `@organization_person_id` tag on a live pane of this session, and
/// nothing else. This is the product's only live record of placement — the
/// column that used to hold it (`person_activity.last_pane_department_id`) was
/// deleted by #751-P9 with "nothing durable replaces this" — so reading the
/// tags is reading the authority rather than a cache of it.
pub fn live_person_ids(tmux: &dyn Tmux, session: &str) -> BTreeSet<String> {
    tmux.run(&[
        "list-panes",
        "-s",
        "-t",
        session,
        "-F",
        &format!("#{{{}}}\t#{{pane_dead}}", tags::PERSON),
    ])
    .lines()
    .filter_map(|line| line.split_once('\t'))
    .filter(|(person, dead)| !person.trim().is_empty() && dead.trim() != "1")
    .map(|(person, _)| person.trim().to_owned())
    .collect()
}

/// The pane and window a person is in RIGHT NOW, or `None`.
///
/// Resolved at CLICK time, never cached. A cached pane id is a second source of
/// truth for placement and is stale the moment that person is moved between
/// windows — the same defect `placement.rs` refuses `last_pane_department_id`
/// for.
///
/// A dead pane is not a placement. A person who died between the draw and the
/// click therefore resolves to `None`, which is what makes the click a no-op
/// rather than a zoom of whoever inherited the pane id.
pub fn pane_of(tmux: &dyn Tmux, session: &str, person_id: &str) -> Option<(String, String)> {
    tmux.run(&[
        "list-panes",
        "-s",
        "-t",
        session,
        "-F",
        &format!("#{{pane_id}}\t#{{window_id}}\t#{{{}}}\t#{{pane_dead}}", tags::PERSON),
    ])
    .lines()
    .filter_map(|line| {
        let mut parts = line.split('\t');
        Some((parts.next()?, parts.next()?, parts.next()?, parts.next()?))
    })
    .find(|(_, _, person, dead)| person.trim() == person_id && dead.trim() != "1")
    .map(|(pane, window, _, _)| (pane.trim().to_owned(), window.trim().to_owned()))
}

/// The logical DEPARTMENT this rail's own window belongs to, or `None`.
///
/// # Why a rail has to know this
///
/// **There is one rail PANE PER WINDOW** — `interpret::ensure_rail_in_window`
/// splits a fresh `chief sidebar <org>` into every window it mints — so a
/// company with three departments is running three rail PROCESSES, each with
/// its own [`super::View`] and its own selection.
///
/// That is what the operator reported as "when I click on a person in the
/// engineering department, it just flicks back to the executive department".
/// Nothing reset anything: [`super::View::refresh`] keeps a selection whose
/// department still exists, and the log they sent shows both clicks resolving
/// correctly. What happened is that [`show_person`] ran `select-window` onto
/// engineering's window, and the rail drawn THERE is a different process which
/// had never been clicked — so it was still showing its own default, the first
/// department, which is `executive`.
///
/// A rail's window already carries the answer: `@organization_window_id` is the
/// logical DEPARTMENT id ([`crate::placement::Window::logical_id`]), tagged by
/// the same converge pass that minted the window. A rail that opens on its own
/// department is a rail that agrees with the panes beside it, and the flick
/// disappears without any cross-process state.
///
/// `None` for a window with no tag — a rail somewhere this company did not
/// mint. The caller falls back to the first department, which is the behaviour
/// this replaces and is still the only honest answer when nothing says
/// otherwise.
pub fn window_department_id(tmux: &dyn Tmux, pane_id: &str) -> Option<String> {
    let tag = tmux.run(&[
        "display-message",
        "-p",
        "-t",
        pane_id,
        "-F",
        &format!("#{{{}}}", tags::WINDOW),
    ]);
    let tag = tag.trim();
    (!tag.is_empty()).then(|| tag.to_owned())
}

/// The tmux window a DEPARTMENT is shown in, resolved BY TAG.
///
/// `@organization_window_id` is the logical department id, written by the same
/// converge pass that minted the window ([`crate::placement::Window`]). It is
/// the only honest way to find the window: a window's NAME is a sanitized
/// display name ([`crate::placement::safe_window_name`]) and never the id, which
/// is why `select-window -t <session>:<department_id>` answered `can't find
/// window: executive` on every click.
///
/// `None` when this company has no window for that department — an empty
/// department mints none at all (`placement.rs`'s empty-department rule), and a
/// department whose people all stopped loses its window the same way.
pub fn department_window(tmux: &dyn Tmux, session: &str, department_id: &str) -> Option<String> {
    department_windows(tmux, session, department_id).into_iter().next()
}

/// Every tmux window carrying one logical window id.
///
/// Most callers require one answer and use [`department_window`]. The focus
/// owner must see the complete set: taking the first duplicate hides the exact
/// topology that makes the actuator fail closed.
fn department_windows(tmux: &dyn Tmux, session: &str, department_id: &str) -> Vec<String> {
    tmux.run(&[
        "list-windows",
        "-t",
        session,
        "-F",
        &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW),
    ])
    .lines()
    .filter_map(|line| line.split_once('\t'))
    .filter(|(_, logical)| logical.trim() == department_id)
    .map(|(window, _)| window.trim().to_owned())
    .filter(|window| window.starts_with('@'))
    .collect()
}

/// A run of tmux WRITES issued as one invocation.
///
/// # Why this exists, and why it is not merely a speed-up
///
/// Operator ruling, 2026-08-14: "make every tmux operation as atomic as
/// possible. If you can do everything once, just do that, because there's
/// flickering and that's the only way you get rid of it and the latency."
///
/// Both halves of that are right and they are the same fact. A tmux command is a
/// PROCESS — fork, exec, socket round trip, exit — at roughly 25ms even with the
/// server on the same machine, so a five-command gesture costs an eighth of a
/// second before tmux has done any work. And tmux renders at the END of a
/// command SEQUENCE, so every separate invocation is a separate frame: killing a
/// pane and then laying the window out shows the operator the geometry BETWEEN
/// those two, which is the rail flashing to full width and back that they
/// reported three times.
///
/// So the rule is: do every READ first, compute, then issue every WRITE in one
/// batch. A read cannot join the batch — its answer is needed to build what
/// follows — which is exactly why the reads are hoisted rather than interleaved.
#[derive(Default)]
struct Batch(Vec<String>);

impl Batch {
    fn new() -> Self {
        Self::default()
    }

    /// Append one command. The `;` separator is tmux's own, and it is inserted
    /// BETWEEN commands rather than after each, so a batch never ends in a
    /// dangling separator that tmux would read as an empty command.
    fn push(&mut self, argv: &[&str]) {
        if argv.is_empty() {
            return;
        }
        if !self.0.is_empty() {
            self.0.push(";".to_owned());
        }
        self.0.extend(argv.iter().map(|arg| (*arg).to_owned()));
    }

    fn push_owned(&mut self, argv: &[String]) {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.push(&argv);
    }

    /// Issue the whole run. Nothing to say is nothing sent — an empty batch must
    /// not become a bare `tmux` invocation.
    fn run(self, tmux: &dyn Tmux) {
        let _ = self.run_output(tmux);
    }

    /// Issue the whole run and keep tmux's output for a mint that reports an id.
    fn run_output(self, tmux: &dyn Tmux) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let argv: Vec<&str> = self.0.iter().map(String::as_str).collect();
        tmux.run(&argv)
    }

    fn run_topology(self, tmux: &dyn Tmux, session: &str) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let Some(generation) = invalidate_viewport_topology(tmux, session) else {
            return String::new();
        };
        let output = self.run_output(tmux);
        refresh_viewport_topology(tmux, session, &generation);
        output
    }

    /// Encode this argv batch as one tmux command string for `if-shell`.
    fn command_string(&self) -> String {
        self.0
            .iter()
            .map(|word| {
                if word == ";" {
                    ";".to_owned()
                } else {
                    format!("'{}'", word.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn invalidate_viewport_topology(tmux: &dyn Tmux, session: &str) -> Option<String> {
    let generation = tmux
        .run(&[
            "set-option",
            "-goq",
            viewport_options::TOPOLOGY_GENERATION,
            "0",
            ";",
            "set-option",
            "-gF",
            viewport_options::TOPOLOGY_GENERATION,
            &format!("#{{e|+:#{{{}}},1}}", viewport_options::TOPOLOGY_GENERATION),
            ";",
            "set-option",
            "-F",
            "-t",
            session,
            viewport_options::TOPOLOGY_EPOCH,
            &format!("#{{{}}}", viewport_options::TOPOLOGY_GENERATION),
            ";",
            "display-message",
            "-p",
            "-t",
            session,
            &format!("#{{{}}}", viewport_options::TOPOLOGY_EPOCH),
        ])
        .trim()
        .to_owned();
    generation.parse::<u64>().is_ok().then_some(generation)
}

fn refresh_viewport_topology(tmux: &dyn Tmux, session: &str, generation: &str) {
    if generation.parse::<u64>().is_err() {
        return;
    }
    tmux.run(&[
        "run-shell",
        "-b",
        "-t",
        session,
        &format!("#{{{}}} {}", viewport_options::REFRESH_COMMAND, generation),
    ]);
}

fn canonical_geometry(tmux: &dyn Tmux, session: &str) -> Option<crate::window_geometry::Geometry> {
    crate::window_geometry::capture(session, |argv| {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        Some(tmux.run(&argv))
    })
}

fn current_geometry(tmux: &dyn Tmux, window: &str) -> Option<crate::window_geometry::Geometry> {
    let (columns, rows) = window_size(tmux, window)?;
    Some(crate::window_geometry::Geometry {
        columns: columns.try_into().ok()?,
        rows: rows.try_into().ok()?,
    })
}

fn normalize_into(
    batch: &mut Batch,
    tmux: &dyn Tmux,
    window: &str,
    canonical: Option<crate::window_geometry::Geometry>,
) {
    let Some(canonical) = canonical else { return };
    if let Some(argv) = crate::window_geometry::normalization_argv(
        window,
        current_geometry(tmux, window),
        canonical,
    ) {
        batch.push_owned(&argv);
    }
}

fn normalize_now(
    tmux: &dyn Tmux,
    window: &str,
    canonical: Option<crate::window_geometry::Geometry>,
) {
    let mut batch = Batch::new();
    normalize_into(&mut batch, tmux, window, canonical);
    batch.run(tmux);
}

/// The equal-grid layout for `window`, or `None` when it cannot be computed.
///
/// The READ half of [`lay_equal_grid`], split out so a caller that is about to
/// issue other writes can fold the layout into the same batch instead of
/// spending an invocation — and, more importantly, instead of letting the
/// operator see the window between the two.
fn grid_layout(
    tmux: &dyn Tmux,
    session: &str,
    window: &str,
    panes: &[(String, bool)],
) -> Option<String> {
    grid_layout_at(tmux, session, window, panes, window_size(tmux, window))
}

/// The equal-grid layout for a pane order that will exist later in the same
/// tmux command sequence.
///
/// A focus return must compute its final layouts before it writes anything.
/// The panes are therefore a projection of the post-return order, and `size`
/// can be the canonical size that an earlier command in the same sequence will
/// apply.
fn grid_layout_at(
    tmux: &dyn Tmux,
    session: &str,
    window: &str,
    panes: &[(String, bool)],
    size: Option<(i64, i64)>,
) -> Option<String> {
    let rail = panes.iter().find(|(_, is_rail)| *is_rail).map(|(id, _)| id.clone());
    let people: Vec<&str> =
        panes.iter().filter(|(_, is_rail)| !*is_rail).map(|(id, _)| id.as_str()).collect();
    let (Some(rail_pane), Some((width, height))) = (rail, size) else {
        // No rail in this window, or tmux would not say how big it is. A layout
        // computed from a guessed size would move every pane in the window to
        // the wrong place — the same refusal `show_person` makes.
        return None;
    };
    if people.is_empty() {
        // A window whose people have all died, pending respawn. There is
        // nothing to grid and converge is already bringing them back.
        return None;
    }
    // THE RAIL'S CURRENT WIDTH, NOT THE RECORDED ONE — so a gesture's layout is
    // a NO-OP for the rail's cell.
    //
    // This read `rail_columns`, the width recorded in a session option. Whenever
    // that drifted from the rail's actual width — and it drifts constantly,
    // because converge, a drag and a mid-layout frame all write it — an ordinary
    // department click RESIZED the sidebar, purely as a side effect of arranging
    // the people beside it.
    //
    // That resize is the whole defect chain. tmux applies a pane's grid resize
    // synchronously but its pty up to 250ms later, so the rail then draws a
    // frame measured at one width and interpreted at another, wrecking the grid;
    // and because the transit usually ends back where it started, ratatui never
    // observes a change and never repairs it. Probed: a mutation that leaves the
    // rail's cell BYTE-IDENTICAL delivers the rail zero SIGWINCHes and touches
    // its grid not at all — through splits, kills and an absolute
    // `select-layout` alike. So the cheapest possible fix for the corruption is
    // to stop moving the cell.
    //
    // The RECORDED width keeps its job: it is what converge enforces and what an
    // operator's drag updates. The two agree in steady state because recording
    // follows the drawn width. They disagree only for one converge cadence after
    // a drag, and converge repairs that — which is the right direction for the
    // disagreement, because converge is allowed to resize the rail and a click
    // is not.
    //
    // A BAND, AND BOTH OF ITS EDGES. Preferring the current width CEMENTS it, so
    // a rail caught mid-transit would be left wherever the transit had it — and
    // a transit can be wrong in either direction. See `plausible_rail_width`.
    //
    // AND THE RECORDED WIDTH IS THE SECOND EDGE, because the window is too far
    // away to see a HALVING.
    //
    // `plausible_rail_width` asks only "is this plausible for a sidebar in a
    // window this size", and a halved rail sails through it: the operator
    // dragged theirs to 37, a split left it at 18, and 18 clears the readable
    // floor and is nowhere near half of 240 — so the layout computed 18,
    // applied it, and the sidebar sat at half width for 5.4 seconds. Verbatim:
    //
    //   16:06:15.892  frame-resized %2: 26 -> 37   (the operator's own drag)
    //   16:06:52.831  window.laid columns=18       <- the layout CHOSE 18
    //   16:06:58.869  frame-resized %4: 18 -> 37   (5.4s later, back)
    //
    // The window cannot answer this. Only the width the rail is KNOWN to have
    // had can, and it is now trustworthy: since transits stopped being painted,
    // `record_width` runs only from a frame that actually reached the glass, so
    // the recorded width is a width the operator saw. See `agrees_with_recorded`.
    // Product geometry has only two answers: 26 columns while open and four
    // after the rail's own collapse control. A pane's current width also
    // reports border drags and split transits, so it is never a layout input.
    let columns = rail_columns(tmux, session);
    let rail = crate::layout::Rail { pane_id: &rail_pane, columns };
    match crate::layout::organization_tmux_layout(width, height, Some(rail), &people) {
        Ok(layout) => {
            // EVERY LAYOUT SAYS WHAT WIDTH IT GAVE THE SIDEBAR. The operator's
            // ask, after two rounds of the rail changing size on its own: "add
            // logging so we can trace future bugs."
            tracing::info!(
                event = "sidebar.window.laid",
                session,
                window,
                columns,
                people = people.len(),
                width,
                height,
                "laid a window as an equal grid beside the rail"
            );
            Some(layout)
        }
        Err(error) => {
            tracing::warn!(
                event = "sidebar.department.unlaid",
                session,
                window,
                diagnostic = %error,
                "the window cannot hold an equal grid; it keeps the layout it had"
            );
            None
        }
    }
}

/// Is `current` a width the OPERATOR could have chosen, in a window `window`
/// columns wide?
///
/// Only such a width may be reproduced by a gesture's layout, because
/// reproducing a width CEMENTS it — and a rail caught mid-transit is wrong in
/// both directions:
///
/// * **Too wide.** A panel just split off the rail leaves it at half the glass;
///   a dead neighbour leaves it at all of it. A sidebar is not half the screen,
///   so at or beyond half the window this is a frame of a split in progress.
///   Caught by a live-tmux test at 113 columns of 200 where 26 was right.
///
/// * **Too narrow.** A frame drawn mid-layout can measure a handful of columns —
///   the product already has a name for this and refuses to RECORD it
///   ([`super::brain::RAIL_MIN_READABLE_COLUMNS`]: below it the rail cannot draw
///   its own headings, so it is an artifact rather than a decision). Reproducing
///   it would do worse than recording it: it would make the artifact permanent,
///   because every later gesture would reproduce it again. Caught in CI at SIX
///   columns.
///
/// Outside the band the RECORDED width wins, which is the whole point of
/// recording one: it is the width converge enforces and a drag updates, and it
/// is how a rail that has been knocked out of shape gets back.
/// Lay one window out as the EQUAL GRID: every live person beside the rail,
/// nobody favoured.
///
/// The department view's whole picture. It is computed from the window's LIVE
/// size and its live panes at click time, never from a cached geometry — a
/// `select-layout` with an absolute string is a window resize as much as an
/// arrangement (tmux `layout-custom.c` `layout_parse` calls `window_resize` to
/// the layout's own size), so a stale size would move every pane in the window.
///
/// `panes` is the caller's own [`window_panes`] read, taken at click time — the
/// caller already needed it to decide whether this window may be shown at all,
/// and a second listing could disagree with the first.
fn lay_equal_grid(tmux: &dyn Tmux, session: &str, window: &str, panes: &[(String, bool)]) {
    if let Some(layout) = grid_layout(tmux, session, window, panes) {
        // NO STAMP. This used to write `@chief_sidebar_gesture` first, so the
        // rails in OTHER windows — separate processes that had performed no
        // gesture — would decline to paint the resize this layout was about to
        // inflict on them. There is one process now and it knows its own
        // gestures, so the session option and the read that answered it are
        // both deleted; the rule is `brain::Brain::gestured_at`.
        tmux.run(&["select-layout", "-t", window, &layout]);
    }
}

// TOMBSTONE: `show_department` — the verb that moved every live person in a
// department back onto the glass, tiled beside the rail.
//
// It served the ruling "click a person to move him into a window of his own,
// click the department to move him back", and the RETURN half is what cost the
// operator every switch after it. Six people in a 129x36 window is 42x17 each,
// so every agent in a department RENDERED at 42 columns for as long as it sat
// there, and the moment one was clicked their pane was moved into the
// full-width focus body and repainted at 129: *"it always starts half screen
// and then resizes full screen so it's very jarring"* (2026-08-21). A pane has
// exactly one size. Nothing at the click could have fixed that, because the
// wrong size was already in the scrollback.
//
// A department now shows a CARD about itself — `show_department_overview` —
// which reads the roster and touches no agent, so no pane in this product is
// ever laid at grid-cell width. Do not reintroduce a tiled people view: the
// grid is the defect, not the way it was entered.

/// What a view gesture did.
///
/// Two facts, and they are independent. `shown` answers the operator's question
/// — is the thing I clicked on the glass. `moved_geometry` answers the BRAIN's —
/// did any pane change size because of this, which is what decides whether a
/// settle pass is armed. Stage 4's whole claim is that navigation makes the
/// first true and the second false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shown {
    /// Whether the operator was moved to a window showing what they clicked.
    pub shown: bool,
    /// Whether any pane's geometry changed: a pane joined or left a window, or a
    /// window was re-laid because its membership changed.
    pub moved_geometry: bool,
}

impl Shown {
    /// Nothing was shown and nothing moved.
    #[must_use]
    pub const fn nothing() -> Self {
        Self { shown: false, moved_geometry: false }
    }

    /// The operator was shown it, and no geometry changed on the way.
    #[must_use]
    pub const fn navigated() -> Self {
        Self { shown: true, moved_geometry: false }
    }

    /// The operator was shown it, and panes moved to make that true.
    #[must_use]
    pub const fn moved() -> Self {
        Self { shown: true, moved_geometry: true }
    }
}

/// What the operator is told when the department they clicked has no window.
///
/// It names the reason, not the mechanism. "No window" is a tmux fact the
/// operator never asked about; "nobody is up in Quant" is the company fact that
/// produced it and the one they can act on.
#[must_use]
pub fn no_window_notice(department_name: &str) -> String {
    format!("nobody is up in {department_name}")
}

/// Say something to the operator, over the pane they are looking at.
///
/// `display-message` renders on top of whatever is on the glass, which is the
/// only surface left now the status bar is deleted — the same mechanism the
/// old zoom hint used, and the reason it existed.
///
/// It is here because SILENCE IS THE WORST ANSWER. A click on a sleeping person
/// did nothing at all: no movement, no message, no visible reason. The operator
/// reported it as the rail being broken, which is exactly the right conclusion
/// to draw from a control that does not respond.
pub fn announce(tmux: &dyn Tmux, pane_id: &str, message: &str) {
    tmux.run(&["display-message", "-t", pane_id, message]);
}

/// What the operator is told when they click somebody who is not up.
///
/// Names the person, the state they were in, and what is now happening to
/// them. "Nothing happened", "that person is parked" and "that person is being
/// woken" are three different facts, and only the last one answers the click.
///
/// The wake is not instant and this sentence must not pretend it is: the
/// daemon commits the decision, its converge pass launches the pane, and the
/// rail redraws when the changefeed says so. `waking…` is the honest tense for
/// a decision that has been made and not yet finished.
#[must_use]
pub fn asleep_notice(name: &str, state: &str) -> String {
    format!("{name} is {state} — waking…")
}

/// What the operator is told when they click somebody chiefd's LAUNCH GATE has
/// declined.
///
/// The gate's own sentence, verbatim, for the same reason
/// [`wake_refused_notice`] carries the daemon's: chiefd is the only process
/// that can see the disk the refusal is about, and it names the two files and
/// the home an operator has to go and fix. It deliberately does not say
/// "waking" — nothing is being woken, and that promise is the defect this
/// notice exists to stop repeating.
#[must_use]
pub fn launch_refused_notice(name: &str, reason: &str) -> String {
    format!("{name} cannot start: {reason}")
}

/// What the operator is told when the wake was REFUSED.
///
/// Carries the daemon's own sentence rather than a rewrite of it. A refusal
/// says something true about the company — that person is benched, their
/// department is paused, you do not manage them — and summarizing it into
/// "could not wake" would throw away the only part the operator can act on.
#[must_use]
pub fn wake_refused_notice(name: &str, reason: &str) -> String {
    format!("{name} was not woken: {reason}")
}

/// The shell that draws one line in the middle of a pane and then waits.
///
/// `sh`, not this binary: a placeholder must not be a second rail, and a
/// program that only prints and sleeps cannot do anything to the company. The
/// message is sanitized by [`plain_message`] before it reaches the quoting.
fn notice_script(message: &str) -> String {
    // `sleep` IN THE BACKGROUND AND `wait` FOR IT, never a foreground sleep.
    //
    // POSIX sh runs a trap for a signal received while a FOREGROUND command is
    // running only once that command finishes — so a bare `sleep 900` would
    // swallow the resize and repaint a quarter of an hour later, which is the
    // same as not repainting at all. `wait` is the one builtin a trap
    // interrupts, so the notice re-centres the moment tmux resizes it.
    let hold = "while :; do sleep 3600 & wait $!; done";
    // REPAINTED ON EVERY RESIZE, never centred once and left.
    //
    // The pane is SPLIT first and LAID OUT after, so a script that measures
    // `tput cols` at startup measures the split's size and not the size it ends
    // up. The layout then widens it, the padding computed for the narrower pane
    // stays put, and the text sits left of centre with a band of empty space on
    // the right — which is exactly what the operator saw: "it's always not
    // centered, it feels like it's leaving some space on the right, like some
    // panels are about to show up."
    //
    // `trap … WINCH` is the whole fix: tmux sends SIGWINCH to the pane's process
    // on every resize, so the notice re-measures and re-centres itself instead
    // of preserving a measurement taken before the geometry settled. The first
    // paint happens immediately, so nothing waits on a resize that may never
    // come.
    format!(
        "m='{message}'; \
         paint() {{ c=$(tput cols 2>/dev/null || echo 80); \
         l=$(tput lines 2>/dev/null || echo 24); p=$(( (c - ${{#m}}) / 2 )); \
         [ $p -lt 0 ] && p=0; clear; i=1; while [ $i -lt $(( l / 2 )) ]; do printf '\\n'; \
         i=$(( i + 1 )); done; printf '%*s%s\\n' $p '' \"$m\"; }}; \
         trap paint WINCH; paint; {hold}"
    )
}

/// What a department with nobody awake says.
///
/// It names the department, states the fact, and gives the one gesture that
/// changes it — the operator asked for exactly that shape: "just show that
/// it's sleeping… click on the person to wake them up."
fn sleeping_script(department: &str, asleep: usize) -> String {
    let who =
        if asleep == 1 { "1 person is asleep" } else { &format!("{asleep} people are asleep") };
    notice_script(&format!(
        "{} — {who}. Click a person in the sidebar to wake them.",
        plain_message(department)
    ))
}

/// Text with everything a shell could read as syntax removed.
///
/// It reaches a `sh -c` string, so this is a fence and not a cosmetic: anything
/// that is not a letter, a digit, a space or a dash is dropped, and an empty
/// result becomes `the person`. A roster is written by the company's own people,
/// which is exactly the input a quoting bug would be reached through.
fn plain_message(name: &str) -> String {
    let cleaned: String =
        name.chars().filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-').take(40).collect();
    let trimmed = cleaned.trim().to_owned();
    if trimmed.is_empty() {
        "the person".to_owned()
    } else {
        trimmed
    }
}

/// SHOW A DEPARTMENT WHOSE PEOPLE ARE ALL ASLEEP — and never somebody else's.
///
/// # The redirect this replaces
///
/// A department with nobody live has no window (placement mints none for an
/// empty one), so a click on it used to fall through to whatever else was
/// showing, and the rail then moved the operator to a department that DID have
/// somebody. Their words: "my engineering department was fully sleeping. When I
/// clicked on engineering, what it showed me is the CEO… we should never fall
/// back to the CEO department."
///
/// So the clicked department gets a window of its own — tagged with its OWN
/// logical id, railed, holding one panel that says how many people are asleep in
/// it and that clicking one wakes them. Converge does not want this window (the
/// department is empty), and the guarded reap will take it the moment the
/// operator looks elsewhere, which is exactly the right lifetime for a notice.
///
/// Answers whether the operator was moved to it, and whether a window had to be
/// minted for that. A notice window that already exists is pure navigation:
/// Stage 4 stops re-laying it on the way in, because a `select-layout` is a
/// window resize whatever its string says.
pub fn show_department_overview(tmux: &dyn Tmux, session: &str, click: &Overview) -> Shown {
    let canonical = canonical_geometry(tmux, session);
    let moved_geometry;
    let fresh;
    let window = match department_window(tmux, session, click.department_id) {
        Some(existing) => {
            fresh = false;
            // ALREADY SHOWING IT: DO NOTHING AT ALL.
            //
            // MEASURED on a live company, and it is the worst defect this
            // surface has shipped. This function runs from `Rail::refresh`,
            // which runs on every changefeed wake — and a company with one
            // chatty agent wakes it many times a SECOND. Each call re-laid the
            // window, re-selected it, and woke the other window's rail, which
            // redrew and woke this one back. The glass churned continuously:
            // panes resizing, the sidebar jumping, clicks landing in the
            // middle of a relayout and appearing to do nothing until the
            // operator pressed a second time. Every symptom the operator
            // reported — "it's shifting", "the people pane grows and flickers
            // back", "I have to double-click" — is this loop.
            //
            // THE RULE, and it is general: an effect reached from the refresh
            // path may only fire on a TRANSITION. When the window is already
            // there, already holding its notice and already on the glass,
            // there is nothing to do and doing it anyway is what breaks the
            // product.
            //
            // THE GUARD USED TO FALL OPEN WHEN ITS OWN PANE EXPIRED, and that
            // is how 409 of these landed in one session's log. The notice pane
            // is a shell that prints and waits; when it ended, no pane carried
            // `ASLEEP` any more, this test went false — and the arm below
            // relaid the window, re-selected it and woke the other rail on
            // EVERY changefeed wake, while never putting the notice back. The
            // operator was left re-selected once a second into a window holding
            // nothing but a full-width rail, which is why their first click
            // always seemed to do nothing.
            //
            // So the pane is ENSURED before the test rather than assumed by it:
            // a missing notice is restored, and the pass after that is
            // genuinely unchanged and returns here. The notice itself now waits
            // without a deadline (`sleeping_script`) so the ordinary case never
            // reaches the restore at all.
            moved_geometry = ensure_sleeping_pane(tmux, session, &existing, click);
            if window_is_active(tmux, session, &existing) {
                tracing::debug!(
                    event = "sidebar.department.sleeping.unchanged",
                    session,
                    department = click.department_id,
                    window = %existing,
                    "the sleeping notice is already up and already on the glass; nothing \
                     was touched"
                );
                return Shown { shown: true, moved_geometry };
            }
            existing
        }
        None => {
            let Some(window) = mint_sleeping_department_window(tmux, session, click, canonical)
            else {
                tracing::error!(
                    event = "sidebar.department.unminted",
                    session,
                    department = click.department_id,
                    "tmux did not report a window for the sleeping department; the operator \
                     was left where they were rather than being sent somewhere else"
                );
                return Shown::nothing();
            };
            fresh = true;
            moved_geometry = true;
            window
        }
    };
    // NAVIGATION ONLY. The `relay` that used to stand here ran on every click on
    // an already-standing notice window — a `select-layout`, which tmux applies
    // as a window resize whatever its string says, on a window whose membership
    // nobody had changed. The two arms above lay this window out when they
    // actually alter it, and this is the whole of the gesture otherwise.
    let mut batch = Batch::new();
    if !fresh {
        normalize_into(&mut batch, tmux, &window, canonical);
    }
    batch.push(&["select-window", "-t", &window]);
    batch.run(tmux);
    // THE SAME RECEIPT AS THE PERSON CLICK. This log asserts the operator is
    // looking at the department's window; asserting it without reading tmux
    // back is the exact shape that made a failed person-click invisible for
    // four reports.
    if navigation_diverged(tmux, session, &window).is_some() {
        let retry = tmux.run(&["select-window", "-t", &window]);
        if let Some(active) = navigation_diverged(tmux, session, &window) {
            tracing::warn!(
                event = "sidebar.navigation.failed",
                session,
                department = click.department_id,
                window = %window,
                active = %active,
                reason = %if retry.trim().is_empty() {
                    "the active window did not change".to_owned()
                } else {
                    retry
                },
                "the department click did not move the glass; the rail now says something \
                 the operator cannot see"
            );
            return Shown { shown: false, moved_geometry };
        }
    }
    tracing::info!(
        event = "sidebar.department.sleeping",
        session,
        department = click.department_id,
        asleep = click.asleep,
        window = %window,
        moved_geometry,
        "the clicked department has nobody up; its own window says so, and NOBODY was \
         redirected to another department"
    );
    Shown { shown: true, moved_geometry }
}

/// Keep a sleeping person's home available without putting it on the glass.
///
/// A wake is shown in the permanent focus window. If the home department has
/// nobody else up, it can have no ordinary window yet. This hidden ensure gives
/// an immediate department click a complete, railed, canonical destination
/// before the wake or its person is published.
pub fn ensure_sleeping_department_window(
    tmux: &dyn Tmux,
    session: &str,
    click: &Overview,
) -> Option<String> {
    if let Some(window) = department_window(tmux, session, click.department_id) {
        return Some(window);
    }
    mint_sleeping_department_window(tmux, session, click, canonical_geometry(tmux, session))
}

/// Mint one sleeping department as a complete hidden frame.
///
/// `new-window -d` keeps the client on its current window, but separate tmux
/// invocations still publish the new window to observers between calls. The
/// whole construction therefore travels in one server command sequence: window
/// identity, notice identity, canonical geometry, exact rail split, rail tag,
/// and final layout. The caller selects the reported window only after this
/// function returns.
fn mint_sleeping_department_window(
    tmux: &dyn Tmux,
    session: &str,
    click: &Overview,
    canonical: Option<crate::window_geometry::Geometry>,
) -> Option<String> {
    let Some(program) = click.rail_program else {
        tracing::error!(
            event = "sidebar.department.unminted",
            session,
            department = click.department_id,
            "a department window cannot be published without its rail program"
        );
        return None;
    };
    let Some(canonical) = canonical else {
        tracing::error!(
            event = "sidebar.department.unminted",
            session,
            department = click.department_id,
            "a department window cannot be published without canonical geometry"
        );
        return None;
    };
    let columns = rail_columns(tmux, session);
    let layout = crate::layout::organization_tmux_layout(
        i64::from(canonical.columns),
        i64::from(canonical.rows),
        Some(crate::layout::Rail { pane_id: "%1", columns }),
        &["%2"],
    )
    .ok()?;
    let target = format!("{session}:$");
    let name = crate::placement::safe_window_name(click.department_name);
    let script = sleeping_script(click.department_name, click.asleep);
    let company_dir = click.company_dir.display().to_string();

    let mut batch = Batch::new();
    let mut open = vec![
        "new-window".to_owned(),
        "-d".to_owned(),
        "-a".to_owned(),
        "-n".to_owned(),
        name.clone(),
        "-t".to_owned(),
        target.clone(),
        "-P".to_owned(),
        "-F".to_owned(),
        "#{window_id}".to_owned(),
    ];
    match click.card {
        Some(program) => open.extend_from_slice(program),
        None => {
            open.push("sh".to_owned());
            open.push("-c".to_owned());
            open.push(script.clone());
        }
    }
    batch.push_owned(&open);
    batch.push(&["set-option", "-w", "-t", &target, tags::ORGANIZATION, click.organization]);
    batch.push(&["set-option", "-w", "-t", &target, tags::WINDOW, click.department_id]);
    batch.push(&["set-option", "-p", "-t", &target, tags::ASLEEP, click.department_id]);
    // WHAT THIS PANE IS DRAWING, stamped by the verb that drew it. Without it
    // the pass right after a click sees an unstamped pane, reads "this is not
    // the card I would draw now", and repaints a card that is one millisecond
    // old — a visible flash for no fact change at all.
    if let Some(fingerprint) = card_fingerprint(click.card) {
        batch.push(&["set-option", "-p", "-t", &target, tags::DEPARTMENT_CARD, &fingerprint]);
    }
    // AND ITS BORDER TITLE, at the MINT. Set here as well as on refresh
    // because a card that never changes is never refreshed, and an untitled
    // pane falls back to tmux's default format — which draws the machine's
    // hostname. See `sidebar::department_border_format`.
    let mint_border = super::department_border_format();
    batch.push(&["set-option", "-p", "-t", &target, "pane-border-format", &mint_border]);
    let Some(normalize) = crate::window_geometry::normalization_argv(&target, None, canonical)
    else {
        tracing::error!(
            event = "sidebar.department.unminted",
            session,
            department = click.department_id,
            "a new department window cannot be published without its canonical geometry"
        );
        return None;
    };
    batch.push_owned(&normalize);
    batch.push(&[
        "split-window",
        "-h",
        "-b",
        "-l",
        &columns.to_string(),
        "-t",
        &target,
        "-c",
        &company_dir,
        program,
        "sidebar",
    ]);
    batch.push(&["set-option", "-p", "-t", &target, tags::SIDEBAR, "1"]);
    batch.push(&["select-layout", "-t", &target, &layout]);

    batch
        .run_topology(tmux, session)
        .lines()
        .find_map(|line| line.trim().starts_with('@').then(|| line.trim().to_owned()))
}

/// The fingerprint of what a department overview card ARGV would draw.
///
/// `None` when there is no card — the one-line notice fallback carries no
/// payload, so there is nothing to compare and nothing to stamp.
///
/// The payload is the LAST argument (`chief department-card <json>`), which is
/// the whole of what the card renders: [`super::department_card::Card`] is
/// `PartialEq` and the program reads nothing else, so two equal payloads are
/// two identical pictures by construction.
fn card_fingerprint(card: Option<&[String]>) -> Option<String> {
    use sha2::{Digest as _, Sha256};

    let payload = card?.last()?;
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(payload.as_bytes()) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Some(hex)
}

/// One department overview window standing in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingOverview {
    /// The tmux window it lives in.
    pub window: String,
    /// The window's own logical id (`__overview__:<department>`), which is also
    /// the value the card pane carries as its [`tags::ASLEEP`] tag.
    pub overview_id: String,
    /// The department it reports on.
    pub department_id: String,
}

/// EVERY department overview standing in this session, in window order.
///
/// # Why every one and not "the selected department"
///
/// The refresh used to ask the brain which department was SELECTED and repaint
/// only that card. MEASURED on a live company: a session holds as many overview
/// windows as the operator has clicked departments — `__overview__:executive`
/// and `__overview__:research` at once — and at most one of them can be the
/// selection, so the other went stale exactly as before the refresh existed.
/// The operator saw a rail reading `Research 0/3` beside a card reading
/// `1 up`, which is the original defect surviving inside its own repair.
///
/// A window is the honest unit here: a card exists because a window holds it,
/// and every window that holds one has to be right. The selection decides what
/// the operator is LOOKING at and has never decided what is true.
#[must_use]
pub fn standing_overviews(tmux: &dyn Tmux, session: &str) -> Vec<StandingOverview> {
    tmux.run(&[
        "list-windows",
        "-t",
        session,
        "-F",
        &format!("#{{window_id}}\t#{{{}}}", tags::WINDOW),
    ])
    .lines()
    .filter_map(|line| line.split_once('\t'))
    .filter_map(|(window, logical)| {
        let logical = logical.trim();
        crate::placement::overview_department_id(logical).map(|department| StandingOverview {
            window: window.trim().to_owned(),
            overview_id: logical.to_owned(),
            department_id: department.to_owned(),
        })
    })
    .collect()
}

/// REPAINT ONE DEPARTMENT OVERVIEW CARD IN PLACE, and only when its facts moved.
///
/// # The staleness this ends
///
/// The card was argv handed to a pane at spawn time and the only thing that
/// ever spawned it again was another department CLICK. So it froze the instant
/// it was drawn. MEASURED on the operator's box: their rail read `Executive
/// 2/5` with Chief and Sam green while the card beside it read `0 up · 4 asleep
/// · 1 starting`, `Chief … starting`, `Sam … asleep`. Both surfaces were
/// correct about the moment they were rendered and one of those moments was
/// minutes old. A report that cannot be told apart from a lie by looking at it
/// is worse than no report.
///
/// # Why it may not do what the click does
///
/// This runs from the COMPANY-READ path, which a chatty company wakes many
/// times a second — the exact path whose effects
/// [`show_department_overview`] documents as the churn loop that re-laid the
/// window, re-selected it and woke the other rail until the glass churned
/// continuously. So this verb obeys that module's rule rather than working
/// around it, in three parts:
///
/// 1. **It fires only on a TRANSITION.** The pane carries a fingerprint of what
///    it is drawing ([`tags::DEPARTMENT_CARD`]); an identical payload issues no
///    tmux write at all, not even a stamp.
/// 2. **It never touches geometry.** `respawn-pane -k` replaces the process
///    inside a pane that keeps its id, its window, its size and its position.
///    There is no `select-layout` here and there must never be one — tmux
///    applies a layout as a window resize whatever its string says.
/// 3. **It never navigates.** No `select-window`, no `select-pane`. If the
///    operator has moved to another window, this repaints a card they are not
///    looking at and leaves them where they are.
///
/// Answers whether it actually repainted, for the caller's log and for the
/// tests that pin the transition rule.
pub fn refresh_department_card(
    tmux: &dyn Tmux,
    session: &str,
    overview: &StandingOverview,
    card: Option<&[String]>,
) -> bool {
    let Some(fingerprint) = card_fingerprint(card) else {
        return false;
    };
    let (window, department_id) = (overview.window.as_str(), overview.overview_id.as_str());
    // THE CARD'S OWN PANE, found by the tag the mint wrote — never "the last
    // pane" and never "the one that is not the rail". A department overview
    // window holds a rail and a card, but `ensure_sleeping_pane` records at
    // length that this window can also hold a DEAD pane tmux has not reaped and
    // that pane order is not creation order. The tag is the only honest answer.
    let panes = window_panes(tmux, window);
    let Some(pane) = panes
        .iter()
        .map(|(pane, _)| pane)
        .find(|pane| pane_tag(tmux, pane, tags::ASLEEP) == department_id)
        .cloned()
    else {
        return false;
    };
    if pane_tag(tmux, &pane, tags::DEPARTMENT_CARD) == fingerprint {
        tracing::trace!(
            event = "sidebar.department.card.unchanged",
            session,
            department = department_id,
            pane = %pane,
            "the card on the glass already draws these facts; nothing was touched"
        );
        return false;
    }
    let mut batch = Batch::new();
    batch.push(&["set-option", "-p", "-t", &pane, tags::DEPARTMENT_CARD, &fingerprint]);
    // AND ITS BORDER TITLE. `pane-border-status` is on globally, and a pane
    // nothing has titled falls back to tmux's default format —
    // `#{pane_index} "#{pane_title}"` — whose title for a pane like this one is
    // THE MACHINE'S HOSTNAME. The rail and every person pane are titled; the
    // card was not, so clicking a department drew the operator's hostname above
    // it, on every box, in every department window.
    let border = super::department_border_format();
    batch.push(&["set-option", "-p", "-t", &pane, "pane-border-format", &border]);
    let mut respawn =
        vec!["respawn-pane".to_owned(), "-k".to_owned(), "-t".to_owned(), pane.clone()];
    respawn.extend_from_slice(card.unwrap_or_default());
    batch.push_owned(&respawn);
    batch.run(tmux);
    tracing::info!(
        event = "sidebar.department.card.repainted",
        session,
        department = department_id,
        pane = %pane,
        "this department's facts moved, so its card was redrawn in place; no window was \
         laid out and nobody was navigated"
    );
    true
}

/// One sleeping notice in `window`, minted only if it has none.
///
/// The sibling of [`ensure_loading_pane`], and it exists for the same reason in
/// reverse: [`show_department_overview`]'s reuse arm found a window it had
/// already built, assumed the notice was still in it, and re-selected the window
/// instead of looking. A notice pane can go — its shell can end, converge can
/// reap around it, an operator can close it — and a department window with no
/// notice and no people is a full-width rail and nothing else.
///
/// Answers whether it actually restored one, because that is a geometry change
/// and the caller has to say so.
fn ensure_sleeping_pane(tmux: &dyn Tmux, session: &str, window: &str, click: &Overview) -> bool {
    let panes = window_panes(tmux, window);
    if panes.iter().any(|(pane, _)| !pane_tag(tmux, pane, tags::ASLEEP).is_empty()) {
        return false;
    }
    // SPLIT A NON-RAIL PANE, AND ONLY SPLIT THE RAIL WHEN IT IS GENUINELY ALONE.
    //
    // This took `panes.last()` unconditionally, and `last()` is the RAIL more
    // often than it looks. Measured on the operator's box as a `43 -> 21 -> 43`
    // round trip — the sidebar halved and restored — one of three churn shapes
    // in 345 logged rail resizes.
    //
    // Two ways `last()` is the rail:
    //
    // 1. Pane ORDER is not creation order. A `join-pane` reorders the window's
    //    panes (this module documents that reorder itself), so the rail can end
    //    up last.
    // 2. `window_panes` filters out DEAD panes — but a dead pane still holds its
    //    columns until tmux reaps it. So the survivor list can be `[rail]` while
    //    the window is still visually full, and splitting "the last pane" halves
    //    the sidebar.
    //
    // `ensure_loading_pane` was given this rule; its sibling never was. The rule
    // is the same one, stated once more: the rail is the pane whose width the
    // OPERATOR chose, so it is the last pane in the window that may be taken
    // from — and taking from it is visible even for the frame before the layout
    // corrects it.
    let beside = panes.iter().find(|(_, is_rail)| !*is_rail).map(|(pane, _)| pane.clone());
    // `Side::After`: THE NOTICE IS THE PANE THAT DIES. `close_sleeping_notices`
    // sweeps it the moment anybody in this department comes up, so it goes on the
    // far side of its sibling and hands its columns back to them. In front, it
    // hands them to the RAIL — measured on the operator's box as a 147-column
    // sidebar 950ms into the session, latched for the rest of it.
    let Some(park) = park_beside(tmux, session, &panes, beside.as_deref(), Side::After) else {
        // Not one pane left, so there is no window to split and nothing to
        // repair. tmux destroys a window with its last pane, so this is a
        // window that has already gone.
        return false;
    };
    let Some(topology_generation) = invalidate_viewport_topology(tmux, session) else {
        return false;
    };
    let argv = match click.card {
        Some(program) => furniture_split_program(park, program),
        None => furniture_split(park, &sleeping_script(click.department_name, click.asleep)),
    };
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let pane = tmux.run(&argv);
    let pane = pane.lines().next().map(str::trim).unwrap_or_default();
    if !pane.starts_with('%') {
        refresh_viewport_topology(tmux, session, &topology_generation);
        return false;
    }
    tmux.run(&["set-option", "-p", "-t", pane, tags::ASLEEP, click.department_id]);
    if let Some(fingerprint) = card_fingerprint(click.card) {
        tmux.run(&["set-option", "-p", "-t", pane, tags::DEPARTMENT_CARD, &fingerprint]);
    }
    // The restored pane is a card too, and it is titled for the same reason.
    tmux.run(&[
        "set-option",
        "-p",
        "-t",
        pane,
        "pane-border-format",
        &super::department_border_format(),
    ]);
    relay(tmux, session, window);
    refresh_viewport_topology(tmux, session, &topology_generation);
    tracing::info!(
        event = "sidebar.department.sleeping.restored",
        session,
        department = click.department_id,
        window = %window,
        pane = %pane,
        "this department's window had lost its notice and held nothing but a rail; the \
         notice is back rather than the operator being re-selected into an empty window"
    );
    true
}

/// Everything the sleeping-department notice needs.
pub struct Overview<'a> {
    /// The company slug used for the `@organization_id` tag.
    pub organization: &'a str,
    /// The department the operator clicked. Also the window's logical id.
    pub department_id: &'a str,
    /// Its display name, for the window name and the notice.
    pub department_name: &'a str,
    /// How many of its people are asleep, which is all of them.
    pub asleep: usize,
    /// The OVERVIEW CARD's argv, when the brain has facts to draw.
    ///
    /// `None` falls back to the one-line notice this surface started as — the
    /// path taken before the brain's first company read, when there is nothing
    /// to draw a card FROM and a card of empty columns would be a worse answer
    /// than a sentence.
    ///
    /// It rides on this struct rather than getting a parallel one because the
    /// window, the pane, the tag and the transition-only discipline are all
    /// identical; the only thing that differs is what the pane runs, and the
    /// churn loop documented on `show_department_overview` is the reason not to
    /// grow a second implementation of the rest.
    pub card: Option<&'a [String]>,
    /// The program a freshly minted rail runs; `None` mints none and says so.
    pub rail_program: Option<&'a str>,
    /// The company root used as the rail process working directory.
    pub company_dir: &'a std::path::Path,
}

/// Kill `pane`, unless doing so would leave its window holding only the rail.
///
/// # The multi-second full-width dwells this ends
///
/// In a flat row `{rail, A}`, killing A hands A's columns to its previous
/// sibling — the RAIL (probed: 29 → 65 columns, SIGWINCH delivered). The
/// sidebar becomes the whole window, and stays there until something reactive
/// notices and repairs it. Measured on the operator's live company: dwells of
/// **3.2 and 6.4 seconds** at full width. That repair latency is the single
/// largest visible artifact in the whole surface — far bigger than any frame.
///
/// It is also the head of the corruption chain, because every one of those
/// transits opens tmux's grid/pty gap twice: once going out, once coming back.
///
/// So a pane whose death would strand the rail is left alone when the operator
/// is LOOKING at that window, and the window is taken whole when they are not.
/// Both arms avoid the dwell; they differ only in what is honest to do in front
/// of somebody:
///
/// * **Watched window** — the pane stays. Whatever it says is at worst a moment
///   stale (a `Loading …` for somebody who just arrived, a notice for a
///   department that just woke), and the next pass replaces it in place. A
///   stale sentence is a far smaller lie than the sidebar swallowing the screen.
/// * **Unwatched window** — `kill-window`. Nothing to strand, nothing to see,
///   and it saves converge a reap.
///
/// # Why only the sleeping-notice sweep uses this
///
/// The LOADING panel is a different case and deliberately keeps its bare kill: it
/// is closed because the person it stood in for has ARRIVED, so a content pane
/// either already replaced it in place (`respawn_into_placeholder`) or is joining
/// the window in the same gesture. Protecting it there would strand a stale
/// `Loading …` beside the person it was waiting for — a worse lie than the
/// moment of full-width it avoids, and one a live test caught immediately.
///
/// Returns whether the pane was actually removed, because the caller logs it.
fn kill_pane_without_stranding_the_rail(tmux: &dyn Tmux, session: &str, pane: &str) -> bool {
    let Some(window) = window_of_pane(tmux, pane) else {
        // No window to strand. Nothing to protect.
        tmux.run(&["kill-pane", "-t", pane]);
        return true;
    };
    let others = window_panes(tmux, &window)
        .into_iter()
        .filter(|(other, is_rail)| other != pane && !is_rail)
        .count();
    if others > 0 {
        // Somebody else holds content here, so the rail inherits nothing.
        tmux.run(&["kill-pane", "-t", pane]);
        return true;
    }
    if window_is_active(tmux, session, &window) {
        tracing::debug!(
            event = "sidebar.pane.kept",
            session,
            pane,
            window = %window,
            "this is the window's last content pane and the operator is looking at it; \
             killing it would hand its columns to the rail and leave the sidebar full-width \
             until something repaired it"
        );
        return false;
    }
    tmux.run(&["kill-window", "-t", &window]);
    tracing::info!(
        event = "sidebar.window.emptied",
        session,
        pane,
        window = %window,
        "the window's last content pane went and nobody was watching, so the window went \
         with it rather than leaving a rail alone in it"
    );
    true
}

/// Why a sleeping notice has stopped being true.
///
/// Two answers, and they are different findings: one is the ordinary life of a
/// department, the other is furniture that outlived the thing it described.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeStale {
    /// Somebody in that department came up.
    DepartmentAwake,
    /// The department itself is gone from the roster.
    DepartmentGone,
}

/// THE RULE, pure, so both halves of it are provable without a tmux server.
///
/// `live_departments` is who has somebody up; `known_departments` is every
/// department the current roster still holds.
///
/// Three things this must NOT sweep, each of which has a live failure behind
/// it:
///
/// * `__focus__`. The permanent focus window parks behind a standing notice
///   carrying that sentinel in the same tag, and it is not a department. Sweep
///   it and `never_blank` is handed the rail-only window it exists to prevent.
/// * A notice for a department that is simply ASLEEP — the whole point of it.
/// * ANY notice, when the roster read produced no departments at all. A
///   company always has at least a root department, so an empty set is a
///   failed or half-built read, and letting one delete every notice on the
///   glass is a worse outcome than keeping stale furniture for one pass.
#[must_use]
pub fn notice_stale(
    department: &str,
    live_departments: &BTreeSet<String>,
    known_departments: &BTreeSet<String>,
) -> Option<NoticeStale> {
    if department == FOCUS_WINDOW_ID {
        return None;
    }
    if live_departments.contains(department) {
        return Some(NoticeStale::DepartmentAwake);
    }
    if !known_departments.is_empty() && !known_departments.contains(department) {
        return Some(NoticeStale::DepartmentGone);
    }
    None
}

/// Take a REMOVED department's notice down, and its window with it when the
/// notice is all the window has left.
///
/// # Why this is not [`kill_pane_without_stranding_the_rail`]
///
/// That function protects a WATCHED window: rather than let a dying last pane
/// hand its columns to the rail in front of somebody, it keeps the pane and
/// lets the next pass replace it in place. That trade is right for every
/// notice it was written for, because a next pass exists — a department that
/// woke gets people in that window seconds later.
///
/// A department that has been REMOVED has no next pass. Its window is not in
/// the desired topology any more, so no converge pass places anything in it and
/// nothing ever replaces the sentence. Measured on a live company: the
/// department was removed, the rail dropped it immediately, and the pane still
/// read `Research — 4 people are asleep. Click a person in the sidebar to wake
/// them.` for a department that did not exist — the operator was LOOKING at
/// that window, which is exactly the branch that keeps it. Only a restart
/// cleared it.
///
/// So the window goes whole. That strands nothing: every window this product
/// builds carries its OWN rail, so killing the window takes that rail with it
/// and tmux moves the client to a window that still has one. The full-width
/// dwell the other function avoids cannot happen, because no pane is left in
/// this window to be widened.
///
/// A window that still holds other content keeps it: only the notice pane is
/// killed, which is the same first branch as the ordinary sweep.
fn retire_notice_with_its_window(tmux: &dyn Tmux, session: &str, pane: &str) -> bool {
    let Some(window) = window_of_pane(tmux, pane) else {
        tmux.run(&["kill-pane", "-t", pane]);
        return true;
    };
    let others = window_panes(tmux, &window)
        .into_iter()
        .filter(|(other, is_rail)| other != pane && !is_rail)
        .count();
    if others > 0 {
        tmux.run(&["kill-pane", "-t", pane]);
        return true;
    }
    tmux.run(&["kill-window", "-t", &window]);
    tracing::info!(
        event = "sidebar.window.retired",
        session,
        pane,
        window = %window,
        "the window belonged to a department that is gone, so it went whole rather than \
         leaving a notice for something that does not exist"
    );
    true
}

/// Kill a sleeping notice that has stopped being true: its department has
/// somebody up again, or its department is GONE.
///
/// The notice is true when it is drawn and false the moment anybody in that
/// department comes up, so it is swept on the refresh that sees them live —
/// leaving a "everyone is asleep" panel beside a working person would be a
/// worse lie than the redirect this replaced.
///
/// # The notice that outlived its department
///
/// A department the operator removed took its people with it and left its
/// `@chief_asleep_for <department>` pane on the glass. Nothing swept it: this
/// function only ever matched departments that were AWAKE, and a department
/// that does not exist has nobody in it to come up, so the notice was true
/// forever by that test. `chief topology` never listed its window either —
/// placement is derived from the CURRENT tree, and the tree no longer had that
/// department — so no converge pass owned it and only a restart cleared it.
/// A notice describes a department; when the department goes, the notice goes
/// with it.
pub fn close_sleeping_notices(
    tmux: &dyn Tmux,
    session: &str,
    live_departments: &BTreeSet<String>,
    known_departments: &BTreeSet<String>,
) {
    let mut topology_generation = None;
    for (pane, department) in tagged_panes(tmux, session, tags::ASLEEP) {
        // AN OVERVIEW IS NOT A NOTICE, and the difference decides both tests
        // below. Its tag names a window (`__overview__:<department>`), not a
        // department, so the roster question has to be asked about the
        // department INSIDE it — and it stays up when somebody comes up,
        // because a card reporting "3 asleep" is a card that will report "1 up"
        // on the next pass rather than a sentence that has become false.
        let overview = crate::placement::overview_department_id(&department);
        let subject = overview.unwrap_or(&department);
        let Some(stale) = notice_stale(subject, live_departments, known_departments) else {
            continue;
        };
        if overview.is_some() && matches!(stale, NoticeStale::DepartmentAwake) {
            continue;
        }
        if topology_generation.is_none() {
            let Some(generation) = invalidate_viewport_topology(tmux, session) else {
                return;
            };
            topology_generation = Some(generation);
        }
        let removed = match stale {
            // A department that woke will be replaced in place by the next
            // pass, so a watched window may keep its stale sentence for a
            // moment rather than let the rail swallow the screen.
            NoticeStale::DepartmentAwake => {
                kill_pane_without_stranding_the_rail(tmux, session, &pane)
            }
            // A department that is GONE has no next pass. Nothing will ever
            // replace this sentence, so the watched-window courtesy becomes a
            // permanent lie — see `retire_notice_with_its_window`.
            NoticeStale::DepartmentGone => retire_notice_with_its_window(tmux, session, &pane),
        };
        if !removed {
            continue;
        }
        match stale {
            NoticeStale::DepartmentAwake => tracing::info!(
                event = "sidebar.department.awake",
                session,
                department = %department,
                pane = %pane,
                "somebody in that department came up, so its sleeping notice is gone"
            ),
            NoticeStale::DepartmentGone => tracing::info!(
                event = "sidebar.department.removed",
                session,
                department = %department,
                pane = %pane,
                "that department is no longer in the roster, so its sleeping notice went with it"
            ),
        }
    }
    if let Some(generation) = topology_generation {
        refresh_viewport_topology(tmux, session, &generation);
    }
}

// ---------------------------------------------------------------------------
// The session's ONE PERMANENT focus window (Stage 4)
// ---------------------------------------------------------------------------

/// What the focus window is called when nobody is in it.
///
/// It is renamed to the person's own name the moment somebody is shown in it,
/// and back to this when they go home — because converge has no rename step at
/// all, so a window left holding the previous occupant's name keeps it for ever.
const PARKED_WINDOW_NAME: &str = "Person";

/// The exact first frame for one cold person click.
pub struct FocusPerson<'a> {
    /// Stable person id written to the private furniture marker.
    pub person_id: &'a str,
    /// Human display name shown in the immediate body.
    pub name: &'a str,
    /// Roster role shown in the pane border.
    pub role: &'a str,
    /// Identity accent used for the role chip.
    pub accent: &'a str,
    // TOMBSTONE: `homes`, `home_names`, `organization`, `rail_program` and
    // `company_dir`. Every one of them existed for `handoff_occupied_focus`,
    // which returned the focus window's LIVE OCCUPANT to their department —
    // minting that department's window around them when it no longer existed —
    // so a cold click could take their cell. The focus window holds no live
    // person any more, so there is nobody to hand off and nothing to mint.
    /// What the body says while the person is on their way, when the plain
    /// promise would be a lie.
    ///
    /// `None` paints `NAME is starting…`, which is the truth for the ordinary
    /// click: chiefd wants them, the actuator is on its way, the pane is
    /// seconds out. A person whose boot has died eleven times in the last four
    /// minutes is NOT seconds out, and this pane sat on that sentence for an
    /// hour and a half on the owner's box while nothing behind it was working.
    /// The crash report goes here instead, so the operator reads the retry
    /// number and the error in the place they clicked to find them.
    pub standing: Option<&'a str>,
}

/// Reserve and repaint the permanent focus body in one tmux publication.
///
/// The body pane is also the final Pi pane. The actuator later claims it with
/// `respawn-pane`; no second content pane is created and no generic frame is
/// visible between the click and Pi's startup wrapper.
pub fn show_waking_focus(
    tmux: &dyn Tmux,
    session: &str,
    waking: &FocusPerson<'_>,
) -> Option<String> {
    let claim = uuid::Uuid::new_v4().simple().to_string();
    let window = department_window(tmux, session, FOCUS_WINDOW_ID)?;
    let listed = tmux.run(&[
        "list-panes",
        "-t",
        &window,
        "-F",
        &format!(
            "#{{pane_id}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}",
            tags::SIDEBAR,
            tags::ASLEEP,
            tags::WAKING_PERSON,
            tags::PERSON
        ),
    ]);
    let pane = listed.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let pane = fields.next()?.trim();
        let rail = fields.next().unwrap_or_default().trim();
        let asleep = fields.next().unwrap_or_default().trim();
        let prior_waking = fields.next().unwrap_or_default().trim();
        let person = fields.next().unwrap_or_default().trim();
        (rail.is_empty()
            && person.is_empty()
            && (asleep == FOCUS_WINDOW_ID || !prior_waking.is_empty()))
        .then(|| pane.to_owned())
    });
    let script = notice_script(&waking.standing.map_or_else(
        || format!("{} is starting…", plain_message(waking.name)),
        |standing| format!("{} · {}", plain_message(waking.name), plain_message(standing)),
    ));
    let launch = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];
    let border = super::person_border_format(waking.name, waking.role, waking.accent);
    let identity = super::person_short_identity(waking.name);
    // NO BODY TO REUSE. There is no live occupant to hand home any more — the
    // card window holds furniture only — so what is left of the old handoff is
    // the split that MAKES the body. See [`mint_focus_card_body`].
    let Some(pane) = pane else {
        return mint_focus_card_body(
            tmux,
            session,
            &window,
            &FocusCard {
                marker: tags::WAKING_PERSON,
                person_id: waking.person_id,
                claim: Some(&claim),
                launch: &launch,
                border: &border,
                window_name: &identity,
            },
        );
    };
    let mut batch = Batch::new();
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::ASLEEP]);
    batch.push(&["set-option", "-p", "-t", &pane, tags::WAKING_PERSON, waking.person_id]);
    batch.push(&["set-option", "-p", "-t", &pane, tags::WAKE_CLAIM, &claim]);
    batch.push(&["set-option", "-p", "-t", &pane, tags::WAKING_PENDING, &claim]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_DESIRED_SEEN]);
    batch.push(&["set-option", "-g", "pane-border-status", "top"]);
    batch.push(&["set-option", "-g", "pane-border-format", super::SAFE_BORDER_DEFAULT]);
    batch.push(&["set-option", "-p", "-t", &pane, "pane-border-format", &border]);
    batch.push(&["rename-window", "-t", &window, &identity]);
    let mut respawn =
        vec!["respawn-pane".to_owned(), "-k".to_owned(), "-t".to_owned(), pane.clone()];
    respawn.extend(launch);
    batch.push_owned(&respawn);
    batch.push(&["select-window", "-t", &window]);
    batch.push(&["select-pane", "-t", &pane]);
    batch.run(tmux);
    Some(pane)
}

/// Put an interactive sleeping-person card in the permanent final focus body.
/// This reserves the body but does not request a wake.
pub fn show_sleeping_focus(
    tmux: &dyn Tmux,
    session: &str,
    person: &FocusPerson<'_>,
    launch: &[String],
) -> Option<String> {
    let window = department_window(tmux, session, FOCUS_WINDOW_ID)?;
    let listed = tmux.run(&[
        "list-panes",
        "-t",
        &window,
        "-F",
        &format!(
            "#{{pane_id}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}",
            tags::SIDEBAR,
            tags::ASLEEP,
            tags::SLEEPING_PERSON,
            tags::WAKING_PERSON,
            tags::PERSON
        ),
    ]);
    let pane = listed.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let pane = fields.next()?.trim();
        let rail = fields.next().unwrap_or_default().trim();
        let asleep = fields.next().unwrap_or_default().trim();
        let sleeping = fields.next().unwrap_or_default().trim();
        let waking = fields.next().unwrap_or_default().trim();
        let owner = fields.next().unwrap_or_default().trim();
        (rail.is_empty()
            && owner.is_empty()
            && (waking.is_empty() || waking == person.person_id)
            && (asleep == FOCUS_WINDOW_ID || !sleeping.is_empty() || waking == person.person_id))
            .then(|| pane.to_owned())
    });
    let border = super::person_border_format(person.name, person.role, person.accent);
    let identity = super::person_short_identity(person.name);
    // See [`show_waking_focus`]: there is no live occupant to displace, only a
    // body to make when the window is rail-only.
    let Some(pane) = pane else {
        return mint_focus_card_body(
            tmux,
            session,
            &window,
            &FocusCard {
                marker: tags::SLEEPING_PERSON,
                person_id: person.person_id,
                claim: None,
                launch,
                border: &border,
                window_name: &identity,
            },
        );
    };
    let mut batch = Batch::new();
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::ASLEEP]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PERSON]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKE_CLAIM]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PENDING]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_DESIRED_SEEN]);
    batch.push(&["set-option", "-p", "-t", &pane, tags::SLEEPING_PERSON, person.person_id]);
    batch.push(&["set-option", "-g", "pane-border-status", "top"]);
    batch.push(&["set-option", "-g", "pane-border-format", super::SAFE_BORDER_DEFAULT]);
    batch.push(&["set-option", "-p", "-t", &pane, "pane-border-format", &border]);
    batch.push(&["rename-window", "-t", &window, &identity]);
    let mut respawn =
        vec!["respawn-pane".to_owned(), "-k".to_owned(), "-t".to_owned(), pane.clone()];
    respawn.extend_from_slice(launch);
    batch.push_owned(&respawn);
    batch.push(&["select-window", "-t", &window]);
    batch.push(&["select-pane", "-t", &pane]);
    batch.run(tmux);
    Some(pane)
}

/// Change one exact sleeping card into waking furniture before a wake request
/// is published. The final pane id does not change.
pub fn activate_sleeping_focus(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    pane: &str,
    person_id: &str,
) -> bool {
    activate_sleeping_focus_with_claim(
        tmux,
        session,
        organization,
        pane,
        person_id,
        &uuid::Uuid::new_v4().simple().to_string(),
    )
}

pub(super) fn activate_sleeping_focus_with_claim(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    pane: &str,
    person_id: &str,
    claim: &str,
) -> bool {
    let success = format!("chief-wake:{}:{}", pane, now_millis());
    let mut guarded = Batch::new();
    guarded.push(&["set-option", "-p", "-t", pane, tags::WAKING_PERSON, person_id]);
    guarded.push(&["set-option", "-p", "-t", pane, tags::WAKE_CLAIM, claim]);
    guarded.push(&["set-option", "-p", "-t", pane, tags::WAKING_PENDING, claim]);
    guarded.push(&["set-option", "-p", "-u", "-t", pane, tags::WAKING_DESIRED_SEEN]);
    guarded.push(&["set-option", "-p", "-u", "-t", pane, tags::SLEEPING_PERSON]);
    guarded.push(&["display-message", "-p", "-t", pane, &success]);
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let predicate = [
        equals("session_name", session),
        equals(tags::ORGANIZATION, organization),
        equals(tags::WINDOW, FOCUS_WINDOW_ID),
        equals("pane_dead", "0"),
        equals(tags::SIDEBAR, ""),
        equals(tags::PERSON, ""),
        equals(tags::ASLEEP, ""),
        equals(tags::WAKING_PERSON, ""),
        equals(tags::MINTING, ""),
        equals(tags::SLEEPING_PERSON, person_id),
    ]
    .into_iter()
    .rev()
    .reduce(|right, left| and(left, right))
    .unwrap_or_default();
    let reply =
        tmux.run(&["if-shell", "-F", "-t", pane, &predicate, &guarded.command_string(), ""]);
    let accepted = reply.lines().any(|line| line.trim() == success)
        || waking_focus_matches(tmux, session, organization, pane, person_id, Some(claim));
    if !accepted {
        restore_sleeping_focus_after_failed_claim(
            tmux,
            session,
            organization,
            pane,
            person_id,
            claim,
        );
    }
    accepted
}

fn restore_sleeping_focus_after_failed_claim(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    pane: &str,
    person_id: &str,
    claim: &str,
) {
    let mut rollback = Batch::new();
    rollback.push(&["set-option", "-p", "-t", pane, tags::SLEEPING_PERSON, person_id]);
    rollback.push(&["set-option", "-p", "-u", "-t", pane, tags::WAKING_PERSON]);
    rollback.push(&["set-option", "-p", "-u", "-t", pane, tags::WAKE_CLAIM]);
    rollback.push(&["set-option", "-p", "-u", "-t", pane, tags::WAKING_PENDING]);
    rollback.push(&["set-option", "-p", "-u", "-t", pane, tags::WAKING_DESIRED_SEEN]);
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let predicate = [
        equals("session_name", session),
        equals(tags::ORGANIZATION, organization),
        equals(tags::WINDOW, FOCUS_WINDOW_ID),
        equals("pane_dead", "0"),
        equals(tags::SIDEBAR, ""),
        equals(tags::PERSON, ""),
        equals(tags::ASLEEP, ""),
        equals(tags::SLEEPING_PERSON, ""),
        equals(tags::MINTING, ""),
        equals(tags::WAKING_PERSON, person_id),
        equals(tags::WAKE_CLAIM, claim),
    ]
    .into_iter()
    .rev()
    .reduce(|right, left| and(left, right))
    .unwrap_or_default();
    let _ = tmux.run(&["if-shell", "-F", "-t", pane, &predicate, &rollback.command_string(), ""]);
}

/// Change an exact sleeping card into the visible waking frame when another
/// backend client requested the wake. This does not publish a wake request.
#[must_use]
pub fn promote_sleeping_focus(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    pane: &str,
    person_id: &str,
    name: &str,
) -> bool {
    let claim = uuid::Uuid::new_v4().simple().to_string();
    let success = format!("chief-external-wake:{}:{}", pane, now_millis());
    let script = notice_script(&format!("{} is starting…", plain_message(name)));
    let mut guarded = Batch::new();
    guarded.push(&["set-option", "-p", "-t", pane, tags::WAKING_PERSON, person_id]);
    guarded.push(&["set-option", "-p", "-t", pane, tags::WAKE_CLAIM, &claim]);
    guarded.push(&["set-option", "-p", "-t", pane, tags::WAKING_PENDING, &claim]);
    guarded.push(&["set-option", "-p", "-u", "-t", pane, tags::WAKING_DESIRED_SEEN]);
    guarded.push(&["set-option", "-p", "-u", "-t", pane, tags::SLEEPING_PERSON]);
    guarded.push(&["respawn-pane", "-k", "-t", pane, "/bin/sh", "-c", &script]);
    guarded.push(&["display-message", "-p", "-t", pane, &success]);
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let predicate = [
        equals("session_name", session),
        equals(tags::ORGANIZATION, organization),
        equals(tags::WINDOW, FOCUS_WINDOW_ID),
        equals("pane_dead", "0"),
        equals(tags::SIDEBAR, ""),
        equals(tags::PERSON, ""),
        equals(tags::ASLEEP, ""),
        equals(tags::WAKING_PERSON, ""),
        equals(tags::MINTING, ""),
        equals(tags::SLEEPING_PERSON, person_id),
    ]
    .into_iter()
    .rev()
    .reduce(|right, left| and(left, right))
    .unwrap_or_default();
    let reply =
        tmux.run(&["if-shell", "-F", "-t", pane, &predicate, &guarded.command_string(), ""]);
    reply.lines().any(|line| line.trim() == success)
        || waking_focus_matches(tmux, session, organization, pane, person_id, Some(&claim))
}

/// Revalidate one already-accepted card action before acknowledging a duplicate.
#[must_use]
pub fn waking_focus_is_exact(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    pane: &str,
    person_id: &str,
) -> bool {
    waking_focus_matches(tmux, session, organization, pane, person_id, None)
}

fn waking_focus_matches(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    pane: &str,
    person_id: &str,
    claim: Option<&str>,
) -> bool {
    let format = format!(
        concat!(
            "#{{session_name}}\t#{{{}}}\t#{{{}}}\t#{{pane_dead}}\t",
            "#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t",
            "#{{{}}}\t#{{{}}}\t#{{{}}}\tchief-end"
        ),
        tags::ORGANIZATION,
        tags::WINDOW,
        tags::WAKING_PERSON,
        tags::SLEEPING_PERSON,
        tags::PERSON,
        tags::SIDEBAR,
        tags::ASLEEP,
        tags::MINTING,
        tags::WAKE_CLAIM,
        tags::WAKING_PENDING,
        tags::WAKING_DESIRED_SEEN,
    );
    let reply = tmux.run(&["display-message", "-p", "-t", pane, &format]);
    waking_focus_reply_matches(&reply, session, organization, person_id, claim)
}

fn waking_focus_reply_matches(
    reply: &str,
    session: &str,
    organization: &str,
    person_id: &str,
    claim: Option<&str>,
) -> bool {
    let fields = reply.trim_end().split('\t').collect::<Vec<_>>();
    if fields.len() != 14 {
        return false;
    }
    let observed_claim = fields[10];
    fields[0] == session
        && fields[1] == organization
        && fields[2] == FOCUS_WINDOW_ID
        && fields[3] == "0"
        && fields[4] == person_id
        && fields[5..10].iter().all(|field| field.is_empty())
        && !observed_claim.is_empty()
        && claim.is_none_or(|expected| observed_claim == expected)
        && fields[11] == observed_claim
        && (fields[12].is_empty() || fields[12] == observed_claim)
        && fields[13] == "chief-end"
}

/// One exact waking body that was returned to the permanent parked frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedOrphanWaking {
    /// The stable pane that was repainted in place.
    pub pane: String,
    /// The person named by the stale waking marker.
    pub person: String,
}

#[derive(Debug, Clone)]
struct LocalFocusPane {
    session_id: String,
    window: String,
    pane: String,
    pid: String,
    width: String,
    dead: bool,
    window_panes: String,
    pane_options: BTreeMap<String, String>,
    window_options: BTreeMap<String, String>,
    session_options: BTreeMap<String, String>,
}

impl LocalFocusPane {
    fn pane_value(&self, name: &str) -> Option<&str> {
        self.pane_options.get(name).map(String::as_str)
    }

    fn pane_absent(&self, name: &str) -> bool {
        !self.pane_options.contains_key(name)
    }

    fn exact_scope(&self, organization: &str) -> bool {
        self.session_options.get(tags::ORGANIZATION).map(String::as_str) == Some(organization)
            && self.window_options.get(tags::ORGANIZATION).map(String::as_str) == Some(organization)
            && self.window_options.get(tags::WINDOW).map(String::as_str) == Some(FOCUS_WINDOW_ID)
            && !self.window_options.contains_key(WAKING_RECOVERY_READY)
            && !self.dead
            && self.pid.parse::<u32>().ok().filter(|pid| *pid > 0).is_some()
            && self.width.parse::<u32>().ok().filter(|width| *width > 0).is_some()
    }

    fn exact_waking(&self) -> Option<(&str, &str)> {
        let person = self.pane_value(tags::WAKING_PERSON)?;
        let claim = self.pane_value(tags::WAKE_CLAIM)?;
        if person.is_empty()
            || claim.is_empty()
            || [
                tags::ORGANIZATION,
                tags::WINDOW,
                tags::SIDEBAR,
                tags::PERSON,
                tags::LAUNCH_HASH,
                tags::ASLEEP,
                tags::MINTING,
                tags::SLEEPING_PERSON,
                WAKING_RECOVERY_READY,
            ]
            .iter()
            .any(|tag| !self.pane_absent(tag))
        {
            return None;
        }
        Some((person, claim))
    }
}

fn parse_local_scope(reply: &str) -> Option<LocalFocusPane> {
    enum Section {
        None,
        Pane,
        Window,
        Session,
    }
    let mut identity = None;
    let mut pane_options = BTreeMap::new();
    let mut window_options = BTreeMap::new();
    let mut session_options = BTreeMap::new();
    let mut section = Section::None;
    let mut ended = false;
    for line in reply.lines() {
        let line = line.trim();
        if let Some(fields) = line.strip_prefix("chief-waking-scope\t") {
            if identity.is_some() {
                return None;
            }
            let fields = fields.split('\t').collect::<Vec<_>>();
            let [session_id, window, pane, pid, width, dead, window_panes] = fields.as_slice()
            else {
                return None;
            };
            if !matches!(*dead, "0" | "1") {
                return None;
            }
            identity = Some((
                (*session_id).to_owned(),
                (*window).to_owned(),
                (*pane).to_owned(),
                (*pid).to_owned(),
                (*width).to_owned(),
                *dead == "1",
                (*window_panes).to_owned(),
            ));
            continue;
        }
        section = match line {
            "chief-waking-pane-options" => Section::Pane,
            "chief-waking-window-options" => Section::Window,
            "chief-waking-session-options" => Section::Session,
            "chief-waking-options-end" => {
                ended = true;
                Section::None
            }
            _ => {
                if !line.starts_with('@') {
                    continue;
                }
                let (name, value) = line.split_once(' ')?;
                let value = value.trim().trim_matches('"').to_owned();
                let map = match section {
                    Section::Pane => &mut pane_options,
                    Section::Window => &mut window_options,
                    Section::Session => &mut session_options,
                    Section::None => return None,
                };
                if map.insert(name.to_owned(), value).is_some() {
                    return None;
                }
                continue;
            }
        };
    }
    let (session_id, window, pane, pid, width, dead, window_panes) = identity?;
    if !ended
        || !session_id.starts_with('$')
        || !window.starts_with('@')
        || !pane.starts_with('%')
        || dead && pid == "0"
    {
        return None;
    }
    Some(LocalFocusPane {
        session_id,
        window,
        pane,
        pid,
        width,
        dead,
        window_panes,
        pane_options,
        window_options,
        session_options,
    })
}

fn local_focus_panes(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
) -> Option<Vec<LocalFocusPane>> {
    let focus = local_focus_scope(tmux, session, organization)?;
    if focus.len() != 2
        || focus.iter().any(|pane| {
            pane.window != focus[0].window
                || pane.session_id != focus[0].session_id
                || pane.window_panes != "2"
        })
    {
        return None;
    }
    Some(focus)
}

/// Every pane in every fully and locally owned focus window.
///
/// A caller gets no partial answer. One unreadable pane, one inherited window
/// tag, or one foreign scope makes the whole observation unusable.
fn local_focus_scope(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
) -> Option<Vec<LocalFocusPane>> {
    let listed = tmux.run(&["list-panes", "-s", "-t", session, "-F", "#{pane_id}"]);
    let pane_ids =
        listed.lines().map(str::trim).filter(|pane| !pane.is_empty()).collect::<Vec<_>>();
    if pane_ids.is_empty() {
        return None;
    }
    let mut focus = Vec::new();
    for pane in pane_ids {
        if !pane.starts_with('%') {
            return None;
        }
        let reply = tmux.run(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "chief-waking-scope\t#{session_id}\t#{window_id}\t#{pane_id}\t#{pane_pid}\t#{pane_width}\t#{pane_dead}\t#{window_panes}",
            ";",
            "display-message",
            "-p",
            "-t",
            pane,
            "chief-waking-pane-options",
            ";",
            "show-options",
            "-p",
            "-t",
            pane,
            ";",
            "display-message",
            "-p",
            "-t",
            pane,
            "chief-waking-window-options",
            ";",
            "show-options",
            "-w",
            "-t",
            pane,
            ";",
            "display-message",
            "-p",
            "-t",
            pane,
            "chief-waking-session-options",
            ";",
            "show-options",
            "-t",
            pane,
            ";",
            "display-message",
            "-p",
            "-t",
            pane,
            "chief-waking-options-end",
        ]);
        let snapshot = parse_local_scope(&reply)?;
        if snapshot.pane != pane {
            return None;
        }
        let logical = snapshot.window_options.get(tags::WINDOW).map(String::as_str);
        if logical == Some(FOCUS_WINDOW_ID) {
            if !snapshot.exact_scope(organization) {
                return None;
            }
            focus.push(snapshot);
        }
    }
    Some(focus)
}

fn clean_focus_rail(pane: &LocalFocusPane) -> bool {
    pane.pane_value(tags::SIDEBAR) == Some("1")
        && [
            tags::ORGANIZATION,
            tags::WINDOW,
            tags::PERSON,
            tags::LAUNCH_HASH,
            tags::ASLEEP,
            tags::WAKING_PERSON,
            tags::WAKE_CLAIM,
            tags::WAKING_PENDING,
            tags::WAKING_DESIRED_SEEN,
            tags::MINTING,
            tags::SLEEPING_PERSON,
            tags::DEPARTMENT_CARD,
            WAKING_RECOVERY_READY,
        ]
        .iter()
        .all(|tag| pane.pane_absent(tag))
}

fn mark_waking_desired_seen(
    tmux: &dyn Tmux,
    pane: &LocalFocusPane,
    organization: &str,
    person: &str,
    claim: &str,
) -> bool {
    let seen = pane.pane_value(tags::WAKING_DESIRED_SEEN);
    if pane.pane_value(tags::WAKING_PENDING) != Some(claim)
        || seen.is_some_and(|seen| seen != claim)
    {
        return false;
    }
    if seen == Some(claim) {
        return true;
    }
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let predicate = [
        equals("session_id", &pane.session_id),
        equals("window_id", &pane.window),
        equals("window_panes", "2"),
        equals("pane_id", &pane.pane),
        equals("pane_pid", &pane.pid),
        equals("pane_dead", "0"),
        equals(tags::ORGANIZATION, organization),
        equals(tags::WINDOW, FOCUS_WINDOW_ID),
        equals(tags::WAKING_PERSON, person),
        equals(tags::WAKE_CLAIM, claim),
        equals(tags::WAKING_PENDING, claim),
        equals(tags::WAKING_DESIRED_SEEN, ""),
    ]
    .into_iter()
    .rev()
    .reduce(|right, left| and(left, right))
    .unwrap_or_default();
    let mut mark = Batch::new();
    mark.push(&["set-option", "-p", "-t", &pane.pane, tags::WAKING_DESIRED_SEEN, claim]);
    let _ = tmux.run(&["if-shell", "-F", "-t", &pane.pane, &predicate, &mark.command_string(), ""]);
    local_focus_panes(tmux, &pane.session_id, organization).is_some_and(|panes| {
        panes.iter().any(|current| {
            current.pane == pane.pane
                && current.pid == pane.pid
                && current.pane_value(tags::WAKING_PENDING) == Some(claim)
                && current.pane_value(tags::WAKING_DESIRED_SEEN) == Some(claim)
        })
    })
}

fn mark_waking_recovery_ready(tmux: &dyn Tmux, pane: &LocalFocusPane, organization: &str) -> bool {
    match pane.session_options.get(WAKING_RECOVERY_READY).map(String::as_str) {
        Some("1") => return true,
        Some(_) => return false,
        None => {}
    }
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let predicate = format!(
        "#{{&&:{},{}}}",
        equals("session_id", &pane.session_id),
        equals(tags::ORGANIZATION, organization),
    );
    let mut mark = Batch::new();
    mark.push(&["set-option", "-t", &pane.session_id, WAKING_RECOVERY_READY, "1"]);
    let _ = tmux.run(&["if-shell", "-F", "-t", &pane.pane, &predicate, &mark.command_string(), ""]);
    local_focus_panes(tmux, &pane.session_id, organization).is_some_and(|panes| {
        panes[0].session_options.get(WAKING_RECOVERY_READY).map(String::as_str) == Some("1")
    })
}

fn require_local_option(batch: &mut Batch, scope: &str, target: &str, option: &str) {
    let mut command = vec!["show-options"];
    if !scope.is_empty() {
        command.push(scope);
    }
    command.extend(["-t", target, option]);
    batch.push(&command);
}

fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn exact_local_option_shell(scope: &str, target: &str, option: &str, value: &str) -> String {
    let scope = if scope.is_empty() { String::new() } else { format!(" {scope}") };
    format!(
        "test \"$(tmux show-options{scope} -v -t {} {} 2>/dev/null)\" = {}",
        shell_word(target),
        shell_word(option),
        shell_word(value),
    )
}

fn exact_local_user_option_count_shell(scope: &str, target: &str, count: usize) -> String {
    let scope = if scope.is_empty() { String::new() } else { format!(" {scope}") };
    format!(
        "tmux show-options{scope} -t {} 2>/dev/null | awk '$1 ~ /^@/ {{ found++ }} END {{ exit found == {count} ? 0 : 1 }}'",
        shell_word(target),
    )
}

fn probe_absent_local_option(
    probes: &mut Batch,
    cleanup: &mut Batch,
    scope: &str,
    target: &str,
    format_pane: &str,
    option: &str,
    sentinel: &str,
) {
    let mut set = vec!["set-option"];
    if !scope.is_empty() {
        set.push(scope);
    }
    set.extend(["-q", "-o", "-t", target, option, sentinel]);
    probes.push(&set);

    let predicate = format!("#{{==:#{{{option}}},{}}}", super::tmux_static(sentinel));
    let mut unset = Batch::new();
    let mut command = vec!["set-option"];
    if !scope.is_empty() {
        command.push(scope);
    }
    command.extend(["-u", "-t", target, option]);
    unset.push(&command);
    cleanup.push(&["if-shell", "-F", "-t", format_pane, &predicate, &unset.command_string(), ""]);
}

/// The one focus pane carrying a waking claim, if the focus window has one.
///
/// A READ, and only a read: the same local-scope rules `park_orphan_waking_focus`
/// uses to find its candidate, with none of the authority to change anything.
/// It exists so the brain can COUNT how long a claim has gone unseen without
/// the counting itself being able to park, retag, or respawn a pane.
///
/// Returns the pane id, the person, and the claim.
#[must_use]
pub fn unseen_waking_focus(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
) -> Option<(String, String, String)> {
    let panes = local_focus_panes(tmux, session, organization)?;
    let candidate = panes.iter().find(|pane| pane.pane_value(tags::WAKING_PERSON).is_some())?;
    let (person, claim) = candidate.exact_waking()?;
    if candidate.pane_value(tags::WAKING_PENDING).is_some()
        || candidate.pane_value(tags::WAKING_DESIRED_SEEN).is_some()
    {
        return None;
    }
    Some((candidate.pane.clone(), person.to_owned(), claim.to_owned()))
}

/// Return one exact orphan waking body to the permanent generic focus frame.
///
/// New waking furniture carries one shared pending claim. A desired=true read
/// marks that same claim on the pane, and only a later desired=false read may
/// retire it. The session's first exact reconciliation may retire legacy
/// waking furniture with neither marker, before it publishes its guarded
/// adoption marker. This makes startup repair possible without letting another
/// rail process retire a fresh pre-POST wake.
#[must_use]
pub fn park_orphan_waking_focus(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    desired: &BTreeSet<String>,
    live: &BTreeSet<String>,
    unseen_expired: bool,
) -> Option<ParkedOrphanWaking> {
    let panes = local_focus_panes(tmux, session, organization)?;
    let mut rails = panes.iter().filter(|pane| clean_focus_rail(pane));
    let rail = rails.next()?;
    if rails.next().is_some() {
        return None;
    }
    let candidate = panes.iter().find(|pane| pane.pane_value(tags::WAKING_PERSON).is_some());
    let Some(candidate) = candidate else {
        let _ = mark_waking_recovery_ready(tmux, &panes[0], organization);
        return None;
    };
    let (person, claim) = candidate.exact_waking()?;
    if desired.contains(person) || live.contains(person) {
        if desired.contains(person) {
            let marked = mark_waking_desired_seen(tmux, candidate, organization, person, claim);
            if marked {
                let _ = mark_waking_recovery_ready(tmux, candidate, organization);
            }
        }
        return None;
    }
    let ready = match candidate.session_options.get(WAKING_RECOVERY_READY).map(String::as_str) {
        Some("1") => true,
        Some(_) => return None,
        None => false,
    };
    let pending = candidate.pane_value(tags::WAKING_PENDING);
    let seen = candidate.pane_value(tags::WAKING_DESIRED_SEEN);
    let startup_legacy = !ready && pending.is_none() && seen.is_none();
    let withdrawn_shared_claim = ready && pending == Some(claim) && seen == Some(claim);
    // THE ORPHAN THAT OUTLIVED ITS WAKE. A claim that is never observed
    // desired=true never gets a `pending` mark, so on a session that is already
    // recovery-ready it matched neither case above and was refused forever: a
    // live company sat on `… is starting…` for an hour with no process behind
    // it and nothing able to reclaim the pane.
    //
    // The fence that produced that refusal is still right, and is still here.
    // A brand-new wake looks IDENTICAL to this orphan for the first round or
    // two — the difference is only that one of them goes on looking that way.
    // So the decision is not made here: `unseen_expired` is the caller's own
    // count of consecutive rounds in which IT watched THIS pane and THIS claim
    // stay unseen, and a Brain that has not watched a claim that long passes
    // `false` and still refuses. Nothing parks a claim on its first sighting.
    let expired_unseen = unseen_expired && ready && pending.is_none() && seen.is_none();
    if !startup_legacy && !withdrawn_shared_claim && !expired_unseen {
        if !ready && pending == Some(claim) && seen.is_none() {
            let _ = mark_waking_recovery_ready(tmux, candidate, organization);
        }
        return None;
    }
    let pane = candidate.pane.clone();
    let pid = candidate.pid.clone();
    let window = candidate.window.clone();
    let session_id = candidate.session_id.clone();
    let rail_pane = rail.pane.clone();
    let rail_pid = rail.pid.clone();
    let rail_width = rail.width.clone();
    let success = format!("chief-orphan-park:{}:{}", pane, uuid::Uuid::new_v4().simple());
    let sentinel = format!("chief-orphan-local:{}", uuid::Uuid::new_v4().simple());
    let script = parked_script();
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let body_forbidden = [
        tags::ORGANIZATION,
        tags::WINDOW,
        tags::SIDEBAR,
        tags::PERSON,
        tags::LAUNCH_HASH,
        tags::ASLEEP,
        tags::MINTING,
        tags::SLEEPING_PERSON,
        WAKING_RECOVERY_READY,
    ];
    let rail_forbidden = [
        tags::ORGANIZATION,
        tags::WINDOW,
        tags::PERSON,
        tags::LAUNCH_HASH,
        tags::ASLEEP,
        tags::WAKING_PERSON,
        tags::WAKE_CLAIM,
        tags::WAKING_PENDING,
        tags::WAKING_DESIRED_SEEN,
        tags::MINTING,
        tags::SLEEPING_PERSON,
        WAKING_RECOVERY_READY,
    ];
    let mut guard = Batch::new();
    require_local_option(&mut guard, "", &session_id, tags::ORGANIZATION);
    require_local_option(&mut guard, "-w", &window, tags::ORGANIZATION);
    require_local_option(&mut guard, "-w", &window, tags::WINDOW);
    require_local_option(&mut guard, "-p", &rail_pane, tags::SIDEBAR);
    require_local_option(&mut guard, "-p", &pane, tags::WAKING_PERSON);
    require_local_option(&mut guard, "-p", &pane, tags::WAKE_CLAIM);
    if ready {
        require_local_option(&mut guard, "", &session_id, WAKING_RECOVERY_READY);
    }
    // Require only what this pane actually carries. An expired orphan is ready
    // WITHOUT a pending mark, which is the whole shape that was unreachable.
    if pending.is_some() {
        require_local_option(&mut guard, "-p", &pane, tags::WAKING_PENDING);
    }
    if seen.is_some() {
        require_local_option(&mut guard, "-p", &pane, tags::WAKING_DESIRED_SEEN);
    }
    let mut probes = Batch::new();
    let mut cleanup = Batch::new();
    for option in body_forbidden {
        probe_absent_local_option(&mut probes, &mut cleanup, "-p", &pane, &pane, option, &sentinel);
    }
    for option in rail_forbidden {
        probe_absent_local_option(
            &mut probes,
            &mut cleanup,
            "-p",
            &rail_pane,
            &rail_pane,
            option,
            &sentinel,
        );
    }
    probe_absent_local_option(
        &mut probes,
        &mut cleanup,
        "-w",
        &window,
        &pane,
        WAKING_RECOVERY_READY,
        &sentinel,
    );
    // An absent tag needs its sentinel fallback so the predicate above can ask
    // "still absent?" at all. Keyed on the tag's OWN absence, not on readiness:
    // an expired orphan is ready and has neither mark, and keying this on
    // `!ready` left its predicate comparing against a sentinel nothing had set,
    // so the reclaim could never match and the pane stayed stuck. When `!ready`
    // both marks are absent anyway, so the startup path is unchanged.
    if pending.is_none() {
        probe_absent_local_option(
            &mut probes,
            &mut cleanup,
            "-p",
            &pane,
            &pane,
            tags::WAKING_PENDING,
            &sentinel,
        );
    }
    if seen.is_none() {
        probe_absent_local_option(
            &mut probes,
            &mut cleanup,
            "-p",
            &pane,
            &pane,
            tags::WAKING_DESIRED_SEEN,
            &sentinel,
        );
    }
    if !ready {
        probe_absent_local_option(
            &mut probes,
            &mut cleanup,
            "",
            &session_id,
            &pane,
            WAKING_RECOVERY_READY,
            &sentinel,
        );
    }
    let mut parked = Batch::new();
    parked.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PERSON]);
    parked.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKE_CLAIM]);
    parked.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PENDING]);
    parked.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_DESIRED_SEEN]);
    parked.push(&["set-option", "-p", "-t", &pane, tags::ASLEEP, FOCUS_WINDOW_ID]);
    parked.push(&["set-option", "-p", "-u", "-t", &pane, "pane-border-format"]);
    parked.push(&["rename-window", "-t", &window, PARKED_WINDOW_NAME]);
    parked.push(&["display-message", "-p", "-t", &pane, &success]);
    let replaced = and(
        equals("pane_dead", "0"),
        format!("#{{!=:#{{pane_pid}},{}}}", super::tmux_static(&pid)),
    );
    let mut replace = Batch::new();
    for option in body_forbidden {
        replace.push(&["set-option", "-p", "-u", "-t", &pane, option]);
    }
    for option in rail_forbidden {
        replace.push(&["set-option", "-p", "-u", "-t", &rail_pane, option]);
    }
    replace.push(&["set-option", "-w", "-u", "-t", &window, WAKING_RECOVERY_READY]);
    if !ready {
        replace.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PENDING]);
        replace.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_DESIRED_SEEN]);
        replace.push(&["set-option", "-u", "-t", &session_id, WAKING_RECOVERY_READY]);
    }
    replace.push(&["respawn-pane", "-k", "-t", &pane, "/bin/sh", "-c", &script]);
    replace.push(&["if-shell", "-F", "-t", &pane, &replaced, &parked.command_string(), ""]);
    let mut predicate = vec![
        equals("session_id", &session_id),
        equals("window_id", &window),
        equals("window_panes", "2"),
        equals("pane_id", &pane),
        equals("pane_pid", &pid),
        equals("pane_dead", "0"),
        equals(tags::WAKING_PERSON, person),
        equals(tags::WAKE_CLAIM, claim),
    ];
    predicate.extend(body_forbidden.into_iter().map(|option| equals(option, &sentinel)));
    // Present tags are pinned to their exact observed value; absent ones must
    // still be absent. The readiness marker is session state and is pane-local
    // ABSENT in every case, which both previous branches already agreed on.
    predicate.extend([
        pending.map_or_else(
            || equals(tags::WAKING_PENDING, &sentinel),
            |value| equals(tags::WAKING_PENDING, value),
        ),
        seen.map_or_else(
            || equals(tags::WAKING_DESIRED_SEEN, &sentinel),
            |value| equals(tags::WAKING_DESIRED_SEEN, value),
        ),
        equals(WAKING_RECOVERY_READY, &sentinel),
    ]);
    let predicate =
        predicate.into_iter().rev().reduce(|right, left| and(left, right)).unwrap_or_default();
    let body_guard = {
        let mut body = Batch::new();
        body.push(&["if-shell", "-F", "-t", &pane, &predicate, &replace.command_string(), ""]);
        body
    };
    let mut rail_predicate = vec![
        equals("session_id", &session_id),
        equals("window_id", &window),
        equals("window_panes", "2"),
        equals("pane_id", &rail_pane),
        equals("pane_pid", &rail_pid),
        equals("pane_width", &rail_width),
        equals("pane_dead", "0"),
        equals(tags::SIDEBAR, "1"),
    ];
    rail_predicate.extend(rail_forbidden.into_iter().map(|option| equals(option, &sentinel)));
    let rail_predicate =
        rail_predicate.into_iter().rev().reduce(|right, left| and(left, right)).unwrap_or_default();
    probes.push(&[
        "if-shell",
        "-F",
        "-t",
        &rail_pane,
        &rail_predicate,
        &body_guard.command_string(),
        "",
    ]);
    // tmux limits a parsed command string. Keep the exact absence transaction
    // in one unique global option, then expand and run it in this same tmux
    // command queue only after both local-scope predicates pass.
    let command_option = format!("@chief_orphan_commands_{}", uuid::Uuid::new_v4().simple());
    let _ = tmux.run(&["set-option", "-g", &command_option, &probes.command_string()]);
    let mut invoke_probes = Batch::new();
    invoke_probes.push(&["run-shell", "-C", &format!("#{{{command_option}}}")]);
    let mut body_scope = vec![
        equals("session_id", &session_id),
        equals("window_id", &window),
        equals("window_panes", "2"),
        equals("pane_id", &pane),
        equals("pane_pid", &pid),
        equals("pane_dead", "0"),
        equals(tags::ORGANIZATION, organization),
        equals(tags::WINDOW, FOCUS_WINDOW_ID),
        equals(tags::WAKING_PERSON, person),
        equals(tags::WAKE_CLAIM, claim),
    ];
    // Scope reads INHERITED values, so an absent tag reads as empty. The
    // readiness marker is the session's, and is the one value `ready` decides.
    body_scope.extend([
        equals(tags::WAKING_PENDING, pending.unwrap_or_default()),
        equals(tags::WAKING_DESIRED_SEEN, seen.unwrap_or_default()),
        equals(WAKING_RECOVERY_READY, if ready { "1" } else { "" }),
    ]);
    let body_scope =
        body_scope.into_iter().rev().reduce(|right, left| and(left, right)).unwrap_or_default();
    let body_scope_guard = {
        let mut body = Batch::new();
        body.push(&[
            "if-shell",
            "-F",
            "-t",
            &pane,
            &body_scope,
            &invoke_probes.command_string(),
            "",
        ]);
        body
    };
    let rail_scope = [
        equals("session_id", &session_id),
        equals("window_id", &window),
        equals("window_panes", "2"),
        equals("pane_id", &rail_pane),
        equals("pane_pid", &rail_pid),
        equals("pane_width", &rail_width),
        equals("pane_dead", "0"),
        equals(tags::ORGANIZATION, organization),
        equals(tags::WINDOW, FOCUS_WINDOW_ID),
        equals(tags::SIDEBAR, "1"),
    ]
    .into_iter()
    .rev()
    .reduce(|right, left| and(left, right))
    .unwrap_or_default();
    let mut scoped_guard = Batch::new();
    scoped_guard.push(&[
        "if-shell",
        "-F",
        "-t",
        &rail_pane,
        &rail_scope,
        &body_scope_guard.command_string(),
        "",
    ]);
    let mut exact_session_values =
        vec![exact_local_option_shell("", &session_id, tags::ORGANIZATION, organization)];
    if ready {
        exact_session_values.push(exact_local_option_shell(
            "",
            &session_id,
            WAKING_RECOVERY_READY,
            "1",
        ));
    }
    guard.push(&[
        "if-shell",
        "-t",
        &pane,
        &exact_session_values.join(" && "),
        &scoped_guard.command_string(),
        "",
    ]);
    guard.run(tmux);
    let _ = tmux.run(&["set-option", "-gu", &command_option]);
    cleanup.run(tmux);
    let applied = parked_focus_matches(tmux, organization, candidate, rail);
    if applied && !ready {
        let rebound = local_focus_panes(tmux, &session_id, organization)?;
        if !mark_waking_recovery_ready(tmux, &rebound[0], organization) {
            return None;
        }
    }
    applied.then_some(ParkedOrphanWaking { pane, person: person.to_owned() })
}

fn parked_focus_matches(
    tmux: &dyn Tmux,
    organization: &str,
    prior_body: &LocalFocusPane,
    prior_rail: &LocalFocusPane,
) -> bool {
    local_focus_panes(tmux, &prior_body.session_id, organization).is_some_and(|panes| {
        let body = panes.iter().any(|current| {
            current.session_id == prior_body.session_id
                && current.window == prior_body.window
                && current.pane == prior_body.pane
                && current.pid != prior_body.pid
                && current.pane_value(tags::ASLEEP) == Some(FOCUS_WINDOW_ID)
                && [
                    tags::ORGANIZATION,
                    tags::WINDOW,
                    tags::SIDEBAR,
                    tags::PERSON,
                    tags::LAUNCH_HASH,
                    tags::WAKING_PERSON,
                    tags::WAKE_CLAIM,
                    tags::WAKING_PENDING,
                    tags::WAKING_DESIRED_SEEN,
                    tags::MINTING,
                    tags::SLEEPING_PERSON,
                ]
                .iter()
                .all(|tag| current.pane_absent(tag))
        });
        let rail = panes.iter().any(|current| {
            current.pane == prior_rail.pane
                && current.pid == prior_rail.pid
                && current.width == prior_rail.width
                && clean_focus_rail(current)
        });
        body && rail
    })
}

/// Return an unclaimed cold-click body to the permanent generic focus frame.
pub fn park_waking_focus(tmux: &dyn Tmux, session: &str, person_id: &str) {
    let Some(window) = department_window(tmux, session, FOCUS_WINDOW_ID) else { return };
    let listed = tmux.run(&[
        "list-panes",
        "-t",
        &window,
        "-F",
        &format!("#{{pane_id}}\t#{{{}}}", tags::WAKING_PERSON),
    ]);
    let Some(pane) = listed.lines().find_map(|line| {
        let (pane, waking) = line.split_once('\t')?;
        (waking.trim() == person_id).then(|| pane.trim().to_owned())
    }) else {
        return;
    };
    let script = parked_script();
    let mut batch = Batch::new();
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PERSON]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKE_CLAIM]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PENDING]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_DESIRED_SEEN]);
    batch.push(&["set-option", "-p", "-t", &pane, tags::ASLEEP, FOCUS_WINDOW_ID]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, "pane-border-format"]);
    batch.push(&["rename-window", "-t", &window, PARKED_WINDOW_NAME]);
    batch.push(&["respawn-pane", "-k", "-t", &pane, "/bin/sh", "-c", &script]);
    batch.run(tmux);
}

/// Return a sleeping card to the permanent generic focus frame when the
/// operator selects something else.
pub fn park_sleeping_focus(tmux: &dyn Tmux, session: &str, person_id: &str) {
    let Some(window) = department_window(tmux, session, FOCUS_WINDOW_ID) else { return };
    let listed = tmux.run(&[
        "list-panes",
        "-t",
        &window,
        "-F",
        &format!("#{{pane_id}}\t#{{{}}}", tags::SLEEPING_PERSON),
    ]);
    let Some(pane) = listed.lines().find_map(|line| {
        let (pane, sleeping) = line.split_once('\t')?;
        (sleeping.trim() == person_id).then(|| pane.trim().to_owned())
    }) else {
        return;
    };
    let script = parked_script();
    let mut batch = Batch::new();
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::SLEEPING_PERSON]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKE_CLAIM]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_PENDING]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, tags::WAKING_DESIRED_SEEN]);
    batch.push(&["set-option", "-p", "-t", &pane, tags::ASLEEP, FOCUS_WINDOW_ID]);
    batch.push(&["set-option", "-p", "-u", "-t", &pane, "pane-border-format"]);
    batch.push(&["rename-window", "-t", &window, PARKED_WINDOW_NAME]);
    batch.push(&["respawn-pane", "-k", "-t", &pane, "/bin/sh", "-c", &script]);
    batch.run(tmux);
}

/// What the parked focus window says.
///
/// Honest and actionable, and specifically NOT an indefinite `…`: nothing is
/// pending here. The window is empty because the operator is looking at a
/// department, and the one gesture that fills it is named.
fn parked_script() -> String {
    notice_script("Click a person in the sidebar to see them here alone.")
}

/// Everything the permanent focus window needs to be minted and kept furnished.
pub struct Parked<'a> {
    /// The company slug used for the `@organization_id` tag.
    pub organization: &'a str,
    /// The program the focus window's own rail runs as `<program> sidebar`.
    ///
    /// **AN INPUT, NEVER `std::env::current_exe()` READ HERE.** It was that once,
    /// and it turned this module's own live test into a FORK BOMB: under `cargo
    /// test` the current executable is the TEST BINARY, so minting a rail spawned
    /// the whole suite in a tmux pane, which reached the same test and spawned it
    /// again. Measured: 31 tmux servers and a rising tree of test processes from
    /// one run. An effect that discovers its own executable cannot be driven
    /// safely by a test, and every effect in this module is deliberately
    /// driveable — see the module doc.
    ///
    /// `None` mints no rail rather than guessing, and says so. This is now the
    /// LAST place a rail is minted by a click-path module at all: the mint runs
    /// once per session off the company read, and no gesture boots a process.
    pub rail_program: Option<&'a str>,
    /// The company root used as the rail process working directory.
    pub company_dir: &'a std::path::Path,
}

/// THE SESSION HAS EXACTLY ONE FOCUS WINDOW, IT IS MINTED ONCE, AND IT NEVER
/// STANDS EMPTY.
///
/// # What this replaces
///
/// The focus window used to be minted by a person click (`break-pane` into a
/// fresh window, plus a rail process booted into it) and destroyed by the next
/// department click (`kill-window`, killing that rail). Every navigation was
/// therefore a topology mutation: a window appeared or vanished, every window
/// index after it shifted, and every pane tmux re-laid on the way got a
/// SIGWINCH it could only answer as fast as the program inside it — which, for a
/// Pi parked on a synchronous read, is seconds. That is §1c of
/// the design record, and this function is what removes its cause.
///
/// # Why it holds a notice rather than standing empty
///
/// tmux destroys a window only when its LAST pane goes, so an "empty" focus
/// window is a window holding its rail — and tmux gives a lone pane the whole
/// window, which is precisely the "the side panel is full screen and the right
/// side is blank" the operator reported. So the parked window holds a standing
/// notice beside the rail, and `never_blank` no longer has to reason about this
/// window at all: it is non-blank by construction.
///
/// The notice carries [`tags::ASLEEP`] with the focus window's own logical id.
/// That tag means "this pane is a notice, and its value says what about" — a
/// department id for a sleeping department, `__focus__` for this. Everything
/// that already acts on the tag then does the right thing with no new case:
/// [`close_sleeping_notices`] never matches it (`__focus__` is not a
/// department and so is never a LIVE department), and `converge`'s
/// actuator layout closes it the moment a person is laid out here.
///
/// Called on every company read, which is off the click path. Answers the
/// window, or `None` when tmux would not mint one.
pub fn ensure_focus_window(tmux: &dyn Tmux, session: &str, parked: &Parked) -> Option<String> {
    let mut windows = department_windows(tmux, session, FOCUS_WINDOW_ID);
    if windows.len() > 1 {
        let window = repair_duplicate_focus_windows(tmux, session, parked.organization, &windows)?;
        windows = vec![window];
    }
    let Some(window) = windows.into_iter().next() else {
        let canonical = canonical_geometry(tmux, session);
        return mint_parked_focus_window(tmux, session, parked, canonical);
    };
    // FURNISHED ALREADY? THEN NOTHING AT ALL — the rule `show_department_overview`
    // learned the hard way: an effect reached from the refresh path may only fire
    // on a TRANSITION, because this runs on every changefeed wake and a company
    // with one chatty agent wakes it many times a second. Three reads, one per
    // kind of furniture, and never a `display-message` per pane.
    //
    // ONE SNAPSHOT, INCLUDING UNKNOWN BODIES. A freshly split person pane is
    // born before its MINTING and PERSON tags are written. Treating an
    // untagged non-rail body as "empty" made refresh split generic furniture
    // beside it during that publication gap. Unknown is not empty; it fails
    // closed and lets converge finish or quarantine it.
    let panes = focus_pane_snapshot(tmux, &window);
    if panes.iter().any(|(_, is_rail)| !*is_rail) {
        return Some(window);
    }
    let Some(rail) = panes.iter().find(|(_, is_rail)| *is_rail).map(|(pane, _)| pane.as_str())
    else {
        return Some(window);
    };
    let Some(park) = park_beside(tmux, session, &panes, None, Side::Before) else {
        return Some(window);
    };
    let Some((_pane, topology_generation)) =
        park_focus_window_if_still_empty(tmux, session, &window, rail, park)
    else {
        // A person or WAKING pane appeared after the snapshot. The write-time
        // guard changed nothing, which is the successful concurrent outcome.
        return Some(window);
    };
    relay(tmux, session, &window);
    refresh_viewport_topology(tmux, session, &topology_generation);
    tracing::info!(
        event = "sidebar.focus.parked",
        session,
        window = %window,
        "the focus window held nothing but its rail; its standing notice is back, so it \
         cannot go blank and the rail cannot inherit the whole window"
    );
    Some(window)
}

#[derive(Clone, Copy)]
enum FocusFurniture {
    Rail,
    Notice,
}

fn focus_furniture(pane: &LocalFocusPane) -> Option<FocusFurniture> {
    let rail_options = BTreeMap::from([(tags::SIDEBAR.to_owned(), "1".to_owned())]);
    if clean_focus_rail(pane) && pane.pane_options == rail_options {
        return Some(FocusFurniture::Rail);
    }
    let notice_options = BTreeMap::from([(tags::ASLEEP.to_owned(), FOCUS_WINDOW_ID.to_owned())]);
    (pane.pane_options == notice_options).then_some(FocusFurniture::Notice)
}

/// Repair the old check-then-mint race, but only where every removed byte is
/// proven to be disposable focus furniture.
fn repair_duplicate_focus_windows(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    observed: &[String],
) -> Option<String> {
    let panes = local_focus_scope(tmux, session, organization)?;
    let mut by_window = BTreeMap::<String, Vec<&LocalFocusPane>>::new();
    for pane in &panes {
        by_window.entry(pane.window.clone()).or_default().push(pane);
    }
    let observed = observed.iter().cloned().collect::<BTreeSet<_>>();
    if by_window.keys().cloned().collect::<BTreeSet<_>>() != observed {
        return None;
    }

    let mut removable = BTreeSet::new();
    let mut non_removable = BTreeSet::new();
    let mut active = BTreeSet::new();
    for (window, panes) in &by_window {
        let expected = panes.len().to_string();
        let exact_window_options = BTreeMap::from([
            (tags::ORGANIZATION.to_owned(), organization.to_owned()),
            (tags::WINDOW.to_owned(), FOCUS_WINDOW_ID.to_owned()),
        ]);
        let roles = panes.iter().map(|pane| focus_furniture(pane)).collect::<Option<Vec<_>>>();
        let exact_count = !panes.is_empty()
            && panes.iter().all(|pane| pane.window_panes == expected)
            && panes.iter().all(|pane| pane.window_options == exact_window_options)
            && roles.as_ref().is_some_and(|roles| {
                roles.iter().filter(|role| matches!(role, FocusFurniture::Notice)).count() == 1
                    && roles.iter().filter(|role| matches!(role, FocusFurniture::Rail)).count() == 1
            });
        let active_read = tmux.run(&["display-message", "-p", "-t", window, "#{window_active}"]);
        match active_read.trim() {
            "0" => {
                if exact_count {
                    removable.insert(window.clone());
                } else {
                    non_removable.insert(window.clone());
                }
            }
            "1" => {
                active.insert(window.clone());
                if exact_count {
                    removable.insert(window.clone());
                } else {
                    non_removable.insert(window.clone());
                }
            }
            _ => return None,
        }
    }
    if active.len() > 1 {
        return None;
    }
    let keeper = if let Some(active) = active.into_iter().next() {
        if observed.iter().any(|window| window != &active && !removable.contains(window)) {
            return None;
        }
        active
    } else if non_removable.len() == 1 {
        non_removable.into_iter().next()?
    } else if non_removable.is_empty() {
        removable
            .iter()
            .filter_map(|window| {
                window.strip_prefix('@')?.parse::<u64>().ok().map(|id| (id, window))
            })
            .min_by_key(|(id, _)| *id)
            .map(|(_, window)| window.clone())?
    } else {
        return None;
    };
    for window in observed.iter().filter(|window| **window != keeper) {
        if !removable.contains(window)
            || !kill_duplicate_focus_furniture(
                tmux,
                session,
                organization,
                &keeper,
                window,
                by_window.get(window)?,
            )
        {
            return None;
        }
    }
    let remaining = department_windows(tmux, session, FOCUS_WINDOW_ID);
    if remaining != [keeper.clone()] {
        return None;
    }
    tracing::warn!(
        event = "sidebar.focus.duplicates.repaired",
        session,
        keeper = %keeper,
        observed = ?observed,
        "removed duplicate focus windows contained only exact parked furniture; inactive \
         duplicates were repaired before the next actuator plan"
    );
    Some(keeper)
}

fn kill_duplicate_focus_furniture(
    tmux: &dyn Tmux,
    session: &str,
    organization: &str,
    keeper: &str,
    window: &str,
    panes: &[&LocalFocusPane],
) -> bool {
    let generation = invalidate_viewport_topology(tmux, session);
    let Some(generation) = generation else { return false };
    let success = format!("chief-focus-duplicate-reaped:{}", uuid::Uuid::new_v4().simple());
    let equals =
        |field: &str, value: &str| format!("#{{==:#{{{field}}},{}}}", super::tmux_static(value));
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let mut action = Batch::new();
    action.push(&["kill-window", "-t", window]);
    action.push(&["display-message", "-p", "-t", session, &success]);
    let expected = panes.len().to_string();
    for pane in panes.iter().rev() {
        let Some(role) = focus_furniture(pane) else {
            refresh_viewport_topology(tmux, session, &generation);
            return false;
        };
        let (sidebar, asleep) = match role {
            FocusFurniture::Rail => ("1", ""),
            FocusFurniture::Notice => ("", FOCUS_WINDOW_ID),
        };
        let predicate = [
            equals("session_id", &pane.session_id),
            equals("window_id", window),
            equals("window_active", "0"),
            equals("window_panes", &expected),
            equals("pane_id", &pane.pane),
            equals("pane_pid", &pane.pid),
            equals("pane_dead", "0"),
            equals(tags::ORGANIZATION, organization),
            equals(tags::WINDOW, FOCUS_WINDOW_ID),
            equals(tags::SIDEBAR, sidebar),
            equals(tags::ASLEEP, asleep),
            equals(tags::PERSON, ""),
            equals(tags::LAUNCH_HASH, ""),
            equals(tags::WAKING_PERSON, ""),
            equals(tags::WAKE_CLAIM, ""),
            equals(tags::WAKING_PENDING, ""),
            equals(tags::WAKING_DESIRED_SEEN, ""),
            equals(tags::MINTING, ""),
            equals(tags::SLEEPING_PERSON, ""),
            equals(tags::DEPARTMENT_CARD, ""),
        ]
        .into_iter()
        .rev()
        .reduce(|right, left| and(left, right));
        let Some(predicate) = predicate else {
            refresh_viewport_topology(tmux, session, &generation);
            return false;
        };
        let mut guarded = Batch::new();
        guarded.push(&[
            "if-shell",
            "-F",
            "-t",
            &pane.pane,
            &predicate,
            &action.command_string(),
            "",
        ]);
        action = guarded;
    }
    // The keeper is part of the mutation boundary too. If it stops being this
    // company's focus window after selection, deleting another focus window
    // can remove the last valid candidate. The server must refuse that stale
    // repair and let the next pass take a new snapshot.
    let local_scope = [
        exact_local_option_shell("", &panes[0].session_id, tags::ORGANIZATION, organization),
        exact_local_option_shell("-w", keeper, tags::ORGANIZATION, organization),
        exact_local_option_shell("-w", keeper, tags::WINDOW, FOCUS_WINDOW_ID),
        exact_local_option_shell("-w", window, tags::ORGANIZATION, organization),
        exact_local_option_shell("-w", window, tags::WINDOW, FOCUS_WINDOW_ID),
        exact_local_user_option_count_shell("-w", window, 2),
    ]
    .into_iter()
    .chain(panes.iter().flat_map(|pane| {
        let (option, value) = match focus_furniture(pane) {
            Some(FocusFurniture::Rail) => (tags::SIDEBAR, "1"),
            Some(FocusFurniture::Notice) => (tags::ASLEEP, FOCUS_WINDOW_ID),
            None => (tags::PERSON, "__never__"),
        };
        [
            exact_local_option_shell("-p", &pane.pane, option, value),
            exact_local_user_option_count_shell("-p", &pane.pane, 1),
        ]
    }))
    .collect::<Vec<_>>()
    .join(" && ");
    let output =
        tmux.run(&["if-shell", "-t", &panes[0].pane, &local_scope, &action.command_string(), ""]);
    refresh_viewport_topology(tmux, session, &generation);
    output.lines().any(|line| line.trim() == success)
}

/// One live focus-pane snapshot. Every non-rail body counts as furnishing,
/// including a pane between process spawn and identity publication.
fn focus_pane_snapshot(tmux: &dyn Tmux, window: &str) -> Vec<(String, bool)> {
    tmux.run(&[
        "list-panes",
        "-t",
        window,
        "-F",
        &format!(
            "#{{pane_id}}\t#{{pane_dead}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}\t#{{{}}}",
            tags::SIDEBAR,
            tags::PERSON,
            tags::ASLEEP,
            tags::WAKING_PERSON,
            tags::MINTING
        ),
    ])
    .lines()
    .filter_map(|line| {
        let mut fields = line.split('\t');
        let pane = fields.next()?.trim();
        let dead = fields.next()?.trim();
        let sidebar = fields.next()?.trim();
        // The transport removes trailing whitespace, so absent trailing tags
        // are missing fields rather than explicit empty fields.
        let person = fields.next().unwrap_or_default().trim();
        let asleep = fields.next().unwrap_or_default().trim();
        let waking = fields.next().unwrap_or_default().trim();
        let minting = fields.next().unwrap_or_default().trim();
        // Only one clean, live sidebar is empty. A dead body and a mixed-tag
        // pane are still bodies for this fail-closed decision: neither permits
        // another generic pane beside it.
        (!pane.is_empty()).then(|| {
            let clean_rail = dead != "1"
                && !sidebar.is_empty()
                && person.is_empty()
                && asleep.is_empty()
                && waking.is_empty()
                && minting.is_empty();
            (pane.to_owned(), clean_rail)
        })
    })
    .collect()
}

/// Where the standing notice goes, and why each arm has to be its own case.
///
/// **THE RAIL MUST NOT INHERIT A DEPARTING PANE'S COLUMNS.** tmux hands them to
/// the departing pane's PREVIOUS SIBLING, which in a `{rail, person}` window is
/// the rail — measured on the operator's own box as multi-second dwells at full
/// width, and worse, as a width their sidebar then LATCHED to, because a width
/// the rail is drawn at is a width the rail records.
///
/// **AND WHICH SIDE IS DECIDED BY WHICH OF THE TWO PANES DIES FIRST.** Getting
/// half of this rule — always splitting in FRONT — is what put a 147-column
/// sidebar on the operator's box: the sleeping notice went in as
/// `{rail, notice, ceo}`, and a notice is swept the moment anybody in its
/// department comes up, so its columns fell straight back onto the rail.
/// Measured on their own session, `sidebar.rail.width-recorded 147` 950ms after
/// the company started, and every layout for the rest of the session reproduced
/// it. **The pane that dies first goes on the FAR side, so its columns fall to a
/// pane that is staying.**
#[derive(Debug, Clone, Copy)]
enum Park<'a> {
    /// Beside a pane, on the side [`Side`] chooses.
    Beside {
        /// The pane the new one sits next to. Never the rail.
        sibling: &'a str,
        /// Which of the two outlives the other.
        side: Side,
    },
    /// The window holds nothing but its rail, so there is nothing to split but
    /// the rail. The notice is sized to take everything BEYOND the width the
    /// operator chose, which is the only way a split off the rail leaves the
    /// rail where it was.
    OffTheRail {
        /// The rail pane.
        rail: &'a str,
        /// Its whole window's width.
        window: i64,
        /// The width the operator chose, which the rail keeps.
        columns: i64,
    },
}

/// Which of a new pane and its sibling outlives the other — and therefore which
/// side of the sibling the new pane goes.
///
/// tmux hands a dying pane's columns to its PREVIOUS SIBLING, and the rail is
/// the first pane in every window this product builds. So the only question
/// that matters is which of the two panes dies first: put THAT one on the far
/// side, and its columns land on a pane that is staying instead of on the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The new pane OUTLIVES its sibling — a standing notice beside the person
    /// leaving the focus window, a loading panel replacing a notice, a person
    /// arriving where a notice is about to be killed. `{rail, new, sibling}`,
    /// so the sibling's columns fall to the new pane.
    Before,
    /// The SIBLING outlives the new pane — a sleeping notice, which is swept by
    /// `close_sleeping_notices` the moment anybody in that department comes up.
    /// `{rail, sibling, new}`, so the notice's own columns fall back to the
    /// sibling. Splitting it in front instead is what latched 147 columns.
    After,
}

/// The `split-window` argv for one pane of the rail's own FURNITURE — a standing
/// notice, a sleeping notice or a loading panel — placed by [`Park`] so the rail
/// can never inherit anything, and reporting the pane it made.
///
/// One builder for all three, because all three had the same defect and only one
/// of them had been given the fix.
fn furniture_split(park: Park, script: &str) -> Vec<String> {
    furniture_split_program(park, &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()])
}

/// The furniture split for a real program and direct argv.
fn furniture_split_program(park: Park, program: &[String]) -> Vec<String> {
    let mut argv: Vec<String> =
        ["split-window", "-h", "-d"].iter().map(|arg| (*arg).to_owned()).collect();
    argv.extend(park_argv(park));
    argv.extend(["-P", "-F", "#{pane_id}"].iter().map(|arg| (*arg).to_owned()));
    argv.extend_from_slice(program);
    argv
}

/// **WHICH PANE IS SPLIT, AND HOW ITS COLUMNS ARE DIVIDED.** The flags shared by
/// the two verbs that put a pane into a window of the rail's — `split-window`
/// for the rail's own furniture, and `join-pane` for a person or a panel
/// arriving.
///
/// # The halving this ends
///
/// `join-pane -t <window>` does not target a window in any useful sense: tmux
/// resolves a window target to that window's ACTIVE pane and splits it in half.
/// The active pane of a parked focus window is the RAIL — read straight off the
/// operator's own box, `%5 pane_active=1 pane_width=26` — so every person click
/// halved their sidebar to 13 columns and the layout that followed put it back.
/// Measured in their log as `sidebar.rail.width-recorded` 13 within ~150ms of
/// each `sidebar.person.retargeted` and 26 again ~400ms later, three times in
/// three clicks: `{26: 4, 13: 3}`. 13 is 26 halved, which is the same signature
/// as the 13-column sidebar catastrophe `plausible_rail_width` was built for —
/// and worse than a flicker, because a width the rail is DRAWN at is a width the
/// rail RECORDS, so the halving was one `record_width` away from becoming the
/// session's remembered sidebar.
///
/// `-h` was already here and was only ever half the rule. It fixed the AXIS —
/// a bare join splits top and bottom and halves the rail's HEIGHT — and left
/// the TARGET and the SIZE saying "take half of whatever pane happens to be
/// active". Both verbs now say which pane, and both say how much.
fn park_argv(park: Park) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    let beside = match park {
        Park::Beside { sibling, side } => {
            if side == Side::Before {
                argv.push("-b".to_owned());
            }
            sibling
        }
        Park::OffTheRail { rail, window, columns } => {
            // The new pane takes the columns the rail is NOT keeping, less the
            // one the divider costs. A window too narrow for both is left to the
            // layout that follows.
            let width = window - columns - 1;
            if width >= 1 {
                argv.push("-l".to_owned());
                argv.push(width.to_string());
            }
            rail
        }
    };
    argv.push("-t".to_owned());
    argv.push(beside.to_owned());
    argv
}

/// Where a pane of furniture goes when the caller has a list of the window's
/// panes and one of them is the pane it wants to sit beside.
///
/// `beside` is the pane whose columns the new one should take — a notice about
/// to be killed, or a person about to leave. When there is no such pane the
/// rail is all there is, and the split is SIZED so the rail keeps the width the
/// operator chose.
fn park_beside<'a>(
    tmux: &dyn Tmux,
    session: &str,
    panes: &'a [(String, bool)],
    beside: Option<&'a str>,
    side: Side,
) -> Option<Park<'a>> {
    if let Some(sibling) = beside {
        return Some(Park::Beside { sibling, side });
    }
    let rail = panes.iter().find(|(_, is_rail)| *is_rail).map(|(pane, _)| pane.as_str())?;
    Some(Park::OffTheRail {
        rail,
        window: window_width(tmux, rail),
        columns: rail_columns(tmux, session),
    })
}

/// Split the ONE card body into a card window that holds only its rail.
///
/// # What this is, and what it is NOT
///
/// It is the surviving half of `handoff_occupied_focus`. That function did two
/// jobs: it returned the focus window's LIVE OCCUPANT to their department —
/// minting that department's window around them if it had gone — and, when
/// there was no occupant, it split the card body directly. The first job is
/// deleted with the model that put a live person in this window; the second is
/// still needed, because a rail-only card window is a real state. It is what
/// `ensure_focus_window` leaves between minting the window and the next company
/// read parking it, and a click can land in that gap.
///
/// # Why it needs no write-time CAS, when the parked notice does
///
/// [`park_focus_window_if_still_empty`] guards its split with an `if-shell`
/// predicate because it runs on the REFRESH path, where a pane it did not see
/// can appear underneath it. Nothing else writes into this window any more:
/// converge never places a person here (`placement::desired_topology` does not
/// name it), and the brain is one loop. The snapshot above the split is the
/// whole check.
fn mint_focus_card_body(
    tmux: &dyn Tmux,
    session: &str,
    window: &str,
    card: &FocusCard<'_>,
) -> Option<String> {
    let panes = window_panes(tmux, window);
    if panes.iter().any(|(_, is_rail)| !*is_rail) {
        // A body is already there and the caller could not use it — a
        // half-tagged pane, or somebody else's card mid-write. Refusing leaves
        // it for the next company read rather than putting a second body beside
        // it.
        return None;
    }
    let rail = panes.iter().find(|(_, is_rail)| *is_rail).map(|(pane, _)| pane.clone())?;
    // `Side` is not consulted for a rail-only window — there is no sibling to
    // be on a side of — but the split is still SIZED so the rail keeps the
    // width the operator chose.
    let park = park_beside(tmux, session, &panes, None, Side::Before)?;
    // The new body's id is not known when the batch is built, so it is
    // addressed by INDEX: `OffTheRail` appends it after the rail.
    let target = format!("{window}.{}", pane_index(tmux, &rail)?.checked_add(1)?);

    let mut batch = Batch::new();
    batch.push_owned(&furniture_split_program(park, card.launch));
    batch.push(&["set-option", "-p", "-t", &target, card.marker, card.person_id]);
    if let Some(claim) = card.claim {
        batch.push(&["set-option", "-p", "-t", &target, tags::WAKE_CLAIM, claim]);
        batch.push(&["set-option", "-p", "-t", &target, tags::WAKING_PENDING, claim]);
    }
    batch.push(&["set-option", "-g", "pane-border-status", "top"]);
    batch.push(&["set-option", "-g", "pane-border-format", super::SAFE_BORDER_DEFAULT]);
    batch.push(&["set-option", "-p", "-t", &target, "pane-border-format", card.border]);
    batch.push(&["rename-window", "-t", window, card.window_name]);
    batch.push(&["select-window", "-t", window]);
    batch.push(&["select-pane", "-t", &target]);
    batch
        .run_topology(tmux, session)
        .lines()
        .rev()
        .find_map(|line| line.trim().starts_with('%').then(|| line.trim().to_owned()))
}

/// One card the rail paints into its own window: a sleeping person's card, or
/// the "…is starting" body a wake click leaves behind.
struct FocusCard<'a> {
    /// The pane-local tag that says which KIND of card this is —
    /// [`tags::SLEEPING_PERSON`] or [`tags::WAKING_PERSON`].
    marker: &'a str,
    /// Who the card is about.
    person_id: &'a str,
    /// The shared wake claim, for a waking card only.
    claim: Option<&'a str>,
    /// What the body runs.
    launch: &'a [String],
    /// The pane border, which carries the person's name, role and accent.
    border: &'a str,
    /// What the window is renamed to while this card stands in it.
    window_name: &'a str,
}

/// Put the standing notice into the focus window.
///
/// The window is also renamed, because the name follows the occupant and there
/// is no occupant now.
///
/// Answers the notice pane, or `None` when tmux would not split one.
fn park_focus_window_if_still_empty(
    tmux: &dyn Tmux,
    session: &str,
    window: &str,
    rail: &str,
    park: Park,
) -> Option<(String, String)> {
    let rail_index = pane_index(tmux, rail)?;
    let argv = furniture_split(park, &parked_script());
    // OffTheRail appends the new body after the rail. Address that new index
    // inside the same tmux queue, before `split-window` reports its id.
    let target_index = rail_index.checked_add(1)?;
    let target = format!("{window}.{target_index}");
    let mut guarded = Batch::new();
    guarded.push(&["set-option", "-goq", viewport_options::TOPOLOGY_GENERATION, "0"]);
    guarded.push(&[
        "set-option",
        "-gF",
        viewport_options::TOPOLOGY_GENERATION,
        &format!("#{{e|+:#{{{}}},1}}", viewport_options::TOPOLOGY_GENERATION),
    ]);
    guarded.push(&[
        "set-option",
        "-F",
        "-t",
        session,
        viewport_options::TOPOLOGY_EPOCH,
        &format!("#{{{}}}", viewport_options::TOPOLOGY_GENERATION),
    ]);
    guarded.push_owned(&argv);
    guarded.push(&["set-option", "-p", "-t", &target, tags::ASLEEP, FOCUS_WINDOW_ID]);
    guarded.push(&["rename-window", "-t", window, PARKED_WINDOW_NAME]);
    guarded.push(&[
        "display-message",
        "-p",
        "-t",
        &target,
        &format!("#{{pane_id}}\t#{{{}}}", viewport_options::TOPOLOGY_EPOCH),
    ]);

    // THE WRITE-TIME CAS. A process pane can appear after the snapshot above.
    // Tmux evaluates this predicate when it executes the mutation queue; if
    // the target is no longer the sole sidebar pane in this exact focus
    // window, no topology option, pane, tag or name is changed.
    let equals = |field: &str, value: &str| format!("#{{==:#{{{field}}},{value}}}");
    let and = |left: String, right: String| format!("#{{&&:{left},{right}}}");
    let predicate = [
        equals("window_id", window),
        equals("window_panes", "1"),
        equals("pane_dead", "0"),
        equals(tags::SIDEBAR, "1"),
        equals(tags::PERSON, ""),
        equals(tags::ASLEEP, ""),
        equals(tags::WAKING_PERSON, ""),
        equals(tags::MINTING, ""),
    ]
    .into_iter()
    .rev()
    .reduce(|right, left| and(left, right))?;
    let output =
        tmux.run(&["if-shell", "-F", "-t", rail, &predicate, &guarded.command_string(), ""]);
    output.lines().rev().find_map(|line| {
        let (pane, generation) = line.trim().split_once('\t')?;
        (pane.starts_with('%') && generation.parse::<u64>().is_ok())
            .then(|| (pane.to_owned(), generation.to_owned()))
    })
}

/// Mint the session's one focus window, parked, railed and tagged.
///
/// `-d`: the glass does not move, so the operator never sees a window being
/// built. `-a -t '<session>:$'`: appended last, which is where
/// `desired_topology` puts the focus window and where it stays for the life of
/// the session — so no index ever shuffles under it.
fn mint_parked_focus_window(
    tmux: &dyn Tmux,
    session: &str,
    parked: &Parked,
    canonical: Option<crate::window_geometry::Geometry>,
) -> Option<String> {
    let last = format!("{session}:$");
    let topology_generation = invalidate_viewport_topology(tmux, session)?;
    let mut mint = Batch::new();
    mint.push(&[
        "new-window",
        "-d",
        "-a",
        "-n",
        PARKED_WINDOW_NAME,
        "-t",
        &last,
        "-P",
        "-F",
        "#{window_id}",
        "sh",
        "-c",
        &parked_script(),
    ]);
    // Identity is part of the create queue. The window cannot be visible as a
    // candidate between these commands, and the first pane cannot be mistaken
    // for a person before its furniture tag arrives.
    mint.push(&["set-option", "-w", "-t", &last, tags::ORGANIZATION, parked.organization]);
    mint.push(&["set-option", "-w", "-t", &last, tags::WINDOW, FOCUS_WINDOW_ID]);
    mint.push(&["set-option", "-p", "-t", &last, tags::ASLEEP, FOCUS_WINDOW_ID]);

    // THE SERVER-SIDE CREATE-IF-ABSENT. A client-side list followed by
    // `new-window` lets two rail processes both observe absence. Tmux evaluates
    // this loop and, when it is empty, runs the complete mint in one command
    // queue. A competing queue then sees the first window's logical tag and
    // takes the empty branch.
    let matches = ["#{W:#{?#{==:#{", tags::WINDOW, "},", FOCUS_WINDOW_ID, "},1,}}"].concat();
    let absent = format!("#{{==:{matches},}}");
    let window = tmux
        .run(&["if-shell", "-F", "-t", session, &absent, &mint.command_string(), ""])
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| line.starts_with('@'))
        .map(ToOwned::to_owned);
    let Some(window) = window else {
        refresh_viewport_topology(tmux, session, &topology_generation);
        let windows = department_windows(tmux, session, FOCUS_WINDOW_ID);
        return (windows.len() == 1).then(|| windows[0].clone());
    };
    // The window's ONE pane is the notice `new-window` just started. It carries
    // no person tag and never will: it is furniture, and the converge audit must
    // read it as furniture rather than adopting it as somebody.
    if let Some((pane, _)) = window_panes(tmux, &window).first() {
        tmux.run(&["set-option", "-p", "-t", pane, tags::ASLEEP, FOCUS_WINDOW_ID]);
        mint_rail(tmux, session, parked.rail_program, parked.company_dir, pane, &window);
    }
    normalize_now(tmux, &window, canonical);
    refresh_viewport_topology(tmux, session, &topology_generation);
    tracing::info!(
        event = "sidebar.focus.minted",
        session,
        window = %window,
        "this session's one focus window is up, parked and railed; it is minted once and \
         nothing on the click path ever destroys it"
    );
    Some(window)
}

/// One pane's value for `tag`, empty when it carries none.
fn pane_tag(tmux: &dyn Tmux, pane: &str, tag: &str) -> String {
    tmux.run(&["display-message", "-p", "-t", pane, &format!("#{{{tag}}}")]).trim().to_owned()
}

/// Is this window the one the session is showing?
fn window_is_active(tmux: &dyn Tmux, session: &str, window: &str) -> bool {
    tmux.run(&["display-message", "-p", "-t", session, "#{window_id}"]).trim() == window
}

/// The window a target lives in — a pane, or a SESSION, which tmux resolves to
/// that session's current window. The brain asks it both ways: once about a
/// client's own pane and once about the session, and compares, which is how it
/// knows whether that client is the one on the glass.
pub fn window_of_pane(tmux: &dyn Tmux, pane_id: &str) -> Option<String> {
    let reply = tmux.run(&["display-message", "-p", "-t", pane_id, "#{window_id}"]);
    let line = reply.lines().next()?.trim().to_owned();
    (!line.is_empty()).then_some(line)
}

/// How many panes are in the window this pane lives in.
///
/// One read, and the answer to a question the rail cannot get any other way:
/// **is this width a choice, or is it just what a lone pane gets?** A tmux pane
/// with no siblings is the whole window by construction.
pub fn window_pane_count(tmux: &dyn Tmux, pane_id: &str) -> i64 {
    tmux.run(&["display-message", "-p", "-t", pane_id, "#{window_panes}"])
        .trim()
        .parse()
        .unwrap_or(0)
}

/// The full width of the window this pane lives in.
///
/// Asked so [`super::brain::width_outcome`] can refuse a rail drawn as wide as
/// its own window — a frame caught between converge splitting a person in and
/// tmux laying the window out, never a width the operator dragged to. `0` when
/// tmux will not answer, which that rule reads as "no window width known" and
/// declines to act on.
pub fn window_width(tmux: &dyn Tmux, pane_id: &str) -> i64 {
    tmux.run(&["display-message", "-p", "-t", pane_id, "#{window_width}"])
        .trim()
        .parse()
        .unwrap_or(0)
}

// WHY EVERY `join-pane` IN THIS FILE CARRIES `-h`.
//
// # The flicker the operator could see and nobody could explain
//
// `join-pane` with no direction splits the TARGET VERTICALLY — top and bottom.
// So moving a person into a window took the rail, which is a full-height column
// down the left, and made it the TOP HALF of that window. The `select-layout`
// that follows restores it. Measured on a live company with
// `sidebar.rail.frame-resized`, on every single click, in every rail in the
// session at once:
//
// ```text
//   %4   49 -> 24 rows   (width 31, unchanged)
//   … ~200ms …
//   %4   24 -> 49 rows
// ```
//
// Twenty-four rows is half of a fifty-row window. The sidebar visibly collapsed
// to half height and sprang back, about a fifth of a second later, every time
// the operator clicked anybody. That is the "really subtle but really annoying"
// jitter, and it was never a WIDTH problem — which is why three rounds of width
// fixes never touched it.
//
// `-h` splits the target horizontally instead: the joined pane lands BESIDE
// what is already there, the rail keeps its full height, and the layout that
// follows only has to fine-tune columns it already roughly has.
//
// It also fixes where a person LANDS when the layout does not run. A vertical
// join puts them in the top cell — the operator reported "an agent showed up on
// the sidebar side (the column)" — and if the converge pass that would have
// arranged the window fails closed for any reason, that is where they stay
// until something else re-lays it. With `-h` even an unarranged window is the
// right shape.

/// Wall-clock milliseconds, or 0 before the epoch.
///
/// The only clock in this module. It survives the deletion of the two rules
/// that used to read it — a wake grace and a session-wide geometry stamp, both
/// of which existed to let separate rail processes compare timestamps — because
/// the loading and sleeping scripts still stamp what they print.
pub fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

/// Every pane in the session carrying `tag`, as `(pane, value)`.
fn tagged_panes(tmux: &dyn Tmux, session: &str, tag: &str) -> Vec<(String, String)> {
    tmux.run(&["list-panes", "-s", "-t", session, "-F", &format!("#{{pane_id}}\t#{{{tag}}}")])
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(pane, value)| !pane.trim().is_empty() && !value.trim().is_empty())
        .map(|(pane, value)| (pane.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// The panes of one window, as `(pane_id, is_rail)`, live ones only.
///
/// Ordered as tmux lists them, which is the order the layout string must
/// enumerate them in.
fn window_panes(tmux: &dyn Tmux, window: &str) -> Vec<(String, bool)> {
    tmux.run(&[
        "list-panes",
        "-t",
        window,
        "-F",
        &format!("#{{pane_id}}\t#{{{}}}\t#{{pane_dead}}", tags::SIDEBAR),
    ])
    .lines()
    .filter_map(|line| {
        let mut parts = line.split('\t');
        Some((parts.next()?, parts.next()?, parts.next()?))
    })
    .filter(|(pane, _, dead)| !pane.trim().is_empty() && dead.trim() != "1")
    .map(|(pane, rail, _)| (pane.trim().to_owned(), !rail.trim().is_empty()))
    .collect()
}

/// A window's size, as `(width, height)`.
fn window_size(tmux: &dyn Tmux, window: &str) -> Option<(i64, i64)> {
    let reply =
        tmux.run(&["display-message", "-p", "-t", window, "#{window_width}\t#{window_height}"]);
    let line = reply.lines().next()?;
    let (width, height) = line.split_once('\t')?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// A pane's current index inside its window.
///
/// Splitting immediately before this pane gives the new focus notice this same
/// index. That makes the notice addressable later in the same tmux server
/// command sequence, before the split command can publish a frame.
fn pane_index(tmux: &dyn Tmux, pane: &str) -> Option<u32> {
    tmux.run(&["display-message", "-p", "-t", pane, "#{pane_index}"])
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// The rail's canonical product width.
///
/// Pre-fix values are clamped to one of the two supported widths. An unreadable
/// option uses the expanded width.
pub fn expanded_columns(tmux: &dyn Tmux, session: &str) -> i64 {
    tmux.run(&["show-options", "-q", "-v", "-t", session, sidebar_options::COLUMNS])
        .trim()
        .parse::<i64>()
        .map_or(super::brain::RAIL_DEFAULT_COLUMNS, super::brain::canonical_columns)
}

/// Read the independent collapse preference for this tmux session.
pub fn collapsed(tmux: &dyn Tmux, session: &str) -> bool {
    tmux.run(&["show-options", "-q", "-v", "-t", session, sidebar_options::COLLAPSED]).trim() == "1"
}

fn rail_columns(tmux: &dyn Tmux, session: &str) -> i64 {
    if collapsed(tmux, session) {
        crate::layout::RAIL_COLLAPSED_COLUMNS
    } else {
        expanded_columns(tmux, session)
    }
}

/// Everything a person click needs that only the rail can know.
pub struct PersonClick<'a> {
    /// Who was clicked.
    pub person_id: &'a str,
    /// Their display name, for the log line that reports where the click went.
    ///
    /// It is NOT used to name a window any more. A person's window is minted by
    /// converge and named for them at mint time (`placement::Window::name`), so
    /// there is nothing here for the rail to rename — which is the whole reason
    /// this gesture no longer writes to tmux at all beyond selecting.
    pub display_name: &'a str,
}

/// Show one person alone beside the rail — by SELECTING THE WINDOW THEY ARE
/// ALREADY ALONE IN.
///
/// # The resize this deletes, which is the last one in the product
///
/// Every earlier version of this function MOVED a pane. It was
/// `resize-pane -Z` first (`bc01cc141`), which put the person full screen and
/// took the rail off the glass with everybody else; then a focused LAYOUT,
/// which held every bystander at 24 columns because a layout string enumerates
/// every pane and can narrow one but never hide it; then a `join-pane` into a
/// permanent focus window, which is what shipped and what the operator
/// recorded on 2026-08-21: *"when I click on an agent I want it should be in
/// the final position, right? Why is it going half screen and growing?"*
///
/// The measurement behind that sentence: their agents' panes were `42x17` and
/// `64x17` inside their department's tiled window while the focus body was
/// `129x35`. Joining a pane into that body is a RESIZE, tmux truncates or pads
/// the alternate screen at the new width, and the Pi inside repaints its whole
/// scrollback — so the operator watched their agent's text arrive at half width
/// and grow.
///
/// A pane has exactly one size. There is no arrangement of `join-pane`,
/// `select-layout` or `resize-window` that gives a pane its final geometry
/// while it is also somewhere else, so the move had to go rather than get
/// better. `placement::desired_topology` now gives every desired person a
/// window of their own, every window is normalized to the same canonical
/// geometry, and a click is `select-window` + `select-pane`.
///
/// **NOTHING HERE MOVES A PANE, AND THAT IS THE INVARIANT.**
/// `a_click_on_a_person_does_not_change_their_pane_width` samples
/// `#{pane_width}` on a real tmux either side of this call and asserts equality.
///
/// Answers what happened: whether the operator was shown the person. Never
/// `Shown::moved`, because no geometry moves — which is also what stops the
/// brain arming a settle pass for a gesture that had no transit to wait out.
pub fn show_person(tmux: &dyn Tmux, session: &str, click: &PersonClick) -> Shown {
    let canonical = canonical_geometry(tmux, session);
    let Some((pane, window)) = pane_of(tmux, session, click.person_id) else {
        // THE SILENT NO-OP. The operator clicked a person and the screen did
        // not change, because that person's pane went away between the draw and
        // the click. Saying nothing here makes a real product event — "clicking
        // does nothing" — invisible to anybody reading the log.
        //
        // It names WHO WAS THERE INSTEAD, and that is the whole point of the
        // line. The first version said only "no live pane", which is the one
        // fact the reader already knows; it cost an expedition to establish
        // whether the resolver was looking in the wrong SCOPE, mis-parsing the
        // output, or being handed an id nothing carried. The answer to all
        // three is in this field.
        let seen = live_person_ids(tmux, session);
        tracing::warn!(
            event = "sidebar.focus.unresolved",
            session,
            person = click.person_id,
            live_people = %seen.iter().cloned().collect::<Vec<_>>().join(","),
            "a click resolved to no live pane; the rail was showing somebody tmux no longer has"
        );
        return Shown::nothing();
    };
    if let Err(reason) = show_window_alone(tmux, &window, &pane, canonical, session) {
        // NEVER A SUCCESS LINE FOR SOMETHING THAT DID NOT HAPPEN. This is the
        // half that cost four operator reports: the click failed and the record
        // said it had worked, so every investigation began from the wrong
        // premise. The rail's own selection still moves — that is the operator's
        // stated intent and it is not this function's to undo — but the record
        // now says the glass did not follow.
        tracing::warn!(
            event = "sidebar.navigation.failed",
            session,
            person = click.person_id,
            name = click.display_name,
            window = %window,
            pane = %pane,
            reason = %reason,
            "the click did not move the glass; tmux did not make the target window active, \
             and the rail now says something the operator cannot see"
        );
        return Shown::nothing();
    }
    tracing::info!(
        event = "sidebar.person.selected",
        session,
        person = click.person_id,
        name = click.display_name,
        window = %window,
        pane = %pane,
        "the operator was taken to the window this person is already alone in; no pane moved"
    );
    Shown::navigated()
}

/// Put one window on the glass with one pane active.
///
/// The shared tail of every arm that does not move a pane, and — Stage 4 — it
/// LAYS NOTHING OUT. It used to end in a `select-layout` computed from the
/// window's live panes, which is a window RESIZE whatever the string says
/// (`layout-custom.c` `layout_parse` calls `window_resize`), and so SIGWINCHed
/// every app in the window on a gesture that moved no pane at all. A window
/// nobody joined and nobody left already holds the geometry converge gave it.
///
/// The un-zoom stays, and is a REPAIR rather than part of the design: nothing in
/// this product creates zoom state any more, but tmux still allows it and
/// `C-M-z` is still bound, so an operator who zoomed by hand must not be left
/// looking at one pane after asking for a view. It was partly redundant with the
/// deleted `select-layout` — which unzoomed the window as a side effect — and it
/// is not redundant with anything now.
/// # THE NAVIGATION VERIFIES ITSELF, AND THE LOG NEVER LIES
///
/// `Batch::run` discards tmux's result (`let _ = self.run_output(tmux)`), and
/// the caller used to log `sidebar.person.selected` — "the operator was taken
/// to the window" — UNCONDITIONALLY. So when the select failed, the click did
/// nothing, the rail's in-process selection moved anyway, and the record
/// asserted a navigation that had not happened.
///
/// Measured on the operator's box: three clicks on the same person in thirteen
/// seconds, each logging "taken to @2" with the correct window id and a live
/// pane, while the active window was @1 throughout and after. The result is
/// rail-says-Reid, glass-shows-Chief — the operator's screenshot — with ZERO
/// destructive events, because nothing was destroyed. **That false success line
/// is why this took four reports and two correct-but-unrelated fixes**: every
/// investigation started from "something stole the glass" because the one
/// surface anybody could read said the click had worked.
///
/// So this reads the active window back, retries once, and returns whether the
/// operator is actually there. The caller logs what was VERIFIED.
fn show_window_alone(
    tmux: &dyn Tmux,
    window: &str,
    pane: &str,
    canonical: Option<crate::window_geometry::Geometry>,
    session: &str,
) -> Result<(), String> {
    // EVERY READ FIRST, THEN ONE WRITE. See `Batch`: this gesture used to be up
    // to five invocations and therefore up to five frames, and the operator saw
    // the window between them.
    let zoomed = window_zoomed(tmux, window);

    let mut batch = Batch::new();
    normalize_into(&mut batch, tmux, window, canonical);
    batch.push(&["select-window", "-t", window]);
    // ONLY EVER CLEARS A ZOOM, NEVER CREATES ONE — guarded on
    // `#{window_zoomed_flag}` because `-Z` is a toggle. The gesture no longer
    // zooms anything; `C-M-z` is still bound and an operator can still zoom by
    // hand, and this is what un-does that for them.
    if zoomed {
        batch.push(&["resize-pane", "-Z", "-t", window]);
    }
    batch.push(&["select-pane", "-t", pane]);
    batch.run(tmux);
    // ONE READ, then ONE retry, then the truth. A single re-select is the whole
    // remedy this can offer: the observed failure is a transient control-client
    // hiccup (two rails attaching under a callback storm), and a loop would
    // turn a bad second into a fight with tmux.
    //
    // POSITIVE EVIDENCE ONLY. A probe that answers nothing means "I could not
    // tell", and inventing a failure from a read nobody took would be the same
    // class of lie as the one being fixed, pointing the other way. Only tmux
    // NAMING a different active window is a failure here.
    match navigation_diverged(tmux, session, window) {
        None => Ok(()),
        Some(_) => {
            let retry = tmux.run(&["select-window", "-t", window]);
            match navigation_diverged(tmux, session, window) {
                None => Ok(()),
                // tmux's own words when there are any — an empty retry output
                // means the command said nothing, which is itself the most
                // useful thing to report.
                Some(active) => Err(if retry.trim().is_empty() {
                    format!("the active window is still {active}")
                } else {
                    retry
                }),
            }
        }
    }
}

/// The active window when tmux names one AND it is not `window`; `None` when it
/// matches or when tmux would not say.
fn navigation_diverged(tmux: &dyn Tmux, session: &str, window: &str) -> Option<String> {
    active_window(tmux, session).filter(|active| active != window)
}

/// Whether the session is showing `logical`'s window RIGHT NOW.
///
/// `false` only on positive evidence: a probe tmux will not answer means "I
/// could not tell", and the caller must not act on a read nobody took — the
/// same rule the click's own verification follows.
pub fn active_window_is(tmux: &dyn Tmux, session: &str, logical: &str) -> bool {
    let Some(active) = active_window(tmux, session) else { return true };
    logical_of_window(tmux, &active).as_deref() == Some(logical)
}

/// The logical id tagged on a tmux window, or `None`.
fn logical_of_window(tmux: &dyn Tmux, window: &str) -> Option<String> {
    let out =
        tmux.run(&["display-message", "-p", "-t", window, "-F", &format!("#{{{}}}", tags::WINDOW)]);
    let id = out.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// Which window the session is actually showing, or `None` when tmux would not
/// say. One read, no client enumeration.
fn active_window(tmux: &dyn Tmux, session: &str) -> Option<String> {
    let out = tmux.run(&["display-message", "-p", "-t", session, "#{window_id}"]);
    let id = out.trim();
    (!id.is_empty() && id.starts_with('@')).then(|| id.to_owned())
}

/// Split this company's own rail into the window `pane` now lives in.
///
/// Never fatal. A window that will not split is a window with no rail until the
/// next converge pass rails it, and the person is on the glass either way.
///
/// **The split and the tag travel as ONE tmux command sequence**, for the same
/// reason [`mint_sleeping_department_window`] does it that way: separate tmux
/// invocations publish the new pane to observers BETWEEN the calls, and a rail
/// pane that is observable before it is tagged is a rail nothing can recognise.
/// This function used to split, return the new pane id, and tag it in a second
/// `tmux.run`. The gap was real and an operator hit it: `chief sidebar` came up
/// in the new pane inside 25ms, the actuator's `repair_session_rails` pass read
/// the window while the tag was still in flight, counted zero TAGGED rails,
/// concluded the window "had lost its sidebar" and split a second rail into it.
/// The repair's own rail got the tag, so every later guard — which counts tags,
/// not panes — saw exactly one rail and stayed quiet, while the first rail sat
/// there untagged, still attached to the brain, still painting, and re-laid by
/// the equal grid as though it were a BODY pane: the company drawn twice, once
/// down the left edge and once down the right.
///
/// Batched, tmux's single command loop runs the split and the tag back to back
/// with no other client's command between them, so the untagged state is never
/// observable. The tag targets the WINDOW with `-p`, which tmux resolves to that
/// window's active pane — the pane `split-window` just made active — because the
/// batch cannot refer to an id the split has not reported yet. `window` is the
/// window `beside` lives in, passed by the caller that just minted it rather
/// than re-read here, so the tag's target cannot drift from the split's.
fn mint_rail(
    tmux: &dyn Tmux,
    session: &str,
    program: Option<&str>,
    company_dir: &std::path::Path,
    beside: &str,
    window: &str,
) {
    let Some(program) = program else {
        // The caller could not name the program to run. A window with no rail
        // until the next converge pass rails it is worse than one with a rail,
        // and better than a pane running something nobody chose.
        tracing::warn!(
            event = "sidebar.rail.unminted",
            session,
            "this client cannot name its own executable, so the new window opens \
             without a rail; the next converge pass adds one"
        );
        return;
    };
    let columns = rail_columns(tmux, session).to_string();
    let mut batch = Batch::new();
    // `-b` with `-h` puts the rail to the LEFT of the person, so `list-panes`
    // reports `[rail, person]` — the order `organization_tmux_layout` requires,
    // because layout cells are filled by position.
    batch.push(&[
        "split-window",
        "-h",
        "-b",
        "-l",
        &columns,
        "-t",
        beside,
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        &company_dir.display().to_string(),
        program,
        "sidebar",
    ]);
    // The tag is what makes this pane a rail rather than a stranger: every
    // `window_panes` read and `observe_rail` find it by exactly this.
    batch.push(&["set-option", "-p", "-t", window, tags::SIDEBAR, "1"]);
    // `run`, not `run_topology`: the only caller is already inside its own
    // topology bracket, and nesting one would publish a refresh mid-construction
    // — the very thing the outer bracket is holding back.
    batch.run(tmux);
}

/// Re-lay one window's equal grid, reading its panes fresh.
///
/// The pane order is re-read EVERY time. `join-pane` appends at the
/// destination's active pane rather than in canonical person order, so a
/// window's pane order after a return may differ from `displayOrder` — the equal
/// grid renders identically and converge emits nothing (it diffs membership, not
/// intra-window order), so that is accepted. What is NOT survivable is building a
/// layout string against a remembered order.
/// [`relay`], reachable from the sibling test module.
///
/// The rule under test — which WIDTH a gesture lays the rail out at — lives
/// inside `grid_layout`, which is private and takes a pane list the test would
/// have to fabricate. Going in through the real entry point is what makes the
/// test exercise the read order as well as the arithmetic.
#[cfg(test)]
pub(super) fn lay_equal_grid_for_test(tmux: &dyn Tmux, session: &str, window: &str) {
    relay(tmux, session, window);
}

fn relay(tmux: &dyn Tmux, session: &str, window: &str) {
    let panes = window_panes(tmux, window);
    lay_equal_grid(tmux, session, window, &panes);
}

/// Is this window zoomed on one of its panes right now?
///
/// `#{window_zoomed_flag}` is tmux's own answer and the only one worth having:
/// zoom is per-WINDOW state that survives switching away and back (measured on
/// 3.3a), so a rail that remembered its own last zoom would be wrong about
/// every window but the one it last clicked in.
fn window_zoomed(tmux: &dyn Tmux, window: &str) -> bool {
    tmux.run(&["display-message", "-p", "-t", window, "#{window_zoomed_flag}"]).trim() == "1"
}

/// Write the pane border titles this rail owns: the rail's own, and one per
/// live person.
///
/// `roles` is `person_id -> role`, from the roster. A pane whose person is not
/// in it is left alone — it is not this company's, or it is not a person.
///
/// `accents` is `person_id -> #rrggbb`, as chiefd allocated it and published it
/// on the launch catalog. It is the GROUND the role chip is filled
/// with, and it takes precedence over the pane's own `@accent` — which is an
/// option of the retired TypeScript launcher that nothing on this tree writes,
/// kept in the read only because a pane that somehow carries one is telling the
/// truth about itself. A person in neither is drawn on the terminal's own
/// ground.
///
/// tmux draws no border title at all until `pane-border-status` is on, and it
/// draws `#{pane_title}` — whatever the program inside the pane set — until
/// `pane-border-format` says otherwise. Both are set here, per pane, which is
/// why nothing about the operator's own tmux configuration can change the
/// answer. The style is inlined on the title SPAN (`#[bg=…,fg=…]`) rather than
/// left to `pane-border-style`, which colours the LINE and never the text.
/// The three per-person maps a chip is drawn from.
///
/// They travel together because they are read together and keyed the same way:
/// one roster person id to their display name, their roster role, and the
/// accent chiefd allocated them. Passing them as three loose arguments beside
/// the session, the rails, the company, and the previous answer is how this
/// call reached eight positional arguments, four of which were the same
/// `BTreeMap<String, String>` type and therefore silently swappable.
pub struct PersonChips<'a> {
    /// Person id to the display name the chip and the rail both show.
    pub names: &'a std::collections::BTreeMap<String, String>,
    /// Person id to their roster role, already resolved to its display form.
    pub roles: &'a std::collections::BTreeMap<String, String>,
    /// Person id to the accent chiefd allocated, which is the chip's ground.
    pub accents: &'a std::collections::BTreeMap<String, String>,
}

/// Answers the border formats it wrote, keyed by pane, so a caller that sees
/// the SAME answer next pass can skip the whole call.
///
/// It is a write per pane per refresh, and the refresh runs on every changefeed
/// wake — a company with one busy agent wakes it many times a second. Nothing
/// about a border changes at that rate, and a `set-option` storm is both tmux
/// traffic and a visible redraw.
pub fn write_pane_titles(
    tmux: &dyn Tmux,
    session: &str,
    rails: &[String],
    company: &str,
    chips: PersonChips<'_>,
    previous: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let PersonChips { names, roles, accents } = chips;
    let mut written = std::collections::BTreeMap::new();
    let rail_format = super::rail_border_format(company);
    for rail in rails {
        if previous.get(rail) != Some(&rail_format) {
            tmux.run(&["set-option", "-g", "pane-border-status", "top"]);
            tmux.run(&["set-option", "-g", "pane-border-format", super::SAFE_BORDER_DEFAULT]);
            tmux.run(&["set-option", "-p", "-t", rail, "pane-border-format", &rail_format]);
        }
        written.insert(rail.clone(), rail_format.clone());
    }
    let listed = tmux.run(&[
        "list-panes",
        "-s",
        "-t",
        session,
        "-F",
        &format!("#{{pane_id}}\t#{{{}}}\t#{{@accent}}", tags::PERSON),
    ]);
    for line in listed.lines() {
        let mut parts = line.split('\t');
        let (Some(pane), Some(person)) = (parts.next(), parts.next()) else {
            continue;
        };
        let pane_accent = parts.next().unwrap_or_default().trim();
        let Some(role) = roles.get(person.trim()) else {
            continue;
        };
        let Some(name) = names.get(person.trim()) else {
            continue;
        };
        // chiefd's own allocation first: it is the accent that exists on this
        // tree, and it is the same one the browser draws.
        let accent = accents.get(person.trim()).map_or(pane_accent, String::as_str);
        let format = super::person_border_format(name, role, accent);
        if previous.get(pane.trim()) != Some(&format) {
            tmux.run(&["set-option", "-p", "-t", pane.trim(), "pane-border-format", &format]);
        }
        written.insert(pane.trim().to_owned(), format);
    }
    written
}

/// Resize the rail, and record the width where the layout will read it.
///
/// Both halves, always together. The `resize-pane` is what the operator sees;
/// the `set-option` is what stops the next converge pass from snapping it back,
/// because `interpret::observe_rail` reads exactly this option when it reserves
/// the rail's column.
pub fn apply_columns(tmux: &dyn Tmux, pane_id: &str, columns: i64) {
    tmux.run(&["resize-pane", "-x", &columns.to_string(), "-t", pane_id]);
}

/// Resize every tagged rail in this session without changing preferences.
pub fn resize_all_rails(tmux: &dyn Tmux, session: &str, columns: i64) {
    let panes = tmux.run(&[
        "list-panes",
        "-s",
        "-t",
        session,
        "-F",
        &format!("#{{pane_id}}\t#{{{}}}", tags::SIDEBAR),
    ]);
    let mut argv: Vec<String> = Vec::new();
    let columns = columns.to_string();
    for pane in panes.lines().filter_map(|line| {
        let (pane, tagged) = line.split_once('\t')?;
        (tagged.trim() == "1").then_some(pane.trim())
    }) {
        if !argv.is_empty() {
            argv.push(";".to_owned());
        }
        argv.extend(["resize-pane", "-x", &columns, "-t", pane].map(str::to_owned));
    }
    if !argv.is_empty() {
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        tmux.run(&refs);
    }
}

/// Store only the collapse choice, then apply its effective width everywhere.
pub fn set_collapsed_and_resize_all(tmux: &dyn Tmux, session: &str, collapsed: bool, columns: i64) {
    let Some(topology_generation) = invalidate_viewport_topology(tmux, session) else {
        return;
    };
    tmux.run(&[
        "set-option",
        "-t",
        session,
        sidebar_options::COLLAPSED,
        if collapsed { "1" } else { "0" },
    ]);
    resize_all_rails(tmux, session, columns);
    refresh_viewport_topology(tmux, session, &topology_generation);
    // The collapse toggle: one line per press. Deliberately NOT inside
    // `record_columns`, which runs on every redraw — see its own note. INFO for
    // the same reason every other click branch is: a press is a decision, and
    // the production filter drops debug.
    tracing::info!(
        event = "sidebar.rail.resized",
        session,
        pane = "all",
        columns,
        "the operator resized the rail"
    );
}

#[cfg(test)]
/// Seed the expanded preference in real-tmux fixtures.
pub fn record_columns(tmux: &dyn Tmux, session: &str, columns: i64) {
    tmux.run(&["set-option", "-t", session, sidebar_options::COLUMNS, &columns.to_string()]);
}
