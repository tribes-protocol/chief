//! `reconcile_cycle` — the M2 orchestrator, wrapped as a [`ReconcileActuator`].
//!
//! One runtime-actuation pass, run after `supervision::cycle` has committed this
//! tick's ledger state (design Q4):
//!
//! `begin_cycle` (single-flight + floor) → **re-project the activity fence**
//! (`activity::reconcile` under the launch-intent fence the actuator read from
//! the legacy `org_documents` store, so the desired set reflects who is
//! actually authorized to run rather than a frozen bootstrap snapshot) → read
//! the actuator's committed observation → project the committed manifest +
//! activity ledger into the desired roster → plan the
//! per-person runtime actions → the #29 pointer-sweep (compute + fenced
//! compare-and-clear) → open a `converge` intent row (audit / shadow-diff) →
//! close it → publish what was observed → `end_cycle`.
//!
//! One safety rule is load-bearing here: the **effective apply mode is the
//! conservative AND** of the daemon's requested mode and the company's durable
//! safety config, so a tripped breaker (which forces the config to shadow)
//! always wins.
//!
//! TOMBSTONE: the start and destructive budgets. They were enforced inside
//! `plan_runtime_actions`, which emitted up to the limit and reported the
//! remainder as `deferred_starts`/`deferred_restarts` — a bound on what was
//! asked for, never a refusal of the pass (#369's livelock came from refusing).
//! Both are deleted with the action stream itself: a budget bounds a count of
//! DESTRUCTIVE ACTIONS, and chiefd issues no actions. It publishes a desired
//! set, and absence from that set is the whole instruction.
//!
//! #751/P8-P10: there is no planning half left to separate. chiefd computes the
//! desired set and the safety policy that rides on it and publishes both; the
//! ordered walk of pane steps, and the executor that drove it, live in the
//! operator client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chiefd_core::actor::{CompanyDb, MutationClass, MutationName};
use chiefd_core::ledger::LedgerSnapshot;
use chiefd_core::runtime::converge_intent::{self, ConvergeIntentBody};
use chiefd_core::runtime::duty_hooks::{
    ActuationMode, BoxFuture, DutyContext, DutyError, ReconcileActuator, ReconcileReport,
};
use chiefd_core::runtime::pointer_sweep::{compute_pointer_sweep, ClearPointerAction};
use chiefd_core::runtime::project::project_sweep_input;
// `RampConfig` moved here with the rest of the admission model: a ramp is not
// part of a plan, and `runtime::actuation` is its only consumer.
use chiefd_core::store::activity::{self, LaunchFence, ReconcileInput};
use chiefd_core::store::organization::{self, OrganizationManifest};
use chiefd_core::store::reconciler_facts::PendingMailFact;
use chiefd_core::store::supervision;

use crate::converge_apply::safety::{self, CycleGate, SkipReason};
// #751/P8: `apply_plan_with_launch_roster`, `observe`, `CommittedBindings`,
// `LaunchInputs` and the `HostExecutor` seam are all GONE from this file, and
// from this crate. chiefd cannot observe a display and cannot drive one; the
// operator client does both and reports what it saw. What survives here is the
// desired set and the safety policy that rides on it.
use chiefd_core::runtime::actuation::{publish_desired_runtime, DesiredPerson, DesiredRuntime};
// The launch catalog is a PUBLISHED FACT now, not an in-process value: this
// crate derives it (it owns the on-disk gate), `chiefd-api` serves it on
// `POST /v1/org/runtime/launch-catalog`, and the operator client maps it into
// its own `LaunchSpec`. `LaunchSpec` itself left with the interpreter in
// #751/P8 and is deliberately not named anywhere in this crate.
use crate::gather::reconciler_facts::ReconcilerFactsStore;
use chiefd_core::runtime::launch_catalog::{EnvAssignment, LaunchCatalog, LaunchEntry};
use chiefd_core::runtime::roster::project_desired_roster;

/// Map any displayable error into a non-fatal [`DutyError`] (one skipped pass).
fn duty(error: impl std::fmt::Display) -> DutyError {
    DutyError::new(error.to_string())
}

// TOMBSTONE: `admission_runtime_bootstrap`. It seeded a minimal `starting`
// runtime projection so the CEO's admission watermark survived the gap before
// the launcher's first stable OBSERVATION arrived. There is no observation and
// no gap to bridge, and its only caller went with the observation publish.

// TOMBSTONE: `startup_admission_ramp`. It carried the live wall-clock remainder
// of `runtime.startup_admission_until` into a `RampConfig` for the pass to pace
// its spawns against. There is no ramp: the actuator boots every missing pane
// in one pass, so there is no pacing for a deadline to carry into. The durable
// `startup_admission_until` column and its writer stay -- an operator-visible
// fact with a live mutator is not deleted because one reader stopped needing it.

/// Host-side configuration an actuator holds for one company: where its runtime
/// server and data live, and the cycle floor interval.
#[derive(Debug, Clone)]
pub struct ActuatorConfig {
    /// The runtime server socket NAME (`runtime -L <socket>`). Not derivable
    /// from the ledger — supplied by the host layer (mirrors the ownership
    /// record).
    ///
    /// A plain `String`, not the `Socket` newtype it used to be: `Socket` was
    /// the handle you PASSED to a runtime command, and chiefd runs none. What
    /// survives is the socket's *identity* — the string the ownership row
    /// compares to decide Owned vs Foreign, and the value published into every
    /// person's env as `ORG_LAUNCHER_RUNTIME_SOCKET`. Both are facts chiefd
    /// owns and neither needs a type that can address a display.
    pub socket: String,
    /// The company DIRECTORY — the one the operator stood in. Everything chief
    /// owns for this company hangs off `<dir>/.chief` ([`Self::data_root`]),
    /// and the pane variable `ORG_LAUNCHER_ORG_DIR` is this value EXACTLY.
    ///
    /// # Why this is the stored fact and the `.chief` root is derived
    ///
    /// It was the other way round, and the two consumers had drifted a segment
    /// apart without any test noticing. The pane env stamp read
    /// `let company_dir = config.data_root().clone()` — so a pane was told its
    /// company directory was `<dir>/.chief`, and every reader that joins onto
    /// it (`chiefd-log`'s `<dir>/.chief/log`, the rendezvous file at
    /// `<dir>/.chief/run/daemon.json`, `organization-intercom.ts`) looked one
    /// `.chief` too deep and found nothing.
    ///
    /// Storing the directory and DERIVING the `.chief` root makes that
    /// mistake unspellable: a join is total and one-directional, whereas
    /// recovering `<dir>` from `<dir>/.chief` means walking up — the same
    /// reconstruction that made the log sink's deleted tier-2 wrong.
    pub dir: PathBuf,
    /// When THIS daemon started watching, ISO-8601 — the instant it began
    /// being able to receive an agent heartbeat at all.
    ///
    /// It is config rather than a per-pass value because it is a fact about the
    /// PROCESS, identical for every company and every pass, and reading it once
    /// where the daemon starts is the only place it is knowable. It reaches the
    /// one thing that needs it, `ReconcileInput::watching_since`, which clamps
    /// the inferred quiet instant so a chiefd restart longer than the liveness
    /// bound does not settle everybody who was mid-turn when it stopped.
    ///
    /// A test config states an instant in the distant past — "watching for
    /// ever" — which is the pre-clamp behaviour and keeps every existing
    /// expectation about quiet instants exact.
    pub watching_since: String,
    /// Authoritative home whose `.chiefd` root holds bootstrap state. Every
    /// pane receives this explicitly so the runtime's inherited environment cannot
    /// redirect its launcher CLI to another operator's registry.
    pub home: PathBuf,
    /// The pinned pi binary to launch people with.
    pub pi_binary: PathBuf,
    /// Minimum spacing between cycle starts (single-flight floor).
    ///
    /// The single-flight floor is NOT the deleted ramp: it bounds how often a
    /// company converges, not how fast its people are allowed to boot within
    /// one pass.
    pub floor: Duration,
    /// The launcher install root (`ORG_LAUNCHER_ROOT`) every spawned pane
    /// receives. The person's `organization-intercom` extension refuses to
    /// load without it (`requiredEnvironment`), which kills a freshly spawned
    /// pi before the plan's tagging step can run — so this is authoritative
    /// daemon config, never a best-effort env passthrough.
    pub launcher_root: PathBuf,
    /// The OPERATOR's own Pi agent directory — `PI_SOURCE_AGENT_DIR` when set,
    /// else `$HOME/.pi/agent`. Chief holds no provider, model or credential
    /// state of its own; this is the one place it reads the operator's, and it
    /// is read-only to the actuator.
    pub root_pi_agent_dir: PathBuf,
    // A pane used to be handed the daemon's own docstore address as an env
    // stamp. It is not any more, and the field that held it is gone: a pane's
    // extensions resolve their OWN company through beacond, once per install
    // (`organization-intercom.ts`, `team-ui.ts`).
    // One address per process was only ever right for one pane serving one
    // company; beacond answers per company, so there is nothing left for the
    // actuator to publish and no inherited value worth forwarding.
    //
    // #739 P3's positive-evidence registry (`ever_observed`) is GONE from this
    // struct and from this crate. It accumulated "this pane was once seen
    // alive" across passes so a later unproven read could not be mistaken for
    // absence — a memory of OBSERVATIONS, and chiefd makes none. It lives in
    // `chief-cli`, next to the `observe()` whose answers it remembers.
}

impl ActuatorConfig {
    /// Everything chief owns for this company: `<dir>/.chief`. The
    /// materialization root, the store, the keys and the logs all hang off it.
    ///
    /// DERIVED and never stored — see [`ActuatorConfig::dir`] for the segment
    /// the two-field version lost.
    #[must_use]
    pub fn data_root(&self) -> PathBuf {
        self.dir.join(CHIEF_DIR)
    }
}

/// The directory chief owns inside a company directory.
///
/// Spelled here as well as in `chiefd_log::sink` and
/// `host_primitives::rendezvous` — deliberately, and this is the whole reason:
/// both of those are LEAF crates whose dependency lists are kept empty on
/// purpose, because anything added to one is forced on both actuators at once.
/// Spending that property to share a nine-character string constant is a worse
/// trade than writing it three times, so each spelling names the others.
pub(crate) const CHIEF_DIR: &str = ".chief";

/// Pane-env keys forwarded best-effort from chiefd's own process environment,
/// ported from `organizationPersonPiCommand`: `TEAM_LAUNCHER_BUN` is the bun
/// runtime a person's extensions shell out through. The bootstrap registry is
/// intentionally not forwarded: it is always resolved under `~/.chiefd`, so
/// an inherited pane environment cannot redirect durable launcher state.
///
/// `ORG_LAUNCHER_ROOT` is deliberately NOT in this list: it IS load-required
/// (`organization-intercom.ts`'s `requiredEnvironment` throws at extension
/// load when it is absent, which is exactly how the first live chiefd spawn
/// died before its tagging step), so the catalog sets it authoritatively from
/// [`ActuatorConfig::launcher_root`] below instead of hoping chiefd's own env
/// happens to carry it. Promoting the remaining keys to real
/// `ActuatorConfig` fields is a follow-up once a concrete caller needs them
/// guaranteed-set.
///
/// `BEACOND_URL` joins the list for #983, and forwarding it VERBATIM is
/// correct in a way forwarding a chiefd address never was. beacond is the
/// company REGISTRY: there is one per box, it is the same for every company,
/// and this daemon registered itself with the one its own env names. A pane
/// that asked a different registry would be asking something that has never
/// heard of this company — a loud miss, not the silent wrong-daemon answer a
/// forwarded per-company address produced (#227/#420 hop-2, below). Absent
/// here, a pane falls back to beacond's own compiled-in default, which is the
/// right answer for every non-test deployment.
/// The CA trust store joins the list because a person who cannot complete a TLS
/// handshake cannot think, and nothing in the product said so.
///
/// MEASURED on a zipbox box: `/etc/hosts` maps `openrouter.ai` and two dozen
/// other provider hosts to a local interceptor, and that interception is TLS
/// terminated — a process reaches it only by trusting
/// `/run/zipbox/ca-bundle.crt`. The path is exported by `/etc/profile.d`, so a
/// LOGIN shell has it and a person's Pi, which is not one, does not. Same key,
/// same URL, only difference the CA variable: `http=000` in 22ms without it,
/// `http=200` in 913ms with it. Every person in every company on that host
/// printed `Error: Connection error.` and then three consecutive provider
/// failures, and the only place it was visible was inside the pane.
///
/// This does NOT weaken Invariant 32 — chiefd holds no credential. These four
/// name a PATH TO A PUBLIC TRUST STORE, which is not a secret and carries no
/// authority: possessing it lets a process verify a server, never impersonate
/// one or authenticate as anybody. A key would still be refused here.
///
/// Best-effort like the rest of the list: absent on the host, absent in the
/// pane, and a deployment that does not intercept egress needs none of them.
const BEST_EFFORT_FORWARDED_ENV_KEYS: [&str; 6] = [
    "TEAM_LAUNCHER_BUN",
    "BEACOND_URL",
    // Node reads the first, OpenSSL-linked clients the second, curl the third;
    // Python's certifi consumers read the fourth. A person's Pi is one process
    // that may use several of those paths, so all four travel together or the
    // one that is missing is the one that decides.
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
];

