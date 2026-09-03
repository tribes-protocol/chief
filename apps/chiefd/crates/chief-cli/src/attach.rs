//! `chief attach` — connect to the company in this directory, or
//! start it and then connect.
//!
//! Ported from the deleted TypeScript `attach.ts` and its integration half
//! `attach-wiring.ts`.
//!
//! # THE HARD RULE, preserved structurally
//!
//! A stopped attach comes back CEO-only, and this module cannot ask for
//! anything else because it cannot ask for a boot AT ALL. There is no
//! boot-shaped call reachable from here — `CompanyClient::prepare_ceo_only`
//! was the one, and it is deleted with the daemon-side CEO boot
//! (chief-home-is-cwd §4c) — and nothing roster-shaped ever existed on the
//! client. A future edit has nowhere to reach for either, which is a stronger
//! guarantee than a test asserting one merely did not fire.
//!
//! CEO-only is reached by the STORE's own fail-safe rather than by a client
//! stating it: an omitted launch intent is an empty allow-list, so the fence
//! admits the root head and denies everyone else, and the root holds an
//! unconditional organization-root lease that keeps it desired
//! (`conformance/fixtures/activity/fence-omitted-is-chief-only-not-unfenced.json`).
//! `stop` clears the launch intent on its way down, so the state a stopped
//! company is already in IS CEO-only.
//!
//! # Attach ensures the company is ACTUATED
//!
//! chiefd decides who runs; a client is what runs them. `attach` therefore does
//! not merely enter a company — it makes sure somebody is actuating one, and
//! starts an actuator when nobody is. Without that step the front door led
//! nowhere: `chief` handed the operator `chief attach <company>`, and
//! that verb could not start anybody, so the operator had to know a second verb
//! (`chief actuate`) before the product would run at all. `chief actuate`
//! remains a first-class verb for an operator who wants the resident process in
//! a terminal of their own; nobody has to type it to get a company running.
//!
//! # What the port deletes
//!
//! The TypeScript had to spawn the daemon against a GUESSED tmux socket
//! (`companyBootSocket`'s precedence minus the tier that needs a live daemon),
//! then read the real socket once the daemon was up, then — when the two
//! differed — STOP the daemon it had just started and respawn it onto the
//! corrected socket before booting. That whole stop-restart-before-boot step is
//! gone: the recorded ownership socket is read from the daemon after it is
//! healthy but BEFORE anything is actuated, and if it differs from the spawn
//! value the daemon is told about it rather than restarted, because the
//! actuator reads its socket per convergence, not once at spawn.

use std::path::Path;
use std::time::{Duration, Instant};

use super::company::CompanyClient;
use super::daemon::{self, DaemonStatus};
use super::http::Client;
use super::tmux::ActuatorSession;
use super::{tmux, LifecycleError, Result};
use chief_cli::actuate::trust;
use chief_cli::ladder::{Ladder, LadderEvents};
use chief_cli::sidebar;

/// How long attach waits for the actuator it started to enrol with chiefd and
/// project the company's session.
///
/// A budget, not a poll interval: the actuator is a process this command just
/// forked, and there is no push channel for "the process I started has taken
/// its lease". It covers the actuator's own preflight (which runs `tmux -V` and
/// `pi --version`), beacond discovery, the first observe → report round trip,
/// and the apply pass that mints the session.
const ACTUATOR_BUDGET: Duration = Duration::from_secs(45);

/// The doors into an operator's company session, as the log names them.
///
/// EVERY path that puts an operator's client into `org-<slug>_` has one of
/// these and passes it to [`enter_company_session`]. They exist because the log
/// has to answer "which way did the operator get in" without anybody having to
/// correlate timestamps against the shell history — the question that cost a
/// live ssh session when Founder mode turned out to be a door nobody had
/// counted.
pub(crate) const DOOR_ATTACH_RUNNING: &str = "attach-running";
/// The company was stopped and this attach started it.
pub(crate) const DOOR_ATTACH_STARTED: &str = "attach-started";
/// Founder mode handed the operator over after creating the company —
/// **the door that shipped with no rail**.
pub(crate) const DOOR_FOUNDER_HANDOVER: &str = "founder-handover";

/// The cadence of that wait.
const ACTUATOR_INTERVAL: Duration = Duration::from_millis(200);

/// The lines [`await_actuator`] writes. Waiting for a window that a process
/// this command just forked has not opened yet is the normal case, so only the
/// entry, the resolution and the exhaustion are worth a default-level line.
const ACTUATOR_WINDOW_LADDER: LadderEvents = LadderEvents {
    waiting: "actuator.window.wait",
    resolved: "actuator.window.running",
    failed: "actuator.window.missing",
};

/// The lines [`await_company_session`] writes. This is the ladder the incident
/// log caught: seven `tmux.verb.failed` warnings in 700 ms, then success.
const COMPANY_SESSION_LADDER: LadderEvents = LadderEvents {
    waiting: "company.session.wait",
    resolved: "company.session.present",
    failed: "company.session.missing",
};

/// The environment the actuator pane needs on top of [`tmux::PANE_ENVIRONMENT`].
///
/// Every one of these is a fact THIS process resolved and the tmux server
/// cannot be assumed to hold. `PATH` and `HOME` are on the list for a specific
/// reason: attach runs the resident-actuator preflight in its own process
/// before starting the window, and that decision is only honest if the window
/// inherits the environment the decision was made from. A `pi` found on
/// attach's PATH and absent from the tmux server's is exactly the fault that
/// made every minted pane die at creation.
/// `CHIEFD_PI_VERSION` was here to reach a pi attestation nothing ever called;
/// the attestation is deleted, so forwarding the variable named a reader that
/// did not exist.
const ACTUATOR_ENVIRONMENT: [&str; 4] =
    ["HOME", "PATH", "BEACOND_URL", "TEAM_LAUNCHER_TMUX_SOCKET"];

/// Run one attach, on the company in `dir`.
///
/// Sequence:
/// refuse a directory with no company → is its daemon running (rendezvous,
/// pid, health, identity) →
/// unhealthy: refuse up front with the recovery command, never prompt to
/// "start" a daemon that is already running →
/// stopped: start the daemon →
/// read the company's own SLUG back and compose its session name →
/// session already up: attach immediately →
/// otherwise: start an actuator, state CEO-only intent, wait for the session,
/// attach. A failure mid-start propagates once: no retry loop, no hanging
/// prompt.
///
/// # THE DAEMON COMES UP BEFORE THE SESSION CAN BE NAMED
///
/// This used to probe tmux first, because a session was `org-<slug>_` and the
/// slug was the argument the operator typed. Nobody types one: a session is
/// `org-<slug>-<key6>_`, the key comes from this directory and the SLUG comes
/// from the store — which this company's own daemon serves. So the daemon is
/// adopted or started first, and every tmux question is asked afterwards
/// against a name that is READ rather than guessed. An adopted daemon costs one
/// file read and two loopback probes to get there, which is what the beacond
/// lookup cost before it.
///
/// The actuator step is not a detail of the daemon step. A daemon serves the
/// desired set; it does not project it, so a company with a healthy daemon and
/// nobody actuating stays `desiredActive: false` for ever. Starting an actuator
/// is therefore its own step, and a doc comment that omits it is describing a
/// sequence that does not work.
///
/// # Errors
/// [`LifecycleError`] naming the refusal and the operator's next move.
pub(crate) async fn run(dir: &Path) -> Result<()> {
    let home = super::paths::home()?;
    super::preflight::require_ready(super::preflight::Surface::Attach)?;
    super::require_a_company_here(dir, "chief attach")?;
    require_a_terminal_to_seat(dir)?;
    // AUTHENTICATED: every request below goes to a COMPANY DAEMON, which
    // verifies a presented bearer.
    let client = Client::operator(dir);
    let key = super::paths::company_key(dir);

    let started = match daemon_move(daemon::status(&client, dir).await) {
        DaemonMove::Adopt => daemon::resolve_running(&client, dir).await.ok_or_else(|| {
            LifecycleError::unreachable(format!(
                "chief attach: the daemon for {} stopped answering between two probes; run \
                 `chief attach` again",
                dir.display()
            ))
        })?,
        DaemonMove::Recover => {
            // A second spawn would collide with the pointer that is already
            // there, so the pointer goes first. `chief stop` is exactly what
            // this branch used to tell the operator to type.
            println!(
                "chief attach: {} has a ChiefD process that is not answering; taking it down and \
                 starting a fresh one",
                dir.display()
            );
            daemon::stop(&client, dir).await?;
            daemon::start(
                &client,
                &home,
                dir,
                &super::company::boot_socket_request(&super::paths::company_key(dir)),
            )
            .await?
        }
        DaemonMove::Start => {
            println!("chief attach: starting ChiefD for {}", dir.display());
            daemon::start(
                &client,
                &home,
                dir,
                &super::company::boot_socket_request(&super::paths::company_key(dir)),
            )
            .await?
        }
    };

    // Now — and only now — the authoritative placement facts are readable,
    // because they live in the SQL store this daemon serves. The SLUG among
    // them: it is what the company is called, and the session is named from it.
    let company_client = CompanyClient::new(&client, &started.url, dir, &key);
    let facts = company_client.facts().await?.ok_or_else(|| {
        LifecycleError::refused(format!(
            "chief attach: the company in {} has a daemon but no manifest — it was never \
             created. Create one here with `chief`.",
            dir.display()
        ))
    })?;
    let session_name = super::company::conventional_session_name(&facts.slug, &key);
    let (socket, started) =
        reconcile_runtime_claim(&client, &home, dir, &key, &session_name, started).await?;
    let command = resolve_actuator_command(dir)?;

    if session_move(started.adopted, tmux::session_exists(&socket, &session_name))
        == SessionMove::Enter
    {
        // A live session is not proof that anybody is still actuating it: an
        // actuator can die and leave its projection standing. Look for the
        // actuator's own window on this socket, and start one if it is not
        // there. The question is asked of tmux rather than of chiefd, which
        // holds no fact about what is running.
        ensure_actuator(dir, &socket, &session_name, &command).await?;
        // THE COMPANY COMES UP EVEN WHEN ITS SESSION DOES NOT ADD UP.
        //
        // `enter_company_session` refuses for exactly one reason now: tmux would
        // not answer for this session at all, which means the projection an
        // unclean shutdown left behind cannot be reasoned about. The answer is
        // not a diagnostic at the operator — it is to throw the projection away
        // and stand the company up again from the CEO, in this same invocation.
        if let Err(error) = enter_company_session(&socket, &session_name, dir, DOOR_ATTACH_RUNNING)
        {
            println!(
                "chief attach: this company's tmux session cannot be reconciled ({error}); \
                 starting '{}' again from the CEO alone",
                facts.slug
            );
            abandon_unreconcilable_session(&socket, &session_name);
            bring_up(dir, &socket, &session_name, &command).await?;
            if let Err(error) =
                enter_company_session(&socket, &session_name, dir, DOOR_ATTACH_STARTED)
            {
                tracing::warn!(
                    event = "attach.session.recovered-unfurnished",
                    session = %session_name,
                    detail = %error,
                    "the rebuilt session took no viewport; the company is up regardless"
                );
            }
        }
        return tmux::attach(&socket, &session_name);
    }

    println!("chief attach: booting '{}' (CEO-only)", facts.slug);
    bring_up(dir, &socket, &session_name, &command).await?;
    if let Err(error) = enter_company_session(&socket, &session_name, dir, DOOR_ATTACH_STARTED) {
        tracing::warn!(
            event = "attach.session.unfurnished",
            session = %session_name,
            detail = %error,
            "the freshly booted session took no viewport; the company is up regardless"
        );
    }
    tmux::attach(&socket, &session_name)
}

/// Move a company off a runtime-ownership claim that is dead by PROOF.
///
/// Returns the socket to actuate on, and the daemon that now serves this
/// company — a reconciliation restarts it, so the caller must not keep the one
/// it passed in.
///
/// # The upgrade this exists for
///
/// `cb63690a0` gave every company its own tmux server, because two companies
/// sharing one meant either teardown killed the other's panes — it cost a live
/// company eleven panes and five people. But every company created BEFORE it
/// holds a live claim naming the shared `default` server, and a claim is the
/// company's own record of where it runs. Obeying it for ever would leave every
/// existing company on the shared server; overriding it blind would converge a
/// second, shadow fleet onto a server the company might still be running on.
///
/// The way out is a PROOF, and only the client can make it: chiefd holds the
/// claim but has no multiplexer, and this process has tmux but cannot name the
/// session to ask about until a daemon serves it the slug. So the order is
/// daemon first, proof second — which is also why chiefd no longer refuses this
/// pair at boot. A refusal that fires before the only evidence which could
/// settle it can exist is one an operator cannot act on.
///
/// A reconciliation RELEASES rather than overwrites, and then restarts. The
/// release is the daemon's own verb over the socket it holds, and the fresh
/// daemon claims the new socket with its own — no second spelling of a claim,
/// and nothing left half-written if the restart fails.
///
/// # THE RE-CLAIM IS NOT OPTIONAL, and there is no ordinary path that does it
///
/// This function used to say the fresh daemon claimed "the ordinary way". It
/// does not, and no daemon ever did: a claim is minted only inside
/// `runtime_lifecycle::claim_ownership`, which only `launch_runtime` and
/// `stop_supervised_runtime` call — the runtime PROJECTING or TEARING DOWN a
/// session. A post-handoff boot does neither. It converges, and the company's
/// people come back from durable start intent they already hold.
///
/// That was invisible until this function existed, because claims are sticky:
/// before it, a restart kept the active claim because nothing released one for
/// a company that stays up. Measured 2026-08-18: 2m 37s after a handoff, with
/// the company demonstrably running, its row still read
/// `status='released'`. A company that is UP holding NO claim is precisely the
/// state the shadow-fleet guard exists to make impossible — a second `chief`
/// in that directory meets no claim to contradict and converges a second fleet.
///
/// So this releases and re-claims as one move. A failure to claim FAILS the
/// attach rather than being logged and stepped over: the company is not yet
/// brought up at this point, so refusing leaves it down and unclaimed, which is
/// honest, where continuing would leave it up and unclaimed, which is the
/// hazard.
async fn reconcile_runtime_claim(
    client: &Client,
    home: &Path,
    dir: &Path,
    key: &str,
    session_name: &str,
    started: daemon::RunningDaemon,
) -> Result<(String, daemon::RunningDaemon)> {
    let company_client = CompanyClient::new(client, &started.url, dir, key);
    let recorded = company_client.active_runtime_owner_socket().await?;
    let own = super::company::boot_socket_from_env(None, key);
    let claimed = match super::company::claim_move(recorded.as_deref(), &own, |claimed| {
        tmux::session_exists(claimed, session_name)
    }) {
        super::company::ClaimMove::Obey => {
            return Ok((super::company::boot_socket_from_env(recorded.as_deref(), key), started));
        }
        super::company::ClaimMove::Reclaim => recorded.unwrap_or_default(),
    };

    println!(
        "chief: this company's runtime claim names tmux socket '{claimed}', where no session for \
         it is running; taking the claim back onto '{own}'"
    );
    tracing::info!(
        event = "attach.claim.reconciled",
        organization = %dir.display(),
        from = %claimed,
        to = %own,
        "the recorded runtime socket holds no session for this company, so the claim is released"
    );
    company_client.release_runtime_ownership().await?;
    // The daemon resolved its socket at boot and refuses to adopt another
    // mid-run, so the new placement takes effect the way every other placement
    // change does: this daemon exits and the next one resolves again. With the
    // claim released it falls to this client's preference, which is `own`.
    daemon::stop(client, dir).await?;
    let restarted =
        daemon::start(client, home, dir, &super::company::boot_socket_request(key)).await?;
    // The route names no socket: this daemon claims the socket IT resolved,
    // which the release above made claimable. Without it the company comes up
    // holding no runtime-ownership claim at all.
    CompanyClient::new(client, &restarted.url, dir, key).claim_runtime_ownership().await?;
    Ok((own, restarted))
}

/// What attach does with the daemon state it reads.
///
/// Starting a stopped company is non-destructive. It restores the normal
/// operator session, so both bare `chief` and explicit `chief attach` take this
/// decision without a confirmation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonMove {
    Adopt,
    /// Stop what is there and start a fresh daemon, in THIS invocation.
    Recover,
    Start,
}

fn daemon_move(status: DaemonStatus) -> DaemonMove {
    match status {
        DaemonStatus::Running => DaemonMove::Adopt,
        // AN UNHEALTHY DAEMON IS RECOVERED, NEVER REPORTED.
        //
        // This used to refuse: *"has a ChiefD process running but unhealthy;
        // run `chief stop` here first, then retry"* — a company that needs two
        // invocations and a memorized command to come back. An unclean shutdown
        // produces this state routinely, and for a reason nothing on disk can
        // tell apart from a real one: `.chief/run/daemon.json` records a bare
        // pid, pids restart low after a reboot, and an unrelated process that
        // inherited the number reads as alive-but-not-answering.
        //
        // The refusal's own advice is the whole recovery, so `chief` performs
        // it. One invocation, no operator surgery.
        DaemonStatus::Unhealthy => DaemonMove::Recover,
        DaemonStatus::Stopped => DaemonMove::Start,
    }
}

/// What attach does after it knows whether it adopted an existing daemon and
/// whether that daemon's company session is present.
///
/// A session is safe to enter only when both facts are positive. Every other
/// state goes through normal bring-up, so attach never enters a missing or
/// uncertain projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMove {
    Enter,
    BringUp,
}

fn session_move(adopted: bool, session_present: Option<bool>) -> SessionMove {
    if adopted && session_present == Some(true) {
        SessionMove::Enter
    } else {
        SessionMove::BringUp
    }
}

/// Refuse early when this caller has no terminal for tmux to seat a client in.
///
/// # `attach` starts its own tmux now, and this is the honest half of that
///
/// Outside tmux, [`tmux::attach`] runs `attach-session`, which needs a terminal
/// on both ends. `chief` has asked exactly this before minting anything
/// since an operator reported the same complaint about it
/// ([`super::founder::run`]); `attach` instead refused for lack of an
/// AMBIENT tmux, which is what the operator hit: *"`chief attach` — when I run
/// this it keeps telling me you need tmux. It should just tmux for me the way
/// the `chief` command does."*
///
/// So the ambient demand is gone — `Surface::Attach` is `TerminalNeed::BootsOwn`
/// — and this is what remains. A caller with no TTY (a pipe, a cron line, CI)
/// still cannot be handed a tmux client, and is told so here rather than
/// reaching `attach-session` and getting tmux's own `open terminal failed`.
/// Both doors call [`super::founder::can_be_attached`], so they give the SAME
/// answer to the same situation.
///
/// Inside tmux nothing is asked: [`tmux::attach`] switches the client the
/// operator is already sitting in, which needs no terminal of its own.
fn require_a_terminal_to_seat(dir: &Path) -> Result<()> {
    use std::io::IsTerminal as _;

    if tmux::ambient_tmux() {
        tracing::debug!(
            event = "attach.terminal.ambient",
            organization = %dir.display(),
            "already inside tmux; the operator's own client will be switched"
        );
        return Ok(());
    }
    let seatable = super::founder::can_be_attached(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    if !seatable {
        tracing::warn!(
            event = "attach.terminal.absent",
            organization = %dir.display(),
            "outside tmux with no terminal on both ends, so no tmux client can be seated"
        );
        return Err(LifecycleError::refused(format!(
            "chief attach: {} is outside tmux and this caller has no terminal to attach a \
             session to. Run `chief attach` from an interactive terminal, or from inside an \
             existing tmux session.",
            dir.display()
        )));
    }
    tracing::info!(
        event = "attach.terminal.booting",
        organization = %dir.display(),
        "outside tmux with a terminal; chief will start the tmux client itself"
    );
    Ok(())
}

/// THE ONE DOOR into an operator's company session. Give every window its
/// sidebar rail, and say what was decided.
///
/// # Why this is a shared function and not `attach`'s private helper
///
/// It used to be `ensure_rails`, private to `attach`, and that shape shipped a
/// live defect: **`attach` is not the only way into a company session.**
/// Founder mode (`chief`) brings the company up and then
/// `switch-client`s the operator straight into `org-<slug>_`
/// (`founder::hand_over_with`), which never went near this code. So a company
/// created the normal way had no rail on its very first run — the worst case,
/// because it reads as a feature that does not exist rather than one that
/// failed.
///
/// Every path that puts an operator's client into a company session calls THIS,
/// immediately before the handover, and `door` names which path it was.
/// `scripts/test/...` and the unit test below enumerate the call sites, so a
/// fourth door cannot be added silently the way the third was.
///
/// # It always leaves a trace
///
/// Diagnosing the missing rail needed an ssh session to the operator's live
/// company, because a `grep` of the log found NOTHING — no attempt, no refusal,
/// no decision. Every branch below writes one line now, so "the rail code was
/// never called" and "it was called and declined" are one grep apart. The
/// `entering` line is written first, before anything can fail, which is what
/// makes the absence itself readable.
///
/// # This is the only place that DECIDES a company is railed
///
/// This verb creates no windows. Every company window is minted by the converge
/// loop — `create_session`, `create_window_with_spawn`, `create_window_by_move`
/// — so a department that starts after the attach opens a window this function
/// has already swept past, and the operator's only navigation is missing from
/// it. That reads as a bug, not a limitation, so the converge loop maintains
/// the invariant afterwards (`actuate::interpret::ensure_rail_in_window`).
///
/// What it does NOT do is record a flag saying so. "Is this company operated
/// with a rail?" is DERIVED, by asking whether any window already has a rail
/// pane — the same question this function asks to decide what to sweep. A
/// stored marker would need something to clear it, and nothing would: an
/// operator who closed every rail would keep getting fresh ones on every new
/// window, with no way to turn the feature off. This repo has paid for a
/// persisted display answer once already (`last_pane_department_id`, #751-P9,
/// "nothing durable replaces this"), and the rule that came out of it applies
/// exactly here.
///
/// # The sweep is an ENUMERATION, not a fix-up of a known set
///
/// It lists every pane in the session and rails every window that has none.
/// That matters for the company that converged headless: the actuator may have
/// minted a session and several windows before anybody attached, and all of
/// them are covered here because the list comes from tmux rather than from
/// anything this process was tracking.
///
/// Idempotent by construction: a window that already carries a rail pane is
/// skipped, so re-attaching an open company adds nothing, no window ever gets a
/// second rail, and the rail survives a detach.
/// Read `list-panes -F '#{window_id}\t#{@organization_sidebar}'` into
/// (windows that already have a rail, windows that need one).
///
/// # THE BUG THIS FUNCTION EXISTS TO HOLD
///
/// [`tmux::TmuxOutput::stdout`] is **trimmed**. A window whose sidebar tag is
/// unset prints `@1\t` — the marker is the empty string — and when that is the
/// LAST (or only) line, the trim strips the trailing tab and leaves `@1`. The
/// original parse was `split_once('\t')` inside a `filter_map`, so a line with
/// no tab was silently dropped.
///
/// For a CEO-only company that is EVERY line: one window, one pane, no rail
/// tag, so the entire output is `@1\t`, the survey saw zero windows, and the
/// rail was never placed. It reached production and read as "the sidebar does
/// not exist"; it survived the tests because a fixture with two panes emits two
/// lines and only the last one loses its tab, leaving the first to parse.
///
/// So a line with NO tab is a window with an EMPTY marker — never a line to
/// skip. That is the whole fix, and it is a pure function so the exact
/// production string can be asserted without a tmux server.
fn rail_survey(listed: &str) -> (std::collections::BTreeSet<&str>, Vec<&str>) {
    let mut railed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut windows: Vec<&str> = Vec::new();
    for line in listed.lines() {
        // NOT `filter_map(split_once)`: see this function's own doc. A trimmed
        // trailing tab makes an untagged window tabless, and dropping it is
        // exactly how a one-window company lost its rail.
        let (window, marker) = line.split_once('\t').unwrap_or((line, ""));
        let window = window.trim();
        if window.is_empty() {
            continue;
        }
        if marker.trim() == "1" {
            railed.insert(window);
        } else if !windows.contains(&window) {
            windows.push(window);
        }
    }
    (railed, windows)
}

/// Leave every pane mode before an operator sees the company.
///
/// Copy mode belongs to the pane, not to the client that entered it. A browser
/// can therefore detach while a rail or body pane still has `pane_in_mode=1`,
/// and the next attach inherits the orange copy-mode status instead of Chief.
/// This is the final handoff fence: it runs after the complete viewport is
/// installed and before either attach door seats the client.
fn leave_session_pane_modes(socket: &str, session: &str) -> Result<()> {
    let listed = tmux::run(
        socket,
        &["list-panes", "-s", "-t", session, "-F", "#{pane_id}\t#{pane_in_mode}"],
    );
    if !listed.ok() {
        return Err(LifecycleError::host(format!(
            "chief attach could not inspect pane modes before handoff: {}",
            listed.diagnostic()
        )));
    }
    for line in listed.stdout.lines() {
        let Some((pane, mode)) = line.split_once('\t') else { continue };
        if mode != "0" && safe_tmux_object_id(pane, '%') {
            // A mode can end between the survey and this command. tmux then
            // refuses `-X cancel`, but the required final state already holds.
            let _ = tmux::run(socket, &["send-keys", "-t", pane, "-X", "cancel"]);
        }
    }
    let remaining = tmux::run(
        socket,
        &["list-panes", "-s", "-t", session, "-F", "#{pane_id}\t#{pane_in_mode}"],
    );
    if !remaining.ok() {
        return Err(LifecycleError::host(format!(
            "chief attach could not verify pane modes before handoff: {}",
            remaining.diagnostic()
        )));
    }
    if remaining
        .stdout
        .lines()
        .any(|line| line.split_once('\t').is_some_and(|(_, mode)| mode != "0"))
    {
        return Err(LifecycleError::host(
            "chief attach could not leave tmux copy mode before handoff",
        ));
    }
    Ok(())
}

/// Resolve one complete native viewport manifest from current tmux truth.
///
/// Every managed window must have exactly one tagged rail. A partial survey
/// disables the native path and leaves the exact asynchronous callback as the
/// fail-closed authority.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewportManifestWindow {
    window: String,
    window_tag: String,
    rail: String,
    panes: Vec<(String, bool)>,
}

/// The survey, or the ONE SENTENCE saying what voided it.
///
/// It used to answer an empty `Vec` for every reason it can fail, and the
/// caller turned that into one fixed string. Measured on a live box
/// 2026-08-24: 486 refusals in a day, one every ~25 seconds, each a distinct
/// hook-spawned `chief` process that refused and exited — and the survey run
/// by hand against the same live session was HEALTHY, every managed window
/// carrying exactly one rail. So the residual trigger is a state a sample does
/// not catch, and a refusal that cannot say which window it read is an
/// investigation rather than a diagnosis. The known cause (an untagged
/// unmanaged window voiding the survey) is genuinely fixed; this does not
/// guess at a second one, it makes the next occurrence legible.
// `std::result::Result` spelled out: this module's bare `Result<T>` is the
// crate alias over `LifecycleError`, and the survey's failure is a SENTENCE for
// a caller to wrap, not a lifecycle error of its own.
fn viewport_manifest_survey(
    listed: &str,
) -> std::result::Result<Vec<ViewportManifestWindow>, String> {
    let mut windows: std::collections::BTreeMap<String, (String, Vec<(String, bool)>)> =
        std::collections::BTreeMap::new();
    // FIRST reason wins and the rest are not collected: the caller needs one
    // sentence to act on, and a survey with two faults is diagnosed by fixing
    // the first.
    let mut invalid: Option<String> = None;
    for line in listed.lines() {
        let mut fields = line.split('\t');
        let window = fields.next().unwrap_or_default().trim();
        let pane = fields.next().unwrap_or_default().trim();
        let window_tag = fields.next().unwrap_or_default().trim();
        let sidebar = fields.next().unwrap_or_default().trim();
        if !window.strip_prefix('@').is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) || !pane.strip_prefix('%').is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            invalid.get_or_insert_with(|| {
                format!("tmux listed a line whose window/pane ids are not ids: '{line}'")
            });
            continue;
        }
        // AN UNMANAGED WINDOW IS NOT PART OF THIS MANIFEST, and must not be
        // able to void it.
        //
        // `list-panes -s` returns EVERY pane in the session, including windows
        // chief does not own — the operator's own shell, a window they split
        // themselves, the founder's. None of those carries an
        // `@organization_window_id` and none of them has a rail, so the
        // exactly-one-rail rule below counted zero for them and discarded the
        // WHOLE survey. The doc above this function has always said "every
        // MANAGED window"; the code never filtered to them.
        //
        // Measured on a live box, 2026-08-21: one untagged single-pane window
        // (`@1`) among three healthy managed ones, and every refresh answered
        // `viewport manifest requires one tagged rail in every managed window`
        // — 26 times in one session. The refresh is what re-installs the
        // resize hook, so the hook kept the topology epoch it was born with
        // while the session moved 23 generations past it, and every sidebar
        // drag the operator made was refused as stale.
        // Ahead of the safety guard below on purpose: an ABSENT tag is not an
        // unsafe one, and `is_safe_logical_id` answers false for the empty
        // string — so testing safety first put every untagged window in the
        // `invalid` bucket and voided the survey, which is the defect itself.
        if window_tag.is_empty() {
            continue;
        }
        // THE PRODUCT'S OWN WINDOW GRAMMAR, not the organization-id rule. See
        // `is_safe_window_logical_id`: the generic rule bans the colon that
        // `__person__:<id>` and `__overview__:<dept>` are built from, so this
        // survey voided on every real session and had done since person
        // windows existed.
        if !trust::is_safe_window_logical_id(window_tag) {
            invalid.get_or_insert_with(|| {
                format!("window {window} carries an unsafe logical id '{window_tag}'")
            });
            continue;
        }
        if !matches!(sidebar, "" | "1") {
            invalid.get_or_insert_with(|| {
                format!("pane {pane} in window {window} has sidebar tag '{sidebar}', not '' or '1'")
            });
            continue;
        }
        let entry =
            windows.entry(window.to_owned()).or_insert_with(|| (window_tag.to_owned(), Vec::new()));
        if entry.0 != window_tag {
            let first = entry.0.clone();
            invalid.get_or_insert_with(|| {
                format!("window {window} is tagged both '{first}' and '{window_tag}'")
            });
        }
        entry.1.push((pane.to_owned(), sidebar == "1"));
    }
    if let Some(reason) = invalid {
        return Err(reason);
    }
    if windows.is_empty() {
        return Err(if listed.trim().is_empty() {
            "tmux listed no panes at all".to_owned()
        } else {
            "no window in the session carries an organization tag".to_owned()
        });
    }
    for (window, (window_tag, panes)) in &windows {
        let rails = panes.iter().filter(|(_, sidebar)| *sidebar).count();
        if rails != 1 {
            return Err(format!(
                "window {window} ('{window_tag}') has {rails} tagged rails among {} panes, not 1",
                panes.len()
            ));
        }
    }
    Ok(windows
        .into_iter()
        .map(|(window, (window_tag, panes))| {
            let rails: Vec<&str> = panes
                .iter()
                .filter_map(|(pane, sidebar)| sidebar.then_some(pane.as_str()))
                .collect();
            ViewportManifestWindow { window, window_tag, rail: rails[0].to_owned(), panes }
        })
        .collect())
}

/// Build the one tmux frame that publishes a fresh rail.
///
/// `split-window` starts the rail process, so a tag or resize sent in a later
/// tmux invocation is already too late: the rail client can draw the transient
/// PTY it received at birth. Tmux redraws after a command sequence, so stamp
/// the shared width, split at that width, tag the active new pane, repeat the
/// exact pane width, and hand the cursor back to the pane the operator was in
/// — all in ONE queue. The operator can then see only the final frame.
fn rail_mint_argv(
    _session: &str,
    window: &str,
    dir: &Path,
    executable: &Path,
    columns: i64,
) -> Vec<String> {
    vec![
        "split-window".to_owned(),
        "-h".to_owned(),
        "-b".to_owned(),
        "-l".to_owned(),
        columns.to_string(),
        "-t".to_owned(),
        window.to_owned(),
        "-P".to_owned(),
        "-F".to_owned(),
        "#{pane_id}".to_owned(),
        "-c".to_owned(),
        dir.display().to_string(),
        executable.display().to_string(),
        "sidebar".to_owned(),
        ";".to_owned(),
        // `split-window` selects its new pane because it has no `-d`. A window
        // target therefore resolves to that rail for both pane-local writes.
        "set-option".to_owned(),
        "-p".to_owned(),
        "-t".to_owned(),
        window.to_owned(),
        trust::tags::SIDEBAR.to_owned(),
        "1".to_owned(),
        ";".to_owned(),
        "resize-pane".to_owned(),
        "-x".to_owned(),
        columns.to_string(),
        "-t".to_owned(),
        window.to_owned(),
        ";".to_owned(),
        // THE CURSOR GOES BACK, AND IT GOES BACK LAST. The rail is furniture;
        // the person is the product. `split-window` left the rail active, so
        // without this the operator opens their company typing into the
        // sidebar. `-l` is the window's LAST pane, which after a split is the
        // pane that was active before it — measured on tmux 3.7c, and true
        // even when the split targeted some other pane. It is the final
        // command of the frame on purpose: the tag and the resize above
        // resolve a WINDOW target to its active pane, so a selection moved
        // ahead of either would write them to the wrong pane.
        "select-pane".to_owned(),
        "-l".to_owned(),
        "-t".to_owned(),
        window.to_owned(),
    ]
}

