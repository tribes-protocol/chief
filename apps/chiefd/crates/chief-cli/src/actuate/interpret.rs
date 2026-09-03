//! The converge-plan interpreter — M1 `Step`s become real tmux actuation.
//!
//! This is the one module that depends on M1's `Step` enum shape; the safety
//! scaffold and observe/spawn helpers are deliberately separated so an M1
//! interface fixup stays localized here (staff's freeze caveat).
//!
//! # The model (design Q1)
//!
//! * Steps run **strictly in plan order** through the [`HostExecutor`] trait. No
//!   reordering, no parallelism; M1 already emits a dependency-safe order.
//! * A [`BindingMap`] threads through execution: a [`WindowSym`](plan::WindowSym)
//!   created earlier in the plan binds to its real tmux window id, and a pane
//!   created for a person binds to its minted tmux pane id. Later steps that
//!   reference a created window/pane resolve through these bindings.
//! * **Fail-stop, compensating cleanup, no continue.** If step *k* fails, steps
//!   *k+1..n* are abandoned and the failure is recorded on the cycle. Existing
//!   resources are never rolled back (a killed pane cannot be un-killed), but
//!   every pane minted and successfully ownership-tagged by THIS interpreter
//!   invocation is re-verified then reaped. Nothing past the failure runs (a
//!   later step may reference a binding the failed step never minted). Idempotency
//!   comes from re-planning next pass, never from replay.
//! * **Per-step precondition re-verify (the single most important part).** Every
//!   destructive step re-reads the live pane ownership tag at apply time and
//!   proceeds only if it still matches what the plan expected — closing the
//!   TOCTOU gap between observe-time and apply-time. A miss is a step failure:
//!   the cycle aborts and the next pass re-observes.
//!
//! The concrete tmux verbs are conservative, from-scratch primitives issued as
//! raw argv through [`HostExecutor::tmux`] (the trait's typed `spawn_pane` is
//! new-window-only and cannot express `split-window`/`new-session`/
//! `respawn-pane`). Non-zero tmux exit is treated as a step failure; the trait's
//! transient-retry ladder still applies underneath.
//!
//! # P2 status (#739) — PARTIAL, and the reason stated plainly
//!
//! the design record's P2 section names the target state
//! as `interpret.rs` deriving every step from "a transaction-consistent
//! snapshot of committed rows" rather than the in-memory [`BindingMap`]
//! below, so a crash mid-pass leaves no state only the dead process
//! understood.
//!
//! **`BindingMap` is NOT removed here, and this is a deliberate, named
//! partial implementation, not an oversight.** The functions in this module
//! (`apply_plan`, `apply_plan_with_launch_roster`) are synchronous and this
//! crate (`chiefd-host`) has no `CompanyDb`/SQL transaction handle at all —
//! the async actor that owns durable writes
//! (`chiefd-core::actor::CompanyDb`) lives in a different crate and a
//! different execution model. Making a single step's binding durable the
//! instant it is minted — the literal reading of "transaction-consistent
//! snapshot" — means either (a) giving this synchronous tmux-actuation loop
//! an async SQL handle (a real layering/threading-model decision: block on
//! the executor from inside a sync loop? make the whole interpreter async
//! and ripple that through every caller?), or (b) restructuring so the
//! caller commits between every individual step rather than once per pass.
//! Neither is a mechanical translation of the design doc's one-paragraph
//! prose; both are real architectural choices this module should not invent
//! unilaterally, mid-pass-durability being the exact place a wrong choice
//! fails silently as a pane that does not converge.
//!
//! **What IS implemented:** [`ApplyReport`] now exposes every window/pane
//! binding this pass minted (`windows_bound`/`panes_bound`), win or fail,
//! so the caller — `cycle.rs`, which DOES hold the async `CompanyDb` — can
//! persist them durably immediately after this
//! call returns, closing the *inter-pass* durability gap (a binding survives
//! from one completed pass to the next) without resolving the sync/async
//! question above. This does NOT close the *intra-pass* gap the design doc
//! describes (a crash between two steps of the SAME pass still loses that
//! pass's bindings) — today's behavior there is unchanged: the next pass
//! re-observes tmux from scratch and re-plans, which is the existing
//! fail-stop/re-observe contract this module's own top section already
//! documents, not a new hazard introduced by leaving this partial.
//!
//! UNVERIFIED BY CONSTRUCTION: not compiled, not tested, not typechecked,
//! not clippy-checked, per the standing no-build phase. The caller-side
//! commit this enables (`cycle.rs`) is a separate, NOT-YET-WRITTEN piece —
//! `windows_bound`/`panes_bound` are wired to nothing yet.

use std::collections::{BTreeMap, BTreeSet};

use crate::actuate::plan::{self, ConvergePlan, Step};

use crate::actuate::host::{HostErr, HostExecutor, PaneId as HostPaneId, Pid, Socket, TmuxCmd};
use crate::actuate::trust::tags;

use crate::actuate::probe::MutationContext;
use crate::actuate::spawn_cmd::{launch_command, LaunchSpec, PaneCommand};

pub(super) fn invalidate_viewport_manifest(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
) -> Result<String, String> {
    let output = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "set-option".into(),
                    "-goq".into(),
                    super::trust::viewport_options::TOPOLOGY_GENERATION.into(),
                    "0".into(),
                    ";".into(),
                    "set-option".into(),
                    "-gF".into(),
                    super::trust::viewport_options::TOPOLOGY_GENERATION.into(),
                    format!(
                        "#{{e|+:#{{{}}},1}}",
                        super::trust::viewport_options::TOPOLOGY_GENERATION
                    ),
                    ";".into(),
                    "set-option".into(),
                    "-F".into(),
                    "-t".into(),
                    session.into(),
                    super::trust::viewport_options::TOPOLOGY_EPOCH.into(),
                    format!("#{{{}}}", super::trust::viewport_options::TOPOLOGY_GENERATION),
                    ";".into(),
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    session.into(),
                    format!("#{{{}}}", super::trust::viewport_options::TOPOLOGY_EPOCH),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if output.status != 0 {
        return Err(output.stderr.trim().to_owned());
    }
    let generation = output.stdout.trim();
    #[cfg(test)]
    let generation = if generation.is_empty() { "1" } else { generation };
    generation
        .parse::<u64>()
        .map(|_| generation.to_owned())
        .map_err(|_| "tmux did not return a numeric viewport topology epoch".to_owned())
}

pub(super) fn request_viewport_manifest_refresh(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    generation: &str,
) {
    if generation.parse::<u64>().is_err() {
        return;
    }
    let _ = executor.tmux(
        socket,
        TmuxCmd {
            argv: vec![
                "if-shell".into(),
                "-F".into(),
                "-t".into(),
                session.to_owned(),
                format!("#{{!=:#{{{}}},}}", super::trust::viewport_options::REFRESH_COMMAND),
                format!(
                    "run-shell -b -t {session} '#{{{}}} {generation} >/dev/null 2>&1 || :'",
                    super::trust::viewport_options::REFRESH_COMMAND,
                ),
                String::new(),
            ],
        },
    );
}

/// How a single step did not complete.
///
/// EVERY VARIANT BUT ONE ABORTS THE CYCLE (fail-stop), and that is by design: a
/// pass that hits a genuine internal error stops rather than carrying on
/// against a plan it has proved wrong about the world.
///
/// [`StepError::LaunchRefused`] and [`StepError::RetryDeferred`] are the
/// exceptions, and neither is a failure of the plan at all. chiefd's launch gate declining a person is an expected,
/// well-understood condition that the daemon re-derives every pass and names in
/// full. It must cost that person their own step and nothing else — see
/// [`apply_plan_with_launch_roster`], which is the one place the difference is
/// acted on.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// This person's boot keeps failing and their retry backoff has not elapsed
    /// yet. NOT a fail-stop, and NOT a give-up: the step is skipped by name for
    /// THIS pass, the rest of the plan is attempted, and the person is spawned
    /// again the moment their delay is up. See `crash_loop`.
    #[error("step {index} ({step}) deferred: '{person}' is crash-looping; retrying shortly")]
    RetryDeferred {
        /// Index of the step in the plan.
        index: usize,
        /// The step variant name.
        step: &'static str,
        /// Whose spawn is waiting.
        person: String,
    },
    /// chiefd's launch gate declined this person, so there is no launch spec to
    /// spawn them with. NOT a fail-stop: the step is skipped by name and the
    /// rest of the plan is attempted.
    #[error("step {index} ({step}) skipped: '{person}' cannot be launched: {reason}")]
    LaunchRefused {
        /// Index of the step in the plan.
        index: usize,
        /// The step variant name.
        step: &'static str,
        /// Who the gate declined.
        person: String,
        /// chiefd's own reason, re-derived by the process that owns the disk.
        reason: String,
    },
    /// A per-step precondition re-verify missed at apply time: the live pane no
    /// longer carries the ownership tag / launch hash the plan expected. This is
    /// the TOCTOU guard firing; the next pass re-observes and re-plans.
    #[error("step {index} ({step}) precondition missed: {detail}")]
    Precondition {
        /// Index of the step in the plan.
        index: usize,
        /// The step variant name.
        step: &'static str,
        /// What no longer held.
        detail: String,
    },
    /// tmux answered "no" (non-zero exit) to a command the step needed.
    #[error("step {index} tmux {verb} failed: {detail}")]
    Tmux {
        /// Index of the step in the plan.
        index: usize,
        /// The tmux verb that failed.
        verb: String,
        /// Redacted stderr.
        detail: String,
    },
    /// The host executor could not run the command, or its observation was
    /// untrusted (which is never evidence to act on).
    #[error("step {index} ({step}) host error: {source}")]
    Host {
        /// Index of the step in the plan.
        index: usize,
        /// The step variant name.
        step: &'static str,
        /// The underlying host failure.
        #[source]
        source: HostErr,
    },
    /// A binding a step referenced was never minted, or the plan/topology were
    /// inconsistent. Unreachable for a well-formed M1 plan.
    #[error("step {index} internal inconsistency: {detail}")]
    Internal {
        /// Index of the step in the plan.
        index: usize,
        /// What was inconsistent.
        detail: String,
    },
}

impl StepError {
    /// Which step of the plan this happened at.
    ///
    /// Every variant carries it, so the caller can ask the plan whose step it
    /// was — which is how tmux's own sentence about a broken workspace reaches
    /// the crash report of the person whose workspace it is.
    #[must_use]
    pub const fn index(&self) -> usize {
        match self {
            Self::RetryDeferred { index, .. }
            | Self::LaunchRefused { index, .. }
            | Self::Precondition { index, .. }
            | Self::Tmux { index, .. }
            | Self::Host { index, .. }
            | Self::Internal { index, .. } => *index,
        }
    }

    /// WHAT WENT WRONG, in the words of whoever said no — never empty.
    ///
    /// tmux's own stderr when tmux refused, chiefd's own sentence when the
    /// gate declined, the host error when the command could not run. The
    /// crash-loop registry records this on the card of the person whose boot
    /// the failure cost, so an operator reading `crash-looping` is never shown
    /// a blank cause. Every construction site fills its detail with something,
    /// and the interpreter's own `tmux` helper guarantees the tmux one is non-empty even
    /// when tmux itself writes nothing.
    #[must_use]
    pub fn cause(&self) -> String {
        match self {
            Self::RetryDeferred { person, .. } => {
                format!("'{person}' is crash-looping; their retry backoff has not elapsed")
            }
            Self::LaunchRefused { person, reason, .. } => {
                format!("chiefd's launch gate declined '{person}': {reason}")
            }
            Self::Precondition { detail, .. } => format!("precondition missed: {detail}"),
            Self::Tmux { verb, detail, .. } => format!("tmux {verb} said no: {detail}"),
            Self::Host { source, .. } => format!("the host could not run tmux: {source}"),
            Self::Internal { detail, .. } => format!("internal inconsistency: {detail}"),
        }
    }
}

/// The outcome of interpreting one converge plan.
#[derive(Debug)]
pub struct ApplyReport {
    /// How many steps the plan contained.
    pub steps_total: usize,
    /// How many steps executed successfully before any failure.
    ///
    /// A step SKIPPED for a refused person is not one of these: nothing was
    /// applied for them, and counting a skip as an application would report
    /// work that did not happen.
    pub steps_ok: usize,
    /// How many steps the loop REACHED — `steps_total` unless a failure stopped
    /// it part-way.
    ///
    /// `steps_ok` used to be both numbers at once, because a fail-stop
    /// interpreter reaches exactly the steps it completes plus the one that
    /// failed. A skipped refusal breaks that identity, and the caller needs the
    /// reached prefix rather than the completed count: it judges boots against
    /// the steps that were WALKED, and a person ordered after a skip has been
    /// walked past whether or not the skip applied anything.
    pub steps_reached: usize,
    /// The failure that aborted the cycle, or `None` on full success.
    ///
    /// A REFUSED PERSON IS NEVER IN HERE. Their step is skipped and recorded in
    /// `refused`; the pass carries on and, if nothing else went wrong, succeeds.
    pub failure: Option<StepError>,
    /// The people whose step was SKIPPED because chiefd's launch gate declined
    /// them, person id to the gate's own reason.
    ///
    /// Separate from `failure`, because they are a different fact: the plan is
    /// sound, these people cannot be launched right now, and everybody else in
    /// it was attempted. Separate from silence, because an operator has to be
    /// told WHO did not come up and WHY — a pass that quietly dropped somebody
    /// would be the `converged` lie in a new place.
    pub refused: BTreeMap<String, String>,
    /// The people this pass SKIPPED because their retry backoff had not
    /// elapsed.
    ///
    /// Distinct from `refused` on purpose. A refusal is chiefd declining to
    /// publish a launch at all and names a repair; a deferral is this actuator
    /// waiting a bounded moment before trying the same launch again, and names
    /// nothing — it is already being handled. The caller needs them apart
    /// because a deferred person must not be counted as a failed boot: that
    /// would make the backoff feed itself.
    pub deferred: BTreeSet<String>,
    /// #739 P2 (PARTIAL — see module doc "P2 status"): every symbolic
    /// window id → real tmux window id this pass minted, whether or not the
    /// pass ultimately succeeded. Exposed so the caller (`cycle.rs`, which
    /// holds the async `CompanyDb` this module deliberately does not) can
    /// persist them durably via `chiefd`'s writer actor immediately
    /// after this call returns.
    pub windows_bound: BTreeMap<String, String>,
    /// #739 P2 (PARTIAL): every person id → minted tmux pane id this pass
    /// bound, same caveats as `windows_bound`.
    pub panes_bound: BTreeMap<String, String>,
}

impl ApplyReport {
    /// Whether every step executed successfully.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.failure.is_none()
    }
}

/// #739 P2: the committed-row snapshot a caller reads INSIDE its own
/// transaction and hands down as a value — the architect's resolution of
/// the threading question this module's "P2 status" doc raised. The
/// interpreter stays synchronous and pure; it derives its starting bindings
/// from this value instead of always starting empty, and reads it exactly
/// once, so transaction-consistency is a property of the caller's read, not
/// of when or how often the interpreter re-reads state.
///
/// `Default` (both maps empty) is the correct value for a caller with no
/// prior committed state to seed from — a fresh company's first pass, or a
/// caller that predates this parameter and is intentionally exercising the
/// empty-state behavior (every test using [`apply_plan`]'s plain wrapper).
#[derive(Debug, Clone, Default)]
pub struct CommittedBindings {
    /// Logical window id → real tmux window id, as of the caller's read.
    pub windows: BTreeMap<String, String>,
    /// Person id → real tmux pane id, as of the caller's read.
    pub panes: BTreeMap<String, String>,
}

/// The symbolic-id → tmux-id bindings accumulated as the plan executes.
///
/// #739 P2: `windows`/`panes` now SEED from the caller-supplied
/// [`CommittedBindings`] rather than always starting empty — see that
/// type's doc and this module's "P2 status" section. `skipped` has no
/// committed-row analog (it is a per-pass capacity-deferral record, not a
/// durable fact) and still starts empty every pass, unchanged.
#[derive(Debug, Default)]
struct BindingMap {
    /// Logical window id (a [`WindowSym`](plan::WindowSym) string) → tmux window id.
    windows: BTreeMap<String, String>,
    /// Person id of a pane created in this plan → its minted tmux pane id.
    panes: BTreeMap<String, String>,
    /// #522: person ids whose `SplitPane` was DEFERRED because the window was at
    /// capacity even when evenly tiled. These have no `panes` binding on purpose;
    /// `apply_layout` skips them (they are re-attempted next reconcile) so one
    /// over-capacity pane never aborts the whole convergence pass.
    skipped: BTreeSet<String>,
}

impl From<CommittedBindings> for BindingMap {
    fn from(committed: CommittedBindings) -> Self {
        Self { windows: committed.windows, panes: committed.panes, skipped: BTreeSet::new() }
    }
}

/// Apply one converge plan against the host, fail-stop, with per-step
/// precondition re-verify.
///
/// `launch` supplies the per-person host-resolved launch inputs M1 omits from
/// its `SpawnSpec`s. `desired`/`observed` are the same topologies the plan was
/// computed from: the interpreter reads window names and logical ids from
/// `desired` (M1's `Step::CreateSession` is not self-describing) and the
/// observe-time ownership tags from `observed` (the expected-tag side of each
/// precondition re-verify).
///
/// Never returns `Err`: a step failure is recorded in the returned
/// [`ApplyReport`] so the caller can count it toward the circuit breaker and let
/// the next pass re-plan.
#[must_use]
pub fn apply_plan(
    executor: &dyn HostExecutor,
    socket: &Socket,
    desired: &crate::placement::Topology,
    observed: &plan::ObservedTopology,
    launch: &BTreeMap<String, LaunchSpec>,
    plan: &ConvergePlan,
) -> ApplyReport {
    apply_plan_with_launch_roster(
        executor,
        socket,
        desired,
        observed,
        LaunchInputs {
            catalog: launch,
            diagnostics: LaunchRosterDiagnostics::default(),
            deferred: &BTreeSet::new(),
        },
        plan,
        PassContext::default(),
    )
}

/// Restore the fixed sidebar rail in every window of an existing company
/// session.
///
/// This is steady-state maintenance, not a creation-only step. A rail process
/// can exit after its window is created, and a topology diff does not include
/// rails, so the next reconcile must inspect and repair them independently of
/// whether person placement has work to do.
pub fn repair_session_rails(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    company_dir: &str,
) -> Result<usize, String> {
    let executable = crate::sidebar::rail_program()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "the chief executable could not be located".to_owned())?;
    repair_session_rails_with(executor, socket, session, company_dir, &executable)
}

#[derive(Debug, Default)]
struct RailRepairWindow {
    current: Option<crate::window_geometry::Geometry>,
    layout: Option<String>,
    mode: Option<String>,
    panes: Vec<String>,
    rails: Vec<String>,
    columns: Option<i64>,
    collapsed: bool,
}

impl RailRepairWindow {
    fn effective_columns(&self, canonical: crate::window_geometry::Geometry) -> i64 {
        let widest = i64::from(canonical.columns)
            .saturating_sub(2)
            .max(crate::layout::RAIL_COLLAPSED_COLUMNS);
        if self.collapsed {
            crate::layout::RAIL_COLLAPSED_COLUMNS
        } else {
            self.columns
                .unwrap_or(crate::sidebar::brain::RAIL_DEFAULT_COLUMNS)
                .clamp(crate::layout::RAIL_COLLAPSED_COLUMNS, widest)
        }
    }

    fn final_layout(
        &self,
        canonical: crate::window_geometry::Geometry,
    ) -> Result<Option<String>, String> {
        let Some(rail) = self.rails.first() else { return Ok(None) };
        if self.panes.first() != Some(rail) {
            return Err(format!("sidebar rail {rail} is not the first pane in its window"));
        }
        let bodies: Vec<&str> =
            self.panes.iter().filter(|pane| *pane != rail).map(String::as_str).collect();
        if bodies.is_empty() {
            return Ok(None);
        }
        crate::layout::organization_tmux_layout(
            i64::from(canonical.columns),
            i64::from(canonical.rows),
            Some(crate::layout::Rail { pane_id: rail, columns: self.effective_columns(canonical) }),
            &bodies,
        )
        .map(Some)
        .map_err(|error| error.to_string())
    }
}

