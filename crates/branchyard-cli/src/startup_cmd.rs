//! `by startup report`: percentiles of the startup phases the engine
//! recorded for this repository's turns, per provider and path. See
//! `docs/providers.md#measuring-launch-time`.

use std::time::Duration;

use branchyard::startup::{self, Filter};
use branchyard_support::time::now_ms;

use crate::commands::Failure;
use crate::commands::{open, print, Env, Outcome, Target};
use crate::errors::Report;

/// Feed events read per page.
const PAGE: usize = 1000;

/// Why the report does not run against a server.
fn local_only() -> Failure {
    Report::usage("startup measurements are read from this repository's own store")
        .next("run it without --remote, in the repository")
        .into()
}

/// The cutoff `--since` means at `now`: startups recorded before it are
/// left out.
fn cutoff(now: u64, since: Option<Duration>) -> Option<u64> {
    since.map(|window| now.saturating_sub(u64::try_from(window.as_millis()).unwrap_or(u64::MAX)))
}

/// `by startup report [--provider P] [--since DUR] [--json]`.
pub fn report(
    _env: &Env,
    target: &Target,
    provider: Option<String>,
    since: Option<Duration>,
    as_json: bool,
) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(local_only());
    }
    let yard = open()?;
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = yard.events_since(cursor, PAGE)?;
        if page.events.is_empty() {
            break;
        }
        cursor = page.next_cursor;
        events.extend(page.events);
    }
    let report = startup::report(
        &events,
        &Filter {
            provider,
            since_ms: cutoff(now_ms(), since),
        },
    );
    if as_json {
        let value = serde_json::to_value(&report).unwrap_or_default();
        return print(&crate::json::text(&value));
    }
    print(&report.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_counts_back_from_now_and_stops_at_the_epoch() {
        assert_eq!(cutoff(10_000, None), None);
        assert_eq!(cutoff(10_000, Some(Duration::from_secs(4))), Some(6_000));
        assert_eq!(cutoff(1_000, Some(Duration::from_secs(60))), Some(0));
    }
}
