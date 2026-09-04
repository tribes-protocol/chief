//! The durable per-recipient mailbox — duty #8's store half, and the durable
//! target of effect delivery (duty #7).
//!
//! It carries what a now-deleted TypeScript mailbox store and the durable half
//! of its supervision transport used to own. What "delivering to a mailbox"
//! MEANS in this system is precisely a write here: a durable envelope row for a
//! recipient, staged `pending` until that recipient's live pane drains it and
//! moves it to a terminal bucket. That durable write is what the transport calls "delivered";
//! the runtime wake that follows is best-effort and its failure never
//! un-delivers the envelope (inv 16). This module owns the durable side; the
//! wake actuation is an injected [`RuntimeWaker`] the host implements.
//!
//! # Relational rows, not a per-person document (recorded divergence)
//!
//! The TypeScript stores each person's whole mailbox as one `org_documents` row
//! `mailbox/<personId>` holding a five-bucket map. chiefd already chose the
//! native shape: a relational `mailbox` table keyed per `(envelope, recipient)`
//! (`schema.rs`), listed among the relational store ledgers in `ledger.rs`. The
//! one-daemon migration's whole direction is off the document contract and onto
//! chiefd-native rows, so the mailbox is a relational sub-store exactly like
//! `effects`; the TypeScript five-bucket map becomes the row's
//! [`MailboxRow::state`] column.
//!
//! # The two invariants that survive verbatim
//!
//! 1. **Durable publish is insert-if-absent by effect identity.** A recipient's
//!    row id is a deterministic, **time-independent** function of the envelope id
//!    (which the delivery path sets to the effect id) and the recipient —
//!    deliberately NOT of `createdAt`, so a crash-retry re-rendered on a later
//!    delivery pass lands on the SAME row and is a no-op success rather than a
//!    duplicate. This is the crux of the two-commit dispatch: the sink stages the
//!    mailbox row, the scheduler commits `mark_delivered` afterward, and the two
//!    commits are NOT atomic — if the process dies between them the effect stays
//!    pending and the next wake pass re-presents the same id, which must be
//!    harmless. A present row is therefore left exactly as-is (never resurrected,
//!    never rewritten).
//! 2. **Absence is not unreachability.** "this person has no pending mail" and
//!    "the store could not be read" are different outcomes end to end. A reader
//!    that collapses the second into the first recreates the silent-mail outage
//!    this store exists to prevent; here the store is `FailClosed`-adjacent (its
//!    rows ride the supervision-grade company database) and a decode failure of
//!    one row is skipped, never read as "no mail for anyone".

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ledger::{Ledgers, MailboxRow};
use crate::ChiefdError;

/// Schema version of a durable envelope body.
pub const MAILBOX_ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// The envelope could not be serialized for storage — the only failure the
/// durable stage can report (a present row is an idempotent no-op, never an
/// error). In practice unreachable for a well-formed envelope; surfaced rather
/// than swallowed so a genuinely unstorable envelope becomes a delivery failure
/// the breaker owns, not silently dropped mail.
pub const MAILBOX_UNSERIALIZABLE: &str = "mailbox-unserializable";

/// The lifecycle bucket of a durable envelope. Port of the TypeScript
/// `MailboxBucket`: `pending` is the live/incoming state, the four terminal
/// buckets are the archive states, and every transition is `pending → terminal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailboxState {
    /// Awaiting drain by the recipient's live pane.
    Pending,
    /// #493(A) fence-archive terminal (Fable ruling #5): the wake-demand was
    /// consumed at fence commit. DISJOINT from the pane-drain terminals below —
    /// a row reaches `Delivered` via the fence-archive path, never via a pane
    /// drain, and the two paths never overlap.
    Delivered,
    /// The recipient accepted it — the ordinary "delivered and read" terminal
    /// (a pane-drain terminal).
    Accepted,
    /// Overtaken by a later envelope before it was accepted (pane-drain).
    Superseded,
    /// The recipient rejected it (pane-drain).
    Rejected,
    /// A health-incident alert whose incident was resolved (pane-drain).
    Resolved,
}