fn repair_session_rails_with(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    company_dir: &str,
    executable: &std::path::Path,
) -> Result<usize, String> {
    let listed = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "list-panes".into(),
                    "-s".into(),
                    "-t".into(),
                    session.to_owned(),
                    "-F".into(),
                    format!(
                        "#{{window_id}}\t#{{pane_id}}\t#{{{}}}\t#{{window_width}}\t#{{window_height}}\t#{{{}}}\t#{{{}}}\t#{{window_layout}}\t#{{window_size}}",
                        tags::SIDEBAR,
                        super::trust::sidebar_options::COLUMNS,
                        super::trust::sidebar_options::COLLAPSED,
                    ),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if listed.status != 0 {
        return Err(listed.stderr.trim().to_owned());
    }

    let mut windows: BTreeMap<String, RailRepairWindow> = BTreeMap::new();
    for line in listed.stdout.lines() {
        // Tmux output is trimmed by the runner. Every optional field therefore
        // uses an empty default; a missing final width option is not a missing
        // pane or window.
        let mut fields = line.split('\t');
        let window = fields.next().unwrap_or_default().trim();
        let pane = fields.next().unwrap_or_default().trim();
        let marker = fields.next().unwrap_or_default();
        if window.is_empty() {
            continue;
        }
        let size = crate::window_geometry::parse_size(&format!(
            "{}\t{}",
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default()
        ));
        let columns = fields.next().unwrap_or_default().trim().parse::<i64>().ok();
        let collapsed = fields.next().unwrap_or_default().trim() == "1";
        let layout = fields.next().unwrap_or_default().trim();
        let mode = fields.next().unwrap_or_default().trim();
        let state = windows.entry(window.to_owned()).or_default();
        state.current = state.current.or(size);
        state.columns = state.columns.or(columns);
        state.collapsed |= collapsed;
        if state.layout.is_none() && !layout.is_empty() {
            state.layout = Some(layout.to_owned());
        }
        if state.mode.is_none() && !mode.is_empty() {
            state.mode = Some(mode.to_owned());
        }
        if !pane.is_empty() {
            state.panes.push(pane.to_owned());
        }
        if marker.trim() == "1" {
            state.rails.push(pane.to_owned());
        }
    }

    let canonical = crate::window_geometry::capture(session, |argv| {
        let output = executor.tmux(socket, TmuxCmd { argv: argv.to_vec() }).ok()?;
        (output.status == 0).then_some(output.stdout)
    });
    let columns = if windows.values().any(|state| state.rails.is_empty()) {
        effective_rail_columns_with(|option| {
            executor
                .tmux(
                    socket,
                    TmuxCmd {
                        argv: vec![
                            "show-options".into(),
                            "-q".into(),
                            "-v".into(),
                            "-t".into(),
                            session.to_owned(),
                            option.to_owned(),
                        ],
                    },
                )
                .ok()
                .filter(|out| out.status == 0)
                .map(|out| out.stdout)
        })
    } else {
        crate::sidebar::brain::RAIL_DEFAULT_COLUMNS
    };

    let topology_generation = windows
        .values()
        .any(|state| state.rails.is_empty())
        .then(|| invalidate_viewport_manifest(executor, socket, session))
        .transpose()?;
    let result = (|| {
        let mut repaired = 0;
        for (window, mut state) in windows {
            if state.rails.is_empty() {
                let split_target = state.panes.first().cloned().unwrap_or_else(|| window.clone());
                // THE SPLIT AND THE TAG ARE ONE TMUX COMMAND SEQUENCE. Issued as
                // two invocations, the new pane is observable — and its
                // `chief sidebar` process is already running and attaching to the
                // brain — while it is still untagged, and an untagged rail is one
                // no guard in this file can see: every count here keys on
                // `tags::SIDEBAR`. That gap is how a window came to hold two rails
                // at once, and a `set-option` that failed on its own left a live
                // untagged rail behind for good. Batched, tmux runs both in its
                // single command loop with nothing interleaved, so a concurrent
                // `list-panes` sees the window either before the split or after
                // the tag and never in between.
                //
                // The tag targets the WINDOW with `-p`, which tmux resolves to its
                // active pane — the one `split-window` just made active — because a
                // batch cannot name an id the split has not reported yet.
                let minted = executor
                    .tmux(
                        socket,
                        TmuxCmd {
                            argv: vec![
                                "split-window".into(),
                                "-h".into(),
                                "-b".into(),
                                "-l".into(),
                                columns.to_string(),
                                "-t".into(),
                                split_target,
                                "-P".into(),
                                "-F".into(),
                                "#{pane_id}".into(),
                                "-c".into(),
                                company_dir.to_owned(),
                                executable.display().to_string(),
                                "sidebar".into(),
                                ";".into(),
                                "set-option".into(),
                                "-p".into(),
                                "-t".into(),
                                window.clone(),
                                tags::SIDEBAR.into(),
                                "1".into(),
                                ";".into(),
                                // AND THE CURSOR GOES BACK, LAST. A repair
                                // pass that mints a missing rail must not
                                // leave the operator typing into furniture.
                                // `-l` is the window's last pane, which a
                                // split makes the pane that was active before
                                // it — not the split target. It follows the
                                // tag because the tag resolves a WINDOW target
                                // to the active pane, which must still be the
                                // new rail.
                                "select-pane".into(),
                                "-l".into(),
                                "-t".into(),
                                window.clone(),
                            ],
                        },
                    )
                    .map_err(|error| error.to_string())?;
                if minted.status != 0 {
                    return Err(minted.stderr.trim().to_owned());
                }
                // Only `split-window -P` prints; the tag is silent. A first line
                // is therefore the minted pane id, and no line at all means tmux
                // ran the sequence without creating anything.
                let pane = minted.stdout.lines().next().unwrap_or_default().trim();
                if pane.is_empty() {
                    return Err(format!("tmux created no sidebar pane in window {window}"));
                }
                state.panes.insert(0, pane.to_owned());
                state.rails.push(pane.to_owned());
                repaired += 1;
            }
            if state.rails.len() > 1 {
                return Err(format!("window {window} has more than one sidebar rail"));
            }
            let Some(canonical) = canonical else { continue };
            let Some(layout) = state.final_layout(canonical)? else {
                // DEBUG, NOT WARN: a rail-only window is a state the product
                // reaches correctly. A department's window outlives its last
                // person on purpose ("the window survives its last person,
                // because the rail pane is still in it"), and a rail alone in a
                // window already has the whole window — there is no layout to
                // publish and nothing is wrong.
                //
                // At `warn` this fired on every repair pass for as long as such
                // a window existed: 180 lines in one session on a live box, in a
                // log where the genuine fault — a viewport manifest refusing 26
                // times — was three orders of magnitude quieter. A line that
                // fires when the product is working correctly is not a fault,
                // and burying the ones that are is what it costs.
                tracing::debug!(
                    event = "sidebar.rail.geometry-unfurnished",
                    window,
                    "the window holds only its rail, which already fills it; there is no \
                     layout to publish"
                );
                continue;
            };
            let Some(argv) = crate::window_geometry::normalization_with_layout_argv(
                &window,
                state.current,
                canonical,
                state.layout.as_deref(),
                state.mode.as_deref(),
                &layout,
            ) else {
                continue;
            };
            let normalized =
                executor.tmux(socket, TmuxCmd { argv }).map_err(|error| error.to_string())?;
            if normalized.status != 0 {
                return Err(normalized.stderr.trim().to_owned());
            }
        }
        Ok(repaired)
    })();
    if let Some(generation) = topology_generation.as_deref() {
        request_viewport_manifest_refresh(executor, socket, session, generation);
    }
    result
}

/// Publish one complete viewport to every managed window in one tmux command
/// queue.
///
/// All pane ownership and all layouts are validated before the first mutation.
/// The human-owned sidebar preferences are read but never written. Managed
/// windows remain manual so a client resize cannot expose tmux's proportional
/// split while this callback is waiting to run.
pub fn resize_session_viewport(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    columns: u32,
    rows: u32,
) -> Result<usize, String> {
    let canonical = crate::window_geometry::Geometry::new(columns, rows)
        .ok_or_else(|| "the operator viewport must have positive columns and rows".to_owned())?;
    resize_session_viewport_with(executor, socket, session, canonical)
}

/// Apply the viewport carried by one tmux resize event if that event is still
/// the session's newest client generation at the mutation boundary.
pub fn viewport_client_is_eligible(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    client: &str,
    nonce: &str,
) -> Result<bool, String> {
    if !super::trust::is_safe_server_nonce(nonce) {
        return Ok(false);
    }
    let observed = observe_viewport_client(executor, socket, client)?;
    let exact_client = observed.name == client
        && observed.session == session
        && observed.columns.is_some_and(|columns| columns > 0)
        && observed.rows.is_some_and(|rows| rows > 0)
        && !observed.is_control_or_ignored();
    if !exact_client {
        return Ok(false);
    }
    let tagged = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    session.to_owned(),
                    format!(
                        "#{{{}}}\t#{{{}}}",
                        super::trust::tags::ORGANIZATION,
                        super::trust::viewport_options::SERVER_NONCE,
                    ),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if tagged.status != 0 {
        return Err(tagged.stderr.trim().to_owned());
    }
    let mut fields = tagged.stdout.trim().split('\t');
    Ok(!fields.next().unwrap_or_default().is_empty() && fields.next().unwrap_or_default() == nonce)
}

struct ViewportClient {
    session: String,
    columns: Option<u32>,
    rows: Option<u32>,
    flags: String,
    name: String,
}

impl ViewportClient {
    fn is_control_or_ignored(&self) -> bool {
        self.flags.split(',').any(|flag| flag == "control-mode" || flag == "ignore-size")
    }
}

fn observe_viewport_client(
    executor: &dyn HostExecutor,
    socket: &Socket,
    client: &str,
) -> Result<ViewportClient, String> {
    let observed = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "display-message".into(),
                    "-p".into(),
                    "-c".into(),
                    client.to_owned(),
                    "-F".into(),
                    "#{client_session}\t#{client_width}\t#{client_height}\t#{client_flags}\t#{client_pid}\t#{client_name}"
                        .into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if observed.status != 0 {
        return Err(format!("the resized tmux client is no longer present: {}", observed.stderr));
    }
    let mut fields = observed.stdout.trim().split('\t');
    let session = fields.next().unwrap_or_default().to_owned();
    let columns = fields.next().and_then(|value| value.parse::<u32>().ok());
    let rows = fields.next().and_then(|value| value.parse::<u32>().ok());
    let flags = fields.next().unwrap_or_default().to_owned();
    let _pid = fields.next().unwrap_or_default();
    let name = fields.next().unwrap_or_default().to_owned();
    Ok(ViewportClient { session, columns, rows, flags, name })
}

/// The outcome of one attach-time viewport publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachViewportPublication {
    /// The current authority published geometry for this many windows.
    Applied(usize),
    /// A newer company topology or server lifetime replaced the authority.
    Stale,
}

/// Publish attach-time geometry only while the captured tmux server and
/// company topology are still current.
pub fn resize_session_viewport_for_attach(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    organization: &str,
    topology_epoch: u64,
    nonce: &str,
    viewport: (u32, u32),
) -> Result<AttachViewportPublication, String> {
    if !super::trust::is_safe_company_session(session)
        || !super::trust::is_safe_logical_id(organization)
        || !super::trust::is_safe_server_nonce(nonce)
    {
        return Err("attach viewport authority is not safe".to_owned());
    }
    let canonical = crate::window_geometry::Geometry::new(viewport.0, viewport.1)
        .ok_or_else(|| "the operator viewport must have positive columns and rows".to_owned())?;
    let (windows, publication) =
        session_viewport_publication(executor, socket, session, canonical, Some(organization))?;
    if publication.is_empty() {
        return Ok(AttachViewportPublication::Applied(0));
    }
    let command = crate::control::quote_argv(&publication)
        .ok_or_else(|| "the attach viewport publication contains an invalid newline".to_owned())?
        .text;
    let predicate = format!(
        "#{{&&:#{{==:#{{@organization_id}},{organization}}},\
         #{{&&:#{{==:#{{@chief_viewport_topology_epoch}},{topology_epoch}}},\
         #{{==:#{{@chief_viewport_server_nonce}},{nonce}}}}}}}"
    );
    let guarded = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "if-shell".into(),
                    "-F".into(),
                    "-t".into(),
                    session.to_owned(),
                    predicate,
                    format!("{command} ; display-message -p applied"),
                    "display-message -p stale".into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if guarded.status != 0 {
        return Err(guarded.stderr.trim().to_owned());
    }
    match guarded.stdout.trim() {
        "applied" => Ok(AttachViewportPublication::Applied(windows)),
        "stale" => Ok(AttachViewportPublication::Stale),
        marker => Err(format!("attach viewport publication returned an unknown marker: {marker}")),
    }
}

/// Publish the current viewport of one exact tmux hook client while its
/// ordered generation remains the session's newest request.
pub fn resize_session_viewport_for_client(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    organization: &str,
    client: &str,
    event: &str,
    nonce: &str,
) -> Result<usize, String> {
    if !super::trust::is_safe_company_session(session) {
        return Err("the viewport target is not a safe company session".to_owned());
    }
    if !super::trust::is_safe_logical_id(organization) {
        return Err("the viewport organization is not a safe logical id".to_owned());
    }
    if !super::trust::is_safe_server_nonce(nonce) {
        return Err("the viewport server nonce is not safe".to_owned());
    }
    if client.is_empty()
        || client
            .bytes()
            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'#' | b'{' | b'}' | b','))
    {
        return Err("the viewport client is not safe tmux format text".to_owned());
    }
    let generation = event
        .parse::<u64>()
        .map_err(|_| "the viewport request generation must be numeric".to_owned())?
        .to_string();
    let result = (|| {
        let observed = observe_viewport_client(executor, socket, client)?;
        if observed.name != client {
            return Err("the resized tmux client target is stale or was reused".to_owned());
        }
        if observed.session != session {
            return Err(format!(
                "viewport resize client belongs to {}, not {session}",
                observed.session
            ));
        }
        if observed.is_control_or_ignored() {
            return Ok(0);
        }
        let (Some(columns), Some(rows)) = (observed.columns, observed.rows) else {
            return Ok(0);
        };
        let canonical = crate::window_geometry::Geometry::new(columns, rows).ok_or_else(|| {
            "the operator viewport must have positive columns and rows".to_owned()
        })?;
        let (windows, publication) =
            session_viewport_publication(executor, socket, session, canonical, Some(organization))?;
        if publication.is_empty() {
            return Ok(0);
        }
        let command = crate::control::quote_argv(&publication)
            .ok_or_else(|| "the viewport publication contains an invalid newline".to_owned())?
            .text;
        let applied = format!("{command} ; display-message -p applied");
        let guard = viewport_request_guard(organization, client, &generation, nonce)?;
        let guarded = executor
            .tmux(
                socket,
                TmuxCmd {
                    argv: vec![
                        "if-shell".into(),
                        "-t".into(),
                        session.to_owned(),
                        "-F".into(),
                        guard,
                        applied,
                        "display-message -p stale".into(),
                    ],
                },
            )
            .map_err(|error| error.to_string())?;
        if guarded.status != 0 {
            return Err(guarded.stderr.trim().to_owned());
        }
        match guarded.stdout.trim() {
            "applied" => {}
            "stale" => {
                return Err("the resized tmux client became stale before publication".to_owned());
            }
            marker => {
                return Err(format!(
                    "the viewport publication returned an unknown guard marker: {marker}"
                ));
            }
        }
        Ok(windows)
    })();

    // Eligibility and mint are consecutive tmux hook commands, but the exact
    // client can detach or switch between them. Any callback that then refuses
    // must remove only its own still-current request. A newer generation stays
    // authoritative, and the persistent generation is never decreased.
    if result.as_ref().is_err() || matches!(&result, Ok(0)) {
        clear_viewport_request_if_current(
            executor,
            socket,
            session,
            organization,
            client,
            &generation,
            nonce,
        )?;
    }
    result
}

fn clear_viewport_request_if_current(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    organization: &str,
    client: &str,
    generation: &str,
    nonce: &str,
) -> Result<(), String> {
    let clear = crate::control::quote_argv(&[
        "set-option".to_owned(),
        "-qu".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        "@chief_viewport_request".to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-qu".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        "@chief_viewport_owner".to_owned(),
    ])
    .ok_or_else(|| "the viewport cleanup contains an invalid newline".to_owned())?
    .text;
    let cleared = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "if-shell".into(),
                    "-t".into(),
                    session.to_owned(),
                    "-F".into(),
                    viewport_request_guard(organization, client, generation, nonce)?,
                    clear,
                    "display-message -p kept".into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if cleared.status != 0 {
        return Err(cleared.stderr.trim().to_owned());
    }
    Ok(())
}

fn viewport_request_guard(
    organization: &str,
    client: &str,
    generation: &str,
    nonce: &str,
) -> Result<String, String> {
    for (label, value) in [("organization", organization), ("client", client)] {
        if value.is_empty() || value.chars().any(|character| "#{},".contains(character)) {
            return Err(format!("the viewport {label} is not safe tmux format text"));
        }
    }
    if !super::trust::is_safe_server_nonce(nonce) {
        return Err("the viewport server nonce is not safe tmux format text".to_owned());
    }
    Ok(format!(
        "#{{&&:#{{==:#{{@organization_id}},{organization}}},#{{&&:#{{==:#{{@chief_viewport_request}},{generation}}},#{{&&:#{{==:#{{@chief_viewport_owner}},{client}}},#{{==:#{{@chief_viewport_server_nonce}},{nonce}}}}}}}}}"
    ))
}

/// Revoke resize events owned by a client after that client changes sessions.
///
/// The global tmux hook gives Chief the client's new session. Tokens in that
/// session stay valid; a matching token in any other tagged company session
/// belongs to the session the client left and is cleared in one publication.
pub fn revoke_client_viewport_tokens_for_client(
    executor: &dyn HostExecutor,
    socket: &Socket,
    client: &str,
    nonce: &str,
) -> Result<usize, String> {
    let new_session = observe_viewport_client(executor, socket, client)
        .ok()
        .filter(|observed| observed.name == client)
        .map_or_else(String::new, |observed| observed.session);
    revoke_client_viewport_tokens(executor, socket, client, &new_session, nonce)
}

/// Publish the only session that can use the native viewport fast path.
///
/// Client lifecycle hooks clear this server-global option and increment the
/// membership generation before this callback starts. The final CAS prevents
/// an older census from restoring authority after a newer attach, detach, or
/// session change.
pub fn refresh_single_ordinary_viewport_session(
    executor: &dyn HostExecutor,
    socket: &Socket,
    expected_generation: &str,
    nonce: &str,
) -> Result<(), String> {
    let generation = expected_generation
        .parse::<u64>()
        .map_err(|_| "the viewport membership generation must be numeric".to_owned())?;
    if !super::trust::is_safe_server_nonce(nonce) {
        return Err("the viewport server nonce is not safe".to_owned());
    }
    let listed = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "list-clients".into(),
                    "-F".into(),
                    "#{client_name}\t#{client_session}\t#{client_width}\t#{client_height}\t#{client_flags}"
                        .into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if listed.status != 0 {
        return Err(listed.stderr.trim().to_owned());
    }
    let candidates: Vec<(&str, &str)> = listed
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let client = fields.next()?.trim();
            let session = fields.next()?.trim();
            let columns = fields.next()?.trim().parse::<u32>().ok()?;
            let rows = fields.next()?.trim().parse::<u32>().ok()?;
            let flags = fields.next().unwrap_or_default();
            (!client.is_empty()
                && !session.is_empty()
                && columns > 0
                && rows > 0
                && !flags.split(',').any(|flag| flag == "control-mode" || flag == "ignore-size"))
            .then_some((client, session))
        })
        .collect();
    let fast = if let [(client, session)] = candidates.as_slice() {
        if session.starts_with("org-")
            && session.ends_with('_')
            && session
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
        {
            let tagged = executor
                .tmux(
                    socket,
                    TmuxCmd {
                        argv: vec![
                            "display-message".into(),
                            "-p".into(),
                            "-t".into(),
                            (*session).to_owned(),
                            format!(
                                "#{{{}}}\t#{{{}}}\t#{{{}}}",
                                super::trust::tags::ORGANIZATION,
                                super::trust::viewport_options::TOPOLOGY_EPOCH,
                                super::trust::viewport_options::MANIFEST_EPOCH,
                            ),
                        ],
                    },
                )
                .map_err(|error| error.to_string())?;
            let mut fields = tagged.stdout.trim().split('\t');
            let organization = fields.next().unwrap_or_default();
            let topology_epoch = fields.next().unwrap_or_default();
            let manifest_epoch = fields.next().unwrap_or_default();
            (tagged.status == 0
                && !organization.is_empty()
                && organization
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
                && topology_epoch.parse::<u64>().is_ok()
                && topology_epoch == manifest_epoch)
                .then_some((*client, *session, organization.to_owned()))
        } else {
            None
        }
    } else {
        None
    };
    let predicate = format!(
        "#{{&&:#{{==:#{{{}}},{generation}}},#{{==:#{{{}}},{nonce}}}}}",
        super::trust::viewport_options::MEMBERSHIP_GENERATION,
        super::trust::viewport_options::SERVER_NONCE,
    );
    let clear = format!(
        "set-option -gu {} ; set-option -gu {} ; set-option -gu {} ; set-option -gu {}",
        super::trust::viewport_options::FAST_SESSION,
        super::trust::viewport_options::FAST_OWNER,
        super::trust::viewport_options::FAST_ORGANIZATION,
        super::trust::viewport_options::FAST_GENERATION,
    );
    let publish = fast.map_or_else(
        || clear,
        |(client, session, organization)| {
            format!(
                "set-option -g {} {session} ; set-option -g {} {client} ; \
                 set-option -g {} {organization} ; set-option -g {} {generation}",
                super::trust::viewport_options::FAST_SESSION,
                super::trust::viewport_options::FAST_OWNER,
                super::trust::viewport_options::FAST_ORGANIZATION,
                super::trust::viewport_options::FAST_GENERATION,
            )
        },
    );
    let applied = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "if-shell".into(),
                    "-F".into(),
                    predicate,
                    publish,
                    "display-message -p stale".into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if applied.status != 0 {
        return Err(applied.stderr.trim().to_owned());
    }
    Ok(())
}

