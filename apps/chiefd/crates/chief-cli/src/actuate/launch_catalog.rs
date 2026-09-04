//! `POST /v1/org/runtime/launch-catalog`, as this client reads it.
//!
//! # Re-derived from the wire, never imported
//!
//! chiefd serves this body from `chiefd_core::runtime::launch_catalog::
//! LaunchCatalog`. The types here are a SECOND declaration of the same shape,
//! written against the JSON rather than against the Rust, because `chief-cli`
//! links none of `chiefd-core`, `chiefd-host`, `chiefd-api` or `chiefd`
//! (`scripts/test/backend-tmux-boundary.test.mjs` rules 2 and 3, enforced in
//! both directions). That is exactly the decision [`crate::roster`] already
//! made about `DesiredRoster`, for the same reason, and it is kept honest the
//! same way: a fixed JSON vector asserted on both sides.
//!
//! # Why the client does not build this itself
//!
//! It cannot, and it must not. The catalog's core is a fail-closed gate over
//! the DAEMON's data root: the required directory set and the session-resume
//! state. Materialization is the daemon's job. A client that re-read that state would
//! be a second reader of private state and a second answer to "may this person
//! launch", and the second answer is the one nobody updates. `apps/api` already
//! ran that experiment with `desiredActive` — it re-derived the predicate in
//! TypeScript against a field name chiefd never wrote, concluded nobody was
//! ever desired, and launched no agent for weeks while every suite stayed
//! green.
//!
//! # Absence is a NAMED refusal, and that is the whole point
//!
//! [`LaunchCatalog::roster`] is every person chiefd iterated;
//! [`LaunchCatalog::refusals`] is why each declined one was declined, re-derived
//! by the process that can actually see the disk. [`ResolvedCatalog`] hands both
//! straight to [`crate::actuate::interpret::LaunchRosterDiagnostics`], so a
//! start step for a person who is not in the catalog fails with *"person 'vera'
//! refused; re-checked cause: required directory 'workspace' is missing"* —
//! never a silent skip, and never the interchangeable "no launch spec" that
//! once sent an engineer hunting inside a function that was never called (#52).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::actuate::spawn_cmd::LaunchSpec;

/// One `NAME=value` pane environment assignment.
///
/// A LIST, not a map, and the order is load-bearing: these become
/// `/usr/bin/env NAME=value …` argv words where a later assignment overrides an
/// earlier one. [`crate::actuate::spawn_cmd::launch_command`] emits
/// `COLORTERM=truecolor` *before* this list precisely so a catalog-supplied
/// `COLORTERM` still wins — a property that only holds while this is ordered.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvAssignment {
    /// The variable name.
    pub name: String,
    /// Its non-secret value.
    pub value: String,
}

/// Whether chiefd could identify the model Pi will use for one person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersonModelState {
    /// A complete provider/model pair was selected.
    Selected,
    /// Pi has no selected pair and will choose its own default.
    PiDefault,
    /// The backend could not safely resolve the source.
    Unavailable,
}

impl PersonModelState {
    /// Exact wire spelling used by the internal card process boundary.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::PiDefault => "pi-default",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parse the exact internal wire spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "selected" => Some(Self::Selected),
            "pi-default" => Some(Self::PiDefault),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

impl PersonModel {
    /// How this model reads on a surface an operator looks at.
    ///
    /// ONE implementation, because two surfaces draw it: the sleeping-person
    /// card and the department overview. It lived in the sleeping card and the
    /// department card needed the same sentence — and "provider/model" written
    /// twice is two answers to one question the first time either changes.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.state, &self.provider, &self.model) {
            (PersonModelState::Selected, Some(provider), Some(model)) => {
                format!("{provider}/{model}")
            }
            (PersonModelState::PiDefault, _, _) => "Pi default".to_owned(),
            _ => "Unavailable".to_owned(),
        }
    }

    /// A typed unavailable value for a missing backend projection.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self { state: PersonModelState::Unavailable, provider: None, model: None }
    }
}

/// One backend-owned model fact. It contains no filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonModel {
    /// Resolution outcome.
    pub state: PersonModelState,
    /// Provider exactly as recorded, when present.
    pub provider: Option<String>,
    /// Model exactly as recorded, when present.
    pub model: Option<String>,
}

