//! `chief actuate <company>` — the resident actuator.
//!
//! # Why this is a resident process and not a one-shot verb
//!
//! Before P8, a person's Pi process was a child of a tmux pane the **daemon**
//! created, and its lifetime was the pane's; the daemon was always there, so
//! somebody was always able to start a person. After P8 the **client** creates
//! the pane. A one-shot `chiefd converge` would actuate once and exit, and the
//! next time chiefd wanted somebody started there would be nobody to do it. So
//! the client has to stay.
//!
//! # The loop
//!
//! ```text
//!   read the DESIRED SET from chiefd    (who, and the hash of what)
//!     → read the roster and the launch catalog
//!     → observe local tmux
//!     → diff, and make reality match
//!     → wait for a wake
//!     → repeat
//! ```
//!
//! **Nothing goes up.** There is no report, no lease and no enrolment. chiefd
//! holds the desired state; what is on this box is this process's business and
//! travels no further than the operator's screen. Whether a person is moved
//! between windows or killed and resumed, whether a pane was adopted or
//! replaced, whether anything is running at all — chiefd learns none of it, by
//! construction, because there is no verb left to tell it with.
//!
//! **It waits, it does not poll.** Work arrives over chiefd's SSE changefeed; a
//! change to the roster, the supervision ledger, the manifest or the safety
//! scaffold wakes the loop. [`Schedule::idle_wait`] is a ceiling on how long it
//! will sit on one connection, not a sampling interval.
//!
//! # What it decides, and what it does not
//!
//! chiefd decides WHO runs and WHAT they run — the desired set is both, and
//! this loop never second-guesses either. Everything about HOW is decided here
//! and only here: which window a pane sits in, whether a moved person is
//! relocated with `break-pane` or killed and resumed, what order the windows
//! are in, and — the one genuinely new decision — when to stop trying to boot
//! somebody who will not stay up. That last one is
//! [`crate::actuate::crash_loop`], and it exists because chiefd can no longer
//! notice a crash loop and this process can.
//!
//! # TOMBSTONE: the observation, the lease, the ramp
//!
//! `Observed`, `observation()`, `steps_for()` and `admission_delay()` are gone.
//! The first two produced a report nothing accepts any more. `steps_for`
//! translated chiefd's four person verbs into steps, and there are no verbs: a
//! verb is a statement about a TRANSITION, and only the thing that can see the
//! current state can compute one. That thing is [`super::plan`], which was
//! already here, already the real diff engine, and is now the only one.
//!
//! `admission_delay` paced starts against chiefd's ramp. The ramp is deleted by
//! operator ruling and every missing pane is booted in the pass that finds it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::actuate::client::{ActuationClient, ActuationError, Wake};
use crate::actuate::crash_loop::{crash_loop_line, CrashLoop, CrashReport};
use crate::actuate::desired::{DesiredRuntime, HoldReason};
use crate::actuate::ever_observed::EverObserved;
use crate::actuate::host::{HostExecutor, Socket};
use crate::actuate::interpret::{LaunchInputs, LaunchRosterDiagnostics, PassContext};
use crate::actuate::launch_catalog::ResolvedCatalog;
use crate::actuate::plan::{self, ObservedTopology};
use crate::placement::Topology;

/// The loop's timing floors, as a value.
///
/// A value rather than four constants because the loop's timing is a decision,
/// and a decision buried inside an `async` body can only be exercised by
/// actually waiting for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// The shortest interval between two consecutive rounds.
    ///
    /// Not a poll interval — nothing is sampled here. It is the floor that
    /// keeps a pathological wake (a changefeed that opens and immediately
    /// closes, a ring replay that resolves to an event already acted on) from
    /// turning into an unbounded spin against tmux. Every wake still arrives
    /// immediately; only a *second* round inside the same interval waits.
    pub min_round_interval: Duration,
    /// The first backoff after a transport failure, doubled up to
    /// [`Schedule::max_retry`].
    pub first_retry: Duration,
    /// The ceiling of the transport backoff.
    pub max_retry: Duration,
    /// The longest this loop will sit on one changefeed connection.
    ///
    /// This replaced `RuntimeActionPlan::renew_after_ms`, and it is a different
    /// KIND of number: the old one was a lease renewal deadline handed down by
    /// chiefd, because an actuator that went quiet lost its lease. There is no
    /// lease. What is left is a local safety net — a feed that has silently
    /// stopped delivering looks exactly like a quiet company, and without a
    /// ceiling this process would park on it for ever.
    pub idle_wait: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            min_round_interval: Duration::from_secs(1),
            first_retry: Duration::from_secs(1),
            max_retry: Duration::from_secs(30),
            idle_wait: Duration::from_secs(30),
        }
    }
}

impl Schedule {
    /// No floors at all: every round runs the moment the last one finished.
    ///
    /// For exercising the loop's decisions without waiting for them. Never for
    /// production — the floors are what stop a misbehaving peer from turning
    /// this into a spin.
    #[must_use]
    pub const fn eager() -> Self {
        Self {
            min_round_interval: Duration::ZERO,
            first_retry: Duration::ZERO,
            max_retry: Duration::ZERO,
            idle_wait: Duration::ZERO,
        }
    }
}

/// How this actuator identifies itself on its own screen.
///
/// Host plus pid, so an operator reading a line from one of several actuators
/// can walk to the right machine and find the right process. It goes nowhere
/// near the wire — there is no wire for it to go on — and it grants nothing.
#[must_use]
pub fn actuator_id(hostname: &str) -> String {
    format!("chiefd-cli@{hostname}#{}", std::process::id())
}

/// What one round of the loop concluded, for the operator's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    /// Steps the diff called for.
    pub requested: usize,
    /// Steps this client applied.
    pub applied: usize,
    /// Whether the pass STOPPED on a failure.
    ///
    /// Separate from `applied < requested`, because the two are not the same
    /// fact and the live proof caught them disagreeing. A pass that minted a
    /// pane and then failed reported a count that matched and printed a claim
    /// of success once per second, forever, while nothing was running.
    pub failed: bool,
    /// Why chiefd is holding actuation, when it is.
    pub hold: Option<HoldReason>,
    /// The people the gate declined this pass, person id to the gate's reason.
    ///
    /// On the round line beside the count, because a pass that applied every
    /// step it could and left two people un-launched has not converged, and the
    /// only useful thing it can say is who and why.
    pub refused: BTreeMap<String, String>,
    /// How many people chiefd wants running at all, this pass.
    ///
    /// Not derivable from `requested`, and that gap is the whole reason this
    /// field exists. `requested` counts PLAN STEPS, so it is zero both when a
    /// company is fully up with nothing left to do AND when chiefd is asking
    /// for nobody. Those are opposite conditions that printed the same word.
    pub desired_people: usize,
    /// How many people tmux was actually holding when this pass planned.
    ///
    /// `None` when this pass never got as far as observing: a hold, or a
    /// catalog it could not read. Absent is not zero, and a pass that did not
    /// look must not report an empty company.
    ///
    /// THE THIRD OPPOSITE CONDITION behind `requested == 0`. A live company
    /// printed `converged · 17 up` once a second for an hour while tmux held
    /// seven people: the count came from the DESIRED set, so it restated the
    /// question as if it were the answer, and ten people who were never
    /// started read as ten people running.
    pub observed_people: Option<usize>,
    /// Everybody who is crash-looping, and what the operator is told about
    /// each: the retry number, how long it has been going on, when the next
    /// attempt is due, and what went wrong.
    ///
    /// Present on EVERY round it is true of, not once. A person who will not
    /// stay up is a live condition, and the operator's question — *how long has
    /// this been going on* — is answered by a line that keeps saying so.
    pub crashing: BTreeMap<String, CrashReport>,
}

/// What applying one plan achieved.
///
/// `count` is what was actually done, never what was asked for: the interpreter
/// is fail-stop, and the difference between "applied 9 of 9" and "applied 7 of
/// 9" is the difference between a converged company and a stuck one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    /// Steps the diff called for.
    pub requested: usize,
    /// How many of them this client carried out.
    pub count: usize,
    /// The failure that stopped the pass, in the words the operator needs.
    pub failure: Option<String>,
    /// The people this pass SKIPPED because chiefd's launch gate declined them,
    /// person id to the gate's own reason.
    ///
    /// Not a failure and not silence. The pass did everything else it planned;
    /// these people did not come up, and the operator is told which ones and
    /// why, on the same line that reports the pass.
    pub refused: BTreeMap<String, String>,
    /// People tmux held when this pass planned, or `None` if it never looked.
    pub observed_people: Option<usize>,
    /// Everybody this pass knows to be crash-looping, with their retry count,
    /// elapsed time and last error.
    ///
    /// Carried out of the pass rather than printed inside it so the report
    /// lands on the round line an operator already reads, beside the refused,
    /// instead of on a line of its own that a busy pane scrolls past.
    pub crashing: BTreeMap<String, CrashReport>,
}

impl Applied {
    /// Nothing was applied, and nothing was asked for.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            requested: 0,
            count: 0,
            failure: None,
            refused: BTreeMap::new(),
            observed_people: None,
            crashing: BTreeMap::new(),
        }
    }

    /// A pass that concluded nothing, for a named reason.
    ///
    /// NOT the same value as [`Applied::none`], and the difference is the whole
    /// fail-safe property: `none` means *converged, there was nothing to do*,
    /// and this means *I could not look, so I did nothing*. Collapsing the two
    /// is the "unreadable becomes empty" conflation this entire change exists
    /// to remove, one layer down from where it used to live.
    #[must_use]
    pub fn blocked(reason: String) -> Self {
        Self {
            requested: 0,
            count: 0,
            failure: Some(reason),
            refused: BTreeMap::new(),
            observed_people: None,
            crashing: BTreeMap::new(),
        }
    }
}

/// The local runtime this actuator drives.
///
/// A seam, so the loop's decisions are exercised as values against a scripted
/// runtime rather than against a tmux server.
#[allow(
    async_fn_in_trait,
    reason = "one crate-local implementor per trait, both awaited in place; no `Send` bound is \
              needed because nothing here is spawned"
)]
pub trait Actuator {
    /// Make the local runtime match `desired`.
    ///
    /// The observation and the diff happen INSIDE this call, in that order,
    /// with nothing between them. That is not an implementation detail: it is
    /// the only arrangement in which what was seen and what was done about it
    /// are the same instant. The old design observed in one call, sent the
    /// observation to chiefd, and applied a plan chiefd computed from it; the
    /// gap that opened was closed by a per-step precondition re-verify, which
    /// [`super::interpret`] still performs and which is now defence in depth
    /// rather than the load-bearing beam.
    async fn converge(&mut self, desired: &DesiredRuntime) -> Applied;
}

/// The chiefd side of the loop.
///
/// The second seam, for the same reason as the first: a loop that could only be
/// driven by a live daemon is a loop whose refusal handling is never tested.
#[allow(
    async_fn_in_trait,
    reason = "one crate-local implementor per trait, both awaited in place; no `Send` bound is \
              needed because nothing here is spawned"
)]
pub trait Wire {
    /// The composite document key every request body carries.
    fn document_key(&self) -> &str;