/// Clear one exact client's request ownership from tagged sessions it left.
pub fn revoke_client_viewport_tokens(
    executor: &dyn HostExecutor,
    socket: &Socket,
    client: &str,
    new_session: &str,
    nonce: &str,
) -> Result<usize, String> {
    if !super::trust::is_safe_server_nonce(nonce) {
        return Err("the viewport server nonce is not safe".to_owned());
    }
    if client.is_empty()
        || client
            .bytes()
            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'#' | b'{' | b'}' | b','))
    {
        return Err("the viewport client is not safe tmux format text".to_owned());
    }
    let listed = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "list-sessions".into(),
                    "-F".into(),
                    "#{session_name}\t#{@organization_id}\t#{@chief_viewport_owner}\t#{@chief_viewport_request}\t#{@chief_viewport_topology_epoch}"
                        .into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if listed.status != 0 {
        return Err(listed.stderr.trim().to_owned());
    }
    let mut targets = Vec::new();
    for line in listed.stdout.lines() {
        let mut fields = line.split('\t');
        let session = fields.next().unwrap_or_default();
        let organization = fields.next().unwrap_or_default();
        let owner = fields.next().unwrap_or_default();
        let request = fields.next().unwrap_or_default();
        let topology_epoch = fields.next().unwrap_or_default();
        if super::trust::is_safe_company_session(session)
            && session != new_session
            && super::trust::is_safe_logical_id(organization)
            && owner == client
            && request.parse::<u64>().is_ok()
            && topology_epoch.parse::<u64>().is_ok()
        {
            targets.push((
                session.to_owned(),
                organization.to_owned(),
                request.to_owned(),
                topology_epoch.to_owned(),
            ));
        }
    }
    if targets.is_empty() {
        return Ok(0);
    }
    let mut argv = Vec::new();
    for (index, (session, organization, request, topology_epoch)) in targets.iter().enumerate() {
        if index > 0 {
            argv.push(";".to_owned());
        }
        let clear = crate::control::quote_argv(&[
            "set-option",
            "-qu",
            "-t",
            session,
            "@chief_viewport_request",
            ";",
            "set-option",
            "-qu",
            "-t",
            session,
            "@chief_viewport_owner",
        ])
        .ok_or_else(|| "the viewport revocation contains an invalid newline".to_owned())?
        .text;
        argv.extend([
            "if-shell".to_owned(),
            "-F".to_owned(),
            "-t".to_owned(),
            session.clone(),
            format!(
                "#{{&&:#{{==:#{{@organization_id}},{organization}}},\
                 #{{&&:#{{==:#{{@chief_viewport_request}},{request}}},\
                 #{{&&:#{{==:#{{@chief_viewport_owner}},{client}}},\
                 #{{&&:#{{==:#{{@chief_viewport_topology_epoch}},{topology_epoch}}},\
                 #{{==:#{{@chief_viewport_server_nonce}},{nonce}}}}}}}}}}}"
            ),
            clear,
            String::new(),
        ]);
    }
    let cleared = executor.tmux(socket, TmuxCmd { argv }).map_err(|error| error.to_string())?;
    if cleared.status != 0 {
        return Err(cleared.stderr.trim().to_owned());
    }
    Ok(targets.len())
}

fn resize_session_viewport_with(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    canonical: crate::window_geometry::Geometry,
) -> Result<usize, String> {
    let (windows, publications) =
        session_viewport_publication(executor, socket, session, canonical, None)?;
    if publications.is_empty() {
        return Ok(0);
    }
    let output =
        executor.tmux(socket, TmuxCmd { argv: publications }).map_err(|error| error.to_string())?;
    if output.status != 0 {
        return Err(output.stderr.trim().to_owned());
    }
    Ok(windows)
}

fn session_viewport_publication(
    executor: &dyn HostExecutor,
    socket: &Socket,
    session: &str,
    canonical: crate::window_geometry::Geometry,
    expected_organization: Option<&str>,
) -> Result<(usize, Vec<String>), String> {
    let organization = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "show-options".into(),
                    "-q".into(),
                    "-v".into(),
                    "-t".into(),
                    session.to_owned(),
                    tags::ORGANIZATION.into(),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if organization.status != 0 || organization.stdout.trim().is_empty() {
        return Err(format!("session {session} is not tagged as a Chief company session"));
    }
    if expected_organization.is_some_and(|expected| organization.stdout.trim() != expected) {
        return Err(format!(
            "session {session} belongs to {}, not {}",
            organization.stdout.trim(),
            expected_organization.unwrap_or_default()
        ));
    }
    let listed = executor
        .tmux(
            socket,
            TmuxCmd {
                argv: vec![
                    "list-panes".into(),
                    "-s".into(),
                    "-t".into(),
                    session.to_owned(),
                    "-F".into(),
                    format!(
                        "#{{window_id}}\t#{{pane_id}}\t#{{{}}}\t#{{window_width}}\t#{{window_height}}\t#{{{}}}\t#{{{}}}\t#{{window_layout}}\t#{{window_size}}\t#{{{}}}",
                        tags::SIDEBAR,
                        super::trust::sidebar_options::COLUMNS,
                        super::trust::sidebar_options::COLLAPSED,
                        tags::WINDOW,
                    ),
                ],
            },
        )
        .map_err(|error| error.to_string())?;
    if listed.status != 0 {
        return Err(listed.stderr.trim().to_owned());
    }

    let mut windows: BTreeMap<String, RailRepairWindow> = BTreeMap::new();
    for line in listed.stdout.lines() {
        let mut fields = line.split('\t');
        let window = fields.next().unwrap_or_default().trim();
        let pane = fields.next().unwrap_or_default().trim();
        let rail = fields.next().unwrap_or_default().trim() == "1";
        let width = fields.next().unwrap_or_default();
        let height = fields.next().unwrap_or_default();
        let columns = fields.next().unwrap_or_default().trim().parse::<i64>().ok();
        let collapsed = fields.next().unwrap_or_default().trim() == "1";
        let layout = fields.next().unwrap_or_default().trim();
        let mode = fields.next().unwrap_or_default().trim();
        let managed_id = fields.next().unwrap_or_default().trim();
        if window.is_empty() || managed_id.is_empty() {
            continue;
        }
        let state = windows.entry(window.to_owned()).or_default();
        state.current = state
            .current
            .or_else(|| crate::window_geometry::parse_size(&format!("{width}\t{height}")));
        state.columns = state.columns.or(columns);
        state.collapsed |= collapsed;
        state.layout.get_or_insert_with(|| layout.to_owned());
        state.mode.get_or_insert_with(|| mode.to_owned());
        if !pane.is_empty() {
            state.panes.push(pane.to_owned());
        }
        if rail {
            state.rails.push(pane.to_owned());
        }
    }
    if windows.is_empty() {
        return Err(format!("session {session} has no managed company windows"));
    }

    let mut publications = Vec::new();
    for (window, state) in &windows {
        if state.rails.len() != 1 {
            return Err(format!(
                "managed window {window} must have exactly one sidebar rail before viewport resize"
            ));
        }
        let argv = if let Some(layout) = state.final_layout(canonical)? {
            crate::window_geometry::normalization_with_layout_argv(
                window,
                state.current,
                canonical,
                state.layout.as_deref(),
                state.mode.as_deref(),
                &layout,
            )
            .unwrap_or_default()
        } else {
            // A managed department can be rail-only while its people are in
            // focus windows. The lone rail is valid furniture and must fill
            // the resized window. It needs geometry and manual ownership, but
            // no rail/body split layout exists to select.
            let mut argv =
                crate::window_geometry::normalization_argv(window, state.current, canonical)
                    .unwrap_or_default();
            if argv.is_empty() && !state.mode.as_deref().is_some_and(|mode| mode == "manual") {
                argv.extend([
                    "set-option".to_owned(),
                    "-w".to_owned(),
                    "-t".to_owned(),
                    window.to_owned(),
                    "window-size".to_owned(),
                    "manual".to_owned(),
                ]);
            }
            argv
        };
        if !argv.is_empty() {
            if !publications.is_empty() {
                publications.push(";".to_owned());
            }
            publications.extend(window_publication_guard(window, state.panes.len(), &argv));
        }
    }
    if publications.is_empty() {
        return Ok((0, publications));
    }
    Ok((windows.len(), publications))
}

/// Fence one window's geometry publication behind the pane census it was
/// computed from.
///
/// THE WEDGE THIS ENDS, in the operator's own words after a hard reboot:
///
/// ```text
/// root@host:~/workspace# chief
/// have 6 panes but need 5: 6a8a,225x47,0,0{26x47,0,0,29,198x47,27,0[...]}
/// ```
///
/// That is tmux's own `select-layout` refusal (`cmd-select-layout.c` prints
/// `<cause>: <layout>`), and it reached the operator's terminal as the WHOLE
/// output of `chief`, which then exited without attaching them to anything.
///
/// [`session_viewport_publication`] reads one `list-panes -s` census and turns
/// it into an ABSOLUTE layout string per window. A layout string enumerates
/// every pane the window holds, so it is only appliable to the exact census it
/// was computed from — and it is applied in a LATER tmux invocation, fenced
/// only on the organization tag, the topology epoch and the server nonce. Not
/// one of those three changes when a pane is added to a window.
///
/// A cold start after an unclean shutdown is precisely the moment when panes
/// are being added: the actuator is a SEPARATE process, converging a whole
/// company from nothing, minting a pane every few hundred milliseconds, while
/// `chief attach` reads its census and publishes it. The census is stale
/// before it lands, tmux refuses the short layout, and the company the
/// operator asked for never appears.
///
/// So each window's publication now carries the census with it. `window_panes`
/// is tmux's own count, evaluated at APPLY time inside the same command queue,
/// so a window that grew between the read and the write is skipped in silence
/// rather than failing the whole publication — and converge, which owns
/// arrangement, lays that window out on its next pass. Nothing is published
/// against a window whose shape has moved.
fn window_publication_guard(window: &str, panes: usize, argv: &[String]) -> Vec<String> {
    vec![
        "if-shell".to_owned(),
        "-F".to_owned(),
        "-t".to_owned(),
        window.to_owned(),
        format!("#{{==:#{{window_panes}},{panes}}}"),
        guarded_command(argv),
    ]
}

/// One tmux command sequence as a single word the guard can carry.
///
/// DOUBLE quotes, and the reason is a measured tmux parse error rather than a
/// preference. tmux 3.2 gave `{` a meaning — it opens a braced command list —
/// so a layout string is not a word tmux will re-lex. A rail-left window's
/// layout is full of them (`981f,120x30,0,0{26x30,0,0,1,93x30,27,0,0}`) and the
/// guard body is re-lexed by definition, once by the outer authority fence and
/// once by this guard. Measured against tmux 3.3a on the same window, same
/// command, three quotings of the layout word:
///
/// ```text
/// bare    -> syntax error (exit 1)
/// '...'   -> syntax error (exit 1)   (single quotes cannot nest in single quotes)
/// "..."   -> applied      (exit 0)
/// ```
///
/// Single quotes are what [`crate::control::quote_argv`] uses for the layer
/// ABOVE this one, so this layer must not use them too: the outer layer would
/// have to escape them and tmux's single-quoted strings take no escapes at all.
/// Double inside single nests cleanly and needs no escaping anywhere.
///
/// A bare `;` stays bare, because it is the sequence separator and not a word —
/// the same rule, and the same reason, as `quote_argv`'s.
fn guarded_command(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            if word == ";" {
                ";".to_owned()
            } else {
                format!(
                    "\"{}\"",
                    word.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read the two independent session preferences and return the width to apply.
fn effective_rail_columns_with(mut read: impl FnMut(&str) -> Option<String>) -> i64 {
    if read(super::trust::sidebar_options::COLLAPSED).is_some_and(|value| value.trim() == "1") {
        return crate::layout::RAIL_COLLAPSED_COLUMNS;
    }
    read(super::trust::sidebar_options::COLUMNS)
        .and_then(|value| value.trim().parse::<i64>().ok())
        .map_or(
            crate::sidebar::brain::RAIL_DEFAULT_COLUMNS,
            crate::sidebar::brain::canonical_columns,
        )
}

/// The launch-roster diagnostic pair `cycle.rs` derives once per cycle so a
/// step failure can name WHICH refusal a person hit, rather than collapsing
/// every "not in launch" case into one interchangeable message (see
/// [`Interpreter::iterated_launch_roster`]/[`Interpreter::refusal_reasons`]
/// for the full history: #52's roster-absent-vs-refused ambiguity). The two
/// fields stay independently optional rather than collapsing into a single
/// `Option<Self>` because they are NOT always supplied together --
/// `interpret/tests.rs` deliberately exercises `iterated_launch_roster: Some`
/// with `refusal_reasons: None` (the roster is known but no precomputed
/// reason exists yet) as a distinct, real state from both-`Some` (a reason
/// was re-derived) and both-`None` (every caller that predates this
/// diagnostic, i.e. every test using [`apply_plan`]'s plain wrapper).
/// Bundling these two -- and *only* these two, not the whole argument list
/// -- is what #840 asks for: they are the two parameters
/// `54d958c9`/`66b05cd7` added, they answer one question together, and
/// `cycle.rs`'s one production call site treats them as one unit (always
/// computed in the same few lines, always passed as a pair).
#[derive(Debug, Default, Clone, Copy)]
pub struct LaunchRosterDiagnostics<'a> {
    /// The exact roster `build_launch_catalog_for_cycle` iterated to build
    /// `launch`. `None` for every caller that predates this diagnostic.
    pub iterated_launch_roster: Option<&'a BTreeSet<String>>,
    /// Precomputed `explain_launch_refusal` results, keyed by person id, for
    /// every person `cycle.rs` found present in the launch roster but absent
    /// from `launch` when this cycle's catalog was built. `None` for every
    /// caller that predates this diagnostic, or that has a roster but no
    /// precomputed reason yet.
    pub refusal_reasons: Option<&'a BTreeMap<String, String>>,
}

/// The launch catalog together with the diagnostics that explain how it was
/// built -- one parameter because one is unreadable without the other.
///
/// [`LaunchRosterDiagnostics`] describes nothing except `catalog`: which
/// roster was iterated to produce it, and why a person the roster carried is
/// absent from it. A caller that has the diagnostics always has the catalog
/// they were derived from, and `cycle.rs` computes both in the same few
/// lines. Nesting the two here is the same grouping #840 made between the
/// diagnostics' own fields, one level out -- and it is what keeps
/// [`apply_plan_with_launch_roster`] inside clippy's argument budget without
/// flattening unrelated parameters into a bag.
#[derive(Debug, Clone, Copy)]
pub struct LaunchInputs<'a> {
    /// The per-person host-resolved launch inputs M1 omits from its
    /// `SpawnSpec`s.
    pub catalog: &'a BTreeMap<String, LaunchSpec>,
    /// How `catalog` was built, so a failure names the real refusal.
    pub diagnostics: LaunchRosterDiagnostics<'a>,
    /// The people whose retry backoff has not elapsed, so this pass must not
    /// spawn them.
    ///
    /// Beside the catalog because it is the same question one layer on: the
    /// catalog says whether a person CAN be launched, and this says whether
    /// they are being launched RIGHT NOW. Both are read by the caller in the
    /// same pass and both cost exactly one step when the answer is no.
    ///
    /// It is a WAIT and never a give-up — see `crash_loop`.
    pub deferred: &'a BTreeSet<String>,
}

/// What one converge PASS carries into the interpreter beyond the plan itself.
///
/// Two facts that are neither the plan nor the world: the bindings a previous
/// partial pass committed, and the person the rail says is selected. They are
/// grouped because they are the same kind of thing — per-pass state, derived
/// each time and never durable (#751-P9) — and because a seventh and eighth
/// positional argument is how a call site starts passing them in the wrong
/// order.
#[derive(Default)]
pub struct PassContext {
    /// Bindings a previous partial pass already committed.
    pub committed: CommittedBindings,
    /// The person the RAIL says is selected, for LOG CONTEXT only.
    ///
    /// **Never a veto.** A selection-based refusal would be wrong in a way that
    /// is easy to reach for: under #1211 a person who has gone stays SELECTED
    /// until the operator clicks elsewhere, so refusing to reap a selected
    /// person's window would make their dead window unreapable for ever — the
    /// same starvation `kill_window`'s own comment warns about, arrived at
    /// through the other door. The destruction veto stays on `window_active`,
    /// which is what the operator is actually LOOKING at.
    ///
    /// It exists because both operator incidents had one signature — the
    /// selection and the current window DIVERGING — and that divergence is
    /// invisible in the record today. With it on the line, the next occurrence
    /// is a grep.
    pub selected_person: Option<String>,
}

/// Same as [`apply_plan`], but with [`LaunchRosterDiagnostics`] for the
/// caller that has already computed them (`cycle.rs`'s real production call
/// site) -- see that type's doc for why they travel together as one
/// parameter rather than two.
///
/// `pass` carries what this converge PASS knows that the plan does not: #739
/// P2's caller-read binding snapshot, and the live selection used for log
/// context. See [`PassContext`]. Pass [`PassContext::default()`] for a caller
/// with nothing to seed from and no selection to report.
#[must_use]
pub fn apply_plan_with_launch_roster(
    executor: &dyn HostExecutor,
    socket: &Socket,
    desired: &crate::placement::Topology,
    observed: &plan::ObservedTopology,
    launch: LaunchInputs<'_>,
    plan: &ConvergePlan,
    pass: PassContext,
) -> ApplyReport {
    let step_changes_manifest = |step: &Step| {
        matches!(
            step,
            Step::CreateWindowWithSpawn { .. }
                | Step::CreateWindowByMove { .. }
                | Step::SplitPane { .. }
                | Step::MovePane { .. }
                | Step::KillPane { .. }
                | Step::KillWindow { .. }
        )
    };
    let selected_person = pass.selected_person.clone();
    let mut interp = Interpreter {
        executor,
        socket,
        desired,
        observed_pane_by_tmux: observed.panes.iter().map(|p| (p.tmux_id.clone(), p)).collect(),
        observed_window_logical: observed
            .windows
            .iter()
            .map(|w| (w.tmux_id.clone(), w.logical_id.clone()))
            .collect(),
        window_of_person: desired
            .windows
            .iter()
            .flat_map(|w| w.panes.iter().map(move |p| (p.person_id.clone(), w.logical_id.clone())))
            .collect(),
        selected_person: selected_person.as_deref(),
        launch: launch.catalog,
        iterated_launch_roster: launch.diagnostics.iterated_launch_roster,
        refusal_reasons: launch.diagnostics.refusal_reasons,
        bindings: BindingMap::from(pass.committed),
        deferred_people: launch.deferred,
        refused_windows: BTreeMap::new(),
        created_panes: Vec::new(),
        canonical_geometry: None,
        pending_focus_selection: None,
    };

    let steps_total = plan.steps.len();
    let mut steps_ok = 0;
    let mut refused: BTreeMap<String, String> = BTreeMap::new();
    let mut deferred: BTreeSet<String> = BTreeSet::new();
    let mut steps_reached = 0;
    let mut topology_generation = None;
    for (index, step) in plan.steps.iter().enumerate() {
        steps_reached = index + 1;
        // EVERY STEP SAYS WHAT IT IS ABOUT TO DO, AND THEN HOW IT WENT.
        //
        // The actuator used to record only the step it DIED on, by index, so a
        // company that would not start produced a log with no attempt in it: no
        // mint, no split, no person, no window. Nobody could name the fault
        // because nothing wrote down what was tried. These three lines (begin,
        // ok, failed) are that record.
        tracing::debug!(
            event = "converge.step.begin",
            step = index,
            kind = step.kind(),
            subject = %step.subject(),
            "attempting this step"
        );
        if topology_generation.is_none() && step_changes_manifest(step) {
            match invalidate_viewport_manifest(executor, socket, &desired.session) {
                Ok(generation) => topology_generation = Some(generation),
                Err(detail) => {
                    tracing::error!(
                        event = "converge.step.failed",
                        step = index,
                        kind = step.kind(),
                        subject = %step.subject(),
                        cause = %detail,
                        "the viewport manifest could not be invalidated before this step, so \
                         the pass stops here"
                    );
                    return ApplyReport {
                        steps_total,
                        steps_ok,
                        steps_reached,
                        failure: Some(StepError::Tmux {
                            index,
                            verb: "invalidate-viewport-manifest".to_owned(),
                            detail,
                        }),
                        refused,
                        deferred,
                        windows_bound: interp.bindings.windows.clone(),
                        panes_bound: interp.bindings.panes.clone(),
                    };
                }
            }
        }
        if let Err(failure) = interp.execute(index, step) {
            // A REFUSED PERSON COSTS THEIR OWN STEP AND NOTHING ELSE.
            //
            // Fail-stop is right for everything else and is not weakened here:
            // a precondition that missed, a tmux that said no, a host that
            // could not run, a plan inconsistent with the topology — every one
            // of those means this pass has proved itself wrong about the world
            // and must stop rather than carry on against a broken plan.
            //
            // chiefd's gate declining a person is none of those. It is an
            // expected condition the daemon re-derives every pass and names in
            // full, and it used to be an `Internal` — so ONE person with a
            // missing file abandoned every healthy person ordered behind them,
            // pass after pass, under `the pass FAILED after X of Y step(s)`.
            // Nobody behind Y was ever attempted.
            //
            // Skipped, named, and the walk continues. The person is recorded in
            // `bindings.skipped` exactly like an over-capacity split (#522), so
            // the trailing `ApplyLayout` does not enumerate a pane that was
            // never minted; and if the skipped step was minting a WINDOW, the
            // window is recorded too, so the steps behind it skip by name
            // instead of failing on an unbound window id.
            // A DEFERRED PERSON COSTS THEIR OWN STEP AND NOTHING ELSE, exactly
            // like a refused one, and by exactly the same machinery: the step
            // is skipped by name, the window it would have minted is recorded
            // so the steps behind it skip by name too, and the walk carries on.
            // The difference from a refusal is only how long it lasts — a
            // refusal waits for an operator, a deferral waits at most ten
            // seconds for nobody.
            if let StepError::RetryDeferred { person, .. } = &failure {
                tracing::debug!(
                    event = "converge.step.retry-deferred",
                    person = %person,
                    step = %index,
                    kind = step.kind(),
                    subject = %step.subject(),
                    "this person is crash-looping and their retry backoff has not elapsed; \
                     skipping their step this pass and trying again shortly"
                );
                interp.bindings.skipped.insert(person.clone());
                if let Some(window) = window_minted_by(step, desired) {
                    interp
                        .refused_windows
                        .insert(window, (person.clone(), DEFERRED_WINDOW_REASON.to_owned()));
                }
                deferred.insert(person.clone());
                continue;
            }
            if let StepError::LaunchRefused { person, reason, .. } = &failure {
                tracing::warn!(
                    event = "converge.step.launch-refused",
                    person = %person,
                    step = %index,
                    kind = step.kind(),
                    subject = %step.subject(),
                    reason = %reason,
                    "chiefd's launch gate declined this person; skipping their step and \
                     continuing with the rest of the plan"
                );
                interp.bindings.skipped.insert(person.clone());
                if let Some(window) = window_minted_by(step, desired) {
                    interp.refused_windows.insert(window, (person.clone(), reason.clone()));
                }
                refused.insert(person.clone(), reason.clone());
                continue;
            }
            // THE FAILING STEP NAMES WHAT IT WAS ATTEMPTING, and what said no.
            // `failure` alone gives the index and the verb; the operator needs
            // the person and the window, and tmux's own words.
            tracing::error!(
                event = "converge.step.failed",
                step = index,
                kind = step.kind(),
                subject = %step.subject(),
                cause = %failure.cause(),
                error = %failure,
                "this step failed and the pass stops here"
            );
            interp.reap_created_panes();
            if let Some(generation) = topology_generation.as_deref() {
                request_viewport_manifest_refresh(executor, socket, &desired.session, generation);
            }
            return ApplyReport {
                steps_total,
                steps_ok,
                steps_reached,
                failure: Some(failure),
                refused,
                deferred,
                // A binding minted before the step that failed is still a
                // real tmux object at this point (reap runs on `created_panes`,
                // which tracks minted PANES for cleanup -- it does not retract
                // this report's record of what was bound before the abort).
                // The caller persists exactly what really exists, not what
                // the plan wanted.
                windows_bound: interp.bindings.windows.clone(),
                panes_bound: interp.bindings.panes.clone(),
            };
        }
        steps_ok += 1;
        tracing::debug!(
            event = "converge.step.ok",
            step = index,
            kind = step.kind(),
            subject = %step.subject(),
            "this step applied"
        );
    }
    if let Some(generation) = topology_generation.as_deref() {
        request_viewport_manifest_refresh(executor, socket, &desired.session, generation);
    }
    ApplyReport {
        steps_total,
        steps_ok,
        steps_reached,
        failure: None,
        refused,
        deferred,
        windows_bound: interp.bindings.windows.clone(),
        panes_bound: interp.bindings.panes.clone(),
    }
}

/// Why a window was not minted when its first person's spawn was deferred.
///
/// The same channel a refusal uses (`refused_windows`), because the structural
/// question is identical: the steps behind an un-minted window must skip by
/// name rather than fail on an unbound window id. The sentence differs because
/// nobody has to do anything about this one.
const DEFERRED_WINDOW_REASON: &str =
    "its first person is crash-looping and their retry backoff has not elapsed";

/// The logical window a step MINTS, when it mints one by spawning its first
/// person.
///
/// Only the two spawn-borne creations qualify: `CreateWindowByMove` moves an
/// existing pane and needs no launch spec, so it cannot be refused. Used to
/// carry a refusal from the step that would have made the window to every step
/// that names it.
fn window_minted_by(step: &Step, desired: &crate::placement::Topology) -> Option<String> {
    match step {
        Step::CreateWindowWithSpawn { w, .. } => Some(w.0.clone()),
        // `CreateSession` mints the session's first window, which is the first
        // window of the desired topology — the same one `create_session` binds.
        Step::CreateSession { .. } => {
            desired.windows.first().map(|window| window.logical_id.clone())
        }
        _ => None,
    }
}

// #18 P2 / task #23: the reap sweep for a window/pane still carrying the
// `tags::MINTING` marker lives in `observe.rs`, folded into `observe()` as an
// extra trailing field on the `list-windows`/`list-panes` calls it already
// makes, rather than as a separate pre-pass here — that avoids adding tmux
// round-trips to every ordinary converge pass (a standalone sweep would have
// needed two more calls on EVERY pass; folding it in needs none, and issues
// a kill only when a torn object genuinely exists). See `observe::observe`'s
// doc for the full reasoning and `tests/interpret_crash.rs` for the crash
// controls.

struct Interpreter<'a> {
    executor: &'a dyn HostExecutor,
    socket: &'a Socket,
    desired: &'a crate::placement::Topology,
    observed_pane_by_tmux: BTreeMap<String, &'a plan::ObservedPane>,
    observed_window_logical: BTreeMap<String, String>,
    window_of_person: BTreeMap<String, String>,
    launch: &'a BTreeMap<String, LaunchSpec>,
    /// The exact roster `build_launch_catalog_for_cycle` iterated to build
    /// `launch` (`org.people_order`), when the caller has it. `None` for
    /// every caller that predates this diagnostic (all current tests) --
    /// they keep the older, undifferentiated message. Only `cycle.rs`'s
    /// real production call site supplies it, because that is the only
    /// place a genuine "which of two failures was this" answer is worth
    /// the extra plumbing: distinguishing "this person was never a
    /// candidate for launch-catalog lookup at all" (absent from this set)
    /// from "was looked up and the resource gate refused" (present in this
    /// set, absent from `launch`) is what a caller chasing #52's
    /// `people_order`/`chief_person_id` mismatch actually needs, and the two
    /// read identically without it.
    iterated_launch_roster: Option<&'a BTreeSet<String>>,
    /// Precomputed `explain_launch_refusal` results, keyed by person id, for
    /// every person `cycle.rs` found present in the launch roster but absent
    /// from `launch` when this cycle's catalog was built. `None` for every
    /// caller that predates this diagnostic. Read-only lookup here -- the
    /// actual explanation is derived once in `cycle.rs`, which already has
    /// the `PersonRecord`/data-root/registry inputs `explain_launch_refusal`
    /// needs; threading those into the interpreter itself would have grown
    /// this module's surface for a diagnostic string, not a behavior.
    refusal_reasons: Option<&'a BTreeMap<String, String>>,
    bindings: BindingMap,
    /// Windows whose CREATING step was skipped because its first person was
    /// The people whose spawn is waiting out a retry backoff this pass.
    deferred_people: &'a BTreeSet<String>,
    /// refused, logical window id -> (person, reason).
    ///
    /// A window is minted by spawning its first pane, so a refused first person
    /// means no window. Every later step that names that window resolves
    /// through `resolve_window`, which answers `LaunchRefused` for these -- so
    /// the tail behind a refused window creation is skipped by name rather than
    /// exploding into `window '...' was referenced before it was created`,
    /// which is an `Internal` and would fail-stop the very pass this exists to
    /// keep alive. Windows this pass did not create are unaffected.
    refused_windows: BTreeMap<String, (String, String)>,
    /// Every pane minted in THIS `apply_plan` call, including a partially-tagged
    /// spawn. Observed/pre-existing panes never enter this list.
    created_panes: Vec<CreatedPane>,
    /// `None` until the first mint or focus selection in this sweep; the inner
    /// value is the one source size captured before that mutation.
    canonical_geometry: Option<Option<crate::window_geometry::Geometry>>,
    /// The focus window that must become visible only after its final layout.
    pending_focus_selection: Option<String>,
    /// Log context only — see `apply_plan_with_launch_roster`'s own note. It is
    /// never read by a guard.
    selected_person: Option<&'a str>,
}

