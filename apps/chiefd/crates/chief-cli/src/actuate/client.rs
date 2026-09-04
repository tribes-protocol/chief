//! The actuation wire: the reads the resident actuator and the operator's
//! rail make, and the changefeed connection they wait on instead of polling.
//!
//! # EVERY OBSERVATION HERE IS A READ, AND EXACTLY ONE CALL IS NOT
//!
//! The architecture in one sentence, unchanged: **chiefd holds the desired
//! state, and host facts never travel up.** No verb on this client reports
//! what it saw, and none commits a runtime row.
//!
//! | call | route | what it is |
//! |---|---|---|
//! | [`ActuationClient::desired`] | `POST /v1/org/runtime/desired` | WHO should be running, and the hash of what each should be running |
//! | [`ActuationClient::roster`] | `POST /v1/org/roster/desired` | the structure — departments, order — the placement input |
//! | [`ActuationClient::launch_catalog`] | `POST /v1/org/runtime/launch-catalog` | WITH WHAT each person launches — the spawn input |
//! | [`ActuationClient::lifecycle_status`] | `POST /v1/org/lifecycle-status/read` | whose settle clock is RUNNING — the rail's IDLE/WORKING split |
//! | [`ActuationClient::wake_person`] | `POST /v1/org/person/wake` | **the one write** — the operator clicked a sleeping person |
//! | [`ActuationClient::wait`] | `GET /v1/docs/watch` | the SSE changefeed the loop parks on |
//!
//! All of them are `POST`; all but the wake are reads whose body carries the
//! composite document key, which is a parameter and not a submission. The
//! method is the shape every `/v1/org/*` route in this daemon takes, and
//! matching it is worth more than a purity argument about verbs.
//!
//! The wake is a WRITE and is named as one, because the rail is a control
//! surface as well as a display: a click on a parked person is a durable
//! decision about who runs, and no amount of tmux can make it. It stays honest
//! about the old rule by being the narrowest write available — one named
//! person, no runtime row, no fence opened for anybody else.
//!
//! # TOMBSTONE: `POST /v1/org/runtime/observed` and `POST /v1/org/runtime/actions`
//!
//! `observed` committed this actuator's report of what it saw in tmux and
//! answered with an action plan computed against it. `actions` was the
//! read-only re-read of that same report. Both are deleted.
//!
//! The single round trip they formed was defended for a real property — the
//! plan came back from the same call that committed the observation, so the
//! client never applied a plan derived from a state it had already changed.
//! That property is not lost; there is no longer any gap for it to close. What
//! chiefd publishes does not depend on what was observed, so this client reads
//! the desired set, reads tmux, and holds both at the instant it acts on them,
//! which is the only place those two facts can honestly be held together.
//!
//! # Every call here carries a credential, and it is the ACTUATOR'S OWN
//!
//! All four present `Authorization: Bearer <jwt>` for the `service` identity —
//! `<data-root>/keys/service.key`, minted at boot beside the operator's key and
//! enrolled into every company on that data root. The actuator is NOT the
//! operator, and the deciding reason is the audit trail rather than least
//! privilege: the staffing routes are losing `String::new()` as their actor,
//! and a record that could not tell an automatic actuation from a deliberate
//! operator action would waste that fix.
//!
//! What the identity is FOR is narrower than it looks. The reads follow one
//! rule — a valid credential is present, and specifically NOT to resolve a
//! person from it — because a person-deriving helper answers `None` for a
//! service, which a handler could mistake for "unauthenticated" and refuse,
//! authenticating this actuator perfectly and then turning it away.
//!
//! The wake is the exception that proves it: it is a write, so its route
//! DERIVES a fence from the caller. That fence is the subtree question every
//! verb in this daemon asks, and it is not a role gate. The rail presents the
//! OPERATOR bearer rather than this one, and the
//! operator names no person row, so `control_authority` gives it unconditional
//! scope — the same reason the rail's roster and lifecycle reads are unfenced.
//!
//! # Why hyper directly, and not [`crate::actuate`]'s sibling transport
//!
//! `src/http.rs` is the binary crate's bounded JSON transport and this module
//! is in the **library** half — `main.rs` declares `mod http;`, so nothing under
//! `src/actuate/` can name it. That is not the only reason, and not the
//! important one: `http::Answer` collects a *complete* body before it returns,
//! and the changefeed's body never completes. A transport whose only shape is
//! "one request, one whole answer" cannot express the one connection this loop
//! spends nearly all of its time inside.
//!
//! So this is the same decision `src/http.rs` records, applied again: hyper,
//! which is already in this crate's graph — never `reqwest`, and never a
//! shelled-out `curl`, because a `curl` subprocess is a thread that cannot be
//! cancelled. No dependency is added for it.
//!
//! # The self-wake that is now impossible rather than avoided
//!
//! [`WAKE_STORES`] used to omit `runtime-actuation` on purpose: every report
//! committed that store, which published a change on the feed the loop was
//! parked on, so subscribing would have made each round wake the next one
//! forever at whatever rate tmux could be driven. The store is gone.
//!
//! [`ActuationClient::wake_person`] does commit `activity`, which IS subscribed
//! — and that is not the same shape. A self-wake needs a write on the LOOP's
//! own path; the wake is on the CLICK's, which no changefeed event can reach.
//! One click produces one write, which produces one refresh, which produces no
//! write. There is no cycle to bound.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt as _;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;

use crate::actuate::desired::DesiredRuntime;
use crate::actuate::launch_catalog::LaunchCatalog;
use crate::bearer::{Bearer, JsonPost};
use crate::lifecycle::LifecycleStatus;
use crate::roster::Roster;

/// The shared hyper client type: HTTP/1.1 over TCP with a full request body.
type Inner = HyperClient<HttpConnector, http_body_util::Full<hyper::body::Bytes>>;

/// How long the desired-set read may take to answer.
///
/// Generous, because the route does real work: it projects the manifest, the
/// activity ledger, the supervision ledger and the safety scaffold, and derives
/// a launch hash per person. A budget tight enough to clip that would turn a
/// slow company into an un-actuated one.
pub const DESIRED_BUDGET: Duration = Duration::from_secs(15);

/// How long a roster read may take to answer.
pub const ROSTER_BUDGET: Duration = Duration::from_secs(10);

/// How long a launch-catalog read may take to answer.
///
/// Wider than the roster's, because this route does real filesystem work: it
/// walks every person's materialized home, reads the root provider and auth
/// registries, and stages each selected provider credential. A budget tight
/// enough to clip that on a cold page cache would leave a fully materialized
/// company un-launchable for no reason.
pub const LAUNCH_CATALOG_BUDGET: Duration = Duration::from_secs(20);

/// The document stores whose changes are work for this actuator.
///
/// Nothing is deliberately missing from this list any more, and that is worth
/// stating. It used to omit `runtime-actuation` because every report this loop
/// made committed that store, which published a change on the same feed the
/// loop was parked on — subscribing would have meant every round woke the round
/// after it, forever, at whatever rate tmux could be driven. This client writes
/// nothing now, so no self-wake is possible and no omission defends against
/// one. The stores here are simply the complete set of writes that can change
/// what chiefd wants running:
///
/// * `activity` — the reconcile ledger; who is desired-active.
/// * `supervision` — the roster/lifecycle authority, including the per-person
///   runtime identity a restart is fenced on.
/// * `converge-safety` — shadow/apply, the breaker, the budgets.
/// * `org-manifest` — the structural authority: people, departments, the tree.
/// * `mailbox/` — every per-person inbox; a drain changes the card count even
///   when no runtime authority changes with it.
pub const WAKE_STORES: [&str; 5] =
    ["activity", "supervision", "converge-safety", "org-manifest", "mailbox/"];

