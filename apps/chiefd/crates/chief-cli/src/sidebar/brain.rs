//! **One brain per session**: the process that holds the operator's whole
//! interactive picture in RAM and answers a click out of it.
//!
//! # What this replaces, and why the shape had to change
//!
//! There was one rail PROCESS PER WINDOW. None of them owned the selection, so
//! the design compensated with a coordination protocol built out of tmux
//! primitives — a company snapshot in a session option, a selection in another,
//! `send-keys` doorbells to wake sibling rails, an adopt pass, a wake-grace
//! timer, a per-rail refused set, a one-second refresh throttle. Every one of
//! those was well reasoned GIVEN THE SHAPE, and every one of them was a place
//! where a click's visible completion waited on another process waking up,
//! re-reading and agreeing.
//!
//! They are all deleted. This is the single authority: the roster, the desired
//! set, the idle set, the selection, the focus, the derived placement, the
//! accents and the in-flight wakes are plain fields of one struct, in one
//! process, on one task. Rail panes are THIN CLIENTS ([`super::client`]) that
//! forward raw bytes and blit whole frames.
//!
//! That is herdr's model, and the property that matters is herdr's:
//! **`handle_mouse` is a field assignment in the same thread that renders the
//! next frame** (`src/app/api/workspaces.rs:74`). There is no optimistic update
//! because there is nothing to reconcile.
//!
//! # A click, end to end
//!
//! ```text
//!   client forwards SGR bytes  →  super::input decodes them
//!     →  super::click hit-tests the frame that is ON THE GLASS
//!     →  the selection is a field assignment
//!     →  ONE batched control-mode message to tmux
//!     →  a frame into every client's mailbox
//! ```
//!
//! **Nothing on that path issues an HTTP request, waits on the writer actor,
//! reads a file, or waits for another process to agree.** The one durable
//! consequence — a sleeper's wake — is `tokio::spawn`ed AFTER the frame is on
//! its way, and its answer is reconciled backward through [`WakeAnswer`].
//!
//! # Where the facts come from
//!
//! The converge loop reads the company every round anyway, and hands what it
//! read here ([`Handle::company`]). Nothing in this file reads chiefd except
//! the wake POST. The other direction is [`Handle::focus`]: converge asks the
//! brain who the operator is looking at, instead of both of them reading a tmux
//! option and hoping. `Handle::nudge` is how a gesture reaches converge AT
//! ONCE rather than on the next changefeed wake — the whole of
//! `actuator.gesture.observed`'s measured 2,831–4,477ms.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn roster_presentations(
    roster: &crate::roster::Roster,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let ceo = roster
        .department(&roster.root_department_id)
        .map(|department| department.head_person_id.as_str());
    let names = roster
        .people
        .iter()
        .map(|person| (person.id.clone(), super::person_first_name(&person.display_name)))
        .collect();
    let roles = roster
        .people
        .iter()
        .map(|person| {
            (
                person.id.clone(),
                super::person_display_role(
                    &person.display_name,
                    &person.title,
                    ceo == Some(person.id.as_str()),
                ),
            )
        })
        .collect();
    (names, roles)
}

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::effects;
use super::gesture::GestureId;
use super::input::{Decoder, Input};
use super::wire::{Frames, Mailbox, Named, ToBrain, ToClient, PROTOCOL};
use super::{click, project, Action, Tmux, View};
use crate::actuate::client::ActuationClient;
use crate::layout::RAIL_COLLAPSED_COLUMNS;
use crate::roster::Roster;

/// How long after a gesture the brain takes one more frame, from scratch.
///
/// tmux applies a pane's GRID resize synchronously with the command but
/// re-sizes the pty — and so delivers SIGWINCH — later: promptly when it has
/// been quiet, up to ~250ms when another resize landed inside the last ~250ms.
/// So for up to a quarter second after a gesture, the size a client REPORTS can
/// differ from the size tmux will INTERPRET the frame at, and a frame drawn in
/// that gap wrecks the grid however carefully it was drawn.
///
/// This was a session-wide tmux option (`@chief_sidebar_gesture`) precisely
/// because one rail's click resized ANOTHER rail's pane and that process knew
/// of no gesture. There is one process now, so it is a field.
///
/// Three hundred milliseconds, comfortably past the 250ms defer ceiling. It
/// costs one extra frame per burst and nothing at rest.
const SETTLE_AFTER: Duration = Duration::from_millis(300);

/// How long after a CLICK the brain will still finish that click's navigation.
///
/// **CLICK-COMPLETION INSURANCE, NOT A STANDING FIGHT.** #1231 proved a click's
/// `select-window` can silently fail to land — three clicks in thirteen seconds
/// each logging success while the active window never changed. That fix made
/// the failure sayable; this window is how long the brain will still act on it.
///
/// Bounded deliberately and tightly. An operator who switches windows BY HAND
/// (tmux `prefix`+digit, or a click in their terminal) five minutes after a
/// click has made a new decision, and re-asserting the old one then would be
/// the brain fighting the person it exists to serve. Ten seconds covers the
/// converge passes that follow a click; anything past it belongs to the
/// operator.
const ENFORCE_SELECTION_WITHIN: Duration = Duration::from_secs(10);

/// The rail's fixed width when it is expanded.
pub const RAIL_DEFAULT_COLUMNS: i64 = 26;

/// The narrowest an EXPANDED rail can be read at.
///
/// `"Departments"` is eleven columns and the selection marker owns a twelfth,
/// so below this the rail cannot draw its own headings — every row is cut
/// mid-word. It is deliberately not the collapsed width: a collapsed rail is
/// four columns ON PURPOSE and is exempt below.
pub const RAIL_MIN_READABLE_COLUMNS: i64 = 12;

/// Read an expanded-width preference, or use the product default.
#[must_use]
pub const fn canonical_columns(recorded: i64) -> i64 {
    if recorded >= RAIL_MIN_READABLE_COLUMNS {
        recorded
    } else {
        RAIL_DEFAULT_COLUMNS
    }
}

/// The four chiefd answers the brain renders, as the converge loop read them.
///
/// It carries FACTS and no rendered rows. Everything session-local — who has a
/// live pane, who is mid-wake — the brain reads from tmux itself, so the state
/// dots and the `starting` mark never inherit the age of a converge round.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The structure: company, departments, people, titles. Also the placement
    /// input every click needs.
    pub roster: Roster,
    /// Who chiefd wants running.
    pub desired: BTreeSet<String>,
    /// Whose settle clock is running — the IDLE/WORKING split.
    pub idle: BTreeSet<String>,
    /// Person id to launch hash: the other half of what `desired_topology`
    /// needs to decide whether somebody gets a window of their own.
    pub hashes: BTreeMap<String, String>,
    /// Person id to their identity accent, `#rrggbb`, as chiefd allocated it.
    ///
    /// The COLOUR, not a path to a file holding it. This used to be the theme
    /// file paths, which the brain then opened and parsed to recover exactly
    /// this hex; chief writes no theme file now, so chiefd publishes its own
    /// allocator's answer and the rail reads it. A person absent from this map
    /// has no allocated colour and gets the explicit no-accent ground.
    pub accents: BTreeMap<String, String>,
    /// Backend-owned current model facts by person id.
    pub models: BTreeMap<String, crate::actuate::launch_catalog::PersonModel>,
    /// Durable inbox-message counts by person id. Every roster person has one,
    /// including a person whose launch gate is refused.
    pub inbox_counts: BTreeMap<String, usize>,
    /// Whose boot the ACTUATOR keeps retrying because it keeps dying, and the
    /// numbers the operator reads about it.
    ///
    /// The one fact in here chiefd cannot supply. chiefd holds the desired
    /// state and is never told what happened on this box, so a person it still
    /// wants whose Pi has died eleven times looks, to chiefd and to every rail
    /// reading only chiefd, exactly like a person who is on their way. That is
    /// how four people came to sit at `starting` for an hour and a half on the
    /// owner's box.
    pub crashing: BTreeMap<String, crate::sidebar::CrashNotice>,
    /// Person id to CHIEFD'S OWN REASON for declining to launch them.
    ///
    /// From the launch catalog, which the converge loop reads in the same pass
    /// as the desired set. The desired set says chiefd WANTS this person; the
    /// catalog says whether it will publish them a launch spec. A person in
    /// both is wanted and blocked, and until this map arrived the rail could
    /// only draw the first half of that and called it `starting`.
    pub refusals: BTreeMap<String, String>,
}

/// The sentence a person's focus-body card carries, and which KIND it is.
///
/// The two kinds differ in the one way the operator can act on, so the card
/// cannot be handed a bare string and left to guess:
///
/// * A WAKE refusal is chiefd's answer to one attempt — benched, paused, not
///   yours. Asking again is a real repair for it, so the card keeps its button.
/// * A LAUNCH GATE refusal is re-derived against the disk on every pass. No
///   button can change it, and a card that offered one would promise an
///   outcome the gate has already declined — which is exactly the card that
///   sat at `Waking up…` for five minutes about a person the rail beside it
///   was already calling `refused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardNotice<'a> {
    /// Nothing to say beyond who this person is.
    None,
    /// chiefd declined this WAKE, in its own words.
    WakeRefused(&'a str),
    /// chiefd's LAUNCH GATE declined this person, in its own words.
    CannotStart(&'a str),
}

impl<'a> CardNotice<'a> {
    /// The wake refusal, if that is what this is.
    const fn wake_refusal(self) -> Option<&'a str> {
        match self {
            Self::WakeRefused(reason) => Some(reason),
            Self::None | Self::CannotStart(_) => None,
        }
    }

    /// The launch gate's refusal, if that is what this is.
    const fn gate_refusal(self) -> Option<&'a str> {
        match self {
            Self::CannotStart(reason) => Some(reason),
            Self::None | Self::WakeRefused(_) => None,
        }
    }
}

/// What the operator is looking at, as the converge loop reads it.
///
/// The brain OWNS this. It used to be the `@chief_sidebar_selection` tmux
/// option, written by whichever rail was clicked and read by converge — which
/// is a bus between processes that are now one process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Focus {
    /// The person `placement::desired_topology` is handed as its focus.
    pub person: Option<String>,
    /// The gesture that put it there, so converge can name the same click.
    pub gesture: Option<u64>,
}

/// chiefd's answer to a wake the brain has ALREADY painted as accepted.
///
/// The click paints first and posts afterwards, so chiefd's answer arrives long
/// after the frame is on the glass and has to be reconciled BACKWARD into a
/// brain that has moved on. The POST therefore runs on a spawned task, and a
/// spawned task cannot hold the brain — one owner is the whole reason these
/// fields are sound. So the task sends and the loop applies, which is the shape
/// herdr uses for every background fact it folds into its state
/// (`src/events.rs`: "Background tasks send events to the main loop through
/// this channel. No polling needed.").
#[derive(Debug)]
pub struct WakeAnswer {
    /// The gesture that asked. Carried so the refusal — reconciled backward
    /// into a brain that has moved on — still names the click it is undoing.
    pub gesture: GestureId,
    /// The person the operator clicked.
    pub person: String,
    /// The name their row carried at CLICK TIME, so the notice says what the
    /// operator pointed at however the roster has changed since.
    pub name: String,
    /// chiefd's OWN words, when it said no. `None` is a grant.
    pub refusal: Option<String>,
}

/// Everything that reaches the brain's one task.
#[derive(Debug)]
enum Event {
    /// A thin client said hello.
    Attach {
        /// This connection's id, minted by the accept loop.
        id: u64,
        /// The client's own tmux pane.
        pane: String,
        /// Columns of that pane.
        columns: u16,
        /// Rows of that pane.
        rows: u16,
        /// Where this client's frames go.
        outbox: Arc<Mailbox>,
    },
    /// Raw stdin bytes from a client.
    Input {
        /// Which client.
        id: u64,
        /// The bytes, verbatim.
        bytes: Vec<u8>,
    },
    /// A client's pane changed size.
    Resize {
        /// Which client.
        id: u64,
        /// Columns now.
        columns: u16,
        /// Rows now.
        rows: u16,
    },
    /// A client went away.
    Detach {
        /// Which client.
        id: u64,
    },
    /// The converge loop read the company.
    Company(Box<Facts>),
    /// The converge loop could not read the company.
    Unreadable,
    /// A spawned wake POST answered.
    Wake(Box<WakeAnswer>),
    /// A converge pass moved geometry — reaped a window, re-laid one, killed a
    /// pane. The rails it reflows can no more attribute that to themselves than
    /// they could when converge was a different process.
    GeometryMoved,
    /// Somebody asked what the rail is drawing. See [`ToBrain::Describe`].
    Describe {
        /// Where the answer goes.
        outbox: Arc<Mailbox>,
    },
    /// A sleeping-person card's Wake Up button was activated.
    WakeCard { pane: String, person: String, outbox: Arc<Mailbox> },
}

/// The brain, as everything outside its task holds it.
#[derive(Debug, Clone)]
pub struct Handle {
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    focus: Arc<Mutex<Focus>>,
    nudge: Arc<tokio::sync::Notify>,
}

impl Handle {
    /// Hand the brain what chiefd just said. Never blocks, never fails: a
    /// converge pass must not be held up by a display.
    pub fn company(&self, facts: Facts) {
        let _ = self.events.send(Event::Company(Box::new(facts)));
    }

