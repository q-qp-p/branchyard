//! Reconciliation: entries whose outcome is not known are settled by
//! asking the upstream, never by calling again.
//!
//! - A `begun` entry whose turn is no longer running (its engine stopped
//!   between writing the entry and recording the answer) becomes
//!   `unknown`. This needs no network, so recovery does it.
//! - An `unknown` entry with a lookup is looked up through the gateway,
//!   under the branch's grant: the call the gateway's report named (a
//!   failed call still reports one), or, for an answer lost entirely, the
//!   lookup the tool's contract declares, resolved when the call was made
//!   from its arguments and its id (the idempotency key). An answer is
//!   `confirmed`; `not_found`, or nothing, is `failed`. Without a lookup,
//!   or when the lookup cannot answer, it stays `unknown` and is shown to
//!   the person; the gateway's audit log can settle it too
//!   (`by effects reconcile --audit`).
//! - A `confirmed` entry whose inverse's deadline passed, with no
//!   compensation left, is `expired`.
//!
//! Run on recovery (the first step), by `by effects reconcile`, by the
//! gateway's supervisor and the server on a timer.
//!
//! A lookup that cannot answer (the upstream is down, the gateway refuses)
//! is not asked again on every sweep: the entry's durable timer
//! (`docs/durability.md#timers`) holds when it is next asked, backing off from 30
//! seconds to an hour, and a claim on it keeps two reconcilers sharing a
//! store from asking at once. A settled entry's timer is finished.

use serde::{Deserialize, Serialize};

use super::ask::note;
use super::mcp::{CallError, Client};
use super::{EffectActivity, EffectEntry, EffectMove, EffectState};
use crate::timers::backoff;
use crate::{Error, Yard};

/// The kind of timer that paces an unknown entry's lookups.
pub(crate) const LOOKUP_TIMER: &str = "effect_lookup";
/// How long a claimed lookup holds its entry: longer than a lookup takes.
const LOOKUP_LEASE_MS: u64 = 120_000;
/// The wait after a lookup's first failure; it doubles after each.
const FIRST_RETRY_MS: u64 = 30_000;
/// The longest wait between two lookups of one entry.
const MAX_RETRY_MS: u64 = 3_600_000;

/// What one reconciliation did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reconciled {
    /// Entries settled, with the state each reached.
    pub settled: Vec<(String, EffectState)>,
    /// Entries still `unknown`, with why.
    pub unknown: Vec<(String, String)>,
}

/// Whether `entry`'s turn may still be running: its branch's lease is held
/// by a live engine and the running turn is the entry's. A branch that is
/// gone has no turn running; a store that cannot be read is an error, not
/// "not running": that would move the effects of a turn still in flight to
/// `unknown`.
pub(crate) fn turn_running(
    store: &crate::state::Store,
    entry: &EffectEntry,
) -> Result<bool, Error> {
    // Lease validity follows the authoritative store clock, even when an
    // obligation or reconciliation is evaluated at an explicit time.
    let now = store.backend().clock_ms()?;
    let record = match store.read(&entry.branch) {
        Ok(record) => record,
        Err(Error::UnknownBranch(_)) => return Ok(false),
        Err(error) => {
            return Err(error.could_not(format_args!("read the record for {}", entry.branch)))
        }
    };
    let held = store
        .backend()
        .leases()
        .map_err(|e| e.could_not("read the branch leases"))?
        .into_iter()
        .any(|l| {
            l.branch == entry.branch
                && l.turn == u64::from(entry.turn)
                && l.owner.is_some()
                && l.stale(now).is_none()
        });
    Ok(record.info.status == crate::BranchStatus::Running
        && held
        && record.info.turns + 1 == entry.turn
        && entry.created_ms >= record.created_ms)
}

/// Move `begun` entries whose turn is not running to `unknown`. No network.
pub(crate) fn orphans(yard: &Yard, now: u64) -> Result<Vec<String>, Error> {
    let ledger = yard.store();
    let mut moved = Vec::new();
    for entry in ledger.effects().effects(None)? {
        if entry.state != EffectState::Begun || turn_running(&ledger, &entry)? {
            continue;
        }
        let change = EffectMove::to(EffectState::Unknown).detail(
            "its turn ended before the call's answer was recorded; it may or may not have happened",
        );
        if let Some(done) =
            ledger
                .effects()
                .move_effect(&entry.id, &[EffectState::Begun], &change, now)?
        {
            note(yard, &done.branch, EffectActivity::of(&done));
            moved.push(done.id);
        }
    }
    Ok(moved)
}