/// How long a wake waits for the rest of its burst before it is answered.
///
/// Not a poll interval and not a debounce on the WORK — the caller's own floor
/// (`resident::Schedule::min_round_interval`) already decides how often it may
/// act. This is narrower: it is how long
/// [`ActuationClient::open_stream`] keeps reading a socket that has already
/// given it one event, so that a burst of writes becomes one wake instead of
/// one wake each.
///
/// Twenty-five milliseconds, chosen against what it is coalescing rather than
/// picked round. A burst here is one logical mutation publishing to several
/// stores in one transaction; those events are written together and arrive
/// within a couple of milliseconds of each other, so this is an order of
/// magnitude of headroom. It is also an order of magnitude BELOW the one-second
/// floors above, so a wake can never be delayed enough to matter to either
/// caller — the worst case is that a click's wake is answered 25ms later, which
/// is invisible beside the daemon round trips it precedes.
const WAKE_COALESCE: Duration = Duration::from_millis(25);

/// Fold a newly parsed wake into the one already in hand.
///
/// Two rules, and both are about not losing information:
///
/// * **A reorg outranks everything.** It says the resume point is unusable, so
///   the caller must drop it and resync. A reorg swallowed by a later
///   `Change` would leave the caller resuming from a sequence the feed has
///   already told it is meaningless.
/// * **Otherwise the HIGHEST sequence wins**, because the caller stores it as
///   its resume point. Taking a lower one would replay events it has already
///   been told about and wake it again for them — the exact loop
///   [`WAKE_STORES`] documents the feed being careful to avoid.
fn coalesce(held: Option<Wake>, next: Wake) -> Wake {
    match (held, next) {
        (Some(Wake::Reorg), _) | (_, Wake::Reorg) => Wake::Reorg,
        (Some(Wake::Change { seq: held }), Wake::Change { seq: next }) => {
            Wake::Change { seq: held.max(next) }
        }
        (None | Some(Wake::Quiet | Wake::Closed), next) => next,
        (Some(held), Wake::Quiet | Wake::Closed) => held,
    }
}

/// What ended a wait on the changefeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    /// A document this actuator subscribes to changed. Carries the feed's own
    /// sequence number so the next connection resumes after it rather than
    /// replaying the ring and waking itself immediately.
    Change {
        /// The change-feed sequence of the event that woke the loop.
        seq: u64,
    },
    /// The feed signalled `reorg`: the resume point is from a previous chiefd
    /// process epoch, or its successor was evicted from the ring. Everything
    /// this client believes may be stale, so the next round re-reads from
    /// scratch — which is exactly what the ordinary round already does.
    Reorg,
    /// The budget elapsed with nothing happening. The loop re-reads anyway.
    Quiet,
    /// chiefd closed the stream. Not an error and not a change: a daemon that
    /// restarted is the ordinary cause, and the next round's read is what
    /// discovers whether it came back.
    Closed,
}

/// Why an actuation call did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActuationError {
    /// Nothing answered: connection refused, aborted, or the budget elapsed.
    /// Transient by nature — the daemon may be restarting.
    #[error("could not reach {url}: {reason}")]
    Transport {
        /// The URL that did not answer.
        url: String,
        /// What the transport reported.
        reason: String,
    },
    /// chiefd answered, and a product rule declined. The caller must act on
    /// `code` and must never retry the same body — `chiefd-api`'s refusal
    /// taxonomy states that as the contract for every 4xx it carries.
    #[error("chiefd refused {path} with {status} {code}: {detail}")]
    Refused {
        /// The route that refused.
        path: String,
        /// The HTTP status.
        status: u16,
        /// The stable machine code.
        code: String,
        /// The human half.
        detail: String,
    },
    /// This actuator cannot prove who it is for a LOCAL reason: its identity
    /// key is missing, unreadable, not a P-256 PKCS#8 PEM, or readable by
    /// somebody other than its owner.
    ///
    /// A HARD REFUSAL, never a degrade to an anonymous call. A key anyone can
    /// read is a key to assume is copied, and quietly continuing without it
    /// would turn a credential-hygiene failure into an unauthenticated request
    /// that nothing reports. It is also never retried: no amount of asking
    /// again creates a key or narrows its mode.
    #[error("this actuator cannot prove who it is: {detail}")]
    Credential {
        /// The key failure's own words, naming the path and the `chmod` that
        /// fixes it.
        detail: String,
    },
    /// chiefd answered with a body this client cannot read. Never treated as
    /// an empty answer: an unreadable plan is not "no actions", and an
    /// unreadable roster is not "nobody works here".
    #[error("chiefd answered {path} with a body this client cannot read: {detail}")]
    Undecodable {
        /// The route.
        path: String,
        /// The decoder's own words.
        detail: String,
    },
}

impl ActuationError {
    /// Whether retrying this exact call could ever produce a different answer.
    ///
    /// The split the loop lives on. A transport failure is the daemon being
    /// restarted or the box being busy, and the actuator's job is to still be
    /// there afterwards. A 4xx is a product rule, and a client that retried one
    /// would ask the identical question forever — `route_error.rs` says so in
    /// as many words: *act on the code, NEVER retry*.
    ///
    /// A 5xx IS TRANSIENT, and getting this wrong was a real bug. Every
    /// non-200 used to be `Refused` and only `Transport` was retried, so the
    /// resident loop RETURNED on the first 500 — one SQLite hiccup, or a 503
    /// while `chiefd run` was still wiring its routes, and that company had no
    /// actuator again until somebody noticed. It is also newly SILENT: chiefd
    /// derived actuator presence from a lease the actuator renewed by
    /// reporting, and that warning is a named accepted loss of this change, so
    /// nothing is left to say "nobody is converging this company".
    ///
    /// "Never retry a 4xx" is right and is unchanged; extending it to 5xx was
    /// the error. A 5xx says the server failed, not that the request was
    /// wrong — including this crate's own `503 launch-catalog-unavailable` and
    /// `503 extension-source-unreadable`, both of which are explicitly states
    /// that resolve on their own.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            // 429 is the ONE 4xx that is a statement about the daemon's
            // current state rather than about the caller. The taxonomy this
            // client decodes says so in its own words — `route_error.rs`'s
            // `busy` row reads "back off and retry" — and classifying it
            // terminal made this loop exit on a sentence that asked it to
            // wait. Not a widening of the 403 rule (#1204): a verdict stays a
            // verdict, and this was never one.
            Self::Refused { status, .. } => *status >= 500 || *status == 429,
            Self::Credential { .. } | Self::Undecodable { .. } => false,
        }
    }
}

/// How long an auth acquisition round trip may take. Tight on purpose: both
/// auth routes do one indexed identity read and one signature check, so a slow
/// answer is a sick daemon rather than a busy one.
const AUTH_BUDGET: Duration = Duration::from_secs(10);

/// One company's actuation endpoint.
#[derive(Clone)]
pub struct ActuationClient {
    base: String,
    document_key: String,
    inner: Inner,
    bearer: Arc<Bearer>,
}

impl ActuationClient {
    /// Bind to a company daemon's proven URL, its composite document key, and
    /// the identity this actuator authenticates as.
    ///
    /// `document_key` is the `slug@digest(dataRoot)` every `/v1/org/*` route
    /// matches its live company against — one slug under two data roots is two
    /// companies, so the plain slug is not an identity and every route answers
    /// `404 unknown-company` to one.
    ///
    /// The bearer is NOT optional, and that is the packet's whole point: the
    /// resident actuator is its OWN principal (`service`), never the operator's,
    /// so a record of an automatic action can be told apart from a record of a
    /// deliberate one. Cloning this client shares the token cache, which is what
    /// keeps one actuator from minting a token per copy of itself.
    #[must_use]
    pub fn new(url: &str, document_key: &str, bearer: Arc<Bearer>) -> Self {
        Self {
            base: url.trim_end_matches('/').to_owned(),
            document_key: document_key.to_owned(),
            inner: HyperClient::builder(TokioExecutor::new()).build_http(),
            bearer,
        }
    }