    /// Tell the brain the company could not be read.
    ///
    /// "I have not read it yet" and "I tried and could not" are different
    /// facts, and a rail that cannot tell them apart draws the boot `…` for
    /// ever — which is the defect [`View::note_unreadable`] exists to end.
    pub fn unreadable(&self) {
        let _ = self.events.send(Event::Unreadable);
    }

    /// Who the operator is looking at, and the gesture that decided it.
    ///
    /// Read by converge once per pass, exactly where it used to read the
    /// selection option. A poisoned lock answers "no focus", which places
    /// everybody in their department — the same answer an unset option gave.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus.lock().map(|focus| focus.clone()).unwrap_or_default()
    }

    /// Tell the brain that a converge pass moved geometry, so the resizes that
    /// follow it are read as a transit rather than as the operator dragging a
    /// border. This is `@chief_sidebar_gesture`, as a function call inside one
    /// process.
    pub fn geometry_moved(&self) {
        let _ = self.events.send(Event::GeometryMoved);
    }

    /// The signal a gesture rings so converge runs NOW.
    ///
    /// A cold click's `actuator.gesture.observed` was measured at 2,831 and
    /// 4,477ms — pure converge-cadence latency between the operator clicking
    /// and the process that spawns panes learning of it. One process cannot
    /// have that latency, and this is why: the brain rings, converge wakes.
    #[must_use]
    pub fn nudge(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.nudge)
    }
}

/// Where a client's rendered ANSI accumulates.
///
/// `CrosstermBackend` writes its cell diff into whatever `Write` it holds, and
/// ratatui keeps that writer private — so the buffer is shared with the seat
/// rather than reached through the backend. It is the whole reason a frame can
/// be composed for a pane this process does not own: the "terminal" is a
/// `Vec<u8>`, and the bytes go over a socket to the process that does.
#[derive(Debug, Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    /// Take everything written since the last take.
    ///
    /// A poisoned lock answers with nothing, which costs one frame and never a
    /// panic in a process the whole session's glass depends on.
    fn take(&self) -> Vec<u8> {
        self.0.lock().map(|mut bytes| std::mem::take(&mut *bytes)).unwrap_or_default()
    }
}

impl std::io::Write for Sink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut bytes) => {
                bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }
            // A poisoned lock is a writer that panicked while holding it. There
            // is no state to repair — the buffer is drained whole every frame —
            // and reporting an error here would only make the caller drop the
            // same frame twice.
            Err(_) => Ok(buffer.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One attached thin client: its pane, its size, and the frames it is owed.
struct Seat {
    /// The client's tmux pane id.
    pane: String,
    /// The terminal the brain renders THIS client's frames through. Per client,
    /// because two rails can be different sizes and a shared back buffer would
    /// describe neither.
    terminal: Terminal<CrosstermBackend<Sink>>,
    /// The bytes that terminal writes into.
    sink: Sink,
    /// Where the frames go. Single slot, latest wins — see [`Mailbox`].
    outbox: Arc<Mailbox>,
    /// The last frame put in that mailbox, so an identical one is not sent.
    last: Vec<u8>,
    /// The size this client last reported.
    size: (u16, u16),
    /// Whether this client's frames are withheld because a gesture of ours put
    /// its geometry in flight.
    withheld: bool,
    /// A gesture whose answering frame has not reached this client yet.
    owed: Option<GestureId>,
}

impl Seat {
    /// Render the view for this client and leave it in the mailbox.
    ///
    /// Answers whether a frame was actually sent.
    ///
    /// EVERY CELL, EVERY TIME. ratatui draws the difference between its own
    /// back buffer and the frame it is about to draw; that is a BELIEF about
    /// what is on the glass, and tmux falsifies it routinely by resizing a pane
    /// between the two. Writing every cell replaces the belief with a fact, and
    /// it is also what makes the single-slot mailbox sound: a frame that can be
    /// dropped in flight must not be a diff.
    ///
    /// It is not done by ERASING first. `Terminal::clear` emits an ED2, which
    /// takes effect the moment tmux reads it — and the operator's tmux 3.3a
    /// honours no synchronized-update markers, so the erased screen reaches the
    /// glass alone and every repaint blanks the pane for a frame.
    fn push(&mut self, view: &View, gesture: Option<GestureId>) -> bool {
        poison_previous_frame(&mut self.terminal);
        let _ = self.sink.take();
        if self.terminal.draw(|frame| super::render::draw(frame, view)).is_err() {
            return false;
        }
        let bytes = self.sink.take();
        // AN IDENTICAL FRAME IS NOT SENT — herdr drops those server-side for
        // the same reason. It is what makes the pointer crossing the rail cost
        // zero bytes. A frame answering a GESTURE is always sent even when it
        // is identical, because the client is what writes
        // `sidebar.frame.painted` and a gesture with no frame has no honest
        // end.
        if gesture.is_none() && bytes == self.last {
            return false;
        }
        self.last.clone_from(&bytes);
        self.outbox.put(ToClient::Frame { gesture: gesture.map(GestureId::raw), bytes });
        true
    }

    /// Re-make this client's terminal at a new size.
    ///
    /// Rebuilt rather than `Terminal::resize`d, because that emits an ED2 into
    /// the byte stream and this file's whole repaint argument is that an erase
    /// must never reach a tmux that ignores synchronized updates.
    fn resize(&mut self, columns: u16, rows: u16) {
        self.size = (columns, rows);
        if let Some((terminal, sink)) = fresh_terminal(columns, rows) {
            self.terminal = terminal;
            self.sink = sink;
        }
        // The record of what this client has been sent is void: it describes a
        // screen of a different shape.
        self.last.clear();
    }
}

/// A terminal that renders into a byte buffer at exactly this size.
///
/// `Viewport::Fixed` and never `Fullscreen`: a fullscreen viewport asks the
/// BACKEND how big it is, and this backend is a `Vec<u8>` whose crossterm
/// implementation would answer with the size of whatever terminal the ACTUATOR
/// happens to have been started in. The size is the client's, and it only ever
/// arrives over the wire.
fn fresh_terminal(columns: u16, rows: u16) -> Option<(Terminal<CrosstermBackend<Sink>>, Sink)> {
    let area = Rect::new(0, 0, columns.max(1), rows.max(1));
    let sink = Sink::default();
    let terminal = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        TerminalOptions { viewport: Viewport::Fixed(area) },
    )
    .ok()?;
    Some((terminal, sink))
}

/// Make ratatui's record of what is on the glass describe a screen that NO
/// frame can match, so the next draw writes every cell.
///
/// # Why not `Buffer::reset`, which is what `clear` does
///
/// `reset` fills the previous buffer with BLANKS, so the diff then skips every
/// cell the new frame also wants blank — and a blank is what most of a rail's
/// cells legitimately are. A glyph stranded in a column the new frame leaves
/// empty is exactly such a cell, so the one class of corruption a repaint
/// exists to repair is the one class `reset` cannot reach. The poison has to be
/// a value no cell can legitimately hold.
///
/// The buffer that has to be poisoned is the PREVIOUS one — `flush` diffs
/// `buffers[1 - current]` against `buffers[current]` — and ratatui exposes only
/// `current_buffer_mut`, so the poison goes into the current buffer and
/// `swap_buffers` moves it into the previous slot.
fn poison_previous_frame<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
    for cell in terminal.current_buffer_mut().content.iter_mut() {
        cell.set_symbol(NEVER_DRAWN);
    }
    terminal.swap_buffers();
}

/// The symbol the previous frame is poisoned with before a repaint.
///
/// One requirement: no cell of a real frame may ever equal it. The empty string
/// is that value by construction — `Cell::set_symbol("")` is reachable from no
/// widget, and a `Cell`'s default is a SPACE. It never reaches a terminal: the
/// poisoned buffer is only ever the left-hand side of a diff.
const NEVER_DRAWN: &str = "";

/// Consecutive rounds one claim must stay unseen before it may be parked.
///
/// Three, because a wake that chiefd is going to grant is desired within one
/// refresh of the POST landing and two is already generous; and because every
/// round costs an operator staring at a card that will never start. Raising it
/// makes a real wedge last longer, lowering it risks parking a wake that was
/// merely slow.
const UNSEEN_WAKING_ROUNDS: u8 = 3;

/// The session brain.
pub struct Brain {
    tmux: Arc<dyn Tmux>,
    client: Arc<ActuationClient>,
    session: String,
    /// The company root used as the working directory for every rail process.
    company_dir: std::path::PathBuf,
    /// The one [`View`] in the session. Every client renders THIS.
    view: View,
    /// The roster and launch hashes the last company read left behind, kept so
    /// a CLICK can compute placement without a network round trip. There is one
    /// copy, in the one process that reads the company.
    placement: Option<(Roster, BTreeMap<String, String>)>,
    /// The pane border formats last written, so an unchanged pass writes none.
    titles: BTreeMap<String, String>,
    /// The department the brain has already shown a sleeping notice for. THE
    /// REFRESH PATH MAY ONLY ACT ON A TRANSITION.
    noticed: Option<String>,
    /// The selected person the brain has already said is no longer up.
    ///
    /// THE SAME TRANSITION RULE AS `noticed`, and it must not be folded into
    /// the card record beside it: `show_selection` can fail to paint a card —
    /// a person whose ROW has gone has nothing to draw from — and a loss that
    /// is announced from the announce-and-repaint pair rather than from the
    /// repaint alone would then be announced again on every company read, about
    /// once a second, for as long as the operator left the selection there.
    gone: Option<String>,
    /// The person a click WOKE, waiting for their pane to exist.
    ///
    /// Cleared by any later gesture, because it records the operator's LAST
    /// one: a sleeper waking two passes after they moved on must not steal the
    /// glass from whatever they moved on to.
    pending_zoom: Option<String>,
    /// The sleeping person whose final focus body currently holds the card.
    sleeping_card: Option<(String, String)>,
    /// The person, and the exact sentence, the focus body's card is currently
    /// showing as chiefd's LAUNCH GATE refusal.
    ///
    /// The card is a PROCESS in a pane, so re-showing it respawns that pane.
    /// A company read happens about once a second and re-derives the same
    /// refusal every time, so without a record of what the body already says
    /// the operator's card would be killed and rebuilt under them once a
    /// second. The reason is kept beside the person because a gate whose
    /// answer CHANGES has something new to say.
    carded_refusal: Option<(String, String)>,
    /// The latest backend desired set. This separates an external wake from
    /// the optimistic `waking` set owned by this brain.
    desired: BTreeSet<String>,
    /// People this brain has posted a wake for and not yet heard back about.
    ///
    /// ONE FIELD WHERE THERE WERE TWO MECHANISMS. A private `Rail::waking` set
    /// deduplicated the POST, and a session-wide `@chief_sidebar_waking` option
    /// with a sixty-second grace told the OTHER rails to draw `starting` and to
    /// leave that person's selection alone. One process needs one set: it
    /// dedupes the POST, it marks the row, and it exempts the person from the
    /// stale-selection rule. No grace, because the timer only ever existed to
    /// bound a record a dead rail could leave behind, and the brain is the
    /// process that outlives every rail.
    waking: BTreeSet<String>,
    /// People whose wake this brain asked for and chiefd refused.
    ///
    /// They are neither live nor desired and never will be until somebody
    /// staffs them, so the stale-selection rule must leave them alone rather
    /// than hauling the operator off to the CEO a tick after they clicked.
    refused: BTreeSet<String>,
    /// One waking claim this brain has watched go unseen, and for how long.
    ///
    /// A wake and an orphan are the same shape at first sight: a pane tagged
    /// with a person and a claim that chiefd has not yet called desired. The
    /// only thing that tells them apart is that the wake stops looking that
    /// way. So this brain counts the CONSECUTIVE rounds in which it saw this
    /// exact pane and this exact claim still unseen, and only past the bound
    /// does it offer to park it. A different pane, a different claim, or one
    /// round where the shape changed resets the count to nothing.
    ///
    /// Brain-local on purpose. The rule it protects is that no process parks a
    /// claim it has not itself watched, so this must not be readable — or
    /// writable — by another brain.
    unseen_waking: Option<(String, String, u8)>,
    /// The expanded width selected by the last explicit rail-border drag.
    expanded_columns: i64,
    /// Each person's identity accent, last resolved. Kept across a failed
    /// company read so one dropped connection does not blank every chip.
    accents: BTreeMap<String, String>,
    /// Backend-owned model facts last read with the launch catalog.
    models: BTreeMap<String, crate::actuate::launch_catalog::PersonModel>,
    inbox_counts: BTreeMap<String, usize>,
    /// The operator's LATEST gesture, for converge's own line.
    gesture: Option<u64>,
    /// Every attached thin client.
    clients: BTreeMap<u64, Seat>,
    /// Each client's decoder. Separate from the seat because a client's bytes
    /// can arrive before its `Hello` is applied.
    decoders: BTreeMap<u64, Decoder>,
    /// When this brain last ran tmux effects of its own — the whole of the
    /// anti-jitter rule, and a plain field now that there is one process.
    gestured_at: Option<Instant>,
    /// When the operator last made ANY gesture — including one that moved no
    /// geometry, which `gestured_at` deliberately does not record.
    clicked_at: Option<Instant>,
    /// The person whose divergence has already been enforced once, so a
    /// persistent failure is reported once rather than re-asserted every pass.
    enforced_for: Option<String>,
    /// When the settle pass is due, or `None`.
    settle_at: Option<Instant>,
    /// The focus cell converge reads.
    focus: Arc<Mutex<Focus>>,
    /// The signal a gesture rings so converge runs at once.
    nudge: Arc<tokio::sync::Notify>,
    /// Where a spawned wake POST posts its answer back to the loop.
    answers: tokio::sync::mpsc::UnboundedSender<Event>,
}

