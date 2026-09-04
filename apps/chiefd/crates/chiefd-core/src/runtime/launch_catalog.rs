//! The per-person launch catalog, as a published FACT (#751/P8).
//!
//! # Why this is a wire type and not a moved function
//!
//! Before P8 the catalog was a value the daemon built and consumed in the same
//! process: `converge_apply::cycle::build_launch_catalog` produced it and the
//! in-process interpreter spawned from it. After P8 the interpreter lives in
//! `chief-cli`, and the obvious move — port the builder to the client — is the
//! one thing that must not happen. The catalog's core is a **fail-closed
//! on-disk gate**: `resource_catalog::read_materialized_resources_for_launch`
//! reads each person's materialized home under the daemon's data root (the
//! required directory set, the generated theme trio, the session-resume
//! state) and answers `None` when the person is not properly materialized.
//! Materialization is the daemon's job. A client that
//! re-implemented that read would be a second reader of the daemon's private
//! on-disk state and a second answer to "may this person launch" — and the
//! second answer is the one nobody updates.
//!
//! So chiefd derives it where materialization happens and publishes it, exactly
//! as [`super::roster::DesiredRoster`] publishes the desired set. The client
//! consumes facts; it never re-derives the gate.
//!
//! # Why this type lives in `chiefd-core`
//!
//! Same reason [`super::roster::DesiredRoster`] and
//! [`super::actuation::RuntimeActionPlan`] do, and it is a dependency fact
//! rather than a preference: `chiefd-host` PRODUCES this value and `chiefd-api`
//! SERIALIZES it, and `chiefd-core` is the only crate both already depend on.
//! Putting it in `chiefd-host` would make every future serializer of a runtime
//! fact reach through the host crate for a struct with no host in it; putting
//! it in `chiefd-api` would make the producer depend on the surface. It is also
//! the honest layering: this type is pure data with no filesystem in it, and
//! `chiefd-core` is where the pure half of the runtime engine lives.
//!
//! # Absence is a NAMED refusal, never a silence
//!
//! [`LaunchCatalog::roster`] carries every person the builder iterated, and
//! [`LaunchCatalog::refusals`] carries the re-derived reason for each one the
//! gate declined. That pair is not decoration — it is the difference between
//! *"this person was never a candidate for lookup"* and *"this person was
//! looked up and the on-disk gate refused them, because `workspace` is
//! missing"*. Collapsing the two into one interchangeable "no launch spec"
//! message once sent an engineer hunting inside a function that was never
//! called (#52), so the distinction travels on the wire rather than being
//! reconstructed by whoever reads the failure.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The catalog's wire schema version.
///
/// Bumped when a field changes meaning, never merely when one is added: a
/// client that decodes this body decodes it strictly, and a peer service in the
/// same workspace has no tolerant second arm.
pub const LAUNCH_CATALOG_SCHEMA_VERSION: u32 = 2;

/// Whether Chief could identify the model Pi will use for one person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersonModelState {
    /// A complete provider/model pair was selected.
    Selected,
    /// No transcript or settings source selected a pair, so Pi chooses its default.
    PiDefault,
    /// A source was unreadable, malformed, incomplete, outside its root, or over budget.
    Unavailable,
}

/// The backend-owned model fact displayed for one roster person.
///
/// The card receives this value, never the transcript or settings paths used
/// to derive it. Partial strings are retained on `Unavailable` so the typed
/// fact does not silently rewrite an incomplete source into a different one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonModel {
    /// Resolution outcome.
    pub state: PersonModelState,
    /// Provider exactly as Pi recorded it.
    pub provider: Option<String>,
    /// Model exactly as Pi recorded it.
    pub model: Option<String>,
}

impl PersonModel {
    /// Pi has no selected pair and will use its own default.
    #[must_use]
    pub const fn pi_default() -> Self {
        Self { state: PersonModelState::PiDefault, provider: None, model: None }
    }

    /// Chief could not safely resolve the pair.
    #[must_use]
    pub const fn unavailable(provider: Option<String>, model: Option<String>) -> Self {
        Self { state: PersonModelState::Unavailable, provider, model }
    }

    /// One complete pair, preserving both strings.
    #[must_use]
    pub fn selected(provider: String, model: String) -> Self {
        Self { state: PersonModelState::Selected, provider: Some(provider), model: Some(model) }
    }
}