    /// The `Authorization` header for the next call. **There is no answer
    /// that means "go without one."**
    ///
    /// The failure kinds are deliberately NOT treated alike, and the split is
    /// where this client's own fault ends and the daemon's begins.
    ///
    /// * **A LOCAL key failure REFUSES**, both halves of it. A widened mode is
    ///   the hard refusal `BearerError::is_key_hygiene_refusal` names for every
    ///   caller. **Absence is fatal for THIS caller specifically**, and that is
    ///   the one place the actuator's answer differs from the operator's: an
    ///   absent `operator.key` is a state the product passes through, because
    ///   `chief` legitimately runs before any daemon exists to mint one —
    ///   but `service.key` is minted at boot by the very daemon this loop is
    ///   converging, so its absence means the thing being actuated is not in
    ///   the state it claims. Converging blind is the worse failure. The shared
    ///   type reports the fact; each caller decides, which is why the split
    ///   exists.
    /// * **A failure to MINT is classified, by the same transient/terminal
    ///   rule every other refusal on this client obeys.** A transport failure
    ///   reaching an auth route is [`ActuationError::Transport`]; a status from
    ///   one is a [`ActuationError::Refused`] AGAINST THAT ROUTE, so a 5xx is
    ///   retried and a 401 is terminal, decided by `is_transient` and nothing
    ///   else.
    ///
    /// # The arm that is gone, and why it had to go
    ///
    /// This used to `warn!` and answer `Ok(None)`, letting the call go out bare
    /// "and the daemon decides". That was written when the universal gate could
    /// be off. Since A6 it cannot: a bare call gets `401 missing bearer token`,
    /// the caller invalidates and re-mints (which fails identically), calls bare
    /// again, gets `401` again, and returns a TERMINAL refusal — so the arm that
    /// existed to avoid taking a company out of actuation was guaranteed to do
    /// exactly that, and to blame the read instead of the mint while doing it.
    /// Its cost is paid at the worst moment: a chiefd restart rotates the
    /// ephemeral secret and invalidates every cached token, which is precisely
    /// when the identity store is most likely to be slow.
    ///
    /// A settled mint refusal still exits, and now says
    /// `chiefd refused /v1/auth/challenge with 401 ...` rather than naming
    /// whichever read happened to need the token. Same outcome, true sentence.
    async fn authorization(&self) -> Result<String, ActuationError> {
        use crate::bearer::{BearerError, CHALLENGE_PATH, TOKEN_PATH};

        // EXHAUSTIVE, with no catch-all: a new `BearerError` variant must stop
        // this crate building rather than fall into whichever arm happens to
        // be last. That is the property the deleted `Ok(None)` arm destroyed.
        match self.bearer.authorization(self, &self.base).await {
            Ok(header) => Ok(header),
            // The two LOCAL key failures. `KeyTooPermissive` is exactly what
            // `BearerError::is_key_hygiene_refusal` names for every caller;
            // `KeyAbsent` is this caller's own addition, because `service.key`
            // is minted at boot by the daemon being converged.
            Err(error @ (BearerError::KeyTooPermissive { .. } | BearerError::KeyAbsent { .. })) => {
                Err(ActuationError::Credential { detail: error.to_string() })
            }
            // Nothing answered the auth route. Transient by the same rule that
            // makes an unreachable read transient — the daemon is restarting.
            Err(BearerError::Transport { route, base_url, reason }) => {
                Err(ActuationError::Transport { url: format!("{base_url}{route}"), reason })
            }
            // The auth route ANSWERED, and its status is the whole decision.
            // A 503 `identity-store-unavailable` is retried; a 401 is not.
            Err(BearerError::Challenge { status, body, .. }) => {
                Err(refusal(CHALLENGE_PATH, status, &body))
            }
            Err(BearerError::Token { status, body, .. }) => Err(refusal(TOKEN_PATH, status, &body)),
            // A 200 this build cannot read, or a key that will not sign. Neither
            // becomes true by asking again, and neither is a refusal by chiefd.
            Err(error @ (BearerError::Malformed { .. } | BearerError::Sign { .. })) => {
                Err(ActuationError::Undecodable {
                    path: CHALLENGE_PATH.to_owned(),
                    detail: error.to_string(),
                })
            }
        }
    }

    /// The document key this client addresses, which is also the `slug` field
    /// every request body carries.
    #[must_use]
    pub fn document_key(&self) -> &str {
        &self.document_key
    }

    /// `POST /v1/org/runtime/desired` — what chiefd wants running.
    ///
    /// A pure read. It commits nothing and renews nothing, because there is no
    /// lease left for a read to renew.
    ///
    /// # Errors
    /// [`ActuationError`] when chiefd cannot be reached, refuses, or answers
    /// with a set this client cannot decode. An undecodable answer is an error
    /// and NEVER an empty set: an empty desired set is a legitimate, actionable
    /// answer meaning *stop everybody*, so producing one from a truncated
    /// response would tear down a whole running company on a slow socket.
    pub async fn desired(&self) -> Result<DesiredRuntime, ActuationError> {
        const PATH: &str = "/v1/org/runtime/desired";
        let body = serde_json::json!({ "slug": self.document_key });
        let answer = self.post(PATH, &body, DESIRED_BUDGET).await?;
        serde_json::from_str(&answer).map_err(|error| ActuationError::Undecodable {
            path: PATH.to_owned(),
            detail: error.to_string(),
        })
    }

    /// `POST /v1/org/roster/desired` — who exists, and who chiefd wants running.
    ///
    /// The placement input, re-read every round rather than cached: a person
    /// hired, transferred or parked changes where everybody is displayed, and a
    /// roster read once at start-up would place them against a company that no
    /// longer exists.
    ///
    /// # Errors
    /// [`ActuationError`] when chiefd cannot be reached, refuses, or answers
    /// with a roster this client cannot decode. A malformed roster is an error
    /// and never an empty one — an empty roster reads to an actuator as *stop
    /// everybody*.
    pub async fn roster(&self) -> Result<Roster, ActuationError> {
        const PATH: &str = "/v1/org/roster/desired";
        let body = serde_json::json!({ "slug": self.document_key });
        let answer = self.post(PATH, &body, ROSTER_BUDGET).await?;
        Roster::from_json(&answer).map_err(|error| ActuationError::Undecodable {
            path: PATH.to_owned(),
            detail: error.to_string(),
        })
    }

    /// `POST /v1/org/lifecycle-status/read` — who is IDLE, and who is WORKING.
    ///
    /// The one published place carrying `idleSince`, which is the running
    /// settle clock and therefore the only positive evidence of quiet this
    /// client can get. Everything else the board answers is read from cheaper
    /// sources and is deliberately ignored here.
    ///
    /// The route derives its disclosure fence from the CALLER. The rail
    /// presents the operator bearer — a non-person principal whose scope is
    /// unconditional — so it is unfenced, exactly as it is on the roster read
    /// and for the same reason `sidebar::for_session` exists.
    ///
    /// # Errors
    /// [`ActuationError`] when chiefd cannot be reached, refuses, or answers
    /// with a board this client cannot decode.
    pub async fn lifecycle_status(&self) -> Result<LifecycleStatus, ActuationError> {
        const PATH: &str = "/v1/org/lifecycle-status/read";
        let body = serde_json::json!({ "slug": self.document_key });
        let answer = self.post(PATH, &body, ROSTER_BUDGET).await?;
        LifecycleStatus::from_json(&answer).map_err(|error| ActuationError::Undecodable {
            path: PATH.to_owned(),
            detail: error.to_string(),
        })
    }