impl Brain {
    /// The events channel this brain's task drains, and the brain itself.
    fn new(
        tmux: Arc<dyn Tmux>,
        client: Arc<ActuationClient>,
        session: String,
        company_dir: std::path::PathBuf,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let expanded_columns = effects::expanded_columns(tmux.as_ref(), &session);
        let collapsed = effects::collapsed(tmux.as_ref(), &session);
        let mut view = View::unread();
        view.set_collapsed(collapsed);
        let brain = Self {
            tmux,
            client,
            session,
            company_dir,
            // UNREAD, not empty. The company has not been asked yet, and any
            // frame that escapes before the first read must say so rather than
            // assert a company with no departments and nobody in it.
            view,
            placement: None,
            titles: BTreeMap::new(),
            noticed: None,
            gone: None,
            pending_zoom: None,
            sleeping_card: None,
            carded_refusal: None,
            desired: BTreeSet::new(),
            waking: BTreeSet::new(),
            refused: BTreeSet::new(),
            unseen_waking: None,
            expanded_columns,
            accents: BTreeMap::new(),
            models: BTreeMap::new(),
            inbox_counts: BTreeMap::new(),
            gesture: None,
            clients: BTreeMap::new(),
            decoders: BTreeMap::new(),
            gestured_at: None,
            clicked_at: None,
            enforced_for: None,
            settle_at: None,
            focus: Arc::new(Mutex::new(Focus::default())),
            nudge: Arc::new(tokio::sync::Notify::new()),
            answers: events,
        };
        (brain, receiver)
    }

    /// Fold one answer about the company into the view.
    ///
    /// THE ONE PLACE THE COMPANY BECOMES A FRAME. It takes chiefd's four facts
    /// and nothing else; every session-local fact it needs — who is live — it
    /// reads from tmux itself, right here.
    /// How many consecutive rounds this brain has watched one unseen claim.
    ///
    /// Returns whether that count has passed [`UNSEEN_WAKING_ROUNDS`], which is
    /// the only thing that lets the orphan park run at all. Every other answer
    /// -- no waking pane, a pane whose person chiefd now wants or tmux now has,
    /// a different pane, a different claim -- clears the count, so a wake that
    /// is merely slow starts from zero again the moment it is seen.
    fn watch_unseen_waking(
        &mut self,
        organization: &str,
        desired: &BTreeSet<String>,
        live: &BTreeSet<String>,
    ) -> bool {
        let Some((pane, person, claim)) =
            effects::unseen_waking_focus(self.tmux.as_ref(), &self.session, organization)
        else {
            self.unseen_waking = None;
            return false;
        };
        if desired.contains(&person) || live.contains(&person) {
            self.unseen_waking = None;
            return false;
        }
        let rounds = match self.unseen_waking.take() {
            Some((seen_pane, seen_claim, rounds)) if seen_pane == pane && seen_claim == claim => {
                rounds.saturating_add(1)
            }
            _ => 1,
        };
        self.unseen_waking = Some((pane, claim, rounds));
        rounds >= UNSEEN_WAKING_ROUNDS
    }

    fn absorb(&mut self, facts: Facts) {
        let first_company_read = !self.view.is_read();
        let Facts {
            roster,
            desired,
            idle,
            hashes,
            accents,
            models,
            inbox_counts,
            crashing,
            refusals,
        } = facts;
        let live = effects::live_person_ids(self.tmux.as_ref(), &self.session);
        let unseen_expired = self.watch_unseen_waking(&roster.company.slug, &desired, &live);
        if let Some(parked) = effects::park_orphan_waking_focus(
            self.tmux.as_ref(),
            &self.session,
            &roster.company.slug,
            &desired,
            &live,
            unseen_expired,
        ) {
            self.waking.remove(&parked.person);
            self.refused.remove(&parked.person);
            if self.pending_zoom.as_deref() == Some(parked.person.as_str()) {
                self.pending_zoom = None;
            }
            tracing::warn!(
                event = "sidebar.waking.orphan-parked",
                session = %self.session,
                pane = %parked.pane,
                person = %parked.person,
                "the final focus body kept a waking claim after launch authority disappeared; \
                 it was returned to the permanent parked frame before another click"
            );
        }
        // User-facing names and roles come from one roster presentation rule;
        // the full person id stays the key and never becomes presentation.
        let (names, roles) = roster_presentations(&roster);
        self.accents = accents;
        self.models = models;
        self.inbox_counts = inbox_counts;
        // EVERY RAIL IN THE SESSION, not just one. There is one brain and N
        // rails now, so the process that knows the company is the process that
        // has to title all of their borders — and the accents are kept so a
        // rail that attaches BETWEEN company reads gets the same chips as its
        // siblings rather than a pass of grey ones.
        let rails: Vec<String> = self.clients.values().map(|client| client.pane.clone()).collect();
        self.titles = effects::write_pane_titles(
            self.tmux.as_ref(),
            &self.session,
            &rails,
            &roster.company.display_name,
            effects::PersonChips { names: &names, roles: &roles, accents: &self.accents },
            &self.titles,
        );
        let company = roster.company.display_name.clone();
        let (departments, people) = project(&roster, &desired, &live, &idle, &crashing, &refusals);
        self.placement = Some((roster, hashes));
        self.view.refresh(company, departments, people);
        // THE CARD IS A LIVE REPORT, not a photograph of the click that opened
        // it. It reads the rows this refresh just wrote, which are the same
        // rows the rail beside it draws, and it is repainted only when they
        // actually moved — see `effects::refresh_department_card` for why this
        // path may not lay out or navigate.
        self.repaint_department_cards();
        if !self.settle_refused_focus(&refusals) {
            // The body is not about a refused person, so nothing is being held
            // open by one.
            self.carded_refusal = None;
        }
        if let Some((person, pane)) = self.sleeping_card.clone() {
            let still_rostered = self.person_is_operational(&person);
            let refused_by_the_gate = refusals.contains_key(&person);
            if !still_rostered {
                self.sleeping_card = None;
                effects::park_sleeping_focus(self.tmux.as_ref(), &self.session, &person);
            } else if live.contains(&person) {
                // THE CARD IS ANSWERED BY NAVIGATION NOW. It used to be
                // answered by a MOVE: the person's live pane was joined into
                // the card's own cell in one guarded tmux batch, so the focus
                // window never published the obsolete card beside the live
                // person. There is no "beside" left — the card is in the rail's
                // card window and the person is in their own — so the whole
                // guarded-swap family is gone and the click is a
                // `select-window` onto them, with the spent card parked behind
                // the operator, off the glass.
                let _ = self.show_person(&person);
                effects::park_sleeping_focus(self.tmux.as_ref(), &self.session, &person);
                self.sleeping_card = None;
            } else if refused_by_the_gate {
                // ALREADY ANSWERED, by `settle_refused_focus` above. Neither
                // promotion nor a spinner may be layered on top of a refusal:
                // this is the person the gate has declined, and the card is
                // already saying so.
            } else if desired.contains(&person) && !self.waking.contains(&person) {
                let name = self
                    .view
                    .people()
                    .iter()
                    .find(|row| row.id == person)
                    .map_or_else(|| person.clone(), |row| row.name.clone());
                let organization = self
                    .placement
                    .as_ref()
                    .map(|(roster, _)| roster.company.slug.as_str())
                    .unwrap_or_default();
                if effects::promote_sleeping_focus(
                    self.tmux.as_ref(),
                    &self.session,
                    organization,
                    &pane,
                    &person,
                    &name,
                ) {
                    self.sleeping_card = None;
                }
            }
        }
        self.desired = desired.clone();
        if first_company_read {
            self.initialize_selection_from_active_window();
        }
        tracing::debug!(
            event = "sidebar.refresh",
            session = %self.session,
            departments = self.view.departments().len(),
            people = self.view.people().len(),
            live = live.len(),
            desired = desired.len(),
            idle = idle.len(),
            selected = self.view.selected().unwrap_or("-"),
            "the brain re-read the company"
        );
        // STARTING SURVIVES THE REFRESH THAT WOULD OTHERWISE ERASE IT.
        // `View::refresh` replaces the people wholesale from chiefd's facts, so
        // the mark a click puts on a row is RE-DERIVED here from the one set
        // that knows a wake is outstanding. EVERY entry, because the operator
        // wakes people in bursts and each click's mark holds until chiefd
        // answers for THAT person.
        for waking in &self.waking {
            self.view.mark_starting(waking);
        }
        // A NOTICE THAT HAS STOPPED BEING TRUE, swept from the same `live` read.
        let awake_departments: BTreeSet<String> =
            live.iter().filter_map(|person| self.homes().get(person).cloned()).collect();
        // AND A NOTICE WHOSE DEPARTMENT IS GONE. The roster is the authority on
        // which departments exist at all, and furniture for one that does not
        // outlives every other sweep: no converge pass places a window for a
        // department the tree no longer has.
        let known_departments: BTreeSet<String> = self
            .placement
            .as_ref()
            .map(|(roster, _)| {
                roster.departments.iter().map(|department| department.id.clone()).collect()
            })
            .unwrap_or_default();
        effects::close_sleeping_notices(
            self.tmux.as_ref(),
            &self.session,
            &awake_departments,
            &known_departments,
        );
        self.tidy_selection(&live, &desired);
        self.finish_pending_zoom(&live);
        // THE SESSION'S ONE FOCUS WINDOW, MINTED HERE AND NOWHERE ELSE. Before
        // `never_blank`, because it is what makes that sweep's hardest case
        // impossible: a parked focus window holds a standing notice, so it can
        // never be the rail-only window on the glass.
        self.ensure_focus_window();
        // LAST, AND UNCONDITIONALLY. See `never_blank`.
        self.never_blank();
        self.enforce_selection(&live);
        self.publish_focus();
    }

    /// **FINISH A CLICK WHOSE NAVIGATION DID NOT LAND.**
    ///
    /// #1231 established the mechanism: `select-window` can silently fail, the
    /// rail's in-process selection moves anyway, and the operator is left
    /// looking at somebody else's window with the rail insisting otherwise.
    /// That fix made the failure SAYABLE. This one completes it — once.
    ///
    /// # Four bounds, and each one is load-bearing
    ///
    /// 1. **Within [`ENFORCE_SELECTION_WITHIN`] of a click.** This is
    ///    click-completion insurance, never a standing fight: an operator who
    ///    switches windows by hand later has made a new decision, and reverting
    ///    it would be the brain overruling the person it serves.
    /// 2. **Gesture-fenced.** Any new gesture clears the episode, so this can
    ///    never re-assert something the operator has moved on from.
    /// 3. **Once per divergence episode.** A window that will not take the
    ///    selection is reported once; asserting it every pass would be a loop
    ///    against tmux, which is what #1231's own one-retry rule refuses.
    /// 4. **LIVE people only** — and this is the bound that keeps the reap
    ///    working. Under #1211 a person who has GONE stays selected until the
    ///    operator clicks elsewhere, so enforcing for them would hold their
    ///    dead window on the glass and make it unreapable for ever: the exact
    ///    starvation `kill_window`'s comment warns about, reached through a
    ///    third door. `tidy_selection` owns gone people and cards them off,
    ///    unchanged.
    ///
    /// # It is ENFORCEMENT, and deliberately NOT a destruction veto
    ///
    /// The actuator's watched-window guard still asks `window_active` and
    /// nothing else. Making the SELECTION able to refuse a reap was considered
    /// and refused for bound 4's reason; this moves the glass toward the
    /// selection instead, which is the operator's stated want without the
    /// starvation.
    fn enforce_selection(&mut self, live: &BTreeSet<String>) {
        let Some(clicked_at) = self.clicked_at else { return };
        if clicked_at.elapsed() >= ENFORCE_SELECTION_WITHIN {
            return;
        }
        let Some(person) = self.view.selected_person().map(str::to_owned) else { return };
        if self.enforced_for.as_deref() == Some(person.as_str()) || !live.contains(&person) {
            return;
        }
        // A person whose card or wake is in flight is not diverged — those
        // paths own the glass on purpose.
        if self.sleeping_card.is_some() || self.pending_zoom.is_some() {
            return;
        }
        let window = crate::placement::person_window_id(&person);
        if effects::active_window_is(self.tmux.as_ref(), &self.session, &window) {
            return;
        }
        self.enforced_for = Some(person.clone());
        let display_name = self
            .view
            .people()
            .iter()
            .find(|row| row.id == person)
            .map_or_else(|| person.clone(), |row| row.name.clone());
        let shown = effects::show_person(
            self.tmux.as_ref(),
            &self.session,
            &effects::PersonClick { person_id: &person, display_name: &display_name },
        );
        tracing::info!(
            event = "sidebar.selection.enforced",
            session = %self.session,
            person = %person,
            window = %window,
            completed = shown.shown,
            "the rail's selection and the glass had diverged inside a click's own window; \
             the click's navigation was re-asserted once"
        );
    }