fn entry_rail_columns(recorded: &str, collapsed: &str) -> i64 {
    if collapsed.trim() == "1" {
        return chief_cli::layout::RAIL_COLLAPSED_COLUMNS;
    }
    recorded
        .trim()
        .parse::<i64>()
        .map_or(sidebar::brain::RAIL_DEFAULT_COLUMNS, sidebar::brain::canonical_columns)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn tmux_command_string(argv: &[String]) -> String {
    argv.iter()
        .map(|word| if word == ";" { ";".to_owned() } else { shell_quote(word) })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn tmux_static(value: &str) -> String {
    chief_cli::sidebar::tmux_static(value)
}

fn run_attach_mutation_if_current(
    socket: &str,
    session: &str,
    organization: &str,
    server_nonce: &str,
    topology_generation: u64,
    argv: &[String],
) -> Result<tmux::TmuxOutput> {
    if !trust::is_safe_company_session(session)
        || !trust::is_safe_logical_id(organization)
        || !trust::is_safe_server_nonce(server_nonce)
    {
        return Err(LifecycleError::host("attach mutation authority is not safe"));
    }
    let predicate = format!(
        "#{{&&:#{{==:#{{{}}},{organization}}},\
         #{{&&:#{{==:#{{{}}},{topology_generation}}},#{{==:#{{{}}},{server_nonce}}}}}}}",
        trust::tags::ORGANIZATION,
        trust::viewport_options::TOPOLOGY_EPOCH,
        trust::viewport_options::SERVER_NONCE,
    );
    let command = tmux_command_string(argv);
    let guarded = tmux::run(
        socket,
        &["if-shell", "-F", "-t", session, &predicate, &command, "display-message -p stale"],
    );
    if !guarded.ok() {
        return Err(LifecycleError::host(guarded.diagnostic()));
    }
    if guarded.stdout.trim() == "stale" {
        return Err(LifecycleError::host("chief attach viewport authority became stale"));
    }
    Ok(guarded)
}

fn viewport_hook_command(executable: &Path, socket: &str, session: &str) -> String {
    format!(
        "{} viewport-resize {} {} #{{q:@organization_id}} #{{q:hook_client}} \
         #{{@chief_viewport_request}} #{{q:@chief_viewport_server_nonce}}",
        shell_quote(&tmux_static(&executable.display().to_string())),
        shell_quote(&tmux_static(socket)),
        shell_quote(&tmux_static(session)),
    )
}

fn viewport_hook_eligibility_command(executable: &Path, socket: &str, session: &str) -> String {
    format!(
        "{} viewport-client-eligible {} {} #{{q:hook_client}} \
         #{{q:@chief_viewport_server_nonce}}",
        shell_quote(&tmux_static(&executable.display().to_string())),
        shell_quote(&tmux_static(socket)),
        shell_quote(&tmux_static(session)),
    )
}

fn viewport_hook_action(
    executable: &Path,
    socket: &str,
    session: &str,
    manifest: &[ViewportManifestWindow],
    expanded_columns: i64,
    collapsed: bool,
    topology_generation: u64,
) -> String {
    let columns =
        if collapsed { chief_cli::layout::RAIL_COLLAPSED_COLUMNS } else { expanded_columns };
    let target = shell_quote(&tmux_static(session));
    let callback =
        format!("{} >/dev/null 2>&1 || :", viewport_hook_command(executable, socket, session));
    let publish = format!(
        "set-option -gF @chief_viewport_generation '#{{e|+:#{{@chief_viewport_generation}},1}}' ; \
         set-option -F -t {target} @chief_viewport_owner '#{{hook_client}}' ; \
         set-option -F -t {target} @chief_viewport_request '#{{@chief_viewport_generation}}' ; \
         run-shell -b -t {target} {}",
        shell_quote(&callback)
    );
    let mut native = Vec::new();
    for entry in manifest {
        let window = shell_quote(&tmux_static(&entry.window));
        let rail = shell_quote(&tmux_static(&entry.rail));
        if entry.panes.len() == 1 {
            // A department whose people are all in focus windows keeps one
            // rail pane as valid managed furniture. tmux must give that lone
            // pane the whole window; asking it for the normal rail width is
            // impossible and used to reject the complete fast publication.
            native.push(format!(
                "resize-window -A -t {window} ; \
                 set-option -w -t {window} window-size manual"
            ));
        } else {
            native.push(format!(
                "resize-window -A -t {window} ; resize-pane -x {columns} -t {rail} ; \
                 set-option -w -t {window} window-size manual"
            ));
        }
    }
    let fast = if native.is_empty() {
        String::new()
    } else {
        let visible = manifest
            .iter()
            .map(|entry| format!("#{{==:#{{window_id}},{}}}", tmux_static(&entry.window)))
            .reduce(|left, right| format!("#{{||:{left},{right}}}"))
            .unwrap_or_else(|| "0".to_owned());
        let clear_preflight = format!("set-option -qu -t {target} @chief_viewport_preflight");
        let earn_proof = format!(
            "set-option -F -t {target} @chief_viewport_preflight \
             '#{{e|+:#{{@chief_viewport_preflight}},1}}'"
        );
        let mut preflight = vec![format!("set-option -t {target} @chief_viewport_preflight 0")];
        let mut expected_proofs = 0usize;
        for entry in manifest {
            // ONE PROOF PER RAIL, NOT ONE PER PANE. The native action below
            // touches exactly two things in a window — the WINDOW (`resize-window
            // -A`, `window-size manual`) and its RAIL (`resize-pane -x`) — so a
            // body pane's identity proves nothing about whether it may run. The
            // window proof that follows already pins `window_panes`, which is the
            // only fact about the body panes the action depends on.
            //
            // THE WEDGE THIS ENDS. A proof per pane made this hook grow ~370
            // bytes for every person in the company, and the finished hook is
            // shell-quoted into a `set-hook` argument and shell-quoted again into
            // an `if-shell` body. The tmux 3.3a client refuses any packed argv
            // over `MAX_IMSGSIZE` — measured on a live box: 16300 bytes accepted,
            // 16350 `failed to send command`, 17000 `command too long`. Measured
            // with this builder, a proof per pane cost 14383 bytes at 24 panes and
            // 17130 bytes at 35, so an ordinary company crossed the ceiling and
            // `chief` exited with `chief attach could not install the viewport
            // hook set: command too long` instead of showing the operator their
            // company. Per window it is 2 proofs whatever the roster.
            let rail_static = tmux_static(&entry.rail);
            let rail_target = shell_quote(&rail_static);
            let rail_identity = format!(
                "#{{&&:#{{==:#{{pane_id}},{rail_static}}},#{{&&:#{{==:#{{window_id}},{}}},#{{==:#{{@organization_sidebar}},1}}}}}}",
                tmux_static(&entry.window)
            );
            let rail_predicate = if entry.panes.len() > 1 {
                format!("#{{&&:{rail_identity},#{{==:#{{pane_width}},{columns}}}}}")
            } else {
                rail_identity
            };
            expected_proofs += 1;
            preflight.push(format!(
                "if-shell -F -t {rail_target} {} {} ''",
                shell_quote(&rail_predicate),
                shell_quote(&earn_proof)
            ));
            let window = shell_quote(&tmux_static(&entry.window));
            let window_id = tmux_static(&entry.window);
            let window_tag = tmux_static(&entry.window_tag);
            let pane_count = entry.panes.len();
            let predicate = format!(
                "#{{&&:#{{==:#{{window_id}},{window_id}}},#{{&&:#{{==:#{{@organization_window_id}},{window_tag}}},#{{==:#{{window_panes}},{pane_count}}}}}}}"
            );
            expected_proofs += 1;
            preflight.push(format!(
                "if-shell -F -t {window} {} {} ''",
                shell_quote(&predicate),
                shell_quote(&earn_proof)
            ));
        }
        let session_static = tmux_static(session);
        let window_count = manifest.len();
        let collapsed = i32::from(collapsed);
        let columns_match = if expanded_columns == sidebar::brain::RAIL_DEFAULT_COLUMNS {
            format!(
                "#{{||:#{{==:#{{@chief_sidebar_columns}},}},\
                 #{{==:#{{@chief_sidebar_columns}},{expanded_columns}}}}}"
            )
        } else {
            format!("#{{==:#{{@chief_sidebar_columns}},{expanded_columns}}}")
        };
        let predicate = format!(
            "#{{&&:#{{==:#{{@chief_viewport_fast_session}},{session_static}}},\
             #{{&&:#{{==:#{{hook_client}},#{{@chief_viewport_fast_owner}}}},\
             #{{&&:#{{==:#{{@organization_id}},#{{@chief_viewport_fast_organization}}}},\
             #{{&&:#{{==:#{{@chief_viewport_membership_generation}},#{{@chief_viewport_fast_generation}}}},\
             #{{&&:#{{==:#{{@chief_viewport_topology_epoch}},{topology_generation}}},\
             #{{&&:#{{==:#{{@chief_viewport_manifest_epoch}},{topology_generation}}},\
             #{{&&:{columns_match},\
             #{{&&:#{{==:#{{?#{{==:#{{@chief_sidebar_collapsed}},1}},1,0}},{collapsed}}},\
             #{{&&:#{{==:#{{session_windows}},{window_count}}},{visible}}}}}}}}}}}}}}}}}}}"
        );
        expected_proofs += 1;
        preflight.push(format!(
            "if-shell -F -t {target} {} {} ''",
            shell_quote(&predicate),
            shell_quote(&earn_proof)
        ));
        preflight.push(format!(
            "if-shell -F -t {target} '#{{==:#{{@chief_viewport_preflight}},{expected_proofs}}}' {} ''",
            shell_quote(&native.join(" ; "))
        ));
        preflight.push(clear_preflight);
        format!("{} ; ", preflight.join(" ; "))
    };
    // `hook_client` is the only tmux 3.3a format that names the exact event
    // client. The synchronous, silent probe must accept it before this ordered
    // hook queue changes the generation or starts a background callback.
    format!(
        "{fast}if-shell {} {}",
        shell_quote(&viewport_hook_eligibility_command(executable, socket, session)),
        shell_quote(&publish)
    )
}

fn viewport_session_changed_action(executable: &Path, socket: &str) -> String {
    let command = format!(
        "{} viewport-client-changed {} #{{q:hook_client}} \
         #{{q:@chief_viewport_server_nonce}}",
        shell_quote(&tmux_static(&executable.display().to_string())),
        shell_quote(&tmux_static(socket)),
    );
    format!("run-shell -b {}", shell_quote(&format!("{command} >/dev/null 2>&1 || :")))
}

fn viewport_membership_action(executable: &Path, socket: &str) -> String {
    let census = format!(
        "{} viewport-client-census {} #{{{}}} \
         #{{q:@chief_viewport_server_nonce}} >/dev/null 2>&1 || :",
        shell_quote(&tmux_static(&executable.display().to_string())),
        shell_quote(&tmux_static(socket)),
        trust::viewport_options::MEMBERSHIP_GENERATION,
    );
    format!(
        "set-option -gF {} '#{{e|+:#{{{}}},1}}' ; \
         set-option -gu {} ; set-option -gu {} ; set-option -gu {} ; \
         set-option -gu {} ; run-shell -b {}",
        trust::viewport_options::MEMBERSHIP_GENERATION,
        trust::viewport_options::MEMBERSHIP_GENERATION,
        trust::viewport_options::FAST_SESSION,
        trust::viewport_options::FAST_OWNER,
        trust::viewport_options::FAST_ORGANIZATION,
        trust::viewport_options::FAST_GENERATION,
        shell_quote(&census),
    )
}

fn viewport_hook_bootstrap_argv(executable: &Path, socket: &str, session: &str) -> Vec<String> {
    let target = shell_quote(&tmux_static(session));
    let clear = format!(
        "set-option -qu -t {target} @chief_viewport_request ; \
         set-option -qu -t {target} @chief_viewport_owner"
    );
    let membership = viewport_membership_action(executable, socket);
    let attached = format!(
        "{membership} ; if-shell {} {}",
        shell_quote(&viewport_hook_eligibility_command(executable, socket, session)),
        shell_quote(&clear)
    );
    let detached = format!(
        "{membership} ; if-shell -t {target} -F '#{{==:#{{@chief_viewport_owner}},#{{hook_client}}}}' {}",
        shell_quote(&clear)
    );
    let session_changed =
        format!("{membership} ; {}", viewport_session_changed_action(executable, socket));
    let refresh_command = format!(
        "{} viewport-manifest-refresh {} {} #{{q:@chief_viewport_server_nonce}}",
        shell_quote(&tmux_static(&executable.display().to_string())),
        shell_quote(&tmux_static(socket)),
        shell_quote(&tmux_static(session)),
    );
    let width_command = format!(
        "{} viewport-sidebar-width {} {} #{{q:@organization_id}} #{{q:session_id}} \
         #{{q:@chief_viewport_server_nonce}}",
        shell_quote(&tmux_static(&executable.display().to_string())),
        shell_quote(&tmux_static(socket)),
        shell_quote(&tmux_static(session)),
    );
    let mut argv = vec![
        "set-option".to_owned(),
        "-goq".to_owned(),
        trust::viewport_options::SERVER_NONCE.to_owned(),
        uuid::Uuid::new_v4().simple().to_string(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-goq".to_owned(),
        "@chief_viewport_generation".to_owned(),
        "0".to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-goq".to_owned(),
        trust::viewport_options::MEMBERSHIP_GENERATION.to_owned(),
        "0".to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-goq".to_owned(),
        trust::viewport_options::TOPOLOGY_GENERATION.to_owned(),
        "0".to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-gF".to_owned(),
        trust::viewport_options::TOPOLOGY_GENERATION.to_owned(),
        format!("#{{e|+:#{{{}}},1}}", trust::viewport_options::TOPOLOGY_GENERATION),
        ";".to_owned(),
        "set-option".to_owned(),
        "-F".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        trust::viewport_options::TOPOLOGY_EPOCH.to_owned(),
        format!("#{{{}}}", trust::viewport_options::TOPOLOGY_GENERATION),
        ";".to_owned(),
        "set-option".to_owned(),
        "-gu".to_owned(),
        trust::viewport_options::FAST_SESSION.to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-gu".to_owned(),
        trust::viewport_options::FAST_OWNER.to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-gu".to_owned(),
        trust::viewport_options::FAST_ORGANIZATION.to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-gu".to_owned(),
        trust::viewport_options::FAST_GENERATION.to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-qu".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        "@chief_viewport_generation".to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-F".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        trust::viewport_options::REFRESH_COMMAND.to_owned(),
        refresh_command,
        ";".to_owned(),
        "set-option".to_owned(),
        "-F".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        trust::viewport_options::WIDTH_COMMAND.to_owned(),
        width_command,
        ";".to_owned(),
        "set-hook".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        "client-resized".to_owned(),
        viewport_hook_action(
            executable,
            socket,
            session,
            &[],
            sidebar::brain::RAIL_DEFAULT_COLUMNS,
            false,
            0,
        ),
        ";".to_owned(),
    ];
    for (index, (event, command)) in
        [("client-attached", attached.as_str()), ("client-detached", detached.as_str())]
            .into_iter()
            .enumerate()
    {
        if index > 0 {
            argv.push(";".to_owned());
        }
        argv.extend([
            "set-hook".to_owned(),
            "-t".to_owned(),
            session.to_owned(),
            event.to_owned(),
            command.to_owned(),
        ]);
    }
    argv.extend([
        ";".to_owned(),
        "set-hook".to_owned(),
        "-g".to_owned(),
        "client-session-changed".to_owned(),
        session_changed,
        ";".to_owned(),
        "if-shell".to_owned(),
        "-F".to_owned(),
        "1".to_owned(),
        membership,
        ";".to_owned(),
        "display-message".to_owned(),
        "-p".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        format!(
            "#{{{}}}\t#{{{}}}\t#{{{}}}",
            trust::viewport_options::TOPOLOGY_EPOCH,
            trust::viewport_options::SERVER_NONCE,
            trust::tags::ORGANIZATION,
        ),
    ]);
    argv
}

fn viewport_bootstrap_authority(output: &str) -> Result<(u64, String, String)> {
    let mut fields = output.trim().split('\t');
    let generation = fields
        .next()
        .unwrap_or_default()
        .parse::<u64>()
        .map_err(|_| LifecycleError::host("chief attach did not receive a topology epoch"))?;
    let nonce = fields.next().unwrap_or_default();
    let organization = fields.next().unwrap_or_default();
    if fields.next().is_some()
        || !trust::is_safe_server_nonce(nonce)
        || !trust::is_safe_logical_id(organization)
    {
        return Err(LifecycleError::host("chief attach did not receive a server nonce"));
    }
    Ok((generation, nonce.to_owned(), organization.to_owned()))
}

/// The most tmux will carry, with room for the `if-shell` authority fence the
/// install wraps this argv in.
///
/// tmux packs a client's whole argv into ONE `imsg`, so `MAX_IMSGSIZE` is a
/// hard ceiling on the command and not on any one word. Measured on a live box
/// against tmux 3.3a: 16300 bytes accepted, 16350 answered `failed to send
/// command`, 17000 answered **`command too long`** — which is verbatim what
/// `chief` printed at the operator instead of showing them their company:
///
/// ```text
/// root@host:~/workspace# chief
/// chief attach could not install the viewport hook set: command too long
/// ```
///
/// A ceiling is not a substitute for the hook being small — [`viewport_hook_action`]
/// is O(windows) for that reason — it is the backstop that makes the size of a
/// company incapable of stopping it from starting. Over the ceiling the hook is
/// rebuilt WITHOUT its manifest, which costs the fast in-tmux resize path and
/// keeps the asynchronous callback that has always been the correct authority.
const VIEWPORT_HOOK_MAX_BYTES: usize = 12 * 1024;

/// Build the hook install argv, and never let a company's size make it
/// unsendable.
fn viewport_resize_hook_refresh_argv(
    executable: &Path,
    socket: &str,
    session: &str,
    manifest: &[ViewportManifestWindow],
    expanded_columns: i64,
    collapsed: bool,
    topology_generation: u64,
) -> Vec<String> {
    let argv = viewport_resize_hook_argv_for(
        executable,
        socket,
        session,
        manifest,
        expanded_columns,
        collapsed,
        topology_generation,
    );
    if tmux_command_string(&argv).len() <= VIEWPORT_HOOK_MAX_BYTES {
        return argv;
    }
    tracing::warn!(
        event = "viewport.hook.manifest-dropped",
        session,
        windows = manifest.len(),
        panes = manifest.iter().map(|entry| entry.panes.len()).sum::<usize>(),
        limit = VIEWPORT_HOOK_MAX_BYTES,
        "this company's viewport manifest will not fit in one tmux command, so the hook is \
         installed without its in-tmux fast path; resizes still publish through the callback"
    );
    viewport_resize_hook_argv_for(
        executable,
        socket,
        session,
        &[],
        expanded_columns,
        collapsed,
        topology_generation,
    )
}

fn viewport_resize_hook_argv_for(
    executable: &Path,
    socket: &str,
    session: &str,
    manifest: &[ViewportManifestWindow],
    expanded_columns: i64,
    collapsed: bool,
    topology_generation: u64,
) -> Vec<String> {
    vec![
        "set-option".to_owned(),
        "-F".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        trust::viewport_options::WIDTH_COMMAND.to_owned(),
        // THE DRAG COMMAND CARRIES NO EPOCH, and it cannot: whatever is
        // written here is frozen, because `set-option -F` expands every
        // `#{...}` in the value AT SET TIME. #1196 changed a literal
        // generation number into `#{q:@chief_viewport_topology_epoch}`
        // believing tmux would expand it when the binding fired. It does not —
        // and `run-shell` does not re-expand a format that arrives through an
        // option substitution either, so the stored string held a frozen
        // number either way and that change was a no-op. Measured on
        // a live box, 2026-08-21, with the #1196 binary installed: the command
        // carried `25` against a live epoch of `26`, every drag was refused as
        // `stale`, `@chief_sidebar_columns` stayed unset and the next layout
        // laid the rail back at the default 26. The operator's words: *"every
        // time I resize the sidebar it resizes it back"*.
        //
        // So the epoch is not passed at all. The verb mints its own — see
        // `mint_width_epoch` — and the identity guards that DO belong to a
        // drag ride along here unchanged: the organization, the session
        // lifetime, and the server nonce.
        format!(
            "{} viewport-sidebar-width {} {} #{{q:@organization_id}} #{{q:session_id}} \
             #{{q:@chief_viewport_server_nonce}}",
            shell_quote(&tmux_static(&executable.display().to_string())),
            shell_quote(&tmux_static(socket)),
            shell_quote(&tmux_static(session)),
        ),
        ";".to_owned(),
        "set-option".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        trust::viewport_options::MANIFEST_EPOCH.to_owned(),
        topology_generation.to_string(),
        ";".to_owned(),
        "set-hook".to_owned(),
        "-t".to_owned(),
        session.to_owned(),
        "client-resized".to_owned(),
        viewport_hook_action(
            executable,
            socket,
            session,
            manifest,
            expanded_columns,
            collapsed,
            topology_generation,
        ),
    ]
}

fn install_viewport_manifest_if_current(
    executable: &Path,
    socket: &str,
    session: &str,
    authority: (&str, &str, u64),
    manifest: &[ViewportManifestWindow],
    rail_state: (i64, bool),
) -> Result<bool> {
    let (organization, server_nonce, topology_generation) = authority;
    let (expanded_columns, collapsed) = rail_state;
    if !trust::is_safe_company_session(session)
        || !trust::is_safe_logical_id(organization)
        || !trust::is_safe_server_nonce(server_nonce)
    {
        return Err(LifecycleError::host("viewport manifest install authority is not safe"));
    }
    let hook_argv = viewport_resize_hook_refresh_argv(
        executable,
        socket,
        session,
        manifest,
        expanded_columns,
        collapsed,
        topology_generation,
    );
    let hook_command = format!("{} ; display-message -p applied", tmux_command_string(&hook_argv));
    let predicate = format!(
        "#{{&&:#{{==:#{{{}}},{topology_generation}}},\
         #{{&&:#{{==:#{{{}}},{organization}}},#{{==:#{{{}}},{server_nonce}}}}}}}",
        trust::viewport_options::TOPOLOGY_EPOCH,
        trust::tags::ORGANIZATION,
        trust::viewport_options::SERVER_NONCE,
    );
    let installed = tmux::run(
        socket,
        &["if-shell", "-F", "-t", session, &predicate, &hook_command, "display-message -p stale"],
    );
    if !installed.ok() {
        return Err(LifecycleError::host(format!(
            "chief attach could not install the viewport hook set: {}",
            installed.diagnostic()
        )));
    }
    match installed.stdout.trim() {
        "applied" => Ok(true),
        "stale" => Ok(false),
        marker => Err(LifecycleError::host(format!(
            "chief attach received an invalid viewport install marker: {marker}"
        ))),
    }
}

pub(crate) fn refresh_viewport_manifest(
    socket: &str,
    session: &str,
    expected_generation: &str,
    server_nonce: &str,
) -> Result<()> {
    if !trust::is_safe_company_session(session) {
        return Err(LifecycleError::host("viewport manifest target is not a company session"));
    }
    let generation = expected_generation
        .parse::<u64>()
        .map_err(|_| LifecycleError::host("viewport manifest generation must be numeric"))?;
    if server_nonce.len() != 32 || !server_nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LifecycleError::host("viewport server nonce is not safe"));
    }
    let executable = std::env::current_exe()
        .map_err(|error| LifecycleError::host(format!("cannot locate Chief: {error}")))?;
    let organization =
        tmux::run(socket, &["show-options", "-qv", "-t", session, trust::tags::ORGANIZATION]);
    let organization = organization.stdout.trim();
    if organization.is_empty()
        || !organization
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(LifecycleError::host("viewport manifest target has no safe organization tag"));
    }
    let recorded =
        tmux::run(socket, &["show-options", "-qv", "-t", session, trust::sidebar_options::COLUMNS]);
    let collapsed = tmux::run(
        socket,
        &["show-options", "-qv", "-t", session, trust::sidebar_options::COLLAPSED],
    );
    let expanded_columns = entry_rail_columns(&recorded.stdout, "0");
    let is_collapsed = collapsed.stdout.trim() == "1";
    let listed = tmux::run(
        socket,
        &[
            "list-panes",
            "-s",
            "-t",
            session,
            "-F",
            "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
        ],
    );
    if !listed.ok() {
        return Err(LifecycleError::host(listed.diagnostic()));
    }
    // The sentence keeps its opening words — they are what a reader greps for
    // and what every earlier report of this refusal quotes — and gains the one
    // fact it never carried: which window it read.
    let manifest = match viewport_manifest_survey(&listed.stdout) {
        Ok(manifest) => manifest,
        Err(reason) => {
            return Err(LifecycleError::host(format!(
                "viewport manifest requires one tagged rail in every managed window: {reason}"
            )))
        }
    };
    let argv = viewport_resize_hook_refresh_argv(
        &executable,
        socket,
        session,
        &manifest,
        expanded_columns,
        is_collapsed,
        generation,
    );
    let command = format!("{} ; display-message -p applied", tmux_command_string(&argv));
    let predicate = format!(
        "#{{&&:#{{==:#{{{}}},{generation}}},#{{&&:#{{==:#{{{}}},{organization}}},\
         #{{==:#{{{}}},{server_nonce}}}}}}}",
        trust::viewport_options::TOPOLOGY_EPOCH,
        trust::tags::ORGANIZATION,
        trust::viewport_options::SERVER_NONCE,
    );
    let installed = tmux::run(
        socket,
        &["if-shell", "-F", "-t", session, &predicate, &command, "display-message -p stale"],
    );
    if !installed.ok() {
        return Err(LifecycleError::host(installed.diagnostic()));
    }
    if !matches!(installed.stdout.trim(), "applied" | "stale") {
        return Err(LifecycleError::host("viewport manifest refresh returned no CAS marker"));
    }
    Ok(())
}

fn safe_tmux_object_id(value: &str, prefix: char) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Mint one fresh topology epoch for a rail drag, guarded ONLY by identity.
///
/// The drag commit does not act on the topology: it records a number and
/// resizes the rails it can see right now. So the only questions it may ask
/// are "is this still the same company, in the same session lifetime, on the
/// same tmux server?" — and the answer is `@organization_id`, `session_id` and
/// `@chief_viewport_server_nonce`, all three of which stay.
///
/// It asked two more, and both refused real drags:
///
/// * that the epoch the EVENT carried still equalled the live epoch. The event
///   epoch was frozen into `@chief_viewport_width_command` by `set-option -F`,
///   so it went stale the moment anything touched the topology.
/// * that `@chief_viewport_manifest_epoch` had caught up with
///   `@chief_viewport_topology_epoch`. The manifest refresh is fired
///   `run-shell -b`, so a company mid-pass fails that equality for as long as
///   the callback takes — and `park_focus_window_if_still_empty` bumps the
///   topology epoch with no refresh at all, which leaves it unequal until the
///   next pass.
///
/// Neither is needed for safety. The freshly minted epoch is what fences the
/// mutation ([`apply_sidebar_width_if_current`] CASes on it and re-surveys
/// every window and pane), and anything holding an older epoch — including an
/// in-flight manifest refresh — loses its own CAS against the new one. The
/// commit then re-installs the manifest at the epoch it minted, so a drag
/// REPAIRS a lagging manifest instead of being refused by it.
fn mint_width_epoch(
    socket: &str,
    session: &str,
    organization: &str,
    session_id: &str,
    server_nonce: &str,
) -> Result<String> {
    let predicate = format!(
        "#{{&&:#{{==:#{{@organization_id}},{organization}}},\
         #{{&&:#{{==:#{{session_id}},{session_id}}},\
         #{{==:#{{@chief_viewport_server_nonce}},{server_nonce}}}}}}}"
    );
    let target = shell_quote(session);
    let mint = format!(
        "set-option -goq @chief_viewport_topology_generation 0 ; \
         set-option -gF @chief_viewport_topology_generation \
         '#{{e|+:#{{@chief_viewport_topology_generation}},1}}' ; \
         set-option -F -t {target} @chief_viewport_topology_epoch \
         '#{{@chief_viewport_topology_generation}}' ; \
         display-message -p -t {target} '#{{@chief_viewport_topology_epoch}}'"
    );
    let result = tmux::run(
        socket,
        &["if-shell", "-F", "-t", session, &predicate, &mint, "display-message -p stale"],
    );
    if !result.ok() {
        return Err(LifecycleError::host(result.diagnostic()));
    }
    let marker = result.stdout.trim();
    if marker == "stale" {
        return Err(LifecycleError::host(
            "sidebar width event no longer belongs to the same company session",
        ));
    }
    if marker.parse::<u64>().is_err() {
        return Err(LifecycleError::host("sidebar width received no topology epoch"));
    }
    Ok(marker.to_owned())
}

fn apply_sidebar_width_if_current(
    socket: &str,
    session: &str,
    authority: (&str, &str, &str, &str),
    columns: i64,
    listed: &str,
) -> Result<&'static str> {
    let (organization, session_id, generation, server_nonce) = authority;
    let mut windows = std::collections::BTreeMap::<String, usize>::new();
    let mut rail_counts = std::collections::BTreeMap::<String, usize>::new();
    let mut rails = Vec::<(String, String)>::new();
    for line in listed.lines() {
        let mut fields = line.split('\t');
        let window = fields.next().unwrap_or_default().trim();
        let pane = fields.next().unwrap_or_default().trim();
        let sidebar = fields.next().unwrap_or_default().trim();
        if !safe_tmux_object_id(window, '@')
            || !safe_tmux_object_id(pane, '%')
            || !matches!(sidebar, "" | "1")
        {
            return Err(LifecycleError::host("sidebar width survey is not safe tmux truth"));
        }
        *windows.entry(window.to_owned()).or_default() += 1;
        if sidebar == "1" {
            *rail_counts.entry(window.to_owned()).or_default() += 1;
            rails.push((window.to_owned(), pane.to_owned()));
        }
    }
    if windows.is_empty()
        || windows.keys().any(|window| rail_counts.get(window).copied().unwrap_or_default() != 1)
    {
        return Err(LifecycleError::host(
            "sidebar width requires exactly one tagged rail in every window",
        ));
    }
    let target = shell_quote(session);
    let proof = "@chief_sidebar_width_preflight";
    let earn = format!("set-option -F -t {target} {proof} '#{{e|+:#{{{proof}}},1}}'");
    let mut batch = vec![format!("set-option -t {target} {proof} 0")];
    let mut expected = 0usize;
    for (window, panes) in &windows {
        expected += 1;
        let predicate = format!(
            "#{{&&:#{{==:#{{window_id}},{}}},#{{==:#{{window_panes}},{panes}}}}}",
            tmux_static(window)
        );
        batch.push(format!(
            "if-shell -F -t {} {} {} ''",
            shell_quote(window),
            shell_quote(&predicate),
            shell_quote(&earn),
        ));
    }
    for (window, rail) in &rails {
        expected += 1;
        let pane_static = tmux_static(rail);
        let sidebar_tag = "1";
        let predicate = format!(
            "#{{&&:#{{==:#{{pane_id}},{pane_static}}},#{{&&:#{{==:#{{window_id}},{}}},#{{==:#{{@organization_sidebar}},{sidebar_tag}}}}}}}",
            tmux_static(window)
        );
        batch.push(format!(
            "if-shell -F -t {} {} {} ''",
            shell_quote(rail),
            shell_quote(&predicate),
            shell_quote(&earn),
        ));
    }
    let mut mutation = format!("set-option -t {target} @chief_sidebar_columns {columns}");
    for (_, rail) in &rails {
        mutation.push_str(&format!(" ; resize-pane -x {columns} -t {}", shell_quote(rail)));
    }
    mutation.push_str(" ; display-message -p applied");
    batch.push(format!(
        "if-shell -F -t {target} '#{{==:#{{{proof}}},{expected}}}' {} \
         'display-message -p invalid'",
        shell_quote(&mutation),
    ));
    batch.push(format!("set-option -qu -t {target} {proof}"));
    let predicate = format!(
        "#{{&&:#{{==:#{{@organization_id}},{organization}}},\
         #{{&&:#{{==:#{{session_id}},{session_id}}},\
         #{{&&:#{{==:#{{@chief_viewport_topology_epoch}},{generation}}},\
         #{{==:#{{@chief_viewport_server_nonce}},{server_nonce}}}}}}}}}"
    );
    let result = tmux::run(
        socket,
        &[
            "if-shell",
            "-F",
            "-t",
            session,
            &predicate,
            &batch.join(" ; "),
            "display-message -p stale",
        ],
    );
    if !result.ok() {
        return Err(LifecycleError::host(result.diagnostic()));
    }
    match result.stdout.trim() {
        "applied" => Ok("applied"),
        "stale" => Ok("stale"),
        "invalid" => Ok("invalid"),
        _ => Err(LifecycleError::host("sidebar width CAS returned no marker")),
    }
}

/// Commit one explicit rail-border drag as a complete company-width batch.
///
/// THE OPERATOR'S DRAG IS THE AUTHORITY, and the only thing this may refuse is
/// somebody else's company. It takes no epoch from its caller: see
/// [`mint_width_epoch`] for the two epoch equalities that used to gate it and
/// why neither belonged here.
pub(crate) fn release_sidebar_width(
    socket: &str,
    session: &str,
    organization: &str,
    session_id: &str,
    server_nonce: &str,
    columns: &str,
) -> Result<()> {
    if !trust::is_safe_company_session(session) {
        return Err(LifecycleError::host("sidebar width target is not a company session"));
    }
    if !trust::is_safe_logical_id(organization)
        || !safe_tmux_object_id(session_id, '$')
        || server_nonce.len() != 32
        || !server_nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(LifecycleError::host("sidebar width event authority is not safe"));
    }
    let columns = columns
        .parse::<i64>()
        .map_err(|_| LifecycleError::host("sidebar width must be numeric"))?;
    if columns < sidebar::brain::RAIL_MIN_READABLE_COLUMNS {
        return Err(LifecycleError::host("sidebar width is below the readable minimum"));
    }
    for _ in 0..WIDTH_RELEASE_ATTEMPTS {
        let generation = mint_width_epoch(socket, session, organization, session_id, server_nonce)?;
        let listed = tmux::run(
            socket,
            &[
                "list-panes",
                "-s",
                "-t",
                session,
                "-F",
                &format!("#{{window_id}}\t#{{pane_id}}\t#{{{}}}", trust::tags::SIDEBAR),
            ],
        );
        let applied = if listed.ok() {
            apply_sidebar_width_if_current(
                socket,
                session,
                (organization, session_id, &generation, server_nonce),
                columns,
                &listed.stdout,
            )
        } else {
            Ok("invalid")
        };
        let _ = refresh_viewport_manifest(socket, session, &generation, server_nonce);
        if applied? == "applied" {
            return Ok(());
        }
    }
    Err(LifecycleError::host("sidebar width could not serialize with company topology"))
}

/// How many times a drag re-mints and re-surveys before it gives up.
///
/// Only a company whose panes move under every single survey exhausts this;
/// identity is a first-attempt refusal, not a retry.
const WIDTH_RELEASE_ATTEMPTS: usize = 3;

const ATTACH_VIEWPORT_ATTEMPTS: usize = 3;

fn acquire_attach_viewport_authority(
    executable: &Path,
    socket: &str,
    session: &str,
) -> Result<(u64, String, String)> {
    let bootstrap = viewport_hook_bootstrap_argv(executable, socket, session);
    let refs: Vec<&str> = bootstrap.iter().map(String::as_str).collect();
    let bootstrapped = tmux::run(socket, &refs);
    if !bootstrapped.ok() {
        return Err(LifecycleError::host(format!(
            "chief attach could not install viewport lifecycle authority: {}",
            bootstrapped.diagnostic()
        )));
    }
    viewport_bootstrap_authority(&bootstrapped.stdout)
}

fn publish_attach_viewport_once(
    executable: &Path,
    socket: &str,
    session: &str,
    authority: &(u64, String, String),
    viewport: (u32, u32),
    rail_state: (i64, bool),
) -> Result<bool> {
    let (topology_generation, server_nonce, organization) = authority;
    let executor = chief_cli::real::RealHostExecutor::production();
    match chief_cli::actuate::resize_session_viewport_for_attach(
        &executor,
        &chief_cli::actuate::Socket(socket.to_owned()),
        session,
        organization,
        *topology_generation,
        server_nonce,
        viewport,
    )
    .map_err(LifecycleError::host)?
    {
        chief_cli::actuate::AttachViewportPublication::Applied(_) => {}
        chief_cli::actuate::AttachViewportPublication::Stale => return Ok(false),
    }

    let manifest = tmux::run(
        socket,
        &[
            "list-panes",
            "-s",
            "-t",
            session,
            "-F",
            "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
        ],
    );
    let manifest = if manifest.ok() {
        viewport_manifest_survey(&manifest.stdout).unwrap_or_default()
    } else {
        Vec::new()
    };
    install_viewport_manifest_if_current(
        executable,
        socket,
        session,
        (organization, server_nonce, *topology_generation),
        &manifest,
        rail_state,
    )
}

fn retry_attach_viewport<Authority>(
    mut acquire: impl FnMut() -> Result<Authority>,
    mut publish: impl FnMut(&Authority) -> Result<bool>,
) -> Result<()> {
    for _ in 0..ATTACH_VIEWPORT_ATTEMPTS {
        let authority = acquire()?;
        if publish(&authority)? {
            return Ok(());
        }
    }
    Err(LifecycleError::host(format!(
        "chief attach viewport authority stayed stale across {ATTACH_VIEWPORT_ATTEMPTS} attempts"
    )))
}

