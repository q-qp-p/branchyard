//! HTTP routes for memory rows, review and consolidation
//! (`docs/memory.md`, sections 3 to 5), merged into the main router with
//! one line in [`crate::api::router`], like [`crate::knowledge_routes`].
//!
//! Reading needs the `read` scope; deciding and consolidating the `run`
//! scope, and the team's memory a little more: approving, rejecting,
//! editing or forgetting a `Repo` row needs `admin` or one of the server's
//! `memory.approvers` (a principal name or `group:<name>`), because a row
//! a consolidation proposed has no pull request to review it. A `Person`
//! row is its subject's alone: another caller's read of it is refused, an
//! administrator's read shows that it exists, its scope and its size, not
//! its text. The caller's name is who decided. `?scope=repo` (the default)
//! names the caller's tenant's memory of the repository, `?scope=mine` the
//! caller's own.
//!
//! A person's approved memory is written to their skills folder, which a
//! server does not have until the registry publishes it (M4): approving a
//! `Person` row here is refused with what to run locally. A `Repo` row's
//! approval writes the bundle into the served repository's `.agents/skills/`,
//! for someone to commit, as `by memory approve` does locally.

use axum::extract::{Path, RawQuery, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use branchyard::{
    Consolidated, MemoryRecord, MemoryRecordEdit, MemoryScope, MemorySettings, MemoryStatus, Yard,
};
use branchyard_client::memory_api::{
    ConsolidateRequest, MemoryDecisionRequest, MemoryEditRequest, MemoryEvidence, MemoryList,
    MemoryPromoteRequest, MemoryRuns, SCOPE_MINE, SCOPE_REPO,
};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/memory", get(list_memory))
        .route("/v1/repos/{repo}/memory/runs", get(memory_runs))
        .route("/v1/repos/{repo}/memory/consolidate", post(consolidate))
        .route(
            "/v1/repos/{repo}/memory/{id}",
            get(memory_record).delete(forget_memory),
        )
        .route(
            "/v1/repos/{repo}/memory/{id}/evidence",
            get(memory_evidence),
        )
        .route("/v1/repos/{repo}/memory/{id}/approve", post(approve_memory))
        .route("/v1/repos/{repo}/memory/{id}/reject", post(reject_memory))
        .route("/v1/repos/{repo}/memory/{id}/edit", post(edit_memory))
        .route("/v1/repos/{repo}/memory/{id}/promote", post(promote_memory))
}

fn row_id(id: &str) -> Result<u64, ApiError> {
    id.trim_start_matches('#')
        .parse()
        .map_err(|_| ApiError::bad_request(format!("{id:?} is not a memory row number")))
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

/// The query's `scope` and `status`.
struct Query {
    mine: bool,
    status: Option<MemoryStatus>,
}

fn parse_query(query: Option<&str>) -> Result<Query, ApiError> {
    let mut parsed = Query {
        mine: false,
        status: None,
    };
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("scope", SCOPE_REPO)) => parsed.mine = false,
            Some(("scope", SCOPE_MINE)) => parsed.mine = true,
            Some(("scope", other)) => {
                return Err(ApiError::bad_request(format!(
                    "scope {other:?} is not {SCOPE_REPO} or {SCOPE_MINE}"
                )))
            }
            Some(("status", value)) => {
                parsed.status = Some(
                    value
                        .parse::<MemoryStatus>()
                        .map_err(|e| ApiError::bad_request(e.to_string()))?,
                )
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// The caller's scope of `yard`: the team's on their tenant, or their own.
fn scope_of(caller: &Caller, yard: &Yard, mine: bool) -> MemoryScope {
    match mine {
        true => yard.person_memory_scope(caller.tenant(), caller.name()),
        false => yard.memory_scope(caller.tenant()),
    }
}

/// Whether `caller` may decide the team's memory: `admin`, or one of the
/// configured approvers, by name or group.
fn may_decide_repo(caller: &Caller, settings: &MemorySettings) -> bool {
    let principal = &caller.0;
    principal.allows("admin")
        || settings
            .approvers
            .iter()
            .any(|approver| match approver.strip_prefix("group:") {
                Some(group) => principal.groups.contains(group),
                None => approver == &principal.name,
            })
}

/// `record` as `caller` may see it: a row of another tenant does not
/// exist for them; a person's row is theirs alone, and an administrator
/// sees that it is there, whose, and how long, not what it says.
fn visible(caller: &Caller, record: MemoryRecord) -> Result<MemoryRecord, ApiError> {
    if record.scope.tenant() != caller.tenant() {
        return Err(error::sdk(&branchyard::Error::UnknownMemory(format!(
            "#{}",
            record.id
        ))));
    }
    match &record.scope {
        MemoryScope::Person { subject, .. } if subject != caller.name() => {
            if !caller.0.allows("admin") {
                return Err(error::sdk(&branchyard::Error::Denied(format!(
                    "memory #{} is {subject}'s; only they read it",
                    record.id
                ))));
            }
            let chars = record.body.chars().count();
            Ok(MemoryRecord {
                title: format!("[private: {} characters]", record.title.chars().count()),
                body: format!("[private: {chars} characters]"),
                note: None,
                ..record
            })
        }
        _ => Ok(record),
    }
}

/// The checks before a decision on `id` as `caller`: the row is theirs to
/// decide. A `Person` row's subject rule is the engine's.
fn decidable(caller: &Caller, yard: &Yard, id: u64) -> Result<MemoryRecord, ApiError> {
    let record = sdk(yard.memory_record(id))?;
    let record = visible(caller, record)?;
    if matches!(record.scope, MemoryScope::Repo { .. })
        && !may_decide_repo(caller, &yard.memory_settings())
    {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "denied",
            "deciding the team's memory needs the admin scope or a name in the server's \
             memory.approvers",
        ));
    }
    Ok(record)
}