    /// Make the first shared rail frame agree with the retained tmux glass.
    ///
    /// Later selection belongs only to the operator. This method is called
    /// only for the first successful company read, so a changefeed refresh can
    /// never replace a click.
    fn initialize_selection_from_active_window(&mut self) {
        let Some(active) = effects::active_window_selection(self.tmux.as_ref(), &self.session)
        else {
            return;
        };
        // A PERSON'S OWN WINDOW, which is what a retained session is on
        // whenever the operator left it looking at somebody. The window id
        // NAMES them, so the pane tag is only a fallback for the rail's own
        // card window, which holds a card about a person and not the person.
        let person = crate::placement::person_window_person_id(&active.window)
            .map(str::to_owned)
            .or(active.person);
        let Some(person) = person else {
            self.view.select(&active.window);
            return;
        };
        let Some(home) = self.homes().get(&person).cloned() else {
            self.view.select(&active.window);
            return;
        };
        self.view.select(&home);
        self.view.select_person(&person);
    }

    /// Make sure this session has its one permanent focus window, furnished.
    ///
    /// **THE ONLY PLACE A FOCUS WINDOW IS EVER MINTED**, and it runs on the
    /// company-read path, never on the click path. That is the whole of Stage 4's
    /// topology rule: a person click moves a pane into a window that already
    /// exists, and a department click moves it back out — no window is created or
    /// destroyed by anything the operator does.
    fn ensure_focus_window(&self) {
        let Some((roster, _)) = self.placement.as_ref() else {
            return;
        };
        let program = super::rail_program();
        effects::ensure_focus_window(
            self.tmux.as_ref(),
            &self.session,
            &effects::Parked {
                organization: &roster.company.slug,
                rail_program: program.as_deref(),
                company_dir: &self.company_dir,
            },
        );
    }

    /// Where every person's pane belongs when nobody is focused.
    ///
    /// DERIVED, never stored — the same derivation `desired_topology` makes.
    fn homes(&self) -> BTreeMap<String, String> {
        let Some((roster, _)) = self.placement.as_ref() else {
            return BTreeMap::new();
        };
        roster
            .people
            .iter()
            .filter_map(|person| {
                Some((
                    person.id.clone(),
                    crate::placement::pane_department_id(roster, person).ok()?,
                ))
            })
            .collect()
    }

    /// Tell the operator the person they are looking at has gone, and repaint
    /// that person honestly. **NOTHING HERE MOVES THE SELECTION.**
    ///
    /// # The CEO landing, and why it is gone
    ///
    /// This used to end by selecting the CEO and putting the CEO on the glass,
    /// so that `interpret::kill_window` — which DEFERS the reap of the window
    /// the operator is watching — would stop being starved by an operator
    /// parked on a stale person window. It bought that reap with the operator's
    /// own attention, and the operator's ruling is that it may not be bought at
    /// all: *"we should never ever switch without the user explicitly
    /// clicking."*
    ///
    /// Measured on a live box, session `org-taperoom-inc-4cc439_`: a
    /// click on the sleeping `pm-exposure` put their card on the FOCUS window,
    /// a disclosure toggle two seconds later changed no selection, and the next
    /// converge pass read the focus window as "a person window" and threw the
    /// glass to `@chief` — while the rail went on highlighting `pm-exposure`.
    /// Two surfaces, two answers, neither of them asked for.
    ///
    /// # The reap is still not starved
    ///
    /// The repaint is the whole of the answer. A person who is neither live nor
    /// desired has no pane, so `show_selection` shows their CARD, and the card
    /// lives on the permanent focus window — which no pass ever reaps. The
    /// operator therefore stops being the active-window watcher of the spent
    /// person window without being taken anywhere: the SUBJECT on the glass is
    /// the person they selected, before and after. The deferral releases on the
    /// next pass because they are no longer standing on it, not because they
    /// were moved off it.
    ///
    /// # There is no bystander left to guard against
    ///
    /// This used to ask whether the rail running it had the glass, whether some
    /// OTHER rail had recorded a wake, and whether the session's shared waking
    /// record exempted the person — three guards, all of them about rails that
    /// had not seen the click. One process sees every click, so the question is
    /// only ever about the person: are they live, are they wanted, is a wake
    /// outstanding, did chiefd refuse them.
    fn tidy_selection(&mut self, live: &BTreeSet<String>, desired: &BTreeSet<String>) {
        let Some(department) = self.view.selected().map(str::to_owned) else {
            return;
        };
        let Some(person) = self.view.selected_person().map(str::to_owned) else {
            // A DEPARTMENT view whose department has emptied out. IT IS
            // ANSWERED WHERE THEY ARE, never by moving them: the version that
            // moved them to a department with somebody live is what made a
            // click on a fully-asleep Engineering land on the CEO.
            let homes = self.homes();
            let asleep = !live
                .iter()
                .chain(self.waking.iter())
                .any(|person| homes.get(person).is_some_and(|home| *home == department));
            if asleep {
                if self.noticed.as_deref() != Some(department.as_str()) {
                    self.noticed = Some(department.clone());
                    self.show_department_overview(&department);
                }
            } else {
                self.noticed = None;
            }
            self.gone = None;
            return;
        };
        if live.contains(&person)
            || desired.contains(&person)
            || self.pending_zoom.as_deref() == Some(person.as_str())
            || self.sleeping_card.as_ref().is_some_and(|(card, _)| card == &person)
            || self.waking.contains(&person)
            || self.refused.contains(&person)
        {
            self.gone = None;
            return;
        }
        // A PERSON THE ROSTER NO LONGER HAS IS NOT A SELECTION. The rail draws
        // no row for them, so there is no mark for the glass to agree with and
        // nothing to redraw them from. The marker is cleared WHERE THE OPERATOR
        // IS — the glass is not moved, and `never_blank` owns the window after
        // this exactly as it does on every other pass.
        if !self.person_is_operational(&person) {
            self.gone = None;
            self.view.select(&department);
            tracing::info!(
                event = "sidebar.selection.stale",
                session = %self.session,
                person = %person,
                department = %department,
                "the selected person has left the roster; the marker is cleared and the \
                 operator is left exactly where they were"
            );
            return;
        }
        // ONCE PER LOSS. A company read arrives about once a second and
        // re-derives the same absence every time.
        if self.gone.as_deref() == Some(person.as_str()) {
            return;
        }
        self.gone = Some(person.clone());
        let name = self
            .view
            .people()
            .iter()
            .find(|row| row.id == person)
            .map_or_else(|| person.clone(), |row| row.name.clone());
        effects::announce(self.tmux.as_ref(), &self.session, &format!("{name} is no longer up"));
        // THE SAME SUBJECT, HONESTLY REDRAWN. They stay selected in the rail
        // and their card takes the glass, so the two halves of the surface
        // still agree and the operator is still looking at the person they
        // asked for.
        self.show_selection();
        tracing::info!(
            event = "sidebar.selection.stale",
            session = %self.session,
            person = %person,
            department = %department,
            "the selected person is neither up nor desired; the operator has been told and \
             the person's own card has taken the glass, and NOTHING was selected for them"
        );
    }

    /// **PUT THE CURRENT SELECTION ON THE GLASS.**
    ///
    /// The operator's rule, and the one thing this surface owes them: the panel
    /// beside the rail says what the rail says is selected. A person if a
    /// person is selected — their own pane while they are up, their card when
    /// they are not — and the department's overview when no person is.
    ///
    /// Every non-click repair goes through here, which is what makes the two
    /// halves of the surface incapable of disagreeing. `never_blank` used to
    /// repair a rail-only window with the selected DEPARTMENT's overview and
    /// never read the selected PERSON, so the first frame after `chief` put the
    /// Executive overview beside a rail that said `@chief`.
    ///
    /// It SELECTS NOTHING. Selection belongs to the click path
    /// (`Brain::perform`) and to the one first-read seed
    /// (`initialize_selection_from_active_window`), and to nothing else.
    fn show_selection(&mut self) -> effects::Shown {
        let Some(person) = self.view.selected_person().map(str::to_owned) else {
            let Some(department) = self.view.selected().map(str::to_owned) else {
                return effects::Shown::nothing();
            };
            // The notice IS the transition, so record it — but only when one
            // actually went up. `show_department_overview` answers
            // `Shown::nothing()` when the brain has had no company read yet or
            // the row does not match, and recording a notice nobody saw makes
            // the next asleep-department transition decline to show it.
            let shown = self.show_department_overview(&department);
            if shown.shown {
                self.noticed = Some(department);
            }
            return shown;
        };
        if effects::live_person_ids(self.tmux.as_ref(), &self.session).contains(&person) {
            return self.show_person(&person);
        }
        // AND IT CARRIES THE GATE'S OWN SENTENCE when there is one. A card that
        // said nothing about a person chiefd has declined would offer a wake
        // button for a launch that cannot happen — the exact promise
        // `settle_refused_focus` exists to take back.
        let refusal = self
            .view
            .people()
            .iter()
            .find(|row| row.id == person)
            .and_then(|row| row.refused.clone());
        let notice = refusal.as_deref().map_or(CardNotice::None, CardNotice::CannotStart);
        let Some(pane) = self.show_sleeping_person_card(&person, notice) else {
            return effects::Shown::nothing();
        };
        self.carded_refusal = refusal.map(|reason| (person.clone(), reason));
        self.sleeping_card = Some((person, pane));
        effects::Shown::navigated()
    }

    /// A WINDOW IS NEVER A RAIL AND NOTHING ELSE.
    ///
    /// The operator's rule: "the right-hand side should never go blank. It's
    /// just impossible." Every previous attempt patched one CAUSE — the loading
    /// panel dying on a timer, a notice expiring, a person settling, converge
    /// reaping, a wake that never landed — and the picture kept coming back
    /// through a path nobody had enumerated. This stops asking WHY and observes
    /// the STATE, at the end of every company read, after every path that could
    /// have emptied the window has had its turn.
    fn never_blank(&mut self) {
        // The window the OPERATOR is looking at. `-t <session>` resolves to the
        // session's current window, which is the only window that can be blank
        // in front of anybody.
        //
        // EXACTLY ONE. A window with no panes is not a window, so `0` is not a
        // blank window — it is `window_pane_count` reporting that tmux did not
        // answer, and repairing on it means acting on a reading nobody took.
        // This used to be `> 1`, which read every failed count as "blank" and
        // is harmless only while the repair is idempotent; the repair now puts
        // the SELECTION on the glass, so a phantom blank would re-card a person
        // who is mid-wake and take the card authority off the body that owns it.
        if effects::window_pane_count(self.tmux.as_ref(), &self.session) != 1 {
            return;
        }
        // THE FOCUS WINDOW IS NOT THIS SWEEP'S BUSINESS ANY MORE, and that is
        // Stage 4's simplification of it. `ensure_focus_window` has already run
        // this pass and put the standing notice back, so a rail-only focus
        // window is a state that no longer exists — and acting on it here would
        // be actively wrong: `show_department_overview` below navigates to a
        // DEPARTMENT's window, so an operator parked on the person view would be
        // dragged off it by a sweep that is supposed to repair what they are
        // looking at.
        if effects::window_department_id(self.tmux.as_ref(), &self.session).as_deref()
            == Some(crate::placement::FOCUS_WINDOW_ID)
        {
            return;
        }
        let Some(department) = self.view.selected().map(str::to_owned) else {
            return;
        };
        tracing::info!(
            event = "sidebar.window.blank",
            session = %self.session,
            department = %department,
            person = self.view.selected_person().unwrap_or("-"),
            "the window on the glass held nothing but its rail; THE CURRENT SELECTION is \
             going back beside it, because a blank right-hand side is a state the operator \
             must never be shown whatever produced it"
        );
        // THE SELECTION, NOT THE DEPARTMENT. This repair used to show the
        // selected department's overview unconditionally, so the first frame
        // after `chief` — where the seed has just selected `@chief` and their
        // home `executive` — put the Executive overview beside a rail that said
        // `@chief`. A repair that contradicts the rail is not a repair.
        self.show_selection();
    }

    /// Show a department whose people are ALL asleep, saying exactly that.
    ///
    /// There is no redirect on this path. The department the operator asked for
    /// gets a window with a rail and one panel that says who is in it and what
    /// to do about it.
    ///
    /// Answers what it did, so a caller on the CLICK path can tell whether any
    /// geometry moved — a notice window that already exists is navigated to and
    /// nothing else.
    fn show_department_overview(&self, department: &str) -> effects::Shown {
        let Some((roster, _)) = self.placement.as_ref() else {
            tracing::warn!(
                event = "sidebar.department.unplaced",
                session = %self.session,
                department,
                "a department click arrived before the brain's first company read; nothing \
                 can be shown for it yet"
            );
            return effects::Shown::nothing();
        };
        let Some(row) = self.view.departments().iter().find(|row| row.id == department) else {
            // THE OPERATOR ASKED FOR THIS TO BE LOUD: "if you can't match, just
            // say error: cannot match so we know what the fuck is going on."
            let known: Vec<&str> =
                self.view.departments().iter().map(|row| row.id.as_str()).collect();
            effects::announce(
                self.tmux.as_ref(),
                &self.session,
                &format!("error: cannot match department '{department}'"),
            );
            tracing::error!(
                event = "sidebar.department.unmatched",
                session = %self.session,
                department,
                known = %known.join(","),
                "a click resolved to a department the brain is not drawing; nothing was \
                 shown and NOTHING was redirected"
            );
            return effects::Shown::nothing();
        };
        let sleeping: Vec<String> =
            self.view.people().iter().map(|person| person.name.clone()).collect();
        let program = super::rail_program();
        let card = self.department_card_launch(department);
        // THE OVERVIEW GETS A WINDOW OF ITS OWN, and must: the department's own
        // logical id already belongs to the window placement puts its PEOPLE
        // in, and two windows claiming one id is a hard plan failure
        // (`fails_closed_when_two_windows_claim_the_same_logical_window`). The
        // card holds no person and converge does not want it, which is the same
        // shape as the focus window and gets the same treatment.
        let overview_id = crate::placement::overview_window_id(department);
        effects::show_department_overview(
            self.tmux.as_ref(),
            &self.session,
            &effects::Overview {
                organization: &roster.company.slug,
                department_id: &overview_id,
                department_name: &row.name,
                asleep: sleeping.len(),
                rail_program: program.as_deref(),
                company_dir: &self.company_dir,
                card: card.as_deref(),
            },
        )
    }