pub(crate) fn enter_company_session(
    socket: &str,
    session: &str,
    dir: &Path,
    door: &'static str,
) -> Result<()> {
    // FIRST, before anything can fail. This line is what makes an absent rail
    // diagnosable: a company with no rail and no `entering` line proves the
    // door was never opened, and an `entering` with no `minted` proves it was
    // and the lines below say why. Distinguishing those two cost a live ssh
    // session, which is the whole reason it is written here and not later.
    tracing::info!(
        event = "sidebar.rails.entering",
        organization = %dir.display(),
        session,
        door,
        "entering a company session; ensuring every window has its sidebar rail"
    );
    let executable = std::env::current_exe().map_err(|error| {
        LifecycleError::refused(format!(
            "chief: cannot locate its own executable, so it cannot start the sidebar ({error})"
        ))
    })?;
    // Static lifecycle authority exists before any rail or sidebar can change
    // topology. Only the literal resized manifest is generation-fenced below.
    let (topology_generation, server_nonce, organization) =
        acquire_attach_viewport_authority(&executable, socket, session)?;
    let listed = tmux::run(
        socket,
        &[
            "list-panes",
            "-s",
            "-t",
            session,
            "-F",
            &format!("#{{window_id}}\t#{{{}}}", trust::tags::SIDEBAR),
        ],
    );
    if !listed.ok() {
        tracing::warn!(
            event = "sidebar.rails.unreadable",
            organization = %dir.display(),
            session,
            detail = %listed.diagnostic(),
            "tmux would not list this session's panes, so no rail could be placed"
        );
        return Ok(());
    }
    let (railed, windows) = rail_survey(&listed.stdout);
    if windows.is_empty() && railed.is_empty() {
        tracing::warn!(
            event = "sidebar.rails.none",
            organization = %dir.display(),
            session,
            "this session listed no windows at all, so there is nowhere to put a rail"
        );
        return Ok(());
    }
    // Resolve both human-owned preferences before any rail process starts.
    // Attach consumes them and never writes them.
    let recorded = tmux::run(
        socket,
        &["show-options", "-q", "-v", "-t", session, trust::sidebar_options::COLUMNS],
    );
    let collapsed = tmux::run(
        socket,
        &["show-options", "-q", "-v", "-t", session, trust::sidebar_options::COLLAPSED],
    );
    let expanded_columns = entry_rail_columns(&recorded.stdout, "0");
    let is_collapsed = collapsed.stdout.trim() == "1";
    let columns =
        if is_collapsed { chief_cli::layout::RAIL_COLLAPSED_COLUMNS } else { expanded_columns };

    for window in windows {
        if railed.contains(window) {
            tracing::debug!(
                event = "sidebar.rails.present",
                organization = %dir.display(),
                session,
                window,
                "this window already carries a rail; leaving it alone"
            );
            continue;
        }
        // `-b` puts the new pane BEFORE the target, which with `-h` means to
        // its left. The rail's complete birth is one tmux frame; see the
        // builder for why no write may follow in a second invocation.
        let argv = rail_mint_argv(session, window, dir, &executable, columns);
        let minted = match run_attach_mutation_if_current(
            socket,
            session,
            &organization,
            &server_nonce,
            topology_generation,
            &argv,
        ) {
            Ok(minted) => minted,
            Err(error) => {
                // FURNITURE IS NOT THE COMPANY. A rail that will not mint is a
                // window without a sidebar, which the operator can see past; a
                // refused attach is a company they cannot see at all.
                tracing::warn!(
                    event = "sidebar.rails.unminted",
                    organization = %dir.display(),
                    session,
                    window,
                    detail = %error,
                    "this window's rail could not be placed; the company is unaffected"
                );
                continue;
            }
        };
        let pane_id = minted.stdout.trim();
        if pane_id.is_empty() || !minted.ok() {
            // A window that will not split is a window too narrow to hold a
            // rail AND its people. That is not a reason to refuse the attach —
            // the operator asked to see their company, and the company is
            // there. It IS a reason to say so: the operator experiences this as
            // a missing feature, so it is a warning and it names the window.
            tracing::warn!(
                event = "sidebar.rails.declined",
                organization = %dir.display(),
                session,
                window,
                detail = %minted.diagnostic(),
                "this window would not split, so it has no rail; the company is unaffected"
            );
            continue;
        }
        tracing::info!(
            event = "sidebar.rails.minted",
            organization = %dir.display(),
            session,
            window,
            pane = pane_id,
            columns,
            "placed a tagged sidebar rail at its final width in one tmux frame"
        );
    }

    // NOTHING BELOW THIS LINE MAY REFUSE THE ATTACH.
    //
    // The operator's ruling, after a hard reboot left their company unstartable
    // twice in a row: *"we cannot have such a setup. Users don't understand
    // this shit. they just want to stand up their company."* Both times `chief`
    // printed tmux's own words and exited — `have 6 panes but need 5: 6a8a,…`,
    // then `chief attach could not install the viewport hook set: command too
    // long`. Neither sentence is about the company; both are about GEOMETRY, and
    // geometry is what the next converge pass and the next client resize fix by
    // themselves.
    //
    // The root causes of those two are fixed where they live — the publication
    // carries its own pane census, and the hook cannot outgrow tmux's command
    // ceiling. This is the standing rule that makes the NEXT member of that
    // family a warning instead of a company nobody can start.
    let published = (|| -> Result<()> {
        let Some((viewport_columns, viewport_rows)) = super::terminal::operator_size() else {
            return Err(LifecycleError::host(
                "chief attach could not read the operator terminal size before publication"
                    .to_owned(),
            ));
        };
        let viewport = (u32::from(viewport_columns), u32::from(viewport_rows));
        retry_attach_viewport(
            || acquire_attach_viewport_authority(&executable, socket, session),
            |authority| {
                publish_attach_viewport_once(
                    &executable,
                    socket,
                    session,
                    authority,
                    viewport,
                    (expanded_columns, is_collapsed),
                )
            },
        )
    })();
    if let Err(error) = published {
        tracing::warn!(
            event = "sidebar.viewport.unpublished",
            organization = %dir.display(),
            session,
            detail = %error,
            "the operator viewport could not be published; the company is up and converge lays \
             its windows out on the next pass"
        );
    }
    if let Err(error) = leave_session_pane_modes(socket, session) {
        tracing::warn!(
            event = "sidebar.copy-mode.unleft",
            organization = %dir.display(),
            session,
            detail = %error,
            "a pane would not leave copy mode before handoff"
        );
    }
    Ok(())
}

/// Abandon a tmux session `chief` cannot reconcile, so the company can be stood
/// up again from the CEO alone.
///
/// THE RULE THIS IMPLEMENTS, in the operator's words: *"If you get a mismatch
/// like that, just boot just `@chief`. we cannot just die like that."*
///
/// Killing a session destroys PANES and never a row. Every durable fact — the
/// roster, every person's transcript under `.chief/agent/`, the department tree
/// — lives in `.chief/db/chief.db` and is untouched, so the company that comes
/// back is the same company: the CEO first, and the actuator converging
/// everybody else behind them. That is also why the CEO's boot standing is
/// unchanged by this. [`crate::actuate::spawn_cmd::BootStanding::from_company`]
/// answers `Founding` only for a company with one person and no transcript, and
/// this leaves both of those facts exactly as it found them — so a rebooted
/// company's CEO comes up `Established`, with work to continue, rather than
/// being told it was created moments ago.
///
/// Returns whether tmux confirmed the session is gone. A socket that will not
/// answer at all is reported to the caller rather than assumed away.
pub(crate) fn abandon_unreconcilable_session(socket: &str, session: &str) -> bool {
    let killed = tmux::run(socket, &["kill-session", "-t", session]);
    tracing::warn!(
        event = "attach.session.abandoned",
        session,
        ok = killed.ok(),
        detail = %killed.diagnostic(),
        "this tmux session could not be reconciled, so it is abandoned and the company is stood \
         up again from the CEO alone"
    );
    matches!(tmux::session_exists(socket, session), Some(false))
}

/// Make a company RUN, and do not return until its own tmux session is there.
///
/// THE ACTUATOR IS THE WHOLE OF IT. Nobody states an intent here: the desired
/// set a company comes back to already exists in the store, because an omitted
/// launch intent is an empty allow-list that admits the root head alone and the
/// root holds an unconditional organization-root lease. So an attach has
/// exactly two jobs — put somebody on the socket who can converge that set, and
/// refuse if the company never appears.
///
/// TOMBSTONE (chief-home-is-cwd §4c): the `prepare_ceo_only` call that stood
/// between these two lines, and the ordering law it needed. Intent stated while
/// nobody was actuating committed durably and then never converged — the caller
/// got a healthy daemon, a 200, an encouraging line of output and no company —
/// so the order (actuator first, intent second, wait third) was the product.
/// Measured on a live host, three fresh companies:
///
/// ```text
/// prepare-ceo-only with no actuator  -> prepared:true, desiredActive false
/// ...5s later                        -> false
/// ...25s later                       -> false      (time changes nothing)
/// start an actuator, prepare nothing -> false      (presence alone too)
/// prepare-ceo-only with the actuator -> desiredActive TRUE, CEO pane in 6s
/// ```
///
/// That table is kept because it is the evidence for what replaced it: row 4
/// says presence alone did not converge the CEO, and it is the row that no
/// longer holds. The intent it was waiting for is not stated late any more, it
/// is not stated at all — the store's fail-safe carries it — so an actuator
/// that comes up finds the CEO already desired and there is no window in which
/// a lost write can hide.
///
/// # Why this is a function and not three lines inside `run`
///
/// Because there are two front doors into a running company, not one, and both
/// must take this exact order. `chief attach` is the second; the FIRST is a
/// Founder who has just created one — `founder::launch_route` handed the
/// operator over to a session nobody had minted, got `can't find session`, and
/// told them to go and type `chief actuate` plus `chief attach` by hand. A
/// copy of this sequence written there would be a second place for the order to
/// be got wrong, over a mistake whose whole signature is that it looks like it
/// worked.
///
/// The sentences the failures carry name `chief attach` because that is the
/// work being done, whoever asked for it, and because the recovery is the same
/// from either door: run the resident verb yourself and then attach.
///
/// # Errors
/// [`LifecycleError`] naming the refusal and the operator's next move.
/// TOMBSTONE (chief-home-is-cwd §4c): the `company: &CompanyClient` parameter.
/// It existed for the one HTTP call this function made, and there is no longer
/// any: bringing a company up is now entirely a tmux operation against the
/// socket, so nothing here needs a daemon to talk to. Callers still hold a
/// client — they read the manifest and the recorded socket through it — they
/// just no longer hand it down.
pub(crate) async fn bring_up(
    dir: &Path,
    socket: &str,
    company_session: &str,
    command: &[String],
) -> Result<()> {
    ensure_actuator(dir, socket, company_session, command).await?;

    // TOMBSTONE (chief-home-is-cwd §4c): the `prepare_ceo_only` call that stood
    // here, and before it the `if !prepared.prepared` refusal that read
    // chiefd's verdict on whether the committed intent would converge.
    //
    // Neither question survives, and neither is LOST -- both are asked of the
    // machine, on either side of where they used to sit. `ensure_actuator`
    // ABOVE proves a live actuator window on this socket, which is ground truth
    // and a stronger fact than the lease the verdict was derived from.
    // `await_company_session` BELOW refuses, by name, if the company never
    // comes up. Between them there is nothing left for a durable write to add.
    //
    // The per-person materialization warnings this printed go with the route:
    // they were collected while the DAEMON brought the pane up, and the daemon
    // brings none up. A failure to materialize somebody now shows up where the
    // work is done -- in the actuator's own pane -- rather than as a line this
    // client relays from a report it did not produce.
    await_company_session(dir, socket, company_session).await
}

/// Make sure somebody is actuating this company, starting one if nobody is.
///
/// # The defect this closes
///
/// Since the actuation switchover chiefd publishes actions and a CLIENT applies
/// them, so stating CEO-only intent no longer makes a CEO. `876e4b545` made
/// attach ask who was actuating and refuse by name — honest, but it left the
/// operator holding a product whose front door tells them to go and type a
/// different verb. There is now one path from nothing to a running company, and
/// it is `chief attach <company>`.
///
/// # At most one actuator, ever
///
/// Two actuators for one company is a second source of truth about what should
/// be running, which is the defect shape that has cost this codebase more than
/// any other. Three things enforce one:
///
/// 1. A live actuator session on the socket is never respawned into.
/// 2. A tmux read that does not answer is [`ActuatorSession::Unknown`] and
///    fails closed, because "I could not tell" must never start a duplicate.
///
/// There used to be a third, consulted FIRST: chiefd's own presence answer,
/// which covered the one case tmux cannot see — an operator's own `chiefd
/// actuate` running in a bare terminal rather than in the actuator session.
/// Presence was a lease derived from a host report the actuator committed
/// upward, and that direction is closed, so the gate is gone with it.
///
/// **NAMED, ACCEPTED LOSS.** A hand-run `chief actuate <slug>` outside the
/// actuator session is invisible here, and this function will start a second
/// actuator beside it. What survives is the layer that always did the real
/// work: every destructive step re-verifies the pane's ownership tags
/// immediately before acting (`actuate::interpret`), so a duplicate actuator
/// loses its steps to preconditions rather than tearing down live panes, and
/// both are converging to the SAME published desired set rather than to two
/// opinions of their own. Recovering the gate itself would mean an actuator
/// announcing its presence to chiefd, which is the direction this change
/// exists to close.
///
/// `command` is the actuator's argv, resolved by [`resolve_actuator_command`]
/// before anything is started. It is a parameter rather than a constant so the
/// suite can place a stand-in process exactly where the actuator goes and
/// exercise THIS function — the presence gate, the at-most-one rule, the wait
/// and the loud failure — rather than a copy of it written for tests.
async fn ensure_actuator(
    dir: &Path,
    socket: &str,
    company_session: &str,
    command: &[String],
) -> Result<()> {
    let session = tmux::actuator_session(socket, &actuator_session_name(company_session));
    if actuator_needed(session) {
        start_actuator(dir, socket, company_session, command)?;
    }
    await_actuator(dir, socket, company_session).await
}

/// Whether this client must run the start path at all.
///
/// # The defect this closes, and how it was measured
///
/// The gate used to be `if !actuator_present()`, and a lease is not a live
/// process: presence was derived from the last committed report against the
/// reader's clock, so it OUTLIVED its holder by up to the lease window. When a
/// previous actuator had exited but its lease had not yet lapsed, this client
/// read `present`, started nobody, stated CEO-only intent that nobody would
/// carry out, and then sat in [`await_company_session`] for the whole
/// `ACTUATOR_BUDGET` before failing with "an actuator holding chiefd's lease,
/// and chiefd is still asking for nobody to run".
///
/// The lease is now DELETED rather than merely outvoted, and the measurement
/// below is why that is an improvement and not a loss: the tmux read it was
/// paired with is the one that was right every time.
///
/// That is not a theory. Measured on a build host over a cold attach, sampling
/// chiefd's own `/v1/org/runtime/actions` beside `pgrep` and the tmux socket
/// every 250ms: **188 consecutive samples — the entire 45s — reported
/// `presence: present` while ZERO `chief actuate` processes existed anywhere
/// on the host and no actuator session existed on the socket.** Two of every
/// five cold attaches burned 45 seconds and then hard-failed. The successful
/// runs differed in exactly one way: the stale lease had already gone `lapsed`,
/// so the old gate happened to open.
///
/// # Why the tmux read is the authority here, and only here
///
/// The lease is not weakened — other consumers depend on it and a lease that
/// expired eagerly would cause the opposite failure, an actuator declared gone
/// while it is mid-report. What changes is who is asked. This client IS the
/// tmux client: it can see whether the actuator's session exists on the socket
/// it is booting, which is ground truth no durable record can hold. So the two
/// answers are required to AGREE before this client concludes somebody is
/// already actuating.
///
/// [`ActuatorSession::Unknown`] runs the start path, which then REFUSES:
/// `start_move(Unknown)` is [`StartMove::FailClosed`], so nothing is started
/// and the operator is told at once that tmux could not be read. "tmux would
/// not answer" is still never evidence of absence — that rule is enforced one
/// layer down, where it always was, rather than by declining to look.
///
/// # How narrow `Unknown` actually is
///
/// Measured, not assumed, and narrower than it reads. TWO preflights stand in
/// front of this arm and both refuse in ~0.01s: a host with no working tmux is
/// refused with "tmux is required", and a caller outside tmux with "ChiefD only
/// runs inside tmux". So reaching `Unknown` needs tmux answering `-V`, the
/// ambient server reachable, and the ACTUATOR-SESSION read specifically
/// failing — a wedged or unreachable server, not a missing binary. Do not
/// over-fear this arm; it is not the common path, and an earlier draft of this
/// comment claimed a broad hazard that the measurement disproved.
///
/// The ordering invariant is untouched: this only decides whether an actuator
/// is STARTED. Nothing here states intent, and `bring_up` still states
/// CEO-only intent strictly after [`await_actuator`] returns — the fix makes
/// the "intent stated while nobody actuates" state harder to reach, never
/// easier, because that is precisely the state the old gate walked into.
#[must_use]
const fn actuator_needed(session: ActuatorSession) -> bool {
    match session {
        // A live window: somebody really is actuating. The wait decides
        // whether they get anywhere.
        ActuatorSession::Running => false,
        // No holder on this socket, or one whose panes have all exited. Start
        // one.
        ActuatorSession::Absent | ActuatorSession::Exited => true,
        // Unreadable tmux. This runs the START PATH, and that is not a
        // reversal of "an unreadable tmux is never evidence of absence" — it
        // is where that rule is actually enforced. `start_move(Unknown)` is
        // `StartMove::FailClosed`, which starts nothing and returns at once
        // naming tmux. So the duplicate actuator this arm used to protect
        // against cannot occur on this path; the guard preventing it lives one
        // layer down and always did.
        //
        // Answering `false` here therefore bought nothing and cost the
        // operator the whole `ACTUATOR_BUDGET`. MEASURED, same setup, only
        // this arm differing:
        //
        //   false → 46.90s, ending in "…chiefd is still asking for nobody to
        //           run … this is chiefd's desired roster, NOT A TMUX FAULT"
        //   true  →  1.75s, ending in "could not read tmux session '…' …
        //           Check the tmux server, then retry."
        //
        // The 27x is the smaller half. The old sentence is not merely unhelpful
        // — it is confident, specific and WRONG, and it aims the reader at
        // chiefd's roster, the one place the answer cannot be, while the real
        // fault is the tmux read this arm just swallowed.
        ActuatorSession::Unknown => true,
    }
}

/// The tmux session that hosts a company's actuator.
///
/// # Why the actuator does NOT live in the company's own session
///
/// It is the obvious placement and the code forbids it. The company session is
/// the actuator's own projection: it mints the session, tags it with the
/// organization id, and owns every window and pane in it. `actuate::observe`
/// never destroys a whole session. An empty organization tag fails closed and
/// is never reap authority.
///
/// Observation, the ownership audit, and the planner all refuse an untagged
/// session. `actuate::interpret` can stop only a session whose organization
/// tag matches. There is no automatic whole-session cleanup path.
///
/// A session minted by `attach` carries no company organization tag, so
/// observation refuses it. The prefix-safe name still
/// prevents tmux's ordinary target lookup from reporting the actuator as the
/// absent company session.
///
/// So the actuator gets its own session, on the company's own socket. Everything
/// wanted from "inside the session" still holds: no daemonization, no invisible
/// background process, visible in `tmux list-sessions`, and `tmux list-panes -a`
/// proves it exists. Its lifetime is the tmux server's rather than the company
/// session's, which is the correct one — re-creating a company session somebody
/// killed is precisely the actuator's job.
///
/// # Why the name is a PREFIX and not a suffix, found by running it
///
/// The first version of this was `<company-session>-actuator`, which reads
/// better and does not work, because **`tmux -t <name>` matches by PREFIX when
/// no session matches exactly**. On a live run of `chief attach attach-proof`
/// the actuator came up in `org-attach-proof-actuator`, its own first
/// `observe` asked tmux for the company session `org-attach-proof`, tmux handed
/// it `org-attach-proof-actuator` — the only session whose name starts with
/// those characters and treated it as the company target. The old observer
/// inferred reap authority from its empty organization tag and killed the
/// actuator session. Current observation has no whole-session destruction
/// path, and the prefix-safe name also prevents the ambiguous target lookup:
///
/// ```text
/// chief attach: nobody is actuating 'attach-proof' — starting one in tmux session 'org-attach-proof-actuator'
/// chief attach: started an actuator for 'attach-proof' … and it has not taken chiefd's lease within 45s. That window printed nothing.
/// ```
///
/// The same prefix rule ran the other way too: attach's own wait asks
/// `session_exists(company_session)`, and while only the actuator session
/// existed tmux would have answered YES for a company with no session at all.
/// Putting the discriminating text FIRST removes both, because a name can only
/// be resolved by prefix to something it is a prefix OF, and the company
/// session name is not a prefix of this one.
#[must_use]
pub(crate) fn actuator_session_name(company_session: &str) -> String {
    format!("chiefd-actuator-{company_session}")
}

/// What attach does about an actuator session that may already hold one.
///
/// Extracted so the whole branch table is provable without a tmux server, and
/// so no arm can be added without a test naming it. Production takes exactly
/// these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartMove {
    /// A live actuator window is already there: touch nothing.
    LeaveItAlone,
    /// A window whose actuator exited: take it down and start a fresh one.
    ReplaceExited,
    /// A LIVE actuator that is not the installed build: take it down and start
    /// a fresh one on the binary that is installed now.
    ///
    /// Distinct from [`StartMove::ReplaceExited`] because the pane is ALIVE.
    /// There is no corpse to quote and no last words to rescue; there is a
    /// working actuator being deliberately replaced, and the operator is told
    /// which build it was and which one it will be.
    ReplaceStale,
    /// Nothing there: start one.
    CreateOne,
    /// tmux would not answer: refuse rather than risk a duplicate.
    FailClosed,
}

/// What the live actuator says about the build it is running.
///
/// Three answers and not two: "not the installed build" and "I could not tell"
/// are different, and collapsing them is how a rule that replaces things starts
/// replacing things it knows nothing about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActuatorBuild {
    /// It is the build installed right now.
    Current,
    /// It is provably a different build.
    Stale,
    /// It did not say, or the question does not apply — an actuator from a
    /// build that predates the tag, one running out of a development tree, or
    /// an install that cannot be read.
    Unknowable,
}

/// How much of a dead actuator's scrollback to read for its last word.
const CORPSE_SCROLLBACK_LINES: usize = 200;

/// What `chief` says when it finds a dead actuator and replaces it.
///
/// Pure so the sentence can be pinned without a tmux server. It names the two
/// facts that survive the corpse — how it exited, and the last thing it said —
/// because the pane is destroyed a line later and nothing else recorded either.
#[must_use]
pub(crate) fn corpse_narration(company: &str, status: Option<i32>, scrollback: &str) -> String {
    let last =
        scrollback.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or_default().trim();
    let died =
        status.map_or_else(|| "an unreadable status".to_owned(), |code| format!("status {code}"));
    if last.is_empty() {
        return format!(
            "chief: the actuator for {company} was dead ({died}) and printed nothing before it \
             went; replacing it"
        );
    }
    format!(
        "chief: the actuator for {company} was dead ({died}) — last line: \"{last}\"; replacing it"
    )
}

/// Is the live actuator on this session the installed build?
///
/// Read from the tag the actuator stamped on its own session — REPORTED, never
/// inferred. The alternative would be finding its pane's pid and asking the
/// operating system what that process is running, which is the check that has
/// no honest macOS arm and, here, would also be asking about a pid this command
/// never minted.
fn actuator_build_check(socket: &str, session_name: &str) -> ActuatorBuild {
    let Some(reported) = tmux::actuator_build(socket, session_name) else {
        return ActuatorBuild::Unknowable;
    };
    let Ok(reported) =
        serde_json::from_str::<host_primitives::rendezvous::ReportedBuild>(&reported)
    else {
        return ActuatorBuild::Unknowable;
    };
    // The actuator IS this program: `resolve_actuator_command` starts it from
    // `current_exe()`. So the build that SHOULD be running is the one this
    // command is running, and the comparison needs no separate install path.
    let Ok(installed) = std::env::current_exe() else { return ActuatorBuild::Unknowable };
    match super::build_identity::check(ACTUATOR_PROGRAM, Some(&reported), &installed) {
        super::build_identity::BuildCheck::Current => ActuatorBuild::Current,
        super::build_identity::BuildCheck::Stale { .. } => ActuatorBuild::Stale,
        super::build_identity::BuildCheck::Unknowable { reason } => {
            tracing::info!(
                event = "actuator.build.unknowable",
                session = %session_name,
                reason = %reason,
                "the resident actuator's build could not be compared against the installed one"
            );
            ActuatorBuild::Unknowable
        }
    }
}

/// The name this component is called by in a log line and a refusal.
const ACTUATOR_PROGRAM: &str = "the resident actuator";

/// The pure rule behind [`start_actuator`].
///
/// TWO INPUTS NOW: whether a session is there, and whether the actuator in it
/// is the installed build. The build only ever decides the RUNNING case — a
/// dead pane is replaced whatever it was built from, an absent one is started,
/// and a session tmux would not describe is still refused rather than doubled.
const fn start_move(session: ActuatorSession, build: ActuatorBuild) -> StartMove {
    match session {
        // A LIVE ACTUATOR ON THE WRONG BUILD IS REPLACED, and only when the
        // answer is PROVEN. `Unknowable` leaves it alone: an actuator nobody
        // can identify is still the actuator this company is being run by, and
        // stopping it to satisfy a question nobody answered would take a
        // working company down.
        ActuatorSession::Running if matches!(build, ActuatorBuild::Stale) => {
            StartMove::ReplaceStale
        }
        _ => start_move_by_session(session),
    }
}

/// The session half of the rule, unchanged since before the build check.
const fn start_move_by_session(session: ActuatorSession) -> StartMove {
    match session {
        // Somebody's actuator window is already up on this socket and chiefd
        // has not seen its lease yet. It is starting, or it is wedged; either
        // way a second one is never the answer. The wait decides.
        ActuatorSession::Running => StartMove::LeaveItAlone,
        // A window whose actuator exited, kept by `remain-on-exit` so its last
        // words survive. A dead pane cannot be a second actuator, so replacing
        // it rolls forward; if the replacement dies too, the wait quotes it.
        ActuatorSession::Exited => StartMove::ReplaceExited,
        ActuatorSession::Absent => StartMove::CreateOne,
        ActuatorSession::Unknown => StartMove::FailClosed,
    }
}

/// Start one resident actuator for the company in `dir`, or leave a live one
/// alone.
fn start_actuator(
    dir: &Path,
    socket: &str,
    company_session: &str,
    command: &[String],
) -> Result<()> {
    let session_name = actuator_session_name(company_session);
    let build = actuator_build_check(socket, &session_name);
    match start_move(tmux::actuator_session(socket, &session_name), build) {
        StartMove::LeaveItAlone => return Ok(()),
        StartMove::ReplaceStale => {
            // A WORKING ACTUATOR, DELIBERATELY REPLACED. No corpse to quote:
            // this pane is alive and is about to be taken down because the
            // binary it runs is not the one installed. The operator is told
            // that in the same breath, because a company whose actuator
            // restarts for no stated reason reads as a fault.
            println!(
                "chief attach: the actuator for {} is running a replaced binary; restarting it \
                 on the installed build",
                dir.display()
            );
            tracing::warn!(
                event = "actuator.build.stale",
                session = %session_name,
                "the resident actuator is not the installed build; taking its session down so a \
                 fresh one starts on the installed binary"
            );
            // The session teardown chief already owns for the actuator. It is
            // the actuator's own graceful path: no person's pane lives on this
            // socket session — it holds one pane, running `chief actuate`.
            tmux::kill_session(socket, &session_name)?;
        }
        StartMove::ReplaceExited => {
            // #1207: SAY WHAT DIED, at the one moment a human is looking.
            //
            // The corpse is the only record of the death — chiefd never knew
            // the actuator's pid or pane, and the pane is about to be killed.
            // On 2026-08-23 an operator spent two hours not knowing, and then
            // the evidence was destroyed by the very command that fixed it.
            let target = format!("{session_name}:");
            let dead_status =
                tmux::run(socket, &["list-panes", "-t", &target, "-F", "#{pane_dead_status}"])
                    .stdout
                    .lines()
                    .next()
                    .and_then(|line| line.trim().parse::<i32>().ok());
            let scrollback = tmux::capture_dead_pane(socket, &target, CORPSE_SCROLLBACK_LINES);
            println!("{}", corpse_narration(&dir.display().to_string(), dead_status, &scrollback));
            tmux::kill_session(socket, &session_name)?;
        }
        StartMove::CreateOne => {}
        StartMove::FailClosed => {
            return Err(LifecycleError::host(format!(
                "chief attach: could not read tmux session '{session_name}' on socket '{socket}', \
                 so whether {} already has an actuator is unknown. Refusing rather than starting \
                 a second one — two actuators for one company disagree about what should be \
                 running. Check the tmux server, then retry.",
                dir.display()
            )));
        }
    }

    let mut names = tmux::PANE_ENVIRONMENT.to_vec();
    names.extend_from_slice(&ACTUATOR_ENVIRONMENT);
    let forward = tmux::forwarded(&names);

    println!(
        "chief attach: nobody is actuating {} — starting one in tmux session '{session_name}'",
        dir.display()
    );
    // THE ACTUATOR'S PANE RUNS IN THE COMPANY DIRECTORY. `chief actuate` takes
    // no argument and resolves its company from its own cwd, so the start
    // directory is how it is told which company to run.
    tmux::start_actuator(socket, &session_name, dir, command, &forward)?;
    Ok(())
}

/// The actuator's argv, resolved only once this host has been cleared to run
/// people at all.
///
/// # The preflight, and why it is here
///
/// This is the command that spawns every person in the company. Left unasked,
/// a host with no `pi` does not refuse — it mints a pane whose command dies
/// instantly, tmux reaps the empty window, and the next step fails against a
/// window that no longer exists with a message about window DIMENSIONS, once
/// per second, forever. `chief actuate` asks for itself; attach asks in its
/// OWN process, before the window exists, because a refusal printed inside a
/// pane that tmux then destroys is a refusal nobody reads.
///
/// # Errors
/// [`LifecycleError::Preflight`] when this host cannot run people;
/// [`LifecycleError::Host`] when this binary cannot name its own path.
pub(crate) fn resolve_actuator_command(dir: &Path) -> Result<Vec<String>> {
    super::preflight::require_ready_to_actuate()?;
    // The company is the pane's WORKING DIRECTORY, never an argument — see
    // `actuator_command`.
    let _ = dir;
    // This binary, by its own absolute path. Not a PATH lookup: the installed
    // chiefd is what the operator invoked and it is what must come back up
    // inside the pane, whatever the tmux server's PATH happens to be.
    let executable = std::env::current_exe().map_err(|error| {
        LifecycleError::host(format!("chief cannot locate its own executable: {error}"))
    })?;
    Ok(actuator_command(&executable.display().to_string()))
}

/// The actuator's argv. Pure, so the verb is provable.
///
/// TWO WORDS, and the company is not one of them. `chief actuate` acts on the
/// directory it is run in, so the company is carried by the pane's start
/// directory (`tmux::start_actuator`'s `-c`) rather than by an argument — and
/// the router REFUSES a positional, so a third word here would not merely be
/// redundant, it would make every actuator start fail.
#[must_use]
fn actuator_command(executable: &str) -> Vec<String> {
    vec![executable.to_string(), "actuate".to_string()]
}

/// Wait, bounded, until this company's actuator is actually up.
///
/// # It asks tmux, and it used to ask chiefd
///
/// The wait was `company.actuator_present()` — a lease chiefd granted to
/// whoever last reported an observation. That route is deleted and so is the
/// report behind it, and the replacement is not a poorer answer but a better
/// one: a RUNNING actuator window on the socket this client is booting is
/// ground truth, and the lease was a durable record that outlived its holder by
/// up to a lease window (see [`actuator_needed`] for the measurement).
///
/// One thing genuinely changes. "The window is running" is not the same claim
/// as "the actuator has enrolled with chiefd and is converging", and this wait
/// no longer distinguishes them. [`await_company_session`] does — it is the
/// wait that was always the real proof of progress, and the two are separate
/// precisely so the second can say what the first cannot.
async fn await_actuator(dir: &Path, socket: &str, company_session: &str) -> Result<()> {
    let session_name = actuator_session_name(company_session);
    let deadline = Instant::now() + ACTUATOR_BUDGET;
    let mut ladder = Ladder::new(
        ACTUATOR_WINDOW_LADDER,
        session_name.as_str(),
        ACTUATOR_BUDGET,
        ACTUATOR_INTERVAL,
    );
    loop {
        let session = tmux::actuator_session(socket, &session_name);
        if session == ActuatorSession::Running {
            ladder.resolved();
            return Ok(());
        }
        if session == ActuatorSession::Exited {
            ladder.failed("the actuator window exited");
            return Err(LifecycleError::host(actuator_failed(
                dir,
                &session_name,
                socket,
                "its window exited",
                &tmux::capture_dead_pane(socket, &session_name, 20),
            )));
        }
        if Instant::now() >= deadline {
            ladder.failed("the budget ran out");
            return Err(LifecycleError::host(actuator_failed(
                dir,
                &session_name,
                socket,
                &format!("its window has not come up within {ACTUATOR_BUDGET:?}"),
                &tmux::capture_pane(socket, &session_name, 20),
            )));
        }
        // The window is not up YET, which is what "just started it" means. One
        // `info` on entry, `debug` for every repeat, and the loud line is the
        // `failed` arm above. See `chief_cli::ladder`.
        ladder.waiting();
        // os-liveness: there is no push channel for "the process I just started
        // is up". Bounded by ACTUATOR_BUDGET above and never
        // unbounded — the same exemption, at the same narrowness, as
        // `discovery::ensure_running`'s bind wait.
        #[allow(clippy::disallowed_methods)]
        tokio::time::sleep(ACTUATOR_INTERVAL).await;
    }
}

/// Wait, bounded, until the company's own session exists.
///
/// A SEPARATE wait from [`await_actuator`], and the split is not tidiness: the
/// two answer different questions and their failures need different sentences.
/// An actuator's window comes up SECONDS before it has read the desired set and
/// applied the plan that mints the company session, so a live actuator is true
/// long before there is anything to enter — attaching on the first answer alone
/// walks into `can't find session`, the failure this whole path exists to
/// delete. And when the session never arrives, the honest sentence is "somebody
/// is actuating and chiefd is asking for nobody", which is a different fault
/// from "nobody is actuating" and must never be reported as one. The merged
/// wait said the second when it meant the first, on a live host, which is the
/// same success-shape-over-a-failure defect this tree keeps paying for.
async fn await_company_session(dir: &Path, socket: &str, company_session: &str) -> Result<()> {
    let session_name = actuator_session_name(company_session);
    let deadline = Instant::now() + ACTUATOR_BUDGET;
    let mut ladder =
        Ladder::new(COMPANY_SESSION_LADDER, company_session, ACTUATOR_BUDGET, ACTUATOR_INTERVAL);
    loop {
        if tmux::session_exists(socket, company_session) == Some(true) {
            ladder.resolved();
            return Ok(());
        }
        // The actuator DIED between coming up and minting the company. Its own
        // last words are on screen and are the whole cause, so this is
        // reported at once rather than after the full budget. `Absent` counts
        // too: the window it was started in is gone.
        if matches!(
            tmux::actuator_session(socket, &session_name),
            ActuatorSession::Exited | ActuatorSession::Absent
        ) {
            ladder.failed("the actuator stopped before minting the company");
            return Err(LifecycleError::host(actuator_failed(
                dir,
                &session_name,
                socket,
                "it then stopped before the company came up",
                &tmux::capture_dead_pane(socket, &session_name, 20),
            )));
        }
        if Instant::now() >= deadline {
            ladder.failed("the budget ran out");
            return Err(LifecycleError::refused(format!(
                "chief attach: {} has a running actuator, and chiefd is \
                 still asking for nobody to run — no tmux session '{company_session}' appeared \
                 within {ACTUATOR_BUDGET:?}. The actuator applies what chiefd decides and never \
                 decides for itself, so this is chiefd's desired roster, not a tmux fault. What \
                 that actuator has been printing:\n{}",
                dir.display(),
                tmux::capture_pane(socket, &session_name, 12)
            )));
        }
        // The session is not minted YET, and this whole function exists because
        // it takes seconds. Quiet: see `chief_cli::ladder`.
        ladder.waiting();
        // os-liveness: the company session is minted by another process
        // applying a plan, with no push channel back to this one. Bounded by
        // ACTUATOR_BUDGET above.
        #[allow(clippy::disallowed_methods)]
        tokio::time::sleep(ACTUATOR_INTERVAL).await;
    }
}

