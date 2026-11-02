//! `GET /v1/problems`: what background work could not do that a person
//! can act on, oldest first; empty when nothing needs one. See
//! `docs/server.md#problems`.
//!
//! There is no shared board. Each kind has one reader, next to the data it
//! derives from, and a request calls each and merges what they find. The
//! `trigger` and `operation` kinds are derived from the shared store
//! ([`crate::triggers::problems`], [`crate::ops::Registry::problems`]).
//! The `webhook`, `push` and `effect` kinds are what this process saw:
//! each is written by the one task that does that work (a webhook
//! delivery task's [`Streak`], the push module's map, a repository
//! poller's last reconcile pass) and starts empty when the process does.
//!
//! Every reader returns every tenant's entries with the [`Scope`] that
//! may see them, and the route filters by the caller ([`visible`]).

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::get;
use axum::{Extension, Json, Router};
use branchyard_client::api::{Problem, ProblemList};
use branchyard_support::LockExt as _;

use crate::api::{blocking, App, Caller, Shared};
use crate::config::{Principal, TenantPolicy};
use crate::error::ApiError;

/// Failures in a row before a [`Streak`] is a problem: fewer are noise a
/// retry or the next attempt usually clears.
pub const FAILURES_BEFORE_PROBLEM: u32 = 3;

/// One writer's run of failures, cleared by its next success.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Streak(Option<Failing>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Failing {
    since_ms: u64,
    failures: u32,
    error: String,
}

impl Streak {
    /// Count one failure at `now_ms`: the run's first keeps its time, the
    /// latest its error.
    pub(crate) fn failed(&mut self, now_ms: u64, error: impl Into<String>) {
        let error = error.into();
        match &mut self.0 {
            Some(failing) => {
                failing.failures = failing.failures.saturating_add(1);
                failing.error = error;
            }
            None => {
                self.0 = Some(Failing {
                    since_ms: now_ms,
                    failures: 1,
                    error,
                })
            }
        }
    }

    /// End the run.
    pub(crate) fn succeeded(&mut self) {
        self.0 = None;
    }

    /// `(since_ms, failures, error)` once the run reached
    /// [`FAILURES_BEFORE_PROBLEM`].
    pub(crate) fn problem(&self) -> Option<(u64, u32, &str)> {
        self.0
            .as_ref()
            .filter(|f| f.failures >= FAILURES_BEFORE_PROBLEM)
            .map(|f| (f.since_ms, f.failures, f.error.as_str()))
    }
}

/// Who may see a problem. `admin` never crosses tenants: a credential
/// configured without `scopes` holds `admin`, so every tenant's default
/// token has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// `tenant`'s principals, `admin` or not; only those that may reach
    /// `repo` when it names one.
    Tenant {
        tenant: String,
        repo: Option<String>,
    },
    /// Whoever may reach the repository, `admin` or not.
    Repo(String),
    /// Operator configuration of the repository: principals with `admin`
    /// that may reach it.
    RepoAdmin(String),
}

/// A problem, with who may see it.
pub(crate) struct Found {
    pub scope: Scope,
    pub problem: Problem,
}

/// Whether `principal`, whose tenant's policy is `policy`, may see a
/// problem of `scope`.
fn visible(principal: &Principal, policy: &TenantPolicy, scope: &Scope) -> bool {
    match scope {
        Scope::Tenant { tenant, repo } => {
            *tenant == principal.tenant
                && repo
                    .as_ref()
                    .is_none_or(|r| principal.repo_allowed(policy, r))
        }
        Scope::Repo(repo) => principal.repo_allowed(policy, repo),
        Scope::RepoAdmin(repo) => principal.allows("admin") && principal.repo_allowed(policy, repo),
    }
}

/// What this process's own tasks publish for the route, built once at
/// start: each handle is written by the one task it was given to.
#[derive(Default)]
pub(crate) struct Seen {
    /// Each webhook delivery task's run of failed attempts, keyed
    /// `<repo>:<webhook id>`.
    pub webhooks: BTreeMap<String, crate::webhook::Watch>,
    /// Each repository poller's last reconcile pass: an entry per effect
    /// it left `unknown`, keyed by repository.
    pub effects: BTreeMap<String, Arc<Mutex<Vec<Problem>>>>,
}

