//! Whether a running branch's engine is still heard from, read from its
//! lease (`docs/durability.md#liveness`).
//!
//! A branch's record says `running` from the moment a turn takes its lease
//! until the turn concludes. While the engine lives it renews the lease
//! every [`HEARTBEAT`] for [`LEASE_TTL`]; when it stops, nothing changes in
//! the record until recovery takes the lease over. Between those two
//! moments `running` is a claim nobody is backing. [`Liveness`] says which
//! case a reader is in, the way the effect ledger says `unknown` rather
//! than guessing: `live` while renewals arrive, `unknown` once they have
//! stopped but the lease still protects the turn, `lost` once the lease no
//! longer protects a live engine and recovery will end the turn.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::state::{LeaseRow, HEARTBEAT, LEASE_TTL};

/// What is known about a running turn's engine; see the module docs.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessState {
    /// The engine renewed the lease within the last two heartbeats.
    Live,
    /// No renewal for longer than two heartbeats, and the lease has not
    /// expired: the engine may be busy, paused or gone. Nothing acts on it
    /// until the lease expires.
    Unknown,
    /// The lease no longer protects a live engine: it expired, or its
    /// engine's process is gone from this host. The next recovery ends the
    /// turn `interrupted`.
    Lost,
}

impl LivenessState {
    /// `live`, `unknown` or `lost`.
    pub fn as_str(&self) -> &'static str {
        match self {
            LivenessState::Live => "live",
            LivenessState::Unknown => "unknown",
            LivenessState::Lost => "lost",
        }
    }
}

/// A running branch's liveness, read from its lease when the branch is
/// listed or looked up; never stored.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Liveness {
    /// What is known.
    pub state: LivenessState,
    /// When the engine last renewed the lease, milliseconds since the Unix
    /// epoch.
    pub heard_ms: u64,
    /// Until when the lease protects the turn without another renewal,
    /// milliseconds since the Unix epoch: past it, the turn is `lost`.
    pub known_until_ms: u64,
    /// Why the turn is `lost`, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Liveness {
    /// The liveness `lease` gives at `now_ms`, or `None` for a released
    /// lease, which backs no turn.
    pub(crate) fn of(lease: &LeaseRow, now_ms: u64) -> Option<Liveness> {
        lease.owner.as_ref()?;
        let ttl = duration_ms(LEASE_TTL);
        let heard_ms = lease.expires_ms.saturating_sub(ttl);
        let silent = now_ms.saturating_sub(heard_ms);
        let reason = lease.stale(now_ms);
        let state = match (&reason, silent > 2 * duration_ms(HEARTBEAT)) {
            (Some(_), _) => LivenessState::Lost,
            (None, true) => LivenessState::Unknown,
            (None, false) => LivenessState::Live,
        };
        Some(Liveness {
            state,
            heard_ms,
            known_until_ms: lease.expires_ms,
            reason,
        })
    }

    /// For people: nothing while live; `no heartbeat for 14s` while
    /// unknown; the reason once lost.
    pub fn describe(&self, now_ms: u64) -> Option<String> {
        let silent = now_ms.saturating_sub(self.heard_ms) / 1000;
        match self.state {
            LivenessState::Live => None,
            LivenessState::Unknown => Some(format!("no heartbeat for {silent}s")),
            LivenessState::Lost => Some(format!(
                "engine lost: {}",
                self.reason.as_deref().unwrap_or("its lease lapsed")
            )),
        }
    }
}

/// The liveness of each branch holding a lease at `now_ms`, by branch.
pub(crate) fn by_branch(leases: &[LeaseRow], now_ms: u64) -> HashMap<String, Liveness> {
    leases
        .iter()
        .filter_map(|lease| Some((lease.branch.clone(), Liveness::of(lease, now_ms)?)))
        .collect()
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(expires_ms: u64) -> LeaseRow {
        LeaseRow {
            branch: "b".into(),
            incarnation: 1,
            generation: 1,
            turn: 1,
            owner: Some("engine".into()),
            // Another host: whether its process lives is not known here,
            // so only the clock decides.
            host: "elsewhere".into(),
            pid: 1,
            start: "0".into(),
            expires_ms,
            deadline_ms: None,
        }
    }

    #[test]
    fn a_renewed_lease_is_live_a_silent_one_unknown_and_an_expired_one_lost() {
        let heard = 1_000_000;
        let held = lease(heard + 30_000);
        let live = Liveness::of(&held, heard + 10_000).unwrap();
        assert_eq!(live.state, LivenessState::Live);
        assert_eq!(live.heard_ms, heard);
        assert_eq!(live.known_until_ms, heard + 30_000);
        assert_eq!(live.describe(heard + 10_000), None);

        let unknown = Liveness::of(&held, heard + 10_001).unwrap();
        assert_eq!(unknown.state, LivenessState::Unknown);
        assert_eq!(
            unknown.describe(heard + 14_000).as_deref(),
            Some("no heartbeat for 14s")
        );

        let lost = Liveness::of(&held, heard + 31_000).unwrap();
        assert_eq!(lost.state, LivenessState::Lost);
        assert_eq!(
            lost.describe(heard + 31_000).as_deref(),
            Some("engine lost: its engine's lease expired 1s ago")
        );
    }

    #[test]
    fn a_released_lease_backs_no_turn() {
        let mut released = lease(2_000_000);
        released.owner = None;
        assert_eq!(Liveness::of(&released, 1_000_000), None);
        assert!(by_branch(&[released], 1_000_000).is_empty());
    }

    #[test]
    fn an_engine_gone_from_this_host_is_lost_before_its_lease_expires() {
        let mut gone = lease(2_000_000);
        gone.host = crate::proc::host().to_owned();
        gone.pid = u32::MAX;
        let liveness = Liveness::of(&gone, 1_990_000).unwrap();
        assert_eq!(liveness.state, LivenessState::Lost);
        assert!(
            liveness
                .reason
                .as_deref()
                .unwrap()
                .contains("no longer running"),
            "{liveness:?}"
        );
    }
}