/// What an operator reads when the actuator attach started did not come up.
///
/// Loud, and never an exit 0: silently attaching to a company nobody is running
/// is the original defect. It quotes the actuator's own last words, because the
/// cause is in that pane and nowhere else, and it ends with the standing
/// recovery — run the resident verb yourself, in a terminal, and watch it.
#[must_use]
fn actuator_failed(
    dir: &Path,
    session_name: &str,
    socket: &str,
    detail: &str,
    pane: &str,
) -> String {
    let mut message = format!(
        "chief attach: started an actuator for {} in tmux session '{session_name}' on \
         socket '{socket}', and {detail}.",
        dir.display()
    );
    if pane.is_empty() {
        message.push_str(" That window printed nothing.");
    } else {
        message.push_str(&format!("\nWhat it printed:\n{pane}"));
    }
    message.push('\n');
    message.push_str(&no_actuator_refusal(dir));
    message
}

/// The standing recovery for a company nobody is running, and the command that
/// runs one.
///
/// # The defect this closes (#751/P8, found by the live proof)
///
/// Since the actuation switchover chiefd publishes actions and a CLIENT applies
/// them, so stating CEO-only intent no longer makes a CEO. On a company with no
/// actuator, `attach` stated the intent and then walked straight into
/// `tmux::attach` against a session nobody had created. What an operator saw
/// was:
///
/// ```text
/// chief attach: starting ChiefD for 'northwind-logistics'
/// chief attach: booting 'northwind-logistics' (CEO-only)
/// can't find session: org-northwind-logistics
/// could not switch this tmux client to ChiefD session '…' on socket 'default'
/// (tmux exited 1). … If the company runs on a different socket, detach first …
/// ```
///
/// — a diagnosis about SOCKETS, for a problem that is not about sockets, ending
/// in advice that cannot work. And it exited 0. Combined with `chief`,
/// which hands the operator `chief attach <company>` as its recovery, the
/// product had NO reachable path from the front door to a running company:
/// every message pointed at the one verb that cannot start anybody, and none of
/// them named `chief actuate`, which is the verb that can.
///
/// `876e4b545` replaced that with an honest refusal carrying this sentence.
/// Attach now goes one step further and starts the actuator itself, so this is
/// no longer what an operator reads on the ordinary path — it is what they read
/// when the actuator attach started did not come up, and it is still the right
/// recovery: run the resident verb in a terminal of your own and watch it.
///
/// Pure, so the recovery is a value a test can hold.
#[must_use]
fn no_actuator_refusal(dir: &Path) -> String {
    let dir = dir.display();
    format!(
        "chief attach: the company in {dir} has nobody actuating it, so its people are not \
         running and there is no session to enter. chiefd decides who runs; a client is what runs \
         them. In another terminal, run and LEAVE OPEN:\n    cd {dir} && chief actuate\nthen run \
         `chief attach` here again."
    )
}

// TOMBSTONE: `socket_for`.
//
// It asked a company's daemon for its recorded runtime socket BEFORE the
// attach path knew whether it had a daemon at all, so it took an
// `Option<&str>` URL and quietly answered the environment tiers alone when
// there was none. `run` proves the daemon first now — it has to, because the
// session name is read from the store — so the recorded tier is always
// available at the one place it is used, and a helper that existed to cope
// with its absence has nothing left to cope with.

#[cfg(test)]
mod tests {

    use std::path::Path;

    use chief_cli::actuate::trust;
    use chief_cli::{layout, sidebar};

    use super::super::company::conventional_session_name;
    use super::super::daemon::DaemonStatus;
    use super::super::tmux::test_support::{
        require_tmux, start_session, unique_socket, wait_until,
    };
    use super::super::tmux::{self, ActuatorSession};
    use super::{
        acquire_attach_viewport_authority, actuator_command, actuator_failed, actuator_needed,
        actuator_session_name, await_company_session, corpse_narration, daemon_move,
        ensure_actuator, entry_rail_columns, leave_session_pane_modes, no_actuator_refusal,
        publish_attach_viewport_once, rail_mint_argv, rail_survey, retry_attach_viewport,
        session_move, start_actuator, start_move, ActuatorBuild, DaemonMove, SessionMove,
        StartMove, ACTUATOR_ENVIRONMENT, DOOR_ATTACH_RUNNING, DOOR_ATTACH_STARTED,
        DOOR_FOUNDER_HANDOVER,
    };
    use super::{
        install_viewport_manifest_if_current, viewport_bootstrap_authority, viewport_hook_action,
        viewport_hook_bootstrap_argv, viewport_hook_command, viewport_resize_hook_refresh_argv,
        ViewportManifestWindow,
    };

    const VIEWPORT_NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn final_handoff_leaves_copy_mode_before_the_operator_sees_chief() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("attach-leaves-copy-mode");
        let session = "org-attach-leaves-copy-mode_";
        start_session(&socket, session, &["sleep", "120"]);
        let pane = tmux::run(&socket, &["display-message", "-p", "-t", session, "#{pane_id}"]);
        assert!(pane.ok(), "fixture pane: {}", pane.diagnostic());
        assert!(tmux::run(&socket, &["copy-mode", "-t", &pane.stdout]).ok());
        assert_eq!(
            tmux::run(&socket, &["display-message", "-p", "-t", &pane.stdout, "#{pane_in_mode}"],)
                .stdout,
            "1",
            "the fixture reproduces the inherited orange copy-mode screen"
        );

        leave_session_pane_modes(&socket, session).expect("final handoff fence");

