//! Durable timers: one due time per `(kind, key)`, claimed by revision
//! (`docs/durability.md#timers`).
//!
//! A timer is a row, not a sleeping thread: it survives a restart, and
//! several engines or servers on one store can share it. A claim marks a
//! due timer taken until a lease runs out, so of any number of claimants
//! one acts on it; a claimant that dies lets the lease lapse and the timer
//! is claimed again, which makes delivery at least once. Setting a timer
//! again bumps its revision, so a claimant finishing the revision it took
//! never deletes a later setting: [`TimerBackend::finish_timer`] removes
//! the row only when the revision still matches.
//!
//! Handlers therefore have to be idempotent. The first user is effect
//! reconciliation ([`crate::effects::reconcile`]): an unknown call whose
//! lookup could not answer is asked again on a backoff, not on every sweep.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

/// One timer, as the store holds it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    /// What the timer is for, such as `effect_lookup`.
    pub kind: String,
    /// What it is about within its kind, such as an effect entry's id.
    pub key: String,
    /// When it is due, milliseconds since the Unix epoch.
    pub due_ms: u64,
    /// Bumped each time the timer is set; finishing it names the revision.
    pub revision: u64,
    /// How many times it has been claimed.
    pub attempts: u32,
    /// While claimed: until when, milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_until_ms: Option<u64>,
    /// What its handler needs, as the setter gave it.
    #[serde(default)]
    pub payload: Value,
}

impl Timer {
    /// Whether it may be claimed at `now_ms`: due, and not held by a claim
    /// still in force.
    pub fn claimable(&self, now_ms: u64) -> bool {
        self.due_ms <= now_ms && self.claimed_until_ms.is_none_or(|until| until <= now_ms)
    }
}

/// Durable timers, in SQLite or PostgreSQL beside the branches. A
/// separate trait, on the same backend value, as
/// [`crate::effects::EffectBackend`] is.
pub(crate) trait TimerBackend: Send + Sync + fmt::Debug {
    /// Set `(kind, key)` due at `due_ms` with `payload`, creating it or
    /// replacing its due time and payload and dropping any claim; its
    /// attempts are kept. Returns the new revision.
    fn set_timer(&self, kind: &str, key: &str, due_ms: u64, payload: &Value) -> Result<u64, Error>;
    /// Claim `(kind, key)` at `now_ms` for `lease_ms`: a missing timer is
    /// created due now and claimed; a claimable one is claimed. `None`
    /// when it is not due yet or another claim holds it.
    fn claim_timer(
        &self,
        kind: &str,
        key: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<Timer>, Error>;
    /// Delete `(kind, key)` if its revision is still `revision`. False when
    /// it was set again meanwhile, or is gone.
    fn finish_timer(&self, kind: &str, key: &str, revision: u64) -> Result<bool, Error>;
    /// One timer.
    fn timer(&self, kind: &str, key: &str) -> Result<Option<Timer>, Error>;
    /// Every timer, or those of `kind`, by due time.
    fn timers(&self, kind: Option<&str>) -> Result<Vec<Timer>, Error>;
}

/// The wait before attempt `attempts + 1` after `attempts` failed ones:
/// `first_ms` doubled each time, at most `cap_ms`.
pub(crate) fn backoff(attempts: u32, first_ms: u64, cap_ms: u64) -> u64 {
    let doublings = attempts.saturating_sub(1).min(32);
    first_ms.saturating_mul(1u64 << doublings).min(cap_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_first_wait_up_to_the_cap() {
        let waits: Vec<u64> = (1..=8).map(|a| backoff(a, 30_000, 3_600_000)).collect();
        assert_eq!(
            waits,
            [30_000, 60_000, 120_000, 240_000, 480_000, 960_000, 1_920_000, 3_600_000]
        );
        assert_eq!(backoff(0, 30_000, 3_600_000), 30_000);
        assert_eq!(backoff(u32::MAX, 30_000, 3_600_000), 3_600_000);
    }

    #[test]
    fn a_timer_is_claimable_when_due_and_not_held() {
        let mut timer = Timer {
            kind: "k".into(),
            key: "a".into(),
            due_ms: 100,
            revision: 1,
            attempts: 0,
            claimed_until_ms: None,
            payload: Value::Null,
        };
        assert!(!timer.claimable(99));
        assert!(timer.claimable(100));
        timer.claimed_until_ms = Some(200);
        assert!(!timer.claimable(150));
        assert!(timer.claimable(200));
    }
}
