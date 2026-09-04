//! Coverage for the launch catalog and the cycle's own notes.
//!
//! #751/P10 removed the `plan_cycle` cases: the projection→plan→sweep they
//! pinned produced an ordered walk of pane steps, and chiefd does not compute
//! one any more. What that walk decided — who should be running and what
//! should happen about it — is pinned against the
//! surviving mechanism in `chiefd_core::runtime::{desired, roster, actuation}`.
//! What is left here is the path/env conventions of `build_launch_catalog`, the
//! launch-subject notes, and the db/host integration cases below.

use std::path::PathBuf;
use std::time::Duration;

use chiefd_core::test_support::northstar_manifest;

use super::{build_launch_catalog, ActuatorConfig};

/// The retired pane env stamp, spelled here — in a test file — precisely
/// because production code may no longer name it. The name is the subject of
/// this assertion, so it has to be written down somewhere, and a test is the
/// only place it belongs (`scripts/test/no-chiefd-url-stamp.test.mjs`).
const STALE_INHERITED_CHIEFD_ENV: &str = "ORG_CHIEFD_URL";

const EPOCH: i64 = 1_784_116_800_000;

fn config() -> ActuatorConfig {
    ActuatorConfig {
        // A plain `String` since #751/P8-P10: `Socket` was the handle you passed
        // to a runtime command, and chiefd runs none. What survives is the
        // socket's identity — the value published into every person's env.
        socket: "cobalt-sock".to_owned(),
        // "watching for ever": the epoch, so an inferred quiet instant is
        // clamped by nothing and every expectation here is the pre-clamp one.
        watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
        dir: PathBuf::from("/work/anvils"),
        home: PathBuf::from("/home/operator"),
        pi_binary: PathBuf::from("/opt/pi/bin/pi"),
        floor: Duration::from_secs(5),
        launcher_root: PathBuf::from("/launcher"),
        root_pi_agent_dir: PathBuf::from("/registry"),
    }
}

/// Give one person the home [`build_launch_catalog`] gates on:
/// `<dir>/.chief/agent/<personId>/`, with the real `sessions/` and the `skills`
/// symlink `ensure_agent_home` writes.
///
/// The link is part of the fixture on purpose: the gate used to refuse any
/// symlink, so a fixture of plain directories would pass against the OLD rule
/// too and prove nothing about the change.
///
/// `clippy.toml` bans `std::fs::write` outside `chiefd_host::files` (the
/// filesystem-effects seam, README §5.6); fixture writes go through the
/// crate's own atomic-publish primitive instead.
fn admit_launch_subject(dir: &std::path::Path, person_id: &str) -> PathBuf {
    if person_id == "chief" {
        let key = crate::agent_home::chief_identity_key_path(dir);
        crate::materialize::publish_text(&key, "test-key", 0o600).expect("Chief key");
        return dir.to_path_buf();
    }
    let home = crate::agent_home::agent_home(dir, person_id);
    std::fs::create_dir_all(home.join("sessions")).expect("sessions");
    std::fs::create_dir_all(home.join(".pi").join("skills")).expect("project skills");
    std::os::unix::fs::symlink("../../../../skills", home.join(".pi/skills/worker"))
        .expect("the role skill install");
    // AND THE OPERATOR MUST REACH A PROVIDER. The gate asks whether the company
    // can reach a model at all, and since chief stopped redirecting
    // `PI_CODING_AGENT_DIR` it asks that of the OPERATOR's own directory rather
    // than of each home. An operator with neither file is a company of people
    // who cannot think, which is a different subject.
    let operator = operator_pi_agent_dir(dir);
    crate::materialize::publish_text(&operator.join("auth.json"), "{}", 0o600)
        .expect("the operator's credential");
    home
}

/// The operator's own Pi agent directory. Chief reads no route and no
/// credential out of it; an agent inherits it through PI'S OWN inheritance,
/// because chief no longer redirects the agent dir at all — so the fixture is a
/// directory, plus whatever the caller needs to RESOLVE through those links.
fn operator_pi_agent_dir(dir: &std::path::Path) -> PathBuf {
    let agent_dir = dir.join("pi-agent");
    std::fs::create_dir_all(&agent_dir).expect("operator agent dir");
    agent_dir
}

/// THE PANE HEADER CARRIES THE ROLE AND NEVER THE USERNAME.
///
/// Operator ruling: the username is how people are addressed, and it belongs
/// in ONE place. The header used to read `@vera · Head of Quant`, which put a
/// second identity in front of every reader while the footer showed a third
/// spelling — the person's kebab id — as though it were a handle. The header
/// is what you DO; the footer is who you ARE.
#[test]
fn the_pane_header_carries_the_role_and_never_the_username() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());
    let person = manifest.people_order[1].clone();
    admit_launch_subject(company_dir.path(), &person);

    let catalog = build_launch_catalog(&manifest, &config);
    let spec = catalog.people.get(&person).expect("an admitted person");

    let expected_role = crate::person_presentation::role(
        &manifest.people[&person].name,
        &manifest.people[&person].title,
        false,
    );
    assert_eq!(spec.display_name, expected_role, "the header is the role, alone");
    assert!(
        !spec.display_name.contains('@'),
        "and carries no username at all: {}",
        spec.display_name
    );
    assert!(
        !spec.display_name.contains(&person),
        "and certainly not the person id: {}",
        spec.display_name
    );
}

/// THE USERNAME IS PUBLISHED AS ITS OWN FACT, beside the id rather than
/// instead of it.
///
/// `ORG_LAUNCHER_PERSON` is the kebab id and addresses the mailbox and the
/// document store; it must not move. The pane footer needs the person's
/// HANDLE, and rendering `@` in front of the id — which is what it did — shows
/// the operator `@portfolio-management-head`. Two different facts, two
/// variables.
#[test]
fn the_launch_env_publishes_the_username_beside_the_person_id() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());
    let person = manifest.people_order[1].clone();
    admit_launch_subject(company_dir.path(), &person);

    let catalog = build_launch_catalog(&manifest, &config);
    let spec = catalog.people.get(&person).expect("an admitted person");
    let value = |name: &str| {
        spec.env.iter().find(|entry| entry.name == name).map(|entry| entry.value.clone())
    };

    assert_eq!(value("ORG_LAUNCHER_PERSON"), Some(person.clone()), "the KEY is unchanged");
    assert_eq!(
        value("ORG_LAUNCHER_PERSON_NAME"),
        Some(crate::person_presentation::handle(&manifest.people[&person].name)),
        "and the USERNAME travels beside it"
    );
    assert!(
        !value("ORG_LAUNCHER_PERSON_NAME").expect("the handle").contains('@'),
        "the variable carries the bare handle; the '@' is the renderer's"
    );
}

#[test]
fn build_launch_catalog_derives_paths_tools_and_env() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let person = "chief".to_owned();
    admit_launch_subject(company_dir.path(), &person);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());
    let catalog = build_launch_catalog(&manifest, &config);

    // The roster carries EVERY person the builder iterated, whether or not the
    // gate admitted them. It is what makes "not in the launch roster" sayable
    // on the client, which is a different failure from "the gate declined you".
    assert_eq!(catalog.roster, manifest.people_order);
    assert_eq!(catalog.company, manifest.slug);
    let spec = &catalog.people[&person];
    assert_eq!(catalog.refusal(&person), None, "an admitted person carries no refusal");
    // The Chief is the operator's own Pi. Its cwd is the company and its Pi
    // agent directory is the operator's normal one, with no override env.
    assert_eq!(spec.pi_home, config.root_pi_agent_dir.display().to_string());
    assert_eq!(spec.workspace, company_dir.path().display().to_string());
    assert!(!spec.env.iter().any(|entry| entry.name == "PI_CODING_AGENT_DIR"));
    assert!(!crate::agent_home::agent_home(company_dir.path(), &person).exists());
    assert_eq!(
        spec.accent.as_deref(),
        Some(crate::accent::CHIEF_EXECUTIVE_ACCENT),
        "the chief wears the fixed operator purple, not the palette slot it consumes"
    );
    assert_ne!(
        spec.accent.as_deref(),
        Some("#e24033"),
        "and specifically not the roster-position-0 red it used to take"
    );
    // `--tools` is derived from the person record (SA-2), not read back from a
    // retired `resources.json`: the declared grants plus the baseline and — for
    // the northstar CEO (an executive) — the manager and root-executive tools.
    assert_eq!(
        spec.tools,
        crate::converge_apply::resource_catalog::person_tool_names(&manifest.people[&person])
    );
    for tool in ["read", "org_send", "org_hire", "org_escalate_to_operator"] {
        assert!(spec.tools.iter().any(|granted| granted == tool), "{tool}");
    }
    assert_eq!(
        spec.extensions,
        [
            "/launcher/packages/piing/extensions/organization-intercom.ts",
            "/launcher/packages/piing/extensions/team-ui.ts",
            "/launcher/packages/piing/extensions/tribes-welcome.ts",
            "/launcher/packages/piing/extensions/company-stop.ts",
        ],
        "agent panes load the exact shipped organization extension set from the launcher root"
    );
    let has = |name: &str, value: &str| {
        spec.env.iter().any(|entry| entry.name == name && entry.value == value)
    };
    // AC6, both directions. The catalog must NOT carry the pane's PLACEMENT
    // — chiefd cannot see a display, and the operator client injects both
    // halves at spawn from the socket and session it is driving
    // (`chief-cli/src/actuate/spawn_cmd.rs::launch_command`). It must still
    // carry the pane's IDENTITY, which is chiefd's to state, so the assertion
    // that nothing is published is paired with one that proves the env plan is
    // populated at all.
    for placement in ["ORG_LAUNCHER_RUNTIME_SOCKET", "ORG_LAUNCHER_RUNTIME_SESSION"] {
        assert!(
            !spec.env.iter().any(|entry| entry.name == placement),
            "chiefd must not publish {placement}: {:?}",
            spec.env
        );
    }
    // ONE pointer to the company, and it names the company DIRECTORY — never
    // the `.chief` root beneath it. Asserted against the tempdir literally,
    // and separately against "does not end in .chief", because the two values
    // differ by one segment and an assertion built from `config.data_root()`
    // would have passed against the wrong one.
    assert!(has("ORG_LAUNCHER_ORG_DIR", &company_dir.path().display().to_string()));
    let stamped = spec
        .env
        .iter()
        .find(|entry| entry.name == "ORG_LAUNCHER_ORG_DIR")
        .expect("the pane is told where its company is");
    assert!(
        !stamped.value.ends_with(".chief"),
        "a pane told `<dir>/.chief` sends every reader that joins onto it \
         (chiefd-log's `<dir>/.chief/log`, the rendezvous at \
         `<dir>/.chief/run/daemon.json`) one level too deep: {}",
        stamped.value
    );
    // `ORG_LAUNCHER_DATA_ROOT` was asserted here beside it, carrying the
    // `.chief` root — the same fact plus one join, and two variables for one
    // directory is two things to keep in step.
    assert!(!spec.env.iter().any(|entry| entry.name == "ORG_LAUNCHER_DATA_ROOT"));
    assert!(has("HOME", "/home/operator"));
    assert!(!spec.env.iter().any(|entry| entry.name == "ORG_LAUNCHER_REGISTRY_ROOT"));
    assert!(has("ORG_LAUNCHER_PERSON", &person));
    // There is deliberately no PI_CODING_AGENT_DIR assignment for the Chief:
    // Pi must resolve the operator's normal agent directory itself.
    assert!(!spec.env.iter().any(|entry| entry.name == "PI_CODING_AGENT_DIR"));
    // BUG-8 regression pin: ORG_LAUNCHER_ROOT is authoritative from
    // ActuatorConfig, NEVER a best-effort passthrough of chiefd's own env. The
    // person's organization-intercom extension refuses to load without it
    // (`requiredEnvironment`), which is exactly how the first live chiefd
    // spawn died before its tagging step ("no such pane").
    assert!(has("ORG_LAUNCHER_ROOT", "/launcher"));
}

// TOMBSTONE: `build_launch_catalog_forwards_the_non_secret_reload_hard_contract_when_present`.
//
// It pinned that a pane was handed `ORG_LAUNCHER_RELOAD_HARD_CONTRACT`, read
// from `.organization-reload-hard-contract.json` inside the person's pi-home.
// That file was a RE-PROJECTION's receipt: it let a running agent tell whether
// an in-process `/reload` would change the tool grant its process started with.
// Nothing re-projects a home, so there is no reload to fence, no receipt to
// write, and no variable to forward.

/// Replaces `build_launch_catalog_keeps_a_generated_theme_for_a_non_standard_
/// identity`, whose subject went with the generated themes. The catalog carries
/// the COLOUR now, and the rule that matters is that it is the allocator's
/// answer for that person's position in the roster's `createdAt` order — the
/// same answer the browser's company tree is served, so a pane's chip and its
/// card cannot disagree.
#[test]
fn build_launch_catalog_carries_each_persons_allocated_accent() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let person = "quant-head".to_owned();
    admit_launch_subject(company_dir.path(), &person);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());

    let catalog = build_launch_catalog(&manifest, &config);
    let spec = &catalog.people[&person];

    let order = crate::accent::identity_accent_order(&manifest.people);
    assert_eq!(
        spec.accent,
        crate::accent::organization_person_accent(
            &order,
            manifest.chief_person_id().ok(),
            &person,
        )
        .ok(),
        "the allocator's answer, not a colour the catalog invented"
    );
    assert!(
        spec.accent.as_deref().is_some_and(|hex| hex.starts_with('#') && hex.len() == 7),
        "and it is a hex, not a path to a file holding one: {:?}",
        spec.accent
    );
}

#[test]
fn build_launch_catalog_never_publishes_a_chiefd_address_into_a_pane() {
    // #420 hop-2 ended here. A pane used to be handed a chiefd address as an
    // env stamp, and forwarding the daemon's INHERITED one was the split-brain
    // the stamp existed to prevent: in an isolated world the daemon's env
    // carries the PRELOAD chiefd's URL, and every pane it launched acked and
    // drained against the WRONG store, silently.
    //
    // Panes now resolve their own company from beacond (the test below), so
    // there is no address left for the actuator to publish and no inherited
    // value worth forwarding. This asserts the ABSENCE, with the worst case
    // staged deliberately: the actuator's own process environment names a
    // chiefd, and not one byte of it reaches the pane.
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let person = manifest.people_order.first().expect("a person").clone();
    admit_launch_subject(company_dir.path(), &person);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());

    std::env::set_var(STALE_INHERITED_CHIEFD_ENV, "http://127.0.0.1:61690");
    let env = build_launch_catalog(&manifest, &config).people[&person].env.clone();
    std::env::remove_var(STALE_INHERITED_CHIEFD_ENV);

    assert!(
        !env.iter().any(|entry| entry.name == STALE_INHERITED_CHIEFD_ENV),
        "the retired chiefd-address stamp must never be published into a pane again"
    );
    assert!(
        !env.iter().any(|entry| entry.value.contains("61690")),
        "no inherited chiefd address may reach a pane under any key"
    );
}

#[test]
fn build_launch_catalog_forwards_the_ca_trust_store_so_a_person_can_reach_a_provider() {
    // MEASURED on a zipbox host: provider egress is intercepted and TLS
    // terminated, so a process reaches `openrouter.ai` only by trusting
    // `/run/zipbox/ca-bundle.crt`. That path is exported by `/etc/profile.d`,
    // which a LOGIN shell reads and a person's Pi does not. Same key, same
    // URL, only difference the CA variable: `http=000` in 22ms without it,
    // `http=200` in 913ms with it.
    //
    // Every person in every company on that box printed `Error: Connection
    // error.` and then three consecutive provider failures, and the only place
    // it appeared was inside the pane. A whole company of people who cannot
    // think looks, from every surface chief owns, exactly like a healthy one.
    //
    // Not a credential, so Invariant 32 still holds: this names a path to a
    // PUBLIC trust store. It lets a process verify a server; it authenticates
    // nobody and impersonates nothing.
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let person = manifest.people_order.first().expect("a person").clone();
    admit_launch_subject(company_dir.path(), &person);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());

    let _guard =
        crate::converge_apply::BEACOND_URL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // All four travel together: Node reads the first, OpenSSL-linked clients
    // the second, curl the third, certifi consumers the fourth. Pi is one
    // process that may take several of those paths, so the one left behind is
    // the one that decides.
    let bundle = "/run/zipbox/ca-bundle.crt";
    for key in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "CURL_CA_BUNDLE", "REQUESTS_CA_BUNDLE"] {
        std::env::set_var(key, bundle);
    }
    let env = build_launch_catalog(&manifest, &config).people[&person].env.clone();
    for key in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "CURL_CA_BUNDLE", "REQUESTS_CA_BUNDLE"] {
        let forwarded = env.iter().find(|entry| entry.name == key).map(|entry| &entry.value);
        assert_eq!(
            forwarded.map(String::as_str),
            Some(bundle),
            "{key} must reach the pane or that person cannot complete a TLS handshake"
        );
        std::env::remove_var(key);
    }

    // Unset stays unset. A host that does not intercept egress needs none of
    // these, and an empty assignment would point a client at a trust store
    // that is not there — worse than the default it would otherwise use.
    //
    // READ THE LIMIT OF THIS WHOLE TEST BEFORE CITING IT AS PROOF A PANE CAN
    // REACH A PROVIDER. It sets the four variables ITSELF, three statements up,
    // and then asserts they are forwarded. That proves FORWARDING works. It
    // says nothing about the case that actually bites, because the arm below is
    // that case and it asserts the outage is correct: on a zipbox box egress IS
    // intercepted, `/run/zipbox/ca-bundle.crt` IS the CA that signs it, and
    // NOTHING exports any of these four — `NODE_EXTRA_CA_CERTS` and
    // `SSL_CERT_FILE` are unset in a shell that has sourced
    // `placeholders.env`. So production takes this arm, every pane gets no
    // trust store, and every person in the company dies at the TLS handshake
    // printing `Error: Connection error.` — which is indistinguishable from a
    // dead provider or an unfunded account, and in this session that
    // misdiagnosis reached the user, who added credit to an account the wallet
    // answered 200 for throughout.
    //
    // The premise "a host that does not intercept egress needs none of these"
    // is true, and it is not the premise this box runs under. Forwarding
    // assumes somebody upstream established trust; on zipbox nobody did.
    // Tracked at `issues/ca-bundle-trust-is-inherited-not-supplied.md` — the
    // fix is for chief to SET the bundle from disk when the host has not,
    // the same shape as `spawn_cmd`'s explicit `COLORTERM` and `f770c8463`'s
    // `COLORFGBG`. This assertion is left as it is on purpose: inverting it
    // before the launcher can supply the bundle would only make CI red about a
    // decision nobody has taken yet. It is annotated instead so the next reader
    // does not conclude, as this test's shape invites, that the trust path is
    // proven end to end. It is not; only the forwarding half is.
    let absent = build_launch_catalog(&manifest, &config).people[&person].env.clone();
    for key in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "CURL_CA_BUNDLE", "REQUESTS_CA_BUNDLE"] {
        assert!(
            !absent.iter().any(|entry| entry.name == key),
            "an unset {key} must not reach a pane as an empty assignment"
        );
    }
}

#[test]
fn build_launch_catalog_forwards_the_boxs_beacond_so_a_pane_can_find_its_own_company() {
    // #983: a pane resolves ITS OWN company's daemon from beacond instead of
    // being handed one address. beacond is one service per box, the same for
    // every company, so forwarding this daemon's own `BEACOND_URL` verbatim is
    // sound in a way forwarding a per-company chiefd address never was: a
    // wrong registry has never heard of the company and says so, where a wrong
    // daemon answers.
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let person = manifest.people_order.first().expect("a person").clone();
    admit_launch_subject(company_dir.path(), &person);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());

    let _guard =
        crate::converge_apply::BEACOND_URL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("BEACOND_URL", "http://127.0.0.1:47301");
    let forwarded = build_launch_catalog(&manifest, &config).people[&person]
        .env
        .iter()
        .find(|entry| entry.name == "BEACOND_URL")
        .map(|entry| entry.value.clone());
    assert_eq!(forwarded.as_deref(), Some("http://127.0.0.1:47301"));

    // Unset stays unset — the pane then uses beacond's own compiled-in
    // default, which is the right answer everywhere but a test box. An empty
    // assignment would instead give the client a malformed address to parse.
    std::env::remove_var("BEACOND_URL");
    let absent = build_launch_catalog(&manifest, &config).people[&person]
        .env
        .iter()
        .any(|entry| entry.name == "BEACOND_URL");
    assert!(!absent, "an unset BEACOND_URL must not reach a pane as an empty assignment");
}