/// Everything needed to launch ONE person, and nothing about where they are
/// displayed.
///
/// Paths arrive as strings. This client turns them straight into `PathBuf` and
/// then straight back into argv words through the same lossy display
/// `spawn_cmd` already uses, so the round trip costs nothing and the wire stays
/// text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchEntry {
    /// The pinned pi binary to exec in the pane.
    pub pi_binary: String,
    /// The non-Chief person's home. It is their CWD and their session store,
    /// and since #1307 it is NOT a Pi agent directory: chief no longer
    /// redirects Pi's config scope, so they inherit the operator's own.
    pub pi_home: String,
    /// The person's workspace — the pane's working directory.
    pub workspace: String,
    /// `<organization name> · <person title>`, passed as `--name`.
    pub display_name: String,
    /// The person's display name, for the fresh-session initial message.
    pub person_name: String,
    /// The person's identity accent, `#rrggbb`, for the rail's role chip.
    /// `None` only when chiefd's palette was exhausted.
    pub accent: Option<String>,
    /// Granted tool names for `--tools`.
    pub tools: Vec<String>,
    /// Exact shipped extension source paths, in Pi argv order.
    pub extensions: Vec<String>,
    /// The transcript to resume (`--session`), or `None` for a fresh session.
    pub session: Option<String>,
    /// Whether a message is waiting unread in this person's mailbox. The whole
    /// of "does this person have assigned work", and the fact
    /// [`crate::actuate::spawn_cmd::BootStanding::from_company`] picks a
    /// fresh-session sentence from.
    pub pending_mail: bool,
    /// Non-secret pane identity environment, in emission order.
    pub env: Vec<EnvAssignment>,
}

impl LaunchEntry {
    /// Map one wire entry into the interpreter's [`LaunchSpec`].
    ///
    /// A total, field-for-field translation with no defaulting and no
    /// invention: every value here came from chiefd, and a field this client
    /// filled in itself would be a launch input chiefd never authorized.
    #[must_use]
    pub fn to_launch_spec(&self) -> LaunchSpec {
        LaunchSpec {
            pi_binary: PathBuf::from(&self.pi_binary),
            pi_home: PathBuf::from(&self.pi_home),
            workspace: PathBuf::from(&self.workspace),
            display_name: self.display_name.clone(),
            person_name: self.person_name.clone(),
            accent: self.accent.clone(),
            tools: self.tools.clone(),
            extensions: self.extensions.iter().map(PathBuf::from).collect(),
            session: self.session.as_ref().map(PathBuf::from),
            pending_mail: self.pending_mail,
            env: self
                .env
                .iter()
                .map(|assignment| (assignment.name.clone(), assignment.value.clone()))
                .collect(),
        }
    }
}

/// One company's complete launch catalog, as served.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchCatalog {
    /// The wire schema version chiefd stamped.
    pub schema_version: u32,
    /// The company slug this catalog was derived for.
    pub company: String,
    /// EVERY person chiefd iterated, not only the launchable ones.
    pub roster: Vec<String>,
    /// Current model facts for every validated roster person.
    pub models: BTreeMap<String, PersonModel>,
    /// Messages in the durable inbox view, for every roster person.
    pub inbox_counts: BTreeMap<String, usize>,
    /// The people the on-disk gate ADMITTED.
    pub people: BTreeMap<String, LaunchEntry>,
    /// Why each person in `roster` but not in `people` was declined.
    pub refusals: BTreeMap<String, String>,
}