/// What a destructive step is about to take, for [`Interpreter::defer_if_operator_watching`].
///
/// Two shapes because the QUESTION differs. A window verb asks only "is this
/// the active window". A pane verb has to ask a second thing — does removing
/// this pane leave its window standing — because tmux destroys a window with
/// its last pane, and a window left holding only its rail is furniture the
/// operator cannot read.
enum WatchedSubject<'a> {
    /// The whole window goes.
    Window(&'a str),
    /// This pane leaves its window, by kill, join or break.
    PaneLeaving(&'a str),
}

/// What one window holds right now, as `list-panes` answers it.
///
/// The layout step is the only caller, and it needs both halves at once: the
/// full pane set decides whether the layout string it is about to build names
/// every pane tmux will count, and the sleeping set decides which of them are
/// furniture to close.
#[derive(Debug, Default)]
struct WindowCensus {
    /// Every pane id in the window, in tmux's own order.
    panes: Vec<String>,
    /// The subset carrying the ASLEEP tag.
    sleeping: Vec<String>,
}

#[derive(Debug)]
struct CreatedPane {
    pane_id: String,
    /// The process tmux started. It is the fence for partial-tag cleanup.
    pid: Pid,
    /// The session tmux reported at mint time; a generated pane must never be
    /// cleaned from another session.
    session: String,
    /// Present only after all ownership tags succeeded.
    person_id: Option<String>,
    /// Present only after all ownership tags succeeded.
    launch_hash: Option<String>,
}

impl Interpreter<'_> {
    fn execute(&mut self, index: usize, step: &Step) -> Result<(), StepError> {
        match step {
            Step::StopSession => self.stop_session(index),
            Step::CreateSession { first } => self.create_session(index, first),
            Step::CreateWindowWithSpawn { w, name, first } => {
                self.create_window_with_spawn(index, w, name, first)
            }
            Step::CreateWindowByMove { w, name, move_pane } => {
                self.create_window_by_move(index, w, name, move_pane)
            }
            Step::SplitPane { w, spec } => self.split_pane(index, w, spec),
            Step::MovePane { pane, to } => self.move_pane(index, pane, to),
            Step::Respawn { pane, spec } => self.respawn(index, pane, spec),
            Step::Retag { pane, person_id, launch_hash } => {
                self.retag(index, pane, person_id, launch_hash)
            }
            Step::KillPane { pane } => self.kill_pane(index, pane),
            Step::KillWindow { w } => self.kill_window(index, w),
            Step::OrderWindows { order } => self.order_windows(index, order),
            Step::ApplyLayout { w, panes, retire_sleeping_notice } => {
                self.apply_layout(index, w, panes, *retire_sleeping_notice)
            }
        }
    }

    // --- steps -------------------------------------------------------------

    fn stop_session(&mut self, index: usize) -> Result<(), StepError> {
        // Re-verify the session is still ours before killing it wholesale.
        let session = &self.desired.session;
        let read = self.tmux(
            index,
            "kill-session",
            vec![
                "show-options".into(),
                "-v".into(),
                "-t".into(),
                session.clone(),
                tags::ORGANIZATION.into(),
            ],
        )?;
        if read.stdout.trim() != self.desired.organization {
            return Err(StepError::Precondition {
                index,
                step: "StopSession",
                detail: format!(
                    "session '{session}' ownership tag is '{}', expected '{}'",
                    read.stdout.trim(),
                    self.desired.organization
                ),
            });
        }
        self.tmux(
            index,
            "kill-session",
            vec!["kill-session".into(), "-t".into(), session.clone()],
        )?;
        Ok(())
    }

    fn create_session(&mut self, index: usize, first: &plan::SpawnSpec) -> Result<(), StepError> {
        let window = self.desired.windows.first().ok_or_else(|| StepError::Internal {
            index,
            detail: "CreateSession with an empty desired topology".into(),
        })?;
        let logical = window.logical_id.clone();
        // The SANITIZED name, derived rather than stored — `Window::name` is the
        // raw fact chiefd published, `window_name()` is what tmux is told.
        let name = window.window_name();
        let command = self.pane_command(index, "CreateSession", first)?;
        let new_session = vec![
            "new-session".to_owned(),
            "-d".to_owned(),
            "-P".to_owned(),
            "-F".to_owned(),
            "#{pane_id}\t#{window_id}\t#{pane_pid}\t#{session_name}".to_owned(),
            "-s".to_owned(),
            self.desired.session.clone(),
            "-n".to_owned(),
            window_arg(&name),
        ];
        let mut argv = vec!["start-server".to_owned()];
        push_server_input_configuration(&mut argv);
        push_tmux_command(&mut argv, new_session);
        push_launch_flags(&mut argv, &command);
        // §2.0(2) ONE SHOT (F12, architecture-audit Step 2): every identity
        // tag rides the SAME tmux client message as the creating command, as
        // `;`-separated follow-on commands. The tmux client transmits the whole
        // argv as one MSG_COMMAND and the single-threaded server executes the
        // list end-to-end once received — so a SIGKILL of this process lands
        // either before the message was sent (nothing exists) or after (the
        // server finishes the entire list: session + window + pane, all fully
        // tagged). There is no longer any instant at which a minted object
        // exists without its identity, which is what makes the #18 P2 minting
        // markers unnecessary on this path. Identity itself stays in the same
        // `@organization_*` user options observe.rs and the TypeScript side
        // already read — only the transport changed. The session is addressed
        // by its (desired, known-up-front) name; its fresh first window/pane
        // is that session's current window/pane, so `session:` resolves them
        // without knowing the minted ids. Order inside the list matters: the
        // session ownership is the first identity tag. Observation fails
        // closed on any empty ownership read and never destroys a session.
        let first_object = format!("{}:", self.desired.session);
        push_set_option(
            &mut argv,
            &[],
            &self.desired.session,
            tags::ORGANIZATION,
            &self.desired.organization,
        );
        push_set_option(
            &mut argv,
            &["-w"],
            &first_object,
            tags::ORGANIZATION,
            &self.desired.organization,
        );
        push_set_option(&mut argv, &["-w"], &first_object, tags::WINDOW, &logical);
        push_set_option(
            &mut argv,
            &["-p"],
            &first_object,
            tags::ORGANIZATION,
            &self.desired.organization,
        );
        push_set_option(&mut argv, &["-p"], &first_object, tags::WINDOW, &logical);
        push_set_option(&mut argv, &["-p"], &first_object, tags::PERSON, &first.person_id);
        push_set_option(&mut argv, &["-p"], &first_object, tags::LAUNCH_HASH, &first.launch_hash);
        crate::pause::at("interpret:create_session:before_mint");
        let out = self.tmux(index, "new-session", argv)?;
        crate::pause::at("interpret:create_session:after_mint");
        let (pane_id, window_id, pid, minted_session) =
            parse_minted_pane_and_window(index, "new-session", &out.stdout)?;
        self.record_minted_pane(&pane_id, pid, &minted_session);
        self.mark_created_pane_owned(index, &pane_id, &first.person_id, &first.launch_hash)?;
        // NO rail here, and it is not an omission. This mints the SESSION, so
        // the session marker `ensure_rail_in_window` reads cannot exist yet —
        // `attach` sets it, and `attach` runs after this and sweeps every
        // window that exists by then, including this one. A call here would be
        // a tmux round trip that is guaranteed to answer no.
        self.bindings.windows.insert(logical, window_id);
        self.bindings.panes.insert(first.person_id.clone(), pane_id);
        Ok(())
    }

    fn create_window_with_spawn(
        &mut self,
        index: usize,
        w: &plan::WindowSym,
        name: &str,
        first: &plan::SpawnSpec,
    ) -> Result<(), StepError> {
        let logical = w.0.clone();
        if self.adopt_existing_pane(index, &logical, &first.person_id)? {
            return Ok(());
        }
        let canonical = self.canonical_geometry(index);
        let command = self.pane_command(index, "CreateWindowWithSpawn", first)?;
        let mut argv = vec![
            "new-window".to_owned(),
            "-d".to_owned(),
            "-P".to_owned(),
            "-F".to_owned(),
            "#{pane_id}\t#{window_id}\t#{pane_pid}\t#{session_name}".to_owned(),
            "-t".to_owned(),
            self.desired.session.clone(),
            "-n".to_owned(),
            window_arg(name),
        ];
        push_launch_flags(&mut argv, &command);
        let out = self.tmux(index, "new-window", argv)?;
        let (pane_id, window_id, pid, session) =
            parse_minted_pane_and_window(index, "new-window", &out.stdout)?;
        self.record_minted_pane(&pane_id, pid, &session);
        // #18 P2 / task #23: see create_session's identical comment.
        self.mark_minting_window(index, &window_id)?;
        self.mark_minting_pane(index, &pane_id)?;
        self.tag_window(index, &window_id, &logical)?;
        self.tag_pane(index, &pane_id, &logical, &first.person_id, &first.launch_hash)?;
        self.clear_minting_window(index, &window_id)?;
        self.clear_minting_pane(index, &pane_id)?;
        self.mark_created_pane_owned(index, &pane_id, &first.person_id, &first.launch_hash)?;
        // AFTER the person's mint sequence has cleared, and before this
        // window's layout is computed. `observe_rail` discovers the rail at
        // `ApplyLayout` time, so a rail minted now is already the first cell of
        // the very first layout this window gets. Minting it earlier would put
        // an untagged pane inside the mint sequence, which is precisely what
        // the torn-mint detector exists to complain about.
        self.ensure_rail_in_window(index, &window_id);
        if logical != crate::placement::FOCUS_WINDOW_ID {
            self.normalize_window_to(index, &window_id, canonical);
        }
        self.stage_focus_window(index, &logical, &window_id);
        self.bindings.windows.insert(logical, window_id);
        self.bindings.panes.insert(first.person_id.clone(), pane_id);
        Ok(())
    }

    fn create_window_by_move(
        &mut self,
        index: usize,
        w: &plan::WindowSym,
        name: &str,
        move_pane: &plan::PaneId,
    ) -> Result<(), StepError> {
        // A move in disguise: re-verify the pane is still ours before joining.
        self.reverify_owned(index, "CreateWindowByMove", &move_pane.0, None)?;
        let canonical = self.canonical_geometry(index);
        let logical = w.0.clone();
        // §2.0(2) ONE SHOT (F12, architecture-audit Step 2): `break-pane` is
        // the move AND the window mint in ONE server-side operation — no
        // bootstrap window, no join-pane, no kill-pane, no leaked `sleep 3600`
        // — and the fresh window's identity tags ride the SAME client message
        // as `;`-separated follow-on commands (see create_session's comment
        // for the atomicity argument). The follow-on `set-option -w`s target
        // the MOVED pane, whose id is known before the message is built and
        // survives the move, resolving to the window that was just minted
        // around it — the new window's own id is not yet known at argv-build
        // time. A crash lands before the message (nothing changed: the pane
        // is still in its old, tagged window) or after it (moved, into a
        // fully tagged window). The old sequence tagged LAST, which was the
        // worst possible order: a crash left a ZERO-tag window invisible to
        // every torn-object detector, with a duplicate minted next pass.
        //
        // The minted identity is read by a `display-message` FOLLOW-ON in the
        // same message, NOT by `-P -F` on the `break-pane` itself: when the
        // moved pane is the only pane of its window, tmux (observed on 3.3a)
        // re-parents the window instead of minting one and prints NOTHING
        // for `-P`, which read as a failed mint and made the next pass create
        // a duplicate window ("Ambiguous duplicate organization window").
        // `display-message -t <moved pane>` resolves the pane's window in
        // both the mint and the re-parent path (same fix as the TypeScript
        // twin in org-tmux.ts).
        let mut argv = vec![
            "break-pane".into(),
            "-d".into(),
            "-n".into(),
            window_arg(name),
            "-s".into(),
            move_pane.0.clone(),
            "-t".into(),
            format!("{}:", self.desired.session),
            ";".into(),
            "display-message".into(),
            "-p".into(),
            "-t".into(),
            move_pane.0.clone(),
            "#{pane_id}\t#{window_id}\t#{pane_pid}\t#{session_name}".into(),
        ];
        push_set_option(
            &mut argv,
            &["-w"],
            &move_pane.0,
            tags::ORGANIZATION,
            &self.desired.organization,
        );
        push_set_option(&mut argv, &["-w"], &move_pane.0, tags::WINDOW, &logical);
        // BREAK-PANE IS A DEPARTURE TOO, and on tmux 3.3a a SINGLE-pane source
        // window is not emptied but RE-PARENTED — the comment above records
        // that behaviour for a different reason (it prints nothing for `-P`).
        // Either way the operator's window stops being what they were looking
        // at, so it is the same theft and takes the same deferral.
        if self.defer_if_operator_watching(
            index,
            "CreateWindowByMove",
            &WatchedSubject::PaneLeaving(&move_pane.0),
        )? {
            return Ok(());
        }
        crate::pause::at("interpret:create_window_by_move:before_mint");
        let out = self.tmux(index, "break-pane", argv)?;
        crate::pause::at("interpret:create_window_by_move:after_mint");
        let (_pane_id, window_id, _pid, _session) =
            parse_minted_pane_and_window(index, "break-pane", &out.stdout)?;
        // The other post-attach window mint. A person moved out into a
        // department that had no window yet opens one exactly like a department
        // starting does, and an operator cannot tell the two apart — so it
        // gets a rail on the same terms.
        self.ensure_rail_in_window(index, &window_id);
        if logical != crate::placement::FOCUS_WINDOW_ID {
            self.normalize_window_to(index, &window_id, canonical);
        }
        tracing::info!(
            event = "converge.window.minted-by-move",
            pane = %move_pane.0,
            window = %window_id,
            logical = %logical,
            selected_person = self.selected_person.unwrap_or_default(),
            "converge: minted a window by moving an existing pane into it"
        );
        self.stage_focus_window(index, &logical, &window_id);
        self.bindings.windows.insert(logical, window_id);
        Ok(())
    }

    fn split_pane(
        &mut self,
        index: usize,
        w: &plan::WindowRef,
        spec: &plan::SpawnSpec,
    ) -> Result<(), StepError> {
        let window_id = self.resolve_window(index, w)?;
        let logical = self.logical_for_window(index, w)?;
        if self.adopt_existing_pane(index, &logical, &spec.person_id)? {
            return Ok(());
        }
        let command = self.pane_command(index, "SplitPane", spec)?;
        // This split mints the FINAL person pane. Its command begins with the
        // final-pane startup wrapper, which paints immediately and then execs
        // Pi in place. No loading pane, private stage, swap, or cleanup pane
        // exists before or after this identity.
        // WHERE THE PERSON LANDS, and it is not wherever tmux felt like putting
        // them.
        //
        // A bare `split-window -t <window>` splits that window's ACTIVE pane,
        // TOP AND BOTTOM. The active pane is very often the rail, so a woken
        // person appeared UNDERNEATH the sidebar — the operator's report: "it
        // put the new agent below the people section instead of at its final
        // destination. At the very least it should be spawning on the right-hand
        // side as a new column."
        //
        // They are describing the company's own layout: a rail down the left and
        // people as equal COLUMNS beside it (`layout::organization_tmux_layout`).
        // `ApplyLayout` imposes that at the end of the pass, so the misplacement
        // was transient — but a transient wrong position is a visible one, and
        // it is the same class of defect as everything else this file has been
        // fixing: an intermediate geometry the operator can see.
        //
        // `-h` makes the split a COLUMN. Targeting a non-rail pane keeps the
        // split from halving the sidebar on its way to being re-laid — the rail
        // is the one pane in the window whose width the operator chose, and
        // taking half of it, even for a frame, is exactly the jump they have
        // been reporting all evening. With no person pane to split, the window
        // holds only the rail and the window target is right.
        let beside = self.person_pane_to_split(index, &window_id)?;
        let split_argv = |window_id: &str| {
            let mut argv = vec![
                "split-window".to_owned(),
                // A COLUMN, never a row. See above.
                "-h".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{pane_id}\t#{pane_pid}\t#{session_name}".to_owned(),
                "-t".to_owned(),
                beside.clone().unwrap_or_else(|| window_id.to_owned()),
            ];
            push_launch_flags(&mut argv, &command);
            argv
        };
        // #522: `split-window` halves ONE pane each time, so after a handful of
        // naive splits the target pane is below tmux's minimum size and tmux
        // refuses with "no space for new pane" -- at ANY window geometry (seen
        // live on a 225x46 window). The plan's `ApplyLayout` re-tile that would
        // reclaim room runs only AFTER every `SplitPane` for the window, so the
        // splits exhaust space first and the whole apply pass aborts. Recover in
        // place: on that specific refusal, re-tile the window `tiled` (which
        // distributes the existing panes evenly and frees room) and retry the
        // split once. The trailing `ApplyLayout` re-imposes the org layout, so
        // the transient `tiled` is invisible in the settled topology.
        let out = match self.tmux(index, "split-window", split_argv(&window_id)) {
            Ok(out) => out,
            Err(StepError::Tmux { verb, detail, .. })
                if verb == "split-window" && detail.contains("no space") =>
            {
                self.tmux(
                    index,
                    "select-layout",
                    vec!["select-layout".into(), "-t".into(), window_id.clone(), "tiled".into()],
                )?;
                match self.tmux(index, "split-window", split_argv(&window_id)) {
                    Ok(out) => out,
                    Err(StepError::Tmux { verb, detail, .. })
                        if verb == "split-window" && detail.contains("no space") =>
                    {
                        // #522: genuinely over-capacity even when evenly tiled -- a
                        // department larger than the window's whole geometry can
                        // hold. Skip THIS pane (record it, log it) instead of
                        // aborting the entire apply pass: a pass-wide abort blocks
                        // ALL convergence, whereas one deferred pane is simply
                        // re-attempted next reconcile. (Unbounded overflow to a new
                        // window is the tracked #522 follow-up.)
                        tracing::warn!(
                            person = %spec.person_id,
                            window = %window_id,
                            "converge: window at capacity even when tiled; deferring \
                             this pane to a later pass instead of aborting convergence (#522)"
                        );
                        self.bindings.skipped.insert(spec.person_id.clone());
                        return Ok(());
                    }
                    Err(other) => return Err(other),
                }
            }
            Err(other) => return Err(other),
        };
        let (pane_id, pid, session) = parse_minted_pane(index, "split-window", &out.stdout)?;
        self.record_minted_pane(&pane_id, pid, &session);
        // #18 P2 / task #23: only the PANE is a fresh mint here (the window
        // already exists and is already fully tagged), so only the pane
        // needs the marker.
        self.mark_minting_pane(index, &pane_id)?;
        self.tag_pane(index, &pane_id, &logical, &spec.person_id, &spec.launch_hash)?;
        self.clear_minting_pane(index, &pane_id)?;
        self.mark_created_pane_owned(index, &pane_id, &spec.person_id, &spec.launch_hash)?;
        self.bindings.panes.insert(spec.person_id.clone(), pane_id);
        // A woken sleeper's pane is BORN here when their own window already
        // exists — the rail records the selection BEFORE it posts the wake, so
        // placement computes the focus window as soon as chiefd grants it and
        // the spawn lands there directly. This is the half that shows it.
        self.stage_focus_window(index, &logical, &window_id);
        Ok(())
    }

    /// A pane in `window` that is NOT the rail, for a split to take room from.
    ///
    /// The rail is the one pane whose width the operator chose, so it is the one
    /// pane a spawn must not halve — even for the single frame before
    /// `ApplyLayout` re-imposes the company layout. `None` when the window holds
    /// nothing but its rail, where splitting the window itself is correct.
    fn person_pane_to_split(
        &mut self,
        index: usize,
        window_id: &str,
    ) -> Result<Option<String>, StepError> {
        let listed = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-t".into(),
                window_id.to_owned(),
                "-F".into(),
                format!("#{{pane_id}}\t#{{{}}}", super::trust::tags::SIDEBAR),
            ],
        )?;
        Ok(listed
            .stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter(|(pane, is_rail)| !pane.trim().is_empty() && is_rail.trim() != "1")
            .map(|(pane, _)| pane.trim().to_owned())
            .next_back())
    }

    fn move_pane(
        &mut self,
        index: usize,
        pane: &plan::PaneId,
        to: &plan::WindowRef,
    ) -> Result<(), StepError> {
        let expected = self.expected_person(index, "MovePane", &pane.0)?;
        self.reverify_owned(index, "MovePane", &pane.0, Some(&expected))?;
        let window_id = self.resolve_window(index, to)?;
        // A JOIN IS A DEPARTURE FROM SOMEWHERE. `join-pane` takes this pane out
        // of its current window, and tmux destroys a window with its last pane
        // — so a move can take the operator's glass just as surely as a kill,
        // and it did so silently until now.
        if self.defer_if_operator_watching(
            index,
            "MovePane",
            &WatchedSubject::PaneLeaving(&pane.0),
        )? {
            return Ok(());
        }
        self.tmux(
            index,
            "join-pane",
            vec![
                "join-pane".into(),
                "-d".into(),
                "-s".into(),
                pane.0.clone(),
                "-t".into(),
                window_id.clone(),
            ],
        )?;
        // The completion of the operator's OWN recorded gesture.
        // `logical_for_window` can only miss for a window this pass never
        // observed, which is never the focus window when a pane was just moved
        // into it.
        let (selected, matches) = self.selection_context(&window_id);
        tracing::info!(
            event = "converge.pane.moved",
            pane = %pane.0,
            person = %expected,
            to_window = %window_id,
            selected_person = selected,
            selection_matches_window = matches,
            "converge: moved a pane into the window this company wants it in"
        );
        if let Ok(logical) = self.logical_for_window(index, to) {
            self.stage_focus_window(index, &logical, &window_id);
        }
        Ok(())
    }

    /// Stage the PERSON WINDOW for publication after its final layout.
    /// A no-op for every other window.
    ///
    /// # Why the actuator makes one display decision
    ///
    /// The rail performs the ordinary person click itself, so this fires on
    /// exactly one path: the WOKEN SLEEPER. The click records the selection,
    /// posts the wake and ends — the pane does not exist yet, so there is
    /// nothing for the rail to move. chiefd grants the wake, the next converge
    /// pass computes the focus window from that same recorded selection and
    /// spawns or moves the pane into it, `-d` like every other mint in this
    /// interpreter. The later `ApplyLayout` publishes it after its final frame;
    /// without that completion, the operator would watch nothing happen and
    /// have to click the person a second time.
    ///
    /// This is not the actuator inventing a view. Only the selection option can
    /// put a pane in that window, the option is the operator's own recorded
    /// gesture, and selecting the window is the last half of it. Every other
    /// window this interpreter mints or moves stays `-d`, so converge remains
    /// invisible to which window the operator is on
    /// (`a_pane_moved_into_an_ordinary_window_moves_nobodys_glass`).
    ///
    /// The rail-side alternative does not work: tmux pane births emit no
    /// changefeed event, so the rail's wake can precede the pane, and the next
    /// wake is up to 120s away.
    ///
    /// Geometry is prepared here while the destination is hidden. Selection is
    /// not a separate tmux invocation: `apply_layout` appends it after the final
    /// layout so no visible placeholder frame can precede that layout.
    fn stage_focus_window(&mut self, index: usize, logical: &str, window_id: &str) {
        if logical != crate::placement::FOCUS_WINDOW_ID {
            return;
        }
        let canonical = self.canonical_geometry(index);
        self.normalize_window_to(index, window_id, canonical);
        self.pending_focus_selection = Some(window_id.to_owned());
    }

    fn canonical_geometry(&mut self, index: usize) -> Option<crate::window_geometry::Geometry> {
        if let Some(captured) = self.canonical_geometry {
            return captured;
        }
        let session = self.desired.session.clone();
        let captured = crate::window_geometry::capture(&session, |argv| {
            self.tmux(index, "display-message", argv.to_vec()).ok().map(|out| out.stdout)
        });
        self.canonical_geometry = Some(captured);
        captured
    }

    fn normalize_window_to(
        &mut self,
        index: usize,
        window_id: &str,
        canonical: Option<crate::window_geometry::Geometry>,
    ) {
        let Some(canonical) = canonical else { return };
        let current = self
            .tmux(
                index,
                "display-message",
                vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    window_id.to_owned(),
                    "#{window_width}\t#{window_height}".into(),
                ],
            )
            .ok()
            .and_then(|out| crate::window_geometry::parse_size(&out.stdout));
        let Some(argv) = crate::window_geometry::normalization_argv(window_id, current, canonical)
        else {
            return;
        };
        if let Err(error) = self.tmux(index, "normalize-window-geometry", argv) {
            tracing::warn!(
                window = window_id,
                diagnostic = %error,
                "converge: the destination window kept its old geometry; a later sweep retries"
            );
        }
    }

    fn respawn(
        &mut self,
        index: usize,
        pane: &plan::PaneId,
        spec: &plan::SpawnSpec,
    ) -> Result<(), StepError> {
        // Re-verify the launch hash is still stale (still the old one) and the
        // pane is still ours and still this person's.
        let identity = self.pane_identity(index, "Respawn", &pane.0)?;
        if identity.organization != self.desired.organization {
            return Err(StepError::Precondition {
                index,
                step: "Respawn",
                detail: format!("pane {} is no longer ours", pane.0),
            });
        }
        if identity.person_id != spec.person_id {
            return Err(StepError::Precondition {
                index,
                step: "Respawn",
                detail: format!(
                    "pane {} is now person '{}', expected '{}'",
                    pane.0, identity.person_id, spec.person_id
                ),
            });
        }
        if identity.launch_hash == spec.launch_hash {
            return Err(StepError::Precondition {
                index,
                step: "Respawn",
                detail: format!(
                    "pane {} is already at launch hash {}; nothing to respawn",
                    pane.0, spec.launch_hash
                ),
            });
        }
        // THE ONE DRIFT CASE. This pane exists, is ours, and its tag does not
        // match: the process is stale and is replaced against its transcript.
        // Nothing is said to the agent about it -- this client used to pass a
        // cause here so chiefd's sentence for a moved launch hash could be
        // selected rather than the vanished-process one, and no sentence is
        // published at all now.
        let command = self.pane_command(index, "Respawn", spec)?;
        let mut argv =
            vec!["respawn-pane".to_owned(), "-k".to_owned(), "-t".to_owned(), pane.0.clone()];
        push_launch_flags(&mut argv, &command);
        self.tmux(index, "respawn-pane", argv)?;
        // Update the launch-hash tag to the new hash (the pane id survives).
        let logical = self
            .window_of_person
            .get(&spec.person_id)
            .cloned()
            .unwrap_or_else(|| identity.person_id.clone());
        self.tag_pane(index, &pane.0, &logical, &spec.person_id, &spec.launch_hash)?;
        Ok(())
    }

    fn retag(
        &mut self,
        index: usize,
        pane: &plan::PaneId,
        person_id: &str,
        launch_hash: &str,
    ) -> Result<(), StepError> {
        // Retag is idempotent by nature and applies unconditionally.
        let logical =
            self.window_of_person.get(person_id).cloned().ok_or_else(|| StepError::Internal {
                index,
                detail: format!("no desired window for retagged person '{person_id}'"),
            })?;
        self.tag_pane(index, &pane.0, &logical, person_id, launch_hash)
    }

    /// **THE ONE PLACE THAT ASKS "IS THE OPERATOR LOOKING AT THIS?"**
    ///
    /// Four steps in this interpreter can destroy or collapse a window, and
    /// until now exactly ONE of them asked — `kill_window`. That asymmetry is
    /// the defect, not an omission in three functions: a guard that lives
    /// inside one step has to be remembered by the author of the next one, and
    /// the operator reported the same "everything jumped to the Chief" twice
    /// because `kill_pane`, `join-pane` and `break-pane` could each take the
    /// window out from under them silently.
    ///
    /// **The Chief is not the target and there is no CEO-specific code.** When
    /// a window dies under a client, tmux walks last-used → previous → next
    /// (session.c, `session_detach`), and index 0 is where that lands — which
    /// is the Chief's window. `kill_window`'s own comment already records the
    /// historical version of this. Do not go looking for intent.
    ///
    /// # A deferral, never a `StepError`
    ///
    /// Nothing is wrong with the world; the operator is merely looking at it. A
    /// failure would abort the whole apply pass over a watched window and keep
    /// doing so for as long as they watched it. `Ok(true)` means "deferred, skip
    /// your verb"; the next pass reaps once they have moved on.
    ///
    /// Starvation is real and handled elsewhere — the rail moves the operator
    /// off a spent window by showing that person's CARD. Do not "fix"
    /// starvation by weakening this; CLAUDE.md's wake-lease lesson is the same
    /// shape.
    fn defer_if_operator_watching(
        &mut self,
        index: usize,
        verb: &str,
        subject: &WatchedSubject<'_>,
    ) -> Result<bool, StepError> {
        // ONE READ for both questions — which window is active, and what is in
        // each window — because a pane-level verb has to know whether removing
        // this pane leaves the window standing. `list-panes -s` is per SESSION,
        // so no client enumeration.
        let listed = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-s".into(),
                "-t".into(),
                self.desired.session.clone(),
                "-F".into(),
                format!(
                    "#{{window_id}}\t#{{pane_id}}\t#{{window_active}}\t#{{{}}}\t\
                     #{{session_attached}}",
                    tags::SIDEBAR
                ),
            ],
        )?;
        let mut active: Option<String> = None;
        let mut owner: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut bodies: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut total: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for line in listed.stdout.lines() {
            let mut fields = line.split('\t');
            let window = fields.next().unwrap_or_default().trim();
            let pane = fields.next().unwrap_or_default().trim();
            let is_active = fields.next().unwrap_or_default().trim() == "1";
            let is_rail = fields.next().unwrap_or_default().trim() == "1";
            // NOBODY IS LOOKING AT AN UNATTACHED SESSION, and `window_active`
            // does not know that: it is true of whichever window a session
            // last had current, client or no client. Deferring on it with
            // nothing attached would stall every reap on a headless server for
            // ever — no navigation can ever release it, because there is
            // nobody to navigate. Read in the SAME `list-panes` rather than a
            // second `list-clients`, so this stays one probe.
            //
            // An ABSENT field reads as attached, deliberately: a reader that
            // cannot tell must assume somebody is looking. Only an explicit
            // `0` from tmux releases the guard.
            let attached = fields.next().unwrap_or_default().trim() != "0";
            if window.is_empty() || pane.is_empty() {
                continue;
            }
            if is_active && attached {
                active = Some(window.to_owned());
            }
            owner.insert(pane.to_owned(), window.to_owned());
            *total.entry(window.to_owned()).or_default() += 1;
            if !is_rail {
                *bodies.entry(window.to_owned()).or_default() += 1;
            }
        }
        let Some(active) = active else {
            // No active window means no operator to steal from. Proceed —
            // failing closed here would stall every reap on a session nobody is
            // attached to, which is the ordinary headless case.
            return Ok(false);
        };
        let (watched, window, why) = match subject {
            WatchedSubject::Window(window_id) => {
                (active == *window_id, (*window_id).to_owned(), "the whole window goes")
            }
            WatchedSubject::PaneLeaving(pane_id) => {
                let Some(window) = owner.get(*pane_id).cloned() else {
                    // A pane tmux does not list is a pane that has already
                    // gone; there is nothing left to take from anybody.
                    return Ok(false);
                };
                // A WINDOW DIES WITH ITS LAST PANE (tmux(1), kill-pane), and it
                // is EMPTIED OF MEANING with its last body pane: a rail alone
                // is furniture, and leaving the operator staring at a rail is
                // the same theft as taking the window. Both count.
                let leaves_nothing = total.get(&window).copied().unwrap_or(0) <= 1;
                let leaves_only_furniture = bodies.get(&window).copied().unwrap_or(0) <= 1;
                (
                    active == window && (leaves_nothing || leaves_only_furniture),
                    window,
                    if leaves_nothing { "its last pane goes" } else { "its last body pane goes" },
                )
            }
        };
        if watched {
            // THE DIVERGENCE, ON THE LINE. Both operator incidents had one
            // signature — the selection naming one person while the current
            // window showed another — and reconstructing that after the fact
            // cost an investigation each time.
            // Read from the map this interpreter already holds, never with a
            // fresh tmux call: a log field must not add a probe to a hot path,
            // and an absent mapping is itself honest (`selection_matches_window
            // = false` for a window this pass never observed).
            let (selected, matches) = self.selection_context(&window);
            tracing::warn!(
                event = "converge.watched.deferred",
                verb,
                window = %window,
                reason = why,
                selected_person = selected,
                selection_matches_window = matches,
                "converge: this step would take the window the operator is looking at; \
                 deferring it rather than moving their glass out from under them"
            );
            return Ok(true);
        }
        Ok(false)
    }

    /// The selection, and whether it names the window in question.
    ///
    /// LOG CONTEXT ONLY. Never a guard input — see
    /// `apply_plan_with_launch_roster`'s note on why a selection-based veto
    /// would make a gone person's window unreapable for ever.
    fn selection_context(&self, window_id: &str) -> (&str, bool) {
        let selected = self.selected_person.unwrap_or_default();
        let matches = !selected.is_empty()
            && self
                .observed_window_logical
                .get(window_id)
                .is_some_and(|logical| *logical == crate::placement::person_window_id(selected));
        (selected, matches)
    }

    fn kill_pane(&mut self, index: usize, pane: &plan::PaneId) -> Result<(), StepError> {
        // THE critical safety check: kill only if the live pane is still ours
        // AND still tagged with the person the plan expected to remove.
        let expected = self.expected_person(index, "KillPane", &pane.0)?;
        self.reverify_owned(index, "KillPane", &pane.0, Some(&expected))?;
        // AFTER the TOCTOU re-verifications and before the destructive command,
        // exactly as `kill_window` orders the same pair. The two say different
        // things and must stay distinct: a re-verification failure says the
        // MODEL is wrong and stays a hard error; this says the operator is
        // looking.
        //
        // The planner's alone→KillWindow / shared→KillPane split
        // (`plan.rs`) means a watched person's stop arrives here whenever they
        // share a window, so the guard cannot depend on which step was chosen.
        if self.defer_if_operator_watching(
            index,
            "KillPane",
            &WatchedSubject::PaneLeaving(&pane.0),
        )? {
            return Ok(());
        }
        self.tmux(index, "kill-pane", vec!["kill-pane".into(), "-t".into(), pane.0.clone()])?;
        // `selected_person` without `selection_matches_window`: a pane verb
        // does not hold its window id here, and issuing a probe to decorate a
        // log line would put a tmux round trip on a hot path. The DEFERRAL —
        // the line that matters for the divergence signature — carries both.
        tracing::info!(
            event = "converge.pane.killed",
            pane = %pane.0,
            person = %expected,
            selected_person = self.selected_person.unwrap_or_default(),
            "converge: killed a pane whose person this company no longer wants"
        );
        Ok(())
    }

    /// Kill an owned window nothing is desired in any more — the spent zoom
    /// window, or a department window whose people have all left it and whose
    /// rail pane is keeping it alive (tmux destroys a window only when its
    /// LAST pane goes — tmux(1), kill-pane — so no pane-level step can ever
    /// finish this).
    ///
    /// The TOCTOU guard is the same shape as [`Self::kill_pane`]'s and it
    /// matters more here, because this verb takes a WINDOW and everything in
    /// it. Two live re-reads gate the kill:
    ///
    /// 1. The window's tags: it must still be OURS, and its logical id must
    ///    still name a window the desired topology does NOT hold — the same
    ///    predicate the planner emitted the step from, re-asked at apply time.
    ///    A window re-tagged, re-used, or whose department came back between
    ///    observe and apply is refused rather than destroyed.
    /// 2. The window's panes: no live pane in it may carry the person tag of
    ///    anybody in the desired set. The plan's own ordering moves every
    ///    desired pane out before this step runs and a failed step aborts the
    ///    plan before this one executes, so a desired person found here means
    ///    the world moved — refuse, and let the next pass re-observe.
    ///
    /// 3. The session's ACTIVE window: if this is it, the kill is DEFERRED
    ///    (`Ok`, with a warning) rather than performed. tmux would otherwise
    ///    move the operator to the last-used window, then the previous, then the
    ///    next (tmux session.c, `session_detach`) — measured live as "every
    ///    click landed me on the CEO". See the guard's own note below.
    fn kill_window(&mut self, index: usize, w: &plan::WindowRef) -> Result<(), StepError> {
        let window_id = self.resolve_window(index, w)?;
        let live = self.tmux(
            index,
            "display-message",
            vec![
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                window_id.clone(),
                "-F".into(),
                format!("#{{{}}}\t#{{{}}}", tags::ORGANIZATION, tags::WINDOW),
            ],
        )?;
        let line = live.stdout.lines().next().unwrap_or_default();
        let mut fields = line.split('\t');
        let organization = fields.next().unwrap_or_default().trim();
        let logical = fields.next().unwrap_or_default().trim();
        let still_undesired = !logical.is_empty()
            && !self.desired.windows.iter().any(|window| window.logical_id == logical);
        if organization != self.desired.organization || !still_undesired {
            return Err(StepError::Precondition {
                index,
                step: "KillWindow",
                detail: format!(
                    "window {window_id} is now organization '{organization}' window \
                     '{logical}', which is not an owned window this company has retired"
                ),
            });
        }
        let panes = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-t".into(),
                window_id.clone(),
                "-F".into(),
                format!("#{{{}}}\t#{{pane_dead}}", tags::PERSON),
            ],
        )?;
        let desired_person = panes.stdout.lines().find_map(|line| {
            let (person, dead) = line.split_once('\t')?;
            let person = person.trim();
            (!person.is_empty()
                && dead.trim() != "1"
                && self
                    .desired
                    .windows
                    .iter()
                    .any(|window| window.panes.iter().any(|pane| pane.person_id == person)))
            .then(|| person.to_owned())
        });
        if let Some(person) = desired_person {
            return Err(StepError::Precondition {
                index,
                step: "KillWindow",
                detail: format!(
                    "window {window_id} still holds a live pane of desired person \
                     '{person}'; refusing to kill it"
                ),
            });
        }
        // NEVER REAP A WINDOW HOLDING THE RAIL'S OWN FURNITURE.
        //
        // The rail mints a sleeping notice that is not a person. It lives in a window that
        // placement does not want — an empty department gets no window, and a
        // person who is not up yet has no pane — so this step planned to destroy
        // them, and did, seconds after the operator clicked.
        //
        // MEASURED on a live company: the sleeping-department notice appeared on
        // one click and was gone within twenty seconds, with the glass thrown
        // back to the executive window. What that teaches an operator is to
        // click twice, which is exactly what they reported.
        //
        // Observation now marks complete, clean furniture on its window, so the
        // planner does not normally create this step. This live re-read remains
        // necessary for a race and for partial or unknown ownership markers,
        // which observation quarantines instead of trusting as protected UI.
        let furniture = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-t".into(),
                window_id.clone(),
                "-F".into(),
                format!("#{{{}}}", tags::ASLEEP),
            ],
        )?;
        if furniture
            .stdout
            .lines()
            .any(|line| line.split('\t').any(|value| !value.trim().is_empty()))
        {
            tracing::debug!(
                window = %window_id,
                logical = %logical,
                "converge: apply-time furniture guard refused a retired-window kill"
            );
            return Ok(());
        }

        // NEVER REAP THE WINDOW THE OPERATOR IS ON.
        //
        // This is the guard the ancestor of the person-window design did not
        // have, and its absence is what killed that design: moving a person out
        // emptied their department window, the window became undesired, this
        // step destroyed it WHILE THE OPERATOR WAS LOOKING AT IT, and tmux fell
        // back last-used → previous → next — which was the CEO, every time.
        //
        // It USED TO LIVE HERE, inline, and being the only step that had it is
        // what let three sibling steps take the glass silently for months. The
        // rule and its full reasoning are now in
        // `defer_if_operator_watching`; what stays here is the ORDERING that is
        // specific to this verb: asked AFTER the TOCTOU re-verifications, so
        // those keep their exact sequence, and before the only destructive
        // command in this function. The two kinds of refusal must not blur —
        // a re-verification failure says the MODEL is wrong and stays a hard
        // error; this one says the operator is looking.
        if self.defer_if_operator_watching(
            index,
            "KillWindow",
            &WatchedSubject::Window(&window_id),
        )? {
            return Ok(());
        }
        // SAID OUT LOUD, ALWAYS. Destroying a window the operator can see is
        // the most visible thing this pass does, and it used to be the only one
        // that logged NOTHING — so a window vanishing under somebody was
        // invisible in the record and cost a live investigation to attribute.
        let (selected, matches) = self.selection_context(&window_id);
        tracing::info!(
            event = "converge.window.reaped",
            window = %window_id,
            logical = %logical,
            selected_person = selected,
            selection_matches_window = matches,
            "converge: destroyed a window this company no longer wants"
        );
        // Killing a window reflows every rail attached to this session's
        // client. The brain is told once, after the whole pass, rather than
        // being warned per step through a session option — see the tombstone on
        // `gesture_stamp_argv`.
        let argv = vec!["kill-window".to_owned(), "-t".to_owned(), window_id];
        self.tmux(index, "kill-window", argv)?;
        Ok(())
    }

    fn order_windows(&mut self, index: usize, order: &[plan::WindowRef]) -> Result<(), StepError> {
        // Thread each window to directly after the previous one, giving the
        // managed windows a contiguous ascending order. Best-effort but honest:
        // a failed move is a step failure like any other.
        let mut previous: Option<String> = None;
        for window in order {
            // A window whose first person was refused does not exist this pass,
            // so it takes no place in the order and the windows that DO exist
            // are still ordered. Skipping the whole step instead would leave a
            // company's windows unordered for as long as one refusal lasted.
            let window_id = match self.resolve_window(index, window) {
                Err(StepError::LaunchRefused { .. }) => continue,
                other => other?,
            };
            if let Some(prev) = &previous {
                self.tmux(
                    index,
                    "move-window",
                    vec![
                        "move-window".into(),
                        // `-d`: reordering must never change the ACTIVE window
                        // for attached clients — without it each moved window
                        // activates in sequence and a watching operator lands
                        // on the last one (the TypeScript side has always
                        // passed -d for this reason, org-tmux.ts).
                        "-d".into(),
                        "-a".into(),
                        "-s".into(),
                        window_id.clone(),
                        "-t".into(),
                        prev.clone(),
                    ],
                )?;
            }
            previous = Some(window_id);
        }
        Ok(())
    }

    fn apply_layout(
        &mut self,
        index: usize,
        w: &plan::WindowRef,
        panes: &[plan::PaneRef],
        retire_sleeping_notice: bool,
    ) -> Result<(), StepError> {
        let window_id = self.resolve_window(index, w)?;
        let select_when_final = self.pending_focus_selection.as_deref() == Some(&window_id);
        let dims = self.tmux(
            index,
            "display-message",
            vec![
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                window_id.clone(),
                "-F".into(),
                "#{window_width}\t#{window_height}\t#{window_layout}".into(),
            ],
        )?;
        let (width, height) = parse_dimensions(index, &dims.stdout)?;
        let current_layout =
            dims.stdout.split('\t').nth(2).map(str::trim).unwrap_or_default().to_owned();
        // #522: a pane deferred by `split_pane` (window over-capacity even when
        // tiled) has no binding on purpose -- lay out only the panes that exist,
        // never abort the layout over a deliberately-skipped one. Any OTHER
        // missing binding is still a real inconsistency and surfaces from
        // `resolve_pane`.
        let pane_ids: Vec<String> = panes
            .iter()
            .filter(|p| {
                !matches!(p, plan::PaneRef::Created(person) if self.bindings.skipped.contains(person))
            })
            .map(|p| self.resolve_pane(index, p))
            .collect::<Result<_, _>>()?;
        // A sleeping department notice is furniture, not a desired person.
        // A speculative spawn keeps it in the layout. Only a plan backed by a
        // positive live-person observation removes it in the same command
        // sequence that applies the final layout.
        let census = self.window_census(index, &window_id)?;
        let sleeping = census.sleeping;
        let mut layout_panes = pane_ids;
        if !retire_sleeping_notice {
            layout_panes.extend(sleeping.iter().cloned());
        }
        let rail = self.observe_rail(index, &window_id)?;
        // EVERY PANE THE WINDOW ACTUALLY HOLDS, NOT ONLY THE ONES THIS PLAN
        // NAMED. A layout string enumerates the whole window (see the tombstone
        // in `layout.rs`), so tmux rejects one that is short by even a single
        // pane — `select-layout` answers `have 7 panes but need 6` and the step
        // FAILS. Fail-stop then throws away every step after it.
        //
        // THE WEDGE THIS ENDS, on the operator's own company. One un-tagged
        // stray pane sat in a company window. The planner quarantined it,
        // correctly — a stray is skipped and left untouched, never killed —
        // and then this layout was computed without it, against a window that
        // still held it. Every pass reached this step, failed, and abandoned
        // the four spawn steps behind it. The kills and splits BEFORE it had
        // already run, so each pass minted twelve fresh panes and died; the
        // crash-loop registry read that churn as twelve people dying, five
        // passes running, and held the entire company. Thirteen people sat at
        // `starting` for ever behind one pane nobody had tagged.
        //
        // A pane this pass will not touch is still a pane that has to be
        // GIVEN A CELL. Quarantine decides that converge does not manage a
        // pane; it cannot decide that tmux stops counting it.
        let mut unaccounted = Vec::new();
        for pane in census.panes {
            if layout_panes.contains(&pane)
                || rail.as_ref().is_some_and(|(rail_pane, _)| rail_pane == &pane)
                || (retire_sleeping_notice && sleeping.contains(&pane))
            {
                continue;
            }
            unaccounted.push(pane);
        }
        if !unaccounted.is_empty() {
            // NAMED, because the layout now absorbs them silently and an
            // operator who never sees this line has no way to learn that
            // something is squatting in their company's window.
            tracing::warn!(
                event = "converge.layout.unmanaged-panes",
                window = %window_id,
                panes = ?unaccounted,
                "this window holds panes this plan does not manage — quarantined strays, most \
                 likely; they are given cells in the layout so the arrangement can apply at all, \
                 and are otherwise left untouched"
            );
            layout_panes.extend(unaccounted);
        }
        let refs: Vec<&str> = layout_panes.iter().map(String::as_str).collect();
        let layout = plan::organization_tmux_layout(
            width,
            height,
            rail.as_ref().map(|(pane_id, columns)| crate::layout::Rail {
                pane_id: pane_id.as_str(),
                columns: *columns,
            }),
            &refs,
        )
        .map_err(|error| StepError::Tmux {
            index,
            verb: "select-layout".into(),
            detail: error.to_string(),
        })?;
        // The layout the window already has is not re-applied. `select-layout`
        // with an absolute string is a window RESIZE as much as an arrangement
        // (measured, tmux 3.7: applying an 80x24 layout to a live 200x50 window
        // shrinks the window to 80x24), so re-stating an identical layout is
        // never free — it is a chance for the converge loop to re-pin geometry
        // a client attach had just corrected. The comparison is on tmux's own
        // layout string, checksum included, so it fails safe: a mismatch costs
        // one redundant `select-layout`, which is what this step did every time
        // before.
        if current_layout == layout
            && (!retire_sleeping_notice || sleeping.is_empty())
            && !select_when_final
        {
            return Ok(());
        }
        // ONE COMMAND, SO THERE IS NO FRAME BETWEEN THE KILL AND THE LAYOUT.
        //
        // THE FLICKER THIS ENDS, in the operator's words: "when the Pi comes
        // in, it flickers on the sidebar for once, and then it takes over the
        // loading part. It's really subtle but it's really annoying."
        //
        // Killing a pane is a RESIZE of everything left in the window: tmux
        // redistributes the dead pane's columns immediately, so the rail —
        // 26 columns beside a placeholder — briefly becomes half the window,
        // and the `select-layout` that follows puts it back. Two tmux
        // invocations are two client command batches and therefore two
        // redraws, so the operator sees the rail jump out and back. That is
        // the whole of the flicker, and it is not something a rate limit or a
        // synchronized-output wrapper can reach: the rail's own process never
        // drew a bad frame, tmux resized the pane underneath it.
        //
        // Sent as one argv with `;` separators, tmux parses a command SEQUENCE
        // and renders once at the end of it. The window goes from
        // rail+placeholder+person straight to rail+person, with no intermediate
        // geometry ever presented.
        //
        // The layout string is computed BEFORE the kills and does not depend on
        // them — it names the rail, the person panes and the KEPT seats, never
        // a doomed pane — so nothing here is reading state the kills would
        // have changed. Window dimensions do not change when a pane dies.
        // CONVERGE RESIZES RAILS TOO — it reaps and re-lays on its own
        // schedule, and a rail that painted those transits read them as the
        // operator dragging its border. Measured at 14:43:36 on the operator's
        // box, seconds after a click: `converge.window.reaped @5`, and two
        // rails went from 49 columns to 240 — the whole window — then back.
        // That warning used to be a session option written first in this same
        // command list; it is now one call into the brain after the pass, in
        // the same process. See the tombstone on `gesture_stamp_argv`.
        let mut argv: Vec<String> = Vec::new();
        if retire_sleeping_notice {
            for pane in &sleeping {
                argv.extend([
                    "kill-pane".to_owned(),
                    "-t".to_owned(),
                    pane.clone(),
                    ";".to_owned(),
                ]);
            }
        }
        let relayout = current_layout != layout;
        if relayout {
            argv.extend([
                "select-layout".to_owned(),
                "-t".to_owned(),
                window_id.clone(),
                layout.clone(),
            ]);
        } else if argv.last().is_some_and(|last| last == ";") {
            // Nothing to arrange, only placeholders to close. Drop the dangling
            // separator rather than handing tmux an empty trailing command.
            argv.pop();
        }
        if select_when_final {
            if !argv.is_empty() {
                argv.push(";".to_owned());
            }
            argv.extend(["select-window".to_owned(), "-t".to_owned(), window_id.clone()]);
        }
        if argv.is_empty() {
            return Ok(());
        }
        // THE GEOMETRY, EVERY TIME IT CHANGES. The operator asked to be able to
        // SEE the flicker in the record, and until now a converge pass that
        // re-laid a window said nothing about what it did to it — which is why
        // "the sidebar jumps" had to be reproduced live to be believed. One
        // line per real change: how wide the window is, how many panes it has,
        // what the rail was given, and how many placeholders went with it.
        tracing::info!(
            event = "converge.layout.applied",
            window = %window_id,
            width,
            height,
            panes = refs.len(),
            rail_columns = rail.as_ref().map_or(0, |(_, columns)| *columns),
            sleeping_notices_closed = sleeping.len(),
            relaid = relayout,
            atomic = !sleeping.is_empty() && relayout,
            "the window was arranged; any sleeping notice closed in the same tmux command \
             sequence as the final person layout"
        );
        self.tmux(index, "select-layout", argv)?;
        if select_when_final {
            // THE ONE PLACE THIS PASS LEGITIMATELY MOVES THE OPERATOR, said out
            // loud so it is never again on the suspects list by silence. It is
            // the completion of their OWN recorded wake: they clicked a
            // sleeping person, chiefd granted it, and this is the pane arriving
            // where the click asked for it.
            let (selected, matches) = self.selection_context(&window_id);
            tracing::info!(
                event = "converge.focus.selected",
                window = %window_id,
                selected_person = selected,
                selection_matches_window = matches,
                "converge: selected the window the operator's own recorded wake asked for"
            );
            self.pending_focus_selection = None;
        }
        Ok(())
    }

    /// Give every freshly minted company window its required sidebar rail.
    ///
    /// # The gap this closes
    ///
    /// `attach` swept the windows that existed when the operator arrived and set
    /// a session marker; it creates no windows itself. Every company window is
    /// minted HERE, so a department that starts while the operator is attached
    /// used to open a window with no rail at all — the operator's only
    /// navigation, missing, with nothing to explain why.
    ///
    /// A failed split remains non-fatal to person placement, but the next
    /// steady-state survey retries it. There is no operator off switch: closing
    /// a required rail is damage that reconcile repairs.
    ///
    /// TOMBSTONE: `gesture_stamp_argv`, deleted with the rest of the
    /// cross-process sidebar bus. It wrote `@chief_sidebar_gesture` so that the
    /// rails in OTHER windows — separate processes that had performed no gesture
    /// — would decline to paint the resize converge was about to inflict on
    /// them. There is one rail process left in the world and it is the BRAIN,
    /// which shares this one with converge: `resident::TmuxActuator` tells it
    /// `Handle::geometry_moved` after a pass that applied any step, and the rule
    /// is a field (`brain::Brain::gestured_at`) rather than an option two
    /// processes have to agree about.
    fn ensure_rail_in_window(&self, index: usize, window_id: &str) {
        let Some(executable) = crate::sidebar::rail_program() else {
            return;
        };
        let Ok(company_dir) = std::env::current_dir() else {
            return;
        };
        let columns = effective_rail_columns_with(|option| {
            self.tmux(
                index,
                "show-options",
                vec![
                    "show-options".into(),
                    "-q".into(),
                    "-v".into(),
                    "-t".into(),
                    self.desired.session.to_owned(),
                    option.to_owned(),
                ],
            )
            .ok()
            .map(|out| out.stdout)
        });
        // `-b` with `-h` puts the new pane to the LEFT of the target.
        let minted = self.tmux(
            index,
            "split-window",
            vec![
                "split-window".into(),
                "-h".into(),
                "-b".into(),
                "-l".into(),
                columns.to_string(),
                "-t".into(),
                window_id.to_owned(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-c".into(),
                company_dir.display().to_string(),
                executable,
                "sidebar".into(),
            ],
        );
        let Ok(out) = minted else { return };
        let pane_id = out.stdout.trim().to_owned();
        if pane_id.is_empty() {
            return;
        }
        // The tag is what makes this pane a rail rather than a stranger:
        // `observe_rail` finds it by this, and the ownership sweep reads it.
        // Deliberately NOT `tags::PERSON` — a rail is not a person and must
        // never be adopted as one.
        let _ = self.tmux(
            index,
            "set-option",
            vec![
                "set-option".into(),
                "-p".into(),
                "-t".into(),
                pane_id,
                tags::SIDEBAR.into(),
                "1".into(),
            ],
        );
    }

    /// What `window_id` actually holds: every pane in tmux's own order, and
    /// which of them are sleeping furniture closed when a real person arrives.
    ///
    /// ONE `list-panes`, answering both, because they are the same question
    /// asked of the same window in the same instant. The layout step needs the
    /// WINDOW's truth and not the plan's — a layout string that omits a live
    /// pane is rejected outright — so a pane converge deliberately does not
    /// manage still has to be counted, and counting it must not cost a second
    /// round trip that could disagree with the first.
    fn window_census(&mut self, index: usize, window_id: &str) -> Result<WindowCensus, StepError> {
        let listed = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-t".into(),
                window_id.to_owned(),
                "-F".into(),
                format!("#{{pane_id}}\t#{{{}}}", super::trust::tags::ASLEEP),
            ],
        )?;
        let mut census = WindowCensus::default();
        for line in listed.stdout.lines() {
            // The tag half is optional: an untagged pane prints the id and
            // tmux's empty expansion, and older fixtures print the id alone.
            let (pane, asleep) = line.split_once('\t').unwrap_or((line, ""));
            let pane = pane.trim();
            if pane.is_empty() {
                continue;
            }
            census.panes.push(pane.to_owned());
            if !asleep.trim().is_empty() {
                census.sleeping.push(pane.to_owned());
            }
        }
        Ok(census)
    }

    /// The operator's sidebar rail in this window, if one is running: its pane
    /// id and the column count it should be laid out at.
    ///
    /// DISCOVERED, never planned. The rail is created by the operator's own
    /// attached processes and this loop's only business with it is to reserve
    /// its column, so there is no rail step, no rail binding and nothing for a
    /// converge pass to mint or reap. A window with no rail — every window of
    /// a company nobody is attached to, and every window of the headless
    /// actuator session — lays out exactly as it did before.
    ///
    /// The width comes from the human-owned expanded preference plus the
    /// independent collapse state. Runtime pane geometry never writes either
    /// option. An unreadable or nonsense option is not a
    /// failure: the rail falls back to its collapsed width, which always fits.
    fn observe_rail(
        &mut self,
        index: usize,
        window_id: &str,
    ) -> Result<Option<(String, i64)>, StepError> {
        let listed = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-t".into(),
                window_id.to_owned(),
                "-F".into(),
                format!("#{{pane_id}}\t#{{{}}}", super::trust::tags::SIDEBAR),
            ],
        )?;
        let Some(pane_id) = listed
            .stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .find(|(_, marker)| marker.trim() == "1")
            .map(|(pane_id, _)| pane_id.trim().to_owned())
        else {
            return Ok(None);
        };
        let expanded = self
            .tmux(
                index,
                "display-message",
                vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    window_id.to_owned(),
                    "-F".into(),
                    format!("#{{{}}}", super::trust::sidebar_options::COLUMNS),
                ],
            )?
            .stdout
            .trim()
            .parse::<i64>()
            // THE DEFAULT IS THE OPEN WIDTH, NEVER THE COLLAPSED ONE. This read
            // answers "how wide is the operator's sidebar", and an option that
            // is unset — a session whose rail has not recorded a width yet — is
            // a question we cannot answer, not an answer of "four columns".
            // Defaulting to the collapsed width laid every pane in that window
            // beside a 4-column rail reading `Depa` / `Peop`, and because the
            // rail declines to record a width it cannot be read at, nothing
            // ever wrote a better number back: the sidebar shrank on load and
            // stayed shrunk. `effects::rail_columns` has always defaulted to the
            // open width; these two halves must agree.
            .map_or(
                crate::sidebar::brain::RAIL_DEFAULT_COLUMNS,
                crate::sidebar::brain::canonical_columns,
            );
        let collapsed = self
            .tmux(
                index,
                "display-message",
                vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    window_id.to_owned(),
                    "-F".into(),
                    format!("#{{{}}}", super::trust::sidebar_options::COLLAPSED),
                ],
            )?
            .stdout;
        let columns =
            if collapsed.trim() == "1" { crate::layout::RAIL_COLLAPSED_COLUMNS } else { expanded };
        Ok(Some((pane_id, columns)))
    }

    // --- shared helpers ----------------------------------------------------

    /// The apply-time precondition for CREATION steps, closing the same
    /// observe→apply TOCTOU gap the destructive steps already re-verify (see
    /// the module doc): between the observation the plan was computed from and
    /// this step's execution, a concurrent actuator — the launcher's attended,
    /// synchronous `start-person` — may have minted exactly the pane this step
    /// was about to create (the measured dual-materialization: one pane from
    /// the daemon's converge pass, one from the launcher, both tagged with the
    /// same person id, the `Ambiguous duplicate organization person/window`
    /// incident). Re-read the live topology for the target person; a LIVE pane
    /// already tagged with THIS organization and person is adopted into the
    /// bindings (its window included) instead of minting a second one. An
    /// adopted pane never enters `created_panes` — we did not mint it, so it
    /// is never a rollback candidate. A dead pane does not count (its respawn
    /// is the reconcile's normal recovery shape). Returns whether an existing
    /// pane was adopted and the caller must skip its spawn.
    fn adopt_existing_pane(
        &mut self,
        index: usize,
        logical: &str,
        person_id: &str,
    ) -> Result<bool, StepError> {
        let out = self.tmux(
            index,
            "list-panes",
            vec![
                "list-panes".into(),
                "-s".into(),
                "-t".into(),
                self.desired.session.clone(),
                "-F".into(),
                "#{pane_id}\t#{window_id}\t#{pane_dead}\t#{@organization_id}\t#{@organization_person_id}".into(),
            ],
        )?;
        for line in out.stdout.lines() {
            let mut fields = line.split('\t').map(str::trim);
            let (Some(pane), Some(window), Some(dead), Some(org), Some(person)) =
                (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if org != self.desired.organization || person != person_id || dead != "0" {
                continue;
            }
            tracing::info!(
                pane = %pane,
                person = %person_id,
                "converge: adopting an already-materialized pane instead of minting a duplicate (apply-time creation precondition)"
            );
            self.bindings.panes.insert(person_id.to_owned(), pane.to_owned());
            self.bindings.windows.entry(logical.to_owned()).or_insert_with(|| window.to_owned());
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolve a plan window reference to a live tmux window id.
    fn resolve_window(&self, index: usize, window: &plan::WindowRef) -> Result<String, StepError> {
        match window {
            plan::WindowRef::Observed(id) => Ok(id.clone()),
            plan::WindowRef::Created(sym) => {
                if let Some(binding) = self.bindings.windows.get(&sym.0) {
                    return Ok(binding.clone());
                }
                // NOT CREATED BECAUSE ITS FIRST PERSON WAS REFUSED. The step
                // that would have minted this window was skipped by name, so
                // this one is skipped by the same name rather than reported as
                // a plan that references a window before creating it.
                if let Some((person, reason)) = self.refused_windows.get(&sym.0) {
                    return Err(StepError::LaunchRefused {
                        index,
                        step: "window",
                        person: person.clone(),
                        reason: reason.clone(),
                    });
                }
                Err(StepError::Internal {
                    index,
                    detail: format!("window '{}' was referenced before it was created", sym.0),
                })
            }
        }
    }

    /// The logical id of a plan window reference (for pane tagging).
    fn logical_for_window(
        &self,
        index: usize,
        window: &plan::WindowRef,
    ) -> Result<String, StepError> {
        match window {
            plan::WindowRef::Created(sym) => Ok(sym.0.clone()),
            // An observed window ref only ever names a window M1 saw in the
            // observed topology, so its logical id is known directly from
            // `observed.windows` (a window may legitimately have no panes yet).
            plan::WindowRef::Observed(id) => {
                self.observed_window_logical.get(id).cloned().ok_or_else(|| StepError::Internal {
                    index,
                    detail: format!("observed window '{id}' has no known logical id"),
                })
            }
        }
    }

    /// Resolve a plan pane reference to a live tmux pane id.
    fn resolve_pane(&self, index: usize, pane: &plan::PaneRef) -> Result<String, StepError> {
        match pane {
            plan::PaneRef::Observed(id) => Ok(id.0.clone()),
            plan::PaneRef::Created(person) => {
                self.bindings.panes.get(person).cloned().ok_or_else(|| StepError::Internal {
                    index,
                    detail: format!("pane for '{person}' was referenced before it was created"),
                })
            }
        }
    }

    /// The person a plan expected an observed pane to belong to (from the
    /// observe-time topology), for a precondition re-verify.
    fn expected_person(
        &self,
        index: usize,
        step: &'static str,
        tmux_id: &str,
    ) -> Result<String, StepError> {
        self.observed_pane_by_tmux.get(tmux_id).map(|p| p.person_id.clone()).ok_or_else(|| {
            StepError::Internal {
                index,
                detail: format!("{step} targets pane '{tmux_id}' absent from the observation"),
            }
        })
    }

    /// Re-read a pane's live ownership and require it still ours (and still the
    /// expected person, when given). The TOCTOU guard.
    fn reverify_owned(
        &self,
        index: usize,
        step: &'static str,
        tmux_id: &str,
        expected_person: Option<&str>,
    ) -> Result<(), StepError> {
        let identity = self.pane_identity(index, step, tmux_id)?;
        if identity.organization != self.desired.organization {
            return Err(StepError::Precondition {
                index,
                step,
                detail: format!(
                    "pane {tmux_id} ownership is '{}', expected '{}'",
                    identity.organization, self.desired.organization
                ),
            });
        }
        if let Some(person) = expected_person {
            if identity.person_id != person {
                return Err(StepError::Precondition {
                    index,
                    step,
                    detail: format!(
                        "pane {tmux_id} is now person '{}', plan expected '{person}'",
                        identity.person_id
                    ),
                });
            }
        }
        Ok(())
    }

    fn pane_identity(
        &self,
        index: usize,
        step: &'static str,
        tmux_id: &str,
    ) -> Result<crate::actuate::host::PaneIdentity, StepError> {
        self.executor
            .pane_identity(self.socket, &HostPaneId(tmux_id.to_owned()))
            .map_err(|source| StepError::Host { index, step, source })
    }

    /// The pane command for one spawn.
    ///
    /// It used to take a `cause` -- this client's reading of whether the pane
    /// had drifted or was simply not there -- purely as a key for selecting
    /// which sentence chiefd had published to inject. No sentence is injected
    /// any more, so the reading has no consumer and the parameter is gone
    /// rather than kept as an unused argument every call site must still
    /// answer for.
    fn pane_command(
        &self,
        index: usize,
        step: &'static str,
        spec: &plan::SpawnSpec,
    ) -> Result<PaneCommand, StepError> {
        // THE BACKOFF, CHECKED BEFORE ANY TMUX WORK. A person whose last boots
        // died is not spawned again until their delay elapses, so a broken
        // workspace does not respawn sixty times a minute and bury every other
        // line on the operator's screen. This is a WAIT and never a give-up:
        // `crash_loop::retry_delay` is bounded at ten seconds and the next pass
        // past it spawns them again.
        //
        // BEFORE the launch-spec lookup, because a person can be both refused
        // and crash-looping and the refusal is the live cause — but a person
        // who is only crash-looping has a perfectly good spec and must still
        // not be spawned this pass.
        if self.deferred_people.contains(&spec.person_id) {
            return Err(StepError::RetryDeferred { index, step, person: spec.person_id.clone() });
        }
        let launch = self.launch.get(&spec.person_id).ok_or_else(|| {
            // A MISSING LAUNCH SPEC IS TWO DIFFERENT EVENTS, and only one of
            // them is a broken plan.
            //
            // The person WAS iterated by the catalog builder and has no spec:
            // chiefd's gate declined them. That is expected, well understood,
            // re-derived from the disk on every pass, and named in full by the
            // daemon that owns that disk. It costs this person their step and
            // NOTHING ELSE -- see `apply_plan_with_launch_roster`. It used to be
            // `Internal`, which the step loop fail-stops on, so one refused
            // person abandoned every healthy person queued behind them: `the
            // pass FAILED after X of Y step(s)`, and the people at Y never
            // attempted, on every pass, for as long as the refusal lasted.
            //
            // The person was NOT iterated at all (absent from the roster the
            // builder walked): the plan asked to spawn somebody the catalog has
            // never heard of, which is the two sides disagreeing about who
            // exists. That is a genuine internal inconsistency and it still
            // fail-stops. The roster size is included because "N iterated" is
            // what makes it actionable rather than merely accurate (#52).
            //
            // With NO diagnostics at all this cannot be told apart, so it stays
            // `Internal`: only the catalog can say a gate refused somebody, and
            // guessing that it did would be inventing a reason.
            let Some(roster) = self.iterated_launch_roster else {
                return StepError::Internal {
                    index,
                    detail: format!("no launch spec for person '{}'", spec.person_id),
                };
            };
            if !roster.contains(&spec.person_id) {
                return StepError::Internal {
                    index,
                    detail: format!(
                        "person '{}' is not in the launch roster ({} people iterated)",
                        spec.person_id,
                        roster.len(),
                    ),
                };
            }
            StepError::LaunchRefused {
                index,
                step,
                person: spec.person_id.clone(),
                // chiefd's own sentence when it published one. A person absent
                // from the catalog's `people` with no entry in `refusals` is
                // still a refusal; it simply carries the generic reason.
                reason: self
                    .refusal_reasons
                    .and_then(|reasons| reasons.get(&spec.person_id))
                    .cloned()
                    .unwrap_or_else(|| {
                        "chiefd's launch gate published no launch spec and no reason".to_owned()
                    }),
            }
        })?;
        // WHAT THIS PANE IS BEING TOLD TO DO, derived here because this is
        // where all three facts are: how many people the company holds (the
        // desired topology's own roster, rebuilt every pass), whether this
        // person has a transcript, and whether chiefd published mail waiting
        // for them. A company seconds old is one person with none of it, and
        // telling that CEO to "continue the next real piece of work" is what
        // made a just-created company start hiring at the operator while they
        // were still reading the first screen; a woken person with an empty
        // mailbox is the same sentence one situation wider, and it built an
        // Engineering department out of the launcher's own source tree.
        // Nothing durable records this and nothing should: see
        // `spawn_cmd::BootStanding`.
        let standing = crate::actuate::spawn_cmd::BootStanding::from_company(
            self.desired.known_person_ids.len(),
            launch.session.as_deref(),
            launch.pending_mail,
        );
        Ok(launch_command(
            spec,
            launch,
            &crate::actuate::spawn_cmd::PanePlacement {
                socket: &self.socket.0,
                session: &self.desired.session,
            },
            standing,
            // The ground this pane will be drawn on, read HERE, on the pass
            // that draws it: `/run/tribes-theme` is the operator's current
            // choice and it changes while this client runs, so the answer is
            // taken fresh per spawn rather than once at start. It is the same
            // reader the rail draws itself from (`crate::appearance`), which is
            // what makes a pane and the rail beside it agree. `None` — no
            // bridge, or an unreadable one — is carried as `None`: this client
            // then states nothing about the screen.
            crate::appearance::read_declared(),
        ))
    }

    fn tag_pane(
        &self,
        index: usize,
        pane: &str,
        logical: &str,
        person_id: &str,
        launch_hash: &str,
    ) -> Result<(), StepError> {
        // #18 P2 / task #23: each pair below is a SEPARATE `set-option`
        // round-trip — tagging a pane is NOT atomic. A process killed between
        // pairs used to leave a partially-tagged live pane that
        // `assert_unambiguous` (reconcile_plan.rs) quarantined as a stray
        // (#410) forever, with a SECOND pane minted next pass alongside it —
        // duplication with no second actuator involved, and the leaked pane
        // going on to break the next `ApplyLayout` step's pane count. The
        // MINTING marker the caller sets before this runs is what makes it
        // recoverable now: `observe()`'s reap sweep destroys a pane still carrying
        // it before the next pass ever observes it, so the planner mints a
        // clean single replacement instead of quarantining a permanent
        // duplicate. Named pause points (crate::pause, TESTING.md §4.3) let a
        // crash-injection test park here deterministically instead of racing
        // a timer. See `tests/interpret_crash.rs`.
        let pause_names = [
            "interpret:tag_pane:after_organization",
            "interpret:tag_pane:after_window",
            "interpret:tag_pane:after_person",
        ];
        let pairs = [
            (tags::ORGANIZATION, self.desired.organization.as_str()),
            (tags::WINDOW, logical),
            (tags::PERSON, person_id),
            (tags::LAUNCH_HASH, launch_hash),
        ];
        for (i, (tag, value)) in pairs.into_iter().enumerate() {
            self.tmux(
                index,
                "set-option",
                vec![
                    "set-option".into(),
                    "-p".into(),
                    "-t".into(),
                    pane.to_owned(),
                    tag.to_owned(),
                    value.to_owned(),
                ],
            )?;
            if let Some(name) = pause_names.get(i) {
                crate::pause::at(name);
            }
        }
        Ok(())
    }

    /// #18 P2 / task #23: mark a freshly minted window as mid-mint, BEFORE its
    /// first identity tag. A crash before this call leaves an untagged window
    /// invisible to `assert_unambiguous` (pre-existing, unrelated behaviour,
    /// unchanged); a crash at or after this call leaves the marker set, and
    /// `observe()`'s reap sweep destroys the window on the next pass instead of the
    /// planner permanently refusing the whole company
    /// (`PlanErr::WindowNotFullyTagged`, task #23).
    fn mark_minting_window(&self, index: usize, window_id: &str) -> Result<(), StepError> {
        self.tmux(
            index,
            "set-option",
            vec![
                "set-option".into(),
                "-w".into(),
                "-t".into(),
                window_id.to_owned(),
                tags::MINTING.into(),
                "1".into(),
            ],
        )?;
        Ok(())
    }

    /// The other half of `mark_minting_window`: clear the marker once every
    /// identity tag has landed, so a LATER crash (after this call) reads as
    /// "fully tagged", not "still minting".
    fn clear_minting_window(&self, index: usize, window_id: &str) -> Result<(), StepError> {
        self.tmux(
            index,
            "set-option",
            vec![
                "set-option".into(),
                "-w".into(),
                "-u".into(),
                "-t".into(),
                window_id.to_owned(),
                tags::MINTING.into(),
            ],
        )?;
        Ok(())
    }

    /// Pane half of `mark_minting_window` — see its doc.
    fn mark_minting_pane(&self, index: usize, pane_id: &str) -> Result<(), StepError> {
        self.tmux(
            index,
            "set-option",
            vec![
                "set-option".into(),
                "-p".into(),
                "-t".into(),
                pane_id.to_owned(),
                tags::MINTING.into(),
                "1".into(),
            ],
        )?;
        Ok(())
    }

    /// Pane half of `clear_minting_window` — see its doc.
    fn clear_minting_pane(&self, index: usize, pane_id: &str) -> Result<(), StepError> {
        self.tmux(
            index,
            "set-option",
            vec![
                "set-option".into(),
                "-p".into(),
                "-u".into(),
                "-t".into(),
                pane_id.to_owned(),
                tags::MINTING.into(),
            ],
        )?;
        Ok(())
    }

    fn tag_window(&self, index: usize, window_id: &str, logical: &str) -> Result<(), StepError> {
        // #18 P2 / task #23: each pair below is a separate `set-option`
        // round-trip, so tagging a window is NOT atomic — a crash between
        // them used to leave a permanently fatal `WindowNotFullyTagged` with
        // no self-heal (measured: identical failure across 5 uncrashed
        // passes). The MINTING marker set by the caller before this runs is
        // what makes that recoverable now: `observe()`'s reap sweep destroys a
        // window still carrying it instead of the planner refusing the whole
        // company forever. See `tests/interpret_crash.rs`.
        let pairs =
            [(tags::ORGANIZATION, self.desired.organization.as_str()), (tags::WINDOW, logical)];
        for (i, (tag, value)) in pairs.into_iter().enumerate() {
            self.tmux(
                index,
                "set-option",
                vec![
                    "set-option".into(),
                    "-w".into(),
                    "-t".into(),
                    window_id.to_owned(),
                    tag.to_owned(),
                    value.to_owned(),
                ],
            )?;
            if i == 0 {
                crate::pause::at("interpret:tag_window:after_organization");
            }
        }
        Ok(())
    }

    fn record_minted_pane(&mut self, pane_id: &str, pid: Pid, session: &str) {
        self.created_panes.push(CreatedPane {
            pane_id: pane_id.to_owned(),
            pid,
            session: session.to_owned(),
            person_id: None,
            launch_hash: None,
        });
    }

    fn mark_created_pane_owned(
        &mut self,
        index: usize,
        pane_id: &str,
        person_id: &str,
        launch_hash: &str,
    ) -> Result<(), StepError> {
        let created = self
            .created_panes
            .iter_mut()
            .rev()
            .find(|created| created.pane_id == pane_id)
            .ok_or_else(|| StepError::Internal {
                index,
                detail: format!("minted pane '{pane_id}' was not tracked before ownership tagging"),
            })?;
        created.person_id = Some(person_id.to_owned());
        created.launch_hash = Some(launch_hash.to_owned());
        Ok(())
    }

    /// Best-effort compensation for panes this apply attempt created. Cleanup
    /// cannot change the original failure report and must never touch an
    /// observed/pre-existing pane. A partially-tagged pane is fenced by its
    /// exact spawn pid plus session; a fully-tagged pane additionally requires
    /// its organization/person/launch-hash identity. A concurrent reconcile that
    /// respawns, moves, takes over, or repurposes it makes cleanup refuse.
    fn reap_created_panes(&self) {
        for created in self.created_panes.iter().rev() {
            let safe_to_kill = match (&created.person_id, &created.launch_hash) {
                (Some(person_id), Some(launch_hash)) => {
                    self.tagged_pane_still_matches(created, person_id, launch_hash)
                }
                _ => self.minted_pane_still_matches(created),
            };
            if !safe_to_kill {
                tracing::warn!(pane = %created.pane_id, "converge: minted pane changed before rollback; leaving it untouched");
                continue;
            }
            match self.executor.tmux(
                self.socket,
                TmuxCmd { argv: vec!["kill-pane".into(), "-t".into(), created.pane_id.clone()] },
            ) {
                Ok(out) if out.status == 0 => tracing::info!(
                    pane = %created.pane_id,
                    person = ?created.person_id,
                    "converge: reaped pane created by failed apply attempt"
                ),
                Ok(out) => tracing::warn!(
                    pane = %created.pane_id,
                    person = ?created.person_id,
                    detail = %out.stderr.trim(),
                    "converge: failed to reap newly created pane after apply failure"
                ),
                Err(error) => tracing::warn!(
                    pane = %created.pane_id,
                    person = ?created.person_id,
                    error = %error,
                    "converge: host error reaping newly created pane after apply failure"
                ),
            }
        }
    }

    fn minted_pane_still_matches(&self, created: &CreatedPane) -> bool {
        if created.session != self.desired.session {
            return false;
        }
        let out = match self.executor.tmux(
            self.socket,
            TmuxCmd {
                argv: vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    created.pane_id.clone(),
                    "-F".into(),
                    "#{pane_pid}\t#{session_name}".into(),
                ],
            },
        ) {
            Ok(out) if out.status == 0 => out,
            Ok(out) => {
                tracing::warn!(pane = %created.pane_id, detail = %out.stderr.trim(), "converge: could not inspect partially tagged pane for rollback");
                return false;
            }
            Err(error) => {
                tracing::warn!(pane = %created.pane_id, error = %error, "converge: host error inspecting partially tagged pane for rollback");
                return false;
            }
        };
        let mut fields = out.stdout.trim().split('\t');
        let pid = fields.next().and_then(|value| value.parse::<i32>().ok());
        let session = fields.next();
        fields.next().is_none()
            && pid == Some(created.pid.0)
            && session == Some(created.session.as_str())
    }

    fn tagged_pane_still_matches(
        &self,
        created: &CreatedPane,
        person_id: &str,
        launch_hash: &str,
    ) -> bool {
        let out = match self.executor.tmux(
            self.socket,
            TmuxCmd { argv: vec!["display-message".into(), "-p".into(), "-t".into(), created.pane_id.clone(), "-F".into(), "#{pane_pid}\t#{session_name}\t#{@organization_id}\t#{@organization_person_id}\t#{@organization_launch_hash}\t#{@organization_window_id}".into()] },
        ) {
            Ok(out) if out.status == 0 => out,
            _ => return false,
        };
        let fields: Vec<&str> = out.stdout.trim().split('\t').collect();
        fields.len() == 6
            && fields[0].parse::<i32>().ok() == Some(created.pid.0)
            && fields[1] == created.session
            && fields[1] == self.desired.session
            && fields[2] == self.desired.organization
            && fields[3] == person_id
            && fields[4] == launch_hash
            && fields[5] == self.window_of_person.get(person_id).map_or("", String::as_str)
    }

    /// Run a tmux command that must succeed. Non-zero exit and host errors both
    /// become step failures (fail-stop).
    fn tmux(
        &self,
        index: usize,
        verb: &str,
        argv: Vec<String>,
    ) -> Result<crate::actuate::host::TmuxOut, StepError> {
        let probe = MutationContext::for_command(
            self.socket,
            &self.desired.organization,
            &self.desired.session,
            verb,
            &argv,
        );
        if let Some(probe) = &probe {
            probe.attempt();
        }
        let out = match self.executor.tmux(self.socket, TmuxCmd { argv }) {
            Ok(out) => out,
            Err(source) => {
                if let Some(probe) = &probe {
                    probe.result(None, "host-error");
                }
                return Err(StepError::Host { index, step: "tmux", source });
            }
        };
        if let Some(probe) = &probe {
            probe.result(Some(out.status), if out.status == 0 { "ok" } else { "nonzero" });
        }
        if out.status != 0 {
            return Err(StepError::Tmux { index, verb: verb.to_owned(), detail: refusal(&out) });
        }
        Ok(out)
    }
}

/// WHAT TMUX SAID, and never nothing.
///
/// tmux's own stderr is the evidence about a refused mint or a refused split,
/// so it is taken verbatim. tmux writes some refusals to stdout instead, and a
/// few it reports with an exit code alone; both used to reduce to an empty
/// sentence, which reached the operator as a failure with no cause. When there
/// are no words, the exit status is the fact that is available and it is
/// stated.
fn refusal(out: &crate::actuate::host::TmuxOut) -> String {
    let stderr = out.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = out.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_owned();
    }
    format!("tmux exited with status {} and wrote no message", out.status)
}