#[test]
fn build_launch_catalog_omits_a_person_whose_materialization_is_missing() {
    // Fail-closed, not a panic and not a garbage launch spec: a person who
    // has not been materialized yet (or whose materialization is unreadable)
    // is simply absent from the catalog's admitted set. The client's
    // interpreter then refuses that person's start step by name, and the next
    // reconcile pass retries once materialization has caught up.
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());
    let catalog = build_launch_catalog(&manifest, &config);
    assert!(catalog.people.is_empty(), "nobody was materialized under the empty data root");
}

/// The property this packet exists to preserve: an absent person is a LOUD,
/// NAMED refusal carrying the daemon's own re-derived on-disk cause — never a
/// silent skip, and never the interchangeable "no launch spec" that once sent
/// an engineer hunting inside a function that was never called (#52).
///
/// The daemon is the only process that can say this: `chief-cli` cannot see
/// this data root, which is precisely why the catalog is published rather than
/// re-derived on the client.
#[test]
fn build_launch_catalog_names_why_each_unmaterialized_person_cannot_launch() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());

    let catalog = build_launch_catalog(&manifest, &config);

    // Every person was ITERATED, which is what distinguishes "the gate declined
    // you" from "you were never a candidate for lookup".
    assert_eq!(catalog.roster, manifest.people_order);
    for person_id in &manifest.people_order {
        let reason = catalog.refusal(person_id).expect("an unmaterialized person is refused");
        assert!(reason.contains(person_id.as_str()), "the refusal must name the person: {reason}");
        assert!(
            !reason.contains("not in the launch roster"),
            "an iterated person must never read as absent from the roster: {reason}"
        );
        // `explain_launch_refusal`'s re-derived cause, not a generic sentence.
        assert!(
            reason.contains("re-checked cause"),
            "the daemon owns the disk and must say what is missing: {reason}"
        );
    }
}

/// A materialized person and an unmaterialized one in the SAME catalog: the
/// first launches, the second is refused by name. A catalog that answered
/// "nobody" because one person was half-staged would read to an actuator as a
/// mandate to start nobody at all.
#[test]
fn build_launch_catalog_admits_the_materialized_and_refuses_the_rest_in_one_answer() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let materialized = "chief".to_owned();
    admit_launch_subject(company_dir.path(), &materialized);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());

    let catalog = build_launch_catalog(&manifest, &config);

    assert!(catalog.people.contains_key(&materialized));
    assert_eq!(catalog.refusal(&materialized), None);
    for person_id in manifest.people_order.iter().filter(|id| **id != materialized) {
        assert!(!catalog.people.contains_key(person_id));
        assert!(catalog.refusal(person_id).is_some(), "{person_id} must be refused by name");
    }
    let zero_counts: std::collections::BTreeMap<String, usize> =
        manifest.people_order.iter().cloned().map(|person| (person, 0)).collect();
    assert_eq!(
        catalog.inbox_counts, zero_counts,
        "every roster person gets an exact zero, including people the launch gate refused"
    );
}

#[test]
fn pending_mail_marks_only_the_admitted_person_named_by_the_builder_input() {
    let manifest = northstar_manifest(EPOCH);
    let company_dir = tempfile::tempdir().expect("tempdir");
    let with_pending_mail = manifest.people_order.first().expect("first roster person").clone();
    let without_pending_mail = manifest.people_order.get(1).expect("second roster person").clone();
    admit_launch_subject(company_dir.path(), &with_pending_mail);
    admit_launch_subject(company_dir.path(), &without_pending_mail);
    let mut config = config();
    config.dir = company_dir.path().to_path_buf();
    config.root_pi_agent_dir = operator_pi_agent_dir(company_dir.path());
    let pending = std::collections::BTreeSet::from([with_pending_mail.clone()]);

    let catalog = crate::converge_apply::build_launch_catalog_for_session_epoch(
        &manifest,
        &config,
        None,
        &std::collections::BTreeMap::new(),
        &pending,
    );

    assert!(
        catalog.people.get(&with_pending_mail).expect("pending person is admitted").pending_mail,
        "the pending set must create launch demand for its admitted person"
    );
    assert!(
        !catalog
            .people
            .get(&without_pending_mail)
            .expect("non-pending person is admitted")
            .pending_mail,
        "an admitted person outside the pending set must not get launch demand"
    );
}

/// The `reconcile.people.withheld` line must never render a reason list it does
/// not have as though it had one. Empty brackets shipped on a live company and
/// made the one diagnostic written for "why is this person not up" answer
/// nothing.
#[test]
fn a_withheld_person_with_no_demand_says_so_instead_of_printing_empty_brackets() {
    #[derive(Debug)]
    enum Reason {
        Requested,
        OrganizationRoot,
        HandoffRequired,
    }
    assert_eq!(
        super::withheld_note("execution-desk-ezra", &[] as &[Reason], &[]),
        "execution-desk-ezra[nothing-demanded-them]"
    );
    assert_eq!(
        super::withheld_note("quant-head", &[Reason::Requested, Reason::OrganizationRoot], &[]),
        "quant-head[Requested+OrganizationRoot]"
    );
    // Demand the operational filter removed is a reason of its own, and it
    // joins the decision's own reasons rather than replacing them.
    assert_eq!(
        super::withheld_note(
            "docs-jordan",
            &[] as &[Reason],
            &["pending-mail-but-not-operational"]
        ),
        "docs-jordan[pending-mail-but-not-operational]"
    );
    assert_eq!(
        super::withheld_note(
            "docs-jordan",
            &[Reason::HandoffRequired],
            &["pending-mail-but-not-operational"]
        ),
        "docs-jordan[HandoffRequired+pending-mail-but-not-operational]"
    );
}

/// chiefd may not render its own authorization as a headcount.
///
/// `reconcile.people.withheld` reported `active=5` every five seconds for forty
/// minutes on 2026-08-18 while the tmux server holding those five people was
/// gone. The COUNT was never wrong — it counts the people this pass decided may
/// run — but the word was: chiefd has been unable to count running people since
/// #751/P8-P10 deleted the actuator's reports, and a field that cannot mean what
/// it says must not say it. Whether anybody is converging the authorization at
/// all is the separate `runtime_unattended` fact.
///
/// # Why this reads the source instead of the log
///
/// The line is emitted inside `db.mutate`, which runs its closure on the writer
/// actor's own thread, and `tracing::subscriber::set_default` is THREAD-LOCAL —
/// a capturing subscriber installed by a test never sees it. Installing a
/// global one to reach it would make every other test in the process share it.
/// The field name is a compile-time literal, so `include_str!` pins it exactly
/// and cannot go stale against a moved file.
#[test]
fn the_withheld_line_names_authorization_and_never_a_headcount() {
    const SOURCE: &str = include_str!("../cycle.rs");
    assert!(
        SOURCE.contains("authorized = snapshot.people.values().filter(|d| d.active).count(),"),
        "the withheld line must report the count as AUTHORIZATION",
    );
    assert!(
        !SOURCE.contains("active = snapshot.people.values()"),
        "`active=` reads as a count of running people, which chiefd cannot produce",
    );
}

/// THE LAUNCH GATE OVER IDENTITY, driven end to end through the real condition.
///
/// Not a shape assertion over a hand-built refusal map: the conflicting
/// `identities` row here is written by the real actor, through the real
/// enrolment call, and is asserted to be the real
/// [`PersonEnrolment::RotationPending`] before the gate is ever asked. That is
/// the condition five people on a live company were in while chiefd handed each
/// of them a full launch spec about once a second, for ever.
///
/// Both directions, in ONE catalog: the conflicted person is absent from
/// `people` and named in `refusals`; the healthy person beside them is admitted
/// and untouched. A gate that withheld everybody would satisfy half of this and
/// be a worse bug than the one it replaced.
mod identity_gate {
    use std::sync::Arc;

    use chiefd_core::actor::CompanyDb;
    use chiefd_core::store::COMPANY_DB_FILENAME;
    use chiefd_core::test_support::{northstar_manifest, ManualClock};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    use crate::identity_enrolment::{enrol_person, identity_launch_refusals, PersonEnrolment};

    use super::{admit_launch_subject, config, operator_pi_agent_dir, EPOCH};

    const CONFLICTED: &str = "quant-head";
    const HEALTHY: &str = "signal-researcher";

    /// Write a person's identity key exactly where the hire path puts it.
    fn write_key(company_dir: &std::path::Path, person_id: &str, seed: u8) {
        let key = SigningKey::from_slice(&[seed; 32]).expect("key");
        let pem = key.to_pkcs8_pem(LineEnding::LF).expect("pem").to_string();
        let path = crate::agent_home::identity_key_path(company_dir, person_id);
        crate::files::publish_atomically(&path, &pem, 0o600).expect("write");
    }

    #[tokio::test]
    async fn a_person_whose_enrolled_key_no_longer_matches_their_own_is_withheld_by_name() {
        let manifest = northstar_manifest(EPOCH);
        let root = tempfile::tempdir().expect("tempdir");
        let company_dir = root.path().join("company");
        std::fs::create_dir_all(&company_dir).expect("company dir");
        let db = Arc::new(
            CompanyDb::open(
                &manifest.slug,
                &root.path().join(COMPANY_DB_FILENAME),
                Arc::new(ManualClock::starting_at(0, 1_700_000_000_000)),
            )
            .expect("open company"),
        );
        let mut config = config();
        config.dir = company_dir.clone();
        // The SAME directory `admit_launch_subject` signs Pi in to. The launch
        // gate reads the operator's own agent dir now rather than a link inside
        // each home, so a fixture that signs in somewhere else refuses both
        // people and the test would be about the gate instead of identity.
        config.root_pi_agent_dir = operator_pi_agent_dir(&company_dir);

        // Both people are fully materialized: the on-disk gate admits them, so
        // whatever the catalog does next is about identity and nothing else.
        // Distinct seeds: an `identities` fingerprint is globally unique, so
        // two people sharing a key is a state the store refuses outright.
        for (person_id, seed) in [(CONFLICTED, 7), (HEALTHY, 5)] {
            admit_launch_subject(&company_dir, person_id);
            write_key(&company_dir, person_id, seed);
            assert_eq!(enrol_person(&db, &company_dir, person_id).await, PersonEnrolment::Enrolled);
        }

        // THE CONDITION, produced by the real actor rather than described: a
        // different key in the home, and an enrolment that refuses to re-point
        // the anchor at it.
        write_key(&company_dir, CONFLICTED, 9);
        assert_eq!(
            enrol_person(&db, &company_dir, CONFLICTED).await,
            PersonEnrolment::RotationPending,
            "the fixture must reproduce the live conflict, not merely resemble it"
        );

        let refusals = identity_launch_refusals(
            &db,
            &company_dir,
            manifest.chief_person_id().expect("chief"),
            manifest.people_order.iter().cloned(),
        )
        .await;
        let catalog = crate::converge_apply::build_launch_catalog_for_session_epoch(
            &manifest,
            &config,
            None,
            &refusals,
            &std::collections::BTreeSet::new(),
        );

        // Withheld — and withheld with a sentence an operator can act on.
        assert!(
            !catalog.people.contains_key(CONFLICTED),
            "a person who cannot authenticate must not be handed a launch spec"
        );
        assert!(catalog.roster.iter().any(|id| id == CONFLICTED), "still a candidate, not absent");
        let reason = catalog.refusal(CONFLICTED).expect("withheld by NAME, never silently");
        assert!(reason.contains("rotation is explicit"), "{reason}");
        assert!(
            reason.contains(
                &crate::agent_home::identity_key_path(&company_dir, CONFLICTED)
                    .display()
                    .to_string()
            ),
            "the refusal must point at the key the operator has to fix: {reason}"
        );

        // The other direction, in the same answer.
        assert!(
            catalog.people.contains_key(HEALTHY),
            "an enrolled person beside them must be entirely unaffected"
        );
        assert_eq!(catalog.refusal(HEALTHY), None);
    }

    /// A key that cannot be parsed is the same class of stuck: nothing repairs
    /// it on its own, and the person authenticates to nothing until it is
    /// removed. A person with NO key yet is deliberately NOT withheld —
    /// provisioning mints one on this very pass, and withholding them would
    /// withhold the ordinary first boot of every new hire.
    #[tokio::test]
    async fn an_unusable_key_withholds_but_a_missing_one_does_not() {
        let manifest = northstar_manifest(EPOCH);
        let root = tempfile::tempdir().expect("tempdir");
        let company_dir = root.path().join("company");
        std::fs::create_dir_all(&company_dir).expect("company dir");
        let db = Arc::new(
            CompanyDb::open(
                &manifest.slug,
                &root.path().join(COMPANY_DB_FILENAME),
                Arc::new(ManualClock::starting_at(0, 1_700_000_000_000)),
            )
            .expect("open company"),
        );
        admit_launch_subject(&company_dir, CONFLICTED);
        admit_launch_subject(&company_dir, HEALTHY);
        crate::files::publish_atomically(
            &crate::agent_home::identity_key_path(&company_dir, CONFLICTED),
            "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n",
            0o600,
        )
        .expect("write");

        let refusals = identity_launch_refusals(
            &db,
            &company_dir,
            manifest.chief_person_id().expect("chief"),
            manifest.people_order.iter().cloned(),
        )
        .await;

        assert!(refusals[CONFLICTED].contains("unusable"), "{:?}", refusals[CONFLICTED]);
        assert!(
            !refusals.contains_key(HEALTHY),
            "a person awaiting their first mint is not withheld: provisioning mints it this pass"
        );
    }
}

// --- async full-cycle integration (reconcile_cycle) -------------------------

mod integration {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use chiefd_core::actor::{CompanyDb, MutationClass, MutationName};
    use chiefd_core::clock::SharedClock;
    use chiefd_core::runtime::duty_hooks::{ActuationMode, DutyContext, ReconcileActuator};
    use chiefd_core::store::activity::{self, LaunchFence, ReconcileInput};
    use chiefd_core::store::goal_delivery_quiesce_rows::GoalDeliveryQuiesce;
    use chiefd_core::store::mailbox::{self, MailboxEnvelope, Urgency};
    use chiefd_core::store::org_ops::{ShutdownKind, ShutdownOutcome};
    use chiefd_core::store::organization::{self, OrganizationManifest};
    use chiefd_core::store::{open_company_db, supervision, COMPANY_DB_FILENAME};
    use chiefd_core::test_support::{northstar_manifest, ManualClock};

    use crate::converge_apply::{
        reconcile_cycle, safety, ActivityProjectionInput, ActuatorConfig, ConvergeActuator,
    };
    use crate::gather::ReconcilerFactsStore;

    use super::EPOCH;

    fn open_db(dir: &std::path::Path, slug: &str) -> CompanyDb {
        let clock: SharedClock = Arc::new(ManualClock::default());
        CompanyDb::open(slug, &dir.join(COMPANY_DB_FILENAME), clock).expect("open company db")
    }

    async fn seed_company(db: &CompanyDb, manifest: OrganizationManifest) {
        db.mutate(MutationClass::Normal, MutationName("test.seed"), move |ledgers| {
            organization::create(ledgers, &manifest)?;
            supervision::seed(ledgers, &manifest)?;
            activity::seed(ledgers, &manifest)?;
            Ok(())
        })
        .await
        .expect("seed");
    }

    /// The agent's own report that it went QUIET -- the only thing that starts
    /// the settle countdown now that chiefd no longer stamps a clock from its
    /// own bookkeeping. Goes through the same writer verb the Pi extension
    /// calls (`org.activity.agent-state`), so these tests exercise the real
    /// seam rather than poking the ledger.
    async fn agent_settled(db: &CompanyDb, person: &str) {
        db.org_activity_note_agent_state(person.to_owned(), false)
            .await
            .expect("the settle report commits");
    }

    async fn activate(db: &CompanyDb, manifest: &OrganizationManifest, person: &str) {
        let manifest = manifest.clone();
        let person = person.to_owned();
        db.mutate(MutationClass::Reconcile, MutationName("test.activate"), move |ledgers| {
            let supervision = supervision::read(ledgers, &manifest)?;
            activity::reconcile(
                ledgers,
                &manifest,
                &supervision,
                &ReconcileInput {
                    launch_intent: LaunchFence::Unfenced,
                    requested_person_ids: vec![person.clone()],
                    watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
                },
            )?;
            Ok(())
        })
        .await
        .expect("activate");
    }

    // --- the actuator's committed report ------------------------------------
    //
    // TOMBSTONE: the observation harness, and every test whose subject it was.
    //
    // `publish_report`, `reported`, `observed`, `observed_nobody`,
    // `observed_untrusted`, `observed_with_dead` and `wall_now_ms` are
    // deleted, together with
    // `an_untrusted_report_withholds_every_action_and_is_never_read_as_absence`,
    // `a_reported_dead_process_is_not_running_and_is_started`,
    // `an_externally_seeded_budget_override_lets_the_over_budget_plan_apply` and
    // `an_over_budget_respawn_plan_drains_on_the_next_pass_through_the_real_cycle`.
    //
    // These pinned two different things, and only one of them is a ruling.
    //
    // THE BUDGET TESTS lost their subject outright: `destructive_budget` and
    // the override are deleted, so there is no cap for a plan to exceed.
    //
    // THE TWO OBSERVATION TESTS are the ones worth being careful about, because
    // the first pinned THE LOAD-BEARING RULE of this entire branch: an actuator
    // that looked and cannot vouch for what it saw must be retained, never read
    // as proven absence, since "untrusted, and here are zero people" reading as
    // "nothing is running" is a mandate to start a whole company a second time
    // on top of one already up. That rule is not weakened. It is made
    // unreachable: chiefd receives no report, so there is no untrusted report to
    // misread and no `WithheldReason::ObservationUntrusted` to assert. The rule
    // now lives one layer out, in `chief-cli`'s `Applied::blocked` versus
    // `Applied::none` -- "I could not look" and "there was nothing to do" are
    // still different sentences, in the only process that can look.
    //
    // The dead-process test's rule moved rather than died: a person whose pane
    // is gone is started because the actuator diffs the desired set against the
    // panes in front of it. `chief-cli`'s
    // `boots_every_missing_pane_in_one_pass_with_no_ramp_at_all` is the same
    // property asserted where it now happens.
    //
    // The remaining tests in this file kept their subjects (stop facts, session
    // teardown, admission debt, authorization) and simply lost their `observed`
    // SETUP lines, which established a fact the cycle no longer reads.

    /// Whether this pass asked for anybody to be brought up. `launch_subjects`
    /// names `Start`/`Restart` subjects and never a `Stop`, so the absence of
    /// this note is the report-level proof that the pass's actions were stops —
    /// the surviving form of "no `new-session`/`respawn-pane` ran".
    fn launched(report: &chiefd_core::runtime::duty_hooks::ReconcileReport) -> Option<&str> {
        report.notes.iter().find(|note| note.starts_with("launching:")).map(String::as_str)
    }

    /// Whether this pass DESIRES `person`.
    ///
    /// The note names the desired SET, not a list of starts, so "is this person
    /// desired" is the question every settle/shrink/teardown assertion below is
    /// really asking. It is stronger than the counts it replaces: a count of
    /// zero and a count of one both pass while naming the wrong person.
    fn desires(report: &chiefd_core::runtime::duty_hooks::ReconcileReport, person: &str) -> bool {
        launched(report).is_some_and(|note| note.contains(person))
    }

    fn config(dir: &std::path::Path) -> ActuatorConfig {
        ActuatorConfig {
            socket: "northstar-sock".to_owned(),
            // "watching for ever": the epoch, so an inferred quiet instant is
            // clamped by nothing and every expectation here is the pre-clamp one.
            watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
            dir: dir.to_path_buf(),
            home: dir.join("home"),
            pi_binary: std::path::PathBuf::from("/opt/pi/bin/pi"),
            floor: std::time::Duration::from_millis(0),
            launcher_root: std::path::PathBuf::from("/launcher"),
            root_pi_agent_dir: dir.join("pi-agent"),
        }
    }

    #[tokio::test]
    async fn a_shadow_cycle_plans_but_actuates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        // A live actuator that proves the runtime is empty: without a present
        // lease the stream would withhold for `NoActuator` and this test would
        // never reach the mode gate it is about.

        // Company config defaults to Shadow, so even a daemon Apply request is
        // downgraded to shadow.
        let report = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::fenced(["signal-researcher".to_owned()]),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("cycle");