    /// Take the operator to the window this person is already alone in.
    fn show_person(&self, person_id: &str) -> effects::Shown {
        let Some(_) = self.placement.as_ref() else {
            tracing::warn!(
                event = "sidebar.person.unplaced",
                session = %self.session,
                person = person_id,
                "a click arrived before the brain's first company read; converge will \
                 complete the gesture from the recorded focus"
            );
            return effects::Shown::nothing();
        };
        let display_name = self
            .view
            .people()
            .iter()
            .find(|row| row.id == person_id)
            .map_or_else(|| person_id.to_owned(), |row| row.name.clone());
        effects::show_person(
            self.tmux.as_ref(),
            &self.session,
            &effects::PersonClick { person_id, display_name: &display_name },
        )
    }

    /// Show the person a click WOKE, once tmux says their pane exists.
    ///
    fn finish_pending_zoom(&mut self, live: &BTreeSet<String>) {
        let arrived: Vec<String> =
            self.waking.iter().filter(|person| live.contains(*person)).cloned().collect();
        for person in arrived {
            self.waking.remove(&person);
        }
        let Some(person) = self.pending_zoom.clone() else {
            return;
        };
        if !live.contains(&person) {
            return;
        }
        self.pending_zoom = None;
        // ONLY WHILE THE OPERATOR STILL MEANS IT. Measured on the operator's
        // box: they woke `dev`, clicked on to the engineering DEPARTMENT, and
        // eleven seconds later the zoom fired and hauled dev out of the grid
        // they were watching.
        if self.view.selected_person() != Some(person.as_str()) {
            tracing::info!(
                event = "sidebar.wake.zoom-dropped",
                session = %self.session,
                person = %person,
                "the woken person's pane came up, but the operator has moved on to another \
                 selection; they stay where placement put them instead of taking the glass"
            );
            return;
        }
        // THE HONEST END OF A COLD CLICK. `sidebar.window.laid` fires at click
        // time — a median 2ms BEFORE the wake it was quoted as completing — and
        // reading it as "visible" is what made a 5,636ms gesture get reported
        // as 1-37ms. This is the line where the person the operator asked for
        // is actually on the glass.
        tracing::info!(
            event = "sidebar.wake.zoomed",
            session = %self.session,
            person = %person,
            "the woken person's pane came up; finishing the click that asked for them"
        );
        self.show_person(&person);
        // THE CARD THAT ASKED FOR THEM, RETIRED BY THE ARRIVAL IT PROMISED.
        // The waking body used to BECOME this person's pane — converge claimed
        // it with `respawn-pane` — so nothing had to retire it. One window per
        // person means they arrive in a window of their own instead, and the
        // card is left saying "… is starting" about somebody who has started.
        // Parked after the navigation, so it is repainted behind the operator
        // rather than in front of them.
        effects::park_waking_focus(self.tmux.as_ref(), &self.session, &person);
    }

    /// Take back what the click painted, because chiefd will not honour it.
    ///
    /// The other end of painting first. Optimism is only sound if it is
    /// WITHDRAWN when it turns out to be wrong, and this is the whole of that.
    /// A GRANT releases the person and does nothing else: the glass has been
    /// showing the right thing since the click.
    fn settle_wake(&mut self, answer: &WakeAnswer) {
        let Some(reason) = answer.refusal.as_deref() else {
            // The grant leaves them in `waking` until their PANE arrives, which
            // is what keeps the row saying `starting` across the converge pass
            // that publishes them. `finish_pending_zoom` releases it.
            return;
        };
        self.waking.remove(&answer.person);
        // THE REFUSAL NAMES THE CLICK IT UNDOES. It runs on the loop, long after
        // the gesture's own span has closed, so the id is restated here.
        let _gesture = answer.gesture.span().entered();
        if self.pending_zoom.as_deref() == Some(answer.person.as_str()) {
            self.pending_zoom = None;
        }
        // THE OPERATOR STAYS WHERE THEY ARE. `tidy_selection` would otherwise
        // see somebody neither live nor desired and throw them to the CEO one
        // tick later.
        let still_current = self.sleeping_card.as_ref().is_some_and(|(person, _)| {
            person == &answer.person && self.view.selected_person() == Some(person.as_str())
        }) && self.person_is_operational(&answer.person)
            && !self.desired.contains(&answer.person)
            && !effects::live_person_ids(self.tmux.as_ref(), &self.session)
                .contains(&answer.person);
        if !still_current {
            tracing::info!(
                event = "sidebar.wake.refusal-stale",
                session = %self.session,
                person = %answer.person,
                "the wake refusal arrived after the card authority changed; it was ignored"
            );
            return;
        }
        self.refused.insert(answer.person.clone());
        self.sleeping_card = self
            .show_sleeping_person_card(&answer.person, CardNotice::WakeRefused(reason))
            .map(|pane| (answer.person.clone(), pane));
        if self.sleeping_card.is_none() {
            effects::park_waking_focus(self.tmux.as_ref(), &self.session, &answer.person);
            effects::announce(
                self.tmux.as_ref(),
                &self.session,
                &effects::wake_refused_notice(&answer.name, reason),
            );
        }
        self.never_blank();
        tracing::warn!(
            event = "sidebar.wake.refused",
            session = %self.session,
            person = %answer.person,
            diagnostic = %reason,
            "the wake was refused; the operator has been told why and everything the click \
             painted has been taken back"
        );
    }

    /// Publish the selection converge reads.
    fn publish_focus(&self) {
        let next =
            Focus { person: self.view.selected_person().map(str::to_owned), gesture: self.gesture };
        if let Ok(mut focus) = self.focus.lock() {
            *focus = next;
        }
    }

    /// Re-read who has a live pane, and fold it into the rows already drawn.
    ///
    /// One tmux call against the operator's own terminal — no network, nothing
    /// `async`, no company read — which is what lets it run on the mouse path.
    /// It exists because liveness is TMUX's fact and chiefd emits no event for
    /// it, so nothing wakes this loop when a pane dies: without it a person
    /// could sit in the company tree, drawn live and clickable, and clicking
    /// them would do nothing at all, silently, for ever.
    fn resync_live(&mut self) {
        let live = effects::live_person_ids(self.tmux.as_ref(), &self.session);
        self.view.set_live(&live);
        // THE OTHER WRITER OF THE ROWS THE CARDS READ. `View::refresh` is
        // refreshed from `absorb`; this is the second and last place the rows a
        // card is derived from can move, so a card left un-repainted here would
        // disagree with the rail for exactly the reason this whole surface was
        // fixed. Same transition guard, so a click that changes no state
        // touches nothing.
        self.repaint_department_cards();
    }

    /// **A FOCUS BODY THAT PROMISES A LAUNCH THE GATE HAS REFUSED BECOMES THE
    /// REFUSAL.**
    ///
    /// Answers whether the body is about a refused person at all, so the
    /// caller can leave the rest of the card rules alone when it is.
    ///
    /// # What the operator watched
    ///
    /// `◓ Waking up…` on the focus card for five minutes and nineteen seconds
    /// — about sixty-four converge rounds — for a person whose rail row, one
    /// pane away, read `refused` the whole time. Two surfaces, two answers,
    /// one person, and the bigger of the two was the wrong one. It cleared
    /// only when the operator selected somebody else.
    ///
    /// This is NOT the stuck-waking shape the reclaim path is for, and it must
    /// not be filed as one: that shape is a person chiefd never calls desired,
    /// and a gate-refused person IS desired — `launch_intent` holds them, and
    /// the gate declines them later, at launch. The reclaim path declining is
    /// correct. The defect is the promise.
    ///
    /// So the rule is the rail's own rule, applied to the card: **a state that
    /// can never advance must stop promising.** The card is rebuilt carrying
    /// the gate's sentence and no button, the optimism this brain painted is
    /// taken back, and the pass after the repair lands the ordinary rules
    /// below take the person back.
    ///
    /// It fires ONCE per person per sentence. The card is a process in a pane
    /// and a company read arrives about once a second, so re-showing it on
    /// every pass would kill and rebuild the operator's card under them
    /// continuously.
    fn settle_refused_focus(&mut self, refusals: &BTreeMap<String, String>) -> bool {
        // WHO THE BODY IS ABOUT. The card if there is one, and otherwise the
        // person a click is still zooming to — the second is the body a click
        // on a sleeping row leaves behind, which says "… is starting…" and is
        // the same promise in fewer words.
        let Some(person) = self
            .sleeping_card
            .as_ref()
            .map(|(person, _)| person.clone())
            .or_else(|| self.pending_zoom.clone())
        else {
            return false;
        };
        let Some(reason) = refusals.get(&person) else {
            return false;
        };
        if self.carded_refusal.as_ref().is_some_and(|(carded, said)| {
            carded == &person && said == reason && self.sleeping_card.is_some()
        }) {
            return true;
        }
        // THE OPTIMISM THIS BRAIN PAINTED, TAKEN BACK. A person the gate has
        // declined has no wake outstanding that anything is going to answer,
        // and leaving them in these sets keeps the row marked `starting` and
        // the zoom pending for ever.
        self.waking.remove(&person);
        self.refused.remove(&person);
        if self.pending_zoom.as_deref() == Some(person.as_str()) {
            self.pending_zoom = None;
        }
        let shown = self.show_sleeping_person_card(&person, CardNotice::CannotStart(reason));
        let carded = shown.is_some();
        if let Some(pane) = shown {
            self.sleeping_card = Some((person.clone(), pane));
            self.carded_refusal = Some((person.clone(), reason.clone()));
            tracing::info!(
                event = "sidebar.focus.refused-by-gate",
                session = %self.session,
                person = %person,
                reason = %reason,
                "the focus body was promising a launch chiefd's gate has declined; it now \
                 carries the gate's own reason and offers no wake"
            );
        } else {
            tracing::warn!(
                event = "sidebar.focus.refusal-unshown",
                session = %self.session,
                person = %person,
                reason = %reason,
                "the focus body could not be given the gate's reason; it is retried on the \
                 next company read rather than left promising a launch"
            );
        }
        carded
    }