impl Seen {
    fn found(&self) -> Vec<Found> {
        let webhooks = self.webhooks.values().filter_map(|watch| {
            watch.problem().map(|problem| Found {
                scope: Scope::RepoAdmin(watch.repo().to_owned()),
                problem,
            })
        });
        let effects = self.effects.iter().flat_map(|(repo, unknown)| {
            unknown
                .lock_recovering("unknown effects")
                .iter()
                .map(|problem| Found {
                    scope: Scope::Repo(repo.clone()),
                    problem: problem.clone(),
                })
                .collect::<Vec<_>>()
        });
        webhooks.chain(effects).collect()
    }
}

pub(crate) fn router() -> Router<Shared> {
    Router::new().route("/v1/problems", get(list))
}

async fn list(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ProblemList>, ApiError> {
    let reader = app.clone();
    let found = blocking(move || read(&reader))
        .await?
        .map_err(|e| ApiError::internal(format!("could not read the problems: {e}")))?;
    let policy = app.tenant_policy(&caller);
    let mut problems: Vec<Problem> = found
        .into_iter()
        .filter(|f| visible(&caller.0, &policy, &f.scope))
        .map(|f| f.problem)
        .collect();
    problems.sort_by_key(|p| p.since_ms);
    Ok(Json(ProblemList { problems }))
}

/// Every kind's problems, of every tenant.
fn read(app: &App) -> io::Result<Vec<Found>> {
    let mut found = crate::triggers::problems(app.triggers.store.as_ref())?;
    found.extend(app.registry.problems()?);
    found.extend(app.seen.found());
    if let Some(companion) = &app.companion {
        found.extend(companion.push_failures.problems(companion.store.as_ref())?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streak_is_a_problem_from_its_third_failure_until_a_success() {
        let mut streak = Streak::default();
        streak.failed(10, "refused with HTTP 500");
        streak.failed(20, "refused with HTTP 502");
        assert_eq!(streak.problem(), None, "two failures are noise");
        streak.failed(30, "timed out");
        assert_eq!(streak.problem(), Some((10, 3, "timed out")));
        streak.failed(40, "refused with HTTP 503");
        assert_eq!(streak.problem(), Some((10, 4, "refused with HTTP 503")));
        streak.succeeded();
        assert_eq!(streak.problem(), None);
        for at in [50, 60, 70] {
            streak.failed(at, "again");
        }
        assert_eq!(streak.problem(), Some((50, 3, "again")), "a new run");
    }

    #[test]
    fn each_scope_is_visible_to_its_own_and_admin_adds_only_its_repositories_configuration() {
        let mut reader = Principal::default_for("reader");
        reader.tenant = "acme".into();
        reader.scopes.remove("admin");
        let policy = TenantPolicy {
            repos: Some(["app".to_owned()].into()),
            ..TenantPolicy::default()
        };
        let tenant = |tenant: &str, repo: Option<&str>| Scope::Tenant {
            tenant: tenant.into(),
            repo: repo.map(Into::into),
        };
        // (scope, seen without admin, seen with admin)
        let cases = [
            (tenant("acme", None), true, true),
            (tenant("acme", Some("app")), true, true),
            (tenant("acme", Some("appb")), false, false),
            (tenant("globex", None), false, false),
            (tenant("globex", Some("app")), false, false),
            (Scope::Repo("app".into()), true, true),
            (Scope::Repo("appb".into()), false, false),
            (Scope::RepoAdmin("app".into()), false, true),
            (Scope::RepoAdmin("appb".into()), false, false),
        ];
        let mut admin = reader.clone();
        admin.scopes.insert("admin".into());
        for (scope, seen, seen_by_admin) in cases {
            assert_eq!(visible(&reader, &policy, &scope), seen, "{scope:?}");
            assert_eq!(
                visible(&admin, &policy, &scope),
                seen_by_admin,
                "admin: {scope:?}"
            );
        }
    }
}