async fn list_memory(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<MemoryList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let query = parse_query(query.as_deref())?;
    let filter = branchyard::MemoryFilter {
        scope: Some(scope_of(&caller, &yard, query.mine)),
        status: query.status,
        ..branchyard::MemoryFilter::default()
    };
    let memories = sdk(blocking(move || yard.memory_records(&filter)).await?)?;
    Ok(Json(MemoryList { memories }))
}

async fn memory_record(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<MemoryRecord>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let id = row_id(&id)?;
    let record = sdk(blocking(move || yard.memory_record(id)).await?)?;
    Ok(Json(visible(&caller, record)?))
}

async fn memory_evidence(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<MemoryEvidence>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let id = row_id(&id)?;
    let (record, observations) = sdk(blocking(move || {
        Ok::<_, branchyard::Error>((yard.memory_record(id)?, yard.memory_evidence(id)?))
    })
    .await?)?;
    let memory = visible(&caller, record)?;
    // A redacted row shows no evidence text either.
    let observations = match memory.title.starts_with("[private: ") {
        true => Vec::new(),
        false => observations,
    };
    Ok(Json(MemoryEvidence {
        memory,
        observations,
    }))
}

async fn approve_memory(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<MemoryDecisionRequest>,
) -> Result<Json<MemoryRecord>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = row_id(&id)?;
    let record = decidable(&caller, &yard, id)?;
    if matches!(record.scope, MemoryScope::Person { .. }) {
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "unsupported",
            "a person's memory is written to their own skills folder, which this server does \
             not have until the registry publishes it; run by memory approve --mine locally",
        ));
    }
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.approve_memory(id, &by, None).map(|a| a.record)
    })
    .await?)?))
}

async fn reject_memory(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<MemoryDecisionRequest>,
) -> Result<Json<MemoryRecord>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = row_id(&id)?;
    decidable(&caller, &yard, id)?;
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.reject_memory(id, &by, request.reason.as_deref())
    })
    .await?)?))
}

async fn edit_memory(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<MemoryEditRequest>,
) -> Result<Json<MemoryRecord>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = row_id(&id)?;
    decidable(&caller, &yard, id)?;
    let by = caller.name().to_owned();
    let edit = MemoryRecordEdit {
        title: request.title,
        body: request.body,
        kind: request.kind,
        paths: request.paths,
        task_kinds: request.task_kinds,
    };
    Ok(Json(sdk(blocking(move || {
        yard.edit_memory_record(id, &edit, &by, None)
    })
    .await?)?))
}

async fn promote_memory(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<MemoryPromoteRequest>,
) -> Result<Json<MemoryRecord>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = row_id(&id)?;
    // A promotion proposes: the subject rule is the engine's, and the
    // team's approvers decide the new row.
    let record = sdk(yard.memory_record(id))?;
    visible(&caller, record)?;
    let by = caller.name().to_owned();
    let team = scope_of(&caller, &yard, false);
    Ok(Json(sdk(blocking(move || {
        yard.promote_memory(id, &by, team)
    })
    .await?)?))
}

async fn forget_memory(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<MemoryRecord>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = row_id(&id)?;
    decidable(&caller, &yard, id)?;
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.forget_memory(id, &by, None)
    })
    .await?)?))
}

async fn consolidate(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<ConsolidateRequest>,
) -> Result<Json<Consolidated>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let mine = match request.scope.as_deref() {
        None | Some(SCOPE_REPO) => false,
        Some(SCOPE_MINE) => true,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "scope {other:?} is not {SCOPE_REPO} or {SCOPE_MINE}"
            )))
        }
    };
    let scope = scope_of(&caller, &yard, mine);
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.consolidate_memory(&scope, &by, "asked")
    })
    .await?)?))
}

async fn memory_runs(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<MemoryRuns>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let query = parse_query(query.as_deref())?;
    let scope = scope_of(&caller, &yard, query.mine);
    let runs = sdk(blocking(move || yard.memory_runs(Some(&scope), 100)).await?)?;
    Ok(Json(MemoryRuns { runs }))
}