/// Truncate a window name to tmux's 40-character managed-name budget (matching
/// [`crate::actuate::exec`]'s spawn).
/// tmux `-n` argument for a window.
///
/// Delegates to [`crate::actuate::safe_window_name`], the shared bounded label
/// canonicalizer. A historical "Leo Capital Inc." incident motivated it, but
/// the current real-tmux control accepts that raw name; the normalizer remains
/// a cross-actuator contract. Its final trim also prevents a cut from leaving
/// a dangling separator.
fn window_arg(name: &str) -> String {
    crate::actuate::safe_window_name(name)
}

/// Module-private again. It was `pub` for one consumer: a workspace test crate
/// that drove chiefd's OWN composed resume copy through this argv construction
/// and then through `control::quote_argv`, because that seam belonged to
/// neither crate. chiefd composes nothing for a pane now, so the sentence, the
/// crate and the reason for the wider visibility all went together.
fn push_launch_flags(argv: &mut Vec<String>, command: &PaneCommand) {
    argv.push("-c".to_owned());
    argv.push(command.cwd.display().to_string());
    for (name, value) in &command.env {
        argv.push("-e".to_owned());
        argv.push(format!("{name}={value}"));
    }
    argv.push("--".to_owned());
    argv.extend(command.argv.iter().cloned());
}