/// The operator's own Pi agent directory: the launcher's source-home override
/// (`PI_SOURCE_AGENT_DIR`) when set, else Pi's own default `$HOME/.pi/agent`.
/// These are the same two tiers Pi itself resolves, written down once rather
/// than once per file inside it. Used to default
/// [`ActuatorConfig::root_pi_agent_dir`].
#[must_use]
pub fn root_pi_agent_dir() -> PathBuf {
    match std::env::var("PI_SOURCE_AGENT_DIR") {
        Ok(home) if !home.trim().is_empty() => PathBuf::from(home.trim()),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".pi").join("agent"),
    }
}

/// Build the per-person launch catalog from the manifest and host config.
///
/// Path convention: ONE folder per person at `<dir>/.chief/agent/<personId>/`,
/// which is both their Pi agent dir and their cwd. The env carries the person's
/// static identity (organization, person, org dir, agent dir) — emitted as argv
/// env assignments by the operator client's `spawn_cmd::launch_command`.
///
/// # This stays here because the gate stays here
///
/// #751/P8 moved every pane decision to `chief-cli`, and this builder looks
/// like it should have gone with them. It must not.
/// [`resource_catalog::read_materialized_resources_for_launch`] is a
/// **fail-closed on-disk gate** over the daemon's own directory: the agent home
/// is there or it is not, and only the daemon can see that directory. The
/// answer is published (`POST /v1/org/runtime/launch-catalog`); the client
/// consumes it.
///
/// # Absence is named, never silent
///
/// A person with no agent home is not in [`LaunchCatalog::people`] — but they
/// ARE in [`LaunchCatalog::roster`], and [`LaunchCatalog::refusals`] carries
/// `explain_launch_refusal`'s re-derived cause for them. The client's
/// interpreter then refuses that person's start step by name with the real
/// reason ("this person has no agent home (<path>)") instead of an
/// interchangeable "no launch spec", and the next pass retries once the hire
/// path has created it.
///
/// # Identity is a launch fact, and it is passed IN
///
/// A person whose enrolled key disagrees with the one in their home cannot
/// authenticate, so starting them starts a process that exits seconds later —
/// for ever, once a second, because the next pass hands them the same spec.
/// [`identity_launch_refusals`](crate::identity_enrolment::identity_launch_refusals)
/// answers that question against the trust table, and its answer arrives here
/// as `identity_refusals`: plain data, computed by the caller that already holds
/// a company handle. This builder stays synchronous, stays pure, and keeps its
/// one job — deciding, from facts it is given, who may launch. Reaching into a
/// database from inside a documented pure read to go and FIND the fact would
/// have put a store round trip behind every entry of a walk that already touches
/// every person's home on disk, and made the sole gate in the system that must
/// never lie depend on an await it cannot see the cost of.
#[must_use]
pub fn build_launch_catalog(org: &OrganizationManifest, config: &ActuatorConfig) -> LaunchCatalog {
    build_launch_catalog_for_cycle(
        org,
        config,
        None,
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    )
}

/// Build the launch catalog with the same clean-session epoch used by the
/// runtime lifecycle's transcript selection, and with the identity facts the
/// caller read from the company's trust table.
///
/// `identity_refusals` is REQUIRED rather than optional on purpose: this is the
/// form the published `POST /v1/org/runtime/launch-catalog` answer is built
/// from, and a caller that holds a company handle must not be able to publish a
/// catalog that forgot to ask whether its people can authenticate. A caller with
/// no handle at all uses [`build_launch_catalog`] and gets the identity-blind
/// answer knowingly.
///
/// `people_with_pending_mail` is REQUIRED for the same reason, one fact over:
/// it is the whole of "does this person have assigned work", it decides which
/// fresh-session sentence the client sends, and only a caller holding a company
/// handle can read a mailbox. It is handed IN rather than looked up, because
/// the builder below is a documented pure read and must not grow a store round
/// trip per person. An empty set therefore means "nobody has mail", which is
/// the honest answer for the identity-blind caller that never asked.
#[must_use]
pub fn build_launch_catalog_for_session_epoch(
    org: &OrganizationManifest,
    config: &ActuatorConfig,
    session_epoch: Option<SystemTime>,
    identity_refusals: &std::collections::BTreeMap<String, String>,
    people_with_pending_mail: &std::collections::BTreeSet<String>,
) -> LaunchCatalog {
    build_launch_catalog_for_cycle(
        org,
        config,
        session_epoch,
        &std::collections::BTreeSet::new(),
        identity_refusals,
        people_with_pending_mail,
    )
}

fn build_launch_catalog_for_cycle(
    org: &OrganizationManifest,
    config: &ActuatorConfig,
    session_epoch: Option<SystemTime>,
    force_fresh_people: &std::collections::BTreeSet<String>,
    identity_refusals: &std::collections::BTreeMap<String, String>,
    people_with_pending_mail: &std::collections::BTreeSet<String>,
) -> LaunchCatalog {
    // The COMPANY DIRECTORY. `agent_home::agent_home` joins `.chief/agent/<id>`
    // onto it itself, so this builder never composes that path and cannot drift
    // from the writer — composing it twice is how the `<slug>` segment survived
    // in three places after it was deleted from a fourth.
    let dir = config.dir.clone();
    let mut catalog = LaunchCatalog::empty(org.slug.clone());
    // Allocated ONCE for the whole pass, not per person: the allocator walks
    // the entire order to guarantee uniqueness, so asking it per person would
    // re-walk the roster for every entry.
    let accent_order = crate::accent::identity_accent_order(&org.people);
    for person_id in &org.people_order {
        // A person id in the order the manifest's `people` map does not know is
        // a corrupt manifest, not a launch refusal: it is not iterated, so it
        // never reaches the roster and can never read as "the gate declined
        // them".
        let Some(person) = org.people.get(person_id) else { continue };
        catalog.roster.push(person_id.clone());
        catalog.inbox_counts.insert(person_id.clone(), 0);
        let entry = launch_entry(
            org,
            config,
            person_id,
            person,
            &dir,
            session_epoch,
            force_fresh_people.contains(person_id),
            crate::accent::organization_person_accent(
                &accent_order,
                org.chief_person_id().ok(),
                person_id,
            )
            .ok(),
            people_with_pending_mail.contains(person_id),
        );
        let is_chief = org.chief_person_id().is_ok_and(|chief| chief == person_id);
        let agent_home = crate::agent_home::agent_home(&dir, person_id);
        let model = if is_chief {
            let transcript =
                entry.as_ref().and_then(|entry| entry.session.as_deref()).map(std::path::Path::new);
            super::person_model::resolve_person_model(&super::person_model::PersonModelSources {
                transcript,
                sessions_root: &config.root_pi_agent_dir.join("sessions"),
                global_settings: &config.root_pi_agent_dir.join("settings.json"),
                global_root: &config.root_pi_agent_dir,
                project_settings: &dir.join(".pi/settings.json"),
                project_root: &dir,
            })
        } else if std::fs::metadata(&agent_home).is_ok_and(|metadata| metadata.is_dir()) {
            let transcript =
                entry.as_ref().and_then(|entry| entry.session.as_deref()).map(std::path::Path::new);
            super::person_model::resolve_person_model(&super::person_model::PersonModelSources {
                transcript,
                sessions_root: &agent_home.join("sessions"),
                // THE OPERATOR'S OWN FILE, for every person, and this is the
                // visible face of the ruling. Global settings used to be read
                // through a symlink inside each home; with the redirect gone
                // Pi resolves global scope to the operator's real agent dir,
                // so chief reads the same file Pi does. One consequence,
                // intended: the default model is SHARED — anybody's `/model`
                // moves the default every fresh session starts from.
                global_settings: &config.root_pi_agent_dir.join("settings.json"),
                global_root: &config.root_pi_agent_dir,
                project_settings: &agent_home.join(".pi/settings.json"),
                project_root: &agent_home,
            })
        } else {
            chiefd_core::runtime::launch_catalog::PersonModel::unavailable(None, None)
        };
        catalog.models.insert(person_id.clone(), model);
        // IDENTITY FIRST, and ahead of the on-disk gate, because it is the
        // refusal an operator can least infer from anywhere else. A person whose
        // home is perfect reads as launchable in every other signal the system
        // publishes; the only thing wrong with them is a fact that lives in the
        // trust table. Withholding them here is what turns "respawning once a
        // second, for ever" into one sentence naming the key to fix.
        if let Some(reason) = identity_refusals.get(person_id) {
            catalog.refusals.insert(person_id.clone(), reason.clone());
            continue;
        }
        let Some(entry) = entry else {
            // The daemon owns the disk, so the daemon re-derives WHY. The
            // client cannot see this data root and must never be asked to
            // guess; a gate that declined for a reason `explain_launch_refusal`
            // cannot name still produces an entry, because "declined, cause
            // unattributed" is a fact an operator needs and a silence is not.
            let reason = if org.chief_person_id().is_ok_and(|chief| chief == person_id) {
                format!(
                    "the Chief's company identity key is missing ({})",
                    crate::agent_home::chief_identity_key_path(&dir).display()
                )
            } else {
                super::resource_catalog::explain_launch_refusal(
                    &crate::agent_home::agent_home(&dir, person_id),
                    &config.root_pi_agent_dir,
                )
                .unwrap_or_else(|| {
                    "the agent home is present but the launch gate still declined".to_owned()
                })
            };
            catalog.refusals.insert(person_id.clone(), reason);
            continue;
        };
        catalog.people.insert(person_id.clone(), entry);
    }
    catalog
}

/// One person's entry in the `reconcile.people.withheld` line.
///
/// # An empty reason list is the loudest thing this line can say
///
/// It was printed as `execution-desk-ezra[]`, and empty brackets are not a
/// reason — they are the ABSENCE of one, rendered as though a reason had been
/// given. The single log line written to answer "why is this person not up"
/// answered nothing, on exactly the people an operator was staring at.
///
/// It is not a missing fact, either. `activity::reconcile` records DEMAND
/// reasons, so no reason at all is itself the finding: nothing in the company
/// asked for this person — no mail, no launch fence, no organization root — and
/// a person nobody asked for is correctly down. That sentence is worth reading;
/// two empty brackets are not.
///
/// # And it is only the finding once the decision has been told everything
///
/// That paragraph was right about the rendering and wrong about the inference,
/// because the decision is not shown all the demand: [`suppressed_demand`] is
/// the half the operational filter removes before `activity::reconcile` ever
/// runs. `suppressed` carries it back, and it JOINS the decision's own reasons
/// rather than replacing them — a person can owe a handoff and be holding
/// unrouted mail, and both are true.
fn withheld_note(
    person_id: &str,
    reasons: &[impl std::fmt::Debug],
    suppressed: &[&'static str],
) -> String {
    let mut terms: Vec<String> = reasons.iter().map(|reason| format!("{reason:?}")).collect();
    terms.extend(suppressed.iter().map(|term| (*term).to_owned()));
    if terms.is_empty() {
        return format!("{person_id}[nothing-demanded-them]");
    }
    format!("{person_id}[{}]", terms.join("+"))
}

/// `nothing-demanded-them` said about somebody the same pass is WARNing about
/// BECAUSE they have mail.
///
/// # The two statements, five seconds apart, about one person
///
/// ```text
/// WARN  mail demand NOT desired ... unmet=docs-jordan (not operational: benched,
///       departed, or its unit is paused)
/// INFO  reconcile.people.withheld ... docs-jordan[nothing-demanded-them]
/// ```
///
/// The WARN is right. The INFO is false, and it is false in the one field an
/// operator and the test suite read to tell a blocked person from an idle one:
/// `nothing-demanded-them` is what a healthy idle company prints constantly, so
/// a genuinely blocked person with unrouted mail was indistinguishable from
/// somebody nobody had asked for.
///
/// # Why the reason line could not see what the WARN sees
///
/// `mail_demand` and `maintenance_demand` are `observed_mail` and the
/// projection's maintenance ids with every NON-OPERATIONAL person filtered out
/// — deliberately, because a benched person must not be launched, and that
/// behaviour is correct. But the filtered sets are also all that reaches
/// `activity::reconcile` as requested demand, so the decision it returns for a
/// benched person carries no reason at all. "Nothing demanded them" was
/// therefore true of the FILTERED input and false of the world.
///
/// This restores the missing half: the RAW demand, before the operational
/// filter, so a person the filter removed carries the cause the pass already
/// detected instead of the default that means the opposite.
fn suppressed_demand(
    manifest: &OrganizationManifest,
    observed_mail: &std::collections::BTreeSet<String>,
    maintenance_person_ids: &[String],
) -> std::collections::BTreeMap<String, Vec<&'static str>> {
    let mut suppressed: std::collections::BTreeMap<String, Vec<&'static str>> =
        std::collections::BTreeMap::new();
    for person_id in observed_mail {
        if !activity::person_is_operational(manifest, person_id) {
            suppressed.entry(person_id.clone()).or_default().push(PENDING_MAIL_NOT_OPERATIONAL);
        }
    }
    for person_id in maintenance_person_ids {
        if manifest.people.contains_key(person_id)
            && !activity::person_is_operational(manifest, person_id)
        {
            suppressed.entry(person_id.clone()).or_default().push(MAINTENANCE_NOT_OPERATIONAL);
        }
    }
    suppressed
}