    /// `POST /v1/org/runtime/launch-catalog` — WITH WHAT each person launches.
    ///
    /// The spawn input, re-read every round rather than cached, for a reason
    /// the roster's doc does not cover: a person materialized while this loop
    /// is running must become launchable WITHOUT restarting it. Hiring writes
    /// SQL and materializes a home; a catalog read once at start-up would
    /// refuse that person by name forever, correctly and uselessly, until an
    /// operator noticed and restarted the actuator.
    ///
    /// # Errors
    /// [`ActuationError`] when chiefd cannot be reached, refuses, or answers
    /// with a catalog this client cannot decode. An undecodable catalog is an
    /// error and never an empty one — an empty catalog is a *successful*
    /// answer meaning "nobody in this company may launch", so producing one
    /// from a truncated response would refuse a whole company for a reason
    /// that is not true.
    pub async fn launch_catalog(&self) -> Result<LaunchCatalog, ActuationError> {
        const PATH: &str = "/v1/org/runtime/launch-catalog";
        let body = serde_json::json!({ "slug": self.document_key });
        let answer = self.post(PATH, &body, LAUNCH_CATALOG_BUDGET).await?;
        LaunchCatalog::from_json(&answer).map_err(|error| ActuationError::Undecodable {
            path: PATH.to_owned(),
            detail: error.to_string(),
        })
    }

    /// `POST /v1/org/person/wake` — bring one parked person back up.
    ///
    /// **THE ONE WRITE ON THIS CLIENT.** Everything above is a read, and the
    /// module doc's "every call here is a read" no longer holds because the
    /// rail is a control surface as well as a display: the operator clicks a
    /// sleeping person and expects them woken, which is a durable decision and
    /// cannot be made in tmux.
    ///
    /// It is deliberately the NARROWEST write that does the job. It names one
    /// person, it commits no runtime row, and it opens no fence for anybody
    /// else — the daemon grants exactly that person's launch intent and lets
    /// its own converge pass do the rest. The company-wide
    /// `/v1/org/runtime/launch` would have been the lazy alternative and is
    /// wrong twice over: only the head of the root may post it, and it is a
    /// fleet decision made on behalf of an operator who pointed at one row.
    ///
    /// The route derives its fence from the CALLER, like every other route
    /// this client dials. The rail presents the operator bearer, whose scope is
    /// unconditional, so it passes; the same route called by a person is
    /// fenced to the subtree they head.
    ///
    /// # Errors
    /// [`ActuationError`] when chiefd cannot be reached or refuses — including
    /// `person-not-staffed` and `destination-paused`, which are answers about
    /// the company and belong in front of the operator rather than in a log.
    pub async fn wake_person(&self, person_id: &str) -> Result<(), ActuationError> {
        const PATH: &str = "/v1/org/person/wake";
        let body = serde_json::json!({ "slug": self.document_key, "personId": person_id });
        self.post(PATH, &body, ROSTER_BUDGET).await.map(|_answer| ())
    }

    /// The changefeed URL this actuator subscribes to.
    #[must_use]
    pub fn watch_url(&self, after: Option<u64>) -> String {
        format!("{}{}", self.base, watch_path(&self.document_key, after))
    }