impl LaunchCatalog {
    /// Decode a `/v1/org/runtime/launch-catalog` body.
    ///
    /// # Errors
    /// The serde error, verbatim: this is a peer service in the same workspace,
    /// so there is no tolerant second arm for a body it has never sent. An
    /// undecodable catalog is an ERROR and never an empty one — an empty
    /// catalog reads to the interpreter as "refuse everybody", which is loud
    /// but names the wrong cause.
    pub fn from_json(body: &str) -> Result<Self, serde_json::Error> {
        let catalog: Self = serde_json::from_str(body)?;
        if catalog.schema_version != 2 {
            return Err(<serde_json::Error as serde::de::Error>::custom(format!(
                "launch catalog schema {} is not supported; expected 2",
                catalog.schema_version
            )));
        }
        let roster: BTreeSet<&str> = catalog.roster.iter().map(String::as_str).collect();
        if roster.len() != catalog.roster.len() {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "launch catalog roster contains duplicate person ids",
            ));
        }
        let counted: BTreeSet<&str> = catalog.inbox_counts.keys().map(String::as_str).collect();
        if counted != roster {
            return Err(<serde_json::Error as serde::de::Error>::custom(format!(
                "launch catalog inboxCounts must name every roster person exactly; roster={roster:?}, counts={counted:?}"
            )));
        }
        Ok(catalog)
    }

    /// Turn the wire body into the facts the interpreter wants.
    ///
    /// Done ONCE per pass rather than per step: `LaunchSpec` owns its strings,
    /// and rebuilding the whole map for every start in a plan would re-clone
    /// every person's environment.
    ///
    /// # A relative `piBinary` is a REFUSAL, not a launch
    ///
    /// THIS process is the one that hands the pane's argv to tmux, and the tmux
    /// SERVER is what execs word 0. A relative pi binary therefore gets its
    /// final answer from a PATH this client never measured — the fault that
    /// killed every CEO pane at creation and reported itself, once per second
    /// forever, as `unusable window dimensions "\t\n"`. chiefd refuses a
    /// non-absolute `--pi-binary` on its own side; this is the same refusal
    /// made by the only process that can see what is actually being launched,
    /// and it lands in `refusals`, where the interpreter already turns absence
    /// into a named cause instead of a silent skip.
    #[must_use]
    pub fn resolve(&self) -> ResolvedCatalog {
        let mut specs = BTreeMap::new();
        let mut refusals = self.refusals.clone();
        for (person_id, entry) in &self.people {
            let spec = entry.to_launch_spec();
            if spec.pi_binary.is_absolute() {
                specs.insert(person_id.clone(), spec);
            } else {
                refusals.insert(
                    person_id.clone(),
                    format!(
                        "pi binary '{}' is not an absolute path; the tmux server would resolve it \
                         against a PATH this client cannot see",
                        entry.pi_binary
                    ),
                );
            }
        }
        ResolvedCatalog {
            specs,
            roster: self.roster.iter().cloned().collect(),
            models: self.models.clone(),
            inbox_counts: self.inbox_counts.clone(),
            refusals,
        }
    }
}