/// The demand that reached this person and was refused because they are not
/// operational — the WARN's `(not operational: benched, departed, or its unit
/// is paused)`, carried onto their own reason.
const PENDING_MAIL_NOT_OPERATIONAL: &str = "pending-mail-but-not-operational";

/// The same defect on the maintenance half of the demand: an open session
/// maintenance request for somebody the operational filter removed.
const MAINTENANCE_NOT_OPERATIONAL: &str = "maintenance-demand-but-not-operational";

/// Every withheld person's reason, for the `reconcile.people.withheld` line.
///
/// The decision's OWN reasons first, then whatever the operational filter took
/// away before the decision was made (see [`suppressed_demand`]). A person with
/// neither is genuinely unasked-for, and only then does the line say so.
fn withheld_notes(
    manifest: &OrganizationManifest,
    snapshot: &activity::ActivitySnapshot,
    observed_mail: &std::collections::BTreeSet<String>,
    maintenance_person_ids: &[String],
) -> Vec<String> {
    let suppressed = suppressed_demand(manifest, observed_mail, maintenance_person_ids);
    snapshot
        .people
        .iter()
        .filter(|(_, decision)| !decision.active)
        .map(|(person_id, decision)| {
            withheld_note(
                person_id,
                &decision.reasons,
                suppressed.get(person_id).map_or(&[][..], Vec::as_slice),
            )
        })
        .collect()
}

/// One person's catalog entry, or `None` when the on-disk gate declines them.
///
/// Split out of [`build_launch_catalog_for_cycle`] so the refusal half of that
/// loop reads as a decision rather than as the tail of a two-hundred-line
/// closure — and so `?` on the gate means exactly "declined" here, with the
/// caller owning what a decline costs.
#[allow(clippy::too_many_arguments)]
fn launch_entry(
    org: &OrganizationManifest,
    config: &ActuatorConfig,
    person_id: &str,
    person: &chiefd_core::store::organization::PersonRecord,
    dir: &std::path::Path,
    session_epoch: Option<SystemTime>,
    force_fresh: bool,
    accent: Option<String>,
    pending_mail: bool,
) -> Option<LaunchEntry> {
    let is_chief = org.chief_person_id().is_ok_and(|chief| chief == person_id);
    // ONE folder for an agent, and it is BOTH the agent dir and the cwd. The
    // Chief is the one exception: it is the operator's own Pi, with the company
    // directory as cwd and the operator's normal Pi agent directory unchanged.
    //
    // TOMBSTONE: `ORG_LAUNCHER_RELOAD_HARD_CONTRACT`, read here from
    // `.organization-reload-hard-contract.json` inside the pi-home. It was the
    // receipt a re-projection left behind so a running agent could tell whether
    // an in-process `/reload` would change its tool grant. Nothing re-projects,
    // so there is no reload to fence and no receipt to read.
    let agent_home = crate::agent_home::agent_home(dir, person_id);
    let (pi_home, workspace, resources) = if is_chief {
        if !crate::agent_home::chief_identity_key_path(dir).is_file() {
            return None;
        }
        // THE CHIEF RESUMES TOO, and its `session: None` was an oversight
        // rather than a ruling: hard-coded here by the commit that made the
        // Chief the operator's own Pi, with no decision recorded anywhere. The
        // transcripts existed and were simply never handed back, so the
        // operator's own front door started with a clean context on every boot
        // while every agent resumed.
        //
        // Scoped by the transcript header's own `cwd`, because this reads the
        // OPERATOR'S personal Pi directory — see `chief_launch_resources`.
        (
            config.root_pi_agent_dir.clone(),
            dir.to_path_buf(),
            super::resource_catalog::chief_launch_resources(
                person,
                &config.root_pi_agent_dir,
                dir,
                session_epoch,
                force_fresh,
            ),
        )
    } else {
        // THE AGENT GATE. Fail-closed, and the reason this whole builder is
        // daemon-side. `None` is a refusal the caller names.
        let resources = super::resource_catalog::read_materialized_resources_for_launch(
            person,
            &agent_home,
            &config.root_pi_agent_dir,
            session_epoch,
            force_fresh,
        )?;
        (agent_home.clone(), agent_home, resources)
    };
    let mut env = vec![
        EnvAssignment::new("ORG_LAUNCHER_ORGANIZATION", org.slug.clone()),
        EnvAssignment::new("ORG_LAUNCHER_PERSON", person_id),
        // THE KEY AND THE NAME ARE DIFFERENT FACTS, and the pane needs both.
        // `ORG_LAUNCHER_PERSON` is the kebab slug: it addresses document-store
        // and mailbox paths and must not change. This is the USERNAME, and it
        // exists because the footer was rendering `@` plus the slug — showing
        // the operator `@portfolio-management-head` where a person's handle
        // belongs. Display-only: nothing keys off it.
        EnvAssignment::new(
            "ORG_LAUNCHER_PERSON_NAME",
            crate::person_presentation::handle(&person.name),
        ),
        // AC6: `ORG_LAUNCHER_RUNTIME_SOCKET` and `ORG_LAUNCHER_RUNTIME_SESSION`
        // are NOT published here. They are a real pane-env contract with real
        // readers (`organization-intercom.ts` requires the pair, and refuses
        // to load with only one of them), but they state WHERE the pane is
        // drawn — and the operator client owns that entirely now. chiefd was
        // asserting a placement fact the client derives independently, which
        // agreed only because the client had handed chiefd its own socket at
        // daemon start and chiefd handed it straight back. The client injects
        // both at spawn from the socket and session it is actually driving
        // (`chief-cli/src/actuate/spawn_cmd.rs::launch_command`), so the
        // contract is unchanged and there is one producer instead of a round
        // trip through a process that cannot see a display.
        // The ONE pointer a pane gets to its company, and it is the company
        // DIRECTORY — not the `.chief` root beneath it. Every pane-side reader
        // joins onto this value (`chiefd-log` writes `<dir>/.chief/log`, the
        // rendezvous lives at `<dir>/.chief/run/daemon.json`), so naming the
        // `.chief` root here sent all of them one level too deep.
        //
        // `ORG_LAUNCHER_DATA_ROOT` used to ride alongside it carrying
        // `config.data_root()`, which is exactly this value plus `.chief`: the
        // same fact twice, and two variables for one directory is two things
        // to keep in step.
        EnvAssignment::new("ORG_LAUNCHER_ORG_DIR", config.dir.display().to_string()),
        EnvAssignment::new("HOME", config.home.display().to_string()),
        // Authoritative for the same reason, and load-required:
        // `organization-intercom.ts` throws at extension load when
        // ORG_LAUNCHER_ROOT is absent (`requiredEnvironment`), so a
        // pane launched without it exits before the plan's tagging
        // step can run (the live "no such pane" abort). Mirrors the
        // unconditional `ORG_LAUNCHER_ROOT=${launcherRoot}` in
        // `organizationPersonPiCommand` (`org-runtime.ts:400`).
        EnvAssignment::new("ORG_LAUNCHER_ROOT", config.launcher_root.display().to_string()),
        // The exact directory holding this pane's company identity key. It is
        // the agent home for agents and `<dir>/.chief` for the Chief, who has
        // no agent home. Pane code carries this path; it never guesses whether
        // the acting person is the root head.
        EnvAssignment::new(
            "ORG_LAUNCHER_IDENTITY_DIR",
            if is_chief { config.data_root() } else { pi_home.clone() }.display().to_string(),
        ),
    ];
    if !is_chief {
        // SESSIONS ONLY. chief used to set `PI_CODING_AGENT_DIR` to the home,
        // which made the home Pi's whole config scope and forced chief to
        // reconstruct the operator's configuration inside it — the three
        // inherited symlinks, and every hazard they carried. Pi already
        // inherits `~/.pi/agent` from the USER for any cwd, so that redirect
        // was chief compensating for a problem chief created. It is gone.
        //
        // Transcripts are the one thing that genuinely IS per person, so they
        // keep their own directory through Pi's own first-class env var. The
        // on-disk layout is unchanged, which is what keeps chiefd's
        // latest-transcript reader working untouched.
        env.push(EnvAssignment::new(
            "PI_CODING_AGENT_SESSION_DIR",
            pi_home.join("sessions").display().to_string(),
        ));
    }
    for key in BEST_EFFORT_FORWARDED_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            env.push(EnvAssignment::new(key, value));
        }
    }
    Some(LaunchEntry {
        pi_binary: config.pi_binary.display().to_string(),
        pi_home: pi_home.display().to_string(),
        // THE HOME IS THE CWD, and that is now the whole of how a person's
        // resources reach them: Pi reads project-scope skills and themes from
        // `<cwd>/.pi/{skills,themes}`, which is where chief installs the role
        // skill and the identity theme. Pointing the cwd at a separate
        // workspace would hand the agent an empty project scope and therefore
        // no role and no identity.
        workspace: workspace.display().to_string(),
        // THE HEADER IS THE ROLE, AND ONLY THE ROLE. The username used to lead
        // it, which put a second identity in front of every reader while the
        // footer showed a third. One person is enough identities per pane: the
        // footer carries who you are, the header carries what you do.
        display_name: crate::person_presentation::role(&person.name, &person.title, is_chief),
        person_name: crate::person_presentation::first_name(&person.name),
        accent,
        tools: resources.tools,
        extensions: crate::materialize::organization_extension_paths(&config.launcher_root)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
        // NO RESUME COPY IS PUBLISHED. chiefd used to author a sentence per
        // cause here, gated on there being a transcript to resume. Operator
        // ruling: "don't insert anything ever to anything. just boot the
        // agent." A relaunched pane is handed its session and nothing else.
        session: resources.session.map(|path| path.display().to_string()),
        // WHETHER ANYTHING IS ASKED OF THIS PERSON. Read by the caller from the
        // mailbox this daemon owns and handed in; the client cannot see a
        // mailbox and must not guess. An empty one is what turned a `Wake Up`
        // click into an unrequested department and an unrequested hire.
        pending_mail,
        env,
    })
}

/// What one activity-fence projection needs from the legacy `org_documents`
/// store: the launch-intent fence itself and the legacy mailbox's pending
/// recipients (person-to-person work mail the intercom/TS path writes, which
/// the native `mailbox` table never sees). Transition authority is already
/// entirely native in the activity ledger's own tables and is never projected
/// from a blob — a legacy document can never claim a transition was released.
#[derive(Debug, Clone)]
pub struct ActivityProjectionInput {
    /// The fence, from the legacy launch-intent row.
    pub fence: LaunchFence,
    /// SQL pending-mail facts, unioned with the native in-memory mailbox facts
    /// inside the projection commit. Each fact keeps its creation timestamp so
    /// a later commanded stop remains authoritative over older unread mail.
    pub pending_mail_facts: Vec<PendingMailFact>,
    /// People with queued, running, or applying session maintenance. An open
    /// request is work for its exact person and therefore grants the same
    /// per-person launch authority as newly arrived durable mail.
    pub maintenance_person_ids: Vec<String>,
}

// `ApplyTimeAuthority` and `ApplyTimeAuthorityProbe` are GONE (#751/P8-P10).
//
// They existed to close ONE window: `run_after_claim` gathered facts, planned,
// then drove the plan against the host, and between the gather and the apply an
// attended `company ceo` could change the launch-intent fence out from under
// it. The probe re-read that fence, on its own connection, immediately before
// the apply, and suppressed the apply when it had moved.
//
// There is no gather-to-apply window left to guard. chiefd applies nothing, and
// there is no stored action stream for a stale fence to leak into: the desired
// set is computed at READ time, on `GET /v1/org/runtime/desired`, from the
// fence as it stands at that moment. The fence is therefore re-read on every
// request that could act on it, which is what the probe was approximating with
// a second connection.

