//! `GET /v1/org/runtime/desired` and `POST /v1/org/runtime/launch-catalog` —
//! what chiefd wants running, and with what.
//!
//! # Two routes, both pure reads
//!
//! `desired` publishes the DESIRED SET: exactly the people who should be
//! running, each with the derived hash of what their process must be built
//! from. It commits nothing. The actuator diffs this against the panes in front
//! of it — "a pane exists for this person AND its tag matches the hash" — and
//! closes the difference itself.
//!
//! `launch-catalog` answers the other half of "start this person": `desired`
//! says WHO, and this says WITH WHAT — the pinned pi binary, the pi-home, the
//! workspace, the model, the provider, the granted tools, the theme files, the
//! session to resume, the non-secret pane environment. It exists as its own
//! route because the derivation behind it is a fail-closed on-disk gate over
//! the daemon's own data root that also stages the person's provider
//! credential. Materialization is the daemon's job, so the daemon is the only
//! process entitled to answer "may this person launch". A client that
//! re-implemented that read would be a second reader of private state and a
//! second answer to the same question.
//!
//! # TOMBSTONE: `POST /v1/org/runtime/observed` and `POST /v1/org/runtime/actions`
//!
//! `observed` was the actuator SPEAKING: it committed the client's report of
//! what it saw in tmux and answered with an action plan computed against it.
//! `actions` was the read-only re-read of that same report.
//!
//! Both are deleted, and the direction they represented is now barred. **The
//! actuator never reports anything to chiefd.** chiefd holds the desired state;
//! host facts do not travel up. What the round trip was defended for is not
//! lost but relocated: it closed an observe→plan TOCTOU gap, and under a
//! desired set there is no gap to close, because the set does not depend on
//! what was observed. The actuator reads chiefd's desired state and observes
//! tmux at the same moment it acts on both, which is the only place those two
//! facts can be held together honestly.
//!
//! The permitted direction is untouched: an AGENT may report facts about
//! ITSELF over HTTP — heartbeats, "I settled", a native model switch. That is a
//! fact about the agent, not about tmux.
//!
//! # What is deliberately NOT on the wire
//!
//! No session name, no socket, no window id, no pane id, no layout string.
//! chiefd publishes facts about PEOPLE. `apps/chiefd/tests/wire-boundary`
//! asserts that mechanically over the real bodies, because the repo's backend
//! boundary guard scans file text and cargo edges and cannot inspect a wire
//! shape.
//!
//! # No actuator is no longer an answer chiefd can give
//!
//! `actions` used to report `actuator.presence = never-attached | lapsed`, so
//! an operator could see a company nobody was actuating. That is gone with the
//! report that carried it: presence was derived from a lease the actuator
//! renewed by reporting, and there is no report. This is a NAMED, ACCEPTED loss
//! (see the design record) — the actuator owns the operator's
//! screen and is the thing that knows it is not converging, so it says so
//! there. Inventing a second upward channel to recover this would reintroduce
//! exactly what was removed.

use axum::extract::{Extension, Json};

use chiefd_core::runtime::actuation::{publish_desired_runtime, DesiredRuntime};
use chiefd_core::runtime::launch_catalog::LaunchCatalog;
use chiefd_core::runtime::launch_hash::{desired_launch_hash, LaunchInputs};
use chiefd_core::runtime::roster::project_desired_roster;
use chiefd_core::store::converge_safety::ConvergeSafetyState;
use chiefd_core::store::mailbox::MailboxState;

use super::org_slice::{failed, live, Refused, SlugRequest};
use super::router::SupervisionLiveSource;

async fn read_safety(source: &SupervisionLiveSource) -> Result<ConvergeSafetyState, Refused> {
    Ok(source
        .company
        .converge_safety_read()
        .await
        .map_err(|error| failed(&error))?
        .map_or_else(ConvergeSafetyState::default_shadow, |(state, _seq)| state))
}