/// One `NAME=value` pane environment assignment.
///
/// A LIST of named pairs, deliberately, not a JSON object. The order is
/// load-bearing: these become `/usr/bin/env NAME=value …` argv words, where a
/// later assignment overrides an earlier one, and a JSON object guarantees no
/// order at all. `COLORTERM` in particular is emitted by the client *before*
/// this list precisely so a catalog-supplied value can still win — a fact that
/// is only true if the list stays a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvAssignment {
    /// The variable name.
    pub name: String,
    /// Its non-secret value. Invariant 32: no credential is ever carried here.
    /// Credentials are Pi's own business — chiefd holds none.
    pub value: String,
}

impl EnvAssignment {
    /// One assignment.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { name: name.into(), value: value.into() }
    }
}

/// Everything a client needs to launch ONE person, and nothing about where it
/// is displayed.
///
/// Paths travel as strings rather than as `PathBuf`: this is a wire, the
/// consumer renders every one of them straight into argv through the same lossy
/// display the argv builder already uses, and a non-UTF-8 path that cannot be
/// spelled cannot be launched either.
///
/// No session, no socket, no window, no pane, no layout — the same absence
/// `roster.rs` documents, for the same reason. chiefd says who may run and with
/// what; the client decides where it shows up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchEntry {
    /// The pinned pi binary to exec in the pane.
    pub pi_binary: String,
    /// The non-Chief person's home. It is their CWD and their session store,
    /// and since #1307 it is NOT a Pi agent directory: chief no longer
    /// redirects Pi's config scope, so they inherit the operator's own.
    pub pi_home: String,
    /// The person's workspace, used as the pane's working directory.
    pub workspace: String,
    /// `<organization name> · <person title>`, passed as `--name`.
    pub display_name: String,
    /// The person's display name, for the fresh-session initial message.
    pub person_name: String,
    /// This person's identity accent, `#rrggbb`.
    ///
    /// The COLOUR, not a path to a file holding it. It used to travel as the
    /// generated theme trio, which the sidebar then opened and parsed to
    /// recover this one hex; chief writes no theme file now, so the one
    /// allocator the daemon already runs publishes its answer directly. `None`
    /// only when the palette and its hue rotations are exhausted — the sidebar
    /// draws its explicit no-accent ground for that person.
    pub accent: Option<String>,
    /// Granted tool names for `--tools` — the only capability argument.
    pub tools: Vec<String>,
    /// Exact shipped extension source paths, emitted as repeated
    /// `--extension <path>` arguments in this order.
    pub extensions: Vec<String>,
    /// The transcript to resume (`--session`), or `None` to start with the
    /// fresh-session initial message instead.
    pub session: Option<String>,
    /// Whether a message is waiting unread in this person's mailbox.
    ///
    /// THE WHOLE OF "does this person have assigned work". Company goals were
    /// deleted outright by #1047 — `manager_goals`, `delegated_goals`,
    /// `goal_watches` and `goal_intents` are dropped, not deprecated — so a
    /// pending envelope is the only durable claim on a person's attention that
    /// outlives their Pi session. Nothing discoverable counts: a repository or
    /// a source tree the person can see is plumbing they were not given.
    ///
    /// It rides HERE, beside `session`, because the client's fresh-session
    /// message has to choose between "get to work" and "you are up, do nothing"
    /// and cannot see a mailbox. A woken person with an empty one used to be
    /// told to continue work that did not exist, and built a department to
    /// have some. See `chief-cli`'s `spawn_cmd::BootStanding`.
    ///
    /// Given to the builder, never found by it: [`crate::store::mailbox`] is
    /// read by the one caller that holds a company handle, so the documented
    /// pure gate below stays a pure gate.
    pub pending_mail: bool,
    /// Non-secret pane identity environment, in emission order.
    pub env: Vec<EnvAssignment>,
}

/// One company's complete launch catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchCatalog {
    /// [`LAUNCH_CATALOG_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The company slug this catalog was derived for.
    pub company: String,
    /// EVERY person the builder iterated, in the manifest's canonical person
    /// order — not only the launchable ones.
    ///
    /// This is what makes "not in the launch roster (N people iterated)"
    /// sayable, which is a structurally different failure from "looked up and
    /// refused" and needs a structurally different message.
    pub roster: Vec<String>,
    /// Current model facts for every validated roster person.
    pub models: BTreeMap<String, PersonModel>,
    /// Messages in the inbox view for every person in [`Self::roster`].
    ///
    /// This is a top-level roster fact because a person whose launch gate is
    /// refused still has an inbox and must still appear on the department
    /// card. `pending` and fence-archived `delivered` rows are in that view;
    /// the four pane-drain states are not. The route that owns the company
    /// mailbox supplies the exact count; the pure builder initializes every
    /// roster person to zero.
    pub inbox_counts: BTreeMap<String, usize>,
    /// The people the on-disk gate ADMITTED, keyed by person id.
    pub people: BTreeMap<String, LaunchEntry>,
    /// Why each person in `roster` but not in `people` was declined, re-derived
    /// by the daemon that owns the disk (`explain_launch_refusal`).
    ///
    /// The reason is computed here and never by the client, because the client
    /// cannot see the daemon's data root. A person absent from `people` with no
    /// entry here is still a refusal — it simply carries the generic reason.
    pub refusals: BTreeMap<String, String>,
}