/// Reconcile the whole ledger at `now`.
pub(crate) fn run(yard: &Yard, now: u64) -> Result<Reconciled, Error> {
    let mut report = Reconciled::default();
    orphans(yard, now)?;
    let ledger = yard.store();
    let entries = ledger.effects().effects(None)?;
    for entry in &entries {
        match entry.state {
            EffectState::Unknown if entry.lookup.is_none() => report.unknown.push((
                entry.id.clone(),
                "its operation declares no lookup; check upstream and decide".into(),
            )),
            EffectState::Unknown => {
                if let Some(settled) = paced_lookup(yard, entry, now, &mut report)? {
                    report.settled.push(settled);
                }
            }
            EffectState::Confirmed
                if entry.expired_at(now) && entry.compensation_at(now).is_none() =>
            {
                let change =
                    EffectMove::to(EffectState::Expired).detail("its undo's deadline passed");
                if let Some(done) = ledger.effects().move_effect(
                    &entry.id,
                    &[EffectState::Confirmed],
                    &change,
                    now,
                )? {
                    note(yard, &done.branch, EffectActivity::of(&done));
                    report.settled.push((done.id, done.state));
                }
            }
            _ => {}
        }
    }
    // A timer whose entry was settled some other way (the audit log, a
    // person) is finished, so the table holds only what is still owed.
    let timers = ledger.timers();
    for timer in timers.timers(Some(LOOKUP_TIMER))? {
        let open = entries
            .iter()
            .any(|e| e.id == timer.key && e.state == EffectState::Unknown);
        if !open {
            timers.finish_timer(LOOKUP_TIMER, &timer.key, timer.revision)?;
        }
    }
    Ok(report)
}

/// Look `entry` up if its timer lets this reconciler: claimed, asked, and
/// either settled (its timer finished) or set again with a backoff. What
/// stays unknown, and when it is asked next, goes in `report`.
fn paced_lookup(
    yard: &Yard,
    entry: &EffectEntry,
    now: u64,
    report: &mut Reconciled,
) -> Result<Option<(String, EffectState)>, Error> {
    let ledger = yard.store();
    let timers = ledger.timers();
    let Some(claim) = timers.claim_timer(LOOKUP_TIMER, &entry.id, now, LOOKUP_LEASE_MS)? else {
        let why = match timers.timer(LOOKUP_TIMER, &entry.id)? {
            Some(t) if t.claimed_until_ms.is_some_and(|until| until > now) => {
                "another reconciler is looking it up now".to_owned()
            }
            Some(t) => format!(
                "its lookup could not answer before; it is asked again at {}",
                super::undo::clock(t.due_ms)
            ),
            None => "it was settled meanwhile".to_owned(),
        };
        report.unknown.push((entry.id.clone(), why));
        return Ok(None);
    };
    match lookup(yard, entry) {
        Ok(Some(change)) => {
            let done =
                ledger
                    .effects()
                    .move_effect(&entry.id, &[EffectState::Unknown], &change, now)?;
            timers.finish_timer(LOOKUP_TIMER, &entry.id, claim.revision)?;
            Ok(done.map(|done| {
                note(yard, &done.branch, EffectActivity::of(&done));
                (done.id, done.state)
            }))
        }
        Ok(None) => {
            timers.finish_timer(LOOKUP_TIMER, &entry.id, claim.revision)?;
            report.unknown.push((
                entry.id.clone(),
                "its operation declares no lookup; check upstream and decide".into(),
            ));
            Ok(None)
        }
        Err(why) => {
            let next = now.saturating_add(backoff(claim.attempts, FIRST_RETRY_MS, MAX_RETRY_MS));
            timers.set_timer(LOOKUP_TIMER, &entry.id, next, &serde_json::Value::Null)?;
            report.unknown.push((
                entry.id.clone(),
                format!("{why}; it is asked again at {}", super::undo::clock(next)),
            ));
            Ok(None)
        }
    }
}

/// Ask the upstream whether `entry`'s call happened: `None` when there is
/// no lookup to ask.
fn lookup(yard: &Yard, entry: &EffectEntry) -> Result<Option<EffectMove>, String> {
    let Some(lookup) = &entry.lookup else {
        return Ok(None);
    };
    let record = yard
        .store()
        .read(&entry.branch)
        .map_err(|e| format!("its branch is gone ({e}); check upstream and decide"))?;
    let (gateway, token) = crate::connectors::branch_token(yard, &record, entry.turn)?;
    let called = Client::new(&gateway.url, token)
        .follow_up(&lookup.tool, &lookup.arguments, None)
        .map_err(|e: CallError| format!("the lookup: {e}"))?;
    Ok(Some(match called.found() {
        Some(true) => EffectMove::to(EffectState::Confirmed).detail(format!(
            "the lookup ({}) found it: the call happened",
            lookup.operation
        )),
        Some(false) => EffectMove::to(EffectState::Failed).detail(format!(
            "the lookup ({}) did not find it: the call did not happen",
            lookup.operation
        )),
        None => return Err(format!("the lookup failed: {}", called.answer())),
    }))
}

/// [`run`] now, by the store's clock.
pub(crate) fn now(yard: &Yard) -> Result<Reconciled, Error> {
    run(yard, yard.store().backend().clock_ms()?)
}