        assert!(!report.applied, "shadow config downgrades the apply request");
        // RE-RULED TWICE, and the second time back toward the original. It
        // first pinned `desired_people > 0` in shadow (chiefd computed the whole
        // pane walk and did not run it), then `== 0` on the reading that shadow
        // means chiefd "asks for nothing". Both were about an ACTION STREAM
        // that no longer exists. Under a desired SET the honest answer is
        // neither: the set is published IN FULL on every held path, because an
        // operator running a shadow diff needs to see exactly what would happen
        // when it resumes -- and the HOLD is what says do not act on it.
        assert!(
            report.desired_people > 0,
            "a shadow pass still publishes the whole desired set, or the diff shows nothing"
        );
        assert!(
            report.notes.iter().any(|note| note.contains("actuation held: Shadow")),
            "the withheld reason is an operator-facing state, never a silence: {:?}",
            report.notes
        );
        // TOMBSTONE: two argv assertions ("no `new-session` in shadow", no
        // `kill-pane`) pinned the emitted terminal command list. chiefd emits
        // none — there is no runner to record calls on.
    }

    #[tokio::test]
    async fn an_empty_plan_is_a_no_op_that_skips_apply_and_keeps_the_breaker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        // Nobody activated -> empty desired. Opt the company into Apply.
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("set apply");

        // No launch-intent source wired: the fence projection is skipped and
        // the cycle plans from the committed (all-idle) activity ledger.
        let report = reconcile_cycle(&db, &config(dir.path()), ActuationMode::Apply, None)
            .await
            .expect("cycle");

        assert!(report.applied, "apply config + apply request: the pass is in Apply mode");
        assert_eq!(
            report.desired_people, 0,
            "nobody desired and a trusted empty runtime: there is nothing to ask for"
        );
        // TOMBSTONE: this asserted the pass never reached the
        // `converge.before_apply` pause point. There is no apply path and no
        // such pause point — `converge.before_sweep` is the only one left.
        // The no-op leaves the company in Apply (the breaker is untouched).
        assert!(matches!(
            safety::read_safety_config(&db).actuation_mode,
            safety::ActuationMode::Apply
        ));
    }

    // TOMBSTONE: `genesis_ceo_admission_persists_a_watermark_before_any_runtime_observation`.
    //
    // It asserted that a genesis pass wrote a minimal `starting` runtime row
    // carrying `startup_admission_until`, so the next authorized department
    // started behind the CEO's ramp slot. Both ends of that sentence are gone:
    // the ramp is deleted by operator ruling (everything missing boots at once)
    // and with it `admission_runtime_bootstrap`, the only writer of that row on
    // this path. There is no watermark to persist and nothing that would read
    // one. The durable column and its EXPLICIT mutator survive and are covered
    // by `consuming_ceo_admission_debt_clears_only_that_runtime_field`.

    // TOMBSTONE: `expired_ceo_admission_debt_is_not_reinterpreted_as_a_wall_clock_offset`.
    //
    // It drove `startup_admission_ramp`, whose whole output was a `RampConfig`
    // for pacing spawns. The ramp is deleted by operator ruling -- the actuator
    // boots every missing pane in one pass -- so there is no schedule for an
    // expired deadline to become an offset in. Not weakened: the function and
    // the type it returned are both gone, and the durable column it read is
    // still covered by `consuming_ceo_admission_debt_clears_only_that_runtime_field`
    // below.

    /// #367, restored: a second pass over an UNCHANGED desired set writes
    /// nothing.
    ///
    /// The gate used to be `planned_steps > 0` -- how many ACTIONS the pass
    /// emitted -- and a converged company emitted none. Under a desired SET
    /// every live company has people in it on every pass, so that same
    /// expression is permanently true and the audit row was opened and closed
    /// twice per pass, for ever, on every company in the fleet. This is the
    /// test that expression could never have failed.
    #[tokio::test]
    async fn an_unchanged_desired_set_writes_no_second_audit_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        // A company that was CREATED, so somebody is asked for. The set has to
        // be non-empty for the second assertion below to mean anything, and
        // since #1148 a company nobody asked for desires nobody -- not even the
        // root, whose unconditional lease used to make every fixture non-empty
        // for free.
        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let actuator = ConvergeActuator::new(Arc::clone(&db), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let first = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("first pass");
        assert!(first.changed, "the first pass records the set it just published");

        // Nothing about the company moved between the two passes.
        let second = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("second pass");
        assert!(
            !second.changed,
            "an identical desired set is not news and must record nothing: {second:?}"
        );
        assert!(
            second.desired_people > 0,
            "and it is NOT quiet because the company emptied -- the set is still full: {second:?}"
        );
    }

    /// The audit body is adopted only after the write COMMITS.
    ///
    /// Adopting first is the tempting shape and it loses data: a write that
    /// fails would leave the next pass believing the change had already been
    /// recorded, so the audit row for the one pass an operator would go looking
    /// for is dropped -- silently, and until the body happens to change again.
    #[test]
    fn a_failed_audit_write_leaves_the_next_pass_still_seeing_a_change() {
        let audit = super::super::LastAudit::default();
        let body = chiefd_core::runtime::converge_intent::ConvergeIntentBody {
            shadow: false,
            sweep_live: false,
            predicted_kill_panes: 0,
            predicted_respawn_persons: 0,
            pointer_clears: 0,
            steps: vec!["desired chief @hash".to_owned()],
        };

        // A pass that asked the question and then FAILED to write.
        assert!(audit.differs(&body), "the first pass sees a change");
        // ...no `adopt`, exactly as the `?` on the write leaves it.
        assert!(audit.differs(&body), "so the next pass sees it too, and retries the record");

        audit.adopt(body.clone());
        assert!(!audit.differs(&body), "once recorded, an identical body is not news");
    }

    // TOMBSTONE (chief-home-is-cwd §4c):
    // `consuming_ceo_admission_debt_clears_only_that_runtime_field` stood here
    // and pinned that `runtime_clear_startup_ceo_admission_debt` consumed the
    // one-shot CEO admission debt without disturbing `startup_admission_until`.
    // The debt was incurred only when the DAEMON admitted the CEO on its own
    // boot; the daemon boots no pane, so column, writer and test are deleted.

    #[tokio::test]
    async fn a_newly_authorized_person_is_admitted_in_the_same_pass() {
        // RUNTIME-SINGLE-WRITER-P2: a person newly named by the fence is
        // immediate daemon demand, while an already desired-active person
        // remains ordinary recovery demand in the same action stream.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        // it-head's demand is settled (desired-active in the committed
        // snapshot) and its process is up; signal-researcher is named by the
        // fence for the FIRST time this pass and is not running.
        activate(&db, &manifest, "it-head").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("set apply");

        let report = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::fenced(["signal-researcher".to_owned(), "it-head".to_owned()]),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("cycle");

        // The count is WHO IS DESIRED, not who is being started: this pass
        // desires the CEO, the newly authorized person and their department
        // head. "Exactly the one person who is down" was a fact about a
        // TRANSITION, and only the actuator can compute one. The real subject
        // -- the new authorization reached the desired set in the SAME pass --
        // is asserted by name, which a count would also pass with the wrong
        // person in it.
        assert!(
            desires(&report, "signal-researcher"),
            "the newly authorized person is desired in the same pass: {report:?}"
        );
        // TOMBSTONE: this pinned that a `new-session`/`new-window`/
        // `split-window` argv reached the runner. chiefd emits no argv; the
        // surviving statement of the same fact is the named launch subject.
        let note = launched(&report).expect("the same-pass launch note names its subject");
        assert!(note.contains("signal-researcher"), "{note}");
        assert!(
            !report.notes.iter().any(|note| note.contains("quiet lease")),
            "the obsolete TypeScript-writer quiet lease is absent: {:?}",
            report.notes
        );
    }

    async fn activate_many(db: &CompanyDb, manifest: &OrganizationManifest, people: &[&str]) {
        let manifest = manifest.clone();
        let ids: Vec<String> = people.iter().map(|s| (*s).to_string()).collect();
        db.mutate(MutationClass::Reconcile, MutationName("test.activate_many"), move |ledgers| {
            let supervision = supervision::read(ledgers, &manifest)?;
            activity::reconcile(
                ledgers,
                &manifest,
                &supervision,
                &ReconcileInput {
                    launch_intent: LaunchFence::Unfenced,
                    requested_person_ids: ids.clone(),
                    watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
                },
            )?;
            Ok(())
        })
        .await
        .expect("activate many");
    }

    /// Regression for #369 (the ~2-day tribes-capital livelock): the
    /// destructive budget used to sum kills + respawns, so it was tightest —
    /// the floor of 2 — exactly when the fleet had to shrink all the way to
    /// empty. A refusal changed nothing on the host, so the identical
    /// over-budget plan replanned every ~30s forever — `budget exceeded:
    /// refused and escalated`, live, 2026-07-21 to 2026-07-23. Stops are now
    /// exempt: this exact shape (empty desired, three processes running — far
    /// over the old floor of 2) must ask for the whole shrink on the very first
    /// pass, no refusal, no escalation, nothing deferred to a pass that would
    /// never arrive.
    #[tokio::test]
    async fn shrinking_to_empty_actuates_in_full_even_far_over_the_old_kill_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let report = reconcile_cycle(&db, &config(dir.path()), ActuationMode::Apply, None)
            .await
            .expect("cycle");

        assert!(report.applied, "shrink-to-empty is never budget-blocked: {:?}", report.notes);
        // A company that desires nobody is stopped OUTRIGHT: one `StopAll`,
        // never a list of per-person stops that would race a client's own
        // teardown. The old fixture's three kills are that one action now.
        // A SHRINK IS AN ABSENCE. There is no stop verb to count and no budget
        // to exempt: the removed people are simply not in the desired set, and
        // that absence is the instruction the actuator acts on. This fixture
        // shrinks to EMPTY, so nobody is desired at all.
        assert_eq!(
            report.desired_people, 0,
            "the shrink leaves nobody desired: {:?}",
            report.notes
        );
        assert_eq!(launched(&report), None, "and nobody is named: {:?}", report.notes);
        assert!(
            !report.notes.iter().any(|note| note.contains("budget")),
            "no budget note at all: stops are exempt, not merely under-budget: {:?}",
            report.notes
        );

        // No escalation either — there was nothing over budget to escalate.
        let escalated = db.read(|snapshot| {
            let manifest = organization::read(snapshot).expect("org");
            let supervision = supervision::read(snapshot, &manifest).expect("supervision");
            supervision.effect_order().iter().any(|id| {
                id.starts_with("reconcile-escalation:") && id.ends_with(":budget_exceeded")
            })
        });
        assert!(!escalated, "shrink never trips the budget escalation");

        // And the breaker stays healthy: a successful pass, not a refusal.
        assert!(matches!(
            safety::read_safety_config(&db).actuation_mode,
            safety::ActuationMode::Apply
        ));
    }

    // --- arch-audit H2, Step 7a: committed stop fact is a first-class fact ---

    /// The runtime row exactly as `stopOrganizationRuntimeUnlocked`
    /// (org-runtime.ts) commits it on an explicit company stop — the ONLY
    /// writer of `status: "stopped"`.
    fn runtime_row(status: &str) -> chiefd_core::store::runtime_rows::RuntimeState {
        chiefd_core::store::runtime_rows::RuntimeState {
            version: 1,
            organization: None,
            observed_at: "2026-07-31T00:00:00.000Z".to_owned(),
            session: None,
            socket_name: "northstar-sock".to_owned(),
            status: status.to_owned(),
            startup_admission_until: None,
            recovery_fingerprint: None,
            recovery_observed_at: None,
            recovery_confirmed: None,
            recovery: None,
            reconciliation: None,
            process_handles: std::collections::BTreeMap::new(),
            monitor_warnings: Vec::new(),
            missing_durable_person_ids: Vec::new(),
            unexpected_observed_person_ids: Vec::new(),
            extra: std::collections::BTreeMap::new(),
        }
    }

    /// Gap 2 closed: the launcher committed the stop fact (intent withdrawn —
    /// here the empty fence — and `runtime.status = "stopped"`) but its own
    /// teardown never ran. The duty daemon must READ that committed fact and
    /// name it, on this pass.
    ///
    /// #751/P8-P10 changed what "acting on it" means. chiefd cannot tear a
    /// session down; the operator-facing consequence of the stop fact is the
    /// note it puts on the pass, and the three tests here are a positive-
    /// evidence trio: only a committed `"stopped"` produces it.
    #[tokio::test]
    async fn a_committed_stop_fact_tears_down_the_owned_session_the_launcher_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.runtime_publish(runtime_row("stopped")).await.expect("commit the stop fact");

        let report = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            // The stop withdrew everyone's launch intent — a CEO-only company
            // fences the EMPTY set (the CEO is root-desired regardless, which
            // is exactly why intent withdrawal alone could never tear down).
            Some(ActivityProjectionInput {
                fence: LaunchFence::deny_all(),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("cycle");

        assert!(report.applied, "the teardown pass applies: {:?}", report.notes);
        // TOMBSTONE: `ran_verb("kill-session")`. chiefd emits no argv.
        assert!(
            report.notes.iter().any(|note| note.contains("company runtime is stopped")),
            "the committed stop fact is named on the pass: {:?}",
            report.notes
        );
        // A STOP EMPTIES THE DESIRED SET. It is not a hold: a hold says "do not
        // act on this set" and leaves the company running, while a stop says
        // chiefd desires nobody -- and absence is what takes them down.
        // Publishing the full set here would have the actuator boot, on its very
        // next pass, the company somebody had just switched off.
        assert_eq!(
            report.desired_people, 0,
            "a stopped company desires nobody: {:?}",
            report.notes
        );
        assert_eq!(launched(&report), None, "a stopped company brings nobody up");
        let runtime = db.runtime_read().await.expect("read runtime").expect("row").0;
        assert_eq!(
            runtime.status, "stopped",
            "an observation may never overwrite the operator's stop intent"
        );
    }

    /// Positive evidence, the other direction: an identical live topology with
    /// NO committed stop fact (no runtime row at all) is never a stop — absence
    /// is not a stop.
    #[tokio::test]
    async fn an_absent_stop_fact_never_tears_the_session_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let report = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::deny_all(),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("cycle");

        assert!(
            !report.notes.iter().any(|note| note.contains("company runtime is stopped")),
            "no committed stop fact => the company is never reported stopped: {:?}",
            report.notes
        );
        assert!(
            db.runtime_read().await.expect("read runtime").is_none(),
            "the observation publish never MINTS a runtime row (rule 4, never insert)"
        );
    }

    /// A LIVE runtime status (`running`) is equally not a stop: the same
    /// observation, the same empty fence, but the committed row names a
    /// running company.
    #[tokio::test]
    async fn a_running_runtime_row_never_tears_the_session_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.runtime_publish(runtime_row("running")).await.expect("commit a live status");

        let report = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::deny_all(),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("cycle");

        assert!(
            !report.notes.iter().any(|note| note.contains("company runtime is stopped")),
            "a live runtime status is never read as a stop: {:?}",
            report.notes
        );
        let runtime = db.runtime_read().await.expect("read runtime").expect("row").0;
        assert_eq!(runtime.status, "running", "the observed status is `running`, not `stopped`");
    }

    /// #823/E8-S1 — the regression team-lead's ruling explicitly asked for: a
    /// crashed runtime (the actuator proves nothing is running) must NOT be
    /// reported as `"stopped"`, or the NEXT pass's `company_stopped` read
    /// collapses the desired topology and chiefd never retries the crash —
    /// supervision quietly giving up on exactly the failure it exists to
    /// handle. `"stopped"` is an INTENT the operator expressed, not an
    /// observation anybody made, and this pins that a converge-pass observation
    /// is never allowed to mint one.
    #[tokio::test]
    async fn a_crashed_session_is_not_reported_as_stopped_and_the_next_pass_still_replans_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = open_db(dir.path(), &manifest.slug);
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.runtime_publish(runtime_row("running"))
            .await
            .expect("commit a live status before the crash");
        // The actuator looked and PROVED the runtime empty — exactly what a
        // crashed runtime server looks like to a client that can still talk to
        // its own box. The fence NAMES THE ROOT, which is the shape a created
        // company actually has: `prepare_ceo_only` writes that row at genesis
        // and at every attach.
        //
        // It used to read `LaunchFence::Unfenced`, chosen so the projection ran
        // at all (passing `None` skips it entirely, which would empty the
        // desired set for a reason unrelated to this claim). `Unfenced` served
        // that purpose only because the CEO's unconditional lease supplied the
        // demand; #1148 deleted the lease, and `Unfenced` deliberately
        // contributes no names of its own, so it now produces the very empty
        // set this test exists to rule out — a false green, for a reason that
        // has nothing to do with crashes.

        let report = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::fenced(["chief".to_owned()]),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("cycle");

        assert!(
            report.desired_people > 0,
            "the desired topology must NOT collapse to empty after a crash: {:?}",
            report.notes
        );

        let runtime = db.runtime_read().await.expect("read runtime").expect("row exists").0;
        // The claim is the polarity, not the literal: whatever positive-evidence
        // word this pass publishes for a proven-empty runtime, it may never be
        // the operator's `"stopped"` intent. Asserting the exact string would
        // pin a spelling this test does not own — the row's own CHECK does.
        assert_ne!(
            runtime.status, "stopped",
            "a crashed runtime must never be reinterpreted as an operator stop"
        );
    }

    // --- the activity-fence projection (normalized launch-intent wired) -----
    //
    // The live bug this wires shut: the CEO staffs people through the normal
    // product path, which writes normalized launch-intent rows, but the
    // chiefd-native activity ledger was never re-projected, so every staffed
    // person planned as a stop. These tests run the REAL `ConvergeActuator`
    // against a real normalized `org.sqlite` fixture so the whole chain — typed
    // row -> LaunchFence -> activity::reconcile -> desired roster -> action
    // stream — is exercised, not a stand-in.

    /// Write the normalized launch-intent rows consumed by the projection and
    /// hand back the read-only adapter over that shared `org.sqlite`.
    fn normalized_intent_store(
        dir: &std::path::Path,
        data_root: &std::path::Path,
        manifest: &OrganizationManifest,
        person_ids: &[&str],
    ) -> ReconcilerFactsStore {
        let path = dir.join("org.sqlite");
        let conn = open_company_db(&path).expect("open writable fixture");
        for person_id in person_ids {
            conn.execute(
                "INSERT INTO launch_intent(slug, person_id) VALUES(?1, ?2)",
                rusqlite::params![manifest.slug, person_id],
            )
            .expect("insert launch-intent row");
        }
        // `updatedAt` is derived from the event feed. A non-empty timestamp is
        // part of the fence's fail-safe validation contract.
        conn.execute(
            "INSERT INTO org_events(slug, seq, entity, entity_id, op, at) \
             VALUES(?1, 1, 'launch-intent', 'fixture', 'upsert', ?2)",
            rusqlite::params![manifest.slug, "2026-07-22T00:00:00.000Z"],
        )
        .expect("insert launch-intent event");
        drop(conn);
        ReconcilerFactsStore::new(path, data_root.to_string_lossy().to_string())
    }

    /// The launch-intent rows a company that was CREATED actually has: the
    /// CEO's start decision, plus whoever else has been staffed since.
    ///
    /// # Why the root is in here now
    ///
    /// Until #1148 `activity::reconcile` handed the CEO an unconditional
    /// `OrganizationRoot` lease, so the root ran whether or not anything had
    /// asked for it and no fixture ever needed to say so. The operator's
    /// "everybody settles" ruling deleted that lease, and `active` is now
    /// derived purely from demand — so a company nobody asked for is a company
    /// with nobody in it, root included.
    ///
    /// The product supplies that demand at the two moments an operator arrives:
    /// `org_ops::prepare_ceo_only` inserts the CEO's launch-intent row, and both
    /// genesis and `chief attach` call it. Every fixture below that models an
    /// EXISTING company is therefore modelling one that has been through
    /// genesis, and must carry the row genesis wrote. Naming the root here is
    /// not a re-exemption in a helper — it is demand, of exactly the kind any
    /// other person needs, and it lapses and settles on the same terms.
    ///
    /// A fixture whose subject is the ABSENCE of any start decision must use
    /// [`normalized_intent_store`] directly and assert that nobody is desired.
    fn post_genesis_intent_store(
        dir: &std::path::Path,
        data_root: &std::path::Path,
        manifest: &OrganizationManifest,
        person_ids: &[&str],
    ) -> ReconcilerFactsStore {
        let mut with_root = vec!["chief"];
        with_root.extend_from_slice(person_ids);
        normalized_intent_store(dir, data_root, manifest, &with_root)
    }

    fn duty_ctx(db: &CompanyDb, slug: &str) -> DutyContext {
        DutyContext { slug: slug.to_owned(), snapshot: db.snapshot() }
    }

    fn desired_active(db: &CompanyDb, person_id: &str) -> Option<bool> {
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let activity = activity::read(snapshot, &org).expect("activity");
            activity.people.get(person_id).map(|state| state.last_desired_active)
        })
    }

    #[tokio::test]
    async fn a_staffed_person_in_launch_intent_is_adopted_with_no_kill_planned() {
        // Requirement (1). The CEO started signal-researcher (launch intent
        // names them; their process is up), so the
        // cycle's fence projection must keep them desired-active and the pass
        // must ADOPT what is running: it asks for NOTHING — never a stop, a
        // restart, or a start.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await; // the CEO's start-person
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store =
            normalized_intent_store(dir.path(), dir.path(), &manifest, &["signal-researcher"]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert!(report.applied, "apply config + apply request");
        // #367: adopting an already-converged runtime asks for NOTHING — the
        // steady state is an empty action stream (no per-pass churn).
        // TOMBSTONE: an eight-verb argv loop (kill-pane / respawn-pane /
        // kill-session / new-session / new-window / set-option / select-layout /
        // move-window) pinned the same claim against a runner that no longer
        // exists. An empty action stream IS that claim, stated once.
        // A converged company still DESIRES its people -- that is the whole
        // difference between a desired set and an action stream. "Asks for
        // nothing" now means it actuates nothing.
        assert!(desires(&report, "signal-researcher"), "the adopted person stays desired");
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "the fence projection keeps the staffed person desired-active"
        );
    }

    /// The store's LABEL and the manifest's SLUG are different strings, and the
    /// converge reader must look rows up by the label.
    ///
    /// This used to be an edge case: the label was `<slug>@<rootHash>` and
    /// differed from the manifest slug only when an operator shared one orgs
    /// root. It is now the NORMAL case and cannot be otherwise — a company is
    /// labelled by its directory key, twelve hex characters that carry no name,
    /// while the manifest keeps the name a person typed. Reading rows by the
    /// manifest slug therefore finds nothing at all rather than finding the
    /// wrong thing occasionally, so a successfully published intent becomes an
    /// empty CEO-only fence and the pass asks for an erroneous stop.
    #[tokio::test]
    async fn the_store_label_and_not_the_manifest_slug_reads_the_committed_launch_intent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        // THE one definition of a company key, not a lookalike literal: a
        // fixture that spells its own would keep passing after the real one
        // changed shape.
        let row_slug = host_primitives::rendezvous::company_key(dir.path());
        assert_ne!(row_slug, manifest.slug, "the label must not be the name");
        let clock: SharedClock = Arc::new(ManualClock::default());
        let db = Arc::new(
            CompanyDb::open(&row_slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open a directory-keyed company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit public-route equivalent launch intent");

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("the committed launch intent is visible to the live actuator");

        // TOMBSTONE: `!ran_verb("kill-pane")`. An empty action stream is the
        // same statement without an argv to inspect.
        assert!(
            desires(&report, "signal-researcher"),
            "the committed fence keeps the staffed person desired"
        );
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "a label that is not the name never degrades a valid explicit start into the empty fence"
        );
    }

    #[tokio::test]
    async fn a_person_removed_from_launch_intent_becomes_desired_inactive_and_is_torn_down() {
        // Requirement (2) — the symmetric half. Intent withdrawn (stop-person
        // or an idle park) must end with the person asked to stop.
        //
        // THE TEARDOWN IS NOW IMMEDIATE, and that is the ruling rather than an
        // accident. This test used to need TWO passes: pass 1 asserted
        // `desired_people == 0` because `bounded_idle_retention` kept the
        // process alive to serve out a quiet lease, and only pass 2 — after the
        // lease expired — asked for the stop. That retention is deleted.
        // chiefd declares the final state, the actuator makes it true, and the
        // agent resumes from its transcript as if it had crashed; there is no
        // "let them finish", and the wait could never have completed anyway
        // because a routine idle park has no releaser. A single stop is exempt
        // from the destructive budget, so this genuinely converges.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await; // was running
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &[]); // only the root is asked for
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // ONE PASS. Desired flips inactive and the action stream asks for the
        // de-authorized process to stop, in the same pass that sees the
        // withdrawal — no lease to serve, no handoff window to wait inside.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("settle pass");

        assert!(report.applied, "one correct stop is not a budget refusal: {:?}", report.notes);
        // TOMBSTONE: `ran_verb("kill-pane")`, on every pass. The surviving
        // statement is the count plus the absence of a launch note, which
        // together say "exactly one action, and it brings nobody up".
        assert_eq!(
            report.desired_people, 1,
            "the de-authorized process is asked to stop once the settle handoff closes: {:?}",
            report.notes
        );
        assert!(
            !desires(&report, "signal-researcher"),
            "a settle brings nobody up: the settled person is ABSENT from the desired set, which \
             is the instruction: {:?}",
            report.notes
        );
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "the terminal park settles the withdrawn person desired-inactive"
        );
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "the CEO is untouched by somebody else's withdrawn intent"
        );
        // The safety posture is exactly as strong as before: no refusal was
        // needed for one in-budget stop, and the breaker did not trip.
        assert!(matches!(
            safety::read_safety_config(&db).actuation_mode,
            safety::ActuationMode::Apply
        ));
    }

    #[tokio::test]
    async fn ceo_only_preparation_removes_a_stale_tagged_head_on_the_first_apply() {
        // `prepare_ceo_only` (chiefd-core `org_ops`) atomically clears the
        // normalized fence and retracts non-CEO desired activity. This starts
        // from the corresponding already-prepared activity fact, then drives a
        // real `ConvergeActuator` over a committed report that still shows the
        // stale head running. It locks the boundary attach needs: CEO-only is
        // immediate cleanup, not the graceful retention policy for an ordinary
        // intent withdrawal.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let prepared_manifest = manifest.clone();
        db.mutate(MutationClass::Normal, MutationName("test.prepare_ceo_only"), move |ledgers| {
            let mut prepared = activity::read(ledgers, &prepared_manifest)?;
            prepared
                .people
                .get_mut("signal-researcher")
                .expect("seeded non-CEO")
                .last_desired_active = false;
            let encoded = serde_json::to_string(&prepared).map_err(|_| {
                chiefd_core::error::store_failure_because(
                    "test.prepare_ceo_only",
                    "injected by the test",
                )
            })?;
            activity::ingest_external_document(ledgers, &prepared_manifest, &encoded)
        })
        .await
        .expect("prepared CEO-only durable activity");
        assert_eq!(desired_active(&db, "signal-researcher"), Some(false));

        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("CEO-only stale-head converge");

        assert!(
            report.applied,
            "the immediate stale-head stop stays within the destructive budget: {:?}",
            report.notes
        );
        // TOMBSTONE: `ran_verb("kill-pane")`.
        assert_eq!(
            report.desired_people, 1,
            "the first CEO-only pass asks for exactly the stale head's stop: {:?}",
            report.notes
        );
        assert!(
            !desires(&report, "signal-researcher"),
            "CEO-only cleanup brings nobody up: {:?}",
            report.notes
        );
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "CEO-only leaves the root desired-active"
        );
        assert_eq!(desired_active(&db, "signal-researcher"), Some(false));
    }

    /// THE TIE NOTHING TESTED: a fired person is de-authorized once the
    /// handoff they were fired with goes terminal.
    ///
    /// `offboard_person` deliberately does NOT clear the fence — the departure
    /// does not apply until the person releases their offboard handoff, and
    /// they must be running to write it, so de-authorizing them in that commit
    /// would have the offboard abandon its own handoff. Authorization is
    /// therefore a DERIVED TERM: Active, OR holding an open offboard handoff.
    ///
    /// The second half expires by itself, in the converge pass's F8 withdrawal
    /// half — and until now nothing asserted that it does. Exactly one filter
    /// in a different module stood between a fired person and being authorized
    /// to run, with no test naming offboard anywhere near it.
    #[tokio::test]
    async fn a_departed_person_is_de_authorized_once_the_handoff_is_terminal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let fenced_store = store.clone();
        let fence_slug = manifest.slug.clone();
        let fenced = move || {
            fenced_store.launch_intent_person_ids(&fence_slug, &fence_slug).expect("fence read")
        };
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // FIRED. The offboard leaves them departed AND still fenced, holding an
        // open handoff — asserted at the op's own level in
        // `offboard_keeps_the_fence_while_the_handoff_is_open_because_the_person_must_write_it`.
        db.offboard_person(
            "signal-researcher".to_owned(),
            "2026-07-28T00:05:00.000Z".to_owned(),
            "operator".to_owned(),
        )
        .await
        .expect("the offboard applies");
        assert!(
            fenced().contains("signal-researcher"),
            "authorization is held THROUGH the offboard so the handoff can be written"
        );
        let held = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("the attended handoff pass");
        assert!(
            desires(&held, "signal-researcher"),
            "operational gating must not end an existing attended offboard handoff: {:?}",
            held.notes
        );
        assert!(
            fenced().contains("signal-researcher"),
            "the existing fence stays through the attended handoff"
        );

        // The handoff completes: the person releases it and the transition goes
        // terminal. From here they are neither active nor mid-transition.
        let open_transition = db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let ledger = chiefd_core::store::activity::read(snapshot, &org).expect("activity");
            ledger
                .people
                .get("signal-researcher")
                .and_then(|state| state.active_transition_id.clone())
                .expect("the offboard left an open transition")
        });
        db.org_activity_release(chiefd_core::store::activity::ReleaseInput {
            transition_id: open_transition,
            person_id: "signal-researcher".to_owned(),
        })
        .await
        .expect("the handoff releases");

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("the converge pass after the terminal handoff");

        assert!(
            !fenced().contains("signal-researcher"),
            "a fired person whose handoff is done must not stay AUTHORIZED to run: the roster \
             filter that stops them today lives in another module and nothing tied it to offboard"
        );
    }

    /// The operator's deleted-unit storm: a real recursive department removal made
    /// two people departed and withdrew their launch fences, but old pending
    /// mail was still treated as fresh launch demand on later supervision
    /// passes. Each pass could then supersede and mint another offboard
    /// transition while the client removed and rebuilt the same bodies.
    ///
    /// The removal is the authority boundary. Mail that remains readable for
    /// a retained departed person is history, not permission to run them.
    #[tokio::test]
    async fn removed_department_people_never_regain_launch_demand_or_offboard_transitions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "quant-head").await;
        activate(&db, &manifest, "signal-researcher").await;
        stage_pending_mail(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let removed = db
            .remove_department_tree(
                "quant".to_owned(),
                "2026-08-17T23:52:01.317Z".to_owned(),
                "chief".to_owned(),
            )
            .await
            .expect("the CEO removes the managed unit");
        assert!(matches!(
            removed,
            chiefd_core::store::org_ops::RemoveDepartmentOutcome::Applied { .. }
        ));

        let transitions_after_removal = db.read(|snapshot| {
            let org = organization::read(snapshot).expect("fresh organization");
            let ledger = activity::read(snapshot, &org).expect("fresh activity");
            assert_eq!(
                org.people["signal-researcher"].employment_state,
                chiefd_core::store::organization::EmploymentState::Departed
            );
            let state = &ledger.people["signal-researcher"];
            assert!(!state.last_desired_active);
            assert!(state.active_transition_id.is_none());
            ledger.transitions.len()
        });

        // Model both other stale demand sources from the live incident. The
        // normalized projection can still carry an older start fence, and a
        // queued maintenance row can outlive the unit. Neither is authority to
        // revive a person the current roster says is departed.
        let store =
            normalized_intent_store(dir.path(), dir.path(), &manifest, &["signal-researcher"]);
        let facts = open_company_db(&dir.path().join("org.sqlite")).expect("open SQL facts");
        facts
            .execute(
                "INSERT INTO maintenance_requests( \
                     slug,id,ordinal,person_id,requested_by,action,status,requested_at) \
                 VALUES(?1,'maintenance-after-removal',1,'signal-researcher','chief', \
                        'fresh_session','queued','2026-08-17T23:51:59.000Z')",
                [&manifest.slug],
            )
            .expect("queue stale maintenance demand");
        drop(facts);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        for pass in 1..=3 {
            // The operator reminted the next cancelled offboard wave a little over
            // four minutes later. Cross that exact class of boundary between
            // each pass without sleeping.
            manual.advance(std::time::Duration::from_secs(4 * 60 + 3));
            let report = actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .unwrap_or_else(|error| panic!("supervision pass {pass}: {error}"));
            assert!(
                !desires(&report, "signal-researcher"),
                "pass {pass} must not remint a departed body: {:?}",
                report.notes
            );
            db.read(|snapshot| {
                let org = organization::read(snapshot).expect("fresh organization");
                let ledger = activity::read(snapshot, &org).expect("fresh activity");
                let state = &ledger.people["signal-researcher"];
                assert!(!state.last_desired_active, "pass {pass}");
                assert!(state.active_transition_id.is_none(), "pass {pass}");
                assert_eq!(
                    ledger.transitions.len(),
                    transitions_after_removal,
                    "pass {pass} must not cancel and remint an offboard transition"
                );
            });
            let fence = db
                .launch_intent_read()
                .await
                .expect("launch intent read")
                .map(|(row, _)| row.person_ids)
                .unwrap_or_default();
            assert!(
                !fence.contains(&"signal-researcher".to_owned()),
                "pass {pass} must not turn retained mail into launch authority: {fence:?}"
            );
        }
    }

    /// F8 / arch Step 5 — THE HARD RULE's shrink half, daemon-only. A worker
    /// the CEO started finishes its work and goes idle; NO TypeScript process
    /// exists to run `reconcileOrganizationActivity`, so the converge cycle
    /// itself must (a) let the fence's start demand lapse once the worker is
    /// up, (b) admit the routine idle park once the sixty-second quiet lease
    /// expires, (c) commit the park's terminal state as a per-person
    /// launch-intent WITHDRAWAL — the durable record, written first — and
    /// (d) derive the stop from it, trending the company to CEO-only.
    /// On the base commit this cannot pass: the fence pinned its people as
    /// permanent `Requested` demand, no park ever fired, and no per-person
    /// withdrawal existed in Rust at all.
    /// THE OPERATOR'S CLICK IS NOT DISCARDED WHILE THEIR PI IS STILL BOOTING.
    ///
    /// The rule this pins is `fence_still_supplies_demand`, and it was
    /// `!last_desired_active` alone until 2026-08-20. That flag is chiefd's own
    /// decision from the previous pass, so the grant lapsed one pass after it
    /// was made — measured on `taperoom-inc` as `launch-intent dev upsert`
    /// 23:24:21.570 followed by `launch-intent dev delete` 23:24:22.132, a
    /// 562ms life, while that person's Pi needed until 23:25:02 to report
    /// `interactive-loop-ready`. The shrink half read the lapsed fence as one
    /// with no demand behind it and dropped the row; the pane arrived into a
    /// company that had stopped wanting it and was reaped. Four clicks, four
    /// times nothing.
    #[test]
    fn a_grant_lapses_when_the_person_answers_not_when_chiefd_wants_them_up() {
        // The rule is time-relative now, so the fixture needs a clock. Every
        // assertion below except the wake-lease pair is taken far past any
        // lease, so the wake floor cannot silently satisfy them.
        const NOW: i64 = 1_787_181_862_132; // 2026-08-19T23:24:22.132Z
        let state = |desired: bool, active_at: Option<&str>, quiet_at: Option<&str>| {
            chiefd_core::store::activity::PersonActivityState {
                person_id: "signal-researcher".to_owned(),
                last_employment_state: chiefd_core::store::organization::EmploymentState::Active,
                last_department_id: "quant".to_owned(),
                last_operational: true,
                last_desired_active: desired,
                idle_since: None,
                agent_quiet_at: quiet_at.map(str::to_owned),
                agent_active_at: active_at.map(str::to_owned),
                operator_wake_at: None,
                active_transition_id: None,
                updated_at: "2026-08-19T23:24:22.132Z".to_owned(),
            }
        };

        // Nobody has ever projected them: the grant is fresh demand.
        assert!(
            crate::converge_apply::cycle::fence_still_supplies_demand(None, NOW),
            "an unprojected person is demand"
        );

        // chiefd decided they should be up and nothing has answered yet — the
        // pane is starting. THE CASE THE OPERATOR HITS.
        assert!(
            crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&state(true, None, None)),
                NOW
            ),
            "a granted wake stopped being demand while the pane was still starting"
        );

        // The agent spoke: from here the settle path owns them, so the grant
        // lapses exactly as it always did.
        assert!(
            !crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&state(true, Some("2026-08-19T23:25:02.045Z"), None)),
                NOW
            ),
            "a person whose agent is working must not be re-requested every pass"
        );
        assert!(
            !crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&state(true, None, Some("2026-08-19T23:26:00.000Z"))),
                NOW
            ),
            "a person who reported quiet must be allowed to settle"
        );

        // Not desired at all: the grant is what brings them up.
        assert!(
            crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&state(false, None, None)),
                NOW
            ),
            "a fenced person nobody is running yet is demand"
        );
    }

    /// A WAKE OUTRANKS THE AGENT'S OWN REPORT, FOR EXACTLY ONE LEASE.
    ///
    /// The rule above reads what the AGENT said about itself, and by those
    /// reports a person woken thirty seconds ago who beat once and was handed
    /// nothing to do is identical to one that finished its work. Operator ruling,
    /// 2026-08-20: *"If woken, it needs to wait the 2 mins."* So the wake floor
    /// gates the whole rule — and gates it for a bounded window, which is the
    /// half that keeps it from becoming a permanent pin.
    #[test]
    fn a_wake_supplies_demand_for_its_whole_lease_and_not_one_pass_longer() {
        const WOKE_AT_MS: i64 = 1_787_181_840_000; // 2026-08-19T23:24:00.000Z
        let woken_and_working = chiefd_core::store::activity::PersonActivityState {
            person_id: "signal-researcher".to_owned(),
            last_employment_state: chiefd_core::store::organization::EmploymentState::Active,
            last_department_id: "quant".to_owned(),
            last_operational: true,
            // Every clause of the pre-wake rule says "this grant has lapsed":
            // chiefd already desires them AND their agent has answered.
            last_desired_active: true,
            idle_since: None,
            agent_quiet_at: None,
            agent_active_at: Some("2026-08-19T23:24:05.000Z".to_owned()),
            operator_wake_at: Some("2026-08-19T23:24:00.000Z".to_owned()),
            active_transition_id: None,
            updated_at: "2026-08-19T23:24:05.000Z".to_owned(),
        };
        let lease = activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS;

        assert!(
            crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&woken_and_working),
                WOKE_AT_MS + 5_000
            ),
            "five seconds after the click the operator's wake still supplies demand"
        );
        assert!(
            crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&woken_and_working),
                WOKE_AT_MS + lease - 1
            ),
            "the floor holds to the last millisecond of the lease"
        );
        assert!(
            !crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&woken_and_working),
                WOKE_AT_MS + lease
            ),
            "and not one millisecond longer: the lease is a floor, never a pin"
        );

        // A SECOND CLICK RESTARTS THE FLOOR rather than inheriting what is left
        // of the first one. An operator asking again is asking again.
        let woken_again = chiefd_core::store::activity::PersonActivityState {
            operator_wake_at: Some("2026-08-19T23:26:00.000Z".to_owned()),
            ..woken_and_working.clone()
        };
        assert!(
            crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&woken_again),
                WOKE_AT_MS + lease + 1_000
            ),
            "a fresh wake buys a fresh window"
        );

        // A DAMAGED STAMP IS NOT A LEASE. Fail-safe in the direction that keeps
        // the settle working: nothing about this column may prolong a person.
        let damaged = chiefd_core::store::activity::PersonActivityState {
            operator_wake_at: Some("not-a-time".to_owned()),
            ..woken_and_working
        };
        assert!(
            !crate::converge_apply::cycle::fence_still_supplies_demand(
                Some(&damaged),
                WOKE_AT_MS + 5_000
            ),
            "an unparseable wake stamp cannot hold anybody up"
        );
    }

    /// A WITHDRAWAL SAYS WHY. All three shrink-half branches printed
    /// `(settled)`, so a fence dropped because the person was never
    /// operational, or because the pass found no demand behind it at all, was
    /// reported to the operator as an agent that had finished its work and
    /// parked. On `taperoom-inc` a grant that lived 562ms was reported with a
    /// word that did not apply to it, which is why the log read as though
    /// nothing had gone wrong.
    #[tokio::test]
    async fn a_withdrawal_names_the_reason_it_dropped_the_fence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["chief".to_owned(), "signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let _ = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 1");
        // The agent answers, then reports it went quiet, then the lease runs
        // out: the ONE branch that is genuinely a settle.
        agent_settled(&db, "signal-researcher").await;
        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 1) as u64,
        ));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("settle pass");
        let withdrawal = report
            .notes
            .iter()
            .find(|note| note.contains("launch intent withdrawn"))
            .expect("the settle withdraws the fence and says so");
        assert!(
            withdrawal.contains("(settled)") && withdrawal.contains("signal-researcher"),
            "a real settle is reported as one: {withdrawal}"
        );
    }

    /// THE WAKE MUST NOT INVENT A BEAT. A person woken while their quiet clock
    /// was running had `agent_active_at` stamped with the WAKE instant, and
    /// `fence_still_supplies_demand` reads any stamp as "their agent answered"
    /// — so the grant lapsed on the very next pass and the shrink half swept
    /// it. The wake defeated itself.
    ///
    /// Measured on a live box (2026-08-20): `engineering-kimi3` was
    /// hired at 17:06:41 and clicked awake; his row ends up
    /// `agent_active_at = 17:08:50` with no quiet stamp, no launch-intent row,
    /// `last_desired_active = 0` — and no pane was ever started for him. The
    /// only writer of that stamp with no pane in existence is the wake itself.
    #[tokio::test]
    async fn a_wake_leaves_no_report_rather_than_inventing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        // The person answered once and then went quiet — the ordinary state of
        // somebody the operator finds asleep and clicks.
        activate(&db, &manifest, "signal-researcher").await;
        agent_settled(&db, "signal-researcher").await;

        db.wake_person(
            "signal-researcher".to_owned(),
            "2026-08-02T06:00:01.000Z".to_owned(),
            "operator".to_owned(),
        )
        .await
        .expect("the wake commits");

        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let ledger = activity::read(snapshot, &org).expect("activity");
            let state = &ledger.people["signal-researcher"];
            assert_eq!(state.agent_quiet_at, None, "the wake spends the silence that preceded it");
            assert_eq!(state.idle_since, None, "and the countdown derived from it");
            assert_eq!(
                state.agent_active_at, None,
                "but it must NOT claim the agent reported anything: the honest state for a \
                 person about to start is `no report yet`, and a stamped instant is read by \
                 the fence rule as an answer that never came — which withdraws the very grant \
                 the wake just made"
            );
            Ok::<_, chiefd_core::error::ChiefdError>(())
        })
        .expect("read");
    }

    #[tokio::test]
    async fn an_idle_worker_is_settled_withdrawn_and_killed_converging_to_ceo_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        // The worker genuinely ran: demand arrived, was answered, and has
        // drained by the time the test's passes run (nothing re-pins them).
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        // The operator's explicit start decision, committed through the same
        // typed row route the public publish API uses — one store, shared by
        // the actor and the cycle's fence reader, exactly like `chiefd run`.
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            // The root's start decision (genesis wrote it) plus the worker's.
            // It used to name the worker alone, because the root's residency came
            // from the deleted unconditional lease rather than from a row of its own.
            person_ids: vec!["chief".to_owned(), "signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");
        // The topology passes 1-4 genuinely have: CEO plus the idle worker.

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let fenced_store = store.clone();
        let fence_slug = manifest.slug.clone();
        let fenced = move || {
            fenced_store.launch_intent_person_ids(&fence_slug, &fence_slug).expect("fence read")
        };

        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // Pass 1 — the fence's start demand has lapsed (the worker IS up), the
        // sixty-second quiet lease just started: adopted, nobody withdrawn,
        // nothing asked for. On the base commit the worker is `Requested`
        // demand FOREVER here — the first observable difference.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 1");
        assert!(desires(&report, "signal-researcher"), "and is still desired: {report:?}");
        assert_eq!(
            fenced(),
            ["chief".to_owned(), "signal-researcher".to_owned()].into_iter().collect(),
            "no withdrawal yet -- both start decisions stand"
        );
        assert!(
            !report.notes.iter().any(|n| n.contains("withdrawn")),
            "no settle decision commits on the lease-start pass: {:?}",
            report.notes
        );

        // The agent reports it went QUIET. THE COUNTDOWN STARTS HERE and
        // nowhere else: chiefd no longer starts a clock from "I desired them
        // active and I see no demand", because that timed a person's silence
        // against a pane that might not exist yet. A worker that never says it
        // went quiet is never settled -- that is the ruling, and it is why this
        // report is now a required step of the end-to-end settle path.
        agent_settled(&db, "signal-researcher").await;

        // Pass 2 — the quiet lease expired. The routine idle park is admitted
        // as a committed durable record and it is FORCED terminal at birth, so
        // the settle path commits the per-person launch-intent withdrawal
        // inside that same transaction and the stop derives from it: the
        // record first, the action second, in ONE pass.
        //
        // Two passes used to sit between these: one for the park's handoff
        // window and one for its overdue window, each retaining the process
        // while nothing arrived. Nothing could: a routine idle park has no
        // releaser, so both windows were a wait for a message no code path
        // sends. Deleting them is what makes the quiet lease the whole settle
        // budget, and this is the test that shows the deletion end to end.
        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 1) as u64,
        ));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 2");
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let activity = activity::read(snapshot, &org).expect("activity");
            let transition = activity
                .active_transition("signal-researcher")
                .expect("a routine idle park was admitted");
            assert_eq!(transition.reason, activity::IDLE_AUTO_PARK_REASON);
            assert_eq!(
                transition.status,
                activity::TransitionStatus::Forced,
                "the park is terminal in the pass that admits it: {transition:?}"
            );
        });
        // TOMBSTONE: `ran_verb("kill-pane")`, and the kill-count comparison
        // across passes 4 and 5 below.
        assert_eq!(
            report.desired_people, 1,
            "the settled worker is asked to stop, exactly once: {report:?}"
        );
        assert!(
            !desires(&report, "signal-researcher"),
            "a settle brings nobody up: the settled person is ABSENT from the desired set, which \
             is the instruction: {:?}",
            report.notes
        );
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "the forced park settles the worker desired-inactive"
        );
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "the CEO is the durable control plane"
        );
        assert_eq!(
            fenced(),
            ["chief".to_owned()].into_iter().collect::<std::collections::BTreeSet<_>>(),
            "the withdrawal is committed: the fence is CEO-only, and it now SAYS so. This \
             asserted the EMPTY set while its own message read 'CEO-only', because the root's \
             residency was an unconditional lease rather than a row anybody could see. The \
             worker's decision is withdrawn; the root's is untouched."
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("launch intent withdrawn") && n.contains("signal-researcher")),
            "the settle pass names its withdrawal: {:?}",
            report.notes
        );
        assert!(
            report.retry_after_floor,
            "a committed withdrawal publishes a reconcile wake for the follow-up pass"
        );
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let activity = activity::read(snapshot, &org).expect("activity");
            let transition = activity
                .active_transition("signal-researcher")
                .expect("the terminal park remains the durable record");
            assert_eq!(
                transition.status,
                activity::TransitionStatus::Forced,
                "no release ever arrived: the park is forced terminal (#337), and THAT record drove the withdrawal"
            );
        });

        // Pass 5 — the client applied the stop and reports the CEO-only
        // topology. Converged: the narrowed fence is observed, and the pass is
        // a writeless no-op (re-running the settle path withdraws nobody
        // twice, and asks for no second stop).
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 5");
        assert_eq!(report.desired_people, 1, "CEO-only is the converged state: {report:?}");
        assert!(desires(&report, "chief"), "and the one desired person is the CEO: {report:?}");
        assert!(!report.retry_after_floor, "nothing left to settle");
        assert_eq!(
            fenced(),
            ["chief".to_owned()].into_iter().collect::<std::collections::BTreeSet<_>>(),
            "the fence stays CEO-only, and stays SAID: re-running the settle path withdraws \
             nobody twice, and it does not quietly evict the root either. This expected the \
             EMPTY set, from the era when the root's residency was an unconditional lease \
             instead of an entry anybody could read."
        );
    }

    /// THE INCIDENT, END TO END. An operator stands the company down; a person
    /// with queued mail must stay down, and must come back when — and only
    /// when — the operator resumes.
    ///
    /// The live sequence this reproduces: the CEO was told to stop all work and
    /// obeyed exactly, parking six people and reporting `Stood down 6 people`.
    /// Forty-five seconds later all six were back up with fresh panes and new
    /// contexts, because the mail they had queued to each other re-granted
    /// PARKED IS NOT DELETED, AND NOW SOMETHING ASSERTS IT.
    ///
    /// The operator ruled personally that a parked person is never
    /// force-killed: they keep identity, sessions, memory, mailbox, workspace,
    /// model, skills and audit history (`docs/testing/TEST_SUITE.md`, Case 15).
    /// That claim was true BY CONSTRUCTION — a park writes the activity ledger
    /// and never the manifest, and every people/mailbox delete is
    /// manifest-driven — and by construction is exactly how a guarantee stops
    /// being true without anybody noticing. `an_idle_worker_is_settled_...`
    /// above checks the roster only.
    ///
    /// So this parks a person for real, through the whole cycle, and compares
    /// what they had against what they have. The person record is compared
    /// WHOLE rather than field by field, so a future change cannot take a
    /// field with it that this test forgot to name: identity, mandate, kind,
    /// department, employment state, activation, tools, prompts, creation
    /// stamp and the append-only staffing history are one assertion. Their
    /// mailbox is compared whole for the same reason.
    ///
    /// GUARD-RAIL, and deliberately so. Nothing today deletes any of this, so
    /// no revert of current code makes it red. Its whole value is the future:
    /// it is the test that fails on the day a park starts tidying up after
    /// itself, which is the one regression the operator asked for by name.
    ///
    /// WHAT IT DOES NOT PIN, stated so nobody reads more into it: the agent
    /// home, the Pi transcript, the workspace directory and the resolved model
    /// are DISK and launch-catalog facts, not store facts. They are not
    /// reachable from this seam and asserting them needs the materialization
    /// harness. This covers the durable store's half.
    /// A WAKE BUYS THE WHOLE QUIET LEASE, MESSAGE OR NOT.
    ///
    /// Operator ruling, 2026-08-20: *"If I tell chief to message it, it'll come
    /// back up and do the 2min settling. We need it to always do that when
    /// woken. Message or not. If woken, it needs to wait the 2 mins."* A wake is
    /// an operator decision and it buys a full
    /// `ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS` window before anything may park
    /// the person, withdraw their launch intent, or stop them. Work arriving
    /// inside the window behaves as it always did; the window is a FLOOR.
    ///
    /// # The measured failure this reproduces
    ///
    /// `research-promoter` on `taperoom-inc` (a live box),
    /// 2026-08-20, from `org_events`:
    ///
    /// ```text
    /// 20:34:00.543  launch-intent   research-promoter  upsert  actor=service
    /// 20:34:02.708  launch-intent   research-promoter  delete  actor=''
    /// 20:34:07.760  person-activity research-promoter  upsert
    /// 20:34:13+     reconcile.people.withheld: research-promoter[nothing-demanded-them]
    /// ```
    ///
    /// The grant lived 2.165 seconds, no `launch intent withdrawn (...)` line
    /// names her anywhere in that window, and the pass that deleted her row
    /// still reported `launching: ..., research-promoter, ...`. She carried a
    /// terminal `forced` routine idle park from 20:22:46.540Z into the wake, and
    /// no message was ever sent to her.
    ///
    /// The fixture is that gesture exactly: park somebody for real, wake them
    /// through the operator's own verb, let their agent beat once, and let mail
    /// arrive FOR SOMEBODY ELSE — which is what drives the whole-document
    /// launch-intent republish that differenced her row away.
    #[tokio::test]
    async fn a_wake_holds_the_launch_intent_for_the_whole_quiet_lease_with_no_message() {
        use chiefd_core::clock::Clock;

        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["chief".to_owned(), "signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // --- the precondition: a real settle, so they carry a terminal park ---
        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 1");
        agent_settled(&db, "signal-researcher").await;
        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 1) as u64,
        ));
        let settled = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("the settle pass");
        assert!(!desires(&settled, "signal-researcher"), "the precondition is a parked person");
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let activity = activity::read(snapshot, &org).expect("activity");
            let transition = activity
                .active_transition("signal-researcher")
                .expect("a routine idle park was admitted");
            assert_eq!(transition.status, activity::TransitionStatus::Forced);
        });

        // --- THE OPERATOR CLICKS WAKE UP. No message is sent to them, ever. ---
        let woke_at = manual.wall().to_iso8601();
        db.wake_person("signal-researcher".to_owned(), woke_at.clone(), "chief".to_owned())
            .await
            .expect("the wake applies");
        assert!(
            fenced(&db).await.contains("signal-researcher"),
            "the wake grants the launch intent it is made of"
        );

        // Their agent answers once — the `person-activity upsert` at 20:34:07 —
        // and then says nothing more, because nobody gave it anything to do.
        db.org_activity_note_agent_state("signal-researcher".to_owned(), true)
            .await
            .expect("the beat commits");

        // MAIL FOR SOMEBODY ELSE. This is what makes the pass compute a
        // non-empty launch demand and republish the launch-intent document,
        // and it is the only role it plays: nothing is ever addressed to the
        // woken person.
        stage_pending_mail_at(&db, &manifest, "quant-head", &manual.wall().to_iso8601()).await;

        // --- the whole lease, one pass at a time ---
        //
        // AND THE AGENT REPORTS IT HAS NOTHING TO DO, TEN SECONDS IN. This is
        // the sharp half: an explicit `agent_settled` is the strongest signal
        // the product has that somebody may be stopped, and until the wake lease
        // existed it withdrew the fence on the very next pass — the operator's
        // click undone ten seconds after they made it, by a report that is
        // perfectly true and completely beside the point. "If woken, it needs to
        // wait the 2 mins."
        let step = std::time::Duration::from_millis(5_000);
        let mut elapsed = 0i64;
        let mut reported_quiet = false;
        while elapsed + 5_000 < activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS {
            manual.advance(step);
            elapsed += 5_000;
            let report = actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .expect("a pass inside the wake lease");
            assert!(
                fenced(&db).await.contains("signal-researcher"),
                "{elapsed}ms after the wake the launch intent was withdrawn; a wake buys the \
                 whole {}ms quiet lease, message or not. Notes: {:?}",
                activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS,
                report.notes
            );
            assert!(
                desires(&report, "signal-researcher"),
                "{elapsed}ms after the wake they stopped being desired: {:?}",
                report.notes
            );
            if !reported_quiet && elapsed >= 10_000 {
                agent_settled(&db, "signal-researcher").await;
                reported_quiet = true;
            }
        }
        assert!(reported_quiet, "the agent reported quiet inside the lease, or this proves less");

        // --- AND THE LEASE IS A FLOOR, NOT A CEILING ---
        //
        // Past it, with the agent's own quiet report standing and no work of
        // their own, the ordinary settle owns them again exactly as it did
        // before the wake. A wake that pinned somebody for ever would be a
        // different defect, not a fix.
        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 5_000) as u64,
        ));
        let mut settled_again = None;
        for _ in 0..3 {
            let report = actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .expect("a pass past the wake lease");
            if !desires(&report, "signal-researcher")
                && !fenced(&db).await.contains("signal-researcher")
            {
                settled_again = Some(report);
                break;
            }
        }
        let settled_again = settled_again
            .expect("past the lease the ordinary settle parks them again; the wake is a floor");
        // AND IT SAYS SO. A withdrawal that nobody can read is the defect this
        // packet also closes: the pass names the person and the reason.
        let withdrawal = settled_again
            .notes
            .iter()
            .find(|note| note.contains("launch intent withdrawn"))
            .expect("the settle past the lease withdraws the fence and says so");
        assert!(
            withdrawal.contains("signal-researcher"),
            "the withdrawal names who it dropped: {withdrawal}"
        );
    }

    /// The people the committed `launch_intent` ROWS authorize right now.
    ///
    /// Read from the rows and never from the actor's in-memory document: the
    /// whole subject of the wake-lease tests is a row-level grant the document
    /// had not seen.
    async fn fenced(db: &CompanyDb) -> std::collections::BTreeSet<String> {
        db.launch_intent_read()
            .await
            .expect("read the committed fence rows")
            .map(|(intent, _seq)| intent.person_ids.into_iter().collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_force_parked_person_keeps_their_whole_record_and_their_mailbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        // REAL AUDIT HISTORY, WRITTEN BY THE REAL VERBS. The fixture person is
        // born with none, and a test that compares an empty history against an
        // empty history proves nothing about history. A bench and the wake that
        // recalls them append `benched` and `recalled` through the same store
        // paths an operator's click drives, and leave the person active — which
        // is the state the park needs.
        // AT THE CLOCK'S OWN INSTANT, not a hand-written stamp. A wake buys a
        // durable quiet lease measured from the moment it happened
        // (`activity::operator_wake_lease_active`), so a fixture that woke
        // somebody at an arbitrary date would be timing the park against a lease
        // that started somewhere else entirely.
        let staffing_at = {
            use chiefd_core::clock::Clock;
            manual.wall().to_iso8601()
        };
        db.bench_person("signal-researcher".to_owned(), staffing_at.clone(), "chief".to_owned())
            .await
            .expect("bench");
        db.wake_person("signal-researcher".to_owned(), staffing_at, "chief".to_owned())
            .await
            .expect("the wake recalls them");
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        // Mail they were sent while they were up. It is archived at fence
        // commit (#493(A)) by the pass below, so by the time the park lands it
        // is terminal history rather than fresh demand — which is the half of
        // "keeps their mailbox" that a park could plausibly tidy away.
        stage_pending_mail(&db, &manifest, "signal-researcher").await;
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["chief".to_owned(), "signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");

        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()));

        // Passes 1-2: they are up, and their mail is consumed into the fence and
        // archived, so nothing below is holding them up on demand that would
        // stop the park.
        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 1");
        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("pass 2");

        // The park, for real: the agent DRAINS its mail — the live person this
        // came from "answered in one line" — then reports quiet, and the lease
        // expires. Draining matters: pending mail is effective demand, and a
        // person with demand is never an idle-park candidate.
        db.mutate(MutationClass::Normal, MutationName("test.drain_mail"), move |ledgers| {
            let row_id = chiefd_core::store::mailbox::pending_for(ledgers, "signal-researcher")
                .first()
                .expect("the staged mail is pending")
                .row_id("signal-researcher");
            assert!(
                chiefd_core::store::mailbox::archive(
                    ledgers,
                    &row_id,
                    chiefd_core::store::mailbox::MailboxState::Accepted,
                ),
                "the pane accepted the message"
            );
            Ok(())
        })
        .await
        .expect("drain the mail");
        agent_settled(&db, "signal-researcher").await;

        // EVERYTHING THEY HAVE, READ THE INSTANT BEFORE THE PARK. Not earlier:
        // the drain and the quiet report above are the TEST's actions, and a
        // baseline taken before them would charge the park with their changes.
        // The only thing that happens between these two reads is the park.
        let before_person = db.read(|snapshot| {
            organization::read(snapshot).expect("org").people["signal-researcher"].clone()
        });
        let (before_mail, _) = db.mailbox_read().await.expect("mailbox before");
        assert!(
            !before_person.staffing_history.as_ref().is_none_or(Vec::is_empty),
            "the person carries staffing history, or this test pins an empty vector against \
             an empty vector and proves nothing about audit history"
        );
        assert!(!before_mail.entries.is_empty(), "and they carry mail, for the same reason");

        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 1) as u64,
        ));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("the park pass");
        assert!(
            !desires(&report, "signal-researcher"),
            "the park landed and the person is no longer desired: {report:?}"
        );
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let activity = activity::read(snapshot, &org).expect("activity");
            let transition = activity
                .active_transition("signal-researcher")
                .expect("a routine idle park was admitted");
            assert_eq!(transition.reason, activity::IDLE_AUTO_PARK_REASON);
            assert_eq!(transition.status, activity::TransitionStatus::Forced);
        });

        // AND EVERYTHING THEY HAVE, READ AFTER THE PARK. Parked means no pane
        // and no compute; it does not mean a smaller person.
        let after_person = db.read(|snapshot| {
            organization::read(snapshot).expect("org").people["signal-researcher"].clone()
        });
        let (after_mail, _) = db.mailbox_read().await.expect("mailbox after");

        assert_eq!(
            after_person, before_person,
            "a force-parked person's record must be untouched — identity, mandate, kind, \
             department, employment state, activation, tools, prompts, creation stamp and \
             staffing history. Parked is not deleted, and it is not diminished either."
        );
        assert_eq!(
            after_mail, before_mail,
            "and their mailbox survives the park intact: a parked person is a person who is \
             not running, not a person whose history was cleared up behind them"
        );
    }

    /// every one of them. The only defence was a per-person watermark derived
    /// from each person's own stop, and any later message defeats it.
    ///
    /// The four things this pins, in order, are the whole rule:
    /// the stand-down holds them; it says WHO it is holding; it holds them
    /// again on the next pass and the pass after (one obeyed pass is not "stays
    /// stopped"); and the resume gives them back with their mail intact.
    #[tokio::test]
    async fn a_stood_down_company_holds_a_persons_mail_instead_of_relaunching_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let fenced_store = store.clone();
        let fence_slug = manifest.slug.clone();
        let fenced = move || {
            fenced_store.launch_intent_person_ids(&fence_slug, &fence_slug).expect("fence read")
        };
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("adopt pass");
        assert!(desires(&report, "signal-researcher"), "precondition: {:?}", report.notes);

        // THE OPERATOR'S GESTURE. One durable decision, and it empties the
        // fence — the same thing `chief stand-down` and `org_stand_down` do.
        db.stand_down_set("2026-08-18T10:00:00.000Z".into(), "stop all work now".into())
            .await
            .expect("stand the company down");
        assert_eq!(
            fenced(),
            std::collections::BTreeSet::new(),
            "the stand-down empties the fence, leaving exactly the CEO"
        );

        // And the mail arrives anyway — a peer messaging them, which is what
        // was already queued when the six were parked.
        deliver_mail_over_the_wire(&db, &manifest, "signal-researcher").await;

        // PASS ONE: held.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("stood-down pass");
        assert!(
            !desires(&report, "signal-researcher"),
            "a message to somebody the operator stopped must NOT relaunch them: {:?}",
            report.notes
        );
        assert_eq!(
            fenced(),
            std::collections::BTreeSet::new(),
            "and nothing put them back into the durable fence"
        );
        assert!(
            !report.notes.iter().any(|note| note.contains("mail wake granted launch intent")),
            "no wake was granted: {:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|note| note.contains("stand-down holds pending mail for")
                && note.contains("signal-researcher")),
            "the pass names who it is holding, so a stopped company is readable rather than \
             merely silent: {:?}",
            report.notes
        );
        assert!(
            !report.notes.iter().any(|note| note.contains("mail demand NOT desired")),
            "and raises no alarm: an operator holding their own company stopped is the intended \
             state, not a fault: {:?}",
            report.notes
        );
        assert!(
            report.actuation_record,
            "a held company still says what it is doing at the default log level"
        );

        // PASS TWO AND THREE: still held. One obeyed pass is not "stays
        // stopped" — the live company obeyed for forty-five seconds.
        for pass in 2..=3 {
            let report = actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .expect("stood-down pass");
            assert!(
                !desires(&report, "signal-researcher"),
                "pass {pass} must still hold them: {:?}",
                report.notes
            );
            assert_eq!(fenced(), std::collections::BTreeSet::new(), "pass {pass}");
        }

        // A SESSION-MAINTENANCE REQUEST IS NOT A WAY BACK IN EITHER. It is the
        // other half of `launch_demand`, and a rule that fenced only the mail
        // would have left this door open.
        db.session_maintenance_queue(chiefd_core::store::session_maintenance_ops::QueueInput {
            // Was `FreshSession`, for no reason but to be A request. One
            // action exists now, and the subject — that a maintenance request
            // is not a way back through a stood-down fence — never depended on
            // which.
            action: chiefd_core::store::session_maintenance::MaintenanceAction::Compact,
            person_id: "signal-researcher".to_owned(),
            requested_by: "chief".to_owned(),
            reason: "stood-down door check".to_owned(),
            automatic: false,
            force: None,
        })
        .await
        .expect("queue maintenance");
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("maintenance pass");
        assert!(
            !desires(&report, "signal-researcher"),
            "a queued maintenance request must not start somebody the operator stopped: {:?}",
            report.notes
        );

        // THE RESUME. The operator lifts it, and the HELD mail is what brings
        // them back — which is why it was held rather than dropped.
        db.stand_down_clear("2026-08-18T10:10:00.000Z".into()).await.expect("resume");
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("resumed pass");
        assert!(
            desires(&report, "signal-researcher"),
            "the message that was held is delivered the moment the company resumes: {:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|note| note.contains("mail wake granted launch intent")
                && note.contains("signal-researcher")),
            "and it is the ordinary mail wake that does it, not a special resume path: {:?}",
            report.notes
        );
        assert!(
            !report.notes.iter().any(|note| note.contains("stand-down holds")),
            "nothing is held any more: {:?}",
            report.notes
        );
    }

    /// THE CARLOS CASE (live `tribes-capital`, 2026-08-13 21:06-21:14).
    ///
    /// A person ran, settled, was routine-idle-parked at the two-minute lease
    /// and had their launch intent withdrawn. Then the CEO messaged them. On the
    /// live company that message was the LAST thing that happened to them: no
    /// wake, no pane, and the converge pass 2.5 minutes later still desired
    /// everybody except the head of Leadership, while reporting `applied: 0`.
    ///
    /// The rule this pins: a genuine durable envelope addressed to a settled,
    /// TERMINAL-PARKED, fence-withdrawn person is work arriving, and it is
    /// itself the explicit per-node decision that authorizes exactly them. The
    /// very next pass re-grants their launch intent, releases the stale terminal
    /// park pointer, and desires them again.
    ///
    /// The terminal park pointer is the part that makes this different from
    /// `pending_mailbox_demand_is_projected_desired_active_and_adopted`, which
    /// mails somebody who never ran: the parked person still carries an active
    /// `Forced` routine-idle-park transition, and the settle path's withdrawal
    /// half re-reads exactly that pointer. A person who cannot get out from
    /// under their own park can never be woken by anything.
    #[tokio::test]
    async fn mail_to_a_settled_parked_person_re_desires_them_and_re_grants_the_fence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec!["signal-researcher".to_owned()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let fenced_store = store.clone();
        let fence_slug = manifest.slug.clone();
        let fenced = move || {
            fenced_store.launch_intent_person_ids(&fence_slug, &fence_slug).expect("fence read")
        };
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // Up.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("adopt pass");
        assert!(desires(&report, "signal-researcher"), "{:?}", report.notes);

        // Settled, then the whole two-minute lease: forced park + withdrawal.
        agent_settled(&db, "signal-researcher").await;
        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 1) as u64,
        ));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("settle pass");
        assert!(!desires(&report, "signal-researcher"), "settled: {:?}", report.notes);
        assert_eq!(desired_active(&db, "signal-researcher"), Some(false));
        assert_eq!(fenced(), std::collections::BTreeSet::new(), "the fence is CEO-only");
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let ledger = activity::read(snapshot, &org).expect("activity");
            let transition = ledger
                .active_transition("signal-researcher")
                .expect("the terminal park is the durable record the wake must get out from under");
            assert_eq!(transition.status, activity::TransitionStatus::Forced);
        });

        // The CEO messages the parked person, through the SAME writer
        // `/v1/org/mailbox/delta` uses. Nothing else changes: no operator start,
        // no fence publish, no observation. The envelope is the whole input,
        // exactly as on the live company — and staging it through the in-memory
        // ledger instead would hide the defect, because that is the one path
        // the grant could already see.
        deliver_mail_over_the_wire(&db, &manifest, "signal-researcher").await;

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("wake pass");
        assert!(
            desires(&report, "signal-researcher"),
            "a message to a parked person is a wake: {:?}",
            report.notes
        );
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "the mailed person is desired-active again on the very next pass"
        );
        assert_eq!(
            fenced(),
            ["signal-researcher".to_owned()].into_iter().collect(),
            "and the durable fence names them again, so a later pass keeps the authority"
        );
        assert!(
            !report.notes.iter().any(|note| note.contains("mail demand NOT desired")),
            "the wake succeeded, so the pass raises no unmet-demand alarm: {:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|note| note.contains("mail wake granted launch intent")
                && note.contains("signal-researcher")),
            "the wake names who it authorized, so it is greppable afterwards: {:?}",
            report.notes
        );
        // AND THE NOTE MUST BE WRITTEN WHERE AN OPERATOR READS IT. A grant is
        // an actuation record, and the daemon logs the pass at INFO for it —
        // `changed` alone put exactly this line at DEBUG, which is how six
        // people relaunched themselves with `daemon.log` holding nothing but
        // `supervision cycle committed`.
        assert!(
            report.actuation_record,
            "a pass that granted launch intent must be readable at the default log level"
        );
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let ledger = activity::read(snapshot, &org).expect("activity");
            assert!(
                ledger.active_transition("signal-researcher").is_none(),
                "the stale terminal park pointer is released by the arriving work"
            );
        });

        // And it STAYS up while the mail is unread: a second pass must not
        // re-settle the person it just woke.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("hold pass");
        assert!(desires(&report, "signal-researcher"), "still up: {:?}", report.notes);
    }

    /// An open session-maintenance request is executable work for its exact
    /// person. It must grant launch authority and hold that person above the
    /// routine idle park until the request reaches a terminal state.
    #[tokio::test]
    async fn open_maintenance_blocks_idle_park_and_terminal_maintenance_releases_the_lease() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
            .expect("open company db");
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        agent_settled(&db, "signal-researcher").await;
        manual.advance(std::time::Duration::from_millis(
            (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS + 1) as u64,
        ));

        let held = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::deny_all(),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: vec!["signal-researcher".to_owned()],
            }),
        )
        .await
        .expect("maintenance pass");
        assert!(desires(&held, "signal-researcher"), "open maintenance is demand: {held:?}");
        db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let ledger = activity::read(snapshot, &org).expect("activity");
            assert!(
                ledger.active_transition("signal-researcher").is_none(),
                "maintenance must prevent the idle-park transition"
            );
        });

        let released = reconcile_cycle(
            &db,
            &config(dir.path()),
            ActuationMode::Apply,
            Some(ActivityProjectionInput {
                fence: LaunchFence::fenced(["signal-researcher".to_owned()]),
                pending_mail_facts: Vec::new(),
                maintenance_person_ids: Vec::new(),
            }),
        )
        .await
        .expect("terminal maintenance pass");
        assert!(
            !desires(&released, "signal-researcher"),
            "terminal maintenance releases its demand and permits the expired idle park: {released:?}"
        );
    }

    #[tokio::test]
    async fn normalized_maintenance_rows_grant_launch_intent_in_the_sql_projection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        let store = normalized_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let conn = open_company_db(&dir.path().join("org.sqlite")).expect("open SQL facts");
        conn.execute(
            "INSERT INTO maintenance_requests( \
                 slug,id,ordinal,person_id,requested_by,action,status,requested_at) \
             VALUES(?1,'maintenance-1',1,'signal-researcher','chief','fresh_session', \
                    'queued','2026-08-17T00:00:00.000Z')",
            [&manifest.slug],
        )
        .expect("queue maintenance in normalized SQL rows");
        drop(conn);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("SQL-backed activity projection");

        assert!(desires(&report, "signal-researcher"), "open SQL maintenance is demand");
        assert!(
            db.launch_intent_read()
                .await
                .expect("launch fence")
                .expect("launch intent row")
                .0
                .person_ids
                .contains(&"signal-researcher".to_owned()),
            "the maintenance demand grants durable per-person launch authority"
        );
    }

    /// The alarm the live investigation did not have. A pass that sees a genuine
    /// pending envelope for somebody it does NOT desire must say so by name and
    /// with a reason — `applied: 0` while the head of a department is not
    /// running is exactly the silent state that cost an evening.
    #[tokio::test]
    async fn unmet_mail_demand_is_named_with_its_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Benched BEFORE the seed, so the activity ledger's persisted
        // employment matches the manifest's and no structural park transition
        // is derived. That matters: a person who is being benched RIGHT NOW is
        // legitimately desired for the length of their handoff, and this test
        // is about the settled state after it — not operational, so `Requested`
        // demand is never even added, and the mail has nowhere to land.
        let mut manifest = northstar_manifest(EPOCH);
        manifest
            .people
            .get_mut("signal-researcher")
            .expect("the fixture worker")
            .employment_state = chiefd_core::store::organization::EmploymentState::Benched;
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &[],
            &[("signal-researcher", "pending", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert!(!desires(&report, "signal-researcher"), "{:?}", report.notes);
        let note = report
            .notes
            .iter()
            .find(|note| note.contains("mail demand NOT desired"))
            .unwrap_or_else(|| panic!("the pass must name unmet mail demand: {:?}", report.notes));
        assert!(note.contains("signal-researcher"), "{note}");
        assert!(note.contains("not operational"), "{note}");
    }

    /// TWO CONTRADICTORY STATEMENTS ABOUT ONE PERSON, IN ONE PASS.
    ///
    /// The pass above WARNs `mail demand NOT desired ... (not operational:
    /// benched, departed, or its unit is paused)` about the benched worker,
    /// and it is right. Five seconds later the per-person reason line called
    /// the same worker `nothing-demanded-them` — the reason a healthy idle
    /// company prints constantly, and the one the suite is taught to read as
    /// benign. A blocked person with unrouted mail was therefore
    /// indistinguishable from an idle one, in the exact field designed to tell
    /// them apart. Measured on a live company: `mail demand NOT desired` fired
    /// 24 times, `mail wake granted launch intent` 0 times, and the reason line
    /// said `docs-jordan[nothing-demanded-them]` through all of it.
    ///
    /// # Why this drives the composition and not a string
    ///
    /// The decision handed to the reason line is the REAL one, from
    /// `activity::reconcile` over a really benched person with really staged
    /// mail — and it is asserted to carry no reasons of its own first, because
    /// that empty list is the whole defect: the launch demand was filtered by
    /// `person_is_operational` before the decision was ever made, so
    /// "nothing demanded them" was true of the filtered input and false of the
    /// world. A test over `withheld_note`'s formatting would pass over this
    /// exactly as the shape assertion above does.
    #[tokio::test]
    async fn a_benched_person_holding_mail_is_never_reported_as_nothing_demanded_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut manifest = northstar_manifest(EPOCH);
        manifest
            .people
            .get_mut("signal-researcher")
            .expect("the fixture worker")
            .employment_state = chiefd_core::store::organization::EmploymentState::Benched;
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &[],
            &[("signal-researcher", "pending", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        // HALF ONE, from the real pass: the WARN names him, and names mail.
        assert!(
            report.notes.iter().any(|note| note.contains("mail demand NOT desired")
                && note.contains("signal-researcher")),
            "precondition — the pass must be warning about this exact person: {:?}",
            report.notes
        );

        // HALF TWO: the per-person reason for the same person, in the same
        // world, through the same composition the pass uses.
        //
        // The raw, pre-filter mail demand — the set the WARN reads — comes from
        // the `mailbox` table, read exactly as the projection reads it. It used
        // to be recomputed from `ledgers.mailbox_rows()` inside the closure
        // below, which is the in-memory half the pass no longer consults.
        let observed_mail: std::collections::BTreeSet<String> = {
            let conn = open_company_db(&dir.path().join("org.sqlite")).expect("open fixture");
            chiefd_core::store::reconciler_facts::read_pending_mail_facts(&conn, &manifest.slug)
                .expect("read the fixture's pending mail")
                .into_iter()
                .map(|fact| fact.person_id)
                .collect()
        };
        let held = db
            .mutate(
                MutationClass::Reconcile,
                MutationName("test.withheld_reasons"),
                move |ledgers| {
                    let manifest = organization::read(ledgers)?;
                    let supervision = supervision::read(ledgers, &manifest)?;
                    // Exactly the demand the pass computes for this world: a
                    // benched person is filtered out of mail demand, so nothing
                    // is requested and no fence names anybody.
                    let snapshot = activity::reconcile(
                        ledgers,
                        &manifest,
                        &supervision,
                        &ReconcileInput {
                            launch_intent: LaunchFence::deny_all(),
                            requested_person_ids: Vec::new(),
                            watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
                        },
                    )?;
                    let decision = snapshot
                        .people
                        .get("signal-researcher")
                        .expect("the benched worker still has a decision")
                        .clone();
                    Ok((
                        decision,
                        observed_mail.clone(),
                        crate::converge_apply::cycle::withheld_notes(
                            &manifest,
                            &snapshot,
                            &observed_mail,
                            &[],
                        ),
                    ))
                },
            )
            .await
            .expect("compose the withheld reasons");
        let (decision, observed_mail, notes) = held;

        assert!(
            observed_mail.contains("signal-researcher"),
            "precondition — real pending mail is waiting for him: {observed_mail:?}"
        );
        assert!(
            !decision.active,
            "precondition — a benched person is correctly withheld, and that must not change"
        );
        assert!(
            decision.reasons.is_empty(),
            "precondition — THE DEFECT: the operational filter ran before the decision, so the \
             decision itself knows of no demand for him: {:?}",
            decision.reasons
        );

        let note = notes
            .iter()
            .find(|note| note.starts_with("signal-researcher["))
            .unwrap_or_else(|| panic!("the withheld line must name him: {notes:?}"));
        assert!(
            !note.contains("nothing-demanded-them"),
            "a reason line must be false for nobody it names, and the same pass is warning \
             about this person BECAUSE he has mail: {note}"
        );
        assert!(
            note.contains("pending-mail-but-not-operational"),
            "his reason must carry the cause the pass already detected — pending mail, not \
             operational — so an operator can act on it: {note}"
        );
    }

    /// The default still means what it says. A person nobody asked for — no
    /// mail, no fence, no organization root — must keep reading
    /// `nothing-demanded-them`, or the fix above would have traded one false
    /// reason for another and made every idle company look blocked.
    #[tokio::test]
    async fn a_person_nobody_asked_for_still_reads_nothing_demanded_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;

        let notes = db
            .mutate(
                MutationClass::Reconcile,
                MutationName("test.withheld_reasons_idle"),
                move |ledgers| {
                    let manifest = organization::read(ledgers)?;
                    let supervision = supervision::read(ledgers, &manifest)?;
                    let snapshot = activity::reconcile(
                        ledgers,
                        &manifest,
                        &supervision,
                        &ReconcileInput {
                            launch_intent: LaunchFence::deny_all(),
                            requested_person_ids: Vec::new(),
                            watching_since: "1970-01-01T00:00:00.000Z".to_owned(),
                        },
                    )?;
                    Ok(crate::converge_apply::cycle::withheld_notes(
                        &manifest,
                        &snapshot,
                        &std::collections::BTreeSet::new(),
                        &[],
                    ))
                },
            )
            .await
            .expect("compose the withheld reasons");

        let note = notes
            .iter()
            .find(|note| note.starts_with("signal-researcher["))
            .unwrap_or_else(|| panic!("the withheld line must name him: {notes:?}"));
        assert_eq!(note, "signal-researcher[nothing-demanded-them]");
    }

    /// #638: an EXTERNAL settle (the TypeScript reconciler committing a
    /// blocked worker's routine idle park — the operator's #29 ruling: blocked =
    /// settle) leaves the worker terminal-parked and desired-inactive while
    /// the fence still names them. The settle pass must withdraw exactly that
    /// worker and leave them DOWN: their lapsed start decision is not fresh
    /// `Requested` demand. On the base commit the pass re-pins the worker
    /// desired-active and asks for a restart of the process it just withdrew,
    /// so CEO-only convergence races the next quiet lease (the ~30% #638 flake).
    #[tokio::test]
    async fn a_settled_fenced_workers_lapsed_start_decision_never_resurrects_them() {
        use chiefd_core::store::activity::{
            BeginTransitionInput, TransitionAction, TransitionStatus,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        // The worker genuinely ran, then the EXTERNAL (TS) reconciler settled
        // them: a terminal routine idle park with the person desired-inactive —
        // and the fence still naming them, exactly the durable shape #638's
        // fixture leaves for the daemon's next pass.
        //
        // The park reaches that terminal state by being MINTED terminal, which
        // is why there is no release here any more. There used to be one, and
        // it was always fiction: `release`'s only production caller is the
        // staffing verb, and nothing ever released a routine idle park.
        activate(&db, &manifest, "signal-researcher").await;
        let worker = "signal-researcher".to_owned();
        let manifest_for_settle = manifest.clone();
        let worker_for_settle = worker.clone();
        db.mutate(MutationClass::Reconcile, MutationName("test.external_settle"), move |ledgers| {
            let supervision = supervision::read(ledgers, &manifest_for_settle)?;
            let begun = activity::begin_transition(
                ledgers,
                &manifest_for_settle,
                &supervision,
                &BeginTransitionInput {
                    person_id: worker_for_settle.clone(),
                    action: TransitionAction::Park,
                    reason: activity::IDLE_AUTO_PARK_REASON.to_owned(),
                    to_department_id: None,
                    intent_id: None,
                },
            )?;
            let supervision = supervision::read(ledgers, &manifest_for_settle)?;
            activity::mutate(ledgers, &manifest_for_settle, &supervision, |draft, _ctx, at| {
                let record =
                    draft.transitions.get_mut(&begun.id).expect("the transition just opened");
                assert_eq!(
                    record.status,
                    TransitionStatus::Forced,
                    "a routine idle park is terminal from the moment it is minted"
                );
                let state =
                    draft.people.get_mut(&worker_for_settle).expect("the worker is projected");
                state.last_desired_active = false;
                state.updated_at = at.to_string();
                Ok(true)
            })?;
            Ok(())
        })
        .await
        .expect("commit the external settle");
        db.launch_intent_publish(chiefd_core::store::launch_intent_rows::LaunchIntent {
            version: 1,
            organization: manifest.slug.clone(),
            person_ids: vec![worker.clone()],
            updated_at: "2026-07-28T00:00:00.000Z".to_owned(),
            attributions: BTreeMap::new(),
            extra: BTreeMap::new(),
        })
        .await
        .expect("commit the worker's launch intent");
        // The topology after the settled worker's process is gone.

        let store = ReconcilerFactsStore::new(
            dir.path().join(COMPANY_DB_FILENAME),
            dir.path().to_string_lossy().to_string(),
        );
        let fenced_store = store.clone();
        let fence_slug = manifest.slug.clone();
        let fenced = move || {
            fenced_store.launch_intent_person_ids(&fence_slug, &fence_slug).expect("fence read")
        };
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("the settle pass");

        // The withdrawal half always committed; the bug is the SAME pass
        // resurrecting the worker it just retired.
        assert_eq!(fenced(), std::collections::BTreeSet::new(), "the settled worker is withdrawn");
        assert_eq!(
            desired_active(&db, &worker),
            Some(false),
            "a terminal routine idle park is settled fact, never lapsed-start demand"
        );
        assert!(
            !desires(&report, &worker),
            "nothing restarts the process the settle just retired -- it is ABSENT from the \
             desired set, which is the instruction: {report:?}"
        );
    }

    #[tokio::test]
    async fn the_ceo_stays_desired_active_with_an_empty_launch_intent() {
        // THIS NAME WENT AWAY AND CAME BACK, deliberately, and the round trip is
        // worth recording so the next reader does not read it as churn.
        //
        // The claim is the original one: CEO-only is the fence's strictest legal
        // value and the root is admitted unconditionally, so with NO staffing
        // intent at all the projection still wants exactly the root.
        //
        // #1148 retired it. The operator's "everybody settles" ruling deleted
        // the root's unconditional `ActivityReason::OrganizationRoot` lease, so
        // `active` became a function of demand alone and an empty intent asked
        // for nobody -- root included. This test was renamed
        // `an_empty_launch_intent_desires_nobody_at_all_including_the_root` to
        // say that.
        //
        // The operator then reversed it for the root ONLY, on a live box where
        // the CEO had settled, its pane was gone, and three workers were still
        // running: "CEO can never go to sleep." The lease is back, so the
        // original claim is true again and the original name is the honest one.
        //
        // WHAT DID NOT COME BACK, and is the reason this is not simply a revert:
        // everybody else still settles on the two-minute lease. The reversal is
        // the root and nothing else.
        //
        // So the two live mechanisms are now different in kind, and a fixture
        // should be explicit about which it exercises. THIS ONE IS THE LEASE:
        // the root is desired because it holds a permanent one, not because
        // anything in this test asked for it -- the intent here is empty. Its
        // companion `an_explicit_launch_intent_starts_its_person_from_a_zero_
        // pane_company` is the other mechanism, where a person is desired
        // because something asked.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;

        let store = normalized_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // Default company config is Shadow: commit the desired set, ask for
        // nothing.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert!(!report.applied, "shadow config downgrades the apply request");
        // Shadow publishes the desired set IN FULL and holds; what it withholds
        // is the acting, not the saying. The durable claim this test is about --
        // the CEO is desired and nobody else is -- is asserted below and is
        // unchanged.
        assert!(desires(&report, "chief"), "{:?}", report.notes);
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "the root holds a permanent lease, so an empty intent still wants it"
        );
        let others = db.read(|snapshot| {
            let org = organization::read(snapshot).expect("org");
            let activity = activity::read(snapshot, &org).expect("activity");
            activity
                .people
                .iter()
                .filter(|(id, state)| id.as_str() != "chief" && state.last_desired_active)
                .count()
        });
        assert_eq!(
            others, 0,
            "and an empty intent authorizes nobody ELSE -- the reversal is the root alone, and \
             every other person still needs something to ask for them"
        );
    }

    #[tokio::test]
    async fn an_explicit_launch_intent_starts_its_person_from_a_zero_pane_company() {
        // A launch intent is an explicit operator decision to start this
        // person, not merely a fence that permits some other demand source.
        // Regress the exact #618 failure: a fresh company read the normalized
        // intent successfully, but projected CEO-only because no mailbox or
        // assignment happened to create Requested demand.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;

        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &["it-head"]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // Shadow proves the projection without asking any client to do
        // anything; the desired set below is what Apply would then act on.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert!(!report.applied, "default safety remains shadow in this focused projection test");
        // Shadow publishes the desired set IN FULL and holds; what it withholds
        // is the acting, not the saying. The projection claims below are the
        // point of the test and are untouched.
        assert_eq!(desired_active(&db, "chief"), Some(true), "the root remains resident");
        assert_eq!(
            desired_active(&db, "it-head"),
            Some(true),
            "intent is projected as explicit demand"
        );
        assert_eq!(
            desired_active(&db, "quant-head"),
            Some(false),
            "an unrequested sibling stays stopped"
        );
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "an unrequested report stays stopped"
        );
    }

    #[tokio::test]
    async fn adoption_actuates_nothing_and_keeps_desiring_the_person() {
        // Requirement (4). A pass over a healthy, authorized process is pure
        // adoption: it actuates nothing and still desires the person.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store =
            normalized_intent_store(dir.path(), dir.path(), &manifest, &["signal-researcher"]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        // TOMBSTONE: `!ran_verb("respawn-pane")`, then an empty action stream.
        // Neither exists. A restart is the ACTUATOR's decision, taken by
        // comparing the pane's launch-hash tag against the desired hash, so what
        // chiefd can assert is that it actuated nothing and still desires the
        // same person -- a pass that quietly dropped somebody would satisfy
        // "asks for nothing" and fail the product.
        assert!(desires(&report, "signal-researcher"), "and still desires them: {report:?}");
    }

    // --- pending-mailbox demand in the fence projection ---------------------
    //
    // The live 2026-07-22 actuation stall: the TypeScript launcher feeds
    // pending-mailbox recipients into its activity reconcile as
    // `requestedPersonIds` (`pending-mailbox-wake-requested`), but chiefd's
    // converge projection hardwired `requested_person_ids` to empty, so the
    // native desired set never saw the demand that keeps the fleet awake and
    // every mailbox-woken person projected desired-inactive — a stop, and then
    // the destructive budget refused the whole cycle, forever. These tests pin
    // that the projection now carries CURRENT pending-mailbox demand
    // (recomputed every pass, so it clears on drain and never defeats the
    // shrink half), and that the launch-intent fence still gates it last.

    /// Stage one pending envelope for `person` — the durable shape the
    /// delivery sink writes when mail arrives for someone.
    async fn stage_pending_mail(db: &CompanyDb, manifest: &OrganizationManifest, person: &str) {
        stage_pending_mail_at(db, manifest, person, "2026-07-22T00:00:00.000Z").await;
    }

    /// Stage native pending mail at an exact instant for precedence tests.
    async fn stage_pending_mail_at(
        db: &CompanyDb,
        manifest: &OrganizationManifest,
        person: &str,
        created_at: &str,
    ) {
        let envelope = MailboxEnvelope {
            schema_version: mailbox::MAILBOX_ENVELOPE_SCHEMA_VERSION,
            id: format!("test-mail:{person}"),
            organization: manifest.slug.clone(),
            from_person_id: "chief".to_owned(),
            to: person.to_owned(),
            recipients: vec![person.to_owned()],
            body: "work for you".to_owned(),
            urgency: Urgency::Normal,
            reply_to: None,
            health_incident: None,
            created_at: created_at.to_owned(),
        };
        db.mutate(MutationClass::Normal, MutationName("test.stage_mail"), move |ledgers| {
            mailbox::enqueue(ledgers, &envelope)?;
            Ok(())
        })
        .await
        .expect("stage mail");
    }

    /// Apply the real atomic commanded-stop writer and verify it landed.
    async fn command_stop(db: &CompanyDb, person: &str, at: &str) {
        let outcome = db
            .shutdown_person(
                person.to_owned(),
                ShutdownKind::Commanded { intent_id: format!("person-stop:test:{person}:{at}") },
                at.to_owned(),
                "chief".to_owned(),
            )
            .await
            .expect("commanded stop commits");
        assert!(matches!(outcome, ShutdownOutcome::Applied { .. }));
    }

    /// Stage one pending envelope THE WAY THE PRODUCT DOES: through
    /// `CompanyDb::mailbox_delta`, the exact writer `/v1/org/mailbox/delta`
    /// calls when one person messages another.
    ///
    /// This is deliberately NOT [`stage_pending_mail`], and the difference is
    /// the whole carlos defect. `mailbox::enqueue` writes through the in-memory
    /// `Ledgers`; `mailbox_delta` writes the `mailbox` table directly on its
    /// transaction and never touches that ledger, which is hydrated exactly once
    /// in `CompanyDb::open`. Every test that staged mail the first way passed
    /// while every message a real company sent was invisible to the wake.
    async fn deliver_mail_over_the_wire(
        db: &CompanyDb,
        manifest: &OrganizationManifest,
        person: &str,
    ) {
        let entry = chiefd_core::store::mailbox_rows::MailboxEntry {
            envelope: MailboxEnvelope {
                schema_version: mailbox::MAILBOX_ENVELOPE_SCHEMA_VERSION,
                id: format!("wire-mail:{person}"),
                organization: manifest.slug.clone(),
                from_person_id: "chief".to_owned(),
                to: person.to_owned(),
                recipients: vec![person.to_owned()],
                body: "Welcome to your role. Report back once your team is set up.".to_owned(),
                urgency: Urgency::Normal,
                reply_to: None,
                health_incident: None,
                created_at: "2026-07-22T00:00:00.000Z".to_owned(),
            },
            person: person.to_owned(),
            state: "pending".to_owned(),
            updated_at: 1,
            extra: BTreeMap::new(),
        };
        db.mailbox_delta(
            person.to_owned(),
            vec![entry],
            Vec::new(),
            "2026-07-22T00:00:00.000Z".to_owned(),
            "chief".to_owned(),
        )
        .await
        .expect("the CEO's delivery commits");
    }

    #[tokio::test]
    async fn pending_mailbox_demand_is_projected_desired_active_and_adopted() {
        // A person with pending mail the fence admits is effective demand: the
        // projection keeps them desired-active WITHOUT any explicit
        // start-person, and the pass adopts their live process (no stop, no
        // restart) — the convergence shape the budget-refused stall never
        // reached.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        stage_pending_mail(&db, &manifest, "signal-researcher").await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store =
            normalized_intent_store(dir.path(), dir.path(), &manifest, &["signal-researcher"]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert!(report.applied, "adoption is within budget: {:?}", report.notes);
        // TOMBSTONE: a five-verb argv loop pinned "adopts, never kill/respawn/
        // new-session". An empty action stream is that statement, exactly.
        assert!(desires(&report, "signal-researcher"), "{:?}", report.notes);
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "pending mail is requested demand in the native projection"
        );
    }

    #[tokio::test]
    async fn commanded_stop_wins_over_older_and_equal_native_pending_mail() {
        for created_at in ["2026-08-15T09:59:59.000Z", "2026-08-15T10:00:00.000Z"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let manifest = northstar_manifest(EPOCH);
            let db = Arc::new(open_db(dir.path(), &manifest.slug));
            seed_company(&db, manifest.clone()).await;
            activate(&db, &manifest, "signal-researcher").await;
            command_stop(&db, "signal-researcher", "2026-08-15T09:00:00.000Z").await;
            stage_pending_mail_at(&db, &manifest, "signal-researcher", created_at).await;
            command_stop(&db, "signal-researcher", "2026-08-15T10:00:00.000Z").await;

            let store = normalized_intent_store(dir.path(), dir.path(), &manifest, &[]);
            let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
                .with_launch_intent_store(Some(store));
            let report = actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .expect("cycle");

            assert_eq!(
                desired_active(&db, "signal-researcher"),
                Some(false),
                "native mail at {created_at} cannot undo the stop watermark"
            );
            assert!(
                !desires(&report, "signal-researcher"),
                "the desired set excludes the stopped live person, so the client plans one kill: {report:?}"
            );
        }
    }

    /// A DRAINED ENVELOPE IS NOT DEMAND, however long chiefd's own memory keeps
    /// calling it pending.
    ///
    /// The live defect, on the operator's company, 2026-08-20: thirteen people
    /// desired-active with `agent_quiet_at` twenty minutes old, `idle_since`
    /// NULL, and ZERO pending mailbox rows in SQL. `mailbox::enqueue` had put a
    /// `pending` row for each of them in the in-memory ledger — a fired
    /// reminder, a health incident — their pane had drained it through
    /// `/v1/org/mailbox/delta`, which writes SQL and never touches that ledger,
    /// and nothing else ever moved the in-memory row. Read as demand it was a
    /// `Requested` reason nobody could clear, so `idle_since` was recomputed as
    /// NULL every pass, the quiet lease never expired, and they never settled.
    ///
    /// This is that exact world: the ledger says pending, the table says
    /// accepted, and the table wins.
    #[tokio::test]
    async fn mail_the_recipient_has_already_drained_is_not_demand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        // chiefd's own delivery, into chiefd's own memory — and it stays there,
        // pending, for the life of the process.
        stage_pending_mail(&db, &manifest, "signal-researcher").await;
        let still_pending_in_memory = db.read(|snapshot| {
            chiefd_core::store::mailbox::pending_for(snapshot, "signal-researcher").len()
        });
        assert_eq!(
            still_pending_in_memory, 1,
            "precondition — the in-memory ledger holds this envelope pending"
        );

        // The pane drained it. The table is the record of that.
        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &[],
            &[("signal-researcher", "accepted", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "a drained envelope must not hold its recipient up: {:?}",
            report.notes
        );
    }

    // TOMBSTONE: `native_pending_mail_newer_than_commanded_stop_wakes_the_person`
    // staged the same envelope through `mailbox::enqueue` into the company's
    // in-memory ledger and asserted the same outcome as
    // `sql_pending_mail_newer_than_commanded_stop_wakes_the_person` above. The
    // two reads it distinguished are one read now — the `mailbox` table — so
    // the pair was one test written twice, and the surviving half is the one
    // whose fixture matches what the daemon does.

    #[tokio::test]
    async fn commanded_stop_with_no_mail_stays_down_and_unrelated_requests_still_wake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        command_stop(&db, "signal-researcher", "2026-08-15T10:00:00.000Z").await;

        let store = normalized_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("stopped cycle");
        assert_eq!(desired_active(&db, "signal-researcher"), Some(false));

        let intent_db =
            open_company_db(&dir.path().join("org.sqlite")).expect("open normalized intent store");
        intent_db
            .execute(
                "INSERT INTO launch_intent(slug, person_id) VALUES(?1, 'it-head')",
                [&manifest.slug],
            )
            .expect("explicit request adds its person");
        intent_db
            .execute(
                "INSERT INTO org_events(slug, seq, entity, entity_id, op, at) \
                 VALUES(?1, 2, 'launch-intent', 'it-head', 'upsert', \
                 '2026-08-15T10:00:01.000Z')",
                [&manifest.slug],
            )
            .expect("explicit request advances the fence event");
        drop(intent_db);
        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("explicit request cycle");
        assert_eq!(
            desired_active(&db, "it-head"),
            Some(true),
            "focus and session-maintenance requests for other people are unchanged"
        );
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "the unrelated request does not widen past its own person"
        );
    }

    #[tokio::test]
    async fn pending_mailbox_demand_never_opens_a_person_the_fence_excludes() {
        // #363/#551: the fence gates last, but a GENUINE durable envelope
        // addressed to a specific person is itself the explicit, per-node
        // decision that authorizes exactly them ("a mailbox message to a
        // stopped person IS work arriving") — the daemon grants exactly that
        // recipient launch intent, bounded by the #363 quiesce watermark, and
        // never opens anybody else.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &[]); // CEO-only fence
        seed_fixture_mailbox(
            dir.path(),
            &manifest,
            &[("signal-researcher", "pending", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "genuine mail grants exactly its recipient (#363)"
        );
        assert_eq!(desired_active(&db, "chief"), Some(true), "the root is unaffected");
        assert_eq!(
            desired_active(&db, "it-head"),
            Some(false),
            "the grant never widens past the mail's own recipient"
        );
    }

    #[tokio::test]
    async fn unwired_legacy_store_eventually_settles_a_stale_roster_to_ceo_only() {
        // THIS NAME WENT AWAY AND CAME BACK. It was briefly
        // `..._settles_a_stale_roster_to_nobody`, and the round trip is
        // recorded here so the next reader does not read it as churn.
        //
        // The floor was "the CEO alone" because of the root's unconditional
        // `OrganizationRoot` lease -- no launch-intent store is wired in this
        // branch and `Unfenced` deliberately contributes no names of its own,
        // so the lease was the only thing holding the root up. #1148 deleted
        // the lease on the "everybody settles" ruling, which made the honest
        // floor EMPTY, and this test was renamed to say so.
        //
        // The operator then reversed that for the ROOT ONLY, on a live box
        // where the CEO had settled, its pane was gone, and three workers were
        // still running: "CEO can never go to sleep." So the floor is CEO-only
        // again and the original name is the accurate one.
        //
        // The property this test protects never moved: idle trends DOWN
        // through the ordinary settle machinery rather than instantly, and
        // every non-root person still settles on the two-minute lease. Only
        // where it stops has changed, and changed back.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate_many(&db, &manifest, &["quant-head", "signal-researcher", "it-head"]).await;
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()));

        // Pass 1 (this actuator's first, i.e. the cold-restart pass itself):
        // the native reason-only projection does NOT instantly collapse a
        // stale roster -- it enters the SAME graceful idle-park settle
        // machinery a genuinely-idle worker uses, which cannot produce
        // `desired_active=false` in one pass by construction.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("native reason-only cold-start pass");
        assert_eq!(desired_active(&db, "chief"), Some(true));
        for person_id in ["quant-head", "signal-researcher", "it-head"] {
            assert_eq!(
                desired_active(&db, person_id),
                Some(true),
                "the cold-restart pass alone never instantly collapses {person_id}"
            );
        }
        assert!(
            !report.notes.iter().any(|note| note.contains("withdrawn")),
            "no settle decision commits on the cold-restart pass itself: {:?}",
            report.notes
        );

        // THE TRIGGER CHANGED, AND THIS IS THE HONEST ACCOUNT OF IT.
        //
        // The settle countdown now starts when the AGENT reports it went quiet
        // and at no other moment. These three never beat, so on the timer alone
        // they would never settle -- and that is not a regression, because
        // under desired-state-only there is no such thing as a "stale roster"
        // that a timer must collapse. chiefd's desired set IS the truth: it
        // says these three should run, the actuator makes that true, they come
        // up, and they then settle through the ordinary path like anybody else.
        // What used to be a stale roster is now simply a desired state that has
        // not been realized yet.
        //
        // So the agents report they went quiet, and from there this drives the
        // SAME machinery to the SAME terminal state. The property this test
        // exists to protect -- idle trends to CEO-only -- is unchanged and is
        // still asserted below; only what starts the clock has moved, from
        // chiefd's own bookkeeping to the agents' own reports.
        for person_id in ["quant-head", "signal-researcher", "it-head"] {
            agent_settled(&db, person_id).await;
        }

        // Drive the settle machinery to its terminal state: quiet lease
        // expiry -> routine idle park admitted, born terminal, committing the
        // withdrawal in the same transaction. Bounded
        // routine-park admission (activity.rs's round-robin in-flight cap,
        // `ActivityReason::MaintenanceBackpressure`) admits at most a bounded
        // number of parks per pass, so three stale people are not guaranteed to
        // finish on the same pass -- loop the same settle-window advance until
        // every one of them has, bounded so a real regression (nobody ever
        // settling) fails loudly instead of hanging.
        let settle_window_ms = (activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS
            + activity::HANDOFF_GRACE_MS
            + activity::ORGANIZATION_SETTLED_IDLE_STOP_LEASE_MS
            + 3) as u64;
        let mut settled = false;
        for _ in 0..8 {
            manual.advance(std::time::Duration::from_millis(settle_window_ms));
            actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .expect("settle-driving pass");
            if ["quant-head", "signal-researcher", "it-head"]
                .iter()
                .all(|person_id| desired_active(&db, person_id) == Some(false))
            {
                settled = true;
                break;
            }
        }

        // The eventual collapse the design actually guarantees: CEO-only, on
        // the settle machinery's own timescale, not the cold-restart pass's.
        assert!(
            settled,
            "the stale roster never settled to CEO-only within a generous bound on passes"
        );
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "the CEO remains the durable control plane -- it never sleeps, so the floor is the \
             root and the root alone"
        );
    }

    #[tokio::test]
    async fn unwired_legacy_store_admits_only_the_person_with_current_native_mail_reason() {
        // The no-legacy branch is not a permanent CEO-only mode: real native
        // work remains an explicit named reason and may restart exactly its
        // recipient, without reviving a department or the rest of the roster.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        stage_pending_mail(&db, &manifest, "signal-researcher").await;
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("native mail reason pass");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "the envelope's own recipient, and the mail is the reason"
        );
        // TWO DIFFERENT MECHANISMS IN ONE ASSERTION BLOCK, which is the thing
        // to keep straight here. `signal-researcher` above is desired because
        // something ASKED -- one envelope. The root below is desired because it
        // holds a permanent lease, and no mail is addressed to it.
        //
        // The test's own subject is the first mechanism, and it is unchanged:
        // real native work is an explicit named reason that restarts exactly
        // its recipient, reviving neither a department nor the rest of the
        // roster. `quant-head` and `it-head` are the proof.
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "the root's lease, not this envelope -- the CEO never sleeps"
        );
        assert_eq!(desired_active(&db, "quant-head"), Some(false));
        assert_eq!(desired_active(&db, "it-head"), Some(false));
    }

    #[tokio::test]
    async fn normalized_quiesce_row_blocks_pre_reset_mailbox_launch_grants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        stage_pending_mail(&db, &manifest, "signal-researcher").await;
        db.goal_delivery_quiesce_publish(GoalDeliveryQuiesce {
            version: 1,
            organization: manifest.slug.clone(),
            quiesced_at: "2026-07-23T00:00:00.000Z".to_owned(),
            extra: Default::default(),
        })
        .await
        .expect("publish normalized quiesce row");

        let store = normalized_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "mail predating the normalized reset watermark cannot reauthorize its recipient"
        );
    }

    #[tokio::test]
    async fn pending_cadence_reemission_never_grants_launch_intent() {
        // #551: a launcher cadence re-emission restates standing state; it must
        // NOT read as demand and must NOT earn a grant — otherwise a settling
        // person's own unread cadence mail re-pins it every pass and CEO-only
        // is unreachable. The three `manager-check-in`/`manager-people-check`/
        // `manager-goal-watch` ids this used to stage were emitted by the
        // protected goal loop and nothing emits them now; `supervision-` is the
        // surviving re-emission prefix.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        for (index, id) in
            ["supervision-cadence-1", "supervision-cadence-2", "supervision-cadence-3"]
                .into_iter()
                .enumerate()
        {
            let envelope = MailboxEnvelope {
                schema_version: mailbox::MAILBOX_ENVELOPE_SCHEMA_VERSION,
                id: id.to_string(),
                organization: manifest.slug.clone(),
                from_person_id: "launcher".to_owned(),
                to: "signal-researcher".to_owned(),
                recipients: vec!["signal-researcher".to_owned()],
                body: format!("cadence {index}"),
                urgency: Urgency::Normal,
                reply_to: None,
                health_incident: None,
                created_at: "2026-07-22T00:00:00.000Z".to_owned(),
            };
            db.mutate(MutationClass::Normal, MutationName("test.stage_mail"), move |ledgers| {
                mailbox::enqueue(ledgers, &envelope)?;
                Ok(())
            })
            .await
            .expect("stage cadence mail");
        }

        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &[]);
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "cadence mail is never demand and never a grant"
        );
        assert_eq!(desired_active(&db, "chief"), Some(true), "the root is unaffected");
    }

    // --- normalized mailbox demand in the fence projection (BUG-10) --------
    //
    // The live 2026-07-22 standoff: BUG-1's fix sourced `requested_person_ids`
    // from the company actor's `mailbox` rows only, but person-to-person work
    // mail also lives in the shared normalized mailbox the intercom path
    // writes. A person the CEO had just hired, started, and mailed had no
    // native-visible demand, so the projection made them desired-inactive, the
    // pass asked for a stop, and the destructive budget refused the whole cycle
    // forever. These tests run the REAL `ConvergeActuator` against a normalized
    // `org.sqlite` fixture so the whole chain — shared mailbox row → union into
    // requested demand → activity::reconcile → desired roster → action stream —
    // is exercised.

    /// A shared normalized store holding launch intent plus one typed mailbox
    /// row per `(person, state, created_at)` fixture entry.
    fn normalized_store_with_mailboxes(
        dir: &std::path::Path,
        data_root: &std::path::Path,
        manifest: &OrganizationManifest,
        person_ids: &[&str],
        mailbox_rows: &[(&str, &str, &str)],
    ) -> ReconcilerFactsStore {
        let store = normalized_intent_store(dir, data_root, manifest, person_ids);
        seed_fixture_mailbox(dir, manifest, mailbox_rows);
        store
    }

    /// Put typed mailbox rows into the normalized store the projection reads.
    ///
    /// This is how a test stages mail now, and the only way. Staging it through
    /// `mailbox::enqueue` into the company's in-memory ledger used to work
    /// because the pass unioned that ledger into its demand set; that half is
    /// deleted (it could never see a drain, so it pinned real companies awake),
    /// and a fixture that writes one file while the pass reads another proves
    /// nothing at all.
    fn seed_fixture_mailbox(
        dir: &std::path::Path,
        manifest: &OrganizationManifest,
        mailbox_rows: &[(&str, &str, &str)],
    ) {
        let path = dir.join("org.sqlite");
        let conn = open_company_db(&path).expect("open writable fixture");
        for (index, (person, state, created_at)) in mailbox_rows.iter().enumerate() {
            let id = format!("fixture-mail-{index}");
            conn.execute(
                "INSERT INTO mailbox(\
                    slug, envelope_id, id, person, from_person_id, to_person_id, \
                    message, urgency, created_at, state, updated_at\
                 ) VALUES(?1, ?2, ?3, ?4, 'chief', ?4, 'fixture mail', 'normal', ?5, ?6, 1)",
                rusqlite::params![
                    manifest.slug,
                    format!("{id}@{person}"),
                    id,
                    person,
                    created_at,
                    state,
                ],
            )
            .expect("insert mailbox row");
        }
    }

    #[tokio::test]
    async fn normalized_mailbox_demand_is_projected_desired_active_and_adopted() {
        // (a) A person whose ONLY pending mail sits in the shared mailbox —
        // the exact live BUG-10 shape — must project desired-active on demand
        // alone, and the pass must ADOPT their live process: no stop, no
        // restart.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &["signal-researcher"],
            &[("signal-researcher", "pending", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert!(report.applied, "adoption is within budget: {:?}", report.notes);
        // TOMBSTONE: a five-verb argv loop pinned "adopts, never kill/respawn/
        // new-session".
        assert!(desires(&report, "signal-researcher"), "{:?}", report.notes);
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "normalized pending mail is requested demand in the projection"
        );
    }

    #[tokio::test]
    async fn commanded_stop_wins_over_older_and_equal_sql_pending_mail() {
        for created_at in ["2026-08-15T09:59:59.000Z", "2026-08-15T10:00:00.000Z"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let manifest = northstar_manifest(EPOCH);
            let db = Arc::new(open_db(dir.path(), &manifest.slug));
            seed_company(&db, manifest.clone()).await;
            activate(&db, &manifest, "signal-researcher").await;
            command_stop(&db, "signal-researcher", "2026-08-15T09:00:00.000Z").await;
            command_stop(&db, "signal-researcher", "2026-08-15T10:00:00.000Z").await;
            let store = normalized_store_with_mailboxes(
                dir.path(),
                dir.path(),
                &manifest,
                &[],
                &[("signal-researcher", "pending", created_at)],
            );
            let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
                .with_launch_intent_store(Some(store));
            let report = actuator
                .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
                .await
                .expect("cycle");

            assert_eq!(
                desired_active(&db, "signal-researcher"),
                Some(false),
                "SQL mail at {created_at} cannot undo the stop watermark"
            );
            assert!(
                !desires(&report, "signal-researcher"),
                "the stopped live person remains a single client kill target: {report:?}"
            );
        }
    }

    #[tokio::test]
    async fn sql_pending_mail_newer_than_commanded_stop_wakes_the_person() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        command_stop(&db, "signal-researcher", "2026-08-15T10:00:00.000Z").await;
        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &[],
            &[("signal-researcher", "pending", "2026-08-15T10:00:00.001Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));
        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "new SQL-backed work is a later decision and relaunches its recipient"
        );
    }

    #[tokio::test]
    async fn company_and_shared_mailbox_demand_union_in_the_projection() {
        // (b) Demand is the UNION of both stores: native pending mail for one
        // person plus shared pending mail for another keeps BOTH
        // desired-active in the same pass.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;
        stage_pending_mail(&db, &manifest, "it-head").await; // native demand

        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &["it-head", "signal-researcher"],
            &[("signal-researcher", "pending", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(desired_active(&db, "it-head"), Some(true), "native demand still projects");
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "shared normalized demand projects alongside it"
        );
    }

    /// (c) THIS TEST USED TO ASSERT THE CARLOS DEFECT, AND ITS OLD ASSERTION IS
    /// WHY THE DEFECT SURVIVED.
    ///
    /// It read: shared pending mail for a person the launch-intent fence does
    /// not admit still projects desired-INACTIVE. That is exactly the live
    /// failure — a message to a settled person whose intent had been withdrawn,
    /// leaving them down for ever — written down as the expected outcome.
    ///
    /// The rule the product actually holds was already stated in this file, on
    /// the native twin (`pending_mailbox_demand_never_opens_a_person_the_fence_
    /// excludes`, #363/#551): "a genuine durable envelope addressed to a
    /// specific person IS work arriving and is itself the explicit, per-node
    /// decision that authorizes exactly them". The two tests asserted OPPOSITE
    /// outcomes for the same fact — one pending row in the same `mailbox` table
    /// — differing only in which of two reads of that table happened to see it.
    /// In production those two reads are the same file, and the read this test
    /// exercises is the one every intercom message actually lands in.
    ///
    /// So the assertion is corrected to the twin's, and NOTHING the fence
    /// guaranteed is given up. "The fence gates last" was never "mail cannot
    /// authorize"; it is "nothing widens past what was authorized", and that is
    /// what the `it-head` assertion below pins — the grant names the mail's own
    /// recipient and nobody else, which is the property that keeps one envelope
    /// from starting a fleet.
    #[tokio::test]
    async fn normalized_mailbox_demand_grants_exactly_its_recipient_and_never_widens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;

        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            // The root's own start decision, and nobody else's -- what genesis
            // leaves behind. It used to read `&[]` with the comment "CEO-only
            // fence", which was true only while the root's residency came from
            // the deleted `OrganizationRoot` lease rather than from a row.
            &["chief"],
            &[("signal-researcher", "pending", "2026-07-22T00:00:00.000Z")],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("cycle");

        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(true),
            "the envelope authorizes its own recipient, whatever the fence said before it arrived"
        );
        assert_eq!(
            desired_active(&db, "chief"),
            Some(true),
            "the root is unaffected -- it holds its own start decision, and somebody else's mail \
             neither adds to it nor takes it away"
        );
        assert_eq!(
            desired_active(&db, "it-head"),
            Some(false),
            "and it widens no further: one envelope starts one person, never the fleet"
        );
        assert_eq!(
            desired_active(&db, "quant-head"),
            Some(false),
            "nor the recipient's own management chain"
        );
    }

    #[tokio::test]
    async fn a_terminal_normalized_mailbox_row_is_not_demand_and_does_not_hide_pending_demand() {
        // (d) Typed terminal rows are never demand and do not hide a pending
        // recipient in the same normalized store. The terminal row's person
        // is deliberately NOT in the launch-intent fence: under F8 (arch
        // Step 5) a fenced person's start decision is itself demand, which
        // would mask the terminal row entirely — unfenced, the ONLY path to
        // desired-active is the mailbox, so a terminal row misread as demand
        // would both grant the fence (#551) and wake them, and this test
        // catches exactly that.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let db = Arc::new(open_db(dir.path(), &manifest.slug));
        seed_company(&db, manifest.clone()).await;

        let store = normalized_store_with_mailboxes(
            dir.path(),
            dir.path(),
            &manifest,
            &["it-head"],
            &[
                ("signal-researcher", "rejected", "2026-07-22T00:00:00.000Z"),
                ("it-head", "pending", "2026-07-22T00:00:00.000Z"),
            ],
        );
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("a terminal normalized mailbox row must not fail the pass");

        assert_eq!(desired_active(&db, "it-head"), Some(true), "the good row's demand reads");
        assert_eq!(
            desired_active(&db, "signal-researcher"),
            Some(false),
            "the terminal row yields no demand for that person"
        );
    }

    // TOMBSTONE (#751-P4): the normalized reflection durability gate ---------
    //
    // Three tests lived here — `a_native_durable_handoff_unblocks_the_cycle`,
    // `a_launcher_published_handoff_unblocks_the_cycle`, and
    // `without_a_memory_row_the_cycle_still_refuses_closed` — plus their
    // `plant_applied_transition(durable: bool)` helper. They pinned an
    // invariant that no longer exists: that an APPLIED transition must be
    // backed by a matching `reflection_handoffs`/`reflection_handoff_items`
    // row, that the cycle refuses closed (`handoff-not-durable`) when the row
    // is missing, and that a launcher-published ledger whose handoff was only
    // ever embedded (never written by this process's own native call) still
    // satisfies it. The handoff payload, both SQL tables, and the gate are
    // deleted with the reflection concept, so all three tests asserted on
    // machinery that is gone: there is no content to be durable, and an
    // applied transition can no longer be "missing" anything.
    //
    // Nothing was lost by deleting rather than porting them. Their subject was
    // the durability of the payload, not the transition state machine — the
    // state machine's own coverage (park admitted -> released or forced ->
    // launch intent withdrawn -> process stopped) lives in the settle and
    // teardown tests above and is untouched. Do not reintroduce a durability
    // gate here; if a future invariant needs an applied transition to carry
    // proof of something, that proof belongs in the transition row itself.

    // --- launch-intent authorization is the whole gate ----------------------

    #[tokio::test]
    async fn an_unauthorized_running_pane_is_still_killed() {
        // A person whom the launch-intent fence does NOT admit is stopped,
        // whatever else is true of them. The fence is the only authorization.
        //
        // THE TEARDOWN IS NOW IMMEDIATE. This used to take two passes, the
        // first asserting `desired_people == 0` while `bounded_idle_retention`
        // kept the unauthorized process alive to serve a quiet lease. That
        // retention is deleted, so the stop lands on the pass that sees the
        // withdrawal. The protection this test pins is unchanged and is if
        // anything now stronger: the unauthorized person is stopped, never
        // restarted, and never authorized -- and is no longer left running for
        // up to two minutes after the fence refused them.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = northstar_manifest(EPOCH);
        let manual = Arc::new(ManualClock::default());
        let clock: SharedClock = manual.clone();
        let db = Arc::new(
            CompanyDb::open(&manifest.slug, &dir.path().join(COMPANY_DB_FILENAME), clock)
                .expect("open company db"),
        );
        seed_company(&db, manifest.clone()).await;
        activate(&db, &manifest, "signal-researcher").await; // was running
        safety::set_actuation_config(&db, safety::ActuationMode::Apply, false, false)
            .await
            .expect("apply");

        let store = post_genesis_intent_store(dir.path(), dir.path(), &manifest, &[]); // intent withdrawn: CEO-only fence
        let actuator = ConvergeActuator::new(db.clone(), config(dir.path()))
            .with_launch_intent_store(Some(store));

        // ONE PASS: the de-authorized process is asked to stop.
        let report = actuator
            .reconcile(&duty_ctx(&db, &manifest.slug), ActuationMode::Apply)
            .await
            .expect("settle pass");

        assert!(report.applied, "one correct stop is not a budget refusal: {:?}", report.notes);
        // TOMBSTONE: `ran_verb("kill-pane")` / `!ran_verb("respawn-pane")`. The
        // count plus the absent launch note carry both halves: exactly one
        // action, and it brings nobody up — so it is a stop, never a restart.
        assert_eq!(
            report.desired_people, 1,
            "the de-authorized person is asked to stop: {:?}",
            report.notes
        );
        assert!(
            !desires(&report, "signal-researcher"),
            "stopped, never restarted: the de-authorized person is absent from the desired set"
        );
        assert_eq!(desired_active(&db, "signal-researcher"), Some(false));
    }
}