/// Put the server-wide keyboard/input contract ahead of the first pane spawn
/// in the same tmux command queue. `start-server` and every following command
/// are delivered in one client message, so the server cannot execute
/// `new-session` until these idempotent options and bindings are complete.
///
/// # BEFORE YOU ADD A LINE HERE: an unrecognised option ABORTS THE QUEUE
///
/// That single client message is the trap, and it is not obvious from reading
/// the calls. If any `set-option` here names an option the running tmux does
/// not know, tmux reports `invalid option: <name>` and **abandons the rest of
/// the list** — including the `new-session` at the end. The result is not a
/// degraded terminal. It is a company that does not start.
///
/// MEASURED, rather than reasoned:
///
/// ```text
/// $ tmux -L x start-server \; set-option -s no-such-option on \; new-session -d -s p
/// invalid option: no-such-option
/// $ tmux -L x list-sessions
/// no server running
/// ```
///
/// So an option that is not available on EVERY tmux this product runs against
/// must be guarded, and the idiom is already here: the `terminal-features`
/// line below wraps its `set-option` in an `if-shell`, which contains the
/// failure. Same probe, guarded:
///
/// ```text
/// $ tmux -L y start-server \; if-shell "false" "" "set-option -s no-such-option on" \; new-session -d -s p
/// invalid option: no-such-option
/// $ tmux -L y list-sessions
/// p: 1 windows          <- the session was created
/// ```
///
/// AND THE HONEST HALF, because a guard that hides a no-op is its own defect:
/// `if-shell` makes an unavailable option SAFE, not APPLIED. On a tmux that
/// lacks it the option is silently never set, so a guarded line must say in
/// its own comment which versions actually get it — otherwise it reads as
/// though it always applies, which is a check that passes without looking
/// wearing different clothes.
///
/// This was found by measuring before adding a fourth option, not by a test:
/// nothing in this repository exercises tmux's parsing of a command list we
/// assemble, so the first symptom would have been a company that would not
/// boot.
fn push_server_input_configuration(argv: &mut Vec<String>) {
    push_tmux_command(argv, ["set-option", "-s", "extended-keys", "on"]);
    // THE OPERATOR TERMINAL, from the one definition every bootstrap reads.
    // `mouse on` and the status bar off are one operator surface, not two
    // independent options, and this is not the only bootstrap that creates a
    // session somebody sits in front of -- see `actuate::OPERATOR_TERMINAL_OPTIONS`
    // for why they moved out of here and what the cost of a second copy was.
    //
    // Kept here because it is the part of the reasoning this call site owns:
    // the server must report the mouse for the rail to be a surface at all,
    // and the `root` key table then decides what reaches the rail's own
    // program. `MouseDown1Pane` forwards unconditionally (`send-keys -M`),
    // while the wheel, the drag and the double/triple click forward only when
    // `#{||:#{pane_in_mode},#{mouse_any_flag}}` holds -- measured on tmux
    // 3.3a, that flag reads 1 once the pane has requested `?1000h`, `?1002h`
    // or `?1003h`, and 0 with none of them. A rail that never asked for mouse
    // reporting would get clicks and silently lose the wheel to copy-mode,
    // which is why the rail asks for the whole set.
    for [scope, option, value] in crate::actuate::OPERATOR_TERMINAL_OPTIONS {
        push_tmux_command(argv, ["set-option", scope, option, value]);
    }
    // A MANUAL zoom toggle, with no prefix. It is NO LONGER the way back from a
    // rail click: clicking a person MOVES them into a window of their own
    // beside a rail (`sidebar::effects::show_person`) rather than zooming,
    // precisely because zoom is a WINDOW state that hides the rail — and the
    // operator ruled that the rail must never disappear. Nothing the rail does
    // now needs undoing, and a REPAIR is all this is; it answers a different
    // question:
    // the operator wants one pane and NOTHING else, rail included, for a
    // moment. tmux's own `prefix z` already does that and is invisible to
    // anybody who does not know tmux, which is most of the people this product
    // is for.
    //
    // `C-M-z` and not something shorter, because the key has to survive being
    // pressed INSIDE the full-screen program the zoom exists to show: `C-z`
    // is job control, `M-z` is a plain editing key in too many programs, and a
    // bare function key is somebody's shortcut. `-n` is the root table, so no
    // prefix is needed.
    //
    // Written `C-M-z`, READ BACK as `M-C-z`. tmux normalises modifiers into its
    // own canonical order, so `tmux list-keys -T root | grep C-M-z` finds
    // nothing and looks exactly like a binding that did not take. Measured on a
    // live server by `sidebar::tests`, which asserts the read-back spelling for
    // this reason.
    push_tmux_command(argv, ["bind-key", "-n", "C-M-z", "resize-pane", "-Z"]);
    push_tmux_command(argv, ["set-option", "-s", "escape-time", "10"]);
    push_tmux_command(argv, ["set-option", "-s", "set-clipboard", "on"]);
    // The browser terminal is xterm-256color and reports true color through
    // COLORTERM, but tmux does not infer RGB from that environment variable.
    // Give future browser clients exact RGB authority before the first managed
    // pane can render. The shared helper also owns reused operator entry paths.
    super::terminal_features::push_browser_rgb_feature(argv);
    // Preserve unrelated terminal capabilities and append extkeys only when
    // it is absent. The conditional itself is idempotent across every company
    // session sharing the server.
    push_tmux_command(
        argv,
        [
            "if-shell",
            "tmux show-options -s -v terminal-features | grep -Fq 'xterm*:extkeys'",
            "",
            "set-option -as terminal-features ,xterm*:extkeys",
        ],
    );
    // TMUX'S OWN REPAINT ARRIVES IN ONE PIECE, which is what stops a window
    // switch showing half of one person above half of another.
    //
    // Selecting a window makes tmux repaint the whole client, and tmux writes
    // that repaint cell by cell, so a terminal is free to show every
    // intermediate state. The operator reported the result three times: *"it
    // just always shows up like half size and then grows full size ... it gives
    // you this flicker and the whole thing scrolls"*. #1200 removed the pane
    // MOVE that made the worst of it — a pane that changes window changes width
    // and Pi repaints its scrollback — but a torn repaint survives a move that
    // never happens, because the tearing is in the delivery and not the layout.
    //
    // MEASURED, 2026-08-22: clicking between two running agents on a real X
    // desktop with a real mouse, captured losslessly at 30fps, counting frames
    // that differ from BOTH neighbours — 1 torn frame in 10 switches without
    // this, 0 in 10 with it, and with it every switch was an identical
    // single-frame change, which is what "arrives in its final layout" looks
    // like to a frame counter.
    //
    // `sync` is the feature that makes tmux wrap each repaint in the DEC
    // private-mode pair `?2026h`/`?2026l`. tmux 3.3a infers it for nothing:
    // `#{client_termfeatures}` against a real xterm 379 listed fifteen features
    // and not this one, so it has to be declared.
    //
    // Declared for `*`, on the ruling this product already made for the rail's
    // own frames (`sidebar::client::blit`): "a terminal that does not support
    // it ignores both, so this can only help and needs no capability probe". A
    // pane surface that emits the pair unconditionally cannot coherently
    // withhold it from the client repaint that draws that same pane.
    //
    // Guarded and appended exactly like the extkeys rule above, so a server
    // shared by several companies collects it once.
    push_tmux_command(
        argv,
        [
            "if-shell",
            "tmux show-options -s -v terminal-features | grep -Fq '*:sync'",
            "",
            "set-option -as terminal-features ,*:sync",
        ],
    );
    push_tmux_command(
        argv,
        [
            "bind-key",
            "-T",
            "copy-mode",
            "MouseDragEnd1Pane",
            "send-keys",
            "-X",
            "copy-selection-and-cancel",
        ],
    );
    push_tmux_command(
        argv,
        [
            "bind-key",
            "-T",
            "copy-mode-vi",
            "MouseDragEnd1Pane",
            "send-keys",
            "-X",
            "copy-selection-and-cancel",
        ],
    );
    // Keep tmux's default MouseDrag1Border -> resize-pane -M motion. The
    // release event is the one explicit authority that records an expanded
    // rail width. `-t =` evaluates the pane under the mouse, so a body border
    // cannot write the preference. A collapsed four-column rail also fails the
    // readable-width guard and cannot overwrite the expanded preference.
    push_tmux_command(
        argv,
        [
            "bind-key",
            "-T",
            "root",
            "MouseDragEnd1Border",
            "if-shell",
            "-F",
            "-t",
            "=",
            &format!(
                "#{{&&:#{{==:#{{{}}},1}},#{{e|>=:#{{pane_width}},{}}}}}",
                tags::SIDEBAR,
                crate::sidebar::brain::RAIL_MIN_READABLE_COLUMNS
            ),
            &format!(
                "run-shell -b -t = '#{{{}}} #{{pane_width}} >/dev/null 2>&1 || :'",
                super::trust::viewport_options::WIDTH_COMMAND,
            ),
        ],
    );
}