/// Build the desired set for one company.
///
/// The launch hash is derived HERE, where the extension digest is available,
/// and handed to `publish_desired_runtime` as a closure. `chiefd-core` does not
/// compute it because it cannot see the launcher checkout, and a second opinion
/// about which code is on disk is precisely the defect the digest exists to
/// prevent.
async fn desired_for(source: &SupervisionLiveSource) -> Result<DesiredRuntime, Refused> {
    // ONE actor visit, ONE manifest reconstruction. These were two calls, and
    // `activity_read` rebuilds the whole manifest for itself because the
    // ledger's `person_order` is anchored to the organization rows — so the
    // route paid `3 + 4N` statements twice for an answer that cannot differ
    // between them, and could observe the two halves from either side of a
    // commit.
    let (manifest, activity) = match source.company.org_manifest_and_activity_read().await {
        Ok(Some((manifest, activity, _seq))) => (manifest, activity),
        Ok(None) => {
            return Err(Refused::not_found(
                "unknown-company",
                "company has no organization manifest",
            ))
        }
        Err(error) => return Err(failed(&error)),
    };
    let roster = project_desired_roster(&manifest, activity.as_ref());

    // The extension digest, read ONCE for the whole company. It is a property
    // of the launcher checkout rather than of a person, so a per-person read
    // would be the same answer computed N times -- and N chances for two people
    // in one pass to disagree about which deploy is live.
    // REFUSE, never default. An absent actuator config used to `unwrap_or_default()`
    // here, hashing the EMPTY STRING as though it were a positive digest -- the
    // same unreadable-becomes-empty collapse this whole change exists to
    // remove, and with a nasty consequence: a standalone or migration router
    // would confidently serve hashes that disagree with the daemon's, and an
    // actuator reading them would replace every pane in the company.
    //
    // The launch-catalog route one function down already refuses in exactly
    // this situation, for exactly this reason ("a wrong data root does not fail
    // -- it confidently refuses every person for a reason that is not true").
    // Same condition, same refusal.
    let config = source.reconcile_actuator_config.as_ref().ok_or_else(no_actuator_config)?;

    // THE DIGEST IS COMPUTED, not read off a field, and it is computed HERE
    // because this is the crate that can see the launcher checkout.
    //
    // It is the input that makes the launch hash catch a DEPLOY: a launcher
    // deploy rewrites extension code and changes no person row, so a hash over
    // rows alone misses it entirely and a fleet comes up on old code reporting
    // success. Every tombstone for the deleted runtime-drift scan rests on this
    // value moving.
    //
    // An unreadable checkout REFUSES. It must never fall back to a default: a
    // digest that differs from the real one replaces every pane in the company,
    // so "I could not read the extension source" and "the extension source
    // hashes to X" have to stay different answers all the way to the wire.
    // A launcher-assets failure is 503, not 500, and for the same reason the
    // digest refusal below is: the checkout could not be read THIS MOMENT, and
    // a moment later it may be. What it must never become is a digest.
    //
    // It carries the REAL reason resolution failed -- the root is not a
    // checkout, or the checkout was never built -- rather than the fixed
    // "extension-source-unreadable" a `|_|` discard used to publish. That
    // discard was its own multi-session hunt: the actuator surface said "could
    // not read its launcher extension source" over a source tree that was
    // perfectly readable and merely UNBUILT, and daemon.log carried no code and
    // no path to tell an unreadable checkout from an unbuilt one apart.
    let assets = chiefd_host::runtime_lifecycle::launcher_assets(&source.company, config)
        .await
        .map_err(launcher_assets_unavailable)?;
    let extension_digest = chiefd_host::materialize::extension_source_digest(&assets)
        .ok_or_else(unreadable_extension_source)?;

    let safety = read_safety(source).await?;
    let effective = safety.effective_config();

    // THE OPERATOR'S EXPLICIT COMPANY STOP. It is read HERE, on the route the
    // actuator actually consumes, and not only inside the daemon's own converge
    // pass -- the actuator diffs THIS body against the host, so a stop that
    // reached the log and not the wire would be undone on the next pass by the
    // very client the stop exists to instruct.
    //
    // An unreadable runtime row is NOT read as "not stopped": that is the
    // unreadable-becomes-empty collapse, and here it would resurrect a company
    // somebody switched off. It refuses instead, like the extension digest one
    // function down and for the same reason.
    let stopped = source
        .company
        .runtime_read()
        .await
        .map_err(|error| failed(&error))?
        .is_some_and(|(runtime, _seq)| runtime.status == "stopped");

    Ok(publish_desired_runtime(
        &roster,
        effective.actuation_mode,
        safety.breaker_tripped,
        stopped,
        |person_id| {
            // The MODEL-FREE launch command form, and the qualifier is
            // load-bearing. The executed argv carries `--provider`, the model
            // and the thinking level, and all three are LIVE-APPLY: feeding the
            // real argv in here would restart a person for changing their own
            // model, which is precisely what `LaunchInputs` is shaped to
            // prevent. `launch_command_fingerprint` is the one producer of that
            // form, so the exclusion is enforced where the string is built
            // rather than trusted at every call site.
            let launch_command =
                manifest.people.get(person_id).map_or_else(String::new, |record| {
                    chiefd_core::runtime::launch_hash::launch_command_fingerprint(
                        record,
                        &config.pi_binary.display().to_string(),
                    )
                });
            desired_launch_hash(&LaunchInputs {
                organization: &roster.company.slug,
                person_id,
                launch_command: &launch_command,
                extension_digest: &extension_digest,
            })
        },
    ))
}