/// #107 — the actuation note must name WHO is launched, not just how many.
///
/// Re-based a second time, onto [`DesiredPerson`]. #751/P10 re-based these from
/// pane steps onto `RuntimeAction`; the actions are now deleted too, because a
/// verb is a statement about a TRANSITION and only the actuator knows the
/// current state. The note's subject is the DESIRED SET, and the question it
/// answers is unchanged: which people, by name, in order, with duplicates
/// intact and a declared cap. Every assertion below survives; the two that
/// drove `Stop`/`StopAll` do not, because absence from the set IS the stop and
/// there is no stop value to pass.
mod launch_subjects_note {
    use chiefd_core::runtime::actuation::DesiredPerson;

    use crate::converge_apply::cycle::{
        launch_subjects, launch_subjects_note, MAX_NAMED_LAUNCH_SUBJECTS,
    };

    fn desired(person: &str) -> DesiredPerson {
        DesiredPerson { person_id: person.to_string(), launch_hash: format!("hash-{person}") }
    }

    #[test]
    fn a_pass_that_launches_nobody_stays_silent() {
        // #367: idle must not add noise. Nobody desired -> no note at all.
        assert_eq!(launch_subjects_note(&[]), None);
    }

    #[test]
    fn the_note_names_each_launched_person_in_plan_order() {
        let people = vec![desired("chief"), desired("quant-head")];
        assert_eq!(launch_subjects(&people), vec!["chief", "quant-head"]);
        assert_eq!(launch_subjects_note(&people).expect("note"), "launching: chief, quant-head");
    }