/// Append one command to a tmux command list. The raw `;` word is interpreted
/// by tmux, not a shell.
fn push_tmux_command<I, S>(argv: &mut Vec<String>, command: I)
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    argv.push(";".to_owned());
    argv.extend(command.into_iter().map(Into::into));
}

/// §2.0(2) ONE SHOT: append one `set-option` to a tmux command LIST, so an
/// identity tag rides the SAME client message as the creating command (the
/// `;` separator between raw argv words — we exec tmux directly, no shell)
/// instead of a follow-up round-trip a crash could interrupt. tmux executes
/// the list in order, so tags must be appended AFTER the creating command,
/// and each is idempotent on its own.
fn push_set_option(argv: &mut Vec<String>, scope: &[&str], target: &str, tag: &str, value: &str) {
    argv.push(";".to_owned());
    argv.push("set-option".to_owned());
    argv.extend(scope.iter().map(|flag| (*flag).to_owned()));
    argv.push("-t".to_owned());
    argv.push(target.to_owned());
    argv.push(tag.to_owned());
    argv.push(value.to_owned());
}

/// Parse a `#{pane_id}\t#{window_id}\t#{pane_pid}\t#{session_name}` reply.
fn parse_minted_pane_and_window(
    index: usize,
    verb: &str,
    stdout: &str,
) -> Result<(String, String, Pid, String), StepError> {
    let line = stdout.lines().next().unwrap_or_default();
    let mut fields = line.split('\t').map(str::trim);
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(pane), Some(window), Some(pid), Some(session)) => match pid.parse::<i32>() {
            Ok(pid) if !pane.is_empty() && !window.is_empty() && !session.is_empty() && pid > 0 => {
                Ok((pane.to_owned(), window.to_owned(), Pid(pid), session.to_owned()))
            }
            _ => Err(StepError::Tmux {
                index,
                verb: verb.to_owned(),
                detail: format!("expected 'pane\\twindow\\tpid\\tsession', got {stdout:?}"),
            }),
        },
        _ => Err(StepError::Tmux {
            index,
            verb: verb.to_owned(),
            detail: format!("expected 'pane\\twindow\\tpid\\tsession', got {stdout:?}"),
        }),
    }
}