/// Which of the two DISJOINT terminal families a bucket belongs to (#493
/// disjointness invariant, Fable ruling #5). `Pending` belongs to neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalFamily {
    /// The wake-demand-consumed-at-fence-commit archive: only `Delivered`.
    FenceArchive,
    /// The recipient's pane drained it: accepted/superseded/rejected/resolved.
    PaneDrain,
}

impl Urgency {
    /// The stored spelling (matches the schema CHECK: 'normal' | 'interrupt').
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Interrupt => "interrupt",
        }
    }

    /// Parse the stored spelling. Unknown text is `None` — a row whose urgency
    /// cannot be read is corruption, never silently downgraded to `normal`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "normal" => Some(Self::Normal),
            "interrupt" => Some(Self::Interrupt),
            _ => None,
        }
    }
}

impl MailboxState {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
            Self::Rejected => "rejected",
            Self::Resolved => "resolved",
        }
    }

    /// Parse the stored spelling. Unknown text is `None`, never a default: a row
    /// whose bucket cannot be read must not be silently treated as pending
    /// (re-delivered forever) or terminal (silently dropped).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "accepted" => Some(Self::Accepted),
            "superseded" => Some(Self::Superseded),
            "rejected" => Some(Self::Rejected),
            "resolved" => Some(Self::Resolved),
            _ => None,
        }
    }

    /// Whether this row still appears in the recipient's durable inbox view.
    ///
    /// `Delivered` is the fence archive: it no longer supplies launch demand,
    /// but it stays in the inbox view.
    #[must_use]
    pub const fn is_inbox_message(self) -> bool {
        match self {
            Self::Pending | Self::Delivered => true,
            Self::Accepted | Self::Superseded | Self::Rejected | Self::Resolved => false,
        }
    }

    /// Whether this row supplies pending-mail launch demand.
    #[must_use]
    pub const fn supplies_launch_demand(self) -> bool {
        match self {
            Self::Pending => true,
            Self::Delivered
            | Self::Accepted
            | Self::Superseded
            | Self::Rejected
            | Self::Resolved => false,
        }
    }

    /// Whether this bucket is a terminal (archive) state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }

    /// Which DISJOINT terminal family a bucket belongs to, or `None` for
    /// `Pending`. The #493 invariant: `Delivered` is the ONLY `FenceArchive`
    /// bucket and every other terminal is `PaneDrain`, so the two sets never
    /// overlap (Fable ruling #5).
    #[must_use]
    pub const fn terminal_family(self) -> Option<TerminalFamily> {
        match self {
            Self::Pending => None,
            Self::Delivered => Some(TerminalFamily::FenceArchive),
            Self::Accepted | Self::Superseded | Self::Rejected | Self::Resolved => {
                Some(TerminalFamily::PaneDrain)
            }
        }
    }

    /// Whether this bucket was reached by the #493 fence-archive path.
    #[must_use]
    pub const fn is_fence_archived(self) -> bool {
        matches!(self.terminal_family(), Some(TerminalFamily::FenceArchive))
    }

    /// Whether this bucket was reached by a recipient's pane drain.
    #[must_use]
    pub const fn is_pane_drained(self) -> bool {
        matches!(self.terminal_family(), Some(TerminalFamily::PaneDrain))
    }
}

/// How urgently an envelope wants its recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    /// Delivered on the next drain.
    Normal,
    /// Wants an immediate wake (an escalation).
    Interrupt,
}

/// The health-incident reference a health-alert envelope carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthIncidentRef {
    /// The incident fingerprint.
    pub fingerprint: String,
    /// The incident kind.
    pub kind: String,
    /// The operator runtime authorized to accept this alert.
    pub recipient_person_id: String,
}

/// A durable envelope, byte-faithful to the TypeScript `OrganizationEnvelope`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxEnvelope {
    /// Always [`MAILBOX_ENVELOPE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The logical envelope id. Deterministic per effect, so re-publishing is
    /// idempotent.
    pub id: String,
    /// The company slug.
    pub organization: String,
    /// Who sent it (`"launcher"` for system effects).
    pub from_person_id: String,
    /// The primary recipient.
    pub to: String,
    /// Every recipient — one durable row is written per entry.
    pub recipients: Vec<String>,
    /// The message body.
    pub body: String,
    /// Delivery urgency.
    pub urgency: Urgency,
    /// A conversation this replies to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Incident reference, on health-alert envelopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_incident: Option<HealthIncidentRef>,
    /// ISO-8601 creation stamp.
    pub created_at: String,
}