/// A decoded catalog in the exact shape
/// [`crate::actuate::interpret::apply_plan_with_launch_roster`] takes.
///
/// The three fields travel together because they are unreadable apart: `specs`
/// alone cannot distinguish a person chiefd never iterated from one its gate
/// declined, and that distinction is the difference between a message that
/// sends somebody to the right place and one that sends them hunting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedCatalog {
    /// The per-person launch inputs, for the people who may launch.
    pub specs: BTreeMap<String, LaunchSpec>,
    /// Every person chiefd iterated, launchable or not.
    pub roster: BTreeSet<String>,
    /// Backend-owned current model facts by person id.
    pub models: BTreeMap<String, PersonModel>,
    /// Durable inbox-message counts by person id, including refused people.
    pub inbox_counts: BTreeMap<String, usize>,
    /// chiefd's own re-derived reason for each declined person.
    pub refusals: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retired_launch_catalog_schema_is_refused_without_a_compatibility_arm() {
        let error = LaunchCatalog::from_json(
            r#"{"schemaVersion":1,"company":"acme","roster":[],"people":{},"models":{},"inboxCounts":{},"refusals":{}}"#,
        )
        .expect_err("schema 1 is retired");
        assert!(error.to_string().contains("expected 2"));
    }
    use crate::actuate::plan::SpawnSpec;
    use crate::actuate::spawn_cmd::launch_command;

    /// The FIXED VECTOR, byte-for-byte the body
    /// `chiefd_core::runtime::launch_catalog`'s own
    /// `the_wire_body_is_camel_case_with_the_names_the_client_decodes` pins from
    /// the other side.
    ///
    /// This is the only thing keeping two independent declarations of one shape
    /// honest, because neither crate can name the other. A round trip through
    /// THIS declaration would prove nothing at all about chiefd's.
    // `r##"..."##`: the fixture carries an accent hex, and the `"#` that opens it
    // would close an `r#"` raw string early.
    const BODY: &str = r##"{
      "schemaVersion": 2,
      "company": "cobalt",
      "roster": ["vera", "nolan"],
      "models": {
        "vera": {"state":"selected","provider":"openai","model":"gpt-5.6"},
        "nolan": {"state":"unavailable","provider":null,"model":null}
      },
      "inboxCounts": {"vera":12,"nolan":0},
      "people": {
        "vera": {
          "piBinary": "/opt/pi/bin/pi",
          "piHome": "/data/cobalt/.chief/agent/vera",
          "workspace": "/data/cobalt/people/vera/workspace",
          "displayName": "Cobalt · Quant Head",
          "personName": "Vera",
          "accent": "#3c7adf",
          "tools": ["read", "bash"],
          "extensions": [
            "/opt/chief/packages/piing/extensions/organization-intercom.ts",
            "/opt/chief/packages/piing/extensions/team-ui.ts",
            "/opt/chief/packages/piing/extensions/tribes-welcome.ts"
          ],
          "session": null,
          "pendingMail": true,
          "env": [
            {"name": "ORG_LAUNCHER_ORGANIZATION", "value": "cobalt"},
            {"name": "ORG_LAUNCHER_PERSON", "value": "vera"},
            {"name": "PI_CODING_AGENT_SESSION_DIR", "value": "/data/cobalt/.chief/agent/vera/sessions"}
          ]
        }
      },
      "refusals": {
        "nolan": "required directory 'workspace' is missing"
      }
    }"##;

    fn catalog() -> LaunchCatalog {
        LaunchCatalog::from_json(BODY).expect("chiefd's body must decode")
    }

    #[test]
    fn the_served_body_decodes_into_this_clients_own_declaration() {
        let catalog = catalog();
        assert_eq!(catalog.schema_version, 2);
        assert_eq!(catalog.company, "cobalt");
        assert_eq!(catalog.roster, vec!["vera".to_owned(), "nolan".to_owned()]);
        assert_eq!(catalog.models["vera"].state, PersonModelState::Selected);
        assert_eq!(catalog.models["vera"].provider.as_deref(), Some("openai"));
        assert_eq!(catalog.models["vera"].model.as_deref(), Some("gpt-5.6"));
        assert_eq!(catalog.inbox_counts["vera"], 12);
        assert_eq!(catalog.inbox_counts["nolan"], 0);
        assert_eq!(catalog.people.len(), 1, "only the admitted person carries an entry");
        assert_eq!(catalog.refusals["nolan"], "required directory 'workspace' is missing");
    }

    #[test]
    fn inbox_counts_name_every_roster_person_including_a_refusal() {
        let mut missing: serde_json::Value = serde_json::from_str(BODY).expect("fixture JSON");
        missing["inboxCounts"].as_object_mut().expect("count map").remove("nolan");
        let error = LaunchCatalog::from_json(&serde_json::to_string(&missing).expect("JSON"))
            .expect_err("a refused person still needs an inbox count");
        assert!(error.to_string().contains("inboxCounts must name every roster person exactly"));

        let mut unknown: serde_json::Value = serde_json::from_str(BODY).expect("fixture JSON");
        unknown["inboxCounts"]["stranger"] = serde_json::json!(1);
        let error = LaunchCatalog::from_json(&serde_json::to_string(&unknown).expect("JSON"))
            .expect_err("an unknown person cannot enter the card through the count map");
        assert!(error.to_string().contains("inboxCounts must name every roster person exactly"));
    }

    #[test]
    fn duplicate_roster_ids_are_refused_before_they_collapse_into_a_set() {
        let mut duplicate: serde_json::Value = serde_json::from_str(BODY).expect("fixture JSON");
        duplicate["roster"] = serde_json::json!(["vera", "nolan", "vera"]);

        let error = LaunchCatalog::from_json(&serde_json::to_string(&duplicate).expect("JSON"))
            .expect_err("one person cannot occupy two roster positions");

        assert!(error.to_string().contains("roster contains duplicate person ids"));
    }

    /// Every field, because a field this client silently dropped would be a
    /// launch input chiefd authorized and the pane never received — and the
    /// symptom would be a Pi that starts with the wrong tools, or a rail whose
    /// chips are all grey, not a failure.
    #[test]
    fn every_wire_field_reaches_the_launch_spec() {
        let spec = catalog().people["vera"].to_launch_spec();
        assert_eq!(spec.pi_binary, PathBuf::from("/opt/pi/bin/pi"));
        assert_eq!(spec.pi_home, PathBuf::from("/data/cobalt/.chief/agent/vera"));
        assert_eq!(spec.workspace, PathBuf::from("/data/cobalt/people/vera/workspace"));
        assert_eq!(spec.display_name, "Cobalt · Quant Head");
        assert_eq!(spec.person_name, "Vera");
        assert_eq!(spec.accent.as_deref(), Some("#3c7adf"), "the COLOUR, not a path to it");
        assert_eq!(spec.tools, vec!["read".to_owned(), "bash".to_owned()]);
        assert_eq!(
            spec.extensions,
            [
                "/opt/chief/packages/piing/extensions/organization-intercom.ts",
                "/opt/chief/packages/piing/extensions/team-ui.ts",
                "/opt/chief/packages/piing/extensions/tribes-welcome.ts",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(spec.session, None);
        assert_eq!(
            spec.env,
            vec![
                ("ORG_LAUNCHER_ORGANIZATION".to_owned(), "cobalt".to_owned()),
                ("ORG_LAUNCHER_PERSON".to_owned(), "vera".to_owned()),
                (
                    "PI_CODING_AGENT_SESSION_DIR".to_owned(),
                    "/data/cobalt/.chief/agent/vera/sessions".to_owned()
                ),
            ],
            "the pane environment keeps chiefd's emission order"
        );
    }

    /// The end-to-end proof that the wire is enough: a body chiefd served
    /// becomes a real pi invocation, with no local input but the spawn spec.
    #[test]
    fn a_served_catalog_entry_is_enough_to_build_the_real_pane_command() {
        let spec = catalog().people["vera"].to_launch_spec();
        let command = launch_command(
            &SpawnSpec { person_id: "vera".to_owned(), launch_hash: "hash-3".to_owned() },
            &spec,
            &crate::actuate::spawn_cmd::PanePlacement {
                socket: "cobalt-sock",
                session: "org-cobalt_",
            },
            crate::actuate::spawn_cmd::BootStanding::Established,
            Some(crate::appearance::Appearance::Light),
        );
        let joined = command.argv.join("\u{0}");
        assert_eq!(command.argv[0], "/usr/bin/env");
        assert_eq!(command.cwd, PathBuf::from("/data/cobalt/people/vera/workspace"));
        assert!(joined.contains("/opt/pi/bin/pi"), "{joined}");
        assert!(joined.contains("--tools\u{0}read,bash"), "{joined}");
        for extension in ["organization-intercom.ts", "team-ui.ts", "tribes-welcome.ts"] {
            assert!(
                joined.contains(&format!(
                    "--extension\u{0}/opt/chief/packages/piing/extensions/{extension}"
                )),
                "{joined}"
            );
        }
        assert!(
            joined.contains("PI_CODING_AGENT_SESSION_DIR=/data/cobalt/.chief/agent/vera/sessions"),
            "{joined}"
        );
    }

    /// THE RULE the pane's ACCENT obeys: it is the colour the CATALOG carried,
    /// and this client neither allocates one nor holds a default.
    ///
    /// It replaces `the_pane_runs_the_provider_the_catalog_carried_and_never_a_
    /// default`, whose subject went with provider management, and it is written
    /// the same way and for the same reason: the expectation is DERIVED from the
    /// entry rather than pinned to the fixture's value, because a client that
    /// ignored the entry and substituted its own answer would pass a
    /// fixture-valued assertion unchanged. The accent used to reach this client
    /// as three theme-file paths it opened and parsed; it arrives as the hex
    /// now, so the rail and the browser cannot disagree about a person's colour.
    #[test]
    fn the_rail_paints_the_accent_the_catalog_carried_and_never_one_of_its_own() {
        for accent in ["#e24033", "#3c7adf", "#a74ef5"] {
            let mut catalog = catalog();
            catalog.people.get_mut("vera").expect("the fixture has vera").accent =
                Some(accent.to_owned());
            assert_eq!(catalog.people["vera"].to_launch_spec().accent.as_deref(), Some(accent));
        }
        // An exhausted palette is an absent colour, never an invented one.
        let mut catalog = catalog();
        catalog.people.get_mut("vera").expect("the fixture has vera").accent = None;
        assert_eq!(catalog.people["vera"].to_launch_spec().accent, None);
    }

    /// THE WIRE KEY, on this side. chiefd serves the causes in kebab-case and
    /// this client looks them up by the enum it re-declared; a rename on either
    /// side turns every resume into silence, which is a missing sentence rather
    /// than a failure and would never be noticed.
    ///
    /// A resumed person carries `--session` instead of the fresh-session
    /// message, and the transcript path is chiefd's — this client never scans a
    /// sessions directory, because it cannot see one.
    #[test]
    fn a_session_to_resume_survives_the_wire_as_a_path() {
        let mut catalog = catalog();
        catalog.people.get_mut("vera").expect("vera").session =
            Some("/data/cobalt/.chief/agent/vera/sessions/abc.jsonl".to_owned());
        let spec = catalog.people["vera"].to_launch_spec();
        assert_eq!(
            spec.session,
            Some(PathBuf::from("/data/cobalt/.chief/agent/vera/sessions/abc.jsonl"))
        );
    }

    /// The diagnostics the interpreter needs, in the shape it needs them.
    #[test]
    fn resolving_yields_the_specs_the_roster_and_the_refusals() {
        let resolved = catalog().resolve();
        assert_eq!(resolved.specs.keys().collect::<Vec<_>>(), vec!["vera"]);
        assert!(resolved.roster.contains("vera"));
        assert!(
            resolved.roster.contains("nolan"),
            "a person the gate declined is still ITERATED; that is what makes 'refused' \
             distinguishable from 'never a candidate'"
        );
        assert_eq!(resolved.refusals["nolan"], "required directory 'workspace' is missing");
        assert_eq!(resolved.models["nolan"].state, PersonModelState::Unavailable);
    }

    /// THE property this whole packet is about. `nolan` is not launchable, and
    /// what the client has must be enough to say so BY NAME with chiefd's own
    /// re-derived cause — not an empty entry, not a skip.
    #[test]
    fn a_declined_person_carries_everything_needed_for_a_loud_named_refusal() {
        let resolved = catalog().resolve();
        assert!(!resolved.specs.contains_key("nolan"), "nolan must not be launchable");
        assert!(
            resolved.roster.contains("nolan"),
            "the roster is what makes the interpreter say 'refused' rather than 'not in the \
             launch roster'"
        );
        let reason = resolved.refusals.get("nolan").expect("chiefd named the cause");
        assert!(reason.contains("workspace"), "{reason}");
    }

    /// A person chiefd never iterated at all — a stale plan naming somebody who
    /// has left. Structurally different from a refusal, and the interpreter must
    /// be able to tell: the roster does not contain them.
    #[test]
    fn a_person_the_catalog_never_iterated_is_absent_from_the_roster_too() {
        let resolved = catalog().resolve();
        assert!(!resolved.roster.contains("stranger"));
        assert!(!resolved.refusals.contains_key("stranger"));
    }

    /// THE GENERALISED FORM OF THE COLD-ATTACH DEFECT, from the only side that
    /// can see it. chiefd refuses a relative `--pi-binary`, but this client is
    /// the process that hands word 0 of the pane argv to a tmux SERVER, and that
    /// server resolves a bare name against a PATH nobody here measured. A person
    /// carrying one is refused BY NAME instead of minted into a pane that dies
    /// on its first millisecond.
    #[test]
    fn a_relative_pi_binary_is_refused_by_name_and_never_handed_to_tmux() {
        for relative in ["pi", "bin/pi", "./node_modules/.bin/pi"] {
            let mut catalog = catalog();
            catalog
                .people
                .get_mut("vera")
                .expect("the fixture has vera")
                .pi_binary
                .clone_from(&relative.to_string());

            let resolved = catalog.resolve();

            assert!(
                !resolved.specs.contains_key("vera"),
                "'{relative}' must never reach a pane argv"
            );
            assert!(
                resolved.roster.contains("vera"),
                "a refused person stays on the roster so the interpreter says 'refused'"
            );
            let reason = resolved.refusals.get("vera").expect("the refusal is named");
            assert!(reason.contains(relative), "the refusal must quote the value: {reason}");
            assert!(reason.contains("absolute"), "{reason}");
        }
    }

    /// The control: the absolute binary the daemon actually serves still
    /// launches, so the check above is not simply refusing everybody.
    #[test]
    fn an_absolute_pi_binary_still_launches() {
        let resolved = catalog().resolve();
        let spec = resolved.specs.get("vera").expect("vera is launchable");
        assert_eq!(spec.pi_binary, PathBuf::from("/opt/pi/bin/pi"));
        assert!(!resolved.refusals.contains_key("vera"));
    }

    /// A body this client cannot read is an ERROR, never an empty catalog. An
    /// empty catalog is a *successful* answer meaning "nobody may launch", and
    /// a decoder that produced one from a truncated response would refuse a
    /// whole company for a reason that is not true.
    #[test]
    fn an_undecodable_body_is_an_error_and_never_an_empty_catalog() {
        assert!(LaunchCatalog::from_json("{\"company\":\"cobalt\"}").is_err());
        assert!(LaunchCatalog::from_json("").is_err());
        assert!(LaunchCatalog::from_json("not json at all").is_err());
    }
}