/// Run one full reconcile cycle for a company.
///
/// When `projection` is `Some`, the cycle first re-projects the activity
/// ledger through [`activity::reconcile`] under that fence (see
/// [`project_activity_fence`]) and plans from the fresh post-projection
/// snapshot. The live actuator always supplies one: a wired legacy fence is
/// authoritative, while an unwired legacy store uses the explicit native
/// reason-only sentinel. That pass recomputes CEO-only plus current native
/// demand instead of treating frozen `last_desired_active` rows as a restart
/// authorization. Every durable mutation goes through `db` (the fence
/// projection, the fenced sweep, the converge intent row, the safety
/// scaffold). Never returns a fatal error: any failure is a [`DutyError`] the
/// scheduler logs as one skipped pass.
///
/// # Errors
/// [`DutyError`] on an untrusted observation, an inconsistent projection, a
/// budget refusal path failure, or any writer error.
pub async fn reconcile_cycle(
    db: &CompanyDb,
    config: &ActuatorConfig,
    daemon_mode: ActuationMode,
    projection: Option<ActivityProjectionInput>,
) -> Result<ReconcileReport, DutyError> {
    reconcile_cycle_with_audit(db, config, daemon_mode, projection, &LastAudit::default()).await
}

/// The last audit body THIS PROCESS wrote, per company.
///
/// It exists to answer one question — "is there anything NEW to record this
/// pass?" — which the durable row cannot answer, because the row is opened and
/// CLOSED within a single pass (`converge_intent::close` deletes it), so a
/// read-back is always `None`. See the #367 gate in
/// [`reconcile_cycle_with_audit`].
///
/// PROCESS-LOCAL on purpose, and its failure mode is the harmless direction: a
/// restarted daemon has no memory and writes one redundant audit row on its
/// first pass per company. The alternative — making the row outlive its pass so
/// it could be compared — would change what `converge_intent::abort_open` finds
/// at startup, and that sweep's whole meaning is "a row that outlived a crash".
#[derive(Debug, Default)]
pub struct LastAudit(std::sync::Mutex<Option<ConvergeIntentBody>>);

impl LastAudit {
    /// Whether `body` differs from the last one this actuator RECORDED.
    ///
    /// A pure question. It deliberately does not adopt: the answer is used to
    /// decide whether to write, and a body adopted before its write could fail
    /// would make the NEXT pass answer "unchanged" for a change that was never
    /// recorded -- losing the audit row for the one pass an operator would go
    /// looking for, and losing it silently until the body happened to change
    /// again.
    fn differs(&self, body: &ConvergeIntentBody) -> bool {
        let last = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        last.as_ref() != Some(body)
    }

    /// Adopt `body` as the last recorded one. Called only after the write that
    /// carries it has COMMITTED.
    fn adopt(&self, body: ConvergeIntentBody) {
        let mut last = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *last = Some(body);
    }
}

async fn reconcile_cycle_with_audit(
    db: &CompanyDb,
    config: &ActuatorConfig,
    daemon_mode: ActuationMode,
    projection: Option<ActivityProjectionInput>,
    last_audit: &LastAudit,
) -> Result<ReconcileReport, DutyError> {
    // Adopt operator control-plane writes to the converge-safety document
    // (set-actuation-config / clear-breaker / bootstrap-store) before anything
    // this pass reads or rewrites it: the actor's snapshot never re-reads the
    // file on its own, and the cycle's own whole-document mutators below would
    // otherwise clobber an external write back within one pass (BUG-1,
    // runtime/takeover-bug-log.md). Once per pass is the adoption cadence.
    safety::refresh_safety_doc(db).await.map_err(duty)?;

    // Single-flight + floor: a second cycle while one is in flight, or one
    // inside the floor, is skipped without side effects.
    match safety::begin_cycle(db, config.floor).await.map_err(duty)? {
        CycleGate::Proceed => {}
        CycleGate::Skipped(reason) => {
            return Ok(ReconcileReport {
                applied: false,
                desired_people: 0,
                // A skipped pass looked at nothing and recorded nothing.
                changed: false,
                actuation_record: false,
                retry_after_floor: matches!(reason, SkipReason::FloorNotElapsed),
                notes: vec![format!("skipped: {reason:?}")],
            });
        }
    }

    // Everything after the claim is taken must release it, so the body is run
    // and its result carried past a best-effort `end_cycle`.
    let outcome = run_after_claim(db, config, daemon_mode, projection, last_audit).await;
    if let Err(error) = safety::end_cycle(db).await {
        // The claim self-reclaims after CLAIM_STALE_MS, so a failed release is
        // logged, never fatal.
        if let Ok(report) = outcome {
            let mut report = report;
            report.notes.push(format!("end_cycle failed (claim self-reclaims): {error}"));
            return Ok(report);
        }
    }
    outcome
}