impl MailboxEnvelope {
    /// The durable row id for one recipient — the mailbox table's primary key.
    ///
    /// Deterministic in `(id, person)` ONLY — deliberately **not** in
    /// `createdAt`. The delivery path sets [`id`](MailboxEnvelope::id) to the
    /// effect id, so a crash-retry re-rendered at a later time (hence a different
    /// `createdAt`) still resolves to the same row and is an idempotent no-op.
    /// Folding `createdAt` into the key would break exactly that property and
    /// duplicate mail on every retried pass.
    #[must_use]
    pub fn row_id(&self, person: &str) -> String {
        format!("{}@{}", self.id, person)
    }
}

/// Durably stage `envelope` into each recipient's mailbox as `pending`,
/// insert-if-absent by [`MailboxEnvelope::row_id`].
///
/// The durable half of delivery. For every distinct recipient: if no row exists
/// for this `(effect id, recipient)`, write a fresh `pending` one; if a row
/// already exists — in ANY bucket — leave it exactly as-is. It is a pure
/// insert-if-absent, never a compare-and-overwrite, because the two-commit
/// dispatch re-presents the same effect id after a crash and that replay must be
/// a harmless no-op, not a conflict and not a rewrite.
///
/// Returns the recipients whose row is `pending` after this call — the set a
/// caller should wake. A recipient whose row has already reached a terminal
/// bucket is not returned: their pane already drained it, so a wake would be
/// spurious.
///
/// # Errors
/// [`MAILBOX_UNSERIALIZABLE`] only if the envelope cannot be serialized — a
/// present row is always an idempotent success, never an error.
pub fn enqueue(
    ledgers: &mut Ledgers,
    envelope: &MailboxEnvelope,
) -> Result<Vec<String>, ChiefdError> {
    // Columnarized store (schema Part B, Fable #7): the envelope is held as a
    // typed value on the row, NOT re-serialized into an opaque `body` blob. The
    // SQL persistence maps its fields to typed columns; `recipients`,
    // `organization` and `schemaVersion` are DERIVED at reconstruct, never stored.
    let now = ledgers.now().0;
    // A BTreeSet dedups repeated recipients and fixes a deterministic order, so
    // one envelope naming a person twice writes one row and two callers agree.
    let recipients: BTreeSet<&str> = envelope.recipients.iter().map(String::as_str).collect();

    let mut pending = Vec::new();
    for person in recipients {
        let id = envelope.row_id(person);
        // Read phase: decide under an immutable borrow, released before any write.
        // `Some(is_pending)` means a row already exists (idempotent replay);
        // `None` means insert a fresh pending row.
        let existing_pending = ledgers
            .mailbox(&id)
            .map(|row| MailboxState::parse(&row.state) == Some(MailboxState::Pending));
        match existing_pending {
            Some(true) => pending.push(person.to_string()),
            Some(false) => {} // already drained to a terminal bucket — no wake
            None => {
                ledgers.put_mailbox(
                    id,
                    MailboxRow {
                        person: person.to_string(),
                        envelope: envelope.clone(),
                        state: MailboxState::Pending.as_str().to_string(),
                        updated_at: now,
                    },
                );
                pending.push(person.to_string());
            }
        }
    }
    // Kept `Result` for signature stability (a caller matches its `Err` arm); a
    // columnarized enqueue has no serialization step and so no failure mode.
    Ok(pending)
}

/// Every pending envelope for `person`, oldest first.
///
/// Ordered by the envelope's `createdAt` then its row id, so the order is a
/// function of the mail itself and not of storage insertion order
/// (TESTING.md §1.2). A row whose body does not decode to an envelope is skipped
/// rather than aborting the whole page — one corrupt row must not hide the rest
/// of a person's mail.
#[must_use]
pub fn pending_for(ledgers: &Ledgers, person: &str) -> Vec<MailboxEnvelope> {
    let mut rows: Vec<(String, MailboxEnvelope)> = ledgers
        .mailbox_rows()
        .filter(|(_, row)| {
            row.person == person && MailboxState::parse(&row.state) == Some(MailboxState::Pending)
        })
        .map(|(id, row)| (id.to_string(), row.envelope.clone()))
        .collect();
    rows.sort_by(|(a_id, a), (b_id, b)| {
        (a.created_at.as_str(), a_id.as_str()).cmp(&(b.created_at.as_str(), b_id.as_str()))
    });
    rows.into_iter().map(|(_, env)| env).collect()
}