    /// Read what chiefd wants running.
    async fn desired(&self) -> Result<DesiredRuntime, ActuationError>;

    /// Park on the changefeed until a wake, or until `budget`.
    async fn wait(&self, after: Option<u64>, budget: Duration) -> Result<Wake, ActuationError>;

    /// Wait, without listening for anything.
    ///
    /// The two waits this loop performs that are NOT changefeed parks — the
    /// transient-error backoff and the minimum round interval — go through the
    /// seam rather than calling `tokio::time::sleep` directly, for the reason
    /// `clippy.toml` disallows that method by name: *all waiting flows through
    /// the injected Clock so tests never sleep*.
    async fn delay(&self, duration: Duration);
}

// The inherent methods of the same names are what these forward to — inherent
// impls win resolution over trait impls, so the paths are spelled out rather
// than left to `Self::`, which reads like recursion to anybody skimming.
impl Wire for ActuationClient {
    fn document_key(&self) -> &str {
        ActuationClient::document_key(self)
    }

    async fn desired(&self) -> Result<DesiredRuntime, ActuationError> {
        ActuationClient::desired(self).await
    }

    async fn wait(&self, after: Option<u64>, budget: Duration) -> Result<Wake, ActuationError> {
        ActuationClient::wait(self, after, budget).await
    }