/// `GET /v1/org/runtime/desired` — what chiefd wants running.
///
/// A pure read: it commits nothing. It does STAMP one in-memory cell — the
/// instant of this read — and that is the whole of chiefd's answer to "is
/// anybody converging this company".
///
/// # Why the stamp belongs here and is not a lease
///
/// The header above bars the actuator from reporting host facts upward, and
/// that bar is untouched: no session, socket, window, pane or layout crosses
/// this boundary, and nothing was added to the request or the response. The
/// only new fact is one chiefd derives about ITSELF — that it was asked. An
/// actuator reads this route on every round of its loop and its loop runs at
/// least once per changefeed ceiling even when the company is silent, so
/// silence here is not a quiet company, it is an absent actuator.
///
/// This is deliberately the weakest possible form of the `actuator.presence`
/// signal `POST /v1/org/runtime/actions` used to carry, and which the tombstone
/// above records as a "NAMED, ACCEPTED LOSS". It was not acceptable: with it
/// gone, a company whose entire tmux server died — eleven panes, five people —
/// went on reporting a healthy supervision pass and a headcount of five for
/// forty minutes, because chiefd held no fact that could have contradicted it.
/// A supervisor owes an honest answer to "does the thing I supervise exist",
/// and this read is the cheapest true one available.
pub(crate) async fn org_runtime_desired(
    Extension(supervision_live): Extension<Option<SupervisionLiveSource>>,
    Json(req): Json<SlugRequest>,
) -> Result<Json<DesiredRuntime>, Refused> {
    let source = live(supervision_live, &req.slug)?;
    // Stamped BEFORE the read is served, not after it succeeds. Somebody is
    // here either way, and a desired-set derivation that refuses (an
    // unreadable extension source, say) is a fault to report on its own terms
    // — reading it as an absent actuator would blame the wrong thing.
    source.actuator_attendance().record_read(source.clock_now());
    Ok(Json(desired_for(&source).await?))
}

/// The refusal a router with no actuator configuration answers with.
///
/// `503`, not `404` and not `500`: the company is real and the request is
/// well-formed — this particular chiefd surface simply has no data root to gate
/// against. Only `chiefd run` wires
/// `SupervisionLiveSource::reconcile_actuator_config`; the standalone and
/// migration routers deliberately do not, exactly like every other
/// daemon-only capability the source carries.
///
/// The route must NEVER discover the paths itself. An `ActuatorConfig` built
/// inside a handler would be a second opinion on where this company's data root
/// is, and a wrong data root does not fail — it confidently refuses every
/// person for a reason that is not true.
fn no_actuator_config() -> Refused {
    Refused::unavailable(
        "launch-catalog-unavailable",
        "this chiefd surface has no actuator configuration, so it cannot gate a launch against \
         any data root; only `chiefd run` serves the launch catalog",
    )
}

/// The refusal when this daemon cannot read its own launcher extension source.
///
/// `503`, like [`no_actuator_config`]: the company is real and the request is
/// well-formed, and a checkout that cannot be read now may be readable a moment
/// later. It is emphatically NOT an empty or default digest — see
/// [`desired_for`] for why a wrong digest is worse than no answer.
fn unreadable_extension_source() -> Refused {
    Refused::unavailable(
        "extension-source-unreadable",
        "this daemon could not read its launcher extension source, so it cannot derive the \
         launch hash for anybody; refusing rather than publishing a digest that would replace \
         every pane in the company",
    )
}

