//! `by startup report`: percentiles of the startup phases the engine
//! recorded for this repository's turns, per provider and path. See
//! `docs/providers.md#measuring-launch-time`.

use std::time::Duration;

use branchyard::startup::{self, Filter};
use branchyard::{FeedEvent, Yard};
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
    let events = feed(&open()?)?;
    print(&render(&events, provider, cutoff(now_ms(), since), as_json))
}

/// Every event in `yard`'s feed, oldest first.
fn feed(yard: &Yard) -> Result<Vec<FeedEvent>, Failure> {
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
    Ok(events)
}

/// The report over `events`, as JSON or a table.
fn render(
    events: &[FeedEvent],
    provider: Option<String>,
    since_ms: Option<u64>,
    as_json: bool,
) -> String {
    let report = startup::report(events, &Filter { provider, since_ms });
    if as_json {
        let value = serde_json::to_value(&report).unwrap_or_default();
        return crate::json::text(&value);
    }
    report.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{env, remote, report_of};
    use branchyard::startup::{Phases, Startup, StartupPath};
    use branchyard::{Activity, RecordedEvent};

    fn startup(position: u64, at_ms: u64, provider: &str, total: u64) -> FeedEvent {
        FeedEvent {
            position,
            branch: "b".into(),
            event: RecordedEvent {
                at_ms,
                activity: Activity::Startup(Box::new(Startup {
                    provider: provider.into(),
                    path: StartupPath::Cold,
                    phases: Phases {
                        queued: Some(0),
                        first_event: Some(total),
                        ..Phases::default()
                    },
                })),
            },
        }
    }

    #[test]
    fn since_counts_back_from_now_and_stops_at_the_epoch() {
        assert_eq!(cutoff(10_000, None), None);
        assert_eq!(cutoff(10_000, Some(Duration::from_secs(4))), Some(6_000));
        assert_eq!(cutoff(1_000, Some(Duration::from_secs(60))), Some(0));
    }

    #[test]
    fn a_server_is_refused_with_the_way_out() {
        let failure = report(&env(), &remote(), None, None, false).unwrap_err();
        let report = report_of(failure);
        assert_eq!(report.kind, "usage");
        assert!(report.next[0].contains("without --remote"), "{report:?}");
    }

    #[test]
    fn the_table_counts_recorded_startups_and_says_when_there_are_none() {
        let events = vec![
            startup(1, 1_000, "microsandbox", 400),
            startup(2, 2_000, "microsandbox", 800),
            startup(3, 3_000, "container", 100),
        ];
        let all = render(&events, None, None, false);
        assert!(all.starts_with("3 startups\n"), "{all}");
        assert!(all.contains("microsandbox") && all.contains("container"));
        let one = render(&events, Some("container".into()), None, false);
        assert!(one.starts_with("1 startups\n"), "{one}");
        assert!(!one.contains("microsandbox"), "{one}");
        let recent = render(&events, None, Some(2_000), false);
        assert!(recent.starts_with("2 startups\n"), "{recent}");
        assert_eq!(
            render(&events, Some("none".into()), None, false),
            "no recorded startups\n"
        );
        assert_eq!(render(&[], None, None, false), "no recorded startups\n");
    }

    #[test]
    fn json_is_the_report_itself() {
        let events = vec![startup(1, 1_000, "microsandbox", 400)];
        let value: serde_json::Value =
            serde_json::from_str(&render(&events, None, None, true)).unwrap();
        assert_eq!(value["startups"], 1);
        let total = value["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["phase"] == "total")
            .unwrap();
        assert_eq!(total["provider"], "microsandbox");
        assert_eq!(total["path"], "cold");
        assert_eq!(total["p50_ms"], 400);
        let empty: serde_json::Value =
            serde_json::from_str(&render(&[], None, None, true)).unwrap();
        assert_eq!(empty, serde_json::json!({"startups": 0, "rows": []}));
    }

    #[test]
    fn a_new_yard_has_an_empty_feed() {
        let dir = branchyard_testkit::Scratch::new("startup-unit");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        let yard = Yard::open(dir.path()).unwrap();
        assert!(feed(&yard).unwrap().is_empty());
    }
}