/// Move one durable envelope from `pending` to a terminal bucket — the drain /
/// archive step the recipient's pane performs when it accepts (or supersedes,
/// rejects, resolves) its mail.
///
/// Idempotent: a row already in a terminal bucket is left unchanged and returns
/// `false`. Returns `true` only when a pending row was actually moved. Moving to
/// [`MailboxState::Pending`] is not a valid archive and is a no-op `false`.
pub fn archive(ledgers: &mut Ledgers, envelope_row_id: &str, to: MailboxState) -> bool {
    if !to.is_terminal() {
        return false;
    }
    let now = ledgers.now().0;
    let Some(row) = ledgers.mailbox(envelope_row_id) else {
        return false;
    };
    if MailboxState::parse(&row.state) != Some(MailboxState::Pending) {
        return false;
    }
    let mut next = row.clone();
    next.state = to.as_str().to_string();
    next.updated_at = now;
    ledgers.put_mailbox(envelope_row_id, next);
    true
}

/// Every recipient who has at least one `pending` envelope, deduplicated and in
/// person order — the input to the wake scan.
#[must_use]
pub fn pending_recipients(ledgers: &Ledgers) -> BTreeSet<String> {
    ledgers
        .mailbox_rows()
        .filter(|(_, row)| MailboxState::parse(&row.state) == Some(MailboxState::Pending))
        .map(|(_, row)| row.person.clone())
        .collect()
}

/// #110/#551: whether an envelope is the launcher re-emitting STANDING STATE on
/// a cadence — a recurring restatement, never new information. It stays pending
/// and durable and is delivered the next time the person is up for any other
/// reason, but it must never by itself justify a wake or count as pending work
/// that pins a settling person resident: the cadence exists precisely so the
/// person SETTLES and a fresh emission re-wakes it at dispatch time.
///
/// Matches the TS transport's hashed `supervision-<sha256>` ids. Real work is
/// never matched: an escalation is `Interrupt`, and a health alert carries an
/// incident reference.
#[must_use]
pub fn is_launcher_re_emission(envelope: &MailboxEnvelope) -> bool {
    if envelope.from_person_id != "launcher"
        || envelope.urgency == Urgency::Interrupt
        || envelope.health_incident.is_some()
    {
        return false;
    }
    envelope.id.starts_with("supervision-")
}

/// Recipients whose pending mail is REAL demand — [`pending_recipients`] minus
/// launcher cadence re-emissions. This is the input the activity-fence
/// projection must use: feeding it the raw set lets a settling manager's (or
/// blocked worker's) own unread cadence mail read as `Requested` demand on
/// every daemon pass, cancelling every idle park and making CEO-only
/// convergence unreachable (the live #551 failure). Parity with the TypeScript
/// `peopleWithPendingMailboxWork` shrink boundary.
///
/// `since_exclusive_ms` is the #363 goal-delivery quiesce watermark: envelopes
/// dated at-or-before it stay durable but no longer count as demand (a
/// CEO-only reset is not undone by the mail that predates it). `None` means no
/// watermark is in force. An unparseable `createdAt` is excluded whenever a
/// watermark applies — the TypeScript `Date.parse(...) > sinceMs` comparison
/// is `false` for `NaN`, and the parity is deliberate.
#[must_use]
pub fn pending_demand_recipients(
    ledgers: &Ledgers,
    since_exclusive_ms: Option<i64>,
) -> BTreeSet<String> {
    ledgers
        .mailbox_rows()
        .filter(|(_, row)| MailboxState::parse(&row.state) == Some(MailboxState::Pending))
        .filter(|(_, row)| !is_launcher_re_emission(&row.envelope))
        .filter(|(_, row)| {
            since_exclusive_ms.is_none_or(|since| {
                crate::isotime::parse_iso_millis(&row.envelope.created_at)
                    .is_some_and(|created| created > since)
            })
        })
        .map(|(_, row)| row.person.clone())
        .collect()
}