/// The desired route's 503 when this daemon cannot resolve a USABLE launcher
/// checkout — carrying the launcher's OWN refusal, not a fixed label over it.
///
/// `launcher_assets` refuses by name: `launcher-root-unusable` for a root that
/// is not a launcher checkout at all, `launcher-root-unbuilt` for a checkout
/// whose extension SOURCES are present but whose built runtime is not. Both are
/// fixable — build the checkout, or repoint the recorded root — so both are
/// 503, exactly like [`unreadable_extension_source`], and a moment later the
/// resolution may succeed. What each must never become is a digest.
///
/// The point of this function is that it does NOT collapse those into
/// "extension-source-unreadable". A `.map_err(|_| unreadable_extension_source())`
/// did, and it lied: on a source tree that was perfectly readable and merely
/// unbuilt it told an operator the SOURCE could not be read, with no code and
/// no path in daemon.log to tell the two apart — the exact defect that let a
/// green workspace suite sit on top of a company whose CEO would not boot.
fn launcher_assets_unavailable(
    error: chiefd_host::runtime_lifecycle::RuntimeLifecycleError,
) -> Refused {
    use chiefd_core::error::ChiefdError;
    use chiefd_host::runtime_lifecycle::RuntimeLifecycleError;
    match error {
        RuntimeLifecycleError::Store(ChiefdError::Refused(refusal)) => {
            Refused::unavailable(refusal.code, refusal.message)
        }
        // No launcher refusal to carry (a settings-read fault, a host error):
        // still 503 and fixable, still never a digest, but say plainly that the
        // launcher assets could not be resolved rather than blame the source.
        other => Refused::unavailable("launcher-assets-unresolved", other.to_string()),
    }
}