async fn run_after_claim(
    db: &CompanyDb,
    config: &ActuatorConfig,
    daemon_mode: ActuationMode,
    projection: Option<ActivityProjectionInput>,
    last_audit: &LastAudit,
) -> Result<ReconcileReport, DutyError> {
    // #559 quiet lease, content-driven: the people the fence names who are NOT
    // yet desired-active in the committed activity snapshot were authorized
    // moments ago. Computed BEFORE the projection below commits them
    // desired-active (which is exactly what makes the lease one pass long, not
    // a wall-clock window — the row-store's launch-intent reconstruction
    // derives `updatedAt` from MAX(org_events.at), so any stamp-based lease
    // re-arms on every duty commit and quiets the drain forever, the measured
    // #618 regression).
    let mut newly_authorized = std::collections::BTreeSet::new();
    if let Some(input) = &projection {
        if let LaunchFence::Fenced(fenced_ids) = &input.fence {
            let pre = db.snapshot();
            let org = organization::read(pre.as_ref()).map_err(duty)?;
            let activity = activity::read(pre.as_ref(), &org).map_err(duty)?;
            let ceo = org.chief_person_id().map_err(|refusal| {
                DutyError::new(format!("manifest has no CEO person id: {refusal:?}"))
            })?;
            newly_authorized = fenced_ids
                .iter()
                .filter(|id| *id != ceo && org.people.contains_key(*id))
                .filter(|id| {
                    !activity.people.get(*id).is_some_and(|state| state.last_desired_active)
                })
                .cloned()
                .collect();
        }
    }
    // TOMBSTONE: the observed-runtime read.
    //
    // This pass used to read the actuator's committed report of what it saw in
    // tmux and thread it through everything below. It is gone with the whole
    // observation path, and the defect it caused is worth keeping written down:
    // the derived id set was handed to `project_activity_fence` as `Some(...)`
    // UNCONDITIONALLY, so an untrusted report ("I could not look") arrived as
    // `Some(EMPTY)` ("I looked, nobody is there"). The consumers used
    // `is_none_or`, so `None` retained and `Some(EMPTY)` withdrew. `Observation`
    // was an enum precisely so untrusted-with-a-roster is unrepresentable, and
    // the conflation reconstructed that state one function call later. A type
    // cannot defend a property that is discarded at the call site.
    // The activity-fence projection, committed before any planning: without it
    // the desired topology below is projected from whatever `last_desired_active`
    // the activity ledger froze at, and a person the CEO started through the
    // (legacy-written) launch-intent ledger plans as a KILL. With it, a staffed
    // and still-authorized person's live pane is adopted as-is, and a person
    // whose intent was withdrawn projects desired-inactive, so the plan shrinks
    // the fleet exactly as the fence requires.
    //
    // F8 (THE HARD RULE's shrink half): the projection is also the SETTLE
    // path — a routine idle park reaching its terminal state, or a fenced
    // person the fresh snapshot proves has no demand and no in-flight
    // handoff, is committed as a per-person launch-intent withdrawal INSIDE
    // the same transaction. The park/settle decision commits first as a
    // durable record; the pane kill below derives from it (desired-inactive
    // against the committed ledger), never the other way around.
    let fence_outcome = if let Some(projection) = projection {
        project_activity_fence(db, projection, config.watching_since.clone()).await?
    } else {
        FenceOutcome::default()
    };
    let FenceOutcome { withdrawn, mail_granted, mail_unmet, stand_down_held } = fence_outcome;
    // Read committed state for planning (fail-closed on a corrupt ledger). This
    // is a FRESH snapshot, taken after the fence projection's commit: planning
    // from the caller's pre-projection snapshot would actuate the stale
    // desired set the projection just corrected.
    let snapshot = db.snapshot();
    let org = organization::read(snapshot.as_ref()).map_err(duty)?;
    // Called for its refusal, not its value: a manifest with no structural-root
    // CEO is a corrupt roster, and this pass must fail closed on it rather than
    // plan a fleet around a hole. Nothing downstream needs the id any more —
    // the CEO's admission is an ordinary first ramp slot in
    // `plan_runtime_actions`, not a special case this function carries.
    org.chief_person_id()
        .map_err(|refusal| DutyError::new(format!("manifest has no CEO person id: {refusal:?}")))?;
    let activity = activity::read(snapshot.as_ref(), &org).map_err(duty)?;

    // Removal as a first-class fact (arch-audit H2, Step 7a): read the
    // committed `runtime` row THIS company's actor holds — the row the
    // launcher's explicit company-stop commits with `status: "stopped"`
    // (org-runtime.ts's `stopOrganizationRuntimeUnlocked`; no other writer
    // ever commits that status). The publish that carries it wakes this duty
    // (wake-by-default on reconcile inputs), so the stop converges
    // reactively. Positive evidence only: `None` (never written / cleared)
    // or any live status plans exactly as before.
    let runtime = db.runtime_read().await.map_err(duty)?.map(|(runtime, _seq)| runtime);
    let company_stopped = runtime.as_ref().is_some_and(|runtime| runtime.status == "stopped");

    // #751/P8 — THIS IS WHERE chiefd STOPPED ACTUATING.
    //
    // Everything that used to sit between here and the report was the actuation
    // machine: observe the runtime, diff it into an ordered `ConvergePlan` of
    // pane steps, trim that plan against the destructive and start budgets,
    // then drive every step against a host executor. All of it is the operator
    // client's job now, and none of it can be done from a process that cannot
    // see a pane.
    //
    // What chiefd keeps is what only chiefd can know: the DESIRED set — already
    // committed by the activity-fence projection above — and the SAFETY POLICY
    // that rides on it (mode, breaker, ramp, budgets). It publishes both. It
    // applies neither.
    //
    // The action stream is deliberately NOT computed and stored here.
    // `plan_runtime_actions` runs at READ time, on `POST /v1/org/runtime/observed`
    // and `POST /v1/org/runtime/actions`, against the observation the calling
    // client committed in that same request. Storing a copy here would be a
    // second answer to "what should happen right now" — durable, going stale
    // between passes, and racing the live one. That is the second-source-of-truth
    // shape this whole workstream exists to delete, and it is the shape the
    // persisted head-in-parent column had. The plan is computed once below for
    // exactly one purpose, the audit / shadow-diff intent row, and that copy is
    // never served to anybody.
    let _ = &newly_authorized;
    // The #29 pointer sweep is computed from the activity ledger alone — it
    // never looked at a pane, so nothing about it changes when the display
    // leaves. It used to ride along inside `CyclePlan`; with the topology
    // planner gone it is computed directly here.
    let sweep_actions = compute_pointer_sweep(&project_sweep_input(&activity));
    let roster = project_desired_roster(&org, Some(&activity));

    // Effective apply = the conservative AND of the daemon's request and the
    // company's durable config (a tripped breaker forces the config to shadow).
    // Two `ActuationMode` enums exist and they are NOT the same type:
    // `duty_hooks`' is the mode the DAEMON requests for this tick, and
    // `converge_safety`'s is the mode the COMPANY durably configured. They are
    // compared here, one against its own `Apply`, precisely because the
    // effective mode is the conservative AND of two independent decisions —
    // matching them against a single enum would be the collapse that lets one
    // silently answer for the other.
    let (safety_config, breaker_tripped) = safety::read_safety_posture(db);
    let effective_apply = matches!(daemon_mode, ActuationMode::Apply)
        && matches!(safety_config.actuation_mode, safety::ActuationMode::Apply);
    let mut notes = Vec::new();

    // A company the operator explicitly stopped desires nobody, which the
    // action planner already renders as a single `StopAll` rather than a list
    // of per-person stops that would race the client's own teardown.
    if company_stopped {
        notes.push("company runtime is stopped: chiefd desires nobody".to_owned());
    }

    let desired = publish_desired_runtime(
        &roster,
        safety_config.actuation_mode,
        breaker_tripped,
        // THE OPERATOR'S EXPLICIT STOP, and it empties the set rather than
        // holding it. A hold leaves the company running; a stop means chiefd
        // desires nobody, and absence is what takes them down.
        company_stopped,
        // The daemon's own converge pass does not have the launcher checkout in
        // hand, and the hash's extension digest must come from ONE place. The
        // read route derives it; this pass publishes the desired SET only, and
        // reports how many people are in it.
        |_person_id| String::new(),
    );
    let desired_people = desired.people.len();

    // The withheld reason is an operator-facing STATE, never an error and never
    // a silence. Naming it here is what makes "nobody is actuating" legible in
    // the daemon log as well as on the board — the failure mode this packet is
    // most likely to produce in the field is a company that looks idle when in
    // fact no client is attached to it.
    if let Some(reason) = desired.hold {
        notes.push(format!("actuation held: {reason:?}"));
    }
    // #107: name the people this pass wants running. A repeated id is the
    // finding, so the list does not dedupe.
    if let Some(subjects) = launch_subjects_note(&desired.people) {
        notes.push(subjects);
    }

    // TOMBSTONE: the startup-admission window write. It stamped
    // `runtime.startup_admission_until` from the plan's `admission_ms` so a
    // client could pace its launches. The ramp is deleted -- everything missing
    // boots at once -- so there is no window to publish.

    // #367 idle→zero: a converged pass must be near-writeless.
    //
    // THE GATE MOVED WITH THE MEANING OF THE COUNT. It used to be
    // `planned_steps > 0` — how many ACTIONS this pass emitted — and a
    // converged company emitted none, so an idle pass wrote nothing. Under a
    // desired SET, every live company has people in it on every pass, so that
    // same expression is now permanently TRUE and would commit two mutations
    // per pass, for ever, on every company in the fleet. The rename made the
    // reader visit; this is the re-judgement it needed.
    //
    // The honest question is not "is there anything in the set" but "is there
    // anything NEW to record", and the audit row answers it itself: its body is
    // derived entirely from the desired set and the two safety flags, so a body
    // identical to the committed one is a write that changes nothing. That is
    // the rule #1042 states for the tool surface, applied to a duty.
    let will_sweep = safety_config.sweep_live && !sweep_actions.is_empty();
    let intent = intent_body(&desired, !effective_apply, safety_config.sweep_live);
    let has_work = last_audit.differs(&intent) || will_sweep;

    if has_work {
        // ADOPT ONLY AFTER THE COMMIT. `write_action_intent` propagates its
        // error with `?`, so a failed write leaves the remembered body
        // untouched and the next pass still sees a change to record.
        write_action_intent(db, &roster.company.slug, intent.clone()).await?;
        last_audit.adopt(intent);
    }

    // The #29 pointer sweep, which stays: it is a fenced compare-and-clear over
    // SQL rows, touches no process and no display, and is exactly the kind of
    // convergence a backend should still be doing.
    if will_sweep {
        crate::pause::at("converge.before_sweep");
        let cleared = apply_pointer_sweep(db, sweep_actions.clone()).await?;
        notes.push(format!("swept {cleared} dangling pointer(s)"));
    }

    // F8: a committed withdrawal is a settle decision the whole system must
    // observe promptly — name it (the operator's answer to "why did that agent
    // go away") and ask the scheduler for one prompt follow-up pass.
    if !withdrawn.is_empty() {
        // EVERY WITHDRAWAL NAMES ITS OWN REASON. This printed `(settled)` for
        // all three branches, so a fence dropped for a reason that is NOT a
        // settle — a person the operational filter had already excluded, or one
        // this pass found with no demand at all — was reported as though their
        // agent had finished its work and parked. On `taperoom-inc` that is how
        // a wake could be granted and deleted 562ms later with the operator's
        // only visible explanation being a word that did not apply.
        let mut by_reason: std::collections::BTreeMap<&'static str, Vec<String>> =
            std::collections::BTreeMap::new();
        for (person_id, reason) in &withdrawn {
            by_reason.entry(reason).or_default().push(person_id.clone());
        }
        for (reason, people) in by_reason {
            notes.push(format!("launch intent withdrawn ({reason}): {}", people.join(", ")));
        }
    }

    // THE WAKE, NAMED. A message to a settled person is the whole mechanism
    // that brings them back, and until now it left no trace at all: the only
    // record was the person quietly reappearing in `launching:` a pass later,
    // or — when it failed — nothing whatsoever.
    if !mail_granted.is_empty() {
        notes.push(format!("mail wake granted launch intent: {}", mail_granted.join(", ")));
    }

    // THE SILENT STATE, ENDED (operator ruling, 2026-08-13). `applied: 0` on
    // every round while the head of a department is not running is exactly what
    // a live investigation had to work from, and it says nothing. A pass that
    // holds a genuine durable envelope for somebody it does NOT desire is a
    // company that has stopped working, so it says so by name and with the
    // reason, at `warn` as well as in the operator-facing notes.
    // THE OPERATOR'S OWN DECISION, NAMED EVERY PASS. A stand-down that produced
    // silence would be indistinguishable from a company that had simply stopped
    // reconciling — and the whole defect this closes is an operator unable to
    // tell what their company is doing. It names WHO is waiting so the operator
    // can weigh resuming, and it says the mail is held rather than lost,
    // because "stopped" and "dropped your messages" are very different
    // promises.
    if !stand_down_held.is_empty() {
        notes.push(format!(
            "stand-down holds pending mail for: {} (held, not lost — `chief resume` delivers it)",
            stand_down_held.join(", ")
        ));
    }

    if !mail_unmet.is_empty() {
        let detail = mail_unmet
            .iter()
            .map(|(person_id, reason)| format!("{person_id} ({reason})"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            company = %roster.company.slug,
            unmet = %detail,
            "chiefd converge: mail demand NOT desired — these people have pending mail and are \
             not being launched"
        );
        notes.push(format!("mail demand NOT desired: {detail}"));
    }

    if has_work {
        close_action_intent(db, &roster.company.slug, &desired).await?;
    }

    // TOMBSTONE: the `runtime` row's observation publish.
    //
    // This wrote what the ACTUATOR had reported -- `process_handles` and a
    // `status` of running/idle -- into the durable
    // runtime row on every pass. Every field of it was a host fact, so the
    // whole publish goes.
    //
    // Worth keeping from its history: `status` was once published as `empty`,
    // which the CHECK constraint does not admit, so the publish failed, the
    // pass returned `Err`, and chiefd called its own store corrupt on every
    // pass for any company whose actuator truthfully reported nothing alive --
    // the ordinary cold boot. It hid because no test had ever committed an
    // actuation record, so the trusted branch was never reached. A field that
    // only executes when a host reports a particular shape is a field whose
    // tests prove very little.

    Ok(ReconcileReport {
        applied: effective_apply,
        desired_people,
        changed: has_work,
        // THE LAUNCH DECISION IS THE RECORD, and it is not the same question as
        // `has_work`. Each of these three is a decision about whether somebody
        // runs, and each can leave the desired set unchanged: a grant for
        // somebody already desired, a withdrawal the projection had already
        // settled, a demand refused every pass. See `ReconcileReport`.
        actuation_record: !mail_granted.is_empty()
            || !withdrawn.is_empty()
            || !mail_unmet.is_empty()
            || !stand_down_held.is_empty(),
        retry_after_floor: !withdrawn.is_empty(),
        notes,
    })
}

/// Re-project the activity ledger through [`activity::reconcile`] under the
/// launch-intent fence, in one committed [`MutationClass::Reconcile`]
/// transaction — the same mutation class and commit convention the duty's own
/// `supervision::cycle` commit uses, so the corrected desired set is durable
/// before the cycle plans from it.
///
/// Returns the people whose launch intent this pass WITHDREW (the settle
/// path's shrink half, F8): everyone the fence named whose routine idle park
/// reached a terminal state (`applied`/`forced`) this pass, or whom the fresh
/// snapshot proves has no demand and no in-flight handoff (the #12 stale
/// sweep, ported from `staleLaunchIntentPersonIds`). The withdrawal commits
/// INSIDE this same transaction — the park/settle decision is the durable
/// record, written first; the pane kill derives from it when the cycle plans
/// from the committed post-projection snapshot. An empty vec means the fence
/// was left untouched (the common case; a converged company stays writeless).
///
/// `requested_person_ids` carries two durable sources of explicit demand:
/// every person named by the launch-intent fence **who is not already
/// desired-active**, plus the *current*
/// pending-mailbox demand — the union of timestamped REAL pending facts from
/// the NATIVE `mailbox` table this pass (launcher cadence
/// re-emissions excluded, #551; acknowledgement-covered assignment-delivery
/// orphans excluded, #638) and the SQL pending facts the actuator read. Facts
/// created at or before the recipient's latest commanded stop are durable
/// history, not demand. The remaining union is ALSO the grant set, and it has to be: the
/// in-memory ledger is hydrated once at open and `/v1/org/mailbox/delta` — the
/// writer every intercom message goes through — never touches it, so a grant
/// computed from the ledger half alone authorized nobody for any real message
/// and a settled person could never be woken. The launch intent is not merely a
/// permission: it is the operator's durable decision to START that exact
/// person. But a start decision is not a lifetime residency permit (F8): once
/// the person IS desired-active, the fence stops contributing demand and the
/// ordinary idle machinery (sixty-second quiet lease → routine park →
/// withdrawal, exactly the TypeScript `reconcileOrganizationActivity` settle
/// semantics, where launch intent is a gate and never a `requested` reason)
/// governs whether they stay. Without that lapse the fence pinned its people
/// forever — mailbox demand drained, the lease never started, no park ever
/// fired, and THE HARD RULE's shrink half was unimplementable from the
/// daemon. Ids the manifest no longer knows are
/// dropped rather than refused (a stale envelope or departed intent must not
/// fail the pass closed). The fence gates every demand reason last, so no
/// demand can open a person the operator has not authorized.
/// What one fence projection DID, in the vocabulary an operator investigating a
/// company that stopped working actually needs.
///
/// It used to be a bare `Vec<String>` of withdrawals, which answered only "why
/// did that agent go away". The two questions the live carlos incident could not
/// answer are the other two: who did an arriving message authorize, and — the
/// one that matters — is there anybody holding a genuine durable envelope whom
/// this pass is nonetheless not going to launch.
#[derive(Debug, Default)]
struct FenceOutcome {
    /// People whose launch intent this pass withdrew (the settle path's shrink
    /// half, F8).
    /// Each withdrawn person and the reason this pass dropped their fence.
    withdrawn: Vec<(String, &'static str)>,
    /// People an arriving durable envelope authorized this pass — the wake.
    mail_granted: Vec<String>,
    /// People with REAL pending mail who are still not desired after the
    /// projection, each with the reason, in person order.
    mail_unmet: Vec<(String, String)>,
    /// People whose demand this pass is HOLDING because the operator stood the
    /// company down. Their mail is untouched and still pending; nothing about
    /// them is lost, and nothing about them runs.
    stand_down_held: Vec<String>,
}

async fn project_activity_fence(
    db: &CompanyDb,
    projection: ActivityProjectionInput,
    watching_since: String,
) -> Result<FenceOutcome, DutyError> {
    // `goal-delivery-quiesce` is normalized-row authoritative. Reading the
    // retired document body here made every SQL-only CEO reset invisible, so
    // pre-reset mailbox rows were reclassified as fresh work and re-granted
    // launch intent on the bounded retry. Read the typed actor row immediately
    // before the projection commit instead.
    let quiesced_ms =
        db.goal_delivery_quiesce_read().await.map_err(duty)?.and_then(|(quiesce, _seq)| {
            chiefd_core::isotime::parse_iso_millis(&quiesce.quiesced_at)
        });
    // STOP MEANS STOP. Read the operator's stand-down immediately before the
    // projection commits, exactly like the quiesce watermark above it and for
    // the same reason: a decision this pass acts on must be the CURRENT one.
    //
    // This is the seam the incident ran through. Six people were parked on an
    // explicit operator instruction, and the mail they had queued to each other
    // re-granted every one of them forty-five seconds later, because the only
    // question this pass asked was per-person. See `store::stand_down`.
    let stood_down = db.stand_down_read().await.map_err(duty)?.is_some();
    // THE COMMITTED FENCE, READ FRESH, for the same reason as the two reads
    // above it: `launch_intent::add` unions onto this and commits the WHOLE
    // document, and persist-dispatch deletes every row the document omits. The
    // actor's in-memory copy is hydrated once at `CompanyDb::open` and never
    // sees a row-level grant — `wake_person`'s among them — so unioning onto it
    // withdrew the operator's own wake 2.165 seconds after they clicked it, with
    // no note. See `launch_intent::add`.
    let committed_fence = chiefd_core::store::launch_intent::LaunchIntent::Fenced(
        db.launch_intent_read()
            .await
            .map_err(duty)?
            .map(|(intent, _seq)| intent.person_ids.into_iter().collect())
            .unwrap_or_default(),
    );
    db.mutate(
        MutationClass::Reconcile,
        MutationName("reconcile_cycle.activity_fence"),
        move |ledgers| {
            let now_ms = ledgers.now().0;
            let manifest = organization::read(ledgers)?;
            let supervision = supervision::read(ledgers, &manifest)?;
            let ctx = organization::company_context(&manifest)?;
            let pre_activity = activity::read(ledgers, &manifest)?;
            // A commanded stop is the latest explicit operator decision for
            // this person. Keep one per-person watermark from the complete
            // transition history. Automatic idle parks have no intent owner,
            // and other lifecycle actions are not stop commands.
            let commanded_stop_watermarks = pre_activity
                .transitions
                .values()
                .filter(|transition| transition.action == activity::TransitionAction::Park)
                .filter(|transition| transition.intent_id.is_some())
                .filter_map(|transition| {
                    chiefd_core::isotime::parse_iso_millis(&transition.requested_at)
                        .map(|at| (transition.person_id.clone(), at))
                })
                .fold(
                    std::collections::BTreeMap::<String, i64>::new(),
                    |mut watermarks, (person_id, at)| {
                        watermarks
                            .entry(person_id)
                            .and_modify(|watermark| *watermark = (*watermark).max(at))
                            .or_insert(at);
                        watermarks
                    },
                );
            // #363: the CEO-only reset watermark. Pre-reset durable mail stays
            // readable forever but no longer counts as a decision to run anyone;
            // a missing/malformed watermark means none is in force (the TS
            // `organizationGoalDeliveryQuiescedSince` polarity, deliberately).
            // #551: REAL pending-mail demand only — a launcher cadence re-emission
            // (check-in / people-check / goal-watch) must not read as `Requested`
            // or the daemon re-pins a settling person from its own unread cadence
            // mail on every pass. Parity with the TypeScript shrink boundary
            // (`peopleWithPendingMailboxWork`).
            //
            // TOMBSTONE, and its removal is the whole of a live defect: this
            // set used to be the UNION of the in-memory `ledgers.mailbox_rows()`
            // and the SQL facts below. The in-memory half is DELETED.
            //
            // `Ledgers` is hydrated from SQLite exactly once, in
            // `CompanyDb::open`, and from then on its mailbox map only ever
            // GAINS rows. `mailbox::enqueue` writes a `pending` row for chiefd's
            // own mail — reminders, health incidents, escalations — and nothing
            // ever moves that row on: a pane drains its mailbox through
            // `/v1/org/mailbox/delta`, which writes the `mailbox` table straight
            // on the transaction and says so in its own words at
            // `CompanyDb::mailbox_delta` ("Bypassing the Ledgers snapshot").
            // `mailbox::archive`, the one accessor that could have retired an
            // in-memory row, had no callers at all.
            //
            // So every envelope chiefd delivered since this process started is
            // `accepted` in SQL and `pending` in memory, for ever. Read as
            // demand it is an `ActivityReason::Requested` nobody can clear, and
            // `Requested` is effective demand: `activity::reconcile` recomputes
            // `idle_since` as NULL on every pass, the quiet lease can never
            // expire, `settled_idle_stop_lease_expired` is never true, and the
            // person is never a park candidate. They stay up — green, connected
            // and doing nothing — until the daemon restarts.
            //
            // MEASURED on the operator's own company, 2026-08-20: thirteen
            // people desired-active, `agent_quiet_at` twenty minutes old,
            // `idle_since` NULL, against ZERO pending rows in SQL. The daemon
            // started at 20:18:41; every person whose reminder fired after that
            // instant was pinned, and `intel-lead`, whose last reminder fired at
            // 20:05:07 — before it — was the only one of them that settled
            // normally. Replaying that exact database through a freshly
            // hydrated ledger stamped `idle_since` for all of them and parked
            // two on the first pass.
            //
            // Nothing is lost with the in-memory half. An enqueue commits its
            // row in the same transaction that wrote the ledger, so SQL sees
            // every envelope memory ever saw — and, unlike memory, it also sees
            // the drain. Both halves already applied the same three filters
            // (pending, not a #551 launcher re-emission, newer than the #363
            // quiesce watermark), so removing one narrows the set only by the
            // rows that were never true.
            let observed_mail: std::collections::BTreeSet<String> = projection
                .pending_mail_facts
                .iter()
                .filter(|fact| manifest.people.contains_key(&fact.person_id))
                .filter(|fact| {
                    let created = chiefd_core::isotime::parse_iso_millis(&fact.created_at);
                    quiesced_ms.is_none_or(|since| created.is_some_and(|at| at > since))
                        && commanded_stop_watermarks
                            .get(&fact.person_id)
                            .is_none_or(|stopped_at| created.is_some_and(|at| at > *stopped_at))
                })
                .map(|fact| fact.person_id.clone())
                .collect();
            let mail_demand: std::collections::BTreeSet<String> = observed_mail
                .iter()
                .filter(|person_id| activity::person_is_operational(&manifest, person_id))
                .cloned()
                .collect();
            let maintenance_demand: std::collections::BTreeSet<String> = projection
                .maintenance_person_ids
                .iter()
                .filter(|person_id| activity::person_is_operational(&manifest, person_id))
                .cloned()
                .collect();
            let mut fence = projection.fence.clone();
            let mut mail_granted: Vec<String> = Vec::new();
            let launch_demand: std::collections::BTreeSet<String> =
                mail_demand.union(&maintenance_demand).cloned().collect();
            // THE HELD DEMAND, NAMED. A stand-down does not touch a mailbox
            // row: the mail stays `Pending` and is delivered the moment the
            // operator resumes. But an operator looking at a stopped company
            // must be able to see WHAT is waiting, or a stand-down is a silence
            // they cannot read — so the pass names everybody whose demand it is
            // holding, every pass, in the same notes that name a wake.
            let held: Vec<String> =
                if stood_down { launch_demand.iter().cloned().collect() } else { Vec::new() };
            if !stood_down && !launch_demand.is_empty() && matches!(fence, LaunchFence::Fenced(_)) {
                let granted = chiefd_core::store::launch_intent::add(
                    ledgers,
                    &ctx,
                    &committed_fence,
                    launch_demand.iter().cloned(),
                )?;
                // UNION, never a replacement: the durable grant merges into the
                // fence the projection already carried — swapping it in wholesale
                // would silently de-authorize every person the grant did not name.
                if let LaunchFence::Fenced(person_ids) = &mut fence {
                    let admitted: Vec<String> = granted
                        .person_ids()
                        .iter()
                        .filter(|id| manifest.people.contains_key(*id))
                        .cloned()
                        .collect();
                    // The GRANT is the wake, and the wake is what the operator
                    // needs to be able to grep for. Only the ids this pass's
                    // mail actually authorized are named — the durable row also
                    // carries every earlier grant, and reporting those as a
                    // fresh wake every pass would make the note worthless.
                    mail_granted = admitted
                        .iter()
                        .filter(|id| mail_demand.contains(*id) && !person_ids.contains(*id))
                        .cloned()
                        .collect();
                    person_ids.extend(admitted);
                }
            }
            // A carried fence can outlive the organization fact that first
            // authorized it. Current operational truth wins before the fence
            // gates structural demand, or a retained departed person with an
            // old fence is allowed to mint a brand-new offboard transition on
            // every supervision pass (the operator's deleted-department storm).
            //
            // The one exception is an ATTENDED offboard already in progress.
            // That person is departed by design, but their existing offboard
            // transition still needs its already-held fence to finish the
            // handoff. Presence of that active offboard pointer is the lease;
            // this code never mints one to create the exception.
            let stale_carried: std::collections::BTreeSet<String> = match &fence {
                LaunchFence::Fenced(person_ids) => person_ids
                    .iter()
                    .filter(|person_id| {
                        !activity::person_is_operational(&manifest, person_id)
                            && !pre_activity.active_transition(person_id).is_some_and(
                                |transition| {
                                    transition.action == activity::TransitionAction::Offboard
                                },
                            )
                    })
                    .cloned()
                    .collect(),
                LaunchFence::Unfenced => std::collections::BTreeSet::new(),
            };
            let mut withdrawn: Vec<(String, &'static str)> = Vec::new();
            if !stale_carried.is_empty() {
                if let LaunchFence::Fenced(person_ids) = &fence {
                    let current =
                        chiefd_core::store::launch_intent::LaunchIntent::Fenced(person_ids.clone());
                    chiefd_core::store::launch_intent::remove(
                        ledgers,
                        &ctx,
                        &current,
                        stale_carried.iter().cloned(),
                    )?;
                }
                if let LaunchFence::Fenced(person_ids) = &mut fence {
                    person_ids.retain(|person_id| !stale_carried.contains(person_id));
                }
                withdrawn.extend(
                    stale_carried.into_iter().map(|person_id| (person_id, "not-operational")),
                );
            }
            let mut requested_person_ids = launch_demand;
            // A normalized launch intent is the durable, explicit start decision
            // itself — not only an allow-list for unrelated mailbox demand.  Carry
            // those exact Fenced ids into activity as Requested demand so a fresh
            // zero-pane company can create only the people the operator named —
            // but ONLY while the person is not already desired-active (see the
            // fn-level docs: a start decision lapses once the person is up, or no
            // fenced person could ever settle, park, and be withdrawn).
            // `Unfenced` deliberately contributes no names: no caller may turn a
            // permissive fence into an eager fleet start.
            if let LaunchFence::Fenced(person_ids) = &fence {
                requested_person_ids.extend(
                    person_ids
                        .iter()
                        // A carried start fence is not permanent demand. Only a
                        // currently operational person can turn it into a new
                        // Requested reason. Do NOT narrow `fence` here: an
                        // attended offboard keeps its existing authorization
                        // until its HandoffRequired transition becomes
                        // terminal, then the withdrawal half below removes it.
                        .filter(|person_id| activity::person_is_operational(&manifest, person_id))
                        // A START DECISION LAPSES WHEN THE PERSON ANSWERS, NOT
                        // WHEN CHIEFD DECIDES THEY SHOULD BE UP.
                        //
                        // This read `!last_desired_active` alone, and that flag
                        // is chiefd's own decision from the previous pass — it
                        // says nothing about whether a pane exists. So the
                        // grant an operator paid for lapsed one pass after it
                        // was made, while their Pi was still booting, and the
                        // shrink half below then withdrew the row as a fence
                        // with no demand behind it.
                        //
                        // Measured on `taperoom-inc`, 2026-08-19, four clicks in
                        // a row: `org_events` carries `launch-intent dev upsert`
                        // at 23:24:21.570 and `launch-intent dev delete` at
                        // 23:24:22.132 — the grant survived 562ms. dev's Pi went
                        // on starting and reported `interactive-loop-ready` at
                        // 23:25:02, into a company that had stopped wanting him
                        // thirty-four seconds earlier, so the pane was reaped on
                        // arrival. The operator clicked four times and saw
                        // nothing happen four times.
                        //
                        // The agent's own report is the honest lapse signal: a
                        // person who has said ANYTHING (`agent_active_at`, or
                        // `agent_quiet_at` from a settle) has a pane that
                        // answered, and from there the ordinary settle path owns
                        // them — the sibling rule in `activity.rs` holds them up
                        // on `MaintenanceBackpressure` until their quiet lease
                        // expires. A person who has said NOTHING is still
                        // starting, and their grant stays demand.
                        //
                        // This cannot pin a person for ever: the actuator's own
                        // crash-loop counter names anybody who will not stay up
                        // (`'ivo' has failed to stay up N times in a row`), and
                        // the grant is still withdrawn the moment they answer
                        // and settle.
                        .filter(|person_id| {
                            fence_still_supplies_demand(pre_activity.people.get(*person_id), now_ms)
                        })
                        // #638: a fenced person whose routine idle park is already
                        // TERMINAL (`applied`/`forced`) is settled — a durable,
                        // committed fact this pass's withdrawal half is about to
                        // de-authorize. Their lapsed start decision must NOT read
                        // as fresh `Requested` demand: an external settle (the TS
                        // reconciler committing the park while the fence still
                        // names them) would otherwise be undone by this very pass,
                        // which re-pins the worker desired-active and respawns the
                        // pane it just withdrew — a blocked worker (the operator's #29
                        // ruling: blocked = settle) never reaches CEO-only.
                        .filter(|person_id| {
                            !pre_activity.active_transition(person_id).is_some_and(|transition| {
                                activity::is_routine_idle_park(transition)
                                    && matches!(
                                        transition.status,
                                        activity::TransitionStatus::Applied
                                            | activity::TransitionStatus::Forced
                                    )
                            })
                        })
                        .cloned(),
                );
            }
            let requested_person_ids = requested_person_ids.into_iter().collect();
            let snapshot = activity::reconcile(
                ledgers,
                &manifest,
                &supervision,
                &ReconcileInput {
                    launch_intent: fence.clone(),
                    requested_person_ids,
                    watching_since: watching_since.clone(),
                },
            )?;
            // WHY EACH PERSON IS OR IS NOT UP, EVERY PASS THAT DECIDES IT.
            //
            // Operator ruling, 2026-08-14: "add some logging too, because you
            // keep f-ing it up." They are right, and this is the line that was
            // missing. A wake writes a launch-intent fence row and the operator
            // then watches an unchanged sidebar; every question that follows —
            // did chiefd see the fence, did it authorize the person, is the
            // delay in the decision or in the delivery of it — is answered
            // HERE, and nowhere else held both halves.
            //
            // The decision's OWN reasons, never a re-derivation: `Requested`
            // beside `active = false` can only be the fence gating last, and
            // its absence means the person is not operational at all. A pass
            // that authorizes everybody says nothing, so a steady company stays
            // silent.
            //
            // The RAW demand sets, never the filtered ones. `mail_demand` and
            // `maintenance_demand` have already had every non-operational
            // person removed, and reading the reason off those is exactly how
            // this line came to say `nothing-demanded-them` about a person the
            // same pass WARNs about because he has mail. See `withheld_notes`.
            {
                let held: Vec<String> = withheld_notes(
                    &manifest,
                    &snapshot,
                    &observed_mail,
                    &projection.maintenance_person_ids,
                );
                if !held.is_empty() {
                    tracing::info!(
                        event = "reconcile.people.withheld",
                        withheld = %held.join(" "),
                        // `authorized`, not `active`. It counts the people this
                        // pass DECIDED may run — chiefd's own answer to its own
                        // question — and it was rendered as `active=`, a word
                        // that reads as a headcount of running people. On
                        // 2026-08-18 it read `active=5` every five seconds for
                        // forty minutes while the tmux server holding those
                        // five was gone. chiefd cannot count running people and
                        // has not been able to since #751/P8-P10 deleted the
                        // actuator's reports; the field now says what it is.
                        // Whether anybody is converging the authorization at
                        // all is the separate `runtime_unattended` fact, and it
                        // is separate because they are different questions.
                        authorized = snapshot.people.values().filter(|d| d.active).count(),
                        "this pass decided somebody should NOT be up, and these are the \
                         decision's own reasons for each of them; \
                         `nothing-demanded-them` means no mail, no launch fence and no \
                         organization root asked for that person at all, while \
                         `pending-mail-but-not-operational` means the opposite — mail is \
                         waiting for them and they are benched, departed, or their unit is \
                         paused"
                    );
                }
            }
            // F8, the settle path's shrink half, committed in THIS transaction
            // after the reconcile that decided it: withdraw the launch intent of
            // every fenced person whose routine idle park just went terminal
            // (`applied` — the person released it — or `forced` — the full
            // handoff grace window expired with nobody releasing it, #337),
            // plus the #12 stale
            // sweep: a fenced person the fresh snapshot proves has no demand and
            // no in-flight handoff (authorized during churn and never genuinely
            // needed). A person mid-handoff KEEPS their intent so the handoff can
            // complete; an operator's or lifecycle intent's park is never a
            // withdrawal reason. Withdrawal only narrows, so re-running is a
            // harmless no-op, and genuine future work re-authorizes through the
            // explicit launch path (the mail grant above, or `org start-person`).
            if let LaunchFence::Fenced(person_ids) = &fence {
                let post_activity = activity::read(ledgers, &manifest)?;
                // The reason travels WITH the person, because the two arms
                // mean different things to whoever reads the line: `settled` is
                // an agent that finished and parked, `no-demand` is a fence
                // this pass could find nothing behind.
                let withdraw: std::collections::BTreeMap<String, &'static str> = person_ids
                    .iter()
                    // A WAKE THE OPERATOR PAID FOR IS NOT WITHDRAWN INSIDE ITS
                    // OWN LEASE. Operator ruling, 2026-08-20: "If woken, it needs
                    // to wait the 2 mins." Both branches below are legitimate
                    // readings of an agent's own reports — `settled` is a park
                    // that went terminal, `no-demand` is a fence with nothing
                    // behind it — and both of them fire on a person who was woken
                    // seconds ago and simply has not been given work yet. The
                    // lease is a FLOOR: past it this filter admits everybody
                    // again and the two branches resume exactly.
                    //
                    // The `not-operational` sweep above is deliberately NOT
                    // gated. That branch fires for somebody benched, departed, or
                    // whose unit was paused — a NEWER operator decision than the
                    // wake, and it must win over the older one.
                    .filter(|person_id| {
                        !post_activity.people.get(person_id.as_str()).is_some_and(|state| {
                            chiefd_core::store::activity::operator_wake_lease_active(state, now_ms)
                        })
                    })
                    .filter_map(|person_id| {
                        if let Some(transition) =
                            post_activity.active_transition(person_id.as_str())
                        {
                            let settled = activity::is_routine_idle_park(transition)
                                && matches!(
                                    transition.status,
                                    activity::TransitionStatus::Applied
                                        | activity::TransitionStatus::Forced
                                );
                            settled.then(|| (person_id.clone(), "settled"))
                        } else {
                            snapshot
                                .people
                                .get(person_id)
                                .is_some_and(|decision| {
                                    !decision.active && decision.transition_id.is_none()
                                })
                                .then(|| (person_id.clone(), "no-demand"))
                        }
                    })
                    .collect();
                if !withdraw.is_empty() {
                    // `current` is the exact fence this pass enforced — the fresh
                    // row read plus this pass's grants — never the actor's
                    // possibly stale in-memory document (see `remove`'s doc).
                    let current =
                        chiefd_core::store::launch_intent::LaunchIntent::Fenced(person_ids.clone());
                    chiefd_core::store::launch_intent::remove(
                        ledgers,
                        &ctx,
                        &current,
                        withdraw.keys().cloned(),
                    )?;
                    withdrawn.extend(withdraw);
                }
            }
            // Did the wake actually work? Asked HERE, against the snapshot the
            // projection just committed, because this is the only place that
            // holds both halves — who had mail, and who the pass decided to run.
            //
            // The reason is read off the decision's own reasons rather than
            // re-derived: `Requested` is added only for an OPERATIONAL person,
            // so its absence is that answer, and its presence beside
            // `active = false` can only be the fence gating last.
            //
            // A STOOD-DOWN COMPANY RAISES NO ALARM. This note means "a company
            // that has stopped working", and while an operator is deliberately
            // holding it stopped that is the intended state, not a fault. Left
            // alone it would WARN once per person once per pass, for as long as
            // the stand-down stands — turning the operator's own decision into
            // a fault log. `stand_down_held` says the same thing truthfully.
            let mail_unmet: Vec<(String, String)> = if stood_down {
                Vec::new()
            } else {
                observed_mail
                    .iter()
                    .filter_map(|person_id| {
                        let decision = snapshot.people.get(person_id)?;
                        if decision.active {
                            return None;
                        }
                        let reason = if decision
                            .reasons
                            .contains(&chiefd_core::store::activity::ActivityReason::Requested)
                        {
                            "authorized but the launch-intent fence excludes them"
                        } else {
                            "not operational: benched, departed, or its unit is paused"
                        };
                        Some((person_id.clone(), reason.to_owned()))
                    })
                    .collect()
            };
            Ok(FenceOutcome { withdrawn, mail_granted, mail_unmet, stand_down_held: held })
        },
    )
    .await
    .map_err(duty)
}

/// Whether a fenced person's start decision is still DEMAND on this pass.
///
/// # The rule
///
/// A grant lapses when the person ANSWERS, not when chiefd decides they should
/// be up. `last_desired_active` is chiefd's own decision from the previous
/// pass and says nothing about whether a pane exists; the agent's own stamps
/// are the first evidence that one does. So:
///
/// * never desired — the grant is fresh demand,
/// * desired and silent — their pane is still starting, the grant STAYS demand,
/// * desired and it has spoken (`agent_active_at`, or `agent_quiet_at` from a
///   settle) — the grant has done its job and the ordinary settle path owns
///   them from here.
///
/// # Why it is not `!last_desired_active` alone
///
/// Measured on `taperoom-inc`, 2026-08-19, on four consecutive operator
/// clicks: `org_events` carries `launch-intent dev upsert` at 23:24:21.570 and
/// `launch-intent dev delete` at 23:24:22.132 — the operator's grant survived
/// 562ms. It lapsed on the pass after it was made, the shrink half then read a
/// fence with no demand behind it and withdrew the row, and dev's Pi — which
/// reported `interactive-loop-ready` at 23:25:02, forty seconds after the
/// click — came up into a company that no longer wanted him and was reaped.
/// The operator clicked four times and saw nothing happen four times.
///
/// This cannot pin somebody up for ever: a person who will not stay up is
/// named by the actuator's crash-loop counter, and the grant is still
/// withdrawn as soon as they answer and settle.
fn fence_still_supplies_demand(
    state: Option<&chiefd_core::store::activity::PersonActivityState>,
    now: i64,
) -> bool {
    state.is_none_or(|state| {
        // THE OPERATOR'S WAKE OUTRANKS THE AGENT'S OWN REPORT, for the length of
        // the lease it bought. Operator ruling, 2026-08-20: "If woken, it needs
        // to wait the 2 mins." Every clause below reads what the AGENT said
        // about itself, and by those clauses a woken agent that beats once and
        // is handed nothing to do is indistinguishable from one that finished
        // its work — which is how a grant paid for by a click lapsed on the very
        // next pass. See `activity::operator_wake_lease_active`.
        chiefd_core::store::activity::operator_wake_lease_active(state, now)
            || !state.last_desired_active
            || (state.agent_active_at.is_none() && state.agent_quiet_at.is_none())
    })
}

/// Deterministic intent-row id for one pass, keyed on the company so a re-run
/// of the same tick supersedes rather than accretes.
fn action_intent_id(slug: &str) -> String {
    format!("converge:{slug}:actions")
}

/// One human-readable audit line per action, in plan order.
///
/// The replacement for `plan_step_summary`, which summarized pane steps. An
/// operator reading a support transcript needs the same thing it gave them —
/// what chiefd asked for, in order — expressed in the vocabulary that survived
/// the split: people and launch hashes, never windows and panes.
/// One audit line per person chiefd wants running, and the hash they must be
/// running at.
///
/// REPLACES `action_summary`, which rendered `start`/`restart`/`stop`/`stop the
/// whole company` lines. There are no actions to summarize: chiefd states the
/// desired set and the actuator computes the transition, so the honest audit
/// row records what chiefd DECIDED rather than what it guessed would happen.
fn desired_summary(desired: &DesiredRuntime) -> Vec<String> {
    desired
        .people
        .iter()
        .map(|person| format!("desired {} @{}", person.person_id, person.launch_hash))
        .collect()
}

/// Record what chiefd asked for this pass, as an audit row.
///
/// This is an AUDIT, not the answer. The answer is computed at read time on the
/// two `/v1/org/runtime/*` routes, against the observation the calling client
/// committed in that same request. This row exists so an operator investigating
/// afterwards can see what was asked and when, and so the shadow diff has
/// something to diff. Nothing reads it back to decide anything — which is the
/// property that keeps it from becoming a second source of truth.
/// The audit body for one pass, derived and comparable.
///
/// Split out of [`write_action_intent`] so the caller can build it BEFORE
/// deciding whether to write, and skip a write whose body the committed row
/// already carries. A body that is a pure function of the pass's decisions is
/// what makes that comparison meaningful.
fn intent_body(desired: &DesiredRuntime, shadow: bool, sweep_live: bool) -> ConvergeIntentBody {
    ConvergeIntentBody {
        shadow,
        sweep_live,
        // chiefd no longer predicts kills or respawns: those are TRANSITIONS,
        // and a transition can only be computed by something that knows the
        // current state. The audit row records the desired SET instead, which
        // is the whole of what chiefd decided this pass.
        predicted_kill_panes: 0,
        predicted_respawn_persons: 0,
        pointer_clears: 0,
        steps: desired_summary(desired),
    }
}

async fn write_action_intent(
    db: &CompanyDb,
    slug: &str,
    body: ConvergeIntentBody,
) -> Result<(), DutyError> {
    let id = action_intent_id(slug);
    db.mutate(
        MutationClass::Reconcile,
        MutationName("reconcile_cycle.open_intent"),
        move |ledgers| converge_intent::open(ledgers, &id, &body),
    )
    .await
    .map_err(duty)
}

async fn close_action_intent(
    db: &CompanyDb,
    slug: &str,
    _plan: &DesiredRuntime,
) -> Result<(), DutyError> {
    let id = action_intent_id(slug);
    db.mutate(
        MutationClass::Reconcile,
        MutationName("reconcile_cycle.close_intent"),
        move |ledgers| {
            converge_intent::close(ledgers, &id);
            Ok(())
        },
    )
    .await
    .map_err(duty)
}

async fn apply_pointer_sweep(
    db: &CompanyDb,
    actions: Vec<ClearPointerAction>,
) -> Result<usize, DutyError> {
    db.mutate(
        MutationClass::Reconcile,
        MutationName("reconcile_cycle.pointer_sweep"),
        move |ledgers| {
            let manifest = organization::read(ledgers)?;
            let supervision = supervision::read(ledgers, &manifest)?;
            activity::apply_pointer_clears(ledgers, &manifest, &supervision, &actions)
        },
    )
    .await
    .map(|cleared| cleared.len())
    .map_err(duty)
}

// `escalate` is GONE (#751/P8-P10). It enqueued a supervision escalation when
// an APPLY failed hard — a tripped circuit breaker or a refused destructive
// budget — and both of those are verdicts about an actuation attempt. This pass
// makes none: it publishes the desired set and the safety policy, and the
// client that actuates reports its own outcome on its next observed-runtime
// POST. The escalation belongs wherever that outcome is judged, and inventing
// one here from a plan nobody ran would be an alarm about an event that did not
// happen. `supervision::…::enqueue_reconcile_escalation` is untouched and still
// has other callers.

/// The production [`ReconcileActuator`]: one company's writer + host + config.
///
/// Holds the writer as an [`Arc<CompanyDb>`] — the SAME actor the daemon loop
/// and every other hook share — never an owned `CompanyDb`. There is exactly one
/// writer thread per company (plan §5); a second `CompanyDb::open` on the same
/// `chief.db` would be a second writer with its own stale snapshot cache, so the
/// daemon clones its `Arc` into this actuator rather than opening another.
///
/// `launch_intent` is the shared legacy `org.sqlite` reader the projection's
/// legacy facts are sourced from (wired when `CHIEFD_STORE_DB_PATH` is set):
/// the launch-intent fence and the legacy mailbox's pending-bucket demand.
/// When it is absent the
/// cycle skips the fence projection entirely and plans from the committed
/// activity ledger as before — never from a fabricated fence.
///
/// TOMBSTONE (#751-P4): a "reflection-memory backfill" was listed here as a
/// fourth legacy fact. It folded the bounded handoff documents embedded in a
/// legacy activity blob into durable memory rows. The handoff payload is
/// deleted from the product, so there is nothing left to fold and this reader
/// never had an accessor for it anyway.
pub struct ConvergeActuator {
    db: Arc<CompanyDb>,
    config: ActuatorConfig,
    launch_intent: Option<ReconcilerFactsStore>,
    /// What this actuator last recorded, so an unchanged desired set writes
    /// nothing (#367). Lives on the actuator because the actuator is what
    /// outlives a pass.
    last_audit: LastAudit,
}

impl ConvergeActuator {
    /// Compose an actuator over the company's shared writer actor.
    #[must_use]
    /// #751/P8 removed the `host` argument. A converge pass no longer touches
    /// the machine, so an actuator that cannot be handed a host executor is the
    /// type-level statement that chiefd stopped actuating.
    pub fn new(db: Arc<CompanyDb>, config: ActuatorConfig) -> Self {
        Self { db, config, launch_intent: None, last_audit: LastAudit::default() }
    }

    /// Wire the legacy launch-intent source the activity-fence projection
    /// reads each cycle (see the type docs).
    #[must_use]
    pub fn with_launch_intent_store(mut self, store: Option<ReconcilerFactsStore>) -> Self {
        self.launch_intent = store;
        self
    }
}

/// Build the cycle's [`LaunchFence`] from the legacy `org_documents`
/// launch-intent row, validated against the committed manifest. Mirrors the
/// TypeScript `documentPersonIds` hygiene: the CEO is implicitly intended
/// (never stored) and ids the current manifest no longer knows are dropped, so
/// a stale fence cannot name a departed person.
fn launch_fence_from_legacy(
    store: &ReconcilerFactsStore,
    snapshot: &LedgerSnapshot,
    row_slug: &str,
) -> Result<LaunchFence, DutyError> {
    let manifest = organization::read(snapshot).map_err(duty)?;
    let person_ids =
        store.launch_intent_person_ids(row_slug, &manifest.slug).map_err(DutyError::new)?;
    tracing::debug!(
        row_slug,
        organization = %manifest.slug,
        person_ids = ?person_ids,
        "chiefd converge: observed normalized launch-intent fence"
    );
    // The CEO is no longer filtered out below, but this read STAYS: it is the
    // fail-closed check that this manifest has a root at all. A manifest with
    // no CEO cannot be fenced meaningfully — `LaunchFence::admits`
    // short-circuits on the CEO — so discovering that here, where the pass can
    // still refuse, is strictly better than discovering it in activity.
    manifest
        .chief_person_id()
        .map_err(|refusal| DutyError::new(format!("manifest has no CEO person id: {refusal:?}")))?;
    // TOMBSTONE: `.filter(|id| *id != ceo)` — the root was STRIPPED OUT of its
    // own fence here, and after #1148 that one clause was the whole of the
    // operator's empty company.
    //
    // The chain it broke: `prepare_ceo_only` writes the CEO's launch-intent row
    // (genesis does it, and so does attach), this function reads the rows back,
    // dropped the CEO, and the CEO therefore never reached
    // `requested_person_ids` — which only extends from `LaunchFence::Fenced`
    // ids. No demand. Since #1148 deleted the unconditional
    // `ActivityReason::OrganizationRoot` lease, demand is the WHOLE of `active`,
    // so the root was never desired and a freshly created company logged
    // `requested=0 applied=0` forever with no tmux session.
    //
    // The clause was correct while that lease existed: the root ran unasked, so
    // naming it in a fence was redundant and filtering it kept the fence
    // honest about what it decided. #1148 removed the lease and silently made
    // this line the thing that breaks its replacement — the same shape as the
    // lease's own tombstone, one layer further out.
    //
    // Removing it does NOT let a fence take the root down. `LaunchFence::admits`
    // short-circuits on the CEO, so an empty or narrow fence is still CEO-only
    // in the permissive direction. What changes is that the root can be NAMED,
    // which is the only way anything can ask for it.
    Ok(LaunchFence::fenced(person_ids.into_iter().filter(|id| manifest.people.contains_key(id))))
}

/// The SQL mailbox facts for the projection: every normalized mailbox row
/// with pending mail, including its envelope creation time. A read failure fails the pass
/// closed, like the launch-intent read: an unobservable store must not
/// silently read as "no demand anywhere" — the projection would plan kills
/// for people whose work mail it simply could not see.
fn pending_mail_from_sql(
    store: &ReconcilerFactsStore,
    _snapshot: &LedgerSnapshot,
    row_slug: &str,
    quiesced_ms: Option<i64>,
) -> Result<Vec<PendingMailFact>, DutyError> {
    store.pending_mail_facts_after(row_slug, quiesced_ms).map_err(DutyError::new)
}

impl ReconcileActuator for ConvergeActuator {
    fn reconcile(
        &self,
        ctx: &DutyContext,
        mode: ActuationMode,
    ) -> BoxFuture<'_, Result<ReconcileReport, DutyError>> {
        let snapshot = Arc::clone(&ctx.snapshot);
        let launch_intent = self.launch_intent.clone();
        // A shared ChiefD database scopes normalized rows by the composite
        // `document_key(slug, data_root)` label. The actor owns that exact key;
        // the manifest's bare slug is only the domain identity carried inside
        // each row. Reading the legacy fact tables under the bare slug makes a
        // successfully published launch intent invisible to the live runtime.
        let row_slug = self.db.label().to_owned();
        Box::pin(async move {
            let quiesced_ms = self.db.goal_delivery_quiesce_read().await.map_err(duty)?.and_then(
                |(quiesce, _seq)| chiefd_core::isotime::parse_iso_millis(&quiesce.quiesced_at),
            );
            // #668: ONE construction, not two arms.
            //
            // No legacy store is NOT permission to plan from the previous
            // activity snapshot. A cold ChiefD restart can inherit a whole
            // roster of old `last_desired_active=true` rows; that is history,
            // not a fresh decision to boot everyone. The cold path reconciles
            // through the deliberate `Unfenced` sentinel: it recomputes
            // reasons from native supervision/mail/transition facts, so
            // CEO-only is the zero-demand default and one named current reason
            // admits only its own person. `Unfenced` is safe precisely because
            // this is a reason pass, never a direct projection of persisted
            // desired-active state.
            //
            // Previously the two cases were two separate `Some(...)`
            // constructions of the same struct, differing only in where each
            // field came from. That is a cold-restart-specific branch beside
            // the warm one, and two constructions of one value are two things
            // that can drift: a field added to the warm arm and forgotten on
            // the cold arm is invisible until a cold restart, which is the
            // least-observed moment the process has. Collapsing them leaves
            // exactly one place a projection is built, so the cold path cannot
            // silently diverge from the warm path — it takes the same
            // fence-and-reason pass with a different fence value, which is the
            // only thing that legitimately differs.
            //
            // A herd is a symptom of a missing fence, not of insufficient
            // spacing: deliberately NO stagger and NO jitter here. Spacing
            // makes the window longer and the failure rarer, which converts a
            // reproducible thundering herd into an intermittent one — strictly
            // worse to diagnose and no safer.
            let (fence, pending_mail_facts) = match &launch_intent {
                Some(store) => (
                    launch_fence_from_legacy(store, snapshot.as_ref(), &row_slug)?,
                    pending_mail_from_sql(store, snapshot.as_ref(), &row_slug, quiesced_ms)?,
                ),
                // NO SHARED FACTS STORE IS NOT NO MAIL. This branch answered
                // `Vec::new()`, and the pass still saw demand — through
                // chiefd's in-memory mailbox, the stale half that pinned a live
                // company awake for ever and is now deleted. Removing that half
                // without this read would have made an unwired actuator blind
                // to every envelope, which is the same class of silence in the
                // other direction. The company's own `mailbox` table is the
                // truth either way; here it is read through the writer that
                // owns it.
                None => {
                    let (mailbox, _seq) = self.db.mailbox_read().await.map_err(duty)?;
                    (
                        LaunchFence::Unfenced,
                        chiefd_core::store::reconciler_facts::pending_mail_facts_from_snapshot(
                            mailbox,
                            quiesced_ms,
                        ),
                    )
                }
            };
            let maintenance_person_ids: Vec<String> = match &launch_intent {
                Some(store) => store
                    .open_maintenance_person_ids(&row_slug)
                    .map_err(DutyError::new)?
                    .into_iter()
                    .collect(),
                None => self
                    .db
                    .session_maintenance_read()
                    .await
                    .map_err(duty)?
                    .into_iter()
                    .flat_map(|(ledger, _seq)| {
                        ledger
                            .ordered_requests()
                            .filter(|request| request.status.is_open())
                            .map(|request| request.person_id.clone())
                            .collect::<std::collections::BTreeSet<_>>()
                    })
                    .collect(),
            };
            let projection =
                Some(ActivityProjectionInput { fence, pending_mail_facts, maintenance_person_ids });
            // #606/#717's apply-time facts probe is GONE with the apply it
            // guarded. It re-read the launch-intent fence on its own
            // connection immediately before `apply_plan` so an attended
            // `company ceo` could not change the fence inside the
            // gather-to-apply window. There is no such window: the action
            // stream is derived per request against a fresh snapshot, so the
            // fence is re-read every time it could matter.
            reconcile_cycle_with_audit(
                self.db.as_ref(),
                &self.config,
                mode,
                projection,
                &self.last_audit,
            )
            .await
        })
    }
}

/// How many launch subjects a single actuation note names before summarising
/// the remainder. `ReconcileReport::notes` is joined into one log line and has
/// no truncation of its own, so an uncapped list would grow without bound on
/// exactly the oversized plans this note exists to explain.
pub(crate) const MAX_NAMED_LAUNCH_SUBJECTS: usize = 12;

/// The people chiefd wants running this pass, in canonical order, **duplicates
/// preserved**.
///
/// #107: `planned=N` could not distinguish one person planned twice from two
/// different people, which is the first question a double-spawn incident asks.
/// A repeated id here IS the finding, so this deliberately does not dedupe --
/// and it still can be one, because a manifest with a duplicated person id
/// reaches the desired set twice.
///
/// Re-based again on `DesiredPerson`. It no longer needs to skip stops, because
/// there are none: a person who should not run is simply ABSENT from the
/// desired set, so every entry here is by definition somebody chiefd wants up.
pub(crate) fn launch_subjects(people: &[DesiredPerson]) -> Vec<&str> {
    people.iter().map(|person| person.person_id.as_str()).collect()
}

/// One operator-facing note naming the launch subjects, or `None` when the pass
/// launches nobody (idle stays silent, per #367's no-op logging rule).
///
/// Over [`MAX_NAMED_LAUNCH_SUBJECTS`] the note names the first N and appends
/// `(+K more)` — it announces its own truncation rather than hiding it.
pub(crate) fn launch_subjects_note(people: &[DesiredPerson]) -> Option<String> {
    let subjects = launch_subjects(people);
    if subjects.is_empty() {
        return None;
    }
    let total = subjects.len();
    if total <= MAX_NAMED_LAUNCH_SUBJECTS {
        return Some(format!("launching: {}", subjects.join(", ")));
    }
    let named = subjects[..MAX_NAMED_LAUNCH_SUBJECTS].join(", ");
    Some(format!("launching: {named} (+{} more)", total - MAX_NAMED_LAUNCH_SUBJECTS))
}

#[cfg(test)]
mod tests;