        assert_eq!(
            tmux::run(&socket, &["display-message", "-p", "-t", &pane.stdout, "#{pane_in_mode}"],)
                .stdout,
            "0",
            "Chief must open in its normal screen, not an inherited pane mode"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    fn viewport_server_nonce(socket: &str) -> String {
        let mut nonce =
            tmux::run(socket, &["show-options", "-gqv", trust::viewport_options::SERVER_NONCE]);
        if nonce.stdout.trim().is_empty() {
            assert!(tmux::run(
                socket,
                &["set-option", "-goq", trust::viewport_options::SERVER_NONCE, VIEWPORT_NONCE],
            )
            .ok());
            nonce =
                tmux::run(socket, &["show-options", "-gqv", trust::viewport_options::SERVER_NONCE]);
        }
        assert!(nonce.ok(), "server nonce: {}", nonce.diagnostic());
        assert!(trust::is_safe_server_nonce(nonce.stdout.trim()));
        nonce.stdout.trim().to_owned()
    }

    fn install_viewport_hooks(
        executable: &Path,
        socket: &str,
        session: &str,
        manifest: &[ViewportManifestWindow],
        columns: i64,
        collapsed: bool,
    ) -> u64 {
        let bootstrap = viewport_hook_bootstrap_argv(executable, socket, session);
        let bootstrap_refs: Vec<&str> = bootstrap.iter().map(String::as_str).collect();
        let bootstrapped = tmux::run(socket, &bootstrap_refs);
        assert!(bootstrapped.ok(), "viewport bootstrap: {}", bootstrapped.diagnostic());
        let (epoch, nonce, _) =
            viewport_bootstrap_authority(&bootstrapped.stdout).expect("bootstrap authority");
        assert_eq!(nonce, viewport_server_nonce(socket));
        let manifest = viewport_resize_hook_refresh_argv(
            executable, socket, session, manifest, columns, collapsed, epoch,
        );
        let manifest_refs: Vec<&str> = manifest.iter().map(String::as_str).collect();
        let installed = tmux::run(socket, &manifest_refs);
        assert!(installed.ok(), "viewport manifest: {}", installed.diagnostic());
        epoch
    }

    fn viewport_test_callback_source(test_executable: &Path) -> String {
        let executable = super::shell_quote(&test_executable.display().to_string());
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             viewport-client-eligible)\n\
               export CHIEF_TEST_VIEWPORT_SOCKET=\"$2\"\n\
               export CHIEF_TEST_VIEWPORT_SESSION=\"$3\"\n\
               export CHIEF_TEST_VIEWPORT_CLIENT=\"$4\"\n\
               export CHIEF_TEST_VIEWPORT_NONCE=\"$5\"\n\
               exec {executable} --ignored --exact attach::tests::viewport_client_eligible_child --nocapture\n\
               ;;\n\
             viewport-resize)\n\
               export CHIEF_TEST_VIEWPORT_SOCKET=\"$2\"\n\
               export CHIEF_TEST_VIEWPORT_SESSION=\"$3\"\n\
               export CHIEF_TEST_VIEWPORT_ORGANIZATION=\"$4\"\n\
               export CHIEF_TEST_VIEWPORT_CLIENT=\"$5\"\n\
               export CHIEF_TEST_VIEWPORT_EVENT=\"$6\"\n\
               export CHIEF_TEST_VIEWPORT_NONCE=\"$7\"\n\
               exec {executable} --ignored --exact attach::tests::viewport_callback_child --nocapture\n\
               ;;\n\
             viewport-client-changed)\n\
               export CHIEF_TEST_VIEWPORT_SOCKET=\"$2\"\n\
               export CHIEF_TEST_VIEWPORT_CLIENT=\"$3\"\n\
               export CHIEF_TEST_VIEWPORT_NONCE=\"$4\"\n\
               exec {executable} --ignored --exact attach::tests::viewport_session_changed_child --nocapture\n\
               ;;\n\
             viewport-client-census)\n\
               export CHIEF_TEST_VIEWPORT_SOCKET=\"$2\"\n\
               export CHIEF_TEST_VIEWPORT_GENERATION=\"$3\"\n\
               export CHIEF_TEST_VIEWPORT_NONCE=\"$4\"\n\
               exec {executable} --ignored --exact attach::tests::viewport_client_census_child --nocapture\n\
               ;;\n\
             viewport-sidebar-width)\n\
               export CHIEF_TEST_WIDTH_SOCKET=\"$2\"\n\
               export CHIEF_TEST_WIDTH_SESSION=\"$3\"\n\
               export CHIEF_TEST_WIDTH_ORGANIZATION=\"$4\"\n\
               export CHIEF_TEST_WIDTH_SESSION_ID=\"$5\"\n\
               export CHIEF_TEST_WIDTH_NONCE=\"$6\"\n\
               export CHIEF_TEST_WIDTH_COLUMNS=\"$7\"\n\
               export CHIEF_TEST_WIDTH_SURPLUS=\"$8\"\n\
               exec {executable} --ignored --exact attach::tests::viewport_sidebar_width_child --nocapture\n\
               ;;\n\
             viewport-manifest-refresh)\n\
               export CHIEF_TEST_MANIFEST_SOCKET=\"$2\"\n\
               export CHIEF_TEST_MANIFEST_SESSION=\"$3\"\n\
               export CHIEF_TEST_MANIFEST_NONCE=\"$4\"\n\
               export CHIEF_TEST_MANIFEST_EPOCH=\"$5\"\n\
               exec {executable} --ignored --exact attach::tests::viewport_manifest_refresh_child --nocapture\n\
               ;;\n\
             *) exit 64 ;;\n\
             esac\n"
        )
    }

    /// BOTH DOORS ASK THE SAME QUESTION ABOUT A TERMINAL.
    ///
    /// `chief` and `chief attach` both start a tmux client when there is
    /// none, so both must refuse identically when there is no terminal to seat
    /// one in. The operator hit the asymmetry from the other side — `attach`
    /// demanded an ambient tmux that `new` was happy to create — and the way
    /// that does not come back is for one predicate to answer for both.
    ///
    /// Asserted on the SOURCE so a second, drifting copy of the check fails
    /// here rather than in production: `attach` must call `can_be_attached`
    /// rather than reimplement it.
    #[test]
    fn attach_and_new_use_one_terminal_predicate() {
        let attach_rs = include_str!("attach.rs");
        let needle = format!("founder::{}", "can_be_attached");
        assert!(
            attach_rs.contains(needle.as_str()),
            "attach must reuse the Founder's terminal predicate, never its own copy"
        );
        // The predicate itself is pinned in `founder::tests`; this is the
        // truth table both doors now share.
        assert!(super::super::founder::can_be_attached(true, true));
        assert!(!super::super::founder::can_be_attached(false, true));
        assert!(!super::super::founder::can_be_attached(true, false));
        assert!(!super::super::founder::can_be_attached(false, false));
    }

    /// THE PRODUCTION STRING. A CEO-only company is ONE window with no rail
    /// tag, so `list-panes` prints exactly `@1\t` — and `TmuxOutput.stdout` is
    /// TRIMMED, which strips that trailing tab and leaves `@1`.
    ///
    /// The original parse dropped any line without a tab, so the survey saw
    /// zero windows and `enter_company_session` logged
    /// `sidebar.rails.none: this session listed no windows at all`. The rail
    /// was never placed, on the exact shape every newly created company has.
    ///
    /// Read off the operator's own box at 21:23:32, two milliseconds after the
    /// Founder handover opened the door.
    #[test]
    fn a_single_untagged_window_still_needs_a_rail_even_though_its_tab_was_trimmed() {
        let (railed, windows) = rail_survey("@1");
        assert!(railed.is_empty(), "nothing is tagged");
        assert_eq!(windows, vec!["@1"], "a tabless line is a window with an EMPTY marker");
    }

    #[test]
    fn the_survey_reads_tagged_and_untagged_windows_apart() {
        // Two panes in one window, then a second window carrying a rail. The
        // last line loses its tab to the trim exactly as production does.
        let (railed, windows) = rail_survey("@1\t\n@2\t1\n@3");
        assert_eq!(railed.into_iter().collect::<Vec<_>>(), vec!["@2"]);
        assert_eq!(windows, vec!["@1", "@3"], "both untagged windows need a rail");
    }

    #[test]
    fn a_window_listed_twice_is_railed_once() {
        // One window, two panes: the survey must not queue two rails for it.
        let (_, windows) = rail_survey("@1\t\n@1\t");
        assert_eq!(windows, vec!["@1"]);
    }

    #[test]
    fn an_empty_listing_is_still_no_windows() {
        // The genuine "nowhere to put a rail" case keeps its meaning: this is
        // what the warning is FOR, and the fix must not make it unreachable.
        let (railed, windows) = rail_survey("");
        assert!(railed.is_empty() && windows.is_empty());
    }

    #[test]
    fn attach_viewport_reacquires_authority_after_one_topology_change() {
        let acquired = std::cell::Cell::new(0_u64);
        let published = std::cell::Cell::new(0_u64);
        retry_attach_viewport(
            || {
                let next = acquired.get() + 1;
                acquired.set(next);
                Ok(next)
            },
            |authority| {
                published.set(published.get() + 1);
                Ok(*authority > 1)
            },
        )
        .expect("a fresh authority retries the attach after the old epoch loses its CAS");
        assert_eq!(acquired.get(), 2, "the retry must capture a new epoch");
        assert_eq!(published.get(), 2, "only one stale attempt is repeated");
    }

    #[test]
    fn attach_viewport_stops_after_three_stale_topologies() {
        let acquired = std::cell::Cell::new(0_u64);
        let published = std::cell::Cell::new(0_u64);
        let error = retry_attach_viewport(
            || {
                acquired.set(acquired.get() + 1);
                Ok(())
            },
            |_| {
                published.set(published.get() + 1);
                Ok(false)
            },
        )
        .expect_err("unbounded actuator churn must not trap bare chief forever");
        assert!(error.to_string().contains("stayed stale across 3 attempts"), "{error}");
        assert_eq!(acquired.get(), 3, "each attempt captures fresh authority");
        assert_eq!(published.get(), 3, "each captured authority is tried once");
    }

    /// THE FIRST VISIBLE RAIL FRAME IS FINAL.
    ///
    /// The live attach minted a one-column rail and sent its tag in a later
    /// tmux process. The rail client reported that transient PTY size, and the
    /// brain repaired it to 26 only 5.36 to 6.61 seconds later. This small tmux
    /// model publishes one frame at the end of each invocation, as tmux does;
    /// it proves that open and collapsed rails now have one invocation, one
    /// pane, one visible frame, and no later repair.
    #[test]
    fn a_fresh_attach_publishes_one_rail_at_its_final_width() {
        for (recorded, collapsed, expected) in [
            ("", "", sidebar::brain::RAIL_DEFAULT_COLUMNS),
            ("31", "0", 31),
            ("31", "1", layout::RAIL_COLLAPSED_COLUMNS),
        ] {
            let columns = entry_rail_columns(recorded, collapsed);
            assert_eq!(columns, expected);
            let argv = rail_mint_argv(
                "org-acme_",
                "@7",
                Path::new("/tmp/acme"),
                Path::new("/usr/bin/chief"),
                columns,
            );
            let commands: Vec<&[String]> = argv.split(|arg| arg == ";").collect();
            assert_eq!(commands.len(), 4, "all writes belong to one tmux invocation: {argv:?}");

            let mut rail_columns = None;
            let mut rail_tagged = false;
            let mut panes = 0;
            for command in commands {
                match command.first().map(String::as_str) {
                    Some("set-option") if command.iter().any(|arg| arg == "-p") => {
                        assert_eq!(command[command.len() - 2], trust::tags::SIDEBAR);
                        rail_tagged = command.last().is_some_and(|value| value == "1");
                    }
                    Some("split-window") => {
                        panes += 1;
                        let width = command
                            .windows(2)
                            .find_map(|pair| (pair[0] == "-l").then(|| pair[1].parse().ok()))
                            .flatten();
                        rail_columns = width;
                    }
                    Some("resize-pane") => {
                        let width = command
                            .windows(2)
                            .find_map(|pair| (pair[0] == "-x").then(|| pair[1].parse().ok()))
                            .flatten();
                        rail_columns = width;
                    }
                    // The rail is built and then handed back; see the frame's
                    // own test below for why this one is last.
                    Some("select-pane") => {}
                    other => panic!("unexpected simulated tmux command {other:?}: {command:?}"),
                }
            }

            // One invocation has one publication boundary. This is the only
            // frame the model can expose, and every final fact is in it.
            let visible_frames = [(rail_columns, rail_tagged, panes)];
            assert_eq!(visible_frames, [(Some(columns), true, 1)]);
            assert!(
                !argv.iter().any(|arg| arg == trust::sidebar_options::COLUMNS)
                    && !argv.iter().any(|arg| arg == trust::sidebar_options::COLLAPSED),
                "attach consumes preferences and never writes them: {argv:?}"
            );
        }
    }

    /// THE OPERATOR LANDS ON THE PERSON, NOT ON THE FURNITURE.
    ///
    /// `split-window` has no `-d`, so the rail is the active pane the moment it
    /// exists — which is what the tag and the resize above need, and which
    /// also means a bare `chief` used to open with the cursor in the sidebar
    /// instead of in the pane the operator types into. The frame gives it back
    /// with `select-pane -l`: the window's last pane, which a split makes the
    /// pane that was active before it. It must be the LAST command of the
    /// frame — every write before it names a WINDOW, which tmux resolves to
    /// the active pane, so a selection moved earlier would tag and resize the
    /// wrong pane.
    #[test]
    fn the_rail_frame_hands_the_cursor_back_after_every_other_write() {
        let argv = rail_mint_argv(
            "org-acme_",
            "@7",
            Path::new("/tmp/acme"),
            Path::new("/usr/bin/chief"),
            26,
        );
        let commands: Vec<&[String]> = argv.split(|arg| arg == ";").collect();
        let last = commands.last().expect("the frame is not empty");
        assert_eq!(
            last,
            &["select-pane".to_owned(), "-l".to_owned(), "-t".to_owned(), "@7".to_owned()],
            "the frame ends by restoring the pane the split took the cursor from: {argv:?}"
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.first().is_some_and(|v| v == "select-pane"))
                .count(),
            1,
            "exactly one selection, and it is that one: {argv:?}"
        );
        // Both window-targeted writes still run while the rail is active.
        let selection = commands.len() - 1;
        for verb in ["split-window", "set-option", "resize-pane"] {
            let at = commands
                .iter()
                .position(|command| command.first().is_some_and(|first| first == verb))
                .unwrap_or_else(|| panic!("the frame still carries {verb}: {argv:?}"));
            assert!(at < selection, "{verb} must run before the cursor moves off the rail");
        }
    }

    #[test]
    fn viewport_hook_is_ordered_and_quotes_static_authority() {
        let surveyed =
            super::viewport_manifest_survey("@7\t%11\texecutive\t1\n@7\t%12\texecutive\t")
                .expect("a complete viewport survey");
        assert_eq!(surveyed.len(), 1);
        for hostile in [
            "@x\t%11\texecutive\t1",
            "@7\t%x\texecutive\t1",
            "@7\t%11\texec}utive\t1",
            "@7\t%11\texec,utive\t1",
            "@7\t%11\t#{pane_id}\t1",
            "@7\t%11\texec'utive\t1",
            "@7\t%11\texec;utive\t1",
            "@7\t%11\texecutive\t2",
        ] {
            assert!(
                super::viewport_manifest_survey(hostile).is_err(),
                "hostile manifest fact must fail closed: {hostile:?}"
            );
        }
        let command =
            viewport_hook_command(Path::new("/opt/Chief's bin/chief"), "chiefd-acme", "org-acme_");
        assert_eq!(
            command,
            "'/opt/Chief'\\''s bin/chief' viewport-resize 'chiefd-acme' 'org-acme_' #{q:@organization_id} #{q:hook_client} #{@chief_viewport_request} #{q:@chief_viewport_server_nonce}"
        );
        let bootstrap = viewport_hook_bootstrap_argv(
            Path::new("/opt/chief/bin/chief"),
            "chiefd-acme",
            "org-acme_",
        );
        let refresh = bootstrap
            .windows(2)
            .find_map(|pair| {
                (pair[0] == trust::viewport_options::REFRESH_COMMAND).then_some(&pair[1])
            })
            .expect("bootstrap stores the hidden manifest refresh prefix");
        assert!(refresh.ends_with("#{q:@chief_viewport_server_nonce}"));
        assert!(
            !refresh.contains(">/dev/null") && !refresh.contains("|| :"),
            "the topology epoch must be appended to Chief before output suppression: {refresh}"
        );
        assert!(!command.contains("run-shell"), "the leaf command is only the callback argv");
        let action = viewport_hook_action(
            Path::new("/opt/Chief's bin/chief"),
            "chiefd-acme",
            "org-acme_",
            &[super::ViewportManifestWindow {
                window: "@7".to_owned(),
                window_tag: "executive".to_owned(),
                rail: "%11".to_owned(),
                panes: vec![("%11".to_owned(), true), ("%12".to_owned(), false)],
            }],
            26,
            false,
            0,
        );
        assert!(action.starts_with("set-option -t 'org-acme_' @chief_viewport_preflight 0"));
        assert!(action.contains("set-option -qu -t 'org-acme_' @chief_viewport_preflight"));
        assert!(action.contains("viewport-client-eligible"));
        assert!(action.contains("#{q:hook_client}"));
        assert!(!action.contains("#{client_name}"));
        assert!(!action.contains("#{client_pid}"));
        assert!(action.contains("set-option -F "));
        assert!(action.contains("set-option -gF @chief_viewport_generation"));
        assert!(action.contains("set-option -F -t "));
        assert!(action.contains("org-acme_"));
        assert!(action.contains("@chief_viewport_generation"));
        assert!(action.contains("@chief_viewport_request"));
        assert!(action.contains("@chief_viewport_request"));
        assert!(action.contains(" ; run-shell "));
        assert!(action.contains("#{pane_width},26"));
        assert!(action.contains("@chief_viewport_topology_epoch"));
        assert!(action.contains("@chief_viewport_manifest_epoch"));
        assert!(action.contains("run-shell -b"), "newer hook events must not be blocked");

        let realistic = company_manifest(7, 4);
        let realistic_action = viewport_hook_action(
            Path::new("/opt/chief/bin/chief"),
            "chiefd-acme",
            "org-acme_",
            &realistic,
            26,
            false,
            41,
        );
        assert_eq!(
            realistic_action.matches("@chief_viewport_preflight").count(),
            1 + (7 * 2 + 1) * 2 + 1 + 1,
            "one reset, one set-and-read proof per check, one final read, and one final clear"
        );

        let install = viewport_hook_bootstrap_argv(
            Path::new("/opt/chief/bin/chief"),
            "chiefd-acme",
            "org-acme_",
        );
        assert_eq!(install.iter().filter(|word| word.as_str() == "set-hook").count(), 4);
        assert!(install.iter().any(|word| word == "client-resized"));
        assert_eq!(&install[..3], ["set-option", "-goq", "@chief_viewport_server_nonce"]);
        assert!(trust::is_safe_server_nonce(&install[3]));
        assert!(install.windows(5).any(|words| {
            words == ["set-option", "-goq", "@chief_viewport_generation", "0", ";"]
        }));
        assert!(install.windows(5).any(|words| {
            words == ["set-option", "-goq", "@chief_viewport_membership_generation", "0", ";"]
        }));
        assert!(install.windows(5).any(|words| {
            words == ["set-option", "-goq", "@chief_viewport_topology_generation", "0", ";"]
        }));
        assert!(install
            .windows(4)
            .any(|words| { words == ["set-option", "-gu", "@chief_viewport_fast_session", ";"] }));
        let manifest = viewport_resize_hook_refresh_argv(
            Path::new("/opt/chief/bin/chief"),
            "chiefd-acme",
            "org-acme_",
            &[],
            26,
            false,
            1,
        );
        assert_eq!(manifest.iter().filter(|word| word.as_str() == "set-hook").count(), 1);
        assert!(manifest.iter().any(|word| word == "client-resized"));

        let hostile = viewport_hook_command(
            Path::new("/x/#{@v}/Chief's chief"),
            "sock-#{socket}",
            "org-#{session}_",
        );
        assert!(hostile.contains("/x/##{@v}/Chief"), "static executable: {hostile}");
        assert!(hostile.contains("sock-##{socket}"), "static socket: {hostile}");
        assert!(hostile.contains("org-##{session}_"), "static session: {hostile}");
        assert!(
            hostile.ends_with(
                "#{q:@organization_id} #{q:hook_client} #{@chief_viewport_request} #{q:@chief_viewport_server_nonce}"
            ),
            "dynamic event fields stay live: {hostile}"
        );
    }

    /// One company-shaped manifest: `windows` departments, each a tagged rail
    /// plus `bodies` people.
    fn company_manifest(windows: usize, bodies: usize) -> Vec<super::ViewportManifestWindow> {
        let mut pane = 0;
        (0..windows)
            .map(|index| {
                let rail = format!("%{pane}");
                pane += 1;
                let mut panes = vec![(rail.clone(), true)];
                for _ in 0..bodies {
                    panes.push((format!("%{pane}"), false));
                    pane += 1;
                }
                super::ViewportManifestWindow {
                    window: format!("@{index}"),
                    window_tag: format!("department-{index}"),
                    rail,
                    panes,
                }
            })
            .collect()
    }

    /// The exact argv `install_viewport_manifest_if_current` hands the tmux
    /// client, measured the way tmux measures it: the whole packed command.
    fn install_command_bytes(manifest: &[super::ViewportManifestWindow]) -> usize {
        let argv = viewport_resize_hook_refresh_argv(
            Path::new("/root/.chief/bin/chief"),
            "4cc439341aa9",
            "org-taperoom-inc-4cc439_",
            manifest,
            26,
            false,
            7,
        );
        let hook_command =
            format!("{} ; display-message -p applied", super::tmux_command_string(&argv));
        let predicate = format!(
            "#{{&&:#{{==:#{{{}}},7}},#{{&&:#{{==:#{{{}}},taperoom-inc}},#{{==:#{{{}}},{}}}}}}}",
            trust::viewport_options::TOPOLOGY_EPOCH,
            trust::tags::ORGANIZATION,
            trust::viewport_options::SERVER_NONCE,
            "0123456789abcdef0123456789abcdef",
        );
        [
            "if-shell",
            "-F",
            "-t",
            "org-taperoom-inc-4cc439_",
            &predicate,
            &hook_command,
            "display-message -p stale",
        ]
        .iter()
        .map(|word| word.len() + 1)
        .sum()
    }

    /// tmux 3.3a's client packs a whole command into one `imsg` and refuses
    /// anything over `MAX_IMSGSIZE`. Measured on a live box against the real
    /// binary, with a single argument of N bytes:
    ///
    /// ```text
    /// 16300 -> ok
    /// 16350 -> failed to send command
    /// 17000 -> command too long
    /// ```
    const TMUX_COMMAND_CEILING: usize = 16_300;

    /// THE SECOND HALF OF THE OPERATOR'S REBOOT, and the reason it was second:
    ///
    /// ```text
    /// root@host:~/workspace# chief
    /// chief attach could not install the viewport hook set: command too long
    /// ```
    ///
    /// A proof per PANE made this command grow with the roster — measured with
    /// this builder before the fix: 14383 bytes at 8 windows / 24 panes, and
    /// **17130 bytes at 7 windows / 35 panes**, past a ceiling of 16300. So the
    /// operator's first run failed on the layout, the actuator kept minting
    /// panes behind it, and the second run failed here. One monotone cause.
    ///
    /// The old guard for this asserted `< 64 * 1024` — four times the real
    /// limit, on a manifest a quarter of the real size. It could not go red.
    #[test]
    fn no_company_size_can_make_the_viewport_hook_unsendable() {
        // (10, 4) is the shape a live reproduction stood up on a live box and
        // measured with a logging shim in front of the real tmux binary: 10
        // windows, 44 panes, **21482 bytes**, argc 9, measured twice and
        // identical. Deterministic arithmetic, not a race.
        for (windows, bodies) in [(1, 1), (7, 4), (8, 2), (10, 4), (10, 6), (12, 8), (40, 20)] {
            let bytes = install_command_bytes(&company_manifest(windows, bodies));
            assert!(
                bytes < TMUX_COMMAND_CEILING,
                "{windows} windows of {bodies} people is {bytes} bytes; tmux refuses \
                 anything over {TMUX_COMMAND_CEILING} with `command too long`, and a company \
                 that cannot install its hook is a company that does not start"
            );
        }
    }

    /// The ceiling is a BACKSTOP and it must be reachable, not decorative: past
    /// it the hook is rebuilt without its manifest, which costs the in-tmux
    /// fast path and keeps the asynchronous callback that has always been the
    /// correct authority.
    #[test]
    fn a_manifest_too_large_for_one_tmux_command_drops_to_the_callback_hook() {
        let enormous = company_manifest(200, 20);
        let argv = viewport_resize_hook_refresh_argv(
            Path::new("/root/.chief/bin/chief"),
            "4cc439341aa9",
            "org-taperoom-inc-4cc439_",
            &enormous,
            26,
            false,
            7,
        );
        let rendered = super::tmux_command_string(&argv);
        assert!(rendered.len() <= super::VIEWPORT_HOOK_MAX_BYTES, "{} bytes", rendered.len());
        assert!(
            !rendered.contains("@chief_viewport_preflight"),
            "the fast path is what is dropped, and it is dropped whole"
        );
        assert!(
            rendered.contains("viewport-client-eligible") && rendered.contains("run-shell"),
            "the callback authority survives: {rendered}"
        );
    }

    /// AN UNMANAGED WINDOW MUST NOT VOID THE WHOLE VIEWPORT MANIFEST.
    ///
    /// `list-panes -s` returns every pane in the SESSION, and an operator's own
    /// window carries no `@organization_window_id` and no rail. The
    /// exactly-one-rail rule counted zero for it and discarded the entire survey,
    /// so the refresh that re-installs the resize hook failed on every pass —
    /// measured on a live box, 26 times in one session, which is why that box's hook
    /// kept the topology epoch it was born with and every sidebar drag was refused
    /// as stale.
    #[test]
    fn an_unmanaged_window_does_not_void_the_viewport_manifest() {
        // Three healthy managed windows and one window the operator owns.
        let listed = "@1\t%1\t\t\n\
                  @8\t%21\texecutive\t1\n\
                  @8\t%20\texecutive\t\n\
                  @4\t%6\t__focus__\t1\n\
                  @4\t%2\t__focus__\t\n";
        let manifest = super::viewport_manifest_survey(listed).expect("a complete viewport survey");

        let tags: Vec<&str> = manifest.iter().map(|entry| entry.window_tag.as_str()).collect();
        assert_eq!(tags, ["__focus__", "executive"], "only the managed windows: {tags:?}");
        assert!(
            !manifest.iter().any(|entry| entry.window == "@1"),
            "the operator's own window is not chief's to resize: {manifest:?}"
        );
        assert_eq!(manifest.iter().find(|e| e.window == "@8").expect("executive").rail, "%21");
    }

    /// The rule the filter above must NOT weaken: a MANAGED window with no rail is
    /// still a broken survey, because the fast path would then resize a window it
    /// cannot find the sidebar of.
    #[test]
    fn a_managed_window_with_no_rail_still_voids_the_manifest() {
        let listed = "@8\t%21\texecutive\t1\n@9\t%30\tquant\t\n";
        let reason = super::viewport_manifest_survey(listed)
            .expect_err("a managed window with no tagged rail must still fail closed");
        // AND IT SAYS WHICH ONE. The refusal used to be one fixed sentence for
        // every cause, so 486 refusals a day on a live box said nothing
        // about the window they read — and a survey run by hand against the same
        // live session was healthy, which is exactly the case a fixed sentence
        // cannot help with.
        assert!(reason.contains("@9"), "the void names the window it read: {reason}");
        assert!(reason.contains("quant"), "and the tag it carried: {reason}");
        assert!(reason.contains("0 tagged rails"), "and what was wrong with it: {reason}");
    }

    /// And two rails in one managed window is equally broken — the fast path would
    /// not know which one the operator owns.
    #[test]
    fn two_rails_in_one_managed_window_still_void_the_manifest() {
        let listed = "@8\t%21\texecutive\t1\n@8\t%22\texecutive\t1\n";
        let reason = super::viewport_manifest_survey(listed).expect_err("two rails fail closed");
        assert!(reason.contains("@8") && reason.contains("2 tagged rails"), "{reason}");
    }

    /// EVERY void says something DIFFERENT, which is the whole change.
    ///
    /// One fixed sentence for six causes is what turned a live refusal loop into
    /// an investigation: measured on a live box 2026-08-24, 486 refusals
    /// in a day, one every ~25 seconds, each a distinct hook-spawned process that
    /// refused and exited — while the same survey run by hand against that
    /// session was healthy. Nobody could tell from the message which state it had
    /// read, and this test is what keeps the messages distinguishable.
    /// **THE SURVEY ACCEPTS THE PRODUCT'S OWN WINDOW GRAMMAR.**
    ///
    /// `is_safe_logical_id` permits no colon — correct for an ORGANIZATION id,
    /// which is interpolated into tmux targets where `:` separates session from
    /// window. But the product's window ids are BUILT from a colon:
    /// `__person__:<person>`, `__overview__:<department>`, and the bare
    /// `__focus__`. So the survey rejected the grammar this codebase writes and
    /// voided on every session containing a person window — which is every real
    /// session, since person windows exist.
    ///
    /// Measured on a live box and named by #1224's own reason string
    /// within the hour: `window @1 carries an unsafe logical id
    /// '__person__:chief'`, one refusal every ~20 seconds, each a distinct
    /// hook-spawned process that refused and exited. `@1` is the Chief's window
    /// only because it is first in scan order.
    ///
    /// RED before this change: the live shape below voided.
    #[test]
    fn the_survey_accepts_the_product_window_grammar_and_nothing_else() {
        // The live shape from the box: a person window with its rail.
        let healthy = "@1\t%1\t__person__:chief\t1\n@1\t%2\t__person__:chief\t\n\
                       @4\t%6\t__focus__\t1\n@4\t%7\t__focus__\t\n\
                       @8\t%9\t__overview__:quant\t1\n@8\t%10\t__overview__:quant\t";
        let manifest =
            super::viewport_manifest_survey(healthy).expect("the product's own grammar is safe");
        let tags: Vec<&str> = manifest.iter().map(|entry| entry.window_tag.as_str()).collect();
        assert_eq!(tags, ["__person__:chief", "__focus__", "__overview__:quant"]);

        // AND NOTHING ELSE. The interpolation-safety property is preserved
        // where it varies: a colon-bearing tag that is not one of the product's
        // three shapes is still refused, and so is a suffix that could inject.
        for hostile in [
            "@7\t%11\tfoo:bar\t1",
            "@7\t%11\t__person__:exec}utive\t1",
            "@7\t%11\t__overview__:a;b\t1",
            "@7\t%11\t__person__:\t1",
        ] {
            let reason = super::viewport_manifest_survey(hostile)
                .expect_err("a non-product colon tag must still fail closed");
            assert!(reason.contains("unsafe logical id"), "{hostile}: {reason}");
        }
    }

    #[test]
    fn every_way_the_survey_can_void_names_what_it_read() {
        let reason = |listed: &str| super::viewport_manifest_survey(listed).expect_err(listed);
        // Nothing at all — tmux answered, and answered empty.
        assert!(reason("").contains("no panes at all"));
        // Panes, but none of them chief's.
        assert!(reason("@1\t%1\t\t\n").contains("no window in the session carries"));
        // An id that is not an id.
        assert!(reason("@x\t%11\texecutive\t1").contains("not ids"));
        // A tag this process must never interpolate into a tmux format.
        let unsafe_tag = reason("@7\t%11\texec}utive\t1");
        assert!(
            unsafe_tag.contains("@7") && unsafe_tag.contains("unsafe logical id"),
            "{unsafe_tag}"
        );
        // A sidebar tag that is neither absent nor "1".
        let odd_sidebar = reason("@7\t%11\texecutive\t2");
        assert!(
            odd_sidebar.contains("%11") && odd_sidebar.contains("sidebar tag"),
            "{odd_sidebar}"
        );
        // One window claiming two identities.
        let two_tags = reason("@7\t%11\texecutive\t1\n@7\t%12\tquant\t");
        assert!(two_tags.contains("tagged both"), "{two_tags}");
        // NON-VACUITY: the six reasons are six DIFFERENT sentences. A refactor
        // that collapsed them back into one would pass every `contains` above
        // only if it happened to contain all six phrases, and would fail here.
        let all = [
            reason(""),
            reason("@1\t%1\t\t\n"),
            reason("@x\t%11\texecutive\t1"),
            unsafe_tag,
            odd_sidebar,
            two_tags,
        ];
        let distinct: std::collections::BTreeSet<&str> = all.iter().map(String::as_str).collect();
        assert_eq!(distinct.len(), all.len(), "each void must read differently: {all:?}");
    }

    /// AN OPTION SET WITH `-F` IS A VALUE, NOT A FORMAT, and that is why the
    /// drag command can carry no epoch at all.
    ///
    /// #1196 replaced the literal generation number in
    /// `@chief_viewport_width_command` with
    /// `#{q:@chief_viewport_topology_epoch}`, expecting tmux to expand it when
    /// `MouseDragEnd1Border` fired. `set-option -F` expands at SET time, and
    /// `run-shell` does not re-expand a format that arrives through an option
    /// substitution, so the stored string held a frozen number either way and
    /// the change was a no-op. Measured on a live box with that binary
    /// installed: the command carried `25` against a live epoch of `26`.
    ///
    /// This asserts the argv the hook installs. It is deliberately NOT the
    /// evidence that a drag works — that is
    /// `real_border_drag_sticks_after_the_company_topology_moves_on`, which
    /// runs the whole chain through a real tmux server, because a test that
    /// pins the SHAPE of a command is exactly what passed while the product
    /// stayed broken.
    #[test]
    fn the_sidebar_width_command_carries_identity_and_never_an_epoch() {
        let argv = super::viewport_resize_hook_argv_for(
            std::path::Path::new("/root/.chief/bin/chief"),
            "chief-founder",
            "org-acme_",
            &[],
            26,
            false,
            4,
        );
        let width_command = argv
            .iter()
            .skip_while(|arg| arg.as_str() != trust::viewport_options::WIDTH_COMMAND)
            .nth(1)
            .expect("the hook installs a width command")
            .clone();

        assert!(
            !width_command.contains("topology_epoch"),
            "no epoch, live or frozen, may ride in the drag command: {width_command}"
        );
        assert!(
            !width_command.contains(" 4 "),
            "least of all the number that happened to be current when the hook was \
             installed: {width_command}"
        );
        // The identity guards are untouched — this removes WHAT the verb is
        // told about the topology, never WHO is allowed to commit a width.
        assert!(width_command.contains("#{q:@chief_viewport_server_nonce}"), "{width_command}");
        assert!(width_command.contains("#{q:session_id}"), "{width_command}");
        assert!(width_command.contains("#{q:@organization_id}"), "{width_command}");
        // tmux appends `#{pane_width}` as the last word, so the verb must see
        // exactly six operands. A seventh is the #1196 argv.
        let operands = width_command
            .split_whitespace()
            .skip_while(|word| *word != "viewport-sidebar-width")
            .count();
        assert_eq!(operands, 6, "verb plus five stored operands: {width_command}");
    }

    #[test]
    fn viewport_fast_path_accepts_default_width_and_a_rail_only_department() {
        let action = viewport_hook_action(
            Path::new("/opt/chief/bin/chief"),
            "default",
            "org-zipbox-ai_",
            &[
                super::ViewportManifestWindow {
                    window: "@1".to_owned(),
                    window_tag: "executive".to_owned(),
                    rail: "%7".to_owned(),
                    panes: vec![("%7".to_owned(), true)],
                },
                super::ViewportManifestWindow {
                    window: "@2".to_owned(),
                    window_tag: "__focus__".to_owned(),
                    rail: "%6".to_owned(),
                    panes: vec![("%6".to_owned(), true), ("%1".to_owned(), false)],
                },
            ],
            sidebar::brain::RAIL_DEFAULT_COLUMNS,
            false,
            10,
        );

        assert!(
            action.contains(
                "#{||:#{==:#{@chief_sidebar_columns},},#{==:#{@chief_sidebar_columns},26}}",
            ),
            "an absent preference is the product default, not stale authority: {action}",
        );
        assert_eq!(
            action.matches("#{pane_width},26").count(),
            1,
            "only a rail beside body panes can have the chosen width; a lone rail fills its window: {action}",
        );
    }

    #[test]
    fn hidden_viewport_routes_reject_unsafe_session_targets_before_tmux() {
        for session in ["org-acme;break_", "org-#{pane_id}_", "org-acme,_", "acme"] {
            assert!(super::refresh_viewport_manifest(
                "missing-socket",
                session,
                "1",
                VIEWPORT_NONCE,
            )
            .is_err());
            assert!(super::release_sidebar_width(
                "missing-socket",
                session,
                "acme",
                "$1",
                VIEWPORT_NONCE,
                "26",
            )
            .is_err());
        }
    }

    #[test]
    fn viewport_hook_fixture_uses_the_current_ci_shard_binary() {
        let source = viewport_test_callback_source(Path::new(
            "/tmp/target/debug/deps/chief-89df0d4af75b4673",
        ));
        assert!(source.contains("'/tmp/target/debug/deps/chief-89df0d4af75b4673'"));
        assert!(!source.contains("target/debug/chief "));
        assert!(source.contains("attach::tests::viewport_callback_child"));
        assert!(source.contains("attach::tests::viewport_client_eligible_child"));
        assert!(source.contains("attach::tests::viewport_session_changed_child"));
    }

    #[test]
    fn real_native_hook_publishes_before_the_blocked_callback() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-native-before-callback");
        let session = "org-acme_";
        start_session(&socket, session, &["-x", "240", "-y", "56", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-g", "status", "off"],
            vec!["set-option", "-t", session, "@organization_id", "acme"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
            vec!["set-option", "-w", "-t", session, "window-size", "manual"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "fixture option: {argv:?}");
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok(), "fixture rail: {}", rail.diagnostic());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());

        let directory = tempfile::tempdir().expect("production callback fixture");
        let executable = directory.path().join("production-callback");
        #[allow(clippy::disallowed_methods)]
        std::fs::write(
            &executable,
            viewport_test_callback_source(
                &std::env::current_exe().expect("current Chief test executable"),
            ),
        )
        .expect("stage production callback");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("executable callback");
        }

        let mut ordinary = std::process::Command::new("script")
            .args([
                "-q",
                "-c",
                &format!("tmux -L {socket} attach-session -t {session}"),
                "/dev/null",
            ])
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ordinary tmux client");
        let _ordinary_input = ordinary.stdin.take().expect("keep ordinary client attached");
        let mut ordinary_row = String::new();
        assert!(wait_until(2_000, || {
            ordinary_row = tmux::run(
                &socket,
                &["list-clients", "-F", "#{client_name}|#{client_pid}|#{client_flags}"],
            )
            .stdout
            .lines()
            .find(|line| !line.contains("control-mode"))
            .unwrap_or_default()
            .to_owned();
            !ordinary_row.is_empty()
        }));
        let ordinary_fields: Vec<&str> = ordinary_row.split('|').collect();
        assert_eq!(ordinary_fields.len(), 3, "ordinary client: {ordinary_row}");
        let ordinary_name = ordinary_fields[0].to_owned();
        let ordinary_pid = ordinary_fields[1].to_owned();
        assert!(std::process::Command::new("stty")
            .args(["-F", &ordinary_name, "cols", "240", "rows", "56"])
            .status()
            .is_ok_and(|status| status.success()));

        let listed = tmux::run(
            &socket,
            &[
                "list-panes",
                "-s",
                "-t",
                session,
                "-F",
                "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
            ],
        );
        let manifest =
            super::viewport_manifest_survey(&listed.stdout).expect("a complete viewport survey");
        let body = manifest[0]
            .panes
            .iter()
            .find_map(|(pane, sidebar)| (!sidebar).then_some(pane.clone()))
            .expect("one exact body pane");
        let window = manifest[0].window.clone();
        install_viewport_hooks(&executable, &socket, session, &manifest, 26, false);
        assert!(
            wait_until(3_000, || {
                tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    session,
                    "#{@chief_viewport_fast_session}|#{@chief_viewport_fast_owner}|#{@chief_viewport_fast_organization}|#{@chief_viewport_fast_generation}|#{@chief_viewport_membership_generation}",
                ],
            )
            .stdout
                == format!("{session}|{ordinary_name}|acme|1|1")
            }),
            "the real census did not grant the sole ordinary client"
        );

        let mut control = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-C",
                "attach-session",
                "-f",
                "no-output,ignore-size",
                "-t",
                session,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("control ignore-size client");
        let _control_input = control.stdin.take().expect("keep control attached");
        assert!(
            wait_until(3_000, || {
                let state = tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    session,
                    "#{@chief_viewport_fast_owner}|#{@chief_viewport_fast_generation}|#{@chief_viewport_membership_generation}",
                ],
            )
            .stdout;
                let fields: Vec<&str> = state.split('|').collect();
                fields.len() == 3
                    && fields[0] == ordinary_name
                    && fields[1] == fields[2]
                    && fields[1].parse::<u64>().is_ok_and(|generation| generation >= 2)
            }),
            "the later control client displaced the ordinary census owner"
        );

        for (columns, rows) in [(360, 84), (240, 56)] {
            let barrier_path = directory.path().join(format!("barrier-{columns}-{rows}.sock"));
            let barrier = std::os::unix::net::UnixListener::bind(&barrier_path)
                .expect("viewport callback barrier");
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                use std::io::{Read as _, Write as _};
                let (mut callback, _) = barrier.accept().expect("callback enters barrier");
                let mut entered = [0_u8; 7];
                callback.read_exact(&mut entered).expect("callback entry");
                assert_eq!(&entered, b"entered");
                entered_tx.send(()).expect("announce callback entry");
                release_rx.recv().expect("release callback");
                callback.write_all(b"1").expect("release byte");
                let mut complete = [0_u8; 8];
                callback.read_exact(&mut complete).expect("callback completion");
                assert_eq!(&complete, b"complete");
            });
            assert!(tmux::run(
                &socket,
                &[
                    "set-environment",
                    "-g",
                    "CHIEF_TEST_VIEWPORT_BARRIER",
                    &barrier_path.display().to_string(),
                ],
            )
            .ok());
            assert!(std::process::Command::new("stty")
                .args([
                    "-F",
                    &ordinary_name,
                    "cols",
                    &columns.to_string(),
                    "rows",
                    &rows.to_string(),
                ])
                .status()
                .is_ok_and(|status| status.success()));
            assert!(std::process::Command::new("kill")
                .args(["-WINCH", &ordinary_pid])
                .status()
                .is_ok_and(|status| status.success()));
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .expect("the asynchronous callback entered and stayed blocked");
            let state = tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    session,
                    "S|#{session_windows}|#{@chief_sidebar_columns}|#{@chief_sidebar_collapsed}|#{@chief_viewport_preflight}|#{@chief_viewport_fast_owner}|#{@chief_viewport_fast_generation}|#{@chief_viewport_membership_generation}",
                    ";",
                    "list-windows",
                    "-t",
                    session,
                    "-F",
                    "W|#{window_id}|#{window_width}|#{window_height}|#{window_panes}|#{@organization_window_id}",
                    ";",
                    "list-panes",
                    "-s",
                    "-t",
                    session,
                    "-F",
                    "P|#{pane_id}|#{window_id}|#{pane_width}|#{pane_height}|#{@organization_sidebar}|#{pane_in_mode}",
                    ";",
                    "show-options",
                    "-wv",
                    "-t",
                    session,
                    "window-size",
                ],
            )
            .stdout;
            let lines: Vec<&str> = state.lines().collect();
            assert_eq!(lines.len(), 5, "one session, window, two panes, and option: {state}");
            let session_fields: Vec<&str> = lines[0].split('|').collect();
            assert_eq!(session_fields.len(), 8, "session facts: {}", lines[0]);
            assert_eq!(&session_fields[..5], ["S", "1", "26", "", ""]);
            assert_eq!(session_fields[5], ordinary_name);
            assert_eq!(session_fields[6], session_fields[7]);
            assert!(session_fields[6].parse::<u64>().is_ok());
            assert_eq!(
                lines[1],
                format!("W|{window}|{columns}|{rows}|2|executive"),
                "the first assertion after hook dispatch must already contain the exact window"
            );
            let panes: std::collections::BTreeSet<String> =
                lines[2..4].iter().map(|line| (*line).to_owned()).collect();
            assert_eq!(
                panes,
                std::collections::BTreeSet::from([
                    format!("P|{}|{window}|26|{rows}|1|0", rail.stdout),
                    format!("P|{body}|{window}|{}|{rows}||0", columns - 27),
                ]),
                "one exact rail and one exact body fill the complete frame"
            );
            assert_eq!(lines[4], "manual");
            release_tx.send(()).expect("release callback");
            thread.join().expect("callback barrier thread");
            assert!(
                tmux::run(&socket, &["set-environment", "-gu", "CHIEF_TEST_VIEWPORT_BARRIER"],)
                    .ok()
            );
        }
        drop(control);
        drop(ordinary);
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_stale_manifest_refresh_cannot_replace_a_newer_company_epoch() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-stale-manifest");
        let session = "org-stale_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "stale"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
            vec!["set-option", "-t", session, "@chief_viewport_topology_epoch", "2"],
            vec!["set-option", "-t", session, "@chief_viewport_manifest_epoch", "2"],
            vec!["set-hook", "-t", session, "client-resized", "display-message sentinel"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "stale fixture: {argv:?}");
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());
        super::refresh_viewport_manifest(&socket, session, "1", &viewport_server_nonce(&socket))
            .expect("a stale refresh is a clean CAS loss");
        assert!(tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"])
            .stdout
            .contains("sentinel"));
        assert_eq!(
            tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_manifest_epoch"]
            )
            .stdout,
            "2"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_post_attach_worker_reap_replaces_the_stale_viewport_manifest() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-post-attach-reap");
        let session = "org-post-attach-reap_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "cobalt"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "portfolio"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "reap fixture option: {argv:?}");
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok(), "reap rail: {}", rail.diagnostic());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());
        let worker = tmux::run(
            &socket,
            &["split-window", "-t", session, "-P", "-F", "#{pane_id}", "sleep", "120"],
        );
        assert!(worker.ok(), "reap worker: {}", worker.diagnostic());
        for (option, value) in [
            ("@organization_id", "cobalt"),
            ("@organization_window_id", "portfolio"),
            ("@organization_person_id", "pm-lead"),
            ("@organization_launch_hash", "hash-pm-lead"),
        ] {
            assert!(
                tmux::run(&socket, &["set-option", "-p", "-t", &worker.stdout, option, value],)
                    .ok()
            );
        }

        let fixture = tempfile::tempdir().expect("manifest callback fixture");
        let callback = fixture.path().join("production-callback");
        let source = viewport_test_callback_source(
            &std::env::current_exe().expect("current chief test executable"),
        );
        #[allow(clippy::disallowed_methods)]
        std::fs::write(&callback, source).expect("stage manifest callback");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&callback, std::fs::Permissions::from_mode(0o755))
                .expect("make manifest callback executable");
        }
        let bootstrap = viewport_hook_bootstrap_argv(&callback, &socket, session);
        let bootstrap_refs: Vec<&str> = bootstrap.iter().map(String::as_str).collect();
        let bootstrapped = tmux::run(&socket, &bootstrap_refs);
        assert!(bootstrapped.ok(), "reap bootstrap: {}", bootstrapped.diagnostic());
        let (initial_epoch, nonce, _) =
            viewport_bootstrap_authority(&bootstrapped.stdout).expect("reap authority");
        assert_eq!(initial_epoch, 1);
        super::refresh_viewport_manifest(&socket, session, "1", &nonce)
            .expect("install the manifest that still counts the worker");
        // THE WORKER IS COUNTED, NOT NAMED. The hook proves a window's shape
        // with `window_panes`, never with a proof per pane — one company's
        // panes used to take the install past tmux's own command ceiling.
        // Three panes here: the rail, the person, and the worker about to die.
        assert!(tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"])
            .stdout
            .contains("window_panes},3"));

        let completion = fixture.path().join("refresh-complete");
        assert!(tmux::run(
            &socket,
            &[
                "set-environment",
                "-g",
                "CHIEF_TEST_MANIFEST_DONE",
                completion.to_str().expect("completion path"),
            ],
        )
        .ok());
        let window =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{window_id}"]).stdout;
        let desired = chief_cli::placement::Topology {
            organization: "cobalt".into(),
            session: session.into(),
            windows: vec![chief_cli::placement::Window {
                logical_id: "portfolio".into(),
                name: "Portfolio".into(),
                panes: Vec::new(),
            }],
            known_person_ids: Default::default(),
        };
        let observed = chief_cli::actuate::plan::ObservedTopology {
            session_exists: true,
            session_organization: "cobalt".into(),
            windows: vec![chief_cli::actuate::plan::ObservedWindow {
                tmux_id: window.clone(),
                organization_id: "cobalt".into(),
                logical_id: "portfolio".into(),
                protected_ui: true,
                sleeping_notice: false,
            }],
            panes: vec![chief_cli::actuate::plan::ObservedPane {
                tmux_id: worker.stdout.clone(),
                tmux_window_id: window,
                organization_id: "cobalt".into(),
                logical_window_id: "portfolio".into(),
                person_id: "pm-lead".into(),
                launch_hash: "hash-pm-lead".into(),
                start_command: "sleep 120".into(),
            }],
        };
        let plan = chief_cli::actuate::plan::ConvergePlan {
            steps: vec![chief_cli::actuate::plan::Step::KillPane {
                pane: chief_cli::actuate::plan::PaneId(worker.stdout.clone()),
            }],
            ..Default::default()
        };
        let report = chief_cli::actuate::apply_plan(
            &chief_cli::real::RealHostExecutor::production(),
            &chief_cli::actuate::Socket(socket.clone()),
            &desired,
            &observed,
            &Default::default(),
            &plan,
        );
        assert_eq!(report.steps_ok, 1, "real KillPane applies: {:?}", report.failure);
        assert!(wait_until(2_000, || completion.exists()), "hidden refresh route completed");
        assert_eq!(
            std::fs::read_to_string(&completion).expect("refresh result"),
            "ok",
            "the production stored command passes epoch 2 to Chief"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    session,
                    "#{@chief_viewport_topology_epoch}:#{@chief_viewport_manifest_epoch}"
                ],
            )
            .stdout,
            "2:2"
        );
        let final_hook = tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"]);
        assert!(
            !final_hook.stdout.contains("window_panes},3"),
            "the reaped worker is gone from the manifest: {}",
            final_hook.stdout
        );
        assert!(
            final_hook.stdout.contains("window_panes},2"),
            "the rail and the person are what is left: {}",
            final_hook.stdout
        );
        assert!(final_hook.stdout.contains(&rail.stdout));
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn attach_bootstrap_keeps_async_authority_and_a_stale_final_install_cannot_win() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-attach-install-cas");
        let session = "org-attach-cas_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "attach-cas"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
        ] {
            assert!(tmux::run(&socket, &argv).ok());
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());
        let executable = Path::new("/bin/true");
        let bootstrap = viewport_hook_bootstrap_argv(executable, &socket, session);
        let refs: Vec<&str> = bootstrap.iter().map(String::as_str).collect();
        let authority = tmux::run(&socket, &refs);
        assert!(authority.ok(), "bootstrap: {}", authority.diagnostic());
        let (old_epoch, nonce, _) =
            viewport_bootstrap_authority(&authority.stdout).expect("bootstrap authority");
        let early = tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"]).stdout;
        assert!(early.contains("viewport-resize"));
        assert!(!early.contains("resize-window -A"), "bootstrap hook is async-only: {early}");
        let listed = tmux::run(
            &socket,
            &[
                "list-panes",
                "-s",
                "-t",
                session,
                "-F",
                "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
            ],
        );
        let manifest =
            super::viewport_manifest_survey(&listed.stdout).expect("a complete viewport survey");
        let newer_output = tmux::run(
            &socket,
            &[
                "set-option",
                "-gF",
                "@chief_viewport_topology_generation",
                "#{e|+:#{@chief_viewport_topology_generation},1}",
                ";",
                "set-option",
                "-F",
                "-t",
                session,
                "@chief_viewport_topology_epoch",
                "#{@chief_viewport_topology_generation}",
                ";",
                "display-message",
                "-p",
                "-t",
                session,
                "#{@chief_viewport_topology_epoch}",
            ],
        );
        let newer = newer_output.stdout.parse::<u64>().expect("numeric newer epoch");
        assert!(newer > old_epoch);
        assert!(!install_viewport_manifest_if_current(
            executable,
            &socket,
            session,
            ("attach-cas", &nonce, old_epoch),
            &manifest,
            (26, false),
        )
        .expect("old attach finalizer loses cleanly"));
        let still_early =
            tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"]).stdout;
        assert!(!still_early.contains("resize-window -A"));
        assert!(install_viewport_manifest_if_current(
            executable,
            &socket,
            session,
            ("attach-cas", &nonce, newer),
            &manifest,
            (26, false),
        )
        .expect("current retry installs"));
        let final_hook =
            tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"]).stdout;
        assert!(final_hook.contains("resize-window -A"));
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_attach_viewport_retry_reacquires_after_actuator_topology_churn() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-attach-retry");
        let session = "org-attach-retry_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "attach-retry"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "retry fixture option: {argv:?}");
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok(), "retry rail: {}", rail.diagnostic());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());
        let panes_before =
            tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout;
        let attempts = std::cell::Cell::new(0_u64);
        retry_attach_viewport(
            || acquire_attach_viewport_authority(Path::new("/bin/true"), &socket, session),
            |authority| {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    let newer = tmux::run(
                        &socket,
                        &[
                            "set-option",
                            "-gF",
                            "@chief_viewport_topology_generation",
                            "#{e|+:#{@chief_viewport_topology_generation},1}",
                            ";",
                            "set-option",
                            "-F",
                            "-t",
                            session,
                            "@chief_viewport_topology_epoch",
                            "#{@chief_viewport_topology_generation}",
                        ],
                    );
                    assert!(newer.ok(), "deterministic actuator churn: {}", newer.diagnostic());
                }
                publish_attach_viewport_once(
                    Path::new("/bin/true"),
                    &socket,
                    session,
                    authority,
                    (120, 30),
                    (26, false),
                )
            },
        )
        .expect("bare chief reacquires after the actuator changes topology once");
        assert_eq!(attempts.get(), 2, "one stale epoch needs one current retry");
        assert_eq!(
            tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout,
            panes_before,
            "viewport retry cannot add a second rail or a temporary body pane"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    session,
                    "#{@chief_viewport_topology_epoch}:#{@chief_viewport_manifest_epoch}",
                ],
            )
            .stdout
            .split_once(':')
            .map(|(topology, manifest)| topology == manifest),
            Some(true),
            "the accepted manifest belongs to the retried topology"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    /// A REAL ATTACH LEAVES THE CURSOR ON THE PERSON.
    ///
    /// This is the operator's own report: a bare `chief` opened the company
    /// with the cursor in the sidebar rather than in the pane they type into.
    /// The argv test above pins the frame; only tmux can say what the frame
    /// leaves active, so the mint frame is run here against a real server.
    /// `remain-on-exit` keeps the rail pane after its stand-in program exits.
    #[test]
    fn a_real_rail_mint_leaves_the_person_pane_active() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("attach-rail-cursor");
        let session = "org-attach-cursor_";
        start_session(&socket, session, &["-x", "240", "-y", "56", "sleep", "120"]);
        assert!(
            tmux::run(&socket, &["set-option", "-w", "-t", session, "remain-on-exit", "on"]).ok()
        );
        let person = tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"])
            .stdout
            .trim()
            .to_owned();
        let window = tmux::run(&socket, &["display-message", "-p", "-t", session, "#{window_id}"])
            .stdout
            .trim()
            .to_owned();

        let argv = rail_mint_argv(session, &window, Path::new("/tmp"), Path::new("/bin/false"), 26);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let minted = tmux::run(&socket, &refs);
        assert!(minted.ok(), "the mint frame ran: {}", minted.diagnostic());

        let panes = tmux::run(
            &socket,
            &[
                "list-panes",
                "-t",
                session,
                "-F",
                &format!("#{{pane_id}}\t#{{pane_active}}\t#{{{}}}", trust::tags::SIDEBAR),
            ],
        )
        .stdout;
        let _ = tmux::run(&socket, &["kill-server"]);

        let rows: Vec<Vec<&str>> = panes.lines().map(|line| line.split('\t').collect()).collect();
        assert_eq!(rows.len(), 2, "the person pane and its new rail: {panes}");
        let active: Vec<&str> = rows.iter().filter(|row| row[1] == "1").map(|row| row[0]).collect();
        assert_eq!(active, vec![person.as_str()], "the operator keeps their own pane: {panes}");
        let rail = rows.iter().find(|row| row[0] != person).expect("the minted rail");
        assert_eq!(rail[2], "1", "and the rail it did not move to is tagged: {panes}");
    }

    #[test]
    fn attach_rail_mint_refuses_after_the_captured_company_epoch_is_replaced() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-attach-rail-cas");
        let session = "org-attach-rail_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        assert!(tmux::run(
            &socket,
            &["set-option", "-t", session, "@organization_id", "attach-rail"],
        )
        .ok());
        let bootstrap = viewport_hook_bootstrap_argv(Path::new("/bin/true"), &socket, session);
        let refs: Vec<&str> = bootstrap.iter().map(String::as_str).collect();
        let authority = tmux::run(&socket, &refs);
        let (old_epoch, nonce, _) =
            viewport_bootstrap_authority(&authority.stdout).expect("bootstrap authority");
        assert!(tmux::run(
            &socket,
            &[
                "set-option",
                "-gF",
                "@chief_viewport_topology_generation",
                "#{e|+:#{@chief_viewport_topology_generation},1}",
                ";",
                "set-option",
                "-F",
                "-t",
                session,
                "@chief_viewport_topology_epoch",
                "#{@chief_viewport_topology_generation}",
            ],
        )
        .ok());
        let before = tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"])
            .stdout
            .lines()
            .count();
        let command = vec![
            "split-window".to_owned(),
            "-h".to_owned(),
            "-t".to_owned(),
            session.to_owned(),
            "sleep".to_owned(),
            "120".to_owned(),
        ];
        let error = super::run_attach_mutation_if_current(
            &socket,
            session,
            "attach-rail",
            &nonce,
            old_epoch,
            &command,
        )
        .expect_err("an old attach cannot split the replacement company session");
        assert!(error.to_string().contains("became stale"), "{error}");
        let after = tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"])
            .stdout
            .lines()
            .count();
        assert_eq!(before, after, "the stale split stays behind the guard");
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn old_manifest_callback_cannot_write_after_abnormal_same_socket_server_restart() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-server-nonce-aba");
        let session = "org-server-nonce_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        assert!(tmux::run(
            &socket,
            &["set-option", "-t", session, "@organization_id", "server-nonce"],
        )
        .ok());
        let old_bootstrap = viewport_hook_bootstrap_argv(Path::new("/bin/true"), &socket, session);
        let old_refs: Vec<&str> = old_bootstrap.iter().map(String::as_str).collect();
        let old_authority = tmux::run(&socket, &old_refs);
        let (old_epoch, old_nonce, _) =
            viewport_bootstrap_authority(&old_authority.stdout).expect("old authority");
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (complete_tx, complete_rx) = std::sync::mpsc::channel();
        let callback_socket = socket.clone();
        let callback_session = session.to_owned();
        let callback_nonce = old_nonce.clone();
        let callback = std::thread::spawn(move || {
            release_rx.recv().expect("release old callback");
            let result = super::refresh_viewport_manifest(
                &callback_socket,
                &callback_session,
                &old_epoch.to_string(),
                &callback_nonce,
            );
            complete_tx.send(result).expect("callback completion");
        });
        let old_pid =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{pid}"]).stdout;
        assert!(std::process::Command::new("kill")
            .args(["-KILL", old_pid.as_str()])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(wait_until(2_000, || !tmux::run(&socket, &["has-session", "-t", session]).ok()));

        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "server-nonce"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
        ] {
            assert!(tmux::run(&socket, &argv).ok());
        }
        let new_bootstrap = viewport_hook_bootstrap_argv(Path::new("/bin/true"), &socket, session);
        let new_refs: Vec<&str> = new_bootstrap.iter().map(String::as_str).collect();
        let new_authority = tmux::run(&socket, &new_refs);
        let (new_epoch, new_nonce, _) =
            viewport_bootstrap_authority(&new_authority.stdout).expect("new authority");
        assert_eq!(old_epoch, new_epoch, "the counter intentionally repeats across servers");
        assert_ne!(old_nonce, new_nonce, "the server nonce is the ABA fence");
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());
        assert!(tmux::run(
            &socket,
            &["set-hook", "-t", session, "client-resized", "display-message replacement"],
        )
        .ok());
        release_tx.send(()).expect("release old callback after replacement server exists");
        complete_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("old callback completed")
            .expect("nonce mismatch is a clean stale manifest result");
        callback.join().expect("old callback thread");
        let hook = tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"]).stdout;
        assert!(hook.contains("replacement"), "old callback cannot replace the new hook: {hook}");
        assert_eq!(
            tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_manifest_epoch"],
            )
            .stdout,
            "",
            "old callback cannot write a manifest epoch on the replacement server"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_native_preflight_refuses_a_missing_pane_or_window() {
        if !require_tmux() {
            return;
        }
        for missing_window in [false, true] {
            let socket = unique_socket(if missing_window {
                "viewport-missing-window"
            } else {
                "viewport-missing-pane"
            });
            let session = if missing_window { "org-missing-window_" } else { "org-missing-pane_" };
            start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
            for argv in [
                vec!["set-option", "-t", session, "@organization_id", "missing"],
                vec!["set-option", "-w", "-t", session, "@organization_window_id", "first"],
                vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
                vec!["set-option", "-t", session, "@chief_viewport_topology_epoch", "7"],
                vec!["set-option", "-t", session, "@chief_viewport_manifest_epoch", "7"],
                vec!["set-option", "-g", "@chief_viewport_membership_generation", "9"],
                vec!["set-option", "-g", "@chief_viewport_fast_generation", "9"],
                vec!["set-option", "-g", "@chief_viewport_fast_session", session],
                vec!["set-option", "-g", "@chief_viewport_fast_owner", ""],
                vec!["set-option", "-g", "@chief_viewport_fast_organization", "missing"],
            ] {
                assert!(tmux::run(&socket, &argv).ok(), "fixture option: {argv:?}");
            }
            let first_rail = tmux::run(
                &socket,
                &[
                    "split-window",
                    "-h",
                    "-b",
                    "-l",
                    "26",
                    "-t",
                    session,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "sleep",
                    "120",
                ],
            );
            assert!(first_rail.ok());
            assert!(tmux::run(
                &socket,
                &["set-option", "-p", "-t", &first_rail.stdout, "@organization_sidebar", "1"],
            )
            .ok());
            let second_window = if missing_window {
                let window = tmux::run(
                    &socket,
                    &[
                        "new-window",
                        "-d",
                        "-t",
                        session,
                        "-P",
                        "-F",
                        "#{window_id}",
                        "sleep",
                        "120",
                    ],
                );
                assert!(window.ok());
                assert!(tmux::run(
                    &socket,
                    &[
                        "set-option",
                        "-w",
                        "-t",
                        &window.stdout,
                        "@organization_window_id",
                        "second",
                    ],
                )
                .ok());
                let rail = tmux::run(
                    &socket,
                    &[
                        "split-window",
                        "-h",
                        "-b",
                        "-l",
                        "26",
                        "-t",
                        &window.stdout,
                        "-P",
                        "-F",
                        "#{pane_id}",
                        "sleep",
                        "120",
                    ],
                );
                assert!(rail.ok());
                assert!(tmux::run(
                    &socket,
                    &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
                )
                .ok());
                Some(window.stdout)
            } else {
                None
            };
            let listed = tmux::run(
                &socket,
                &[
                    "list-panes", "-s", "-t", session, "-F",
                    "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
                ],
            );
            let manifest = super::viewport_manifest_survey(&listed.stdout)
                .expect("a complete viewport survey");
            let action = viewport_hook_action(
                Path::new("/bin/false"),
                &socket,
                session,
                &manifest,
                26,
                false,
                7,
            );
            assert!(
                tmux::run(&socket, &["set-hook", "-t", session, "client-resized", &action],).ok()
            );
            if let Some(window) = second_window {
                assert!(tmux::run(&socket, &["kill-window", "-t", &window]).ok());
            } else {
                assert!(tmux::run(&socket, &["kill-pane", "-t", &first_rail.stdout]).ok());
            }
            assert!(tmux::run(
                &socket,
                &["set-option", "-w", "-t", session, "window-size", "latest"]
            )
            .ok());
            let _ = tmux::run(&socket, &["run-hook", "-t", session, "client-resized"]);
            assert_eq!(
                tmux::run(&socket, &["show-options", "-wv", "-t", session, "window-size"]).stdout,
                "latest",
                "a missing cached {} must not earn the complete native proof",
                if missing_window { "window" } else { "pane" }
            );
            assert_eq!(
                tmux::run(
                    &socket,
                    &["show-options", "-qv", "-t", session, "@chief_viewport_preflight"],
                )
                .stdout,
                "",
                "the event-local proof counter is always removed"
            );
            let _ = tmux::run(&socket, &["kill-server"]);
        }
    }

    #[test]
    fn real_topology_epochs_are_monotonic_and_isolated_per_company() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-company-epochs");
        let first = "org-first_";
        let second = "org-second_";
        start_session(&socket, first, &["sleep", "120"]);
        start_session(&socket, second, &["sleep", "120"]);
        for (session, organization) in [(first, "first"), (second, "second")] {
            assert!(tmux::run(
                &socket,
                &["set-option", "-t", session, "@organization_id", organization],
            )
            .ok());
        }
        let bootstrap = |session: &str| {
            let argv = viewport_hook_bootstrap_argv(Path::new("/bin/true"), &socket, session);
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            let result = tmux::run(&socket, &refs);
            assert!(result.ok(), "bootstrap {session}: {}", result.diagnostic());
            viewport_bootstrap_authority(&result.stdout).expect("bootstrap authority").0
        };
        let first_epoch = bootstrap(first);
        let first_nonce = viewport_server_nonce(&socket);
        let second_epoch = bootstrap(second);
        let second_nonce = viewport_server_nonce(&socket);
        assert!(second_epoch > first_epoch, "the server allocator is monotonic");
        assert_eq!(
            first_nonce, second_nonce,
            "all companies on one tmux server share one server-lifetime nonce"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &["show-options", "-qv", "-t", first, "@chief_viewport_topology_epoch"],
            )
            .stdout,
            first_epoch.to_string(),
            "another company must not replace the first company's epoch"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &["show-options", "-qv", "-t", second, "@chief_viewport_topology_epoch"],
            )
            .stdout,
            second_epoch.to_string()
        );
        let other_socket = unique_socket("viewport-other-server-nonce");
        start_session(&other_socket, first, &["sleep", "120"]);
        assert!(tmux::run(
            &other_socket,
            &["set-option", "-t", first, "@organization_id", "first"],
        )
        .ok());
        let other_argv = viewport_hook_bootstrap_argv(Path::new("/bin/true"), &other_socket, first);
        let other_refs: Vec<&str> = other_argv.iter().map(String::as_str).collect();
        let other = tmux::run(&other_socket, &other_refs);
        assert!(other.ok(), "other server bootstrap: {}", other.diagnostic());
        let (_, other_nonce, _) =
            viewport_bootstrap_authority(&other.stdout).expect("other server authority");
        assert_ne!(first_nonce, other_nonce, "a new tmux server gets a new nonce");
        let _ = tmux::run(&other_socket, &["kill-server"]);
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    /// THE OPERATOR'S DRAG, END TO END, ON A COMPANY THAT HAS MOVED ON.
    ///
    /// This is the test #1196 needed and did not have. That change asserted the
    /// SHAPE of `@chief_viewport_width_command` — that it contained the text
    /// `#{q:@chief_viewport_topology_epoch}` — and passed while the product
    /// stayed broken, because `set-option -F` expands every `#{...}` in a value
    /// AT SET TIME. The stored string held a frozen number either way.
    ///
    /// So this drives the whole chain instead: the production hook set is
    /// installed with the production builder, the company's topology epoch is
    /// then advanced past the install (which is the ordinary resting state of a
    /// live company — the manifest refresh that follows an epoch bump is fired
    /// `run-shell -b`, and `park_focus_window_if_still_empty` bumps with no
    /// refresh at all), and the drag is committed by running the command tmux
    /// itself stores, in a real `run-shell -b` job, through a fixture that
    /// re-enters this binary the way tmux re-enters `chief`.
    ///
    /// The pass/fail signature is the operator's: `@chief_sidebar_columns` is
    /// the width they dragged to, and every rail is that wide.
    ///
    /// The one thing no test here can do is move a physical mouse. tmux offers
    /// no way to synthesize a `MouseDragEnd1Border` event, so this fires the
    /// binding's own payload — `#{@chief_viewport_width_command}` with the
    /// released `#{pane_width}` appended — which is the binding minus its
    /// `if-shell` guard on the pane under the pointer. That guard is pinned
    /// separately, in `actuate::interpret::tests`.
    #[test]
    fn real_border_drag_sticks_after_the_company_topology_moves_on() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-width-drag");
        let session = "org-widthdrag_";
        start_session(&socket, session, &["-x", "180", "-y", "40", "sleep", "120"]);
        assert!(tmux::run(
            &socket,
            &["set-option", "-t", session, "@organization_id", "widthdrag"],
        )
        .ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
        )
        .ok());
        let second = tmux::run(
            &socket,
            &["new-window", "-d", "-t", session, "-P", "-F", "#{window_id}", "sleep", "120"],
        );
        assert!(second.ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-w", "-t", &second.stdout, "@organization_window_id", "research"],
        )
        .ok());
        let mut rails = Vec::new();
        for window in [session, second.stdout.as_str()] {
            let rail = tmux::run(
                &socket,
                &[
                    "split-window",
                    "-h",
                    "-b",
                    "-l",
                    "26",
                    "-t",
                    window,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "sleep",
                    "120",
                ],
            );
            assert!(rail.ok(), "fixture rail: {}", rail.diagnostic());
            assert!(tmux::run(
                &socket,
                &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
            )
            .ok());
            rails.push(rail.stdout);
        }

        // The production hook set, installed by the production builder. This is
        // the `set-option -F` that writes `@chief_viewport_width_command`.
        let fixture = tempfile::tempdir().expect("drag fixture directory");
        let callback = fixture.path().join("production-drag");
        #[allow(clippy::disallowed_methods)]
        std::fs::write(
            &callback,
            viewport_test_callback_source(
                &std::env::current_exe().expect("current chief test executable"),
            ),
        )
        .expect("stage drag callback");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&callback, std::fs::Permissions::from_mode(0o755))
                .expect("make drag callback executable");
        }
        let listed = tmux::run(
            &socket,
            &[
                "list-panes",
                "-s",
                "-t",
                session,
                "-F",
                "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
            ],
        );
        let manifest =
            super::viewport_manifest_survey(&listed.stdout).expect("a complete viewport survey");
        let installed_epoch =
            install_viewport_hooks(&callback, &socket, session, &manifest, 26, false);
        let stored = tmux::run(
            &socket,
            &["show-options", "-qv", "-t", session, trust::viewport_options::WIDTH_COMMAND],
        )
        .stdout;
        assert!(
            !stored.contains(&format!(" {installed_epoch} ")),
            "no epoch may be frozen into the drag command: {stored}"
        );

        // THE COMPANY MOVES ON. Exactly the two tmux commands the rail brain's
        // `invalidate_viewport_topology` runs before every batch, twice, and no
        // manifest refresh behind them — so `@chief_viewport_manifest_epoch`
        // is left behind as well, which is the state a real drag lands in.
        for _ in 0..2 {
            assert!(tmux::run(
                &socket,
                &[
                    "set-option",
                    "-gF",
                    trust::viewport_options::TOPOLOGY_GENERATION,
                    &format!("#{{e|+:#{{{}}},1}}", trust::viewport_options::TOPOLOGY_GENERATION),
                    ";",
                    "set-option",
                    "-F",
                    "-t",
                    session,
                    trust::viewport_options::TOPOLOGY_EPOCH,
                    &format!("#{{{}}}", trust::viewport_options::TOPOLOGY_GENERATION),
                ],
            )
            .ok());
        }
        let moved = tmux::run(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                session,
                "#{@chief_viewport_topology_epoch}|#{@chief_viewport_manifest_epoch}",
            ],
        )
        .stdout;
        let moved: Vec<&str> = moved.split('|').collect();
        assert_ne!(moved[0], installed_epoch.to_string(), "the topology epoch has moved on");
        assert_ne!(moved[0], moved[1], "and the manifest refresh has not caught up");
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"])
                .stdout,
            "",
            "the operator has never yet had a width recorded"
        );

        // THE DRAG. The binding's own payload, in a real background job.
        let completion = fixture.path().join("drag-complete");
        assert!(tmux::run(
            &socket,
            &[
                "set-environment",
                "-g",
                "CHIEF_TEST_WIDTH_DONE",
                completion.to_str().expect("completion path"),
            ],
        )
        .ok());
        let fired = tmux::run(
            &socket,
            &[
                "run-shell",
                "-b",
                "-t",
                &rails[0],
                &format!("#{{{}}} 41", trust::viewport_options::WIDTH_COMMAND),
            ],
        );
        assert!(fired.ok(), "fire the drag: {}", fired.diagnostic());
        let mut verdict = String::new();
        assert!(
            wait_until(30_000, || {
                verdict = std::fs::read_to_string(&completion).unwrap_or_default();
                !verdict.is_empty()
            }),
            "the drag commit never reported"
        );
        assert_eq!(verdict, "ok", "the operator's drag must be committed, not refused");

        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"])
                .stdout,
            "41",
            "the width the operator released is the company's recorded preference"
        );
        let state = tmux::run(
            &socket,
            &["list-panes", "-s", "-t", session, "-F", "#{@organization_sidebar}|#{pane_width}"],
        )
        .stdout;
        assert_eq!(
            state.lines().filter(|line| *line == "1|41").count(),
            2,
            "every rail in the company takes it: {state}"
        );
        let repaired = tmux::run(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                session,
                "#{@chief_viewport_topology_epoch}|#{@chief_viewport_manifest_epoch}",
            ],
        )
        .stdout;
        let repaired: Vec<&str> = repaired.split('|').collect();
        assert_eq!(
            repaired[0], repaired[1],
            "and the drag re-installs the manifest at the epoch it minted: {repaired:?}"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_sidebar_width_release_updates_all_rails_and_one_company_epoch() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-width-release");
        let session = "org-width_";
        start_session(&socket, session, &["-x", "180", "-y", "40", "sleep", "120"]);
        assert!(
            tmux::run(&socket, &["set-option", "-t", session, "@organization_id", "width"]).ok()
        );
        assert!(tmux::run(
            &socket,
            &["set-option", "-t", session, "@chief_viewport_topology_epoch", "1"],
        )
        .ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-t", session, "@chief_viewport_manifest_epoch", "1"],
        )
        .ok());
        let session_id =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{session_id}"]).stdout;
        assert!(tmux::run(
            &socket,
            &["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
        )
        .ok());
        let second = tmux::run(
            &socket,
            &["new-window", "-d", "-t", session, "-P", "-F", "#{window_id}", "sleep", "120"],
        );
        assert!(second.ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-w", "-t", &second.stdout, "@organization_window_id", "research"],
        )
        .ok());
        for window in [session, second.stdout.as_str()] {
            let rail = tmux::run(
                &socket,
                &[
                    "split-window",
                    "-h",
                    "-b",
                    "-l",
                    "26",
                    "-t",
                    window,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "sleep",
                    "120",
                ],
            );
            assert!(rail.ok());
            assert!(tmux::run(
                &socket,
                &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
            )
            .ok());
        }
        super::release_sidebar_width(
            &socket,
            session,
            "width",
            &session_id,
            &viewport_server_nonce(&socket),
            "31",
        )
        .expect("width release");
        let state = tmux::run(
            &socket,
            &["list-panes", "-s", "-t", session, "-F", "#{@organization_sidebar}|#{pane_width}"],
        )
        .stdout;
        assert_eq!(
            state.lines().filter(|line| *line == "1|31").count(),
            2,
            "all and only the two rails take the released width: {state}"
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"])
                .stdout,
            "31"
        );
        let epochs = tmux::run(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                session,
                "#{@chief_viewport_topology_epoch}|#{@chief_viewport_manifest_epoch}",
            ],
        )
        .stdout;
        let parts: Vec<&str> = epochs.split('|').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], parts[1]);
        assert!(parts[0].parse::<u64>().is_ok());
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    /// A DRAG BELONGS TO ONE SESSION LIFETIME.
    ///
    /// tmux reuses a session NAME after a kill, so the name alone cannot say
    /// whether the company the operator dragged is the company that is here
    /// now. `session_id` can, and it is one of the three identity operands the
    /// drag command carries.
    ///
    /// This used to assert a second rule in the same test — that a company
    /// whose `@chief_viewport_manifest_epoch` had not yet caught up with its
    /// `@chief_viewport_topology_epoch` may not record a width either. That
    /// rule is gone; see
    /// `real_border_drag_is_not_blocked_by_a_manifest_refresh_still_in_flight`
    /// for what replaced it and why.
    #[test]
    fn real_sidebar_width_event_refuses_a_recreated_session_lifetime() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-width-identity");
        let session = "org-width-identity_";
        start_session(&socket, session, &["sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "width"],
            vec!["set-option", "-t", session, "@chief_viewport_topology_epoch", "1"],
            vec!["set-option", "-t", session, "@chief_viewport_manifest_epoch", "1"],
        ] {
            assert!(tmux::run(&socket, &argv).ok());
        }
        let old_session_id =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{session_id}"]).stdout;
        assert!(tmux::run(
            &socket,
            &["new-session", "-d", "-s", "keep-server-alive", "sleep", "120"],
        )
        .ok());
        assert!(tmux::run(&socket, &["kill-session", "-t", session]).ok());
        start_session(&socket, session, &["sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "width"],
            vec!["set-option", "-t", session, "@chief_viewport_topology_epoch", "2"],
            vec!["set-option", "-t", session, "@chief_viewport_manifest_epoch", "2"],
        ] {
            assert!(tmux::run(&socket, &argv).ok());
        }
        let new_session_id =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{session_id}"]).stdout;
        assert_ne!(new_session_id, old_session_id);
        assert!(super::release_sidebar_width(
            &socket,
            session,
            "width",
            &old_session_id,
            &viewport_server_nonce(&socket),
            "31",
        )
        .is_err());
        assert!(
            super::release_sidebar_width(
                &socket,
                session,
                "width",
                &new_session_id,
                "ffffffffffffffffffffffffffffffff",
                "31",
            )
            .is_err(),
            "and a drag from another tmux server lifetime is refused too"
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"],)
                .stdout,
            "",
            "no drag from another lifetime may write the preference"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_topology_epoch"],
            )
            .stdout,
            "2",
            "and a refused drag mints nothing"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    /// A MANIFEST REFRESH THAT HAS NOT LANDED YET IS NOT A REASON TO REFUSE
    /// THE OPERATOR.
    ///
    /// `@chief_viewport_manifest_epoch` trails `@chief_viewport_topology_epoch`
    /// for as long as the `run-shell -b` refresh behind an epoch bump takes,
    /// and `park_focus_window_if_still_empty` bumps the epoch with no refresh
    /// at all. The drag commit used to require the two to be equal, which made
    /// every drag in that window fail — silently, because the binding ends in
    /// `|| :`.
    ///
    /// It mints its own epoch instead. Anything holding an older one, the
    /// in-flight refresh included, loses its own CAS; and the commit
    /// re-installs the manifest at the epoch it minted, so the drag REPAIRS the
    /// lag rather than being refused by it.
    #[test]
    fn real_border_drag_is_not_blocked_by_a_manifest_refresh_still_in_flight() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-width-manifest-lag");
        let session = "org-width-lag_";
        start_session(&socket, session, &["-x", "180", "-y", "40", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "width"],
            // The topology has moved and the manifest has not: nine against
            // four, the state a live company spends every pass passing through.
            vec!["set-option", "-t", session, "@chief_viewport_topology_epoch", "9"],
            vec!["set-option", "-t", session, "@chief_viewport_manifest_epoch", "4"],
        ] {
            assert!(tmux::run(&socket, &argv).ok());
        }
        let session_id =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{session_id}"]).stdout;
        assert!(tmux::run(
            &socket,
            &["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
        )
        .ok());
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());

        super::release_sidebar_width(
            &socket,
            session,
            "width",
            &session_id,
            &viewport_server_nonce(&socket),
            "37",
        )
        .expect("a lagging manifest may not refuse the operator's drag");

        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"])
                .stdout,
            "37"
        );
        assert_eq!(
            tmux::run(&socket, &["display-message", "-p", "-t", &rail.stdout, "#{pane_width}"],)
                .stdout,
            "37"
        );
        let epochs = tmux::run(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                session,
                "#{@chief_viewport_topology_epoch}|#{@chief_viewport_manifest_epoch}",
            ],
        )
        .stdout;
        let parts: Vec<&str> = epochs.split('|').collect();
        assert_eq!(parts[0], parts[1], "the drag leaves the lag repaired: {epochs}");
        assert_ne!(parts[0], "9", "behind a freshly minted fence: {epochs}");
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_sidebar_width_refuses_two_rails_in_one_window_and_none_in_another() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-width-rail-distribution");
        let session = "org-width-distribution_";
        start_session(&socket, session, &["sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "width"],
            vec!["set-option", "-t", session, "@chief_viewport_topology_epoch", "1"],
            vec!["set-option", "-t", session, "@chief_viewport_manifest_epoch", "1"],
            vec!["set-option", "-g", "@chief_viewport_topology_generation", "1"],
        ] {
            assert!(tmux::run(&socket, &argv).ok());
        }
        let session_id =
            tmux::run(&socket, &["display-message", "-p", "-t", session, "#{session_id}"]).stdout;
        for _ in 0..2 {
            let rail = tmux::run(
                &socket,
                &[
                    "split-window",
                    "-h",
                    "-b",
                    "-l",
                    "20",
                    "-t",
                    session,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "sleep",
                    "120",
                ],
            );
            assert!(rail.ok());
            assert!(tmux::run(
                &socket,
                &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
            )
            .ok());
        }
        assert!(tmux::run(&socket, &["new-window", "-d", "-t", session, "sleep", "120"],).ok());
        assert!(super::release_sidebar_width(
            &socket,
            session,
            "width",
            &session_id,
            &viewport_server_nonce(&socket),
            "31",
        )
        .is_err());
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"],)
                .stdout,
            ""
        );
        assert_ne!(
            tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_topology_epoch"],
            )
            .stdout,
            "1",
            "the failed survey stays behind its newly minted fence"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn real_tmux_fires_the_installed_hook_for_the_exact_ordinary_client() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-hook");
        let session = "org-acme_";
        start_session(&socket, session, &["-x", "100", "-y", "30", "sleep", "120"]);
        let directory = tempfile::tempdir().expect("callback recorder directory");
        let record = directory.path().join("events");
        // THE HOOK'S OWN FOOTPRINT, and the reason this test no longer proves
        // absence by waiting. Every `viewport-client-eligible` probe lands here
        // BEFORE its verdict, so "tmux ran the client-resized hook for this
        // client" is a positive edge to wait FOR rather than a duration to sit
        // out. See the ineligible-client assertion below.
        let probes = directory.path().join("probes");
        let hostile_directory = directory.path().join("#{@v}");
        std::fs::create_dir(&hostile_directory).expect("literal tmux-format directory");
        let executable = hostile_directory.join("record-callback");
        // This test must fire a real hook into a real executable. The file is
        // a fixture inside a tempdir, not a production filesystem effect.
        #[allow(clippy::disallowed_methods)]
        let staged = std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = viewport-client-eligible ]; then\n\
                   expected_client=$4\n\
                   printf 'probed %s\\n' \"$4\" >> '{}'\n\
                   details=$(tmux -L \"$2\" display-message -p -c \"$4\" -F \
                     '#{{client_session}}|#{{client_width}}|#{{client_height}}|#{{client_flags}}|#{{client_name}}') \
                     || exit 1\n\
                   old_ifs=$IFS; IFS='|'; set -- $details; IFS=$old_ifs\n\
                   [ \"$1\" = '{}' ] && [ \"$5\" = \"$expected_client\" ] || exit 1\n\
                   case \"$2:$3\" in *[!0-9:]*|0:*|*:0|:) exit 1 ;; esac\n\
                   case \",$4,\" in *,control-mode,*|*,ignore-size,*) exit 1 ;; esac\n\
                   exit 0\n\
                 fi\n\
                 printf '%s\\n' \"$*\" >> '{}'\n",
                probes.display(),
                session,
                record.display()
            ),
        );
        staged.expect("callback recorder");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("executable callback recorder");
        }
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "acme"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
            vec!["set-option", "-t", session, "@v", "MUST-NOT-EXPAND"],
        ] {
            let output = tmux::run(&socket, &argv);
            assert!(output.ok(), "fixture option: {}", output.diagnostic());
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok(), "fixture rail: {}", rail.diagnostic());
        let tagged = tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        );
        assert!(tagged.ok(), "fixture rail tag: {}", tagged.diagnostic());
        let manual =
            tmux::run(&socket, &["set-option", "-w", "-t", session, "window-size", "manual"]);
        assert!(manual.ok(), "fixture manual mode: {}", manual.diagnostic());
        let listed = tmux::run(
            &socket,
            &[
                "list-panes",
                "-s",
                "-t",
                session,
                "-F",
                "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
            ],
        );
        let manifest =
            super::viewport_manifest_survey(&listed.stdout).expect("a complete viewport survey");
        install_viewport_hooks(&executable, &socket, session, &manifest, 26, false);
        let shown =
            super::super::tmux::run(&socket, &["show-hooks", "-t", session, "client-resized"]);
        assert!(shown.stdout.contains("viewport-resize"), "installed action: {}", shown.stdout);
        assert!(shown.stdout.contains("run-shell -b"), "hook is asynchronous: {}", shown.stdout);

        let mut client = std::process::Command::new("script")
            .args([
                "-q",
                "-c",
                &format!("tmux -L {socket} attach-session -t {session}"),
                "/dev/null",
            ])
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ordinary tmux client in a pty");
        let mut observed = String::new();
        assert!(wait_until(2_000, || {
            observed = tmux::run(
                &socket,
                &[
                    "list-clients",
                    "-F",
                    "#{client_name}|#{client_pid}|#{client_flags}|#{client_width}|#{client_height}",
                ],
            )
            .stdout;
            !observed.is_empty()
        }));
        let fields: Vec<&str> = observed.trim().split('|').collect();
        assert_eq!(fields.len(), 5, "ordinary client survey: {observed:?}");
        let client_name = fields[0].to_owned();
        let client_pid = fields[1].to_owned();
        assert!(!fields[2].contains("control-mode"), "ordinary client: {observed}");
        assert!(!fields[2].contains("ignore-size"), "geometry owner: {observed}");
        let callbacks_before_resize = std::fs::read_to_string(&record).map_or(0, |events| {
            events.lines().filter(|event| event.starts_with("viewport-resize ")).count()
        });

        let stty = std::process::Command::new("stty")
            .args(["-F", &client_name, "cols", "120", "rows", "40"])
            .status()
            .expect("resize the client pty");
        assert!(stty.success());
        let signal = std::process::Command::new("kill")
            .args(["-WINCH", &client_pid])
            .status()
            .expect("notify the tmux client");
        assert!(signal.success());
        assert!(
            wait_until(2_000, || {
                std::fs::read_to_string(&record).is_ok_and(|events| {
                    events.lines().filter(|event| event.starts_with("viewport-resize ")).count()
                        > callbacks_before_resize
                })
            }),
            "the actual client-resized hook did not select the ordinary client"
        );

        let token =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"]);
        let first_event = token.stdout;
        let first_generation = first_event.parse::<u64>().expect("numeric first generation");
        let expected = format!(
            "viewport-resize {socket} {session} acme {client_name} {first_event} {}",
            viewport_server_nonce(&socket)
        );
        assert!(
            std::fs::read_to_string(&record)
                .is_ok_and(|events| events.lines().any(|event| event == expected)),
            "the exact latest token did not reach its hook callback"
        );
        let owner =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_owner"]);
        assert_eq!(owner.stdout, client_name);

        // Reproduce the live order exactly after a valid request exists: the
        // actuator attaches its persistent control client and fires a resize
        // with a blank height. Neither event may clear or replace the ordinary
        // owner/request, and neither may launch a callback.
        let mut control_client = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-C",
                "attach-session",
                "-f",
                "no-output,ignore-size",
                "-t",
                session,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("newest actuator-like control client");
        let _control_input = control_client.stdin.take().expect("keep control client attached");
        let mut control_observed = String::new();
        assert!(wait_until(2_000, || {
            control_observed = tmux::run(
                &socket,
                &[
                    "list-clients",
                    "-F",
                    "#{client_name}|#{client_pid}|#{client_flags}|#{client_width}|#{client_height}",
                ],
            )
            .stdout
            .lines()
            .find(|line| line.contains("control-mode") && line.contains("ignore-size"))
            .unwrap_or_default()
            .to_owned();
            !control_observed.is_empty()
        }));
        assert!(control_observed.ends_with('|'), "control height is blank: {control_observed}");
        // THE INELIGIBLE CLIENT REALLY REACHED THE HOOK, AND THE HOOK REALLY
        // REFUSED IT — waited FOR, never waited OUT.
        //
        // This was `!wait_until(150, …)`: sit for 150ms and pass if nothing
        // moved. Two things are wrong with that and only one of them is the
        // flake. Measured here, the control client's `client-resized` had not
        // fired after TWO SECONDS of passive polling, so the usual outcome was
        // a wait that expired before the event it was judging existed — the
        // assertion passed by describing an empty window, and the rule went
        // untested. Then on a contended runner something DOES land inside the
        // 150ms: `@chief_viewport_request` is a generation counter, and a late
        // publication by the ORDINARY client (the WINCH resize chain, or tmux
        // resizing it because a second client attached) advances it. Authority
        // never moved — `@chief_viewport_owner` was still the ordinary client —
        // but the predicate read the counter and reported that an ineligible
        // control attach had replaced ordinary authority. A test that can only
        // fail when it is WRONG is worse than no test.
        //
        // The hook leaves a footprint: it calls `viewport-client-eligible`
        // before it decides anything, and the recorder logs every such call.
        // That is a positive edge, so the ordering is provable instead of
        // hoped for: wait until the probe names the CONTROL client, and only
        // then ask what the hook did with it. The generation counter is not
        // asserted on, because holding it still is not a rule this product has.
        let control_name = control_observed.split('|').next().unwrap_or_default().to_owned();
        assert!(!control_name.is_empty(), "the control client is named: {control_observed}");
        let mut authority = Vec::new();
        assert!(
            wait_until(10_000, || {
                authority.push(
                    tmux::run(
                        &socket,
                        &["show-options", "-qv", "-t", session, "@chief_viewport_owner"],
                    )
                    .stdout,
                );
                std::fs::read_to_string(&probes)
                    .is_ok_and(|logged| logged.contains(&format!("probed {control_name}\n")))
            }),
            "the ineligible control client's attach never reached the client-resized hook, so \
             nothing below is evidence about what the hook does with it"
        );
        // Not one sample, at any point across that wait, handed authority to
        // the ineligible client.
        assert!(
            authority.iter().all(|owner| owner != &control_name),
            "an ineligible control attach or resize cannot replace ordinary authority; the owner \
             read {authority:?} while the control client was {control_name}"
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_owner"])
                .stdout,
            client_name,
            "and the ordinary client still holds it once the hook has judged the control client"
        );
        // …and no viewport-resize CALLBACK ran for it either, which is the
        // other half of "neither may launch a callback" and the half a stable
        // owner cannot prove on its own. The `viewport-resize ` prefix is the
        // whole filter: `client-session-changed` legitimately reports this
        // client by name through the same recorder, and a check that read
        // every line would refuse a callback the product is supposed to make.
        let resize_callbacks = std::fs::read_to_string(&record).expect("recorded resize hook");
        assert!(
            !resize_callbacks
                .lines()
                .filter(|event| event.starts_with("viewport-resize "))
                .any(|event| event.contains(&control_name)),
            "the ineligible client {control_name} must not have launched a viewport-resize \
             callback; the recorder holds:\n{resize_callbacks}"
        );

        let event_count = std::fs::read_to_string(&record)
            .expect("recorded resize hook")
            .lines()
            .filter(|event| event.starts_with("viewport-resize "))
            .count();
        let executor = chief_cli::real::RealHostExecutor::production();
        // THE CURRENT request, read here, and not the `first_event` captured
        // before the control client attached.
        //
        // `@chief_viewport_request` is a generation counter that any eligible
        // publication advances, and one lands here routinely — the WINCH resize
        // chain settling, or tmux resizing the ordinary client because a second
        // client joined the session. Handing this call a token that old asks it
        // to publish for a request that has already been superseded, which it
        // correctly REFUSES with "the resized tmux client became stale before
        // publication". This is what the deleted 150ms wait was hiding: it
        // usually expired before the advance, so the stale token usually still
        // matched, and when it did not the failure surfaced as the wrong
        // assertion entirely. The rule under test here is that a live request
        // publishes through the real callback; the STALE-token refusal is a
        // different rule and is driven deliberately, thirty lines below.
        let live_event =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout;
        assert_eq!(
            chief_cli::actuate::resize_session_viewport_for_client(
                &executor,
                &chief_cli::actuate::Socket(socket.clone()),
                session,
                "acme",
                &client_name,
                &live_event,
                &viewport_server_nonce(&socket),
            )
            .expect("the actual hook event publishes through the real callback"),
            1
        );
        // THE ONE ABSENCE HERE THAT IS STILL PROVED BY WAITING, AND THE BUDGET
        // IS NOT TO BE QUIETLY DOUBLED WHEN IT FLAKES.
        //
        // The ineligible-client check above stopped waiting out a window
        // because the hook leaves a footprint to wait FOR. This one has none.
        // A recursion would arrive through `run-shell -b` — a BACKGROUND
        // process, deliberately outside tmux's ordered command queue — so there
        // is no command this test can issue whose completion proves the
        // callback has finished re-entering. Firing a marker hook and waiting
        // for its probe orders this against anything tmux queued SYNCHRONOUSLY
        // and says nothing about the background path, which is the only path
        // the recursion could take.
        //
        // So the number is load-bearing and it is a weakness, stated rather
        // than hidden: on a runner contended enough, a recursion that DID
        // happen could land after the look and be missed. That direction is a
        // false GREEN, not the false red that the 150ms wait produced — this
        // assertion cannot fail because the machine was slow, only because a
        // callback really re-fired. If it ever does start flaking, the answer
        // is a footprint for the background callback to leave, not 500.
        assert!(
            !wait_until(250, || std::fs::read_to_string(&record).is_ok_and(|events| {
                events.lines().filter(|event| event.starts_with("viewport-resize ")).count()
                    > event_count
            })),
            "the callback must not recursively fire client-resized"
        );
        let final_state = tmux::run(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                &rail.stdout,
                "-F",
                "#{window_width}|#{window_height}|#{pane_width}|#{@chief_sidebar_columns}",
            ],
        );
        assert_eq!(final_state.stdout, "120|40|26|26");
        let mode = tmux::run(&socket, &["show-options", "-wv", "-t", &rail.stdout, "window-size"]);
        assert_eq!(mode.stdout, "manual");

        let second_stty = std::process::Command::new("stty")
            .args(["-F", &client_name, "cols", "130", "rows", "41"])
            .status()
            .expect("second client resize");
        assert!(second_stty.success());
        let second_signal = std::process::Command::new("kill")
            .args(["-WINCH", &client_pid])
            .status()
            .expect("notify the second resize");
        assert!(second_signal.success());
        assert!(wait_until(2_000, || {
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout
                .parse::<u64>()
                .is_ok_and(|generation| generation > first_generation)
        }));
        let second_event =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout;
        let stale = chief_cli::actuate::resize_session_viewport_for_client(
            &executor,
            &chief_cli::actuate::Socket(socket.clone()),
            session,
            "acme",
            &client_name,
            &first_event,
            &viewport_server_nonce(&socket),
        )
        .expect_err("the second resize makes the first event stale");
        assert!(stale.contains("became stale before publication"), "{stale}");

        let mut second_client = std::process::Command::new("script")
            .args([
                "-q",
                "-c",
                &format!("tmux -L {socket} attach-session -t {session}"),
                "/dev/null",
            ])
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("second ordinary tmux client in a pty");
        let mut second_observed = String::new();
        assert!(wait_until(2_000, || {
            second_observed = tmux::run(
                &socket,
                &[
                    "list-clients",
                    "-F",
                    "#{client_name}|#{client_pid}|#{client_flags}|#{client_width}|#{client_height}",
                ],
            )
            .stdout
            .lines()
            .find(|line| {
                !line.starts_with(&format!("{client_name}|"))
                    && !line.contains("control-mode")
                    && !line.contains("ignore-size")
            })
            .unwrap_or_default()
            .to_owned();
            !second_observed.is_empty()
        }));
        let second_fields: Vec<&str> = second_observed.trim().split('|').collect();
        assert_eq!(second_fields.len(), 5, "second ordinary client: {second_observed:?}");
        let second_name = second_fields[0].to_owned();
        let second_pid = second_fields[1].to_owned();
        assert!(!second_fields[2].contains("control-mode"), "{second_observed}");
        assert!(!second_fields[2].contains("ignore-size"), "{second_observed}");
        let callbacks_before_second_client_resize = std::fs::read_to_string(&record)
            .map_or(0, |events| {
                events.lines().filter(|event| event.starts_with("viewport-resize ")).count()
            });
        assert!(std::process::Command::new("stty")
            .args(["-F", &second_name, "cols", "140", "rows", "42"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", &second_pid])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(wait_until(2_000, || {
            let callback_arrived = std::fs::read_to_string(&record).is_ok_and(|events| {
                events.lines().filter(|event| event.starts_with("viewport-resize ")).count()
                    > callbacks_before_second_client_resize
            });
            let owner = tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_owner"],
            )
            .stdout;
            let request = tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_request"],
            )
            .stdout;
            callback_arrived && owner == second_name && request.parse::<u64>().is_ok()
        }));
        let second_client_event =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout;
        assert_eq!(
            chief_cli::actuate::resize_session_viewport_for_client(
                &executor,
                &chief_cli::actuate::Socket(socket.clone()),
                session,
                "acme",
                &second_name,
                &second_client_event,
                &viewport_server_nonce(&socket),
            )
            .expect("the latest of two ordinary clients owns the viewport event"),
            1
        );

        let test_executable = std::env::current_exe().expect("current test executable");
        let production_executable = directory.path().join("production-callback");
        let callback_source = viewport_test_callback_source(&test_executable);
        // The executable is a fixture in the existing tempdir. It lets the
        // real tmux hook enter the current shard's compiled production route
        // without assuming the non-test package binary exists.
        #[allow(clippy::disallowed_methods)]
        let staged = std::fs::write(&production_executable, callback_source);
        staged.expect("stage the production callback fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &production_executable,
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("executable production callback fixture");
        }
        let listed = tmux::run(
            &socket,
            &[
                "list-panes",
                "-s",
                "-t",
                session,
                "-F",
                "#{window_id}\t#{pane_id}\t#{@organization_window_id}\t#{@organization_sidebar}",
            ],
        );
        let manifest =
            super::viewport_manifest_survey(&listed.stdout).expect("a complete viewport survey");
        install_viewport_hooks(&production_executable, &socket, session, &manifest, 26, false);
        let status = tmux::run(&socket, &["set-option", "-g", "status", "off"]);
        assert!(status.ok(), "fixture status: {}", status.diagnostic());
        let fast =
            tmux::run(&socket, &["set-option", "-g", "@chief_viewport_fast_session", session]);
        assert!(fast.ok(), "fixture fast path: {}", fast.diagnostic());

        let pane_ids_before =
            tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout;
        let preference_before =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"])
                .stdout;
        if tmux::run(
            &socket,
            &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
        )
        .stdout
            == "1"
        {
            let cancelled = tmux::run(&socket, &["send-keys", "-t", &rail.stdout, "-X", "cancel"]);
            assert!(cancelled.ok(), "leave pre-existing fixture mode: {}", cancelled.diagnostic());
        }
        assert_eq!(
            tmux::run(
                &socket,
                &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
            )
            .stdout,
            "0",
            "the viewport cycle starts from the rail's normal mode"
        );
        let stop_samples = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sampled_frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sampler_stop = std::sync::Arc::clone(&stop_samples);
        let sampler_frames = std::sync::Arc::clone(&sampled_frames);
        let sampler_socket = socket.clone();
        let sampler_rail = rail.stdout.clone();
        let sampler_client = second_name.clone();
        let sampler = std::thread::spawn(move || {
            while !sampler_stop.load(std::sync::atomic::Ordering::Acquire) {
                let frame = tmux::run(
                    &sampler_socket,
                    &[
                        "display-message",
                        "-p",
                        "-c",
                        &sampler_client,
                        "-F",
                        "client|#{client_width}|#{client_height}",
                        ";",
                        "display-message",
                        "-p",
                        "-t",
                        &sampler_rail,
                        "-F",
                        "window|#{window_width}|#{window_height}|#{pane_width}|#{pane_in_mode}|#{@chief_sidebar_columns}|#{@chief_sidebar_collapsed}",
                    ],
                )
                .stdout;
                if !frame.is_empty() {
                    sampler_frames.lock().expect("sample frame lock").push(frame);
                }
                std::thread::yield_now();
            }
        });

        for (columns, rows) in [(240, 56), (267, 60), (300, 67), (360, 84), (240, 56)] {
            assert!(std::process::Command::new("stty")
                .args(
                    ["-F", &second_name, "cols", &columns.to_string(), "rows", &rows.to_string(),]
                )
                .status()
                .is_ok_and(|status| status.success()));
            assert!(std::process::Command::new("kill")
                .args(["-WINCH", &second_pid])
                .status()
                .is_ok_and(|status| status.success()));
            let expected = format!("{columns}|{rows}|26");
            assert!(
                wait_until(3_000, || tmux::run(
                    &socket,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &rail.stdout,
                        "-F",
                        "#{window_width}|#{window_height}|#{pane_width}",
                    ],
                )
                .stdout
                    == expected),
                "production hook did not publish {expected}"
            );
        }
        stop_samples.store(true, std::sync::atomic::Ordering::Release);
        sampler.join().expect("viewport boundary sampler");
        let frames = sampled_frames.lock().expect("sampled viewport frames");
        assert!(!frames.is_empty(), "the boundary sampler must observe the cycle");
        assert!(
            frames.iter().all(|frame| {
                let mut lines = frame.lines();
                let client: Vec<&str> = lines.next().unwrap_or_default().split('|').collect();
                let window: Vec<&str> = lines.next().unwrap_or_default().split('|').collect();
                client.len() == 3
                    && window.len() == 7
                    && window[3] == "26"
                    && window[4] == "0"
                    && window[5] == "26"
                    && window[6].is_empty()
            }),
            "every server state keeps the effective rail and normal pane mode: {frames:?}"
        );
        let published: Vec<(&str, &str)> = frames
            .iter()
            .filter_map(|frame| {
                let mut lines = frame.lines();
                let client: Vec<&str> = lines.next()?.split('|').collect();
                let window: Vec<&str> = lines.next()?.split('|').collect();
                (client.len() == 3
                    && window.len() == 7
                    && client[1] == window[1]
                    && client[2] == window[2])
                    .then_some((window[1], window[2]))
            })
            .collect();
        for geometry in [(240, 56), (267, 60), (300, 67), (360, 84), (240, 56)] {
            let expected = (geometry.0.to_string(), geometry.1.to_string());
            assert!(
                published.contains(&(expected.0.as_str(), expected.1.as_str())),
                "the native hook must publish each complete geometry directly: {frames:?}"
            );
        }
        drop(frames);
        assert_eq!(
            tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout,
            pane_ids_before
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"],)
                .stdout,
            preference_before
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-wv", "-t", session, "window-size"]).stdout,
            "manual"
        );

        assert!(std::process::Command::new("stty")
            .args(["-F", &second_name, "cols", "150", "rows", "43"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", &second_pid])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(
            wait_until(3_000, || {
                tmux::run(
                    &socket,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &rail.stdout,
                        "-F",
                        "#{window_width}|#{window_height}|#{pane_width}",
                    ],
                )
                .stdout
                    == "150|43|26"
            }),
            "the installed production CLI route did not publish the viewport"
        );

        let barrier_path = directory.path().join("viewport-callback.sock");
        let barrier =
            std::os::unix::net::UnixListener::bind(&barrier_path).expect("test callback barrier");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (complete_tx, complete_rx) = std::sync::mpsc::channel();
        let barrier_thread = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut callback, _) = barrier.accept().expect("built Chief enters its callback");
            let mut entered = [0_u8; 7];
            callback.read_exact(&mut entered).expect("callback entry message");
            assert_eq!(&entered, b"entered");
            entered_tx.send(()).expect("announce callback entry");
            release_rx.recv().expect("test releases callback");
            callback.write_all(b"1").expect("release callback");
            let mut complete = [0_u8; 8];
            callback.read_exact(&mut complete).expect("callback completion message");
            assert_eq!(&complete, b"complete");
            complete_tx.send(()).expect("announce callback completion");
        });
        let barrier_env = tmux::run(
            &socket,
            &[
                "set-environment",
                "-g",
                "CHIEF_TEST_VIEWPORT_BARRIER",
                &barrier_path.display().to_string(),
            ],
        );
        assert!(barrier_env.ok(), "publish callback barrier: {}", barrier_env.diagnostic());
        assert!(std::process::Command::new("stty")
            .args(["-F", &second_name, "cols", "160", "rows", "44"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", &second_pid])
            .status()
            .is_ok_and(|status| status.success()));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("the built Chief callback must be pending before detach");
        let pending_generation =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout;
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_owner"])
                .stdout,
            second_name
        );
        assert!(
            pending_generation
                .parse::<u64>()
                .is_ok_and(|generation| generation > second_client_event.parse().unwrap_or(0)),
            "the pending callback owns a newer numeric generation: {pending_generation}"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &rail.stdout,
                    "-F",
                    "#{window_width}|#{window_height}|#{pane_width}",
                ],
            )
            .stdout,
            "150|43|26",
            "the pending callback has not published"
        );
        let detached_now = tmux::run(&socket, &["detach-client", "-t", &second_name]);
        assert!(detached_now.ok(), "detach during callbacks: {}", detached_now.diagnostic());
        assert!(
            wait_until(2_000, || {
                tmux::run(
                    &socket,
                    &["show-options", "-qv", "-t", session, "@chief_viewport_request"],
                )
                .stdout
                .is_empty()
            }),
            "detach must revoke every overlapping callback token"
        );
        release_tx.send(()).expect("release the revoked callback");
        complete_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("the revoked production callback completed its CAS");
        barrier_thread.join().expect("callback barrier thread");
        assert!(
            tmux::run(&socket, &["set-environment", "-gu", "CHIEF_TEST_VIEWPORT_BARRIER"],).ok()
        );
        assert!(
            !wait_until(400, || tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &rail.stdout,
                    "-F",
                    "#{window_width}|#{window_height}|#{pane_width}",
                ],
            )
            .stdout
                != "150|43|26"),
            "a callback mutated the company after detach invalidated its event"
        );

        let other = "org-other_";
        start_session(&socket, other, &["sleep", "120"]);
        let live_before_switch = "999".to_owned();
        let live_token = tmux::run(
            &socket,
            &["set-option", "-t", session, "@chief_viewport_request", &live_before_switch],
        );
        assert!(live_token.ok(), "seed live switch token: {}", live_token.diagnostic());
        assert!(tmux::run(
            &socket,
            &["set-option", "-t", session, "@chief_viewport_owner", &client_name],
        )
        .ok());
        let switched = tmux::run(&socket, &["switch-client", "-c", &client_name, "-t", other]);
        assert!(switched.ok(), "switch first client: {}", switched.diagnostic());
        assert!(
            wait_until(2_000, || {
                tmux::run(
                    &socket,
                    &["show-options", "-qv", "-t", session, "@chief_viewport_request"],
                )
                .stdout
                .is_empty()
            }),
            "session switch must clear a live token independently of detach"
        );
        let switched_error = chief_cli::actuate::resize_session_viewport_for_client(
            &executor,
            &chief_cli::actuate::Socket(socket.clone()),
            session,
            "acme",
            &client_name,
            &second_event,
            &viewport_server_nonce(&socket),
        )
        .expect_err("a client session switch revokes its old event");
        assert!(switched_error.contains("belongs to"), "{switched_error}");

        // Close the smaller race between the synchronous eligibility result
        // and the hook queue's mint. The exact client is valid when Chief
        // answers the predicate, then detaches before mint or callback. The
        // callback must refuse that gone client and remove only the generation
        // it just received; it must not leave a live owner behind.
        let mut race_client = std::process::Command::new("script")
            .args([
                "-q",
                "-c",
                &format!("tmux -L {socket} attach-session -t {session}"),
                "/dev/null",
            ])
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ordinary client for the eligibility-to-mint race");
        let mut race_observed = String::new();
        assert!(wait_until(2_000, || {
            race_observed = tmux::run(
                &socket,
                &["list-clients", "-F", "#{client_name}|#{client_pid}|#{client_flags}"],
            )
            .stdout
            .lines()
            .find(|line| {
                !line.starts_with(&format!("{client_name}|"))
                    && !line.contains("control-mode")
                    && !line.contains("ignore-size")
            })
            .unwrap_or_default()
            .to_owned();
            !race_observed.is_empty()
        }));
        let race_fields: Vec<&str> = race_observed.split('|').collect();
        assert_eq!(race_fields.len(), 3, "race client survey: {race_observed}");
        let race_name = race_fields[0].to_owned();
        let race_pid = race_fields[1].to_owned();
        assert!(tmux::run(
            &socket,
            &[
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
            ],
        )
        .ok());
        let generation_before =
            tmux::run(&socket, &["show-options", "-gqv", "@chief_viewport_generation"])
                .stdout
                .parse::<u64>()
                .expect("generation before pre-mint race");
        let eligibility_path = directory.path().join("viewport-eligibility.sock");
        let eligibility_barrier = std::os::unix::net::UnixListener::bind(&eligibility_path)
            .expect("test eligibility barrier");
        assert!(tmux::run(
            &socket,
            &[
                "set-environment",
                "-g",
                "CHIEF_TEST_VIEWPORT_ELIGIBILITY_BARRIER",
                &eligibility_path.display().to_string(),
            ],
        )
        .ok());
        assert!(std::process::Command::new("stty")
            .args(["-F", &race_name, "cols", "170", "rows", "45"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", &race_pid])
            .status()
            .is_ok_and(|status| status.success()));
        use std::io::{Read as _, Write as _};
        let (mut eligibility, _) =
            eligibility_barrier.accept().expect("production eligibility route enters before mint");
        let mut entered = [0_u8; 7];
        eligibility.read_exact(&mut entered).expect("eligibility entry message");
        assert_eq!(&entered, b"entered");
        assert!(race_client.kill().is_ok(), "detach exact eligible client before mint");
        let _ = race_client.wait();
        eligibility.write_all(b"1").expect("release eligibility result");
        assert!(
            wait_until(3_000, || {
                let request = tmux::run(
                    &socket,
                    &["show-options", "-qv", "-t", session, "@chief_viewport_request"],
                )
                .stdout;
                let owner = tmux::run(
                    &socket,
                    &["show-options", "-qv", "-t", session, "@chief_viewport_owner"],
                )
                .stdout;
                request.is_empty() && owner.is_empty()
            }),
            "a refusal after eligibility must leave no request or owner"
        );
        let generation_after =
            tmux::run(&socket, &["show-options", "-gqv", "@chief_viewport_generation"])
                .stdout
                .parse::<u64>()
                .expect("generation after pre-mint race");
        assert!(generation_after > generation_before, "the refused generation remains monotonic");
        assert!(tmux::run(
            &socket,
            &["set-environment", "-gu", "CHIEF_TEST_VIEWPORT_ELIGIBILITY_BARRIER"],
        )
        .ok());

        assert!(client.kill().is_ok());
        let _ = client.wait();
        assert!(wait_until(2_000, || {
            !tmux::run(&socket, &["list-clients", "-F", "#{client_name}"])
                .stdout
                .lines()
                .any(|name| name == client_name)
        }));
        let detached = chief_cli::actuate::resize_session_viewport_for_client(
            &executor,
            &chief_cli::actuate::Socket(socket.clone()),
            session,
            "acme",
            &client_name,
            &second_event,
            &viewport_server_nonce(&socket),
        )
        .expect_err("a detached client cannot publish");
        assert!(
            detached.contains("no longer present") || detached.contains("target is stale"),
            "{detached}"
        );
        assert!(second_client.kill().is_ok());
        let _ = second_client.wait();
        assert!(control_client.kill().is_ok());
        let _ = control_client.wait();
        let _ = super::super::tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn a_forced_background_callback_error_is_silent_and_never_enters_view_mode() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-hook-error");
        let session = "org-acme_";
        start_session(&socket, session, &["-x", "100", "-y", "30", "sleep", "120"]);
        assert!(tmux::run(&socket, &["set-option", "-t", session, "@organization_id", "acme"]).ok());
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok(), "error-test rail: {}", rail.diagnostic());
        let directory = tempfile::tempdir().expect("error callback directory");
        let executable = directory.path().join("forced-error");
        #[allow(clippy::disallowed_methods)]
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = viewport-client-eligible ]; then\n\
                   details=$(tmux -L \"$2\" display-message -p -c \"$4\" -F \
                     '#{{client_session}}|#{{client_width}}|#{{client_height}}|#{{client_flags}}|#{{client_name}}') \
                     || exit 1\n\
                   case \"$details\" in '{}|'*'|control-mode'*|'{}|'*'|ignore-size'*) exit 1 ;; esac\n\
                   exit 0\n\
                 fi\n\
                 printf '%s\\n' \"forced viewport callback error\" >&2\n\
                 exit 64\n",
                session, session
            ),
        )
        .expect("forced-error callback");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("executable forced-error callback");
        }
        install_viewport_hooks(&executable, &socket, session, &[], 26, false);

        let mut client = std::process::Command::new("script")
            .args([
                "-q",
                "-c",
                &format!("tmux -L {socket} attach-session -t {session}"),
                "/dev/null",
            ])
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ordinary error-test client");
        let mut observed = String::new();
        assert!(wait_until(2_000, || {
            observed = tmux::run(
                &socket,
                &["list-clients", "-F", "#{client_name}|#{client_pid}|#{client_flags}"],
            )
            .stdout;
            !observed.is_empty()
        }));
        let fields: Vec<&str> = observed.split('|').collect();
        assert_eq!(fields.len(), 3, "error-test client: {observed}");
        assert!(!fields[2].contains("control-mode"), "{observed}");
        assert_eq!(
            tmux::run(
                &socket,
                &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
            )
            .stdout,
            "0"
        );
        assert!(std::process::Command::new("stty")
            .args(["-F", fields[0], "cols", "120", "rows", "40"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", fields[1]])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(wait_until(2_000, || tmux::run(
            &socket,
            &["show-options", "-gqv", "@chief_viewport_generation"],
        )
        .stdout
        .parse::<u64>()
        .is_ok_and(|generation| generation > 0)));
        assert!(
            !wait_until(150, || tmux::run(
                &socket,
                &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
            )
            .stdout
                == "1"),
            "a forced callback error must not enter view mode"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
            )
            .stdout,
            "0",
            "silenced run-shell errors cannot replace the rail with view mode"
        );
        assert!(client.kill().is_ok());
        let _ = client.wait();
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn production_hook_cycle_never_publishes_a_wrong_rail_or_view_mode() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-hook-cycle");
        let session = "org-acme_";
        start_session(&socket, session, &["-x", "100", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "acme"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
            vec!["set-option", "-w", "-t", session, "window-size", "manual"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "cycle fixture option: {argv:?}");
        }
        let rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(rail.ok(), "cycle rail: {}", rail.diagnostic());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &rail.stdout, "@organization_sidebar", "1"],
        )
        .ok());
        let directory = tempfile::tempdir().expect("cycle callback directory");
        let executable = directory.path().join("production-callback");
        let source = viewport_test_callback_source(
            &std::env::current_exe().expect("current chief test executable"),
        );
        #[allow(clippy::disallowed_methods)]
        std::fs::write(&executable, source).expect("cycle production callback");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("executable cycle callback");
        }
        install_viewport_hooks(&executable, &socket, session, &[], 26, false);

        let mut client = std::process::Command::new("script")
            .args([
                "-q",
                "-c",
                &format!("tmux -L {socket} attach-session -t {session}"),
                "/dev/null",
            ])
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ordinary cycle client");
        let mut ordinary = String::new();
        assert!(wait_until(2_000, || {
            ordinary = tmux::run(
                &socket,
                &["list-clients", "-F", "#{client_name}|#{client_pid}|#{client_flags}"],
            )
            .stdout;
            !ordinary.is_empty()
        }));
        let fields: Vec<&str> = ordinary.split('|').collect();
        assert_eq!(fields.len(), 3, "ordinary cycle client: {ordinary}");
        let client_name = fields[0].to_owned();
        let client_pid = fields[1].to_owned();
        assert!(!fields[2].contains("control-mode"), "{ordinary}");

        let mut control = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-C",
                "attach-session",
                "-f",
                "no-output,ignore-size",
                "-t",
                session,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("newer control cycle client");
        let _control_input = control.stdin.take().expect("hold cycle control client");
        assert!(wait_until(2_000, || tmux::run(
            &socket,
            &["list-clients", "-F", "#{client_flags}"],
        )
        .stdout
        .lines()
        .any(|flags| flags.contains("control-mode") && flags.contains("ignore-size"))));
        assert_eq!(
            tmux::run(
                &socket,
                &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
            )
            .stdout,
            "0"
        );

        let pane_ids_before =
            tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let samples = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sample_stop = std::sync::Arc::clone(&stop);
        let sample_rows = std::sync::Arc::clone(&samples);
        let sample_socket = socket.clone();
        let sample_rail = rail.stdout.clone();
        let sampler = std::thread::spawn(move || {
            while !sample_stop.load(std::sync::atomic::Ordering::Acquire) {
                let row = tmux::run(
                    &sample_socket,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &sample_rail,
                        "-F",
                        "#{window_width}|#{window_height}|#{pane_width}|#{pane_in_mode}",
                    ],
                )
                .stdout;
                if !row.is_empty() {
                    sample_rows.lock().expect("cycle sample lock").push(row);
                }
                std::thread::yield_now();
            }
        });
        for (columns, rows) in [(240, 56), (360, 84), (240, 56)] {
            assert!(std::process::Command::new("stty")
                .args(
                    ["-F", &client_name, "cols", &columns.to_string(), "rows", &rows.to_string(),]
                )
                .status()
                .is_ok_and(|status| status.success()));
            assert!(std::process::Command::new("kill")
                .args(["-WINCH", &client_pid])
                .status()
                .is_ok_and(|status| status.success()));
            let expected = format!("{columns}|{rows}|26|0");
            assert!(
                wait_until(3_000, || tmux::run(
                    &socket,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &rail.stdout,
                        "-F",
                        "#{window_width}|#{window_height}|#{pane_width}|#{pane_in_mode}",
                    ],
                )
                .stdout
                    == expected),
                "expected {expected}; got {}; clients {}",
                tmux::run(
                    &socket,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &rail.stdout,
                        "-F",
                        "#{window_width}|#{window_height}|#{pane_width}|#{pane_in_mode}",
                    ],
                )
                .stdout,
                tmux::run(
                    &socket,
                    &[
                        "list-clients",
                        "-F",
                        "#{client_name}|#{client_pid}|#{client_flags}|#{client_width}|#{client_height}|#{client_cell_width}|#{client_cell_height}",
                    ],
                )
                .stdout
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        sampler.join().expect("production cycle sampler");
        let observed = samples.lock().expect("production cycle samples");
        assert!(!observed.is_empty());
        assert!(
            observed.iter().all(|row| {
                let fields: Vec<&str> = row.split('|').collect();
                fields.len() == 4 && fields[2] == "26" && fields[3] == "0"
            }),
            "no production boundary may expose a wrong rail or pane mode: {observed:?}"
        );
        drop(observed);
        assert_eq!(
            tmux::run(&socket, &["list-panes", "-t", session, "-F", "#{pane_id}"]).stdout,
            pane_ids_before
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_sidebar_columns"],)
                .stdout,
            "26"
        );
        assert_eq!(
            tmux::run(&socket, &["show-options", "-wv", "-t", session, "window-size"]).stdout,
            "manual"
        );
        let current_generation =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout
                .parse::<u64>()
                .expect("current production-hook generation");
        assert!(current_generation > 0);
        let stale = (current_generation - 1).to_string();
        let executor = chief_cli::real::RealHostExecutor::production();
        let stale_error = chief_cli::actuate::resize_session_viewport_for_client(
            &executor,
            &chief_cli::actuate::Socket(socket.clone()),
            session,
            "acme",
            &client_name,
            &stale,
            &viewport_server_nonce(&socket),
        )
        .expect_err("an older real callback gets the silent stale marker");
        assert!(stale_error.contains("became stale before publication"), "{stale_error}");
        assert_eq!(
            tmux::run(
                &socket,
                &["display-message", "-p", "-t", &rail.stdout, "-F", "#{pane_in_mode}"],
            )
            .stdout,
            "0",
            "the stale marker is not a failing tmux job and cannot enter view mode"
        );

        // A session name is reusable. Pause one accepted callback, destroy its
        // session, recreate the same name with the same organization, and mint
        // a newer event before the old callback continues. Only the
        // server-global generation distinguishes these two session lifetimes.
        let aba_path = directory.path().join("viewport-session-aba.sock");
        let aba_listener =
            std::os::unix::net::UnixListener::bind(&aba_path).expect("ABA callback barrier");
        let (aba_entered_tx, aba_entered_rx) = std::sync::mpsc::channel();
        let (aba_release_tx, aba_release_rx) = std::sync::mpsc::channel();
        let (aba_complete_tx, aba_complete_rx) = std::sync::mpsc::channel();
        let aba_thread = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut callback, _) = aba_listener.accept().expect("old callback enters barrier");
            let mut entered = [0_u8; 7];
            callback.read_exact(&mut entered).expect("old callback entry");
            assert_eq!(&entered, b"entered");
            aba_entered_tx.send(()).expect("announce old callback");
            aba_release_rx.recv().expect("release old callback");
            callback.write_all(b"1").expect("release old callback socket");
            let mut complete = [0_u8; 8];
            callback.read_exact(&mut complete).expect("old callback completion message");
            assert_eq!(&complete, b"complete");
            aba_complete_tx.send(()).expect("announce old callback completion");
        });
        assert!(tmux::run(
            &socket,
            &[
                "set-environment",
                "-g",
                "CHIEF_TEST_VIEWPORT_BARRIER",
                &aba_path.display().to_string(),
            ],
        )
        .ok());
        assert!(std::process::Command::new("stty")
            .args(["-F", &client_name, "cols", "250", "rows", "60"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", &client_pid])
            .status()
            .is_ok_and(|status| status.success()));
        aba_entered_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("old callback is pending before session replacement");
        let old_generation =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout
                .parse::<u64>()
                .expect("old session request generation");
        assert!(tmux::run(&socket, &["set-environment", "-gu", "CHIEF_TEST_VIEWPORT_BARRIER"]).ok());

        let anchor = "viewport-anchor";
        start_session(&socket, anchor, &["sleep", "120"]);
        assert!(tmux::run(&socket, &["set-option", "-g", "detach-on-destroy", "off"]).ok());
        assert!(tmux::run(&socket, &["kill-session", "-t", session]).ok());
        start_session(&socket, session, &["-x", "180", "-y", "50", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "acme"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
            vec!["set-option", "-t", session, "@chief_sidebar_columns", "26"],
            vec!["set-option", "-w", "-t", session, "window-size", "manual"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "recreated option: {argv:?}");
        }
        let recreated_rail = tmux::run(
            &socket,
            &[
                "split-window",
                "-h",
                "-b",
                "-l",
                "26",
                "-t",
                session,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep",
                "120",
            ],
        );
        assert!(recreated_rail.ok(), "recreated rail: {}", recreated_rail.diagnostic());
        assert!(tmux::run(
            &socket,
            &["set-option", "-p", "-t", &recreated_rail.stdout, "@organization_sidebar", "1",],
        )
        .ok());
        install_viewport_hooks(&executable, &socket, session, &[], 26, false);
        assert!(tmux::run(&socket, &["switch-client", "-c", &client_name, "-t", session]).ok());
        assert!(std::process::Command::new("stty")
            .args(["-F", &client_name, "cols", "260", "rows", "61"])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(std::process::Command::new("kill")
            .args(["-WINCH", &client_pid])
            .status()
            .is_ok_and(|status| status.success()));
        assert!(wait_until(3_000, || {
            let generation = tmux::run(
                &socket,
                &["show-options", "-qv", "-t", session, "@chief_viewport_request"],
            )
            .stdout
            .parse::<u64>()
            .unwrap_or_default();
            let frame = tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &recreated_rail.stdout,
                    "-F",
                    "#{window_width}|#{window_height}|#{pane_width}|#{pane_in_mode}",
                ],
            )
            .stdout;
            generation > old_generation && frame == "260|61|26|0"
        }));
        let new_generation =
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"])
                .stdout;
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_owner"])
                .stdout,
            client_name
        );
        aba_release_tx.send(()).expect("release old session callback");
        aba_complete_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("the old production callback completed its CAS");
        aba_thread.join().expect("old callback barrier thread");
        assert_eq!(
            tmux::run(&socket, &["show-options", "-qv", "-t", session, "@chief_viewport_request"],)
                .stdout,
            new_generation,
            "the completed old callback cannot clear the recreated session request"
        );
        assert_eq!(
            tmux::run(
                &socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &recreated_rail.stdout,
                    "-F",
                    "#{window_width}|#{window_height}|#{pane_width}|#{pane_in_mode}",
                ],
            )
            .stdout,
            "260|61|26|0",
            "the completed old callback cannot publish to the recreated session"
        );
        assert!(client.kill().is_ok());
        let _ = client.wait();
        assert!(control.kill().is_ok());
        let _ = control.wait();
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    #[ignore = "entered only by the real tmux hook fixture"]
    fn viewport_client_eligible_child() {
        let Ok(socket) = std::env::var("CHIEF_TEST_VIEWPORT_SOCKET") else {
            return;
        };
        if !super::super::run_viewport_client_eligible(
            &socket,
            &std::env::var("CHIEF_TEST_VIEWPORT_SESSION").expect("eligible session"),
            &std::env::var("CHIEF_TEST_VIEWPORT_CLIENT").expect("eligible client"),
            &std::env::var("CHIEF_TEST_VIEWPORT_NONCE").expect("server nonce"),
        ) {
            std::process::exit(1);
        }
    }

    #[test]
    #[ignore = "entered only by the real tmux hook fixture"]
    fn viewport_client_census_child() {
        let Ok(socket) = std::env::var("CHIEF_TEST_VIEWPORT_SOCKET") else {
            return;
        };
        super::super::run_viewport_client_census(
            &socket,
            &std::env::var("CHIEF_TEST_VIEWPORT_GENERATION").expect("membership generation"),
            &std::env::var("CHIEF_TEST_VIEWPORT_NONCE").expect("server nonce"),
        )
        .expect("production viewport census");
    }

    /// The drag commit, entered exactly as tmux enters it.
    ///
    /// `CHIEF_TEST_WIDTH_SURPLUS` is not an operand — it records whether tmux
    /// handed this verb an EIGHTH word. The #1196 command carried a frozen
    /// epoch there, so a fixture that silently tolerated one would go on
    /// passing after the product regressed.
    #[test]
    #[ignore = "entered only by the real rail drag fixture"]
    fn viewport_sidebar_width_child() {
        let Ok(socket) = std::env::var("CHIEF_TEST_WIDTH_SOCKET") else {
            return;
        };
        let completion = std::env::var("CHIEF_TEST_WIDTH_DONE").expect("drag completion");
        let surplus = std::env::var("CHIEF_TEST_WIDTH_SURPLUS").unwrap_or_default();
        let result = if surplus.is_empty() {
            super::release_sidebar_width(
                &socket,
                &std::env::var("CHIEF_TEST_WIDTH_SESSION").expect("drag session"),
                &std::env::var("CHIEF_TEST_WIDTH_ORGANIZATION").expect("drag organization"),
                &std::env::var("CHIEF_TEST_WIDTH_SESSION_ID").expect("drag session id"),
                &std::env::var("CHIEF_TEST_WIDTH_NONCE").expect("drag nonce"),
                &std::env::var("CHIEF_TEST_WIDTH_COLUMNS").expect("drag columns"),
            )
        } else {
            Err(super::LifecycleError::host(format!(
                "the drag command carried a surplus word: {surplus}"
            )))
        };
        #[allow(clippy::disallowed_methods)]
        std::fs::write(
            completion,
            match &result {
                Ok(()) => "ok".to_owned(),
                Err(error) => format!("error: {error}"),
            },
        )
        .expect("write drag completion");
        result.expect("production rail drag");
    }

    #[test]
    #[ignore = "entered only by the real manifest refresh fixture"]
    fn viewport_manifest_refresh_child() {
        let Ok(socket) = std::env::var("CHIEF_TEST_MANIFEST_SOCKET") else {
            return;
        };
        let result = super::refresh_viewport_manifest(
            &socket,
            &std::env::var("CHIEF_TEST_MANIFEST_SESSION").expect("manifest session"),
            &std::env::var("CHIEF_TEST_MANIFEST_EPOCH").expect("manifest epoch"),
            &std::env::var("CHIEF_TEST_MANIFEST_NONCE").expect("manifest nonce"),
        );
        let completion = std::env::var("CHIEF_TEST_MANIFEST_DONE").expect("manifest completion");
        #[allow(clippy::disallowed_methods)]
        std::fs::write(completion, if result.is_ok() { "ok" } else { "error" })
            .expect("write manifest completion");
        result.expect("production manifest refresh");
    }

    #[test]
    #[ignore = "entered only by the real tmux hook fixture"]
    fn viewport_callback_child() {
        let Ok(socket) = std::env::var("CHIEF_TEST_VIEWPORT_SOCKET") else {
            return;
        };
        super::super::run_viewport_resize(
            &socket,
            &std::env::var("CHIEF_TEST_VIEWPORT_SESSION").expect("callback session"),
            &std::env::var("CHIEF_TEST_VIEWPORT_ORGANIZATION").expect("callback organization"),
            &std::env::var("CHIEF_TEST_VIEWPORT_CLIENT").expect("callback client"),
            &std::env::var("CHIEF_TEST_VIEWPORT_EVENT").expect("callback event"),
            &std::env::var("CHIEF_TEST_VIEWPORT_NONCE").expect("server nonce"),
        )
        .expect("production viewport callback");
    }

    #[test]
    #[ignore = "entered only by the real tmux hook fixture"]
    fn viewport_session_changed_child() {
        let Ok(socket) = std::env::var("CHIEF_TEST_VIEWPORT_SOCKET") else {
            return;
        };
        super::super::run_viewport_client_changed(
            &socket,
            &std::env::var("CHIEF_TEST_VIEWPORT_CLIENT").expect("changed client"),
            &std::env::var("CHIEF_TEST_VIEWPORT_NONCE").expect("server nonce"),
        )
        .expect("production session-change callback");
    }

    /// EVERY DOOR INTO AN OPERATOR'S COMPANY SESSION CALLS `enter_company_session`.
    ///
    /// This is the guard for the live defect. `attach` was not the only way in
    /// — Founder mode hands the operator over with `switch-client` after
    /// creating the company — so a rail created only by `attach` left every
    /// newly created company railless on its first run, which is the run that
    /// decides whether an operator thinks the feature exists.
    ///
    /// The test reads the SOURCE rather than a transcribed list, because a
    /// hand-kept list of call sites is the next thing to fall out of step with
    /// the code. It asserts two things that together close the hole: every
    /// handover verb is preceded by a call, and every declared door is used.
    #[test]
    fn every_path_into_a_company_session_places_the_rail_first() {
        let attach_rs = include_str!("attach.rs");
        let founder_rs = include_str!("founder.rs");

        // 1. Each declared door is actually passed at a call site. A door
        //    constant nobody uses is a path somebody deleted the call from.
        for door in [DOOR_ATTACH_RUNNING, DOOR_ATTACH_STARTED, DOOR_FOUNDER_HANDOVER] {
            let symbol = match door {
                "attach-running" => "DOOR_ATTACH_RUNNING",
                "attach-started" => "DOOR_ATTACH_STARTED",
                _ => "DOOR_FOUNDER_HANDOVER",
            };
            // Built at run time: a complete needle written as a literal here
            // would be found in this test's own source and the guard would
            // pass by matching itself.
            let needle = format!("{}, {symbol}", "dir");
            let used =
                attach_rs.matches(needle.as_str()).count() + founder_rs.matches(symbol).count();
            assert!(used > 0, "{door} is declared but no path passes it");
        }

        // 2. THE DEFECT ITSELF: the Founder handover must place the rail before
        //    it switches the operator's client. Without this the company is
        //    created, the operator lands in it, and there is no sidebar.
        let handoff = founder_rs
            .find("tmux::handoff_clients(socket, founder_session, company_session)")
            .expect("the Founder handover must still exist");
        let enters = founder_rs
            .find("enter_company_session(")
            .expect("the Founder handover must place the rail — this is the shipped defect");
        assert!(
            enters < handoff,
            "the rail must be placed BEFORE the operator's client is switched in"
        );

        // 3. Every `tmux::attach` in this file is preceded by a call, so a new
        //    attach branch cannot ship railless either.
        //
        //    COUNTED BY ORDER, NOT BY EQUALITY. This used to assert the two
        //    counts were equal, which broke the moment one branch called the
        //    door twice — and it does: an unreconcilable session is abandoned,
        //    the company is stood up again from the CEO, and the rebuilt
        //    session is furnished by a SECOND call before the same single
        //    handover. Equality would have made that recovery look like a
        //    regression. What the guard is actually about is ORDER, so it asks
        //    the question it means: no handover happens before a door.
        let door_needle = format!("enter_company_session(&{}", "socket");
        let attach_needle = format!("tmux::attach(&{}, &session_name)", "socket");
        let doors: Vec<usize> =
            attach_rs.match_indices(door_needle.as_str()).map(|(at, _)| at).collect();
        let attaches: Vec<usize> =
            attach_rs.match_indices(attach_needle.as_str()).map(|(at, _)| at).collect();
        assert!(!attaches.is_empty(), "this file hands the terminal over somewhere");
        assert!(doors.len() >= attaches.len(), "every handover has a door: {doors:?} {attaches:?}");
        for attach in &attaches {
            assert!(
                doors.iter().any(|door| door < attach),
                "every branch that hands the terminal over must place the rail first: the \
                 handover at byte {attach} has no `enter_company_session` before it"
            );
        }
    }

    /// The evidence line the live proof quotes, from a real tmux server.
    fn panes(socket: &str) -> String {
        tmux::run(
            socket,
            &["list-panes", "-a", "-F", "#{session_name} #{window_name} #{pane_current_command}"],
        )
        .stdout
    }

    // TOMBSTONE: `actions_stub`.
    //
    // It served `/v1/org/runtime/actions` with an `actuator.presence` that went
    // `never-attached` for N calls and then `present` — the sequence attach
    // used to gate on. The route is deleted and nothing on this path asks
    // chiefd anything, so these tests now stand up only the thing that decides:
    // a real tmux server on a private socket. That is a stronger harness, not a
    // reduced one — the old stub could report `present` over a socket with no
    // actuator on it, which is the state that stalled a real attach for 45s.

    /// A stand-in for the actuator process: something that stays up and does
    /// nothing. The actuator's OWN behaviour is `actuate::resident`'s and is
    /// tested there; what is under test here is where attach puts it and what
    /// attach does about one that is already there.
    /// The company directory every test in this module acts on.
    ///
    /// A literal, not a tempdir: nothing below reads or writes a company's
    /// files — what is under test is tmux placement and the sentences a
    /// refusal carries — and the directory is what those sentences NAME.
    const COMPANY_DIR: &str = "/work/acme";

    /// Its key, and therefore the tail of every session name below.
    fn company_dir() -> &'static Path {
        Path::new(COMPANY_DIR)
    }

    /// The tmux session the company called `slug` in [`company_dir`] projects
    /// onto. Composed through the production helper so a change to the naming
    /// convention reaches these tests rather than passing them by.
    fn company_session(slug: &str) -> String {
        conventional_session_name(slug, &super::super::paths::company_key(company_dir()))
    }

    fn stand_in() -> Vec<String> {
        vec!["sleep".to_string(), "300".to_string()]
    }

    /// THE PACKET'S FIRST CRITERION.
    ///
    /// `attach` on a company with `presence: never-attached` starts an actuator
    /// and then attaches. Before this, it refused and told the operator to go
    /// and type a second verb; before `876e4b545` it walked into a session
    /// nobody had created and exited 0.
    #[tokio::test]
    async fn a_company_nobody_is_actuating_gets_an_actuator_started_for_it() {
        if !require_tmux() {
            return;
        }
        // A socket nobody has ever served: no `kill-server` precedes the mint
        // below, so the mint has no teardown to lose a race with. See
        // `tmux::test_support`.
        let socket = unique_socket("attach-start");
        let company_session: &str = &company_session("acme");
        let actuator = actuator_session_name(company_session);
        // The company's OWN session, as its actuator would have minted and
        // tagged it. attach never mints this one — see `actuator_session_name`.
        start_session(&socket, company_session, &["sleep", "300"]);

        let outcome = ensure_actuator(company_dir(), &socket, company_session, &stand_in()).await;

        let listed = panes(&socket);
        let state = tmux::actuator_session(&socket, &actuator);
        tmux::run(&socket, &["kill-server"]);

        outcome.expect("attach must START an actuator, not refuse for the lack of one");
        assert_eq!(state, ActuatorSession::Running);
        assert!(
            listed.contains(&format!("{actuator} {}", tmux::ACTUATOR_WINDOW)),
            "the actuator window must be visible to `tmux list-panes -a`; got:\n{listed}"
        );
    }

    /// THE PACKET'S SECOND CRITERION.
    ///
    /// `attach` over a LIVE actuator window attaches and starts NOTHING. Two
    /// actuators for one company is a second source of truth about what should
    /// be running.
    #[tokio::test]
    async fn a_company_that_is_already_actuated_gets_no_second_actuator() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("attach-present");
        let company_session: &str = &company_session("acme");
        let actuator = actuator_session_name(company_session);
        start_session(&socket, company_session, &["sleep", "300"]);
        // An ACTUATED company has a live actuator window, and this fixture
        // mints one. It once stubbed `present` over an empty socket — which is
        // not an actuated company at all, it is a lease whose holder is gone,
        // the exact state that stalled a real attach for 45s. The assertion is
        // unchanged in intent and is the stronger one for it: a company
        // somebody is really actuating gets no SECOND actuator.
        start_session(&socket, &actuator, &["-n", tmux::ACTUATOR_WINDOW, "sleep", "300"]);
        let before = tmux::run(&socket, &["display-message", "-p", "-t", &actuator, "#{pid}"])
            .stdout
            .trim()
            .to_owned();

        let outcome = ensure_actuator(company_dir(), &socket, company_session, &stand_in()).await;

        let listed = panes(&socket);
        let state = tmux::actuator_session(&socket, &actuator);
        let after = tmux::run(&socket, &["display-message", "-p", "-t", &actuator, "#{pid}"])
            .stdout
            .trim()
            .to_owned();
        tmux::run(&socket, &["kill-server"]);

        outcome.expect("an actuated company is entered, not refused");
        assert_eq!(state, ActuatorSession::Running, "the live actuator must survive untouched");
        // The one that was already there is the one still there: not replaced,
        // and not joined by a second.
        assert_eq!(after, before, "the running actuator must not be restarted");
        assert_eq!(
            listed.matches(&actuator).count(),
            1,
            "exactly one actuator session may exist; got:\n{listed}"
        );
    }

    /// At most one actuator, even for one that has only just come up and has
    /// not converged anything yet. A live window is a live actuator, whatever
    /// it has managed to do so far; starting a second beside it is exactly how
    /// a company ends up with two processes disagreeing about what should be
    /// running.
    #[tokio::test]
    async fn an_actuator_that_has_not_converged_anything_yet_is_never_started_twice() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("attach-once");
        let company_session: &str = &company_session("acme");
        let actuator = actuator_session_name(company_session);
        start_session(&socket, company_session, &["sleep", "300"]);
        start_session(&socket, &actuator, &["-n", tmux::ACTUATOR_WINDOW, "sleep", "300"]);
        let before =
            tmux::run(&socket, &["list-panes", "-t", &actuator, "-F", "#{pane_id}"]).stdout;

        let outcome = ensure_actuator(company_dir(), &socket, company_session, &stand_in()).await;

        let after = tmux::run(&socket, &["list-panes", "-t", &actuator, "-F", "#{pane_id}"]).stdout;
        tmux::run(&socket, &["kill-server"]);

        outcome.expect("a starting actuator is waited for, not refused");
        assert_eq!(before, after, "the live actuator pane must be neither respawned nor doubled");
        assert_eq!(before.lines().count(), 1, "exactly one actuator pane");
    }

    /// THE PACKET'S FOURTH CRITERION, and the original defect's shape.
    ///
    /// A failed actuator start makes attach fail LOUDLY. Exiting 0 on a company
    /// that is not running is what sent an operator hunting tmux sockets for a
    /// fault that was never about sockets. The pane is created with
    /// `remain-on-exit` so its own last words survive to be quoted — without
    /// that, tmux reaps the window and the refusal has nothing to say.
    #[tokio::test]
    async fn an_actuator_that_dies_makes_attach_fail_loudly_and_quote_it() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("attach-dead");
        let company_session: &str = &company_session("acme");
        start_session(&socket, company_session, &["sleep", "300"]);

        let doomed = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo no pi runtime was found; exit 3".to_string(),
        ];

        let outcome = ensure_actuator(company_dir(), &socket, company_session, &doomed).await;

        tmux::run(&socket, &["kill-server"]);

        let error = outcome.expect_err("a company that did not come up must never be an exit 0");
        let message = error.to_string();
        assert!(message.contains("its window exited"), "{message}");
        // The actuator's OWN words, which are the only place the cause is.
        assert!(message.contains("no pi runtime was found"), "{message}");
        // …and tmux's own tombstone, which carries the exit status.
        assert!(message.contains("status 3"), "{message}");
        // And the standing recovery, unchanged.
        assert!(message.contains("cd /work/acme && chief actuate"), "{message}");
    }

    /// THE OTHER LIVE-RUN REGRESSION: an actuator that IS present, over a
    /// company chiefd is asking nobody to run, must not be reported as no
    /// actuator at all.
    ///
    /// Measured on a live host. The intent stated while nobody was actuating
    /// committed and left the CEO `desiredActive: false` forever, so the
    /// actuator attach had just started held its lease and converged an EMPTY
    /// plan, round after round. The merged wait reported "it has not taken
    /// chiefd's lease within 45s" — the one thing that was not true — and then
    /// appended the standing "nobody is actuating it" recovery, sending an
    /// operator to start a second actuator beside the healthy one.
    ///
    /// The lost-write half of that is structurally gone: no client states any
    /// intent (chief-home-is-cwd §4c), so there is no write to lose. This test
    /// covers the half that is NOT gone and never was about the intent — an
    /// empty desired set still reaches this wait, from a company chiefd is
    /// genuinely asking nobody to run, and the wait must still say what it
    /// actually sees rather than blaming the actuator standing right there.
    #[tokio::test]
    async fn an_actuator_that_is_present_over_an_empty_roster_is_never_called_missing() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("attach-empty");
        let company_session: &str = &company_session("acme");
        let actuator = actuator_session_name(company_session);
        // An actuator is up; the company session is not, because chiefd is
        // asking for nobody.
        start_session(&socket, &actuator, &["-n", tmux::ACTUATOR_WINDOW, "sleep", "300"]);

        let outcome = await_company_session(company_dir(), &socket, company_session).await;

        tmux::run(&socket, &["kill-server"]);

        let message = outcome.expect_err("no company session is not a success").to_string();
        assert!(message.contains("has a running actuator"), "{message}");
        assert!(message.contains("asking for nobody to run"), "{message}");
        // The two sentences that would send the operator the wrong way.
        assert!(!message.contains("window has not come up"), "{message}");
        assert!(!message.contains("nobody actuating it"), "{message}");
    }

    /// Simulated tmux: the actuator window exists on the company's socket, and
    /// it goes away with the session that hosts it.
    #[test]
    fn the_actuator_window_exists_and_dies_with_its_session() {
        if !require_tmux() {
            return;
        }
        // A virgin socket, because `start_actuator` — production, and the
        // subject here — is what mints on it. A mint that lost a race with a
        // `kill-server` would have been reported as production failing to
        // place an actuator on a live server.
        let socket = unique_socket("attach-life");
        let company_session: &str = &company_session("acme");
        let actuator = actuator_session_name(company_session);

        start_actuator(company_dir(), &socket, company_session, &stand_in())
            .expect("placing an actuator must succeed on a live tmux server");
        let listed = panes(&socket);
        let alive = tmux::actuator_session(&socket, &actuator);

        let killed = tmux::kill_session(&socket, &actuator);
        let gone = tmux::actuator_session(&socket, &actuator);
        let listed_after = panes(&socket);
        tmux::run(&socket, &["kill-server"]);

        assert_eq!(alive, ActuatorSession::Running);
        assert!(
            listed.contains(&format!("{actuator} {}", tmux::ACTUATOR_WINDOW)),
            "`tmux list-panes -a` must prove the actuator exists; got:\n{listed}"
        );
        assert!(killed.expect("killing the actuator session must not error"));
        assert_eq!(gone, ActuatorSession::Absent, "the window must not outlive its session");
        assert!(!listed_after.contains(&actuator), "got:\n{listed_after}");
    }

    /// The company session is the ACTUATOR's projection, and attach must never
    /// mint it.
    ///
    /// `actuate::observe` never destroys a whole session. An empty
    /// organization tag is refused and never authorizes destruction.
    ///
    /// A session minted by attach carries no company tag. Keeping the actuator
    /// out of the company projection also keeps
    /// company replacement independent from actuator lifetime.
    #[test]
    fn the_actuator_session_is_never_the_company_session() {
        let company: &str = &company_session("northwind-logistics");
        let actuator = actuator_session_name(company);
        assert_ne!(actuator, company);
        // The company's name is still IN it, so one `tmux list-sessions` says
        // whose actuator this is.
        assert!(actuator.contains(company), "{actuator}");
        assert_eq!(actuator, format!("chiefd-actuator-{company}"));
    }

    /// THE LIVE-RUN REGRESSION, and the reason the name is a prefix.
    ///
    /// `tmux -t <name>` matches by PREFIX when nothing matches exactly. With
    /// the actuator in `<company-session>-actuator`, the actuator's own first
    /// `observe` asked tmux for the company session, was handed its OWN
    /// session, read an empty organization tag, and the old observer inferred
    /// permission to reap it. Current observation has no whole-session
    /// destruction path; the name rule also prevents the ambiguous lookup.
    ///
    /// Asserted against a REAL tmux server, because the rule under test is
    /// tmux's target resolution and no fake can be evidence about it.
    #[test]
    fn a_live_actuator_session_never_answers_for_the_company_session() {
        if !require_tmux() {
            return;
        }
        // A SOCKET PER ARRANGEMENT, each one virgin. The two halves used to
        // share one socket with a `kill-server` between them, and the mint
        // that followed that teardown is precisely what lost the race under a
        // loaded workspace run: it left NO server, `session_exists` read `no
        // server running` and answered `Some(false)`, and that is
        // character-for-character the answer a real change in tmux target
        // resolution gives. The assertions below are deliberately strict and
        // stay strict; `start_session` makes the SETUP deterministic instead —
        // it retries the mint COMMAND, probes by the EXACT session name so no
        // setup step depends on the prefix behaviour under test, and fails as
        // `setup failed: …` rather than as evidence about tmux.
        let with_the_actuator = unique_socket("attach-prefix-actuator");
        let with_the_old_name = unique_socket("attach-prefix-oldname");
        let company_session: &str = &company_session("acme");
        let actuator = actuator_session_name(company_session);

        start_session(&with_the_actuator, &actuator, &["sleep", "300"]);

        // The company has NO session — only its actuator is up.
        let company_seen = tmux::session_exists(&with_the_actuator, company_session);

        // The negative control, and the whole point: the name this replaced.
        // With the actuator in `<company-session>-actuator`, the SAME probe —
        // the one `actuate::observe` makes before it decides whether a session
        // is torn debris — answers YES for a company that has no session.
        let suffixed = format!("{company_session}-actuator");
        start_session(&with_the_old_name, &suffixed, &["sleep", "300"]);
        let company_seen_with_the_old_name =
            tmux::session_exists(&with_the_old_name, company_session);
        tmux::run(&with_the_actuator, &["kill-server"]);
        tmux::run(&with_the_old_name, &["kill-server"]);

        assert_eq!(
            company_seen,
            Some(false),
            "a company with only an actuator session must read as HAVING NO SESSION"
        );
        assert_eq!(
            company_seen_with_the_old_name,
            Some(true),
            "this test is only evidence while tmux really does match a target by prefix; if this \
             ever reads false, tmux changed and the reasoning above must be re-checked rather than \
             the assertion relaxed"
        );
    }

    /// The whole branch table of what attach does about an existing actuator
    /// session. Production takes exactly these four.
    #[test]
    fn what_attach_does_about_an_existing_actuator_session() {
        // UNCHANGED, and now stated with the build input the rule gained:
        // `Unknowable` is what an actuator that said nothing reports, which is
        // exactly the world these four assertions were written in.
        assert_eq!(
            start_move(ActuatorSession::Running, ActuatorBuild::Unknowable),
            StartMove::LeaveItAlone
        );
        assert_eq!(
            start_move(ActuatorSession::Exited, ActuatorBuild::Unknowable),
            StartMove::ReplaceExited
        );
    }

    /// THE BUILD ONLY EVER DECIDES THE LIVE CASE, and only when the answer is
    /// PROVEN.
    ///
    /// A live actuator on a different build is replaced; a live actuator
    /// nobody can identify is left alone, because it is still the actuator
    /// this company is being run by and stopping it to satisfy an unanswered
    /// question takes a working company down. Every other session state is
    /// what it always was, whatever the build says — a corpse is replaced
    /// because it is a corpse, an absent session is created, and a session
    /// tmux would not describe is still refused rather than doubled.
    #[test]
    fn a_live_actuator_is_replaced_only_when_its_build_is_provably_wrong() {
        assert_eq!(
            start_move(ActuatorSession::Running, ActuatorBuild::Stale),
            StartMove::ReplaceStale,
            "a proven mismatch on a LIVE actuator is the one case the build decides"
        );
        assert_eq!(
            start_move(ActuatorSession::Running, ActuatorBuild::Current),
            StartMove::LeaveItAlone
        );
        assert_eq!(
            start_move(ActuatorSession::Running, ActuatorBuild::Unknowable),
            StartMove::LeaveItAlone,
            "unknowable never stops a live actuator"
        );
        for build in [ActuatorBuild::Current, ActuatorBuild::Stale, ActuatorBuild::Unknowable] {
            assert_eq!(
                start_move(ActuatorSession::Exited, build),
                StartMove::ReplaceExited,
                "a corpse is replaced because it is a corpse: {build:?}"
            );
            assert_eq!(
                start_move(ActuatorSession::Absent, build),
                StartMove::CreateOne,
                "nothing there is nothing to judge: {build:?}"
            );
            assert_eq!(
                start_move(ActuatorSession::Unknown, build),
                StartMove::FailClosed,
                "an unreadable session must never be doubled, whatever the build says: {build:?}"
            );
        }
    }

    /// #1207: the death is reported at the moment a human is looking, because
    /// the pane that holds the evidence is destroyed one line later.
    #[test]
    fn replacing_a_corpse_says_what_it_replaced() {
        let line = corpse_narration(
            "/companies/northstar",
            Some(1),
            "chiefd: converged 3 up\nerror: 403 unknown identity\n\n",
        );
        assert!(line.contains("/companies/northstar"), "{line}");
        assert!(line.contains("status 1"), "{line}");
        assert!(line.contains("403 unknown identity"), "the last WORD it said: {line}");
        assert!(line.contains("replacing it"), "{line}");

        // Trailing blank lines are not the last line: a corpse whose pty was
        // drained would otherwise report an empty quotation.
        let blank = corpse_narration("acme", Some(101), "panicked at 'boom'\n\n\n");
        assert!(blank.contains("panicked at 'boom'"), "{blank}");

        // A pane that printed nothing says so rather than quoting emptiness.
        let silent = corpse_narration("acme", Some(0), "   \n\n");
        assert!(silent.contains("printed nothing"), "{silent}");
        assert!(silent.contains("status 0"), "even a clean exit is a dead actuator: {silent}");

        // An unreadable status is named, never guessed at.
        let unknown = corpse_narration("acme", None, "last words");
        assert!(unknown.contains("an unreadable status"), "{unknown}");
        assert_eq!(
            start_move(ActuatorSession::Absent, ActuatorBuild::Unknowable),
            StartMove::CreateOne
        );
        // The one that matters: "I could not tell" must never start a second.
        assert_eq!(
            start_move(ActuatorSession::Unknown, ActuatorBuild::Unknowable),
            StartMove::FailClosed
        );
    }

    /// Only a LIVE actuator window stops this client starting one.
    ///
    /// # The measured defect this rule came from
    ///
    /// The gate was `if !actuator_present()`. Presence was derived from the
    /// last committed report against the reader's clock, so it outlived its
    /// holder by up to the lease window — and in that window attach started
    /// nobody, stated CEO-only intent nobody would carry out, and burned the
    /// whole 45s `ACTUATOR_BUDGET` before failing.
    ///
    /// Measured on a build host, sampling chiefd's own
    /// `/v1/org/runtime/actions` beside `pgrep` and the tmux socket every
    /// 250ms across cold attaches: 188 consecutive samples reported
    /// `presence: present` while ZERO `chief actuate` processes existed on the
    /// host and no actuator session existed. Two of five cold attaches failed
    /// that way; the passing ones differed only in that the stale lease had
    /// already gone `lapsed`.
    ///
    /// The lease is now gone entirely, so the state it produced is
    /// unrepresentable rather than outvoted. This asserts the whole surviving
    /// table, because every arm of it is a decision about starting a second
    /// process.
    #[test]
    fn only_a_live_actuator_window_stops_a_second_being_started() {
        // A LIVE window: somebody is really actuating, so this client must not
        // add a second. The wait decides whether they arrive.
        assert!(!actuator_needed(ActuatorSession::Running));

        // THE REGRESSION, in the two states that used to read `present` from a
        // lease no process backed.
        assert!(
            actuator_needed(ActuatorSession::Absent),
            "no session on the socket is nobody actuating, whatever any record says"
        );
        assert!(
            actuator_needed(ActuatorSession::Exited),
            "an exited window is a corpse kept on screen by remain-on-exit, not an actuator"
        );

        // An unreadable tmux runs the START PATH, which then refuses. This
        // answered `false` and cost the operator the whole ACTUATOR_BUDGET for
        // nothing: `start_move(Unknown)` is `FailClosed`, so the duplicate
        // actuator the old answer protected against could never have happened
        // on this path. Measured, same setup, only this arm differing —
        // `false`: 46.90s ending in "chiefd is still asking for nobody to run
        // … not a tmux fault", which is confidently wrong about the cause;
        // `true`: 1.75s ending in "could not read tmux session … Check the
        // tmux server, then retry."
        assert!(
            actuator_needed(ActuatorSession::Unknown),
            "an unreadable tmux must reach the start path, which refuses by name"
        );
    }

    /// THE ARGV IS THIS BINARY AND ONE VERB, and the company is not a word in
    /// it.
    ///
    /// `chief actuate` acts on the directory it is run in, and the router
    /// REFUSES a positional — so a third word here would not be harmless
    /// redundancy, it would make every actuator start fail with a usage error
    /// inside a pane nobody is reading. The company reaches the pane as its
    /// start directory instead (`tmux::start_actuator`'s `-c`).
    #[test]
    fn the_actuator_is_started_with_this_binarys_own_path_and_the_verb_that_runs_people() {
        let command = actuator_command("/root/.chief/bin/chief");
        assert_eq!(command, vec!["/root/.chief/bin/chief".to_string(), "actuate".to_string()]);
        assert!(
            super::super::route(&command[1..]).is_ok(),
            "the argv the actuator is started with must be one this binary routes: {command:?}"
        );
    }

    #[test]
    fn a_failed_actuator_names_the_window_quotes_it_and_ends_with_the_recovery() {
        let message = actuator_failed(
            company_dir(),
            "org-acme-actuator",
            "chiefd-acme",
            "its window exited",
            "Pi is required but no runtime was found.",
        );
        assert!(message.contains("org-acme-actuator"), "{message}");
        assert!(message.contains("chiefd-acme"), "{message}");
        assert!(message.contains("Pi is required but no runtime was found."), "{message}");
        assert!(message.contains("cd /work/acme && chief actuate"), "{message}");
        // A silent pane is said to be silent rather than left looking read.
        let quiet =
            actuator_failed(company_dir(), "org-acme-actuator", "chiefd-acme", "it exited", "");
        assert!(quiet.contains("printed nothing"), "{quiet}");
    }

    /// THE COLD-ATTACH REGRESSION, ordering half — on a REAL tmux server.
    ///
    /// A preflight that refuses is only worth anything if it refuses BEFORE the
    /// window exists. The defect this packet closes was reported as
    /// `unusable window dimensions "\t\n"` precisely because the thing that was
    /// wrong was discovered after tmux had already minted a pane, watched its
    /// command die, and reaped the empty window around it. So this asserts the
    /// order against a live socket: `resolve_actuator_command` refuses a host
    /// it cannot clear, and the socket is left with no actuator session at all.
    #[test]
    fn a_refused_host_mints_nothing_on_the_tmux_server() {
        if !require_tmux() {
            return;
        }
        // A socket nobody has ever served. This test asserts an ABSENCE, and
        // an absence over a socket that never had a server is evidence in a
        // way an absence over a reused one is not.
        let socket = unique_socket("attach-refuse");
        let company_session = company_session("refused");
        let actuator_session = actuator_session_name(&company_session);
        // TOMBSTONE: a throwaway `$HOME` used to stand here, on the reasoning
        // that "a home no release has ever touched" cannot clear the gates.
        // The gate no longer asks about `$HOME` at all — it asks whether this
        // running binary has a `resources/` directory beside it, and a test
        // binary under `target/…/deps/` never does. So the host is uncleared
        // for a reason that is a property of THIS process rather than of a
        // directory the test invented, which is a stronger setup and a shorter
        // one.
        let refusal = super::resolve_actuator_command(company_dir())
            .expect_err("an uncleared host must never produce an actuator command");

        let message = refusal.to_string();
        // Named, with the operator's next move — never a tmux symptom.
        assert!(
            message.contains("chief actuate <company>"),
            "the refusal must name the verb that was refused: {message}"
        );
        assert!(!message.contains("dimensions"), "{message}");
        assert!(!message.contains("window"), "{message}");
        // And nothing was minted: no actuator session, no company session.
        assert_eq!(
            tmux::actuator_session(&socket, &actuator_session),
            ActuatorSession::Absent,
            "a refusal must happen before any pane exists"
        );
        assert_ne!(
            tmux::session_exists(&socket, &company_session),
            Some(true),
            "a refused host must not leave a company session behind"
        );
        assert!(
            !panes(&socket).contains(&actuator_session),
            "tmux must show no pane for a host that was refused"
        );
    }

    /// The pi runtime travels as a SPAWN ARGUMENT, not as a forwarded ambient
    /// variable, and this pins that decision.
    ///
    /// `ACTUATOR_ENVIRONMENT` is an allowlist — a hand-maintained inventory of
    /// what somebody once remembered a child process needed. The cold-attach
    /// defect was a missing entry in exactly that class of list, and growing
    /// the list would have fixed this host and left the next one to be found
    /// the same way. `chiefd` resolves the runtime absolutely and passes it to
    /// the daemon it spawns, so no ambient forwarding decides whether a company
    /// can start.
    #[test]
    fn the_pi_runtime_is_not_something_this_list_has_to_remember() {
        assert!(
            !ACTUATOR_ENVIRONMENT.contains(&super::super::daemon::PI_BINARY_ENV),
            "the pane's pi binary is passed to the daemon, never inherited from a tmux server"
        );
    }

    /// The two crates that must agree on one string, and neither links the
    /// other. `chiefd` asserts the same literal from its own side; a
    /// rename on either side leaves one of these two tests red.
    #[test]
    fn the_pi_binary_environment_name_is_the_one_the_daemon_reads() {
        assert_eq!(super::super::daemon::PI_BINARY_ENV, "CHIEFD_PI_BINARY");
    }

    #[test]
    fn the_actuator_pane_environment_carries_no_credential() {
        // Same rule as the Founder pane's list: credentials travel only on each
        // person's private 0600 files, never through a tmux server's long-lived
        // environment.
        for name in ACTUATOR_ENVIRONMENT {
            let lowered = name.to_lowercase();
            assert!(!lowered.contains("key"), "{name}");
            assert!(!lowered.contains("token"), "{name}");
            assert!(!lowered.contains("secret"), "{name}");
            assert!(!lowered.contains("credential"), "{name}");
        }
    }

    /// THE LIVE-PROOF REGRESSION (#751/P8).
    ///
    /// A live run of the old `chief attach northwind-logistics --yes` surface against a
    /// company nobody was actuating printed, and then exited 0:
    ///
    /// ```text
    /// chief attach: booting 'northwind-logistics' (CEO-only)
    /// can't find session: org-northwind-logistics
    /// could not switch this tmux client to ChiefD session '…' on socket
    /// 'default' (tmux exited 1). … If the company runs on a different socket,
    /// detach first (prefix then d) and retry.
    /// ```
    ///
    /// The socket was right, the daemon was healthy, and the advice could not
    /// have worked. The real fact — nobody is actuating, so nobody is running —
    /// was available on `/v1/org/runtime/actions` the whole time and was never
    /// asked for.
    #[test]
    fn a_company_with_no_actuator_is_refused_by_name_and_told_the_verb_that_runs_it() {
        let refusal = no_actuator_refusal(company_dir());
        // The ONE command that starts a company, AND the directory it has to be
        // typed in — a bare `chief actuate` in another terminal would act on
        // whatever company that terminal happens to be standing in.
        assert!(
            refusal.contains("cd /work/acme && chief actuate"),
            "the ONE command that starts a company must appear, aimed: {refusal}"
        );
        assert!(refusal.contains("nobody actuating it"), "{refusal}");
        // The wrong diagnosis this replaces must not come back.
        assert!(!refusal.contains("different socket"), "{refusal}");
        assert!(!refusal.contains("detach first"), "{refusal}");
    }

    /// THE OPERATOR'S RULING, TESTED WHERE IT BITES.
    ///
    /// After a hard reboot their company printed tmux's own words twice and
    /// never came up:
    ///
    /// ```text
    /// have 6 panes but need 5: 6a8a,225x47,0,0{26x47,0,0,29,…}
    /// chief attach could not install the viewport hook set: command too long
    /// ```
    ///
    /// Both are raised inside the viewport publication, and both used to be
    /// fatal. This drives the SAME branch for real rather than mocking it: under
    /// `cargo test` stdio is captured, so `terminal::operator_size` reports no
    /// terminal and the publication cannot run at all — the harshest version of
    /// "the viewport pass failed". On the tree that shipped to the operator
    /// this returns `Err("chief attach could not read the operator terminal
    /// size before publication")` and the attach is refused. It must not.
    ///
    /// The company is what the operator asked for, and the company is there: the
    /// rail is minted before the publication and this proves it survived.
    #[test]
    fn a_viewport_that_cannot_be_published_never_refuses_the_attach() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("viewport-never-refuses");
        let session = "org-never-refuses_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        for argv in [
            vec!["set-option", "-t", session, "@organization_id", "cobalt"],
            vec!["set-option", "-w", "-t", session, "@organization_window_id", "executive"],
        ] {
            assert!(tmux::run(&socket, &argv).ok(), "fixture option: {argv:?}");
        }
        let dir = tempfile::tempdir().expect("company dir");

        let entered = super::enter_company_session(&socket, session, dir.path(), "test");

        assert!(
            entered.is_ok(),
            "geometry is never a reason to keep an operator out of their company: {:?}",
            entered.err()
        );
        let rails = tmux::run(
            &socket,
            &["list-panes", "-s", "-t", session, "-F", "#{@organization_sidebar}"],
        );
        assert!(
            rails.stdout.lines().filter(|line| line.trim() == "1").count() == 1,
            "the rail is still placed: {}",
            rails.stdout
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    /// *"If you get a mismatch like that, just boot just `@chief`. we cannot
    /// just die like that."* The first half of obeying that is throwing the
    /// unreconcilable projection away — for real, on a real server, leaving the
    /// server itself up so the CEO can be stood back up on it.
    #[test]
    fn an_unreconcilable_session_is_abandoned_and_its_server_survives() {
        if !require_tmux() {
            return;
        }
        let socket = unique_socket("abandon-unreconcilable");
        let session = "org-abandon_";
        let keeper = "org-keeper_";
        start_session(&socket, session, &["-x", "120", "-y", "30", "sleep", "120"]);
        start_session(&socket, keeper, &["-x", "120", "-y", "30", "sleep", "120"]);
        assert_eq!(tmux::session_exists(&socket, session), Some(true));

        assert!(super::abandon_unreconcilable_session(&socket, session));

        assert_eq!(tmux::session_exists(&socket, session), Some(false));
        assert_eq!(
            tmux::session_exists(&socket, keeper),
            Some(true),
            "abandoning one company's projection is not a reason to take the server down"
        );
        let _ = tmux::run(&socket, &["kill-server"]);
    }

    #[test]
    fn a_running_company_adopts_its_daemon_immediately() {
        assert_eq!(daemon_move(DaemonStatus::Running), DaemonMove::Adopt);
    }

    #[test]
    fn a_running_company_with_a_live_session_is_entered_immediately() {
        assert_eq!(session_move(true, Some(true)), SessionMove::Enter);
    }

    /// ONE INVOCATION. This used to refuse and tell the operator to run `chief
    /// stop` and retry — two invocations and a memorized command, for a state a
    /// hard reboot produces on its own (a re-used pid behind
    /// `.chief/run/daemon.json` reads as alive-but-not-answering). The refusal's
    /// own advice IS the recovery, so `chief` performs it instead of printing
    /// it.
    #[test]
    fn an_unhealthy_daemon_is_recovered_in_the_same_invocation() {
        assert_eq!(daemon_move(DaemonStatus::Unhealthy), DaemonMove::Recover);
    }

    #[test]
    fn a_stopped_company_starts_without_a_confirmation_branch() {
        assert_eq!(daemon_move(DaemonStatus::Stopped), DaemonMove::Start);
    }

    #[test]
    fn a_running_daemon_whose_session_is_gone_is_brought_up_not_entered() {
        assert_eq!(session_move(true, Some(false)), SessionMove::BringUp);
        assert_eq!(session_move(true, None), SessionMove::BringUp);
    }

    /// WHERE A REAL OPERATOR'S TERMINAL LANDS, with two prefix-related
    /// companies on one tmux server.
    ///
    /// [`super::super::tmux::attach`] execs exactly
    /// `tmux -L <socket> attach-session -t <session>`, and its `<session>` is
    /// [`conventional_session_name`]'s answer, unchanged. This drives those
    /// three tokens from a REAL pty client — a second tmux server hosting the
    /// terminal, the same shape as an operator's own — because where a client
    /// ends up is decided inside tmux and no fake can be evidence about it.
    ///
    /// Two arrangements, plus the control:
    ///
    /// 1. `acme` STOPPED, `acme-corp` running: attaching to `acme` must land
    ///    NOBODY. Under `org-<slug>` it landed the operator in `acme-corp`.
    /// 2. both up: attaching to `acme` lands on `acme`'s session and no other.
    /// 3. the control, under the old names, showing the operator being moved
    ///    into the wrong company for real.
    #[test]
    fn attach_lands_on_the_named_company_when_another_companys_slug_prefixes_it() {
        if !require_tmux() {
            return;
        }
        // A SOCKET PER ARRANGEMENT, never `kill-server` and rebuild: a
        // teardown and a re-creation on one socket race each other, and a
        // `new-session` that lost that race is an EMPTY server — which reads
        // exactly like "the attach found nothing", the answer under test. Each
        // socket is one nobody has ever served, so no teardown precedes a mint
        // on it at all.
        let stopped_socket = unique_socket("attach-stopped");
        let old_socket = unique_socket("attach-oldnames");
        let acme = company_session("acme");
        let acme_corp = company_session("acme-corp");

        // Where the clients on a socket are sitting.
        let clients =
            |socket: &str| tmux::run(socket, &["list-clients", "-F", "#{session_name}"]).stdout;
        // What an operator's own terminal has on screen — a failed attach
        // prints tmux's refusal there and nowhere else.
        let terminal_says = |terminal: &str| {
            tmux::run(terminal, &["capture-pane", "-p", "-S", "-", "-t", "term"]).stdout
        };
        // Wait, bounded, for something an EXTERNAL process does.
        fn wait_for(condition: impl FnMut() -> bool) -> bool {
            wait_until(10_000, condition)
        }
        // A company's session, retried into existence. A fixture that failed
        // to build must never be reported as a finding about the product.
        let company_session_on =
            |socket: &str, name: &str| start_session(socket, name, &["sleep", "300"]);
        // The operator's terminal, running the exact argv `tmux::attach` execs
        // against `target`: `tmux -L <socket> attach-session -t <session>`.
        // The command IS the pane's process rather than something typed into a
        // shell, so there is no race between the pane starting and the keys
        // arriving — a lost keystroke would read as "the attach found nothing",
        // which is precisely the answer under test. `sleep` keeps the pane
        // alive afterwards so tmux's refusal survives to be read.
        let operator_attaches_to = |terminal: &str, socket: &str, target: &str| {
            let command = format!("tmux -L {socket} attach-session -t {target}; sleep 300");
            let made =
                tmux::run(terminal, &["new-session", "-d", "-s", "term", "sh", "-c", &command]);
            assert!(made.ok(), "the operator's terminal must exist: {}", made.diagnostic());
        };

        // 1. `acme` is STOPPED. Only `acme-corp` is up. Then, on the SAME
        //    server, `acme` comes up too and the operator asks for it again.
        let stopped_terminal = unique_socket("attach-term-stopped");
        let both_terminal = unique_socket("attach-term-both");
        company_session_on(&stopped_socket, &acme_corp);
        operator_attaches_to(&stopped_terminal, &stopped_socket, &acme);
        // Wait for tmux's own answer rather than for a timeout to elapse: an
        // absence proved by "I gave up" is the weakest evidence available.
        let refused = wait_for(|| terminal_says(&stopped_terminal).contains("can't find session"));
        let refusal = terminal_says(&stopped_terminal);
        let after_stopped_attach = clients(&stopped_socket);

        // 2. Both companies up. The operator asks for `acme` and must get it.
        company_session_on(&stopped_socket, &acme);
        operator_attaches_to(&both_terminal, &stopped_socket, &acme);
        let landed_on_acme =
            wait_for(|| clients(&stopped_socket).lines().any(|line| line.trim() == acme));
        let after_both_up = clients(&stopped_socket);
        let panes_evidence = panes(&stopped_socket);

        // 3. THE CONTROL, on its own server: the same arrangement under the
        //    names the old `org-<slug>` convention minted, which is the
        //    shipped defect.
        let old_terminal = unique_socket("attach-term-old");
        company_session_on(&old_socket, "org-acme-corp");
        operator_attaches_to(&old_terminal, &old_socket, "org-acme");
        let old_convention_landed_on_the_neighbour =
            wait_for(|| clients(&old_socket).lines().any(|line| line.trim() == "org-acme-corp"));
        let old_convention_clients = clients(&old_socket);
        let old_convention_terminal = terminal_says(&old_terminal);

        for server in
            [&stopped_socket, &old_socket, &stopped_terminal, &both_terminal, &old_terminal]
        {
            tmux::run(server, &["kill-server"]);
        }

        assert!(
            refused,
            "attaching to a STOPPED '{acme}' must be REFUSED by name; the terminal said:\n{refusal}"
        );
        assert!(
            after_stopped_attach.trim().is_empty(),
            "no client may have landed anywhere, and none may be in '{acme_corp}'; clients \
             were:\n{after_stopped_attach}"
        );
        assert!(
            landed_on_acme,
            "attaching to '{acme}' with both companies up must land on it; clients \
             were:\n{after_both_up}\npanes:\n{panes_evidence}"
        );
        assert!(
            !after_both_up.lines().any(|line| line.trim() == acme_corp),
            "no client may be sitting in '{acme_corp}'; clients were:\n{after_both_up}"
        );
        assert!(
            old_convention_landed_on_the_neighbour,
            "this test is only evidence while tmux really does resolve a target by PREFIX: under \
             the old convention, attaching to a stopped 'org-acme' put the operator's own terminal \
             inside 'org-acme-corp'. If this ever reads false, tmux changed and the reasoning must \
             be re-checked rather than the assertion relaxed. Clients were:\n{old_convention_clients}\n\
             The terminal said:\n{old_convention_terminal}"
        );
    }
}