impl LaunchCatalog {
    /// An empty catalog for `company`, with nobody iterated.
    ///
    /// Only for a caller building one up field by field. Note what it is NOT: a
    /// fallback. An empty catalog served in place of a real one would refuse
    /// every person by name, which is loud — but it would name the wrong cause,
    /// so nothing constructs one to paper over a failed derivation.
    #[must_use]
    pub fn empty(company: impl Into<String>) -> Self {
        Self {
            schema_version: LAUNCH_CATALOG_SCHEMA_VERSION,
            company: company.into(),
            roster: Vec::new(),
            models: BTreeMap::new(),
            inbox_counts: BTreeMap::new(),
            people: BTreeMap::new(),
            refusals: BTreeMap::new(),
        }
    }

    /// The launch entry for one person, or `None` when the gate declined them.
    #[must_use]
    pub fn entry(&self, person_id: &str) -> Option<&LaunchEntry> {
        self.people.get(person_id)
    }

    /// Why this person cannot launch, in the words an operator needs, or `None`
    /// when they can.
    ///
    /// Total on purpose: every person the caller can ask about gets a sentence,
    /// including one the builder never iterated at all. Returning nothing for
    /// an unknown id would be the silence this type exists to prevent.
    #[must_use]
    pub fn refusal(&self, person_id: &str) -> Option<String> {
        if self.people.contains_key(person_id) {
            return None;
        }
        if !self.roster.iter().any(|id| id == person_id) {
            return Some(format!(
                "person '{person_id}' is not in the launch roster ({} people iterated)",
                self.roster.len()
            ));
        }
        Some(self.refusals.get(person_id).map_or_else(
            || format!("person '{person_id}' refused: materialized state does not validate"),
            |reason| format!("person '{person_id}' refused; re-checked cause: {reason}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> LaunchEntry {
        LaunchEntry {
            pi_binary: "/opt/pi/bin/pi".to_owned(),
            pi_home: "/data/cobalt/.chief/agent/vera".to_owned(),
            workspace: "/data/cobalt/people/vera/workspace".to_owned(),
            display_name: "Cobalt · Quant Head".to_owned(),
            person_name: "Vera".to_owned(),
            accent: Some("#3c7adf".to_owned()),
            tools: vec!["read".to_owned(), "bash".to_owned()],
            extensions: vec![
                "/opt/chief/packages/piing/extensions/organization-intercom.ts".to_owned(),
                "/opt/chief/packages/piing/extensions/team-ui.ts".to_owned(),
                "/opt/chief/packages/piing/extensions/tribes-welcome.ts".to_owned(),
            ],
            session: None,
            // TRUE in the fixed vector on purpose: a `false` here would let the
            // field be dropped from the wire and still round-trip, which is the
            // exact defect the two-sided fixture exists to catch.
            pending_mail: true,
            // No session, so no resume: a first boot has no transcript, and
            // telling that agent it was interrupted would send it hunting for
            // work that never existed.
            env: vec![
                EnvAssignment::new("ORG_LAUNCHER_ORGANIZATION", "cobalt"),
                EnvAssignment::new("ORG_LAUNCHER_PERSON", "vera"),
            ],
        }
    }

    fn catalog() -> LaunchCatalog {
        let mut catalog = LaunchCatalog::empty("cobalt");
        catalog.roster = vec!["vera".to_owned(), "nolan".to_owned()];
        catalog.models.insert(
            "vera".to_owned(),
            PersonModel::selected("openai".to_owned(), "gpt-5.6".to_owned()),
        );
        catalog.models.insert("nolan".to_owned(), PersonModel::unavailable(None, None));
        catalog.inbox_counts.insert("vera".to_owned(), 12);
        catalog.inbox_counts.insert("nolan".to_owned(), 0);
        catalog.people.insert("vera".to_owned(), entry());
        catalog
            .refusals
            .insert("nolan".to_owned(), "required directory 'workspace' is missing".to_owned());
        catalog
    }

    /// The fixed vector `chief-cli`'s second declaration is written against.
    ///
    /// `chief-cli` links none of the backend crates, so this body is the ONLY
    /// thing keeping the two declarations honest. Every field name here is
    /// asserted literally rather than through a round trip, because a round
    /// trip through one declaration proves nothing about the other.
    #[test]
    fn the_wire_body_is_camel_case_with_the_names_the_client_decodes() {
        let body = serde_json::to_value(catalog()).expect("serialize");
        assert_eq!(body["schemaVersion"], 2);
        assert_eq!(body["company"], "cobalt");
        assert_eq!(body["roster"][0], "vera");
        assert_eq!(body["models"]["vera"]["state"], "selected");
        assert_eq!(body["models"]["vera"]["provider"], "openai");
        assert_eq!(body["models"]["vera"]["model"], "gpt-5.6");
        assert_eq!(body["refusals"]["nolan"], "required directory 'workspace' is missing");
        let vera = &body["people"]["vera"];
        for field in [
            "piBinary",
            "piHome",
            "workspace",
            "displayName",
            "personName",
            "accent",
            "tools",
            "extensions",
            "session",
            "pendingMail",
            "env",
        ] {
            assert!(vera.get(field).is_some(), "{field} must be spelled in camelCase");
        }
        // The nullable field is PRESENT and null, never omitted: a client
        // decoding `session` strictly must find the key.
        assert!(vera["session"].is_null());
        assert_eq!(vera["pendingMail"], true, "the client decodes this field strictly");
        assert_eq!(body["inboxCounts"]["vera"], 12);
        assert_eq!(body["inboxCounts"]["nolan"], 0);
        assert_eq!(vera["piHome"], "/data/cobalt/.chief/agent/vera");
        assert_eq!(vera["env"][0]["name"], "ORG_LAUNCHER_ORGANIZATION");
        assert_eq!(vera["env"][0]["value"], "cobalt");
    }

    /// The env is a LIST because a later assignment overrides an earlier one in
    /// `/usr/bin/env`. A map would lose that, silently, and only for the pane
    /// that happened to depend on it.
    #[test]
    fn the_pane_environment_keeps_its_emission_order_across_the_wire() {
        let encoded = serde_json::to_string(&catalog()).expect("serialize");
        let decoded: LaunchCatalog = serde_json::from_str(&encoded).expect("deserialize");
        let env = &decoded.people["vera"].env;
        assert_eq!(env[0].name, "ORG_LAUNCHER_ORGANIZATION");
        assert_eq!(env[1].name, "ORG_LAUNCHER_PERSON");
    }

    #[test]
    fn an_admitted_person_has_no_refusal() {
        assert_eq!(catalog().refusal("vera"), None);
    }

    /// The #52 distinction, on the wire: a person the gate DECLINED gets the
    /// re-derived on-disk cause, and it names the actual missing path.
    #[test]
    fn a_declined_person_is_refused_with_the_daemons_own_re_derived_cause() {
        let refusal = catalog().refusal("nolan").expect("nolan cannot launch");
        assert!(refusal.contains("nolan"), "{refusal}");
        assert!(refusal.contains("re-checked cause"), "{refusal}");
        assert!(refusal.contains("required directory 'workspace' is missing"), "{refusal}");
    }

    /// The structurally different failure: never a candidate for lookup at all.
    /// It must NOT read as "the gate refused them", which sends whoever reads
    /// it hunting inside a function that was never called.
    #[test]
    fn a_person_the_builder_never_iterated_is_refused_as_absent_from_the_roster() {
        let refusal = catalog().refusal("stranger").expect("a stranger cannot launch");
        assert!(refusal.contains("not in the launch roster"), "{refusal}");
        assert!(refusal.contains("2 people iterated"), "{refusal}");
        assert!(!refusal.contains("re-checked cause"), "{refusal}");
    }

    /// Absence is never silence. A person the gate declined with no recorded
    /// reason still gets a named refusal, not `None`.
    #[test]
    fn a_declined_person_with_no_recorded_reason_is_still_a_named_refusal() {
        let mut catalog = catalog();
        catalog.refusals.clear();
        let refusal = catalog.refusal("nolan").expect("nolan still cannot launch");
        assert!(refusal.contains("nolan"), "{refusal}");
        assert!(refusal.contains("does not validate"), "{refusal}");
    }

    #[test]
    fn an_empty_catalog_carries_the_company_and_nobody() {
        let empty = LaunchCatalog::empty("cobalt");
        assert_eq!(empty.company, "cobalt");
        assert_eq!(empty.schema_version, LAUNCH_CATALOG_SCHEMA_VERSION);
        assert!(empty.people.is_empty());
        assert!(empty.roster.is_empty());
    }
}