/// `POST /v1/org/runtime/launch-catalog` — WITH WHAT each person launches.
///
/// A pure read. It commits nothing, renews no lease, and is deliberately not
/// folded into the `observed` round trip: the catalog changes when
/// materialization changes, not when an observation does, and merging them
/// would make every heartbeat re-walk every person's home on disk.
///
/// A person the gate declines is absent from `people` but present in `roster`
/// with a named reason in `refusals`. That is the whole contract — the client
/// refuses their start step by name and the next pass retries once
/// materialization has caught up.
///
/// # The gate is not only about disk
///
/// It reads the trust table too, and this route is where that read belongs: it
/// is the one caller of the catalog builder that holds a company handle, and it
/// is already async and already committing nothing. A person whose enrolled
/// identity disagrees with the key in their own home cannot authenticate, so
/// publishing them a launch spec publishes a process that exits seconds later
/// and is published the same spec again on the next pass — which is exactly what
/// five people on a live company did, once a second, for a working day. They are
/// declined here, by name, with the key path to fix.
///
/// Still a pure read: `identity_read` is the pooled read path, one lookup per
/// person, beside a walk that already stats every person's home.
pub(crate) async fn org_runtime_launch_catalog(
    Extension(supervision_live): Extension<Option<SupervisionLiveSource>>,
    Json(req): Json<SlugRequest>,
) -> Result<Json<LaunchCatalog>, Refused> {
    let source = live(supervision_live, &req.slug)?;
    // The router ALREADY carries this — `chiefd run` builds it once and hands
    // it over. Taken, never constructed.
    let config = source.reconcile_actuator_config.as_ref().ok_or_else(no_actuator_config)?;
    let manifest = match source.company.org_manifest_read().await {
        Ok(Some((manifest, _))) => manifest,
        Ok(None) => {
            return Err(Refused::not_found(
                "unknown-company",
                "company has no organization manifest",
            ))
        }
        Err(error) => return Err(failed(&error)),
    };
    let session_epoch = chiefd_host::runtime_lifecycle::session_epoch_system_time(&source.company)
        .await
        .map_err(|error| failed(&error))?;
    // An organization with no root department has no Chief to compare against;
    // the identity check then simply treats every person as an ordinary one,
    // which is the right answer for a manifest in that state and is a refusal
    // nobody is served by inventing here.
    let chief_person_id = manifest.chief_person_id().unwrap_or_default().to_owned();
    let identity_refusals = chiefd_host::identity_enrolment::identity_launch_refusals(
        &source.company,
        &config.dir,
        &chief_person_id,
        manifest.people_order.iter().cloned(),
    )
    .await;
    // WHO HAS MAIL WAITING. One whole-company mailbox read, on the pooled read
    // path, beside a walk that already stats every person's home — the same
    // shape as `identity_launch_refusals` above and for the same reason: this
    // is the one caller of the catalog builder that holds a company handle.
    //
    // It decides which fresh-session sentence each person is sent. A pending
    // envelope is the only durable claim on somebody's attention that outlives
    // their Pi session (goals were deleted by #1047), so a person with none has
    // nothing assigned and is told to come up and idle. Without it a woken
    // person was told to "continue the next real piece of work" with no work to
    // continue, went looking, adopted the launcher's own source tree as the
    // company's project, and created a department and a hire nobody asked for.
    //
    // A read failure is a REFUSAL, never an empty set. Empty reads as "nobody
    // has mail", which would tell a person with work waiting to stand still —
    // the same fail-open the pending-mail projection already refuses to make
    // ("an unobservable store must not silently read as 'no demand anywhere'").
    let (mailbox, _seq) = source.company.mailbox_read().await.map_err(|error| failed(&error))?;
    let typed_mailbox = mailbox
        .entries
        .iter()
        .map(|entry| {
            MailboxState::parse(&entry.state)
                .map(|state| (entry.person.as_str(), state))
                .ok_or_else(|| {
                    Refused::fault(
                        "mailbox-state-unreadable",
                        format!(
                            "mailbox entry '{}' has unknown state '{}'",
                            entry.envelope_id(),
                            entry.state
                        ),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (people_with_pending_mail, inbox_counts) =
        mailbox_facts(&manifest.people_order, typed_mailbox);
    let mut catalog = chiefd_host::converge_apply::build_launch_catalog_for_session_epoch(
        &manifest,
        config,
        session_epoch,
        &identity_refusals,
        &people_with_pending_mail,
    );
    catalog.inbox_counts = inbox_counts;
    Ok(Json(catalog))
}

/// Derive the two mailbox facts the launch catalog publishes from one durable
/// snapshot. Launch demand is only a `pending` row. The operator-facing inbox
/// view also includes a fence-archived `delivered` row, which is the same rule
/// the person's own footer uses. The four pane-drain states are excluded.
fn mailbox_facts<'a>(
    roster: &[String],
    entries: impl IntoIterator<Item = (&'a str, MailboxState)>,
) -> (std::collections::BTreeSet<String>, std::collections::BTreeMap<String, usize>) {
    let mut pending_people = std::collections::BTreeSet::new();
    let mut inbox_counts: std::collections::BTreeMap<String, usize> =
        roster.iter().map(|person| (person.clone(), 0)).collect();
    for (person, state) in entries {
        let known = inbox_counts.contains_key(person);
        if known && state.supplies_launch_demand() {
            pending_people.insert(person.to_owned());
        }
        if known && state.is_inbox_message() {
            if let Some(count) = inbox_counts.get_mut(person) {
                *count += 1;
            }
        }
    }
    (pending_people, inbox_counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox_entry(
        id: &str,
        person: &str,
        state: MailboxState,
    ) -> chiefd_core::store::mailbox_rows::MailboxEntry {
        chiefd_core::store::mailbox_rows::MailboxEntry {
            envelope: chiefd_core::store::mailbox::MailboxEnvelope {
                schema_version: chiefd_core::store::mailbox::MAILBOX_ENVELOPE_SCHEMA_VERSION,
                id: id.to_owned(),
                organization: "northstar-conformance".to_owned(),
                from_person_id: "chief".to_owned(),
                to: person.to_owned(),
                recipients: vec![person.to_owned()],
                body: format!("message {id}"),
                urgency: chiefd_core::store::mailbox::Urgency::Normal,
                reply_to: None,
                health_incident: None,
                created_at: "2026-07-15T12:00:00.000Z".to_owned(),
            },
            person: person.to_owned(),
            state: state.as_str().to_owned(),
            updated_at: 1_784_116_800_000,
            extra: std::collections::BTreeMap::new(),
        }
    }

    fn admit_launch_subject(dir: &std::path::Path, person_id: &str) {
        let home = chiefd_host::agent_home::agent_home(dir, person_id);
        std::fs::create_dir_all(home.join("sessions")).expect("sessions");
        std::fs::create_dir_all(home.join(".pi/skills")).expect("project skills");
        std::os::unix::fs::symlink("../../../../skills", home.join(".pi/skills/worker"))
            .expect("role skill link");
    }

    /// The real route joins one real mailbox snapshot to the launch catalog.
    /// Delivered mail remains visible but cannot change the launch sentence.
    #[tokio::test]
    async fn the_launch_catalog_route_keeps_inbox_visibility_separate_from_launch_demand() {
        use chiefd_core::actor::{CompanyDb, MutationClass, MutationName};
        use chiefd_core::clock::SystemClock;
        use chiefd_core::store::{activity, organization, supervision};

        let dir = tempfile::tempdir().expect("tempdir");
        let company = std::sync::Arc::new(
            CompanyDb::open(
                "northstar-conformance",
                &dir.path().join("company.db"),
                std::sync::Arc::new(SystemClock::default()),
            )
            .expect("company"),
        );
        let manifest = chiefd_core::test_support::northstar_manifest(1_784_116_800_000);
        let seeded = manifest.clone();
        company
            .mutate(MutationClass::Normal, MutationName("test.seed"), move |ledgers| {
                organization::create(ledgers, &seeded)?;
                supervision::seed(ledgers, &seeded)?;
                activity::seed(ledgers, &seeded)?;
                Ok(())
            })
            .await
            .expect("seed company");

        let chief = "chief";
        let delivered_only = "quant-head";
        admit_launch_subject(dir.path(), delivered_only);
        let operator = dir.path().join("pi-agent");
        chiefd_host::files::publish_atomically(&operator.join("auth.json"), "{}", 0o600)
            .expect("operator provider credential");
        let mint = chiefd_host::identity_key::host_identity_key_mint();
        for (person, outcome) in chiefd_host::identity_enrolment::provision_people(
            &company,
            dir.path(),
            chief,
            [chief.to_owned(), delivered_only.to_owned()],
            &mint,
        )
        .await
        {
            assert!(outcome.is_authenticable(), "{person} identity: {outcome:?}");
        }

        company
            .mailbox_publish(chiefd_core::store::mailbox_rows::MailboxSnapshot {
                entries: vec![
                    mailbox_entry("pending", chief, MailboxState::Pending),
                    mailbox_entry("fence-archive", chief, MailboxState::Delivered),
                    mailbox_entry("delivered-only", delivered_only, MailboxState::Delivered),
                    mailbox_entry("settled", "signal-researcher", MailboxState::Accepted),
                ],
            })
            .await
            .expect("publish mailbox rows");

        let source = SupervisionLiveSource::new(
            std::sync::Arc::clone(&company),
            "northstar-conformance".to_owned(),
        )
        .with_reconcile_actuator_config(chiefd_host::converge_apply::ActuatorConfig {
            socket: "test-socket".to_owned(),
            watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
            dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            pi_binary: dir.path().join("pi"),
            floor: std::time::Duration::ZERO,
            launcher_root: dir.path().to_path_buf(),
            root_pi_agent_dir: operator,
        });
        let Json(catalog) = org_runtime_launch_catalog(
            Extension(Some(source)),
            Json(SlugRequest { slug: "northstar-conformance".to_owned() }),
        )
        .await
        .expect("route answer");

        assert_eq!(catalog.inbox_counts[chief], 2);
        assert_eq!(catalog.inbox_counts[delivered_only], 1);
        assert_eq!(catalog.inbox_counts["signal-researcher"], 0);
        assert_eq!(catalog.inbox_counts["it-head"], 0);
        assert!(catalog.people[chief].pending_mail, "pending mail supplies launch demand");
        assert!(
            !catalog.people[delivered_only].pending_mail,
            "delivered-only mail stays visible without supplying launch demand"
        );
    }

    /// Inbox is the durable VIEW, not only launch demand: a fence-archived
    /// delivered row stays visible. Every roster person gets an explicit zero,
    /// and a stale row for an unknown person cannot add a card fact for somebody
    /// the roster does not contain.
    #[test]
    fn inbox_counts_include_pending_and_delivered_for_every_roster_person() {
        let roster = vec!["vera".to_owned(), "nolan".to_owned(), "rhea".to_owned()];
        let (pending, inbox) = mailbox_facts(
            &roster,
            [
                ("vera", MailboxState::Pending),
                ("vera", MailboxState::Delivered),
                ("vera", MailboxState::Accepted),
                ("nolan", MailboxState::Delivered),
                ("unknown", MailboxState::Pending),
            ],
        );
        assert_eq!(pending, ["vera".to_owned()].into_iter().collect());
        assert_eq!(inbox["vera"], 2, "pending and delivered are both still in the inbox");
        assert_eq!(inbox["nolan"], 1, "a delivered row remains visible");
        assert_eq!(inbox["rhea"], 0, "an empty inbox is explicit");
        assert!(!inbox.contains_key("unknown"), "only roster people get card facts");
    }

    /// A surface with no actuator configuration says so, in a code a client can
    /// act on — it never answers an empty catalog. An empty catalog would be a
    /// *successful* answer meaning "nobody in this company may launch", which
    /// is a sentence this route must never be able to say by accident.
    /// An unreadable extension source refuses; it never defaults.
    ///
    /// The consequence of getting this wrong is the largest in the change: a
    /// digest that differs from the real one moves EVERY person's launch hash
    /// at once, so every pane's tag mismatches and the whole company is
    /// replaced in one pass. "I could not look" must not be spellable as a
    /// digest.
    #[test]
    fn an_unreadable_extension_source_refuses_rather_than_hashing_nothing() {
        let refusal = unreadable_extension_source();
        assert_eq!(refusal.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refusal.code(), "extension-source-unreadable");
        assert!(
            refusal.detail().contains("replace every pane"),
            "the refusal must name what it is preventing: {}",
            refusal.detail()
        );
    }

    #[test]
    fn a_surface_with_no_actuator_configuration_is_unavailable_not_an_empty_catalog() {
        let refusal = no_actuator_config();
        assert_eq!(refusal.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refusal.code(), "launch-catalog-unavailable");
        assert!(refusal.detail().contains("chiefd run"), "{}", refusal.detail());
    }

    /// THE ONE FACT CHIEFD KEPT ABOUT WHETHER ANYBODY IS CONVERGING IT.
    ///
    /// Reading the desired set IS the actuator saying it is here, and this is
    /// the assertion that the route records it. Without it a company whose
    /// whole tmux server died goes on reporting a healthy supervision pass and
    /// a headcount for as long as the daemon lives, which is what happened on
    /// 2026-08-18 for forty minutes.
    ///
    /// The stamp must land even when the read then REFUSES. This source has no
    /// manifest, so `desired_for` returns `unknown-company` — and an actuator
    /// that asked and was turned away is still an actuator that is here.
    /// Recording attendance only on success would blame an absent actuator for
    /// a fault of chiefd's own.
    #[tokio::test]
    async fn reading_the_desired_set_records_that_an_actuator_is_here() {
        use chiefd_core::runtime::attendance::ACTUATOR_LAPSE_MS;

        let dir = tempfile::tempdir().expect("tempdir");
        let manual = std::sync::Arc::new(chiefd_core::test_support::ManualClock::default());
        let clock: chiefd_core::clock::SharedClock = manual.clone();
        let company = std::sync::Arc::new(
            chiefd_core::actor::CompanyDb::open(
                "northstar-conformance",
                &dir.path().join("company.chief.db"),
                clock,
            )
            .expect("open company writer"),
        );
        let source = super::super::router::SupervisionLiveSource::new(
            company,
            "northstar-conformance".to_string(),
        );

        // Let the company go quiet for longer than the lapse window. Advanced,
        // never slept: `clippy.toml` bans both sleeps outright, and the whole
        // point of stamping off the company's own injected clock is that a
        // timing rule can be driven rather than waited out.
        manual.advance(std::time::Duration::from_millis(
            u64::try_from(ACTUATOR_LAPSE_MS * 10).expect("positive window"),
        ));
        let lapsed = source.clock_now();
        assert!(
            !source.actuator_attendance().attended(lapsed),
            "premise: nobody has read the desired set since this company booted"
        );

        let refused = org_runtime_desired(
            Extension(Some(source.clone())),
            Json(SlugRequest { slug: "northstar-conformance".to_string() }),
        )
        .await;
        assert!(refused.is_err(), "premise: a company with no manifest cannot answer");
        assert_eq!(
            source.actuator_attendance().last_read_ms(),
            lapsed,
            "the read must be recorded, at the company's own clock, even though it refused"
        );
        assert!(
            source.actuator_attendance().attended(lapsed),
            "an actuator that just read the desired set is attending this company"
        );
    }

    /// The catalog and the action stream must name the same company, or a
    /// client would start people from one company's homes against another's
    /// roster. Both derive it from the manifest this daemon owns.
    #[test]
    fn an_empty_catalog_still_names_its_company() {
        let catalog = LaunchCatalog::empty("cobalt");
        assert_eq!(catalog.company, "cobalt");
        assert!(
            catalog.refusal("chief").is_some(),
            "an uniterated person is still refused by name"
        );
    }
}