/// Parse a `#{pane_id}\t#{pane_pid}\t#{session_name}` reply.
fn parse_minted_pane(
    index: usize,
    verb: &str,
    stdout: &str,
) -> Result<(String, Pid, String), StepError> {
    let line = stdout.lines().next().unwrap_or_default();
    let mut fields = line.split('\t').map(str::trim);
    match (fields.next(), fields.next(), fields.next()) {
        (Some(pane), Some(pid), Some(session)) => match pid.parse::<i32>() {
            Ok(pid) if !pane.is_empty() && !session.is_empty() && pid > 0 => {
                Ok((pane.to_owned(), Pid(pid), session.to_owned()))
            }
            _ => Err(StepError::Tmux {
                index,
                verb: verb.to_owned(),
                detail: format!("expected 'pane\\tpid\\tsession', got {stdout:?}"),
            }),
        },
        _ => Err(StepError::Tmux {
            index,
            verb: verb.to_owned(),
            detail: format!("expected 'pane\\tpid\\tsession', got {stdout:?}"),
        }),
    }
}

/// Parse a `#{window_width}\t#{window_height}` reply into positive integers.
fn parse_dimensions(index: usize, stdout: &str) -> Result<(i64, i64), StepError> {
    let line = stdout.lines().next().unwrap_or_default();
    let mut fields = line.split('\t').map(str::trim);
    let width = fields.next().and_then(|v| v.parse::<i64>().ok());
    let height = fields.next().and_then(|v| v.parse::<i64>().ok());
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Ok((width, height)),
        _ => Err(StepError::Tmux {
            index,
            verb: "display-message".into(),
            detail: format!("unusable window dimensions {stdout:?}"),
        }),
    }
}

#[cfg(test)]
mod tests;