// --- the injected host wake seam ------------------------------------------

/// The host wake, injected so the pure delivery logic is testable without the runtime.
///
/// "Waking" a recipient is NOT injecting the message into a pane — the durable
/// envelope already carries the message. It is ensuring the recipient's live
/// pane *exists* (spawn/respawn), after which the resident agent drains its own
/// mailbox on boot. A recipient whose pane is already live needs no wake; the
/// resident drains in-process. The the real runtime implementation lives in
/// `chiefd-host`; `chiefd-core` may not touch the runtime, so it drives this trait.
pub trait RuntimeWaker {
    /// Best-effort: ensure each recipient's live pane exists so it drains its
    /// durable mailbox. Returns the recipients actually woken.
    ///
    /// Infallible by contract: a recipient that could not be woken is simply
    /// absent from the result, NEVER an error — a failed mailbox wake must never
    /// be mistaken for a failed delivery (inv 16 / the 19-hour-blackout
    /// property). The detached wake scan re-drives whoever is still pending.
    fn wake(&self, recipients: &[String]) -> Vec<String>;
}

// --- the wake scan (duty #8) ----------------------------------------------

/// Whether a reconcile already covering a person is in flight.
///
/// The coalescing seam: when a bounded reconciliation writer already targets a
/// recipient's current activity, issuing a second wake is redundant, so the scan
/// coalesces onto the in-flight one rather than racing it (TypeScript
/// `pending-mailbox-wake-coalesced`).
pub trait WakeDecider {
    /// Whether a reconcile covering `person_id` is already in flight.
    fn reconcile_in_flight(&self, person_id: &str) -> bool;
}

/// A decider that never coalesces — every pending recipient is woken. The
/// correct default when no reconciliation-liveness signal is wired up, because
/// it errs toward waking (fail-safe) rather than silently withholding a wake.
pub struct WakeEveryone;

impl WakeDecider for WakeEveryone {
    fn reconcile_in_flight(&self, _person_id: &str) -> bool {
        false
    }
}

/// The pure result of one wake scan: who to wake now, and who was coalesced onto
/// an in-flight reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WakeScanPlan {
    /// Recipients with pending mail who need a wake this pass.
    pub wake: Vec<String>,
    /// Recipients with pending mail already covered by an in-flight reconcile.
    pub coalesced: Vec<String>,
}

/// Scan the durable mailbox for people with pending mail and partition them into
/// wake-now vs coalesced. Pure: no host I/O, deterministic person order, so it
/// is safe to compute off the writer thread from a snapshot.
#[must_use]
pub fn pending_mailbox_wake_scan(ledgers: &Ledgers, decider: &dyn WakeDecider) -> WakeScanPlan {
    let mut plan = WakeScanPlan::default();
    for person in pending_recipients(ledgers) {
        if decider.reconcile_in_flight(&person) {
            plan.coalesced.push(person);
        } else {
            plan.wake.push(person);
        }
    }
    plan
}

/// The outcome of actuating a wake scan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WakeScanOutcome {
    /// Recipients whose pane the waker actually converged.
    pub woken: Vec<String>,
    /// Recipients coalesced onto an in-flight reconcile (not re-woken).
    pub coalesced: Vec<String>,
}

/// Compute and actuate one pending-mailbox wake pass. The scan decides who needs
/// waking; the waker converges their panes best-effort; a recipient the waker
/// could not reach is simply absent from `woken` (never an error), and the next
/// scan re-drives whoever is still pending.
pub fn run_pending_mailbox_wake(
    ledgers: &Ledgers,
    decider: &dyn WakeDecider,
    waker: &dyn RuntimeWaker,
) -> WakeScanOutcome {
    let plan = pending_mailbox_wake_scan(ledgers, decider);
    let woken = if plan.wake.is_empty() { Vec::new() } else { waker.wake(&plan.wake) };
    WakeScanOutcome { woken, coalesced: plan.coalesced }
}

#[cfg(test)]
mod tests;