    /// The department overview card's argv, built from the roster this brain is
    /// already holding.
    ///
    /// `None` before the first company read — there is nothing to draw a card
    /// FROM, and a card of empty columns is a worse answer than the one-line
    /// notice the caller falls back to.
    ///
    /// It reads NOTHING from any agent. That is the whole point of the surface:
    /// the tiled grid it replaces put every live person in the unit on the glass
    /// at 42 columns and repainted each of them at 129 the moment one was
    /// clicked, and the operator's report was that switching agents "always
    /// starts half screen and then resizes full screen". A card disturbs
    /// nobody, because there is no agent pane in this window to disturb.
    fn department_card_launch(&self, department_id: &str) -> Option<Vec<String>> {
        let program = super::rail_program()?;
        let rows = self.view.departments();
        let row = rows.iter().find(|row| row.id == department_id)?;
        // The ancestor chain, outermost first. The rail draws a DEPTH per row
        // and the tree is a depth-first walk, so the chain is every preceding
        // row whose depth is strictly decreasing — the same reading the rail
        // itself does, and the reason two same-named units under two parents
        // are told apart on this card.
        let index = rows.iter().position(|candidate| candidate.id == department_id)?;
        let mut path: Vec<String> = Vec::new();
        let mut depth = row.depth;
        for ancestor in rows[..index].iter().rev() {
            if ancestor.depth < depth {
                path.push(ancestor.name.clone());
                depth = ancestor.depth;
            }
        }
        path.reverse();
        // Direct children only: every following row that sits exactly one level
        // deeper, stopping at the first row that returns to this level or above.
        let children: Vec<String> = rows[index + 1..]
            .iter()
            .take_while(|candidate| candidate.depth > row.depth)
            .filter(|candidate| candidate.depth == row.depth + 1)
            .map(|candidate| candidate.name.clone())
            .collect();
        let members = match self.view.everybody().get(department_id) {
            Some(people) => people
                .iter()
                .map(|person| {
                    Some(super::department_card::Member {
                        name: person.name.clone(),
                        role: person.title.clone(),
                        state: person.state(),
                        model: self
                            .models
                            .get(&person.id)
                            .map(crate::actuate::launch_catalog::PersonModel::label)
                            .unwrap_or_default(),
                        inbox_messages: self.inbox_counts.get(&person.id).copied()?,
                        head: person.manager,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        let card = super::department_card::Card { name: row.name.clone(), path, members, children };
        let payload = serde_json::to_string(&card).ok()?;
        Some(vec![program, "department-card".to_owned(), payload])
    }

    /// Redraw the department overview card when the department it is about has
    /// changed.
    ///
    /// # Why the company-read path and not the click
    ///
    /// A click is the only thing that ever put this card up, so the card said
    /// what was true at the click and went on saying it. The operator watched
    /// Chief come up in the rail while the card one pane away still read
    /// `starting`, and read `0 up` beside a rail that said `2/5`. Two surfaces,
    /// one company, and the smaller one was right — which is the same shape as
    /// the refusal defect `settle_refused_focus` documents, and it has the same
    /// answer: **a surface that can go stale must be refreshed by the thing
    /// that learns.**
    ///
    /// # It derives from the ROWS, never from a second read
    ///
    /// [`Self::department_card_launch`] reads `View::everybody()`, which is the
    /// very map `View::refresh` above has just written and `View::set_live`
    /// re-liveness on the click path — the same rows, and the same
    /// `PersonRow::state()`, that the rail draws. There is one answer to "what
    /// is this person doing" in this process and this is a second READER of it,
    /// never a second SOURCE.
    ///
    /// # EVERY standing card, never "the selected one"
    ///
    /// This asked `View::selected()` first, and that was wrong on the operator's
    /// own box within minutes: a session holds one overview window per
    /// department they have clicked — `__overview__:executive` and
    /// `__overview__:research` at once — and only one of those can be the
    /// selection, so the other kept its first reading for ever. Measured: a rail
    /// reading `Research 0/3` beside a card reading `1 up`, which is the defect
    /// this function exists to end, surviving inside the repair for it.
    ///
    /// The selection says what the operator is LOOKING at. It has never said
    /// what is TRUE, and a card is a claim about a department rather than about
    /// a cursor.
    fn repaint_department_cards(&self) {
        for overview in effects::standing_overviews(self.tmux.as_ref(), &self.session) {
            let card = self.department_card_launch(&overview.department_id);
            effects::refresh_department_card(
                self.tmux.as_ref(),
                &self.session,
                &overview,
                card.as_deref(),
            );
        }
    }

    /// Paint one roster-backed sleeping card and return its exact final pane.
    fn show_sleeping_person_card(&self, id: &str, notice: CardNotice<'_>) -> Option<String> {
        let rows = self.view.people();
        let row = rows.iter().find(|row| row.id == id)?;
        let name = row.name.clone();
        let role = row.title.clone();
        let accent = self.accents.get(id).cloned().unwrap_or_default();
        let program = super::rail_program()?;
        let model = self
            .models
            .get(id)
            .cloned()
            .unwrap_or_else(crate::actuate::launch_catalog::PersonModel::unavailable);
        let launch = vec![
            program,
            "sleeping-person-card".to_owned(),
            id.to_owned(),
            name.clone(),
            role.clone(),
            model.state.as_str().to_owned(),
            model.provider.unwrap_or_default(),
            model.model.unwrap_or_default(),
            notice.wake_refusal().unwrap_or_default().to_owned(),
            notice.gate_refusal().unwrap_or_default().to_owned(),
        ];
        effects::show_sleeping_focus(
            self.tmux.as_ref(),
            &self.session,
            &effects::FocusPerson {
                person_id: id,
                name: &name,
                role: &role,
                accent: &accent,
                standing: None,
            },
            &launch,
        )
    }

    /// Whether one roster identity can still own an operator action.
    /// Departed rows remain in the durable roster, so raw id membership is not
    /// an employment check.
    fn person_is_operational(&self, id: &str) -> bool {
        self.placement.as_ref().is_some_and(|(roster, _)| {
            roster.people.iter().any(|person| person.id == id && !person.departed())
        })
    }

    /// Carry out what a click meant.
    ///
    /// **SYNCHRONOUS, END TO END.** There is not one `.await` in this function,
    /// and that is a property Stage 3 establishes rather than an accident: the
    /// hit test is a lookup, the selection is a field assignment, and the tmux
    /// verbs go over a control-mode client measured at 0.086ms each. The only
    /// durable consequence — a sleeper's wake — is `tokio::spawn`ed after the
    /// glass has already answered.
    fn perform(&mut self, action: Action, gesture: GestureId, _pane: &str) {
        // TOMBSTONE: a department row USED to be rewritten into a click on its
        // manager here, so that a sleeping manager reached the card path. It
        // took the department row's own meaning away to do it: the operator's
        // standing ruling is that clicking a department shows that department --
        // everybody in it who is up, moved back onto the glass -- and they
        // reported its loss as a regression the day it shipped. It shipped
        // unnoticed because the commit that added it did not compile, so nobody
        // ever ran it. A sleeping manager still gets the card path from their
        // OWN row, which is the row that names them.
        // ANY new gesture replaces the last one. A pending zoom is the
        // operator's most recent request; a person waking three passes after
        // they moved on must not take the glass off whatever they moved on to.
        //
        // A DISCLOSURE TOGGLE IS NOT A NEW GESTURE FOR THIS PURPOSE, because it
        // NAVIGATES NOWHERE: its whole arm is `toggle_department_disclosure`,
        // it selects nothing, it shows nothing, and it reports
        // `moved_geometry = false`. Parking on it was the second half of the
        // operator's BUG 1 and it survived the first cut of this change: the
        // measured sequence was a click on a sleeping person, then a triangle,
        // and the triangle killed that person's card into the parked "Click a
        // person in the sidebar to see them here alone." notice while the rail
        // went on highlighting them — the same two-halves disagreement the
        // CEO landing produced, reached by a different path. Worse, the next
        // pass's `tidy_selection` re-carded them by ANNOUNCING "«name» is no
        // longer up", which is false for somebody who was only ever asleep,
        // and it repeated on every triangle click because the re-card resets
        // `gone`. The same holds for a pending zoom and for a refused wake:
        // parking them on a toggle leaves the glass on a notice under a rail
        // that names a person, which is the one thing this brain must not do.
        let navigates = !matches!(action, Action::Ignored | Action::ToggleDepartmentDisclosure(_));
        let repeats_pending = matches!(
            &action,
            Action::FocusPerson { person_id, .. }
                if self.pending_zoom.as_deref() == Some(person_id.as_str())
        );
        if navigates && !repeats_pending {
            if let Some(person) = self.pending_zoom.take() {
                effects::park_waking_focus(self.tmux.as_ref(), &self.session, &person);
            }
        }
        // EVERY gesture, including one that moves no geometry. `gestured_at`
        // records only geometry transits (it gates repaint suppression);
        // enforcement needs "when did the operator last decide", which is a
        // different question. A new gesture also clears the enforcement
        // episode: the operator has spoken again, so whatever diverged before
        // is no longer the thing being completed.
        if !matches!(action, Action::Ignored) {
            self.clicked_at = Some(Instant::now());
            self.enforced_for = None;
        }
        let targets_a_person = matches!(&action, Action::FocusPerson { .. });
        if navigates && !targets_a_person {
            if let Some((person, _)) = self.sleeping_card.take() {
                effects::park_sleeping_focus(self.tmux.as_ref(), &self.session, &person);
            }
        }
        // DID THIS GESTURE MOVE ANY GEOMETRY? Every arm below answers, and the
        // answer is what arms the settle pass — see the block at the end.
        let moved_geometry;
        match action {
            Action::Ignored => return,
            Action::SelectDepartment(id) => {
                self.view.select(&id);
                // A CLICK IS A TRANSITION. Whatever notice was last put up is
                // spent, so the company-read path is free to put up the right
                // one — and only once.
                self.noticed = None;
                self.gesture = Some(gesture.raw());
                // The department's own NAME, because the only thing the
                // operator can be told about a department with no window is a
                // company fact ("nobody is up in Quant"), and an id is not one.
                let name = self
                    .view
                    .departments()
                    .iter()
                    .find(|row| row.id == id)
                    .map_or_else(|| id.clone(), |row| row.name.clone());
                // MOVING THEM BACK. The operator's ruling: click a person to
                // move him into a window of his own, click the department to
                // move him back. ANY department click returns whoever is out.
                // A DEPARTMENT SHOWS ITSELF, NOT ITS PEOPLE.
                //
                // This used to move every live person in the unit back onto the
                // glass, tiled. Six agents in a 129x36 window is 42x17 each, so
                // every one of them RENDERED at 42 columns, and the moment the
                // operator clicked one it was moved into the full-width focus
                // body and repainted at 129. Their report, 2026-08-21: *"it
                // always starts half screen and then resizes full screen so
                // it's very jarring"*. A pane has exactly one size; a pane that
                // is ever shown in a grid cell is a pane rendered at grid-cell
                // width, and no amount of care at the click removes that.
                //
                // Their ruling: *"every agent lives in its own kind of thing so
                // there's no flickering, and when I click on the department show
                // me an overview… something simple, something valuable, some
                // good metadata"*. So the department window now holds ONE card
                // that reads the roster this brain is already carrying — who
                // heads the unit, who is in it, what each of them is doing and
                // what each is running. It touches no agent, so there is
                // nothing here to resize and nothing to reflow.
                //
                // TOMBSTONE: `effects::show_department` and the ruling it served
                // ("click a person to move him into a window of his own, click
                // the department to move him back") are retired by the ruling
                // above. The RETURN half is what carried the cost, and the card
                // replaces the reason for it.
                let _ = &name;
                moved_geometry = self.show_department_overview(&id).moved_geometry;
            }
            Action::ToggleDepartmentDisclosure(id) => {
                self.view.toggle_department_disclosure(&id);
                self.gesture = Some(gesture.raw());
                moved_geometry = false;
            }
            Action::FocusPerson { department_id, person_id: id } => {
                // The clicked row owns its department. A second department can
                // stay expanded while another is selected, so the person id
                // alone cannot identify which disclosed branch was clicked.
                self.view.select(&department_id);
                // The mark moves on the CLICK, not on the outcome. The operator
                // pointed at this row and the rail must say so even if the pane
                // has since gone.
                self.view.select_person(&id);
                self.gesture = Some(gesture.raw());
                let state =
                    self.view.people().iter().find(|row| row.id == id).map(|row| row.state());
                if let Some(state) = state.filter(|state| !state.is_live()) {
                    let name = self
                        .view
                        .people()
                        .iter()
                        .find(|row| row.id == id)
                        .map_or_else(|| id.clone(), |row| row.name.clone());
                    let role = self.view.people().iter().find(|row| row.id == id).map_or_else(
                        || super::TEAM_MEMBER_DISPLAY_ROLE.to_owned(),
                        |row| row.title.clone(),
                    );
                    if state == super::PersonState::Refused {
                        // A CLICK ON A REFUSED PERSON GETS THE REASON, NOT A
                        // PROMISE. Everything below this arm announces
                        // "waking…" and asks chiefd to start somebody; for a
                        // person chiefd's own gate has declined that is the
                        // same false promise the row used to make, said again
                        // out loud. The gate names a repair — which files, in
                        // whose home — so the click hands that sentence
                        // straight to the operator and nothing else happens.
                        // The next pass re-derives the refusal, so it clears
                        // itself the moment the repair lands.
                        //
                        // ON THE FOCUS BODY, NOT ONLY IN A STATUS FLASH. The
                        // first version of this arm announced and stopped
                        // there, and `announce` is `tmux display-message` — one
                        // line, for `display-time`, on a session this product
                        // runs with `status off`. The operator clicked to find
                        // out WHY, the rail mark moved onto the row, the focus
                        // body went on showing somebody ELSE's card, and the
                        // whole of the answer had already gone. The card is
                        // where every other person click puts its answer, so
                        // the refusal goes there too — and it carries no
                        // button, because there is nothing to press.
                        let reason = self
                            .view
                            .people()
                            .iter()
                            .find(|row| row.id == id)
                            .and_then(|row| row.refused.clone())
                            .unwrap_or_default();
                        effects::announce(
                            self.tmux.as_ref(),
                            &self.session,
                            &effects::launch_refused_notice(&name, &reason),
                        );
                        let shown =
                            self.show_sleeping_person_card(&id, CardNotice::CannotStart(&reason));
                        self.sleeping_card = shown.map(|pane| (id.clone(), pane));
                        self.carded_refusal =
                            self.sleeping_card.as_ref().map(|_| (id.clone(), reason.clone()));
                        moved_geometry = false;
                        tracing::info!(
                            event = "sidebar.wake.refused-by-gate",
                            session = %self.session,
                            person = %id,
                            reason = %reason,
                            carded = self.sleeping_card.is_some(),
                            "the operator clicked somebody chiefd's launch gate has declined; \
                             no wake was asked for and the gate's own reason is on their card"
                        );
                    } else if state == super::PersonState::Sleeping {
                        let shown = self.show_sleeping_person_card(&id, CardNotice::None);
                        self.sleeping_card = shown.map(|pane| (id.clone(), pane));
                        moved_geometry = false;
                        if let Some((_, pane)) = &self.sleeping_card {
                            tracing::info!(
                                event = "sidebar.sleeping-card.shown",
                                session = %self.session,
                                person = %id,
                                pane = %pane,
                                "the sleeping person's card is on the final focus body; no wake was requested"
                            );
                        } else {
                            tracing::warn!(
                                event = "sidebar.sleeping-card.refused",
                                session = %self.session,
                                person = %id,
                                diagnostic = "the permanent focus body did not provide exact sleeping-card authority",
                                "the selected sleeping person was not reported as shown because no exact final body was returned"
                            );
                        }
                    } else {
                        // WHAT THIS PERSON'S PANE WILL SAY. A person the
                        // actuator keeps restarting is not "starting"; the
                        // sentence they get names the retry number, how long it
                        // has been going on, and what went wrong.
                        let crashing = self
                            .view
                            .people()
                            .iter()
                            .find(|row| row.id == id)
                            .and_then(|row| row.crash.as_ref())
                            .map(super::CrashNotice::sentence);
                        if let Some((card, _)) = self.sleeping_card.take() {
                            effects::park_sleeping_focus(self.tmux.as_ref(), &self.session, &card);
                        }
                        self.view.mark_starting(&id);
                        effects::announce(
                            self.tmux.as_ref(),
                            &self.session,
                            &effects::asleep_notice(&name, state.tag()),
                        );
                        let accent = self.accents.get(&id).cloned().unwrap_or_default();
                        let _ = effects::show_waking_focus(
                            self.tmux.as_ref(),
                            &self.session,
                            &effects::FocusPerson {
                                person_id: &id,
                                name: &name,
                                role: &role,
                                accent: &accent,
                                // THE CRASH REPORT WHERE THE CLICK LANDS. This
                                // is the pane the operator was staring at while
                                // it said `Ivo is starting…` for ninety minutes.
                                standing: crashing.as_deref(),
                            },
                        );
                        // THE OTHER HALF OF THE GESTURE, remembered BEFORE the POST
                        // rather than after it. The pane does not exist yet, so the
                        // zoom is finished when the person's pane turns up.
                        self.pending_zoom = Some(id.clone());
                        // No temporary pane answers this click. Converge mints the
                        // final tagged pane, whose own startup wrapper paints the
                        // immediate frame before it execs Pi in place.
                        moved_geometry = false;
                        tracing::info!(
                            event = "sidebar.wake.requested",
                            session = %self.session,
                            person = %id,
                            state = state.tag(),
                            "the operator clicked somebody who is not up; the wake is on its way, \
                             and their final pane will paint its own startup frame"
                        );
                        // ONCE PER PERSON PER BURST. The glass answered every click
                        // above, because a click into silence is the worst outcome
                        // and all of those effects are idempotent. chiefd is asked
                        // once: the operator's log shows nine clicks in three and a
                        // half seconds on a lagging row, and five consecutive failed
                        // boots is what makes the actuator give up on somebody for
                        // good.
                        if self.waking.insert(id.clone()) {
                            self.post_wake(id.clone(), name, gesture);
                        } else {
                            tracing::info!(
                                event = "sidebar.wake.already-asked",
                                session = %self.session,
                                person = %id,
                                "a wake is outstanding for them; the glass answered the click \
                                 again but chiefd is not asked twice"
                            );
                        }
                    }
                } else {
                    if let Some((card, _)) = self.sleeping_card.take() {
                        effects::park_sleeping_focus(self.tmux.as_ref(), &self.session, &card);
                    }
                    // The person may have died between the draw and the click.
                    // `show_person` resolves the pane NOW and answers
                    // `shown: false` when there is none, so a stale click is a
                    // no-op rather than a move of whoever inherited the pane id.
                    moved_geometry = self.show_person(&id).moved_geometry;
                }
            }
            Action::ToggleCollapsed => {
                // A border release writes the expanded preference directly in
                // tmux. Read it at the next explicit sidebar gesture so a
                // drag followed by collapse/expand restores the dragged width
                // even if no later SIGWINCH reached the brain.
                self.expanded_columns =
                    effects::expanded_columns(self.tmux.as_ref(), &self.session);
                self.view.toggle_collapsed();
                let columns = if self.view.collapsed() {
                    RAIL_COLLAPSED_COLUMNS
                } else {
                    self.expanded_columns
                };
                // The one gesture whose whole purpose IS a resize: it moves the
                // rail's own border, deliberately, at the operator's request.
                moved_geometry = true;
                effects::set_collapsed_and_resize_all(
                    self.tmux.as_ref(),
                    &self.session,
                    self.view.collapsed(),
                    columns,
                );
            }
        }
        // A GESTURE IS SOMETHING WE DID; A RESIZE IS SOMETHING DONE TO US. The
        // size changes that follow a geometry-moving gesture within
        // `SETTLE_AFTER` are OUR transit and must not be painted; a resize with
        // no such gesture outstanding is the operator's hand on the border and
        // must follow the pointer.
        //
        // STAGE 4 SHRANK THIS TO THE GESTURES THAT ACTUALLY MOVE SOMETHING.
        // Every gesture used to stamp it, because every gesture used to churn
        // topology — a person click minted a window, a department click killed
        // one, and both re-laid whatever they touched. Navigation moves nothing
        // now, so a department click that returns nobody arms no settle at all:
        // there is no transit to wait out, and pretending otherwise would make
        // the brain withhold the next 300ms of resizes the OPERATOR asked for.
        if moved_geometry {
            self.gestured_at = Some(Instant::now());
            self.arm_settle();
        }
        self.publish_focus();
        // CONVERGE, NOW. The process that spawns panes used to learn of a click
        // only on the next changefeed wake — measured at 2,831ms and 4,477ms.
        // It is the same process now, so it learns immediately.
        self.nudge.notify_one();
    }

    /// Ask chiefd to wake somebody, on a task nothing waits for.
    ///
    /// THIS ORDERING IS THE WHOLE GESTURE. `wake_person` was once awaited
    /// inline, at a 3115ms median, with everything the operator can see on the
    /// far side of it. Nothing on the glass depends on its answer: a grant
    /// commits `activity`, which is on the changefeed, so converge wakes and
    /// the pane arrives; a refusal is reconciled backward through
    /// [`WakeAnswer`].
    ///
    /// NOTHING IN HERE MAY TOUCH THE BRAIN. Every field the gesture wrote has
    /// exactly one owner — the loop — and that is what the channel is for.
    fn post_wake(&self, person: String, name: String, gesture: GestureId) {
        let client = Arc::clone(&self.client);
        let answers = self.answers.clone();
        let session = self.session.clone();
        // THE ID TRAVELS WITH THE TASK. The POST outlives the handler that
        // spawned it, so the answer lands with no enclosing span of its own;
        // stating the id explicitly is what keeps the daemon's cost attached to
        // the click that paid it.
        let raw = gesture.raw();
        tokio::spawn(async move {
            // THE DAEMON'S COST, MEASURED WHERE IT NOW LIVES. It used to be the
            // click's cost and could be read off the click's own latency; off
            // the path it would be invisible, and "the wake is slow but nobody
            // waits for it" is a claim that has to stay checkable.
            let started = Instant::now();
            let answer = client.wake_person(&person).await;
            let elapsed_ms = started.elapsed().as_millis();
            let refusal = match answer {
                Ok(()) => {
                    tracing::info!(
                        event = "sidebar.wake.answered",
                        session = %session,
                        gesture_id = raw,
                        person = %person,
                        elapsed_ms,
                        granted = true,
                        "chiefd granted the wake; the glass has been showing it since the click"
                    );
                    None
                }
                Err(error) => {
                    tracing::warn!(
                        event = "sidebar.wake.answered",
                        session = %session,
                        gesture_id = raw,
                        person = %person,
                        elapsed_ms,
                        granted = false,
                        diagnostic = %error,
                        "chiefd refused the wake; the optimism the click painted is being \
                         withdrawn"
                    );
                    Some(error.to_string())
                }
            };
            let _ =
                answers.send(Event::Wake(Box::new(WakeAnswer { gesture, person, name, refusal })));
        });
    }

    /// Apply one Wake Up button action. The final pane changes from sleeping
    /// furniture to waking furniture before chiefd is asked.
    fn wake_from_card(&mut self, pane: &str, person: &str) -> bool {
        let Some((card_person, card_pane)) = self.sleeping_card.as_ref() else {
            return false;
        };
        let organization = self
            .placement
            .as_ref()
            .map(|(roster, _)| roster.company.slug.as_str())
            .unwrap_or_default();
        if self.waking.contains(person) && self.pending_zoom.as_deref() == Some(person) {
            return card_person == person
                && card_pane == pane
                && effects::waking_focus_is_exact(
                    self.tmux.as_ref(),
                    &self.session,
                    organization,
                    pane,
                    person,
                );
        }
        if card_person != person
            || card_pane != pane
            || self.view.selected_person() != Some(person)
            || self.desired.contains(person)
        {
            return false;
        }
        if !super::authorize_sleeping_card(
            self.tmux.as_ref(),
            &self.session,
            organization,
            pane,
            person,
        ) {
            tracing::warn!(
                event = "sidebar.sleeping_card.wake_rejected",
                session = %self.session,
                pane,
                person,
                "the sleeping card pane changed before its guarded wake action; the card stays actionable"
            );
            return false;
        }
        self.refused.remove(person);
        let name = self
            .view
            .people()
            .iter()
            .find(|row| row.id == person)
            .map_or_else(|| person.to_owned(), |row| row.name.clone());
        let gesture = super::gesture::next();
        self.pending_zoom = Some(person.to_owned());
        self.view.mark_starting(person);
        self.gesture = Some(gesture.raw());
        self.waking.insert(person.to_owned());
        self.post_wake(person.to_owned(), name, gesture);
        self.publish_focus();
        self.render(Some(gesture));
        self.nudge.notify_one();
        true
    }

    /// Arm the settle pass, coalesced: a burst of gestures schedules one.
    fn arm_settle(&mut self) {
        if self.settle_at.is_none() {
            self.settle_at = Some(Instant::now() + SETTLE_AFTER);
        }
    }

    /// Push a frame to every attached client.
    fn render(&mut self, gesture: Option<GestureId>) {
        let view = &self.view;
        for seat in self.clients.values_mut() {
            seat.draw(view, gesture);
        }
    }

    /// The last word lands on settled geometry without changing a preference.
    /// Runtime geometry is never evidence of human intent. Only the explicit
    /// tmux rail-border release binding can write the expanded width.
    fn settle(&mut self) {
        self.settle_at = None;
        for seat in self.clients.values_mut() {
            seat.withheld = false;
        }
        self.render(None);
    }

    /// Write the border titles of every rail in the session, and of every
    /// person pane, from the roster last read.
    fn write_titles(&mut self) {
        let Some((roster, _)) = self.placement.as_ref() else {
            return;
        };
        let (names, roles) = roster_presentations(roster);
        let company = roster.company.display_name.clone();
        let rails: Vec<String> = self.clients.values().map(|client| client.pane.clone()).collect();
        self.titles = effects::write_pane_titles(
            self.tmux.as_ref(),
            &self.session,
            &rails,
            &company,
            effects::PersonChips { names: &names, roles: &roles, accents: &self.accents },
            &self.titles,
        );
    }

    /// A thin client attached.
    ///
    /// **Its first frame is a push**, which is the whole of a thin client's
    /// boot: it has no state to read, no company to fetch and nothing to wait
    /// for, so a freshly minted window's rail paints in one socket round trip.
    fn attach(&mut self, id: u64, pane: String, columns: u16, rows: u16, outbox: Arc<Mailbox>) {
        let Some((terminal, sink)) = fresh_terminal(columns, rows) else {
            outbox.close();
            return;
        };
        tracing::info!(
            event = "sidebar.client.attached",
            session = %self.session,
            pane = %pane,
            columns,
            rows,
            clients = self.clients.len() + 1,
            "a rail attached to this session's brain"
        );
        let attached_pane = pane.clone();
        self.clients.insert(
            id,
            Seat {
                pane,
                terminal,
                sink,
                outbox,
                last: Vec::new(),
                size: (columns, rows),
                withheld: false,
                owed: None,
            },
        );
        self.decoders.insert(id, Decoder::new());
        let view = &self.view;
        if let Some(seat) = self.clients.get_mut(&id) {
            seat.draw(view, None);
        }
        self.write_titles();
        let canonical =
            if self.view.collapsed() { RAIL_COLLAPSED_COLUMNS } else { self.expanded_columns };
        if i64::from(columns) != canonical {
            effects::apply_columns(self.tmux.as_ref(), &attached_pane, canonical);
        }
    }

    /// A thin client went away: its pane was killed, or its process ended.
    fn detach(&mut self, id: u64) {
        if let Some(seat) = self.clients.remove(&id) {
            seat.outbox.close();
            tracing::info!(
                event = "sidebar.client.detached",
                session = %self.session,
                pane = %seat.pane,
                clients = self.clients.len(),
                "a rail left this session's brain"
            );
        }
        self.decoders.remove(&id);
    }

    /// A client's pane changed size.
    ///
    /// # A SIZE THIS BRAIN'S OWN GESTURE PUT IN FLIGHT IS NOT DRAWN AT ALL
    ///
    /// tmux hands a dying pane's columns to its previous sibling — the rail —
    /// and takes them back when the layout that follows lands. Measured on the
    /// operator's box: `33 -> 137`, back at `33` about 250ms later, three times
    /// in three minutes. Painting that intermediate is what the operator sees
    /// as the sidebar leaping to half the screen and back.
    ///
    /// So the frame is SKIPPED, not drawn-and-corrected, and the settle pass
    /// draws once the geometry has stopped moving. What stays on the glass
    /// meanwhile is the last good frame, which is also the last TRUE one.
    fn resize(&mut self, id: u64, columns: u16, rows: u16) {
        let transit = self.gestured_at.is_some_and(|at| at.elapsed() < SETTLE_AFTER);
        let expanded = effects::expanded_columns(self.tmux.as_ref(), &self.session);
        if expanded != self.expanded_columns {
            self.expanded_columns = expanded;
            if !self.view.collapsed() {
                effects::resize_all_rails(self.tmux.as_ref(), &self.session, expanded);
            }
        }
        let effective =
            if self.view.collapsed() { RAIL_COLLAPSED_COLUMNS } else { self.expanded_columns };
        let viewport_changed_rows = self.clients.get(&id).is_some_and(|seat| seat.size.1 != rows);
        if !transit && viewport_changed_rows && i64::from(columns) != effective {
            // A horizontal rail-border drag does not change the window's row
            // count. An attached-client viewport change does. tmux has already
            // redistributed the active split when this SIGWINCH arrives, so
            // keep the last true frame and restore the human-owned effective
            // width before a frame for the temporary width can be sent.
            if let Some(seat) = self.clients.get_mut(&id) {
                seat.withheld = true;
            }
            effects::resize_all_rails(self.tmux.as_ref(), &self.session, effective);
            self.gestured_at = Some(Instant::now());
            self.arm_settle();
            return;
        }
        let Some(_pane) = self.clients.get_mut(&id).map(|seat| {
            let moved = seat.size != (columns, rows);
            seat.resize(columns, rows);
            seat.withheld = moved && transit;
            if seat.withheld {
                tracing::debug!(
                    event = "sidebar.rail.transit-skipped",
                    pane = %seat.pane,
                    to_width = columns,
                    to_height = rows,
                    "a gesture of ours is still in flight and the pane changed size; the \
                     frame that would have been painted at this size is skipped, and the \
                     settle pass draws once the geometry stops moving"
                );
            }
            seat.pane.clone()
        }) else {
            return;
        };
        if transit {
            self.arm_settle();
            return;
        }
        let view = &self.view;
        if let Some(seat) = self.clients.get_mut(&id) {
            seat.draw(view, None);
        }
        // A generic SIGWINCH never writes a preference. The explicit tmux
        // MouseDragEnd1Border binding is the only writer. The read above only
        // mirrors that completed human choice to sibling rails.
    }

    /// Decode one client's raw stdin bytes and act on every gesture in them.
    ///
    /// Answers whether the client should stay attached.
    fn input(&mut self, id: u64, bytes: &[u8]) -> bool {
        let Some((height, pane)) =
            self.clients.get(&id).map(|seat| (usize::from(seat.size.1), seat.pane.clone()))
        else {
            return false;
        };
        let Some(decoder) = self.decoders.get_mut(&id) else {
            return false;
        };
        for input in decoder.feed(bytes) {
            match input {
                Input::Quit => return false,
                Input::Click { column, row } => self.on_click(column, row, height, &pane),
                Input::ScrollUp { row } => self.scroll(height, row, -1),
                Input::ScrollDown { row } => self.scroll(height, row, 1),
            }
        }
        true
    }

    /// The wheel, over whichever section the pointer is in.
    fn scroll(&mut self, height: usize, row: usize, delta: isize) {
        if row < height.saturating_sub(1) {
            self.view.scroll(delta);
            self.render(None);
        }
    }

    /// One left click, from the mouse event to the frame that answers it.
    fn on_click(&mut self, column: usize, row: usize, height: usize, pane: &str) {
        // THE FIRST THING THAT HAPPENS TO A CLICK, before any work it causes.
        // Every line below is stamped with this id, and the id IS the click's
        // wall clock in microseconds, so the funnel needs no join and no
        // nearest-in-time guess.
        let gesture = super::gesture::next();
        let span = gesture.span();
        span.in_scope(|| {
            // BEFORE resolving the row, not after. Liveness is TMUX's fact and
            // chiefd emits no event for it, so nothing else can wake this loop
            // when a pane dies.
            self.resync_live();
            let action = click(&self.view, height, column, row);
            // THE RAW INPUTS, every click. The pure row->entity mapping is
            // pinned by tests and passes, so when the operator reports that a
            // click landed on the wrong row the question is what this function
            // was HANDED — which no unit test can see.
            tracing::info!(
                event = "sidebar.click",
                session = %self.session,
                column,
                row,
                height,
                control_row = height.saturating_sub(1),
                tree_scroll = self.view.scroll_offset(),
                resolved = ?action,
                "a click arrived"
            );
            self.perform(action, gesture, pane);
        });
        // THE FRAME THAT ANSWERS THE CLICK, pushed while the gesture is still
        // the freshest thing that happened. It carries the id, which is how the
        // client — the only process that can honestly say the bytes reached a
        // pty — names this click in `sidebar.frame.painted`.
        self.render(Some(gesture));
    }

    /// Apply one event from the brain's own channel.
    fn apply(&mut self, event: Event) {
        match event {
            Event::Attach { id, pane, columns, rows, outbox } => {
                self.attach(id, pane, columns, rows, outbox);
            }
            Event::Input { id, bytes } => {
                if !self.input(id, &bytes) {
                    self.detach(id);
                }
            }
            Event::Resize { id, columns, rows } => self.resize(id, columns, rows),
            Event::Detach { id } => self.detach(id),
            Event::Company(facts) => {
                self.absorb(*facts);
                self.render(None);
            }
            Event::Unreadable => {
                // A brain that HAS a company keeps drawing it — replacing a
                // real company with a notice would be a worse trade. But one
                // that has never read one has been drawing `…` since it booted,
                // and `…` promises an answer that is not coming.
                self.view.note_unreadable();
                self.render(None);
            }
            Event::Wake(answer) => {
                self.settle_wake(&answer);
                self.render(None);
            }
            Event::GeometryMoved => {
                self.gestured_at = Some(Instant::now());
                self.arm_settle();
            }
            Event::Describe { outbox } => {
                // WHAT THE RAIL IS DRAWING, not what chiefd holds: the harness
                // matches a row the operator can point at, and the root
                // department is drawn as "Executive" rather than by the company
                // name it is stored under.
                let mut people: Vec<(String, String)> = Vec::new();
                for rows in self.view.everybody().values() {
                    for row in rows {
                        if !people.iter().any(|(id, _)| *id == row.id) {
                            people.push((row.id.clone(), row.name.clone()));
                        }
                    }
                }
                outbox.put(ToClient::Company(Named {
                    departments: self
                        .view
                        .departments()
                        .iter()
                        .map(|row| (row.id.clone(), row.name.clone()))
                        .collect(),
                    people,
                }));
            }
            Event::WakeCard { pane, person, outbox } => {
                if self.wake_from_card(&pane, &person) {
                    outbox.put(ToClient::WakeAccepted { person });
                } else {
                    outbox.put(ToClient::WakeRejected { person });
                }
            }
        }
    }

    /// Drain the events channel until every sender is gone.
    async fn pump(&mut self, mut events: tokio::sync::mpsc::UnboundedReceiver<Event>) {
        use crate::actuate::resident::Wire as _;

        // Cloned out of `self` so the settle future borrows a local rather than
        // the brain the handlers mutate.
        let clock = Arc::clone(&self.client);
        loop {
            let due = self.settle_at;
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.apply(event),
                    None => return,
                },
                () = clock.delay(settle_in(due)), if due.is_some() => self.settle(),
            }
        }
    }
}

/// How long until the settle pass is due, or zero when it is overdue.
fn settle_in(at: Option<Instant>) -> Duration {
    at.map_or(Duration::ZERO, |at| at.saturating_duration_since(Instant::now()))
}

impl Seat {
    /// Render this client's frame and leave it in the mailbox.
    ///
    /// A GESTURE WHOSE FRAME WAS WITHHELD KEEPS ITS CLAIM, so the line the
    /// client eventually writes names the SETTLE pass's frame — the one the
    /// operator actually sees — and never the one that was withheld.
    fn draw(&mut self, view: &View, gesture: Option<GestureId>) {
        let claim = gesture.or(self.owed);
        if self.withheld {
            self.owed = claim;
            return;
        }
        if self.push(view, claim) {
            self.owed = None;
        }
    }
}

// ---------------------------------------------------------------------------
// The unix socket
// ---------------------------------------------------------------------------

/// Start this session's brain: bind its socket, serve it, and answer a handle.
///
/// # `path` is given rather than derived, and that is the library boundary
///
/// It used to be `socket_path(home, slug)` — `~/.chiefd/run/<slug>.rail.sock`,
/// a box-wide directory keyed by a display word. A company is a DIRECTORY now
/// and its rail socket is `<dir>/.chief/run/rail.sock`, which
/// `chief_cli::paths` names once for the whole program. This half is pure
/// placement arithmetic and owns no opinion about where a company lives, so the
/// binary hands it the path rather than this module growing a second speller of
/// `.chief/run`.
///
/// # Errors
/// Any failure to create the run directory or to bind the socket. A brain that
/// cannot listen is a session with no rail, which is a loud refusal rather than
/// a silent degradation.
pub async fn start(
    tmux: Arc<dyn Tmux>,
    client: Arc<ActuationClient>,
    session: String,
    company_dir: &Path,
    path: &Path,
) -> std::io::Result<Handle> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A SOCKET FILE OUTLIVES THE PROCESS THAT BOUND IT. An actuator that was
    // killed leaves the path behind and `bind` refuses an existing path, so the
    // stale entry is removed first. This is not company state: it is a
    // rendezvous point for two processes on one box, and it dies with the
    // session it serves.
    #[allow(
        clippy::disallowed_methods,
        reason = "a unix-socket rendezvous is not durable company state; the host-executor \
                  seam governs a company's files, and this is neither"
    )]
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    let (mut brain, events) = Brain::new(tmux, client, session.clone(), company_dir.to_path_buf());
    let handle = Handle {
        events: brain.answers.clone(),
        focus: Arc::clone(&brain.focus),
        nudge: Arc::clone(&brain.nudge),
    };
    tracing::info!(
        event = "sidebar.brain.listening",
        session = %session,
        socket = %path.display(),
        "this session's brain is serving its rails"
    );
    let accepting = brain.answers.clone();
    tokio::spawn(async move { accept(listener, accepting).await });
    tokio::spawn(async move { brain.pump(events).await });
    Ok(handle)
}