    // THE one sanctioned `tokio::time::sleep` in this crate. `clippy.toml`
    // disallows the method so that waiting cannot be scattered; the rule's
    // purpose is served by there being exactly one call, behind the seam every
    // test substitutes. A second one anywhere is the defect.
    #[expect(
        clippy::disallowed_methods,
        reason = "the injected-Clock seam has to bottom out in a real sleep somewhere; this is it"
    )]
    async fn delay(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Decide what a round did.
///
/// Pure, so the rule is a value rather than a shape buried in an async loop.
/// The whole [`Applied`] is passed in rather than only its count: a fail-stop
/// interpreter can stop part-way, so the two numbers agreeing is not evidence
/// the pass succeeded. The failure it already carries is the authority.
#[must_use]
pub fn round_outcome(desired: &DesiredRuntime, applied: &Applied) -> Round {
    Round {
        requested: applied.requested,
        applied: applied.count,
        failed: applied.failure.is_some(),
        hold: desired.hold,
        refused: applied.refused.clone(),
        desired_people: desired.people.len(),
        observed_people: applied.observed_people,
        crashing: applied.crashing.clone(),
    }
}

/// The operator-facing line for one round.
///
/// A held round says WHY. "nothing to do" and "chiefd is refusing to act" look
/// identical from the outside otherwise, and the second one is the one somebody
/// has to act on.
#[must_use]
pub fn round_line(company: &str, round: &Round) -> String {
    format!(
        "{}{}{}",
        round_state_line(company, round),
        refused_clause(round),
        crashing_clause(company, round)
    )
}

/// The clause naming everybody who is crash-looping, with the numbers the
/// operator asked for.
///
/// ON THE ROUND LINE, because `chief` calls `chiefd_log::console_off()` at
/// start-up — *no log ever reaches a screen* — so every actuator event lands in
/// `.chief/log/chief.jsonl` and nothing else. A signal an operator cannot see
/// while watching the pane it is about is silence in a smaller room.
///
/// EVERY ROUND, not once. This replaces a design that gave up after five
/// failures and announced the give-up exactly once; the give-up is gone, so
/// there is no transition to announce and nothing that stops. What there is
/// instead is a condition that persists, and the operator's own requirement was
/// to be able to look at the screen at any moment and know *what broke* and
/// *how long it has been going on*. A line printed once, an hour ago, answers
/// neither.
fn crashing_clause(company: &str, round: &Round) -> String {
    if round.crashing.is_empty() {
        return String::new();
    }
    let mut clause = String::new();
    for (person_id, report) in &round.crashing {
        clause.push_str(" · ");
        clause.push_str(&crash_loop_line(company, person_id, report));
    }
    clause
}

/// The clause naming everybody chiefd's gate declined this pass, or nothing.
///
/// ON EVERY LINE, whatever else the pass did, because a pass that applied every
/// step it could and still left two people un-launched has not converged — and
/// the previous version of this said nothing at all about them, because a
/// refusal aborted the pass and the operator got the abort instead.
///
/// Named, never counted. "2 people were refused" sends somebody to the log to
/// find out who; the names and the daemon's own reason are what they would go
/// there for.
fn refused_clause(round: &Round) -> String {
    if round.refused.is_empty() {
        return String::new();
    }
    let named = round
        .refused
        .iter()
        .map(|(person, reason)| format!("{person} ({reason})"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        " · chiefd's launch gate REFUSED {} of them, so their step was skipped and the rest of \
         this plan still ran: {named}",
        round.refused.len()
    )
}

fn round_state_line(company: &str, round: &Round) -> String {
    if let Some(hold) = round.hold {
        return format!("{company}: {}", hold.explain());
    }
    if round.failed {
        // A pass that FAILED never claims the plan. "applied 1 step(s)"
        // underneath a failure line read as a success that happened to log a
        // warning — which is exactly how a company that started nobody looked
        // converged for seven minutes.
        return format!(
            "{company}: the pass FAILED after {} of {} step(s); nothing beyond that was attempted \
             and the rest of this plan did not happen",
            round.applied, round.requested
        );
    }
    if round.requested == 0 {
        // CONVERGED TO NOTHING IS NOT CONVERGED, and saying so cost a live
        // company. A freshly created company whose CEO nobody had asked for
        // printed `converged` once a second while `requested=0 applied=0` and
        // no tmux session ever appeared. The Founder agent read that word,
        // reported "the company is live, its CEO was booted", and the operator
        // believed it — because "converged" is the vocabulary of a healthy
        // steady state and there was nothing beside it to say otherwise.
        //
        // Both conditions produce zero STEPS. Only one of them is a company.
        if round.desired_people == 0 {
            return format!(
                "{company}: converged, but chiefd is asking for NOBODY to run — this company \
                 has no runtime. Nothing is wrong with tmux; the desired set is empty."
            );
        }
        // CONVERGED MEANS THE WORLD MATCHES THE ASK. A plan with no steps
        // while people are missing is the opposite: nobody is left to start
        // them, and every later pass will say the same thing. Name the gap,
        // because the word `converged` is what stopped anyone looking.
        if let Some(observed) = round.observed_people {
            if observed < round.desired_people {
                return format!(
                    "{company}: NOT converged · chiefd wants {} people and tmux holds {}; this                      plan asked for NOTHING, so the {} missing will not be started by it",
                    round.desired_people,
                    observed,
                    round.desired_people - observed
                );
            }
            return format!("{company}: converged · {observed} up");
        }
        return format!("{company}: converged · {} up", round.desired_people);
    }
    format!("{company}: applied {} step(s)", round.applied)
}

/// Run this company's actuator until it is stopped or permanently refused.
///
/// Returns only on a refusal no retry can change — a 403 for an identity that
/// is revoked or was never enrolled, a 404 for a company this daemon does not
/// serve, a 422 for a body this client should never have built. Everything
/// else is retried, and that includes a `503 identity-store-unavailable`: a
/// daemon whose trust store cannot be read has not decided anything about this
/// caller, and it is a daemon restarting rather than a reason to leave a
/// company un-actuated. It used to answer 403 for that fault, and one
/// seven-second stall cost a live company its actuator — and the sidebar brain
/// this process hosts — for two hours (#1204).
///
/// `gestured` is the session brain's own signal: the operator clicked, so the
/// placement this loop computes has changed and there is no reason to wait for
/// chiefd's changefeed to say so. Before the brain existed, a click reached
/// this loop only when the wake it posted committed `activity` — measured at
/// **2,831ms and 4,477ms** from the click to `actuator.gesture.observed`, which
/// is how long the process that spawns panes took to learn a click had
/// happened. It is the same process now.
///
/// # Errors
/// [`ActuationError`] for the terminal refusals above.
pub async fn run<W: Wire, A: Actuator>(
    wire: &W,
    actuator: &mut A,
    company: &str,
    actuator_id: &str,
    schedule: Schedule,
    gestured: &tokio::sync::Notify,
) -> Result<(), ActuationError> {
    println!("{company}: actuating as {actuator_id}");
    tracing::info!(
        event = "actuator.start",
        company,
        actuator_id,
        "the resident actuator is running"
    );
    // Resume point for the changefeed. `None` on the first connection replays
    // whatever the ring retains, which costs one extra round; after that every
    // connection carries the highest seq seen, so a reconnect can never replay
    // the same backlog and wake itself forever.
    let mut after: Option<u64> = None;
    let mut backoff = schedule.first_retry;
    // Every retry in this loop is counted and every backoff is stated. A
    // reconnect ladder that only prints to a stderr nobody keeps is a company
    // that looks stalled with no record of why.
    let mut retries: u64 = 0;
    let mut rounds: u64 = 0;
    loop {
        let started = Instant::now();
        rounds += 1;

        let desired = match wire.desired().await {
            Ok(desired) => {
                backoff = schedule.first_retry;
                desired
            }
            Err(error) if error.is_transient() => {
                retries += 1;
                eprintln!("{company}: {error}; still actuating, retrying in {backoff:?}");
                tracing::warn!(
                    event = "actuator.desired.retry",
                    company,
                    round = rounds,
                    attempt = retries,
                    backoff_ms = chiefd_log::duration_ms(backoff),
                    reason = %error,
                    "reading the desired state failed; retrying"
                );
                wire.delay(backoff).await;
                backoff = (backoff * 2).min(schedule.max_retry);
                continue;
            }
            Err(error) => {
                tracing::error!(
                    event = "actuator.desired.refused",
                    company,
                    round = rounds,
                    reason = %error,
                    "reading the desired state was refused in a way no retry can change"
                );
                return Err(error);
            }
        };

        // A hold is obeyed exactly: nothing is applied and nothing is
        // improvised. An actuator that acted on its own judgement while the
        // breaker was tripped would defeat the breaker. Note that the set still
        // arrived in full — a hold says "do not act", not "I have nothing to
        // say" — so an operator running a shadow diff can still read it.
        let applied = if desired.hold.is_some() {
            Applied::none()
        } else {
            actuator.converge(&desired).await
        };
        if let Some(failure) = &applied.failure {
            eprintln!("{company}: {failure}");
            tracing::error!(
                event = "actuator.round.failed",
                company,
                round = rounds,
                reason = %failure,
                "a converge pass failed part way through its plan"
            );
        }
        let round = round_outcome(&desired, &applied);
        println!("{}", round_line(company, &round));
        tracing::info!(
            event = "actuator.round",
            company,
            round = rounds,
            requested = round.requested,
            applied = round.applied,
            failed = round.failed,
            held = desired.hold.is_some(),
            elapsed_ms = chiefd_log::elapsed_ms(started),
            "a converge pass finished"
        );

        let elapsed = started.elapsed();
        if elapsed < schedule.min_round_interval {
            wire.delay(schedule.min_round_interval - elapsed).await;
        }

        // THE OPERATOR IS A SOURCE OF WORK, exactly as the changefeed is. A
        // gesture changes the desired TOPOLOGY (the focused person is an input
        // to `desired_topology`) without changing anything chiefd knows, so the
        // changefeed cannot carry it and only this arm can.
        let woken = tokio::select! {
            wake = wire.wait(after, schedule.idle_wait) => wake,
            () = gestured.notified() => Ok(Wake::Quiet),
        };
        match woken {
            Ok(Wake::Change { seq }) => after = Some(seq),
            // The resume point is unusable — a previous chiefd process epoch,
            // or an evicted successor. Dropping it restarts from the retained
            // ring, and the round that follows re-reads everything anyway,
            // which IS the resync the feed is asking for.
            Ok(Wake::Reorg) => after = None,
            Ok(Wake::Quiet | Wake::Closed) => {}
            Err(error) if error.is_transient() => {
                retries += 1;
                eprintln!("{company}: {error}; the changefeed will be reopened");
                tracing::warn!(
                    event = "actuator.changefeed.retry",
                    company,
                    round = rounds,
                    attempt = retries,
                    backoff_ms = chiefd_log::duration_ms(backoff),
                    reason = %error,
                    "the changefeed dropped; reopening after a backoff"
                );
                wire.delay(backoff).await;
                backoff = (backoff * 2).min(schedule.max_retry);
            }
            Err(error) => {
                tracing::error!(
                    event = "actuator.changefeed.refused",
                    company,
                    round = rounds,
                    reason = %error,
                    "the changefeed was refused in a way no retry can change"
                );
                return Err(error);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The production actuator
// ---------------------------------------------------------------------------

/// The [`Actuator`] that drives a real tmux server.
///
/// Owns the per-company [`EverObserved`] registry and the [`CrashLoop`]
/// counter, because both accumulate across the whole life of the loop — a
/// registry rebuilt per pass can never accumulate "ever", and a counter rebuilt
/// per pass can never count "consecutive". That ownership is the main reason
/// this is a struct and not a function.
///
/// It re-reads the roster and the launch catalog every round rather than
/// caching them: a hire, a transfer or a pause changes where everybody is
/// displayed, and a person materialized while this loop is running must become
/// launchable without restarting it — which would take the company down, since
/// this process owns the panes.
pub struct TmuxActuator {
    client: ActuationClient,
    executor: Box<dyn HostExecutor>,
    socket: Socket,
    session: String,
    ever_observed: EverObserved,
    crash_loop: CrashLoop,
    company: String,
    /// The company directory the front door resolved for this actuator.
    company_dir: std::path::PathBuf,
    /// The session brain: where this loop's company read goes, and where the
    /// operator's focus comes from.
    brain: crate::sidebar::brain::Handle,
    /// The last operator gesture this actuator has placed for.
    ///
    /// Remembered ONLY so the arrival of a new one can be announced once, on
    /// the pass that first places for it. The alternative — stamping every
    /// round with the current gesture — would put the same id on hundreds of
    /// rounds that had nothing to do with the click, in a file that already
    /// rotates every three and a half hours.
    last_gesture: Option<u64>,
}

impl TmuxActuator {
    /// Bind an actuator to one company's tmux session.
    ///
    /// There is deliberately no roster or launch-catalog parameter. Both are
    /// chiefd's, both are fetched from the client this already holds, once per
    /// pass. A constructor parameter would be a second source for the same
    /// value, and the one passed at start-up is the one that goes stale.
    #[must_use]
    pub fn new(
        client: ActuationClient,
        executor: Box<dyn HostExecutor>,
        socket: Socket,
        session: String,
        company_dir: std::path::PathBuf,
        brain: crate::sidebar::brain::Handle,
    ) -> Self {
        let company = client.document_key().to_owned();
        Self {
            client,
            executor,
            socket,
            session,
            ever_observed: EverObserved::new(),
            crash_loop: CrashLoop::new(),
            company,
            company_dir,
            brain,
            last_gesture: None,
        }
    }
}

/// The failure an apply reports when this pass has no launch catalog.
///
/// Pure, so the one sentence an operator reads when a company will not start is
/// a value a test can hold rather than a string buried in an async body. It
/// says WHAT is missing and WHOSE it is, because the recovery ("chiefd is not
/// serving the catalog" versus "this person is not materialized") is different
/// in each case and the operator has to be able to tell.
#[must_use]
pub fn catalog_unavailable(reason: &str) -> Applied {
    Applied::blocked(format!(
        "the launch catalog could not be read from chiefd ({reason}); nothing was applied. chiefd \
         owns the per-person launch inputs — the pi binary, pi-home, workspace, model, provider, \
         tools and pane environment — and this client never guesses one"
    ))
}

/// The people whose panes this company owns, as seen this pass.
///
/// The same ownership rule [`plan::compute_converge_plan`] applies, stated
/// once here for the crash-loop counter, which has to know who survived BEFORE
/// the plan filters anybody out. It is not a second opinion about anything: a
/// pane tagged for this company and carrying a person this company knows is
/// that person's pane, and nothing about desire enters into it.
/// The same observation, keyed by person, carrying the PANE each was seen in.
///
/// The crash-loop registry needs the pane and not only the person: a process
/// that starts and exits within a pass is present at the next observation, so
/// "there is a pane for them" cannot distinguish a boot that took from one that
/// died and was replaced. A changed pane id can.
fn observed_person_panes(
    observed: &ObservedTopology,
    organization: &str,
    known_person_ids: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    observed
        .panes
        .iter()
        .filter(|pane| pane.organization_id == organization)
        .filter(|pane| known_person_ids.contains(&pane.person_id))
        .map(|pane| (pane.person_id.clone(), pane.tmux_id.clone()))
        .collect()
}

// What this pass places, and with what.
//
// EXACTLY chiefd's desired set, less the people this actuator has stopped
// trying to start. Nothing in this process may add a name to it: chiefd
// decides WHO runs, and a person who should not be running is ABSENT from
// that set, so absence is an instruction to tear their pane down (see
// `chiefd-core`'s `runtime::actuation`). It is a function rather than three
// lines inside `converge` so that the rule is a value a test can hold.
//
// # TOMBSTONE: `retain_focused_observed_person`, the operator-view lease
//
// A person the operator had SELECTED in the rail used to be put back into
// these hashes whenever chiefd omitted them, on the strength of one owned live
// pane (`c28764437`, 2026-08-17). It was meant to cover one instant — a
// one-shot wake grant lapsing while the operator was still looking — and its
// stated bound was "chiefd is authoritative again as soon as focus leaves".
//
// THAT BOUND WAS UNREACHABLE, and the lease is what made it so. The only thing
// that moves the selection off a person is `sidebar::brain::tidy_selection`,
// whose first guard returns early while the person is LIVE — and the lease is
// what kept them live. The pane was the lease's evidence and the lease was the
// pane's cause, so each half proved the other and chiefd's withdrawal, the one
// input that should have ended it, was the one input neither half read.
//
// Measured on a live company: an idle auto-park was decided and minted
// `forced`, chiefd dropped the person from the desired set, and eight minutes
// later they still held a pane and a live Pi process while every round printed
// `converged · 1 up` — because that count asks how many DESIRED people have a
// pane, and this person was no longer one of them.
//
// Nothing replaces it. The defect it patched was a daemon-side withdrawal that
// has since been repaired where it belonged (`converge_apply::cycle`'s mail
// demand union), and the display half it was really about is answered on the
// glass: the brain announces the person is no longer up, keeps the operator
// where they are, and their row goes back to `sleeping` with a card that wakes
// them again. Focus keeps its legitimate job — it decides WHERE a desired
// person is placed (`placement::desired_topology`), never WHETHER they run.
//
// # TOMBSTONE: the crash-loop filter, deleted 2026-08-19
//
// This function used to remove everybody the actuator had given up on, so a
// held person was not placed and their window became undesired and was reaped.
// The give-up is gone (`crash_loop`), and with it the reason to subtract
// anybody here. A person waiting out a retry backoff STAYS PLACED: only their
// spawn step is skipped, the way a refused person's is, so their department
// window and the rail that carries their diagnostics are not torn down and
// re-minted every few seconds while the operator is trying to read them.
//
// **Nothing may be subtracted from chiefd's desired placement here again.**
// Placement is what chiefd wants; whether a spawn is attempted THIS pass is a
// separate question with a separate seam.

/// The people whose spawn step ACTUALLY RAN, in walk order.
///
/// `steps` is the executed PREFIX, not the whole plan, and the distinction is a
/// bug the review caught: the interpreter is fail-stop, so a plan that failed at
/// step k never attempted step k+1. Handing the whole plan to the crash-loop
/// counter blamed everybody ordered after the failure for a boot nobody tried.
/// One person with a broken workspace would then take the people behind them in
/// the walk down too — a few passes later the whole company would be sitting on
/// the ten-second retry ceiling for a fault none of them had. That is the exact
/// "one broken workspace must not slow the company down" property this module
/// claims.
///
/// The counter judges these against the NEXT pass's observation, which is the
/// only place the evidence that they did or did not survive exists.
fn spawned_people(steps: &[plan::Step]) -> Vec<String> {
    steps.iter().filter_map(spawn_person).collect()
}

/// The people whose boot this pass actually ATTEMPTED, from the reached prefix.
///
/// `spawned_people` names everybody a spawn step was walked for. The refused
/// are then removed, because nothing was spawned for them: chiefd published no
/// launch spec, so no pane was ever created and no pane could die. Counting a
/// refusal as a failed boot would back them off for crash-looping — a second
/// wrong answer stacked on a condition that already has a true one, and one
/// that makes the true one slower to clear once the operator fixes it.
fn attempted_boots(
    steps: &[plan::Step],
    refused: &BTreeMap<String, String>,
    deferred: &BTreeSet<String>,
) -> Vec<String> {
    spawned_people(steps)
        .into_iter()
        .filter(|person| !refused.contains_key(person) && !deferred.contains(person))
        .collect()
}

/// The person whose spawn step the pass died on, when it died on one.
///
/// The interpreter is fail-stop and every [`StepError`] carries the index of
/// the step it happened at, so the plan itself says who the step was for. That
/// is how tmux's own sentence about a broken workspace reaches the card of the
/// person whose workspace it is, instead of only the log.
fn failed_step_person(
    steps: &[plan::Step],
    failure: &crate::actuate::interpret::StepError,
) -> Option<String> {
    spawn_person(steps.get(failure.index())?)
}

/// WHAT THE FAILING STEP WAS ATTEMPTING: its kind and its subject.
///
/// One definition, because the same sentence goes to three places — the
/// operator's screen, the round log, and the card of every person the failure
/// cost a boot — and three copies of it would drift.
fn attempted_step(
    plan: &plan::ConvergePlan,
    failure: &crate::actuate::interpret::StepError,
) -> String {
    plan.steps.get(failure.index()).map_or_else(
        || format!("step {}", failure.index()),
        |step| format!("{} for {}", step.kind(), step.subject()),
    )
}

/// The people whose spawn step was ordered BEHIND the step that failed.
///
/// The interpreter is fail-stop: a pass that died at step k never attempted
/// step k+1. So these people did not fail to boot — nobody tried to boot them —
/// and the cause of their silence is the other person's broken step. It is the
/// only sentence anybody has about them, and without it their card is blank.
fn blocked_boots(steps: &[plan::Step], failed_at: usize) -> Vec<String> {
    steps.iter().skip(failed_at.saturating_add(1)).filter_map(spawn_person).collect()
}

/// The person a single spawn-bearing step is for.
fn spawn_person(step: &plan::Step) -> Option<String> {
    match step {
        plan::Step::CreateSession { first } | plan::Step::CreateWindowWithSpawn { first, .. } => {
            Some(first.person_id.clone())
        }
        plan::Step::SplitPane { spec, .. } | plan::Step::Respawn { spec, .. } => {
            Some(spec.person_id.clone())
        }
        _ => None,
    }
}

/// Missing and unknown inbox-count owners at the display handoff.
///
/// The roster and launch catalog are separate HTTP reads. The launch catalog
/// validates its count map against its OWN roster, but the brain draws the
/// roster read by this pass. Keep that boundary explicit: a mismatch is not an
/// empty inbox and is not launch authority.
fn inbox_count_roster_mismatch<'a>(
    roster_people: impl IntoIterator<Item = &'a str>,
    inbox_counts: &BTreeMap<String, usize>,
) -> Option<(Vec<String>, Vec<String>)> {
    let roster: BTreeSet<&str> = roster_people.into_iter().collect();
    let counted: BTreeSet<&str> = inbox_counts.keys().map(String::as_str).collect();
    let missing: Vec<String> =
        roster.difference(&counted).map(|person| (*person).to_owned()).collect();
    let unknown: Vec<String> =
        counted.difference(&roster).map(|person| (*person).to_owned()).collect();
    if missing.is_empty() && unknown.is_empty() {
        None
    } else {
        Some((missing, unknown))
    }
}

impl TmuxActuator {
    /// Hand the session brain the company this pass just read.
    ///
    /// # Why the CONVERGE LOOP is the reader
    ///
    /// Not because it is convenient — because it is the process that has to
    /// read the company to do its job. Every fact here except one was already
    /// in hand a few lines above; the exception is `lifecycle_status`, the
    /// IDLE/WORKING split, which nothing else can get and which costs one read
    /// per round. Against it, the rails stop making FOUR chiefd reads apiece on
    /// every changefeed wake — and there was one rail per window, so reads per
    /// wake used to scale with how many windows the operator had open.
    ///
    /// It is a HAND-OFF inside one process now, not a publication through tmux:
    /// this is where `sidebar_options::COMPANY` (the whole company as JSON in a
    /// session option) and the `send-keys` doorbells that told every rail to
    /// re-read it used to be.
    ///
    /// Best effort, on purpose: a `lifecycle_status` that will not answer costs
    /// the operator one round of an IDLE/WORKING split, never a converge pass.
    async fn feed_brain(
        &self,
        roster: &crate::roster::Roster,
        desired: &DesiredRuntime,
        launch: &ResolvedCatalog,
        crashing: BTreeMap<String, CrashReport>,
    ) {
        if let Some((missing, unknown)) = inbox_count_roster_mismatch(
            roster.people.iter().map(|person| person.id.as_str()),
            &launch.inbox_counts,
        ) {
            self.brain.unreadable();
            tracing::warn!(
                event = "sidebar.company.inbox-counts-inconsistent",
                company = %self.company,
                ?missing,
                ?unknown,
                "the launch catalog's inbox counts do not exactly cover this pass's roster; the display was not updated"
            );
            return;
        }
        let Ok(board) = self.client.lifecycle_status().await else {
            tracing::debug!(
                event = "sidebar.company.lifecycle-unreadable",
                company = %self.company,
                "the lifecycle board did not answer; nobody is shown IDLE this round"
            );
            return;
        };
        let accents = launch
            .specs
            .iter()
            .filter_map(|(person, spec)| {
                spec.accent.as_ref().map(|accent| (person.clone(), accent.clone()))
            })
            .collect();
        self.brain.company(crate::sidebar::brain::Facts {
            roster: roster.clone(),
            desired: desired.people.iter().map(|person| person.person_id.clone()).collect(),
            idle: board.idle_person_ids(),
            hashes: desired.hashes(),
            accents,
            models: launch.models.clone(),
            inbox_counts: launch.inbox_counts.clone(),
            // THE ACTUATOR'S OWN CRASH REPORT, HANDED TO THE GLASS. This
            // process is the only one that knows a person's boot keeps dying,
            // how many times, since when, and what tmux said about it. Until it
            // said so the rail drew `starting` at an operator who could not tell
            // a company that was coming up from one that had been failing for
            // an hour.
            crashing: crashing
                .into_iter()
                .map(|(person_id, report)| {
                    (
                        person_id,
                        crate::sidebar::CrashNotice {
                            failures: report.failures,
                            elapsed: crate::actuate::crash_loop::human_duration(report.elapsed),
                            retry_in: crate::actuate::crash_loop::human_duration(report.retry_in),
                            // A BLANK SENTENCE IS NOT A SENTENCE. An empty
                            // string would draw the card with nothing after
                            // the numbers; the notice has its own fallback
                            // for "nothing was learned" and it must be the
                            // thing that fires.
                            last_error: report
                                .last_error
                                .filter(|detail| !detail.trim().is_empty()),
                        },
                    )
                })
                .collect(),
            // CHIEFD'S GATE, HANDED TO THE GLASS ON THE SAME SEAM. The catalog
            // this pass already fetched names every person the gate declined
            // and why. Without it the rail knows only that chiefd wants them
            // and tmux has not got them, which it drew as `starting` — a
            // promise that nobody was going to keep, on every pass, for ever.
            // The reason travels verbatim: chiefd is the only process that can
            // see the disk a refusal is about.
            refusals: launch.refusals.clone(),
        });
    }
}

impl Actuator for TmuxActuator {
    async fn converge(&mut self, desired: &DesiredRuntime) -> Applied {
        // The roster is read first because it decides which panes are OURS and
        // where each person is displayed. An unreadable roster is a pass that
        // concludes nothing, NOT an empty company: placing nobody because a
        // route was slow reads to the diff as "kill everybody".
        let roster = match self.client.roster().await {
            Ok(roster) => roster,
            Err(error) => {
                // THE RAIL SAYS SO RATHER THAN WAITING FOR EVER. "I have not
                // read the company yet" and "I tried and could not" are
                // different facts, and a brain that has never read one has been
                // drawing the boot ellipsis since it booted.
                self.brain.unreadable();
                return Applied::blocked(format!(
                    "this company's roster could not be read ({error}); nothing was applied. An \
                     unreadable roster is never an empty one"
                ));
            }
        };

        // THE CATALOG IS FETCHED PER PASS. A person materialized while this
        // loop is running must become launchable without restarting it.
        //
        // After the roster, not before: when the daemon is down BOTH reads time
        // out, and paying the catalog's wider budget as well before discovering
        // it would double the cost of every round of an outage.
        let launch: ResolvedCatalog = match self.client.launch_catalog().await {
            Ok(catalog) => catalog.resolve(),
            Err(error) => {
                self.brain.unreadable();
                return catalog_unavailable(&error.to_string());
            }
        };

        let observed = match crate::actuate::observe::observe(
            self.executor.as_ref(),
            &self.socket,
            &self.session,
            &self.ever_observed,
        ) {
            Ok(observed) => observed,
            // `ObserveError`'s own words, not a paraphrase: this string reaches
            // an operator verbatim when a company will not come up. NOTHING is
            // concluded from it — in particular nobody is counted as having
            // failed to boot, because an unreadable runtime is not an empty
            // one.
            Err(error) => {
                return Applied::blocked(format!(
                    "this company's runtime could not be observed ({error}); nothing was applied \
                     and nothing was concluded from it"
                ))
            }
        };

        // Rails are operator infrastructure, not person topology. A rail can
        // exit while every desired person remains correctly placed, so an
        // empty topology plan is not evidence that the company window is
        // complete. Survey and repair rails before the diff on every readable
        // pass; this is what makes a failed rail self-heal without a reattach.
        if observed.session_exists {
            match crate::actuate::interpret::repair_session_rails(
                self.executor.as_ref(),
                &self.socket,
                &self.session,
                &self.company_dir.display().to_string(),
            ) {
                Ok(0) => {}
                Ok(repaired) => {
                    self.brain.geometry_moved();
                    tracing::warn!(
                        event = "sidebar.rails.repaired",
                        company = %self.company,
                        repaired,
                        "a company window had lost its sidebar; the actuator restored it"
                    );
                }
                Err(error) => {
                    return Applied::blocked(format!(
                        "this company's sidebar could not be repaired ({error}); person placement was not changed"
                    ))
                }
            }
        }

        // THE CRASH LOOP IS JUDGED BEFORE ANYTHING IS DRAWN OR PLANNED.
        //
        // The pane walk is what tells a boot that took from a boot that died,
        // and both the glass and the plan need its verdict THIS pass: the rail
        // has to draw `crashing` with the current retry number, and the plan has
        // to know whose backoff has not elapsed. Reading it after either would
        // publish last pass's answer.
        let now = std::time::Instant::now();
        let desired_hashes = desired.hashes();
        let known = roster.known_person_ids();
        self.crash_loop.observed(
            &desired_hashes,
            &observed_person_panes(&observed, &desired.company, &known),
            now,
        );
        let crashing = self.crash_loop.reports(now);
        // WHOSE SPAWN WAITS THIS PASS — and nobody's for ever. A person inside
        // their backoff window keeps their place in the topology and keeps
        // their department's window; only their spawn step is skipped, exactly
        // as a refused person's is. See `placement_hashes`'s tombstone for why
        // this is not done by subtracting them from placement.
        let waiting = self.crash_loop.waiting(now);
        for (person_id, report) in &crashing {
            // WARN, and on every round the condition holds. `chief` turns the
            // console sink off, so this reaches `.chief/log/chief.jsonl` only;
            // the operator's own copy is `crashing_clause` on the round line.
            tracing::warn!(
                event = "actuator.person.crash-looping",
                company = %self.company,
                person = %person_id,
                failures = report.failures,
                elapsed_ms = report.elapsed.as_millis(),
                retry_in_ms = report.retry_in.as_millis(),
                // `cause()`, NEVER `last_error` directly. The unwrap this
                // replaced printed the empty string when the actuator had
                // learned no sentence, so a live outage wrote a `crash-looping`
                // line every five seconds for seven minutes with `last_error`
                // blank on every one of them. A line that names a person and no
                // cause tells the operator nothing they can act on.
                last_error = report.cause(),
                "this person will not stay up; the actuator keeps retrying them on a backoff and \
                 never stops"
            );
        }

        // THE COMPANY, HANDED TO THE SESSION BRAIN.
        //
        // This is the one-reader change, and after Stage 3 it does not travel
        // through tmux at all: the brain is a task in this process. Everything
        // above was already read to do this loop's own job; only
        // `lifecycle_status` inside is new, and it is one read per round
        // against the FOUR each rail was making on every changefeed wake — one
        // rail per window, so fifteen reads of one company per wake on a
        // three-window session.
        self.feed_brain(&roster, desired, &launch, crashing.clone()).await;

        // WHO THIS PASS PLACES: exactly who chiefd wants, with nothing
        // subtracted. See `placement_hashes`'s tombstone for the two things
        // that used to be taken out here and why neither may be again.
        let hashes = desired_hashes.clone();

        // The operator's own view, ASKED OF THE BRAIN. Placement is derived per
        // pass precisely so a display answer cannot go stale between the click
        // and the next mutation; this is a field read of the process that OWNS
        // the selection, never a cache of it.
        //
        // It decides WHERE a desired person is drawn and nothing else. This pass
        // does not usually MOVE anything as a result: the brain acted the same
        // diff at click time, so converge reads the same inputs and emits an
        // empty plan. Reading it is what makes that agreement true — without it,
        // converge would compute the department placement and drag the focused
        // person straight back out of the window they were just put in
        // (`Step::MovePane`, within 30s).
        let selection = self.brain.focus();
        // THE CLICK REACHING THE PROCESS THAT SPAWNS PANES. A cold person click
        // is answered on the glass by this actuator — it is what mints the pane
        // the operator is waiting for — and this line is how long that took.
        // Measured at **2,831ms and 4,477ms** while the correlator had to cross
        // a tmux option and wait for the changefeed; the brain rings this loop
        // directly now, so the same subtraction is the cost of one `select!`
        // wake.
        //
        // ONCE PER GESTURE, on the pass that first places for it.
        if let Some(gesture) = selection.gesture {
            if self.last_gesture != Some(gesture) {
                self.last_gesture = Some(gesture);
                tracing::info!(
                    event = "actuator.gesture.observed",
                    company = %self.company,
                    gesture_id = gesture,
                    person = selection.person.as_deref().unwrap_or_default(),
                    "this converge pass is the first to place for the operator's latest gesture"
                );
            }
        }
        // THE SELECTION IS NOT A PLACEMENT INPUT. It was: the operator's
        // clicked person was lifted into the focus window, so converge had to
        // agree or drag them back out within its cadence. One window per person
        // means there is nothing to lift — see `placement::desired_topology`'s
        // own note — so the selection reaches tmux as a `select-window` and
        // reaches this pass only as the label on the line above.
        let topology: Topology =
            match crate::placement::desired_topology(&roster, &hashes, &self.session) {
                Ok(topology) => topology,
                Err(error) => {
                    return Applied::blocked(format!(
                        "this company's roster could not be placed ({error}); nothing was applied"
                    ))
                }
            };

        let converge = match plan::compute_converge_plan(&topology, &observed) {
            Ok(converge) => converge,
            Err(error) => {
                return Applied::blocked(format!(
                    "the converge plan failed closed ({error}); nothing was applied"
                ))
            }
        };
        for warning in &converge.warnings {
            eprintln!("{}: {warning}", self.company);
        }
        if converge.steps.is_empty() {
            self.crash_loop.spawning(Vec::new());
            // An empty plan is not a failed one: nothing moved, so the next
            // pass's pane walk is ordinary evidence again.
            self.crash_loop.pass_failed(false);
            // NOT `Applied::none()`. This pass DID observe — it planned, and the
            // plan came back empty — and an empty plan while people are missing
            // is the one condition the round line exists to name. Reporting
            // `none` here discarded the count and let the line fall back to the
            // DESIRED number, so a live company printed `converged · 11 up`
            // while tmux held six people: the diagnostic could never fire in
            // the exact case it was written for.
            // COUNT THE SAME SET THE QUESTION IS ABOUT. `owned_panes` holds
            // every fully-tagged pane in the session, keyed by person, with no
            // requirement that chiefd still wants that person -- a departed or
            // stale-but-tagged pane is in there too. Comparing its raw length
            // against the desired count is arithmetic between two different
            // sets, and on a live company it printed `converged · 13 up` while
            // only eight of those thirteen panes belonged to somebody wanted.
            // Ask only how many DESIRED people have a pane.
            let observed_desired = desired
                .people
                .iter()
                .filter(|person| converge.owned_panes.contains_key(&person.person_id))
                .count();
            return Applied {
                requested: 0,
                count: 0,
                failure: None,
                refused: BTreeMap::new(),
                observed_people: Some(observed_desired),
                crashing,
            };
        }

        // BEFORE THE FIRST STEP, NOT ONLY AFTER THE LAST. This plan is not
        // empty, so geometry is about to move, and the brain has to know that
        // BEFORE tmux tells its clients — otherwise a resize this pass caused
        // arrives while the brain still believes nothing is in flight, and gets
        // read as the operator dragging the sidebar's border.
        //
        // MEASURED, on the live company, and it latched: converge killed the
        // parked focus window's standing notice and re-laid the window in one
        // argv; tmux handed the notice's columns to the rail for the instant
        // between the two; the rail's client reported 113 columns; the brain
        // wrote 113 into `@chief_sidebar_columns` twenty milliseconds before the
        // after-the-pass stamp arrived — and every later layout in the session
        // reproduced 113, because the recorded width is what a layout falls back
        // to. The stamp twenty milliseconds later then correctly skipped the
        // 113 -> 26 resize that would have undone it.
        //
        // The old design wrote `@chief_sidebar_gesture` ONE COMMAND AHEAD of
        // every kill and every layout, which had this property by construction.
        // Stage 3 replaced that session option with a function call and moved it
        // to the end of the pass; this is the half of the old contract that move
        // dropped. The end-of-pass stamp below stays: it extends the transit
        // past the LAST step, which a long pass needs.
        self.brain.geometry_moved();
        // `apply_plan_with_launch_roster`, not the plain `apply_plan` wrapper:
        // the diagnostics are what keep a missing person a LOUD, NAMED refusal.
        // With them, a start for somebody chiefd's gate declined fails with
        // "person 'vera' refused; re-checked cause: required directory
        // 'workspace' is missing"; without them it fails with the
        // interchangeable "no launch spec for person 'vera'", which names
        // neither the cause nor even which of the two possible situations it is
        // (#52). chiefd re-derives those causes because it is the only process
        // that can see the disk they are about.
        let report = crate::actuate::interpret::apply_plan_with_launch_roster(
            self.executor.as_ref(),
            &self.socket,
            &topology,
            &observed,
            LaunchInputs {
                catalog: &launch.specs,
                diagnostics: LaunchRosterDiagnostics {
                    iterated_launch_roster: Some(&launch.roster),
                    refusal_reasons: Some(&launch.refusals),
                },
                deferred: &waiting,
            },
            &converge,
            PassContext {
                committed: crate::actuate::interpret::CommittedBindings::default(),
                // The live selection, for LOG CONTEXT on the destructive steps.
                // Read from the same `self.brain.focus()` this pass already
                // holds — in-process per-pass state, never a durable record of
                // placement (#751-P9). It is never a guard input.
                selected_person: selection.person.clone(),
            },
        );
        // Recorded AFTER the apply and over the REACHED PREFIX ONLY. A person
        // whose spawn step was never reached because an earlier step failed has
        // not failed to boot — nobody tried to boot them — and counting them
        // would be blaming them for somebody else's broken workspace.
        //
        // REACHED, not completed, and the two are no longer the same number: a
        // pass now walks PAST a person the gate refused instead of stopping at
        // them, so the people ordered behind a refusal really were attempted.
        //
        // And the refused themselves are removed from the list. Nobody tried to
        // boot them either — chiefd published no launch spec, so no pane was
        // ever spawned — and counting a refusal as a failed boot would hold
        // them for crash-looping, which is a second wrong answer stacked on a
        // condition that already has a true one.
        //
        // AND THE DEFERRED, for the same reason as the refused: a person whose
        // backoff had not elapsed had their spawn step skipped, so no pane was
        // created for them and no pane of theirs could die. Counting a
        // deferral as a failed boot would make the backoff feed itself — every
        // wait would earn another failure, and the delay would climb to the
        // ceiling and stay there for a person nothing was wrong with.
        let attempted = converge.steps.get(..report.steps_reached).unwrap_or(&converge.steps);
        self.crash_loop.spawning(attempted_boots(attempted, &report.refused, &report.deferred));
        // WHAT WENT WRONG, ONTO THE CARD. The interpreter's own words about the
        // step that failed are the only sentence anybody has about why a person
        // will not boot, and an operator reading `crashing` needs it more than
        // the log does.
        if let Some(failure) = &report.failure {
            let attempted = attempted_step(&converge, failure);
            if let Some(person) = failed_step_person(&converge.steps, failure) {
                self.crash_loop.note_error(&person, failure.to_string());
            }
            // AND THE PEOPLE THE FAILURE COST A BOOT, WHO ARE NOT THE PERSON
            // THE STEP WAS FOR.
            //
            // `note_error` had exactly ONE caller and it was the line above:
            // the failing step has to BE a spawn step for anybody to learn
            // anything. A layout step that tmux refuses is not a spawn step, so
            // in the outage this was written for — `select-layout` refusing
            // six people's window on every round — not one person was ever
            // told a cause, and every card read blank.
            //
            // The pass is fail-stop, so every spawn ordered BEHIND the failure
            // was never attempted. That is a true fact about their boot and it
            // is the only one available, so it goes on their card.
            for person in blocked_boots(&converge.steps, failure.index()) {
                self.crash_loop.note_error(
                    &person,
                    format!(
                        "their boot was not attempted this pass: {attempted} failed first ({})",
                        failure.cause()
                    ),
                );
            }
            tracing::error!(
                event = "actuator.pass.failed",
                company = %self.company,
                step = failure.index(),
                attempted = %attempted,
                cause = %failure.cause(),
                "a converge pass stopped at this step"
            );
        }
        // AND WHETHER THIS PASS FINISHED. A fail-stop leaves the window
        // half-converged — its kills and splits ran, the steps behind the
        // failure did not — so the panes the next pass sees moved because this
        // actuator moved them and then gave up. Counting that churn as deaths
        // held an entire live company at `starting` behind one stray pane; the
        // registry needs to be told, because it cannot see a step list.
        self.crash_loop.pass_failed(report.failure.is_some());
        // CONVERGE RESIZES RAILS TOO. A pass that applied anything may have
        // reaped a window or re-laid one, and the rails it reflows can no more
        // attribute that to themselves than they could when converge was a
        // different process. This is where `@chief_sidebar_gesture` used to be
        // written, one command ahead of every kill and every layout.
        if report.steps_ok > 0 {
            self.brain.geometry_moved();
        }
        Applied {
            requested: converge.steps.len(),
            count: report.steps_ok,
            // THE OPERATOR'S OWN COPY NAMES THE ATTEMPT. `failure` alone says
            // which step INDEX died and which tmux verb said no; it never said
            // which person or which window the step was for, so the sentence
            // printed on the screen named nothing anybody could go and look at.
            failure: report.failure.as_ref().map(|failure| {
                format!("{failure}; attempted {}", attempted_step(&converge, failure))
            }),
            refused: report.refused.clone(),
            // The plan's own owned set: the panes this pass proved were a
            // person, tagged, on this socket. Not the desired set, and not a
            // count of anything the planner merely hoped for.
            observed_people: Some(converge.owned_panes.len()),
            crashing,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::actuate::desired::DesiredPerson;

    fn inbox_counts(people: &[&str]) -> BTreeMap<String, usize> {
        people.iter().enumerate().map(|(count, person)| ((*person).to_owned(), count)).collect()
    }

    #[test]
    fn exact_inbox_count_keys_cover_the_display_roster() {
        let counts = inbox_counts(&["chief", "vera"]);
        assert_eq!(inbox_count_roster_mismatch(["chief", "vera"], &counts), None);
    }

    #[test]
    fn a_missing_inbox_count_is_not_an_empty_inbox() {
        let counts = inbox_counts(&["chief"]);
        assert_eq!(
            inbox_count_roster_mismatch(["chief", "vera"], &counts),
            Some((vec!["vera".to_owned()], Vec::new()))
        );
    }

    #[test]
    fn an_unknown_inbox_count_cannot_enter_the_display() {
        let counts = inbox_counts(&["chief", "stranger"]);
        assert_eq!(
            inbox_count_roster_mismatch(["chief"], &counts),
            Some((Vec::new(), vec!["stranger".to_owned()]))
        );
    }

    #[test]
    fn inbox_count_validation_is_display_only_and_placement_still_runs() {
        let source = include_str!("resident.rs");
        let handoff = source
            .find("self.feed_brain(&roster, desired, &launch, crashing.clone()).await;")
            .expect("the display handoff");
        let placement = source[handoff..]
            .find("crate::placement::desired_topology(&roster, &hashes, &self.session)")
            .expect("placement follows the display handoff");
        assert!(
            placement > 0,
            "the handoff returns unit to converge; an unreadable display does not block placement"
        );
    }

    fn observed_person(person: &str, organization: &str, hash: &str) -> ObservedTopology {
        ObservedTopology {
            session_exists: true,
            session_organization: organization.to_owned(),
            windows: vec![plan::ObservedWindow {
                tmux_id: "@9".to_owned(),
                organization_id: organization.to_owned(),
                logical_id: crate::placement::FOCUS_WINDOW_ID.to_owned(),
                protected_ui: false,
                sleeping_notice: false,
            }],
            panes: vec![plan::ObservedPane {
                tmux_id: "%19".to_owned(),
                tmux_window_id: "@9".to_owned(),
                organization_id: organization.to_owned(),
                logical_window_id: crate::placement::FOCUS_WINDOW_ID.to_owned(),
                person_id: person.to_owned(),
                launch_hash: hash.to_owned(),
                start_command: "pi".to_owned(),
            }],
        }
    }

    /// THE LIVE CASE 15 LOOP, measured on a live box 2026-08-18.
    ///
    /// `quality-jordan` was woken from the rail, answered, went quiet, and
    /// chiefd minted their idle auto-park `forced` and dropped them from the
    /// desired set. Eight minutes later they still held a pane and a live Pi
    /// process, and every round printed `converged · 1 up` — because the
    /// actuator put the operator's SELECTED person back into this pass's
    /// placement, and the selection could never move off them, since the only
    /// thing that moves it is the pane going away.
    ///
    /// The operator's selection is not authority over WHO runs.
    #[test]
    fn the_operator_selection_cannot_keep_a_person_chiefd_withdrew() {
        let chiefd_wants = desired(&[("chief", "hash-chief")], None);
        let hashes = chiefd_wants.hashes();

        assert_eq!(
            hashes.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["chief"],
            "chiefd's set is the whole of what this pass places"
        );
        assert!(
            !hashes.contains_key("quality-jordan"),
            "a person chiefd withdrew is placed by nobody, however the rail is pointed"
        );
    }

    /// And the pane goes. The same shape one layer down: the withdrawn person
    /// is missing from the placement, so the diff against the live session
    /// asks for their pane to be killed rather than emitting nothing.
    #[test]
    fn a_withdrawn_person_with_a_live_pane_is_torn_down() {
        let mut observed = observed_person("quality-jordan", "acme", "hash-jordan");
        observed.windows.push(plan::ObservedWindow {
            tmux_id: "@1".to_owned(),
            organization_id: "acme".to_owned(),
            logical_id: "executive".to_owned(),
            protected_ui: false,
            sleeping_notice: false,
        });
        observed.panes.push(plan::ObservedPane {
            tmux_id: "%1".to_owned(),
            tmux_window_id: "@1".to_owned(),
            organization_id: "acme".to_owned(),
            logical_window_id: "executive".to_owned(),
            person_id: "chief".to_owned(),
            launch_hash: "hash-chief".to_owned(),
            start_command: "pi".to_owned(),
        });

        let topology = crate::placement::Topology {
            organization: "acme".to_owned(),
            session: "org-acme_".to_owned(),
            windows: vec![crate::placement::Window {
                logical_id: "executive".to_owned(),
                name: "Executive".to_owned(),
                panes: vec![crate::placement::Pane {
                    person_id: "chief".to_owned(),
                    launch_hash: "hash-chief".to_owned(),
                    order: 0,
                }],
            }],
            known_person_ids: BTreeSet::from(["chief".to_owned(), "quality-jordan".to_owned()]),
        };

        let converge = plan::compute_converge_plan(&topology, &observed).expect("plan");
        assert!(
            converge.steps.iter().any(|step| matches!(
                step,
                plan::Step::KillPane { pane } if pane.0 == "%19"
            )),
            "the parked person's pane must be reaped: {:?}",
            converge.steps
        );
    }

    /// The actuator's own safety hold is unchanged, and it is the ONE thing
    /// that may still subtract from chiefd's set. It never adds to it.
    fn desired(people: &[(&str, &str)], hold: Option<HoldReason>) -> DesiredRuntime {
        DesiredRuntime {
            company: "acme".to_owned(),
            actuation_mode: "apply".to_owned(),
            people: people
                .iter()
                .map(|(person, hash)| DesiredPerson {
                    person_id: (*person).to_owned(),
                    launch_hash: (*hash).to_owned(),
                })
                .collect(),
            hold,
        }
    }

    /// The old actuator reaches its hold, publishes it in live tmux, and is
    /// replaced. The replacement adopts before its first plan. That plan is
    /// empty, and the actual notice process and rail geometry do not move.
    #[test]
    fn the_company_handed_to_the_brain_carries_this_passs_launch_refusals() {
        let source = include_str!("resident.rs");
        let feed = source.find("async fn feed_brain").expect("the brain hand-off");
        let body = &source[feed..];
        let end = body.find("impl Actuator for TmuxActuator").expect("end of the hand-off");
        assert!(
            body[..end].contains("refusals: launch.refusals"),
            "the launch catalog's refusals must reach the rail beside the desired set"
        );
    }

    /// Both scripts are POPPED, so a caller pushes its answers in REVERSE
    /// order — the last push is the first answer. `answers` has always worked
    /// that way and `waits` matches it rather than inventing a second
    /// convention in one type.
    ///
    /// An exhausted `answers` is a terminal 404, which is how these tests end:
    /// the loop runs for ever otherwise. An exhausted `waits` is
    /// `Ok(Wake::Quiet)`, the old fixed behaviour, so every test written
    /// before the changefeed could be scripted reads exactly as it did.
    #[derive(Default)]
    struct ScriptedWire {
        answers: RefCell<Vec<Result<DesiredRuntime, ActuationError>>>,
        waits: RefCell<Vec<Result<Wake, ActuationError>>>,
        seen: RefCell<Vec<String>>,
    }

    impl Wire for ScriptedWire {
        fn document_key(&self) -> &str {
            "acme@abc123"
        }

        async fn desired(&self) -> Result<DesiredRuntime, ActuationError> {
            self.seen.borrow_mut().push("desired".to_owned());
            self.answers.borrow_mut().pop().unwrap_or(Err(ActuationError::Refused {
                path: "/v1/org/runtime/desired".to_owned(),
                status: 404,
                code: "unknown-company".to_owned(),
                detail: "script exhausted".to_owned(),
            }))
        }

        async fn wait(
            &self,
            _after: Option<u64>,
            _budget: Duration,
        ) -> Result<Wake, ActuationError> {
            self.seen.borrow_mut().push("wait".to_owned());
            self.waits.borrow_mut().pop().unwrap_or(Ok(Wake::Quiet))
        }

        async fn delay(&self, _duration: Duration) {}
    }

    /// The exact answer the measured box would now receive from the changefeed
    /// during an identity-store stall.
    fn identity_store_unavailable(path: &str) -> ActuationError {
        ActuationError::Refused {
            path: path.to_owned(),
            status: 503,
            code: "identity-store-unavailable".to_owned(),
            detail: "the identity store could not be read: store failure: auth-identities: \
                     database is locked"
                .to_owned(),
        }
    }

    #[derive(Default)]
    struct ScriptedActuator {
        converged: RefCell<Vec<DesiredRuntime>>,
        result: Applied,
    }

    impl Actuator for ScriptedActuator {
        async fn converge(&mut self, desired: &DesiredRuntime) -> Applied {
            self.converged.borrow_mut().push(desired.clone());
            self.result.clone()
        }
    }

    /// THE CENTRAL PROPERTY OF THIS CHANGE, asserted where it can actually be
    /// enforced: [`ScriptedWire`] implements the WHOLE of [`Wire`] while
    /// carrying no way to send anything, and it compiles. If a reporting verb
    /// is ever added back to the trait, this type stops implementing it and
    /// this module stops building — which is a louder failure than any runtime
    /// assertion about a method that would by then already exist.
    #[test]
    fn the_wire_is_implementable_by_a_client_that_can_only_read() {
        let wire = ScriptedWire::default();
        assert_eq!(Wire::document_key(&wire), "acme@abc123");
        assert!(wire.seen.borrow().is_empty(), "constructing a wire sends nothing");
    }

    #[tokio::test]
    async fn a_held_company_is_read_in_full_and_acted_on_not_at_all() {
        let wire = ScriptedWire::default();
        wire.answers.borrow_mut().push(Ok(desired(&[("vera", "aaa")], Some(HoldReason::Shadow))));
        let mut actuator = ScriptedActuator::default();
        let error = run(
            &wire,
            &mut actuator,
            "acme",
            "cli@box",
            Schedule::eager(),
            &tokio::sync::Notify::new(),
        )
        .await;
        assert!(error.is_err(), "the script exhausts and the loop returns the terminal refusal");
        assert!(
            actuator.converged.borrow().is_empty(),
            "a hold must not be improvised around; an actuator that helped while the breaker was \
             tripped would defeat the breaker"
        );
    }

    #[tokio::test]
    async fn a_transient_failure_retries_rather_than_leaving_a_company_un_actuated() {
        let wire = ScriptedWire::default();
        wire.answers.borrow_mut().push(Ok(desired(&[], None)));
        wire.answers.borrow_mut().push(Err(ActuationError::Transport {
            url: "http://127.0.0.1:9/".to_owned(),
            reason: "connection refused".to_owned(),
        }));
        let mut actuator = ScriptedActuator::default();
        let _ = run(
            &wire,
            &mut actuator,
            "acme",
            "cli@box",
            Schedule::eager(),
            &tokio::sync::Notify::new(),
        )
        .await;
        assert!(
            wire.seen.borrow().iter().filter(|call| *call == "desired").count() >= 2,
            "a restarting daemon is not a reason to stop actuating"
        );
    }

    /// #1204 — THE REGRESSION TEST FOR THE MEASURED DEATH.
    ///
    /// A company's identity store stalled for seven seconds. Every route
    /// answered `403 unknown identity` to every caller, and this loop —
    /// correctly, because a 4xx is a product rule and retrying one loops for
    /// ever — returned. `chief actuate` exited, the sidebar brain it hosts
    /// died with the process, and every rail froze on its last frame at 26
    /// columns inside a 34-column pane while chiefd logged
    /// `actuator_silent_ms=6576837` for two hours.
    ///
    /// The daemon answers `503 identity-store-unavailable` for that fault now.
    /// This asserts the consequence: the loop rides it out and keeps reading.
    #[tokio::test]
    async fn a_transient_identity_store_fault_on_the_changefeed_does_not_kill_the_actuator() {
        let wire = ScriptedWire::default();
        // Popped, so the LAST push is the FIRST answer.
        wire.answers.borrow_mut().push(Ok(desired(&[("vera", "aaa")], None)));
        wire.answers.borrow_mut().push(Ok(desired(&[("vera", "aaa")], None)));
        wire.waits.borrow_mut().push(Ok(Wake::Quiet));
        wire.waits.borrow_mut().push(Err(identity_store_unavailable("/v1/docs/watch")));

        let mut actuator = ScriptedActuator::default();
        let error = run(
            &wire,
            &mut actuator,
            "acme",
            "cli@box",
            Schedule::eager(),
            &tokio::sync::Notify::new(),
        )
        .await
        .expect_err("the script exhausts eventually and the loop returns THAT");

        // It returned on the exhaustion 404, not on the 503.
        let ActuationError::Refused { status, .. } = &error else {
            panic!("expected the exhaustion refusal, got {error:?}")
        };
        assert_eq!(*status, 404, "a 503 from the identity store must not end the loop: {error:?}");
        let reads = wire.seen.borrow().iter().filter(|call| *call == "desired").count();
        assert!(
            reads >= 2,
            "the loop must keep reading after the store fault; it read {reads} time(s)"
        );
        assert_eq!(
            actuator.converged.borrow().len(),
            2,
            "and it must keep CONVERGING — a company whose actuator survives but stops acting is \
             the same frozen rail"
        );
    }

    /// The same rule on the other call the loop makes. The measured blackout
    /// hit every route for every caller, so the read is as exposed as the
    /// changefeed park.
    #[tokio::test]
    async fn a_transient_identity_store_fault_on_the_desired_read_does_not_kill_the_actuator() {
        let wire = ScriptedWire::default();
        wire.answers.borrow_mut().push(Ok(desired(&[("vera", "aaa")], None)));
        wire.answers.borrow_mut().push(Err(identity_store_unavailable("/v1/org/runtime/desired")));

        let mut actuator = ScriptedActuator::default();
        let error = run(
            &wire,
            &mut actuator,
            "acme",
            "cli@box",
            Schedule::eager(),
            &tokio::sync::Notify::new(),
        )
        .await
        .expect_err("the script exhausts eventually");

        let ActuationError::Refused { status, .. } = &error else {
            panic!("expected the exhaustion refusal, got {error:?}")
        };
        assert_eq!(*status, 404, "a 503 on the read must not end the loop either: {error:?}");
        assert!(
            wire.seen.borrow().iter().filter(|call| *call == "desired").count() >= 2,
            "the read is retried, exactly as a transport failure is"
        );
    }

    /// THE OTHER HALF OF THE RULE, and the reason the two tests above cannot
    /// be satisfied by "retry everything".
    ///
    /// A real 403 — a revoked identity, a rotated key, a company this daemon
    /// does not serve — is a verdict, and it does not change by being asked
    /// again. A client that retried one would put the identical question at
    /// whatever rate the socket allows, which is the loop `route_error.rs`
    /// forbids by name. Nothing about the client's 403 handling was widened
    /// for #1204; the SERVER stopped sending a 403 for a fault.
    #[tokio::test]
    async fn a_real_403_on_the_changefeed_is_still_terminal() {
        let wire = ScriptedWire::default();
        wire.answers.borrow_mut().push(Ok(desired(&[("vera", "aaa")], None)));
        wire.waits.borrow_mut().push(Err(ActuationError::Refused {
            path: "/v1/docs/watch".to_owned(),
            status: 403,
            code: "unknown".to_owned(),
            detail: "revoked identity".to_owned(),
        }));

        let mut actuator = ScriptedActuator::default();
        let error = run(
            &wire,
            &mut actuator,
            "acme",
            "cli@box",
            Schedule::eager(),
            &tokio::sync::Notify::new(),
        )
        .await
        .expect_err("a verdict ends the loop");

        let ActuationError::Refused { status, detail, .. } = &error else {
            panic!("expected the 403, got {error:?}")
        };
        assert_eq!(*status, 403);
        assert_eq!(detail, "revoked identity", "it returns THAT error, on the first round");
        assert_eq!(
            wire.seen.borrow().iter().filter(|call| *call == "desired").count(),
            1,
            "it must not go round again: one read, one park, and out"
        );
    }

    /// An empty desired set is a REAL instruction — stop everybody — and it
    /// reaches the actuator rather than being mistaken for nothing to do.
    #[tokio::test]
    async fn a_company_desiring_nobody_still_reaches_the_actuator() {
        let wire = ScriptedWire::default();
        wire.answers.borrow_mut().push(Ok(desired(&[], None)));
        let mut actuator = ScriptedActuator::default();
        let _ = run(
            &wire,
            &mut actuator,
            "acme",
            "cli@box",
            Schedule::eager(),
            &tokio::sync::Notify::new(),
        )
        .await;
        assert_eq!(actuator.converged.borrow().len(), 1);
        assert!(actuator.converged.borrow()[0].people.is_empty());
    }

    #[test]
    fn a_converged_round_and_a_held_round_read_differently() {
        let converged = round_outcome(&desired(&[("chief", "hash-chief")], None), &Applied::none());
        assert_eq!(round_line("acme", &converged), "acme: converged \u{b7} 1 up");

        // THE LIVE WEDGE, 2026-08-18. A company printed `converged \u{b7} 17 up`
        // once a second for an hour while tmux held seven people and ten had
        // never been started: the count was read off the DESIRED set, so the
        // line restated the question as though it were the answer. A pass that
        // planned nothing while people are missing is the opposite of
        // converged, and it now says so and names the gap.
        let stuck = round_outcome(
            &desired(&[("chief", "h1"), ("vera", "h2"), ("nia", "h3")], None),
            &Applied {
                requested: 0,
                count: 0,
                failure: None,
                refused: BTreeMap::new(),
                observed_people: Some(1),
                crashing: BTreeMap::new(),
            },
        );
        let line = round_line("acme", &stuck);
        assert!(line.contains("NOT converged"), "{line}");
        assert!(line.contains("wants 3 people and tmux holds 1"), "{line}");
        assert!(line.contains("2 missing"), "{line}");

        // Agreement still reads as converged, and the number it prints is the
        // OBSERVED one, so the word and the count come from the same fact.
        let agreed = round_outcome(
            &desired(&[("chief", "h1"), ("vera", "h2")], None),
            &Applied {
                requested: 0,
                count: 0,
                failure: None,
                refused: BTreeMap::new(),
                observed_people: Some(2),
                crashing: BTreeMap::new(),
            },
        );
        assert_eq!(round_line("acme", &agreed), "acme: converged \u{b7} 2 up");

        // A pass that never looked (held, or no catalog) reports no gap it did
        // not measure. Absent is not zero.
        let unlooked = round_outcome(&desired(&[("chief", "h1")], None), &Applied::none());
        assert_eq!(round_line("acme", &unlooked), "acme: converged \u{b7} 1 up");

        let held = round_outcome(&desired(&[], Some(HoldReason::BreakerTripped)), &Applied::none());
        assert!(round_line("acme", &held).contains("circuit breaker"));
    }

    /// CONVERGED TO NOTHING IS NOT CONVERGED, and this line is what a live
    /// company cost.
    ///
    /// A freshly created company whose CEO nobody had asked for printed
    /// `acme: converged` once a second while `requested=0 applied=0` and no
    /// tmux session ever appeared. The Founder agent read that word, told the
    /// operator "the company is live, its CEO was booted", and the operator
    /// believed it — reasonably, because "converged" is the vocabulary of a
    /// healthy steady state and nothing beside it said otherwise.
    ///
    /// Zero STEPS is produced by both a company that is fully up and a company
    /// chiefd is asking nobody to run. Only one of them is a company, so only
    /// one of them may read as one.
    #[test]
    fn a_company_with_nobody_desired_never_reads_as_converged_alone() {
        let empty = round_outcome(&desired(&[], None), &Applied::none());
        let line = round_line("acme", &empty);
        assert!(line.contains("NOBODY"), "the empty desired set must be shouted: {line}");
        assert!(
            line.contains("no runtime"),
            "and named as the absence of a runtime, not as a steady state: {line}"
        );
        // It must also clear tmux of a fault it does not have. The operator's
        // first move on seeing this was to inspect tmux, which was innocent.
        assert!(line.contains("tmux"), "{line}");
        // The distinguishing property, stated as a comparison rather than a
        // spelling: a company with people up and a company with nobody must
        // never print the same line.
        let up = round_outcome(&desired(&[("chief", "hash-chief")], None), &Applied::none());
        assert_ne!(round_line("acme", &up), line);
    }

    /// A REFUSED PERSON IS NAMED ON THE ROUND LINE, and the pass is not a
    /// failure.
    ///
    /// The operator used to be told `the pass FAILED after X of Y step(s);
    /// nothing beyond that was attempted` — which was true, and was the defect:
    /// everybody behind that step was abandoned. Now the pass runs, and the one
    /// person who could not start is named with chiefd's own reason on the same
    /// line that reports the round.
    #[test]
    fn a_refused_person_is_named_on_the_round_line_and_the_pass_did_not_fail() {
        let applied = Applied {
            requested: 12,
            count: 11,
            failure: None,
            refused: BTreeMap::from([(
                "vera".to_owned(),
                "required directory 'workspace' is missing".to_owned(),
            )]),
            observed_people: Some(11),
            crashing: BTreeMap::new(),
        };
        let line = round_line("acme", &round_outcome(&desired(&[], None), &applied));
        assert!(!line.contains("FAILED"), "a refusal is not a failed pass: {line}");
        assert!(line.contains("REFUSED 1 of them"), "{line}");
        assert!(line.contains("vera"), "named, never counted: {line}");
        assert!(
            line.contains("required directory 'workspace' is missing"),
            "with chiefd's own reason, which is the only part an operator can act on: {line}"
        );
        assert!(
            line.contains("the rest of this plan still ran"),
            "and the operator is told the pass was not abandoned: {line}"
        );
    }

    /// A pass that genuinely failed still says so, refusals or not.
    ///
    /// NOTE: the `FAILED` half of this passes on the reverted tree too. It pins
    /// that naming refusals did not swallow the fail-stop line an operator
    /// relies on.
    #[test]
    fn a_failure_beside_a_refusal_still_reports_the_failure() {
        let applied = Applied {
            requested: 12,
            count: 3,
            failure: Some("tmux said no".to_owned()),
            refused: BTreeMap::from([("vera".to_owned(), "no workspace".to_owned())]),
            observed_people: Some(3),
            crashing: BTreeMap::new(),
        };
        let line = round_line("acme", &round_outcome(&desired(&[], None), &applied));
        assert!(line.contains("the pass FAILED after "), "{line}");
        assert!(line.contains("vera"), "and the refusal is still named beside it: {line}");
    }

    /// A REFUSED PERSON IS NOT A FAILED BOOT.
    ///
    /// Nothing was spawned for them, so no pane could die. Counting the refusal
    /// as an attempt would hold them for crash-looping — a hold is released only
    /// by evidence a held person never gets the chance to produce, so it would
    /// outlive the refusal that caused it.
    #[test]
    fn a_refused_person_is_not_counted_as_a_boot_this_pass_attempted() {
        let steps = vec![
            plan::Step::CreateSession {
                first: plan::SpawnSpec {
                    person_id: "vera".to_owned(),
                    launch_hash: "hash-1".to_owned(),
                },
            },
            plan::Step::SplitPane {
                w: plan::WindowRef::Observed("@1".to_owned()),
                spec: plan::SpawnSpec {
                    person_id: "theo".to_owned(),
                    launch_hash: "hash-2".to_owned(),
                },
            },
        ];
        let refused = BTreeMap::from([("vera".to_owned(), "no workspace".to_owned())]);
        assert_eq!(
            attempted_boots(&steps, &refused, &BTreeSet::new()),
            vec!["theo".to_owned()],
            "only the person a pane was actually spawned for"
        );
        assert_eq!(
            attempted_boots(&steps, &BTreeMap::new(), &BTreeSet::new()),
            vec!["vera".to_owned(), "theo".to_owned()],
            "and with nobody refused the count is unchanged"
        );
        // A DEFERRED PERSON IS NOT A FAILED BOOT. Their spawn step was skipped,
        // so no pane was created for them and no pane of theirs could die.
        // Counting it would make the backoff feed itself: every wait would earn
        // another failure and the delay would climb to the ceiling for a person
        // nothing was wrong with.
        assert_eq!(
            attempted_boots(&steps, &BTreeMap::new(), &BTreeSet::from(["vera".to_owned()])),
            vec!["theo".to_owned()],
            "a person waiting out a retry backoff was not attempted either"
        );
    }

    /// A pass that FAILED never claims the plan.
    #[test]
    fn a_failed_pass_says_so_rather_than_reporting_a_count() {
        let applied = Applied {
            requested: 9,
            count: 2,
            failure: Some("tmux said no".to_owned()),
            refused: BTreeMap::new(),
            observed_people: Some(2),
            crashing: BTreeMap::new(),
        };
        let round = round_outcome(&desired(&[], None), &applied);
        let line = round_line("acme", &round);
        assert!(line.contains("FAILED"), "{line}");
        assert!(
            !line.contains("applied 2 step(s)"),
            "a failure must not read as a success: {line}"
        );
    }

    /// "I could not look" and "there was nothing to do" must never render as
    /// the same round. This is the same conflation the whole change removes,
    /// checked at the layer it could still be reintroduced at.
    /// The observed count is about the DESIRED people, not about every pane
    /// that happens to be tagged.
    ///
    /// `owned_panes` admits any pane carrying an organization, a window and a
    /// person -- a departed person's leftover pane is in there too. Comparing
    /// its raw length against the desired count compares two different sets,
    /// and a live company printed `converged · 13 up` on thirteen panes of
    /// which only eight belonged to anybody chiefd wanted.
    #[test]
    fn the_observed_count_only_counts_desired_people_who_have_a_pane() {
        let wanted = desired(&[("chief", "h1"), ("priya", "h2"), ("nadia", "h3")], None);
        let owned: std::collections::BTreeMap<String, String> = [
            ("chief", "%1"),
            // wanted, and present
            ("priya", "%2"),
            // NOT wanted: a leftover pane from somebody who has left
            ("departed-ghost", "%3"),
            ("another-ghost", "%4"),
        ]
        .into_iter()
        .map(|(person, pane)| (person.to_owned(), pane.to_owned()))
        .collect();

        let observed =
            wanted.people.iter().filter(|person| owned.contains_key(&person.person_id)).count();
        assert_eq!(observed, 2, "chief and priya; the two ghosts are not people we asked for");
        assert_ne!(observed, owned.len(), "the raw pane count is the bug this pins");

        let round = round_outcome(
            &wanted,
            &Applied {
                requested: 0,
                count: 0,
                failure: None,
                refused: BTreeMap::new(),
                observed_people: Some(observed),
                crashing: BTreeMap::new(),
            },
        );
        let line = round_line("acme", &round);
        assert!(line.contains("NOT converged"), "{line}");
        assert!(line.contains("wants 3 people and tmux holds 2"), "{line}");
        assert!(line.contains("1 missing"), "nadia is the one missing: {line}");
    }

    /// An empty plan REPORTS its observation. This is the shape the round line
    /// was written for, and the first cut of it could not reach that line: the
    /// empty-plan return said `Applied::none()`, which means "never looked", so
    /// the count fell back to the desired set and a live company printed
    /// `converged · 11 up` while tmux held six people.
    #[test]
    fn an_empty_plan_still_reports_what_it_observed() {
        assert_eq!(
            Applied::none().observed_people,
            None,
            "`none` means this pass never observed, and must stay that way"
        );
        // Five wanted, three held: the live shape, where the plan is empty and
        // two people will never be started by it.
        let planned_nothing = Applied {
            requested: 0,
            count: 0,
            failure: None,
            refused: BTreeMap::new(),
            observed_people: Some(3),
            crashing: BTreeMap::new(),
        };
        assert_ne!(
            planned_nothing,
            Applied::none(),
            "a pass that planned an empty plan is not a pass that never looked"
        );
        let round = round_outcome(
            &desired(
                &[
                    ("chief", "h1"),
                    ("elena", "h2"),
                    ("marcus", "h3"),
                    ("priya", "h4"),
                    ("nadia", "h5"),
                ],
                None,
            ),
            &planned_nothing,
        );
        let line = round_line("taperoom", &round);
        assert!(line.contains("NOT converged"), "{line}");
        assert!(line.contains("wants 5 people and tmux holds 3"), "{line}");
        assert!(line.contains("2 missing"), "{line}");
    }

    #[test]
    fn a_blocked_pass_is_not_a_converged_one() {
        let blocked = Applied::blocked("the roster could not be read".to_owned());
        assert_ne!(blocked, Applied::none());
        let round = round_outcome(&desired(&[], None), &blocked);
        assert!(round.failed);
        assert_ne!(round_line("acme", &round), "acme: converged");
    }

    #[test]
    fn the_actuator_id_names_a_machine_and_a_process_an_operator_can_walk_to() {
        let id = actuator_id("box-17");
        assert!(id.contains("box-17"));
        assert!(id.contains(&std::process::id().to_string()));
    }

    #[test]
    fn the_idle_wait_is_a_ceiling_on_one_connection_and_not_a_sampling_interval() {
        let schedule = Schedule::default();
        assert!(schedule.idle_wait > schedule.min_round_interval);
    }

    // -----------------------------------------------------------------------
    // THE INSTRUMENT (the 2026-08-26 start outage)
    // -----------------------------------------------------------------------

    fn spawn_step(person: &str) -> plan::Step {
        plan::Step::SplitPane {
            w: plan::WindowRef::Observed("@1".to_owned()),
            spec: plan::SpawnSpec {
                person_id: person.to_owned(),
                launch_hash: "hash-1".to_owned(),
            },
        }
    }

    /// THE GAP THIS CLOSES. `note_error` had one caller, and it fired only when
    /// the step that failed was itself a spawn step. A `select-layout` that
    /// tmux refuses is not a spawn step, so on a live company six people
    /// crash-looped for seven minutes and not one card ever carried a cause.
    ///
    /// The pass is fail-stop, so every spawn ordered behind the failure was
    /// never attempted. Those people are named here, and their cause is the
    /// other person's broken step.
    #[test]
    fn a_failure_at_a_non_spawn_step_still_names_the_boots_it_cost() {
        let steps = vec![
            spawn_step("vera"),
            plan::Step::ApplyLayout {
                w: plan::WindowRef::Observed("@1".to_owned()),
                panes: vec![plan::PaneRef::Created("vera".to_owned())],
                retire_sleeping_notice: false,
            },
            spawn_step("theo"),
            spawn_step("ada"),
        ];
        let failure = crate::actuate::interpret::StepError::Tmux {
            index: 1,
            verb: "select-layout".to_owned(),
            detail: "Tmux layout requires at least one pane".to_owned(),
        };
        // The failing step is for nobody, which is exactly why the old wiring
        // learned nothing.
        assert_eq!(failed_step_person(&steps, &failure), None);
        assert_eq!(
            blocked_boots(&steps, failure.index()),
            vec!["theo".to_owned(), "ada".to_owned()],
            "everybody ordered behind the failure had their boot cost by it"
        );
        // And the person ordered BEFORE it is not blamed: their step ran.
        assert!(!blocked_boots(&steps, failure.index()).contains(&"vera".to_owned()));
    }

    /// A failing round names WHAT WAS ATTEMPTED, not only which index died.
    #[test]
    fn the_failing_round_names_the_step_it_attempted() {
        let converge = plan::ConvergePlan {
            steps: vec![
                spawn_step("vera"),
                plan::Step::ApplyLayout {
                    w: plan::WindowRef::Observed("@1".to_owned()),
                    panes: vec![plan::PaneRef::Created("vera".to_owned())],
                    retire_sleeping_notice: false,
                },
            ],
            ..plan::ConvergePlan::default()
        };
        let failure = crate::actuate::interpret::StepError::Tmux {
            index: 1,
            verb: "select-layout".to_owned(),
            detail: "Tmux layout requires at least one pane".to_owned(),
        };
        let attempted = attempted_step(&converge, &failure);
        assert!(attempted.contains("ApplyLayout"), "the kind: {attempted}");
        assert!(attempted.contains("@1"), "and the window it was for: {attempted}");
        assert!(
            failure.cause().contains("Tmux layout requires at least one pane"),
            "with tmux's own words: {}",
            failure.cause()
        );
    }

    /// A spawn step that fails still names its person, unchanged.
    #[test]
    fn a_failure_at_a_spawn_step_still_names_that_person() {
        let steps = vec![spawn_step("vera"), spawn_step("theo")];
        let failure = crate::actuate::interpret::StepError::Tmux {
            index: 0,
            verb: "split-window".to_owned(),
            detail: "no space for new pane".to_owned(),
        };
        assert_eq!(failed_step_person(&steps, &failure), Some("vera".to_owned()));
        assert_eq!(blocked_boots(&steps, failure.index()), vec!["theo".to_owned()]);
    }
}