    #[test]
    fn a_person_launched_twice_appears_twice() {
        // THE #107 CASE. The incident's pass logged `planned=2 actuated=2` and
        // nothing could distinguish this from two different people. Deduping
        // here would erase the only signal that matters.
        let people = vec![desired("validator-2"), desired("validator-2")];
        assert_eq!(launch_subjects(&people), vec!["validator-2", "validator-2"]);
        assert_eq!(
            launch_subjects_note(&people).expect("note"),
            "launching: validator-2, validator-2",
            "a double-spawn must be readable as a double-spawn"
        );
    }

    #[test]
    fn one_person_twice_is_distinguishable_from_two_people() {
        let doubled = vec![desired("validator-2"), desired("validator-2")];
        let distinct = vec![desired("validator-2"), desired("quant-head")];
        assert_ne!(
            launch_subjects_note(&doubled),
            launch_subjects_note(&distinct),
            "the note must answer the question `planned=2` could not"
        );
    }

    #[test]
    fn over_the_cap_the_note_declares_its_own_truncation() {
        // Exercise the cap by EXCEEDING it: an unexercised cap is
        // indistinguishable from no cap.
        let over = MAX_NAMED_LAUNCH_SUBJECTS + 3;
        let people: Vec<DesiredPerson> =
            (0..over).map(|i| desired(&format!("person-{i}"))).collect();
        let note = launch_subjects_note(&people).expect("note");

        assert!(note.contains("person-0"), "the first subject is named: {note}");
        assert!(
            note.contains(&format!("person-{}", MAX_NAMED_LAUNCH_SUBJECTS - 1)),
            "the last named subject is the Nth: {note}"
        );
        assert!(
            !note.contains(&format!("person-{}", MAX_NAMED_LAUNCH_SUBJECTS)),
            "subjects past the cap are not named: {note}"
        );
        assert!(note.ends_with("(+3 more)"), "the note says what it dropped: {note}");
    }

    #[test]
    fn exactly_at_the_cap_nothing_is_dropped_and_no_marker_is_added() {
        let people: Vec<DesiredPerson> =
            (0..MAX_NAMED_LAUNCH_SUBJECTS).map(|i| desired(&format!("person-{i}"))).collect();
        let note = launch_subjects_note(&people).expect("note");
        assert!(!note.contains("more)"), "no truncation marker when nothing was dropped: {note}");
    }
}