/// Accept thin clients for the life of the session.
async fn accept(
    listener: tokio::net::UnixListener,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    let mut next: u64 = 0;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                next = next.saturating_add(1);
                let events = events.clone();
                let id = next;
                tokio::spawn(async move { converse(id, stream, events).await });
            }
            Err(error) => {
                // A listener that will not accept is a session that can take no
                // more rails, and there is nothing to retry against: the socket
                // is gone, or the process is out of descriptors. Said once.
                tracing::error!(
                    event = "sidebar.brain.unlistening",
                    diagnostic = %error,
                    "this session's brain can no longer accept rails"
                );
                return;
            }
        }
    }
}

/// One connection: raw bytes up, whole frames down.
async fn converse(
    id: u64,
    stream: tokio::net::UnixStream,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (mut incoming, mut outgoing) = stream.into_split();
    let outbox = Arc::new(Mailbox::new());
    let writing = Arc::clone(&outbox);
    // THE WRITER IS ITS OWN TASK, so a client that stops reading blocks nothing
    // but itself. The mailbox behind it is one slot, latest wins.
    let writer = tokio::spawn(async move {
        while let Some(message) = writing.take().await {
            if outgoing.write_all(&message.encode()).await.is_err() {
                return;
            }
        }
    });
    let mut frames = Frames::new();
    let mut buffer = vec![0_u8; 8192];
    let mut attached = false;
    'reading: while let Ok(count) = incoming.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        frames.feed(buffer.get(..count).unwrap_or_default());
        loop {
            match frames.next_to_brain() {
                Ok(Some(ToBrain::Hello { protocol, pane, columns, rows })) => {
                    if protocol != PROTOCOL {
                        tracing::warn!(
                            event = "sidebar.client.mismatched",
                            pane = %pane,
                            protocol,
                            expected = PROTOCOL,
                            "a rail speaks a protocol this brain does not; it is dropped \
                             rather than decoded, and converge mints a fresh one"
                        );
                        break 'reading;
                    }
                    attached = true;
                    let _ = events.send(Event::Attach {
                        id,
                        pane,
                        columns,
                        rows,
                        outbox: Arc::clone(&outbox),
                    });
                }
                Ok(Some(ToBrain::Input(bytes))) => {
                    let _ = events.send(Event::Input { id, bytes });
                }
                Ok(Some(ToBrain::Resize { columns, rows })) => {
                    let _ = events.send(Event::Resize { id, columns, rows });
                }
                Ok(Some(ToBrain::Describe)) => {
                    let _ = events.send(Event::Describe { outbox: Arc::clone(&outbox) });
                }
                Ok(Some(ToBrain::WakePerson { protocol, pane, person })) => {
                    if protocol != PROTOCOL {
                        break 'reading;
                    }
                    let _ =
                        events.send(Event::WakeCard { pane, person, outbox: Arc::clone(&outbox) });
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(
                        event = "sidebar.client.unreadable",
                        diagnostic = %error,
                        "a rail's stream cannot be framed any further; it is dropped"
                    );
                    break 'reading;
                }
            }
        }
    }
    outbox.close();
    writer.abort();
    if attached {
        let _ = events.send(Event::Detach { id });
    }
}

#[cfg(test)]
mod tests;