    /// Park on the changefeed until something happens, or until `budget`.
    ///
    /// **This is the wait, and it is not a poll.** The connection is opened once
    /// and held; chiefd pushes. `budget` is the lease-renewal deadline, not a
    /// sampling interval — it is the ceiling on how long this loop will sit on
    /// one connection before reopening it, which is what keeps a silently dead
    /// feed from parking the actuator for ever.
    ///
    /// `after` resumes the feed. Passing `None` on the first connection replays
    /// whatever the ring still holds, which costs exactly one extra round; every
    /// connection after that carries the highest seq the loop has seen, which is
    /// what stops a reconnect from replaying the same backlog and waking itself
    /// in a tight loop forever.
    ///
    /// # Errors
    /// [`ActuationError::Transport`] when the feed cannot be opened at all.
    /// A feed that opens and then ends is [`Wake::Closed`], not an error.
    pub async fn wait(&self, after: Option<u64>, budget: Duration) -> Result<Wake, ActuationError> {
        let url = self.watch_url(after);
        match tokio::time::timeout(budget, self.stream_until_event(&url)).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => Ok(Wake::Quiet),
        }
    }

    /// Hold the SSE connection open until it yields one event this loop cares
    /// about, or the server ends the stream.
    ///
    /// A `401` is answered BEFORE the stream opens, so the ordinary
    /// re-acquire-once rule applies here exactly as it does to the three reads.
    /// A token that dies MID-stream produces no status code at all — the daemon
    /// drops the connection — and that case is A4's, not this packet's: a
    /// dropped stream is re-authenticated rather than merely reconnected.
    async fn stream_until_event(&self, url: &str) -> Result<Wake, ActuationError> {
        match self.open_stream(url, self.authorization().await?).await? {
            Opened::Wake(wake) => Ok(wake),
            Opened::Refused { status: 401, .. } => {
                self.bearer.invalidate(&self.base);
                match self.open_stream(url, self.authorization().await?).await? {
                    Opened::Wake(wake) => Ok(wake),
                    Opened::Refused { status, body } => {
                        Err(refusal("/v1/docs/watch", status, &body))
                    }
                }
            }
            Opened::Refused { status, body } => Err(refusal("/v1/docs/watch", status, &body)),
        }
    }

    /// Open the changefeed once. The credential is not optional — see
    /// [`ActuationClient::authorization`].
    async fn open_stream(
        &self,
        url: &str,
        authorization: String,
    ) -> Result<Opened, ActuationError> {
        let transport = |reason: String| ActuationError::Transport { url: url.to_owned(), reason };
        let request = hyper::Request::builder()
            .method("GET")
            .uri(url)
            .header("accept", "text/event-stream")
            .header(hyper::header::AUTHORIZATION, authorization)
            .body(http_body_util::Full::new(hyper::body::Bytes::new()))
            .map_err(|error| transport(error.to_string()))?;
        let response =
            self.inner.request(request).await.map_err(|error| transport(error.to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            // A changefeed that refuses is a wiring fault, never a wake: falling
            // through to "nothing happened" would turn a permanently broken
            // subscription into a silent renewal-only loop that never actuates.
            let body = response.into_body().collect().await.map_or_else(
                |_| String::new(),
                |collected| String::from_utf8_lossy(&collected.to_bytes()).into_owned(),
            );
            return Ok(Opened::Refused { status, body });
        }
        let mut body = response.into_body();
        let mut buffer = String::new();
        let mut wake: Option<Wake> = None;
        loop {
            // ONE WAKE PER BURST, NOT ONE PER EVENT.
            //
            // This used to `return` on the first event it parsed, and that is
            // what made the changefeed look like a poll. The caller reconnects
            // after every wake, so N writes in a row cost N connections — each
            // one an HTTP round trip, an `Authorization` header, and a replay
            // scan of the ring — and, worse, N converge rounds. Measured on the
            // operator's box: **2029 `/v1/docs/watch` calls in eight minutes,
            // 46% of all daemon traffic**, against 4 subscribers and roughly one
            // write a second. 4 x 1/s x 480s is 1920, which is that number: not
            // a hot loop, one reconnect per event per subscriber.
            //
            // It is pure waste, because a wake carries no payload the caller
            // uses. Both callers re-read the whole company and reconcile to a
            // fixed point, so ten events and one event ask for exactly the same
            // work — which is why 81 of 229 converge rounds applied nothing.
            //
            // So once an event has arrived, keep absorbing whatever else is
            // already in flight and answer ONCE. The grace window is what makes
            // this a coalesce rather than a race: a burst is written in a few
            // milliseconds and arrives spread over slightly more than that, so
            // returning the instant the socket goes quiet would still split most
            // bursts in two.
            let next = if wake.is_some() {
                match tokio::time::timeout(WAKE_COALESCE, body.frame()).await {
                    // The socket went quiet: the burst is over, answer with it.
                    Err(_elapsed) => break,
                    Ok(frame) => frame,
                }
            } else {
                body.frame().await
            };
            let Some(frame) = next else {
                // The stream ended. A wake already in hand outranks the close:
                // the events were real and the caller must act on them.
                return Ok(Opened::Wake(wake.unwrap_or(Wake::Closed)));
            };
            let frame = frame.map_err(|error| transport(error.to_string()))?;
            let Ok(chunk) = frame.into_data() else {
                // A trailers frame carries no event data. Keep reading.
                continue;
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(split) = buffer.find("\n\n") {
                let block = buffer[..split].to_owned();
                buffer.drain(..split + 2);
                if let Some(parsed) = parse_event_block(&block) {
                    wake = Some(coalesce(wake, parsed));
                }
            }
        }
        Ok(Opened::Wake(wake.unwrap_or(Wake::Closed)))
    }

    /// One read, with the bearer attached — and exactly ONE retry when chiefd
    /// says the token is no good.
    ///
    /// A `401` means the cached token outlived the thing that anchored it: the
    /// identity's key rotated, or the daemon restarted onto a fresh ephemeral
    /// signing secret. Both are fixed by minting a new token. A SECOND `401` is
    /// not a stale credential — it is an identity this daemon does not accept —
    /// and retrying it would put the identical question at whatever rate the
    /// socket allows, which is the loop `route_error.rs` forbids.
    async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        budget: Duration,
    ) -> Result<String, ActuationError> {
        let (status, text) =
            self.send(path, body, budget, Some(self.authorization().await?)).await?;
        if status == 200 {
            return Ok(text);
        }
        if status != 401 {
            return Err(refusal(path, status, &text));
        }
        self.bearer.invalidate(&self.base);
        let (status, text) =
            self.send(path, body, budget, Some(self.authorization().await?)).await?;
        if status == 200 {
            return Ok(text);
        }
        Err(refusal(path, status, &text))
    }

    /// One `POST`, answering the status and the raw body. No retry, no refusal
    /// decoding: those belong to the caller that knows whether it has already
    /// re-acquired.
    ///
    /// The `Option` is the ONE place a credential is legitimately absent, and
    /// it is not a policy this function decides. This is also the body of
    /// [`JsonPost::post_json_unauthenticated`], the transport the two exempt
    /// auth routes are dialled through — a transport that attached a bearer to
    /// them would need a token in order to get a token. Every other caller
    /// passes `Some`, because [`ActuationClient::authorization`] has no answer
    /// that means "go without one".
    async fn send(
        &self,
        path: &str,
        body: &serde_json::Value,
        budget: Duration,
        authorization: Option<String>,
    ) -> Result<(u16, String), ActuationError> {
        let url = format!("{}{path}", self.base);
        let transport = |reason: String| ActuationError::Transport { url: url.clone(), reason };
        let mut builder = hyper::Request::builder()
            .method("POST")
            .uri(&url)
            .header("content-type", "application/json");
        if let Some(value) = authorization {
            builder = builder.header(hyper::header::AUTHORIZATION, value);
        }
        let request = builder
            .body(http_body_util::Full::new(hyper::body::Bytes::from(body.to_string())))
            .map_err(|error| transport(error.to_string()))?;
        let response = tokio::time::timeout(budget, self.inner.request(request))
            .await
            .map_err(|_elapsed| transport(format!("no response within {budget:?}")))?
            .map_err(|error| transport(error.to_string()))?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| transport(error.to_string()))?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// What one connection to the changefeed produced: a wake, or a refusal the
/// caller must decide about (a `401` is re-acquirable; nothing else is).
enum Opened {
    /// The stream opened and ended for one of [`Wake`]'s four reasons.
    Wake(Wake),
    /// The stream never opened.
    Refused {
        /// The HTTP status.
        status: u16,
        /// The body, for the refusal decoder.
        body: String,
    },
}

/// The UNAUTHENTICATED half of this client's transport, which is what the two
/// middleware-exempt auth routes require: a transport that attached a bearer to
/// them would need a token in order to get a token.
impl JsonPost for ActuationClient {
    async fn post_json_unauthenticated(
        &self,
        url: String,
        body: serde_json::Value,
    ) -> Result<(u16, String), String> {
        // The URL is absolute and already carries this client's base, so the
        // path passed to `send` is the remainder. Splitting it back off keeps
        // ONE request builder rather than a second one that could drift.
        let path = url.strip_prefix(&self.base).unwrap_or(&url).to_owned();
        self.send(&path, &body, AUTH_BUDGET, None).await.map_err(|error| error.to_string())
    }
}

/// The `/v1/docs/watch` path and query for one company.
///
/// Pure, so the one place a store list or a resume point could be lost is a
/// value a test can hold.
#[must_use]
pub fn watch_path(document_key: &str, after: Option<u64>) -> String {
    let stores = WAKE_STORES.join(",");
    let mut path = format!("/v1/docs/watch?slug={document_key}&stores={stores}");
    if let Some(seq) = after {
        path.push_str(&format!("&after={seq}"));
    }
    path
}

/// Decode chiefd's `{code, detail}` refusal body.
///
/// A body that is not a refusal still produces one, carrying the raw text as
/// its detail: a status this client did not expect is still a refusal it must
/// surface verbatim, and inventing a friendlier message would hide the only
/// evidence an operator has.
fn refusal(path: &str, status: u16, body: &str) -> ActuationError {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    ActuationError::Refused {
        path: path.to_owned(),
        status,
        code: field("code").unwrap_or_else(|| "unknown".to_owned()),
        detail: field("detail").unwrap_or_else(|| body.trim().to_owned()),
    }
}

/// Read one SSE block into a [`Wake`], or `None` when it is not an event this
/// loop acts on (a `:hb` heartbeat comment, or an event kind it does not know).
///
/// The `seq` is taken from the `data` payload first and the `id:` line second.
/// Both carry it — the route publishes it in both places precisely so a
/// hand-rolled parser never has to correlate the two — and preferring the
/// payload means a malformed `id:` line cannot silently reset the resume point
/// to zero, which would replay the whole ring on the next connection.
#[must_use]
pub fn parse_event_block(block: &str) -> Option<Wake> {
    let mut event = String::new();
    let mut data = String::new();
    let mut id = None;
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = value.to_owned(),
            "data" => data.push_str(value),
            "id" => id = value.parse::<u64>().ok(),
            _ => {}
        }
    }
    match event.as_str() {
        "reorg" => Some(Wake::Reorg),
        "doc-change" => {
            let seq = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|value| value.get("seq").and_then(serde_json::Value::as_u64))
                .or(id)
                .unwrap_or(0);
            Some(Wake::Change { seq })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub daemon that answers the two auth routes and streams ONE burst of
    /// `events` doc-changes on `/v1/docs/watch` back to back, then goes quiet
    /// without closing.
    ///
    /// BACK TO BACK, with no sleep between them, and that is about keeping the
    /// test honest under load rather than about realism — a real burst is one
    /// transaction publishing to several stores, so back to back IS the shape.
    /// An earlier version spaced them two milliseconds apart, which made every
    /// gap a race against the coalesce window: under `cargo test --workspace`
    /// the machine is saturated, a 2ms sleep can overshoot 25ms, and the drain
    /// would correctly conclude the burst had ended and answer a lower
    /// sequence. It failed exactly once that way, in the full run and never
    /// alone, which is the signature of a timing flake and not of a defect.
    /// With no gaps the only timing this test depends on is the quiet tail,
    /// which is the property actually under test.
    ///
    /// Quiet-but-open is the case that matters: it is what a real feed does
    /// between bursts, and it is the only shape that can tell a client which
    /// STOPS at the first event from one which drains. A stream that closed
    /// would hand both of them everything.
    ///
    /// Answers the base URL and the connection counter.
    ///
    /// The sleep is the STUB DAEMON's, spacing the events it writes — it is not
    /// this crate waiting on anything, so the injected-clock rule the lint
    /// enforces does not reach it. The client under test does its own waiting
    /// through `tokio::time::timeout`, which is exactly what is being measured.
    #[allow(clippy::disallowed_methods)]
    async fn bursting_daemon(events: u64) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::response::sse::{Event, Sse};
        use axum::routing::{get, post};

        let connections = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&connections);
        let app = axum::Router::new()
            .route(
                "/v1/auth/challenge",
                post(|| async { axum::Json(serde_json::json!({"nonceId":"n","nonce":"abc"})) }),
            )
            .route(
                "/v1/auth/token",
                post(|| async { axum::Json(serde_json::json!({"token":"jwt-1"})) }),
            )
            .route(
                "/v1/docs/watch",
                get(move || {
                    let seen = Arc::clone(&seen);
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let stream = futures_util::stream::unfold(0u64, move |sent| async move {
                            if sent < events {
                                let seq = sent + 1;
                                let event = Event::default()
                                    .event("doc-change")
                                    .data(format!("{{\"seq\":{seq}}}"));
                                return Some((Ok::<_, std::convert::Infallible>(event), seq));
                            }
                            // Quiet, and deliberately never closed.
                            std::future::pending::<()>().await;
                            None
                        });
                        Sse::new(stream)
                    }
                }),
            );
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the stub daemon");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, connections)
    }

    /// A client pointed at `base`, with a staged operator key so it can mint a
    /// token rather than refusing before it reaches the network.
    ///
    /// Staging a key fixture in a tempdir is the sanctioned use of the
    /// seam-disallowed writer — production filesystem effects belong to
    /// `chiefd_host` and nothing in this crate writes a key. The same allow
    /// `sidebar/rail/tests.rs` carries, for the same fixture.
    #[allow(clippy::disallowed_methods)]
    fn client_for(base: &str, dir: &std::path::Path) -> ActuationClient {
        use std::os::unix::fs::PermissionsExt as _;

        use p256::pkcs8::{EncodePrivateKey as _, LineEnding};

        let keys = keys_of(dir);
        std::fs::create_dir_all(&keys).expect("keys dir");
        let secret = p256::SecretKey::from_slice(&[9u8; 32]).expect("scalar");
        let path = identity_keys::operator_key_path(&keys);
        std::fs::write(&path, secret.to_pkcs8_pem(LineEnding::LF).expect("pem").as_bytes())
            .expect("write key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        // ONE PATH, STAGED AND READ. This fixture used to stage at
        // `<root>/keys` and read from `<root>/../keys`, because the two
        // constructors took roots one directory apart and both were called "a
        // root" — the #13 collision, reproduced in a test. It passed anyway on
        // a machine where an earlier fixture had left `/tmp/keys/operator.key`
        // lying around (the root is a tempdir under `/tmp`, so the parent IS
        // `/tmp`) and failed on a clean CI runner. There is no root to resolve
        // now: `Bearer::operator` takes the KEYS DIRECTORY, so the fixture
        // hands it the same value it staged into.
        ActuationClient::new(base, "0123456789ab", Arc::new(crate::bearer::Bearer::operator(&keys)))
    }

    /// A company's keys directory, `<dir>/.chief/keys` — the production layout
    /// `chief_cli::paths::keys_dir` names, spelled here because the library
    /// half may not reach the binary's module.
    fn keys_of(dir: &std::path::Path) -> std::path::PathBuf {
        identity_keys::keys_dir(&dir.join(".chief"))
    }

    /// THE MEASUREMENT: five writes cost ONE wake and ONE connection.
    ///
    /// This is the fix stated as a number. The caller reconnects after every
    /// wake and re-reads the whole company, so before this drained, five writes
    /// in a row cost five connections and five converge rounds — and the answer
    /// to all five was identical, because a wake carries no payload anyone
    /// reads. On the operator's box that was 2029 `/v1/docs/watch` calls in
    /// eight minutes, 46% of every request the daemon served, and 81 of 229
    /// converge rounds that applied nothing.
    ///
    /// `seq: 5` is the whole assertion. A client that stopped at the first
    /// event would answer `seq: 1` here, having consumed one event and left
    /// four to be collected by four more connections.
    #[tokio::test]
    async fn a_burst_of_five_writes_costs_one_wake_and_one_connection() {
        use std::sync::atomic::Ordering;

        let data_root = tempfile::tempdir().expect("tempdir");
        let (base, connections) = bursting_daemon(5).await;
        let client = client_for(&base, data_root.path());

        let wake = client.wait(None, Duration::from_secs(5)).await.expect("the feed answers");

        assert_eq!(
            wake,
            Wake::Change { seq: 5 },
            "one wake must carry the WHOLE burst: a client that stopped at the first event would \
             answer seq 1 and leave four more connections' worth behind it"
        );
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "and it must cost ONE connection — each one is an HTTP round trip, an Authorization \
             header and a replay scan of the ring"
        );
    }

    /// THE CONTROL: a single write still wakes immediately.
    ///
    /// The drain must not turn a quiet company into a delayed one. One event,
    /// then silence, still answers — bounded by the coalesce window, nowhere
    /// near the caller's own one-second floor.
    #[tokio::test]
    async fn a_single_write_still_wakes_without_waiting_for_a_burst() {
        let data_root = tempfile::tempdir().expect("tempdir");
        let (base, _connections) = bursting_daemon(1).await;
        let client = client_for(&base, data_root.path());

        let started = std::time::Instant::now();
        let wake = client.wait(None, Duration::from_secs(5)).await.expect("the feed answers");

        assert_eq!(wake, Wake::Change { seq: 1 });
        // Two seconds against a five-second budget, and deliberately loose: the
        // claim is "this is not gated on an interval", not a latency figure. A
        // tight bound here would only measure how busy the machine running the
        // suite is — which is how the sibling test above learned to flake.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a lone write answers as soon as the socket goes quiet, not after some interval: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_wake_filter_carries_every_store_that_can_change_what_chiefd_wants() {
        // `runtime-actuation` is not excluded here — it no longer exists, and
        // this client writes nothing at all, so there is no store whose changes
        // it could wake itself with.
        assert!(
            !WAKE_STORES.contains(&"runtime-actuation"),
            "the actuation store is deleted; nothing may subscribe to it"
        );
        for expected in ["activity", "supervision", "converge-safety", "org-manifest", "mailbox/"] {
            assert!(WAKE_STORES.contains(&expected), "{expected} is work somebody else commits");
        }
    }

    #[test]
    fn the_watch_query_carries_the_company_the_stores_and_the_resume_point() {
        let first = watch_path("acme@abc123", None);
        assert!(first.contains("slug=acme@abc123"), "{first}");
        assert!(
            first.contains("stores=activity,supervision,converge-safety,org-manifest"),
            "{first}"
        );
        assert!(
            !first.contains("after="),
            "a first connection has nothing to resume from: {first}"
        );

        let resumed = watch_path("acme@abc123", Some(42));
        assert!(resumed.ends_with("&after=42"), "{resumed}");
    }

    #[test]
    fn a_doc_change_wakes_the_loop_and_carries_its_sequence() {
        let block =
            "event: doc-change\nid: 7\ndata: {\"seq\":7,\"slug\":\"acme\",\"store\":\"activity\"}";
        assert_eq!(parse_event_block(block), Some(Wake::Change { seq: 7 }));
    }

    #[test]
    fn a_sequence_is_read_from_the_payload_even_when_the_id_line_is_unusable() {
        // A resume point that silently fell back to zero would replay the whole
        // ring on the next connection and wake the loop immediately, forever.
        let block = "event: doc-change\nid: not-a-number\ndata: {\"seq\":19}";
        assert_eq!(parse_event_block(block), Some(Wake::Change { seq: 19 }));
    }

    #[test]
    fn a_reorg_is_its_own_wake_rather_than_a_change() {
        assert_eq!(parse_event_block("event: reorg\ndata: {}"), Some(Wake::Reorg));
    }

    #[test]
    fn a_heartbeat_comment_is_not_a_wake() {
        assert_eq!(parse_event_block(":hb"), None, "quiet state must stay quiet");
        assert_eq!(parse_event_block(""), None);
        assert_eq!(parse_event_block("event: something-else\ndata: {}"), None);
    }

    #[test]
    fn a_refusal_body_is_decoded_into_its_machine_code() {
        let error = refusal(
            "/v1/org/runtime/desired",
            422,
            r#"{"code":"unknown-company","detail":"carries nobody"}"#,
        );
        match &error {
            ActuationError::Refused { status, code, detail, .. } => {
                assert_eq!(*status, 422);
                assert_eq!(code, "unknown-company");
                assert_eq!(detail, "carries nobody");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!error.is_transient(), "a product rule is never retried");
    }

    #[test]
    fn an_unrecognized_error_body_keeps_its_text_rather_than_inventing_one() {
        let error = refusal("/v1/org/runtime/desired", 503, "chiefd is starting");
        match error {
            ActuationError::Refused { code, detail, .. } => {
                assert_eq!(code, "unknown");
                assert_eq!(detail, "chiefd is starting", "the only evidence must survive");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A 5xx must not permanently un-actuate a company.
    ///
    /// The loop returns on a non-transient error, so this classification IS the
    /// difference between riding out a restarting daemon and leaving a company
    /// with nobody — silently, since chiefd can no longer report that nobody is
    /// actuating.
    #[test]
    fn a_server_failure_is_retried_and_a_product_refusal_is_not() {
        let refusal = |status| ActuationError::Refused {
            path: "/v1/org/runtime/desired".to_owned(),
            status,
            code: "x".to_owned(),
            detail: String::new(),
        };
        assert!(refusal(500).is_transient(), "a store hiccup is not a product rule");
        assert!(refusal(503).is_transient(), "including this crate's own two 503s");
        assert!(!refusal(404).is_transient(), "a company this daemon does not serve is terminal");
        assert!(!refusal(422).is_transient(), "a body this client should not have built is too");
        assert!(
            refusal(429).is_transient(),
            "429 is 'back off and retry' by the taxonomy's own words, not a verdict on the caller"
        );
        assert!(!refusal(403).is_transient(), "a verdict about the caller stays terminal");

        // #1204, both measured answers in one place. The daemon USED to send
        // the second one for a seven-second store stall and this loop exited
        // on it, correctly; it sends the first one now and this loop survives
        // it. Nothing about `is_transient` changed — the server's sentence did.
        let store_fault = ActuationError::Refused {
            path: "/v1/docs/watch".to_owned(),
            status: 503,
            code: "identity-store-unavailable".to_owned(),
            detail: "the identity store could not be read".to_owned(),
        };
        assert!(store_fault.is_transient(), "a trust decision not yet made is asked again");
        let revoked = ActuationError::Refused {
            path: "/v1/docs/watch".to_owned(),
            status: 403,
            code: "unknown".to_owned(),
            detail: "unknown identity".to_owned(),
        };
        assert!(!revoked.is_transient(), "a REAL 403 stays terminal; retrying it loops forever");
    }

    #[test]
    fn only_a_transport_failure_is_worth_trying_again() {
        let transport = ActuationError::Transport {
            url: "http://127.0.0.1:9/".to_owned(),
            reason: "connection refused".to_owned(),
        };
        assert!(transport.is_transient(), "a restarting daemon is not a refusal");
        let undecodable = ActuationError::Undecodable {
            path: "/v1/org/roster/desired".to_owned(),
            detail: "missing field `people`".to_owned(),
        };
        assert!(!undecodable.is_transient());
    }

    /// A surface with no actuator configuration refuses the catalog with its
    /// own code. The loop must NOT treat that as terminal: it is handled inside
    /// the pass (the actuator refuses every start by name and reports it) and
    /// never propagated out of [`crate::actuate::resident::run`], because an
    /// actuator that exited over a catalog it could re-ask for a second later
    /// leaves the company with nobody at all.
    #[test]
    fn an_unavailable_launch_catalog_decodes_into_its_own_machine_code() {
        let error = refusal(
            "/v1/org/runtime/launch-catalog",
            503,
            r#"{"code":"launch-catalog-unavailable","detail":"this chiefd surface has no actuator configuration"}"#,
        );
        match &error {
            ActuationError::Refused { status, code, path, .. } => {
                assert_eq!(*status, 503);
                assert_eq!(code, "launch-catalog-unavailable");
                assert_eq!(path, "/v1/org/runtime/launch-catalog");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The catalog route does real filesystem work — it walks every person's
    /// materialized home and stages their provider credential — so its budget
    /// must be the widest of the three, not copied from the roster's.
    #[test]
    fn the_launch_catalog_budget_allows_for_a_walk_of_every_persons_home() {
        assert!(LAUNCH_CATALOG_BUDGET > ROSTER_BUDGET);
        assert!(LAUNCH_CATALOG_BUDGET > DESIRED_BUDGET);
    }

    #[test]
    fn the_client_addresses_the_composite_key_and_never_the_bare_slug() {
        let client = ActuationClient::new(
            "http://127.0.0.1:8791/",
            "acme@abc123",
            Arc::new(Bearer::service(std::path::Path::new("/home/pat/.chiefd"))),
        );
        assert_eq!(client.document_key(), "acme@abc123");
        assert_eq!(
            client.watch_url(Some(3)),
            "http://127.0.0.1:8791/v1/docs/watch?slug=acme@abc123&stores=activity,supervision,converge-safety,org-manifest,mailbox/&after=3",
            "a trailing slash on the base must never double the separator"
        );
        // A3: this actuator is its OWN principal. It never borrows the
        // operator's, because an audit record that could not tell an automatic
        // action from a deliberate one would be worth much less than one that
        // can.
        assert_eq!(client.bearer.identity_id(), "service");
    }

    /// A key this client cannot use is a HARD REFUSAL, and it names the file.
    ///
    /// The tempting alternative — warn and call anonymously — is what turns a
    /// credential-hygiene failure (a key gone missing, or one whose mode
    /// widened so anybody on the box can read it) into an unauthenticated
    /// request that nothing reports. Nothing is dialled at all: the refusal
    /// happens before the socket, which is why the discard port here never
    /// produces a transport error.
    #[tokio::test]
    async fn an_unusable_identity_key_refuses_the_read_rather_than_calling_anonymously() {
        let client = ActuationClient::new(
            "http://127.0.0.1:9",
            "acme@abc123",
            Arc::new(Bearer::service(std::path::Path::new("/nonexistent"))),
        );
        let error = client.desired().await.expect_err("there is no key on disk");
        let ActuationError::Credential { detail } = &error else {
            panic!("a local key failure must not be reported as a transport one: {error:?}")
        };
        assert!(detail.contains("service.key"), "the refusal names the file: {detail}");
        assert!(!error.is_transient(), "no amount of retrying creates a key");
    }

    /// The auth routes are two of the three the verify-middleware exempts, and
    /// they are what MINTS the bearer. A transport that authenticated them
    /// would ask for a token in order to get a token.
    #[tokio::test]
    async fn the_acquisition_transport_carries_no_credential() {
        // Port 9 (discard) never answers, so this asserts the shape of the
        // request that is built rather than any server behaviour: the call
        // fails as a transport error, never as an authorization one.
        let client = ActuationClient::new(
            "http://127.0.0.1:9",
            "acme@abc123",
            Arc::new(Bearer::service(std::path::Path::new("/nonexistent"))),
        );
        let error = client
            .post_json_unauthenticated(
                "http://127.0.0.1:9/v1/auth/challenge".to_owned(),
                serde_json::json!({ "identityId": "service" }),
            )
            .await
            .expect_err("nothing answers on the discard port");
        assert!(error.contains("could not reach"), "{error}");
    }

    /// THE RULE: a burst answers ONCE, at its HIGHEST sequence.
    ///
    /// The caller stores the answer as its resume point and reconnects. Taking
    /// anything lower would replay events it has already been told about and
    /// wake it again for them — the loop the resume point exists to prevent.
    /// Answering once per EVENT instead is what made the changefeed 46% of the
    /// daemon's traffic: 2029 calls in eight minutes.
    #[test]
    fn a_burst_answers_once_at_its_highest_sequence() {
        let mut wake = None;
        for seq in [7, 8, 9] {
            wake = Some(coalesce(wake, Wake::Change { seq }));
        }
        assert_eq!(
            wake,
            Some(Wake::Change { seq: 9 }),
            "the resume point must clear the WHOLE burst; a lower one replays events the caller \
             has already been woken for"
        );
    }

    /// THE RULE: the highest sequence wins whatever order it arrived in.
    ///
    /// The feed is ordered, so this should not arise — which is exactly why it
    /// is pinned. `max` is the rule; "whatever arrived last" only happens to
    /// agree with it while the feed behaves.
    #[test]
    fn the_resume_point_never_goes_backwards() {
        let wake = coalesce(Some(Wake::Change { seq: 12 }), Wake::Change { seq: 4 });
        assert_eq!(wake, Wake::Change { seq: 12 });
    }

    /// THE RULE: a reorg is never swallowed by the burst it arrived in.
    ///
    /// A reorg says the resume point is unusable and the caller must resync.
    /// Coalescing it away would leave the caller resuming from a sequence the
    /// feed has just told it is meaningless — a silent, permanent desync, far
    /// worse than the one extra round a reorg costs.
    #[test]
    fn a_reorg_outranks_every_change_in_its_burst() {
        assert_eq!(
            coalesce(Some(Wake::Change { seq: 3 }), Wake::Reorg),
            Wake::Reorg,
            "a reorg arriving after changes still wins"
        );
        assert_eq!(
            coalesce(Some(Wake::Reorg), Wake::Change { seq: 99 }),
            Wake::Reorg,
            "and a change arriving after a reorg cannot bury it"
        );
    }

    /// THE CONTROL: the first event of a burst is taken as-is.
    ///
    /// Without this, a `coalesce` that ignored its argument and returned a
    /// constant would satisfy every assertion above.
    #[test]
    fn the_first_event_is_the_wake_when_nothing_is_held() {
        assert_eq!(coalesce(None, Wake::Change { seq: 5 }), Wake::Change { seq: 5 });
        assert_eq!(coalesce(None, Wake::Reorg), Wake::Reorg);
    }

    /// THE RULE: the grace window coalesces a burst and never delays the work.
    ///
    /// The caller gates itself at one second
    /// (`resident::Schedule::min_round_interval`), so a wake held for a
    /// fraction of that cannot change when it acts. A window anywhere near those floors would stop
    /// being a coalesce and start being a debounce on the work itself.
    #[test]
    fn the_coalesce_window_cannot_delay_the_work_it_precedes() {
        assert!(
            WAKE_COALESCE < Duration::from_millis(100),
            "a burst is written in one transaction and arrives within a couple of milliseconds"
        );
    }

    /// A stub daemon whose `/v1/auth/challenge` answers `status` with `body`,
    /// and which COUNTS how many times `/v1/org/runtime/desired` was called.
    ///
    /// The counter is the assertion that matters in the two tests below. The
    /// deleted arm's whole signature was a read that went out with no header
    /// after the mint failed, so a `desired` handler that is never reached is
    /// the direct evidence that no bare call happens any more.
    async fn daemon_refusing_the_mint(
        status: u16,
        body: &'static str,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::routing::post;

        let desired_calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&desired_calls);
        let app = axum::Router::new()
            .route(
                "/v1/auth/challenge",
                post(move || async move {
                    (axum::http::StatusCode::from_u16(status).expect("status"), body)
                }),
            )
            .route(
                "/v1/org/runtime/desired",
                post(move || {
                    let counted = Arc::clone(&counted);
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "company": "acme",
                            "actuationMode": "apply",
                            "people": [],
                        }))
                    }
                }),
            );
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the stub daemon");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, desired_calls)
    }

    /// #1204 — A MINT THAT FAILED TRANSIENTLY IS RETRIED, NOT EXITED ON.
    ///
    /// The measured death has a second door the server fix alone does not
    /// close. A cached token survives a store blackout, because the thing that
    /// faults is the middleware's read — but a chiefd restart rotates the
    /// ephemeral secret and invalidates every cached token, and that is
    /// exactly when the store is slowest. The mint then fails, and the old
    /// code called the READ bare, got `401 missing bearer token`, re-minted
    /// (failing the same way), called bare again and returned a TERMINAL 401
    /// naming `/v1/org/runtime/desired`. Now the 503 from the auth route is
    /// what surfaces, it names the auth route, and it is transient.
    #[tokio::test]
    async fn a_mint_refused_with_a_5xx_is_a_transient_refusal_of_the_auth_route() {
        use std::sync::atomic::Ordering;

        let data_root = tempfile::tempdir().expect("tempdir");
        let (base, desired_calls) = daemon_refusing_the_mint(
            503,
            r#"{"code":"identity-store-unavailable","detail":"the identity store could not be read"}"#,
        )
        .await;
        let client = client_for(&base, data_root.path());

        let error = client.desired().await.expect_err("the mint is refused");
        let ActuationError::Refused { path, status, code, .. } = &error else {
            panic!("a mint refusal must carry the auth route's own status: {error:?}")
        };
        assert_eq!(path, "/v1/auth/challenge", "the refusal names the CAUSE, not a symptom");
        assert_eq!(*status, 503);
        assert_eq!(code, "identity-store-unavailable");
        assert!(error.is_transient(), "the actuator must ride this out, not exit");
        assert_eq!(
            desired_calls.load(Ordering::SeqCst),
            0,
            "THE BARE-CALL ARM IS GONE: no request may go out without a credential"
        );
    }

    /// The other half. A settled mint refusal is still terminal — an identity
    /// this daemon does not accept does not become acceptable by asking again
    /// — and it names `/v1/auth/challenge` rather than whichever read happened
    /// to need the token.
    #[tokio::test]
    async fn a_mint_refused_with_a_401_is_terminal_and_names_the_auth_route() {
        use std::sync::atomic::Ordering;

        let data_root = tempfile::tempdir().expect("tempdir");
        let (base, desired_calls) =
            daemon_refusing_the_mint(401, "unknown or inactive identity").await;
        let client = client_for(&base, data_root.path());

        let error = client.desired().await.expect_err("the mint is refused");
        let ActuationError::Refused { path, status, .. } = &error else {
            panic!("expected a refusal, got {error:?}")
        };
        assert_eq!(path, "/v1/auth/challenge");
        assert_eq!(*status, 401);
        assert!(!error.is_transient(), "an identity this daemon rejects is not retried");
        assert_eq!(desired_calls.load(Ordering::SeqCst), 0, "and still nothing goes out bare");
    }

    /// An auth route nothing answers is a TRANSPORT failure and therefore
    /// transient — a restarting daemon, which is the ordinary case. The key is
    /// staged, so this is the mint failing rather than the local refusal that
    /// `an_unusable_identity_key_refuses_the_read_rather_than_calling_anonymously`
    /// covers.
    #[tokio::test]
    async fn an_unreachable_mint_is_a_transport_failure() {
        let data_root = tempfile::tempdir().expect("tempdir");
        // Port 9 (discard) never answers.
        let client = client_for("http://127.0.0.1:9", data_root.path());

        let error = client.desired().await.expect_err("nothing answers the auth route");
        let ActuationError::Transport { url, .. } = &error else {
            panic!("an unreachable mint is not a refusal: {error:?}")
        };
        assert!(url.contains("/v1/auth/challenge"), "the failure names the route: {url}");
        assert!(error.is_transient(), "a restarting daemon is not a reason to stop actuating");
    }
}
