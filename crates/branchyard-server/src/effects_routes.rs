//! HTTP routes for [approvals and the effect ledger](../../../docs/effects.md),
//! merged into the main router with one line in [`crate::api::router`],
//! like [`crate::knowledge_routes`].
//!
//! Reading the ledger and the waiting asks needs the `read` scope;
//! answering an ask, promoting a staged effect, reconciling and undoing
//! need `run`, and are recorded as the caller's. None locks a branch: an
//! answer is one store write the waiting turn sees, and a promotion or an
//! inverse is one gateway call recorded on its entry.

use std::collections::BTreeMap;

use axum::extract::{Path, RawQuery, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use branchyard::effects::reconcile::Reconciled;
use branchyard::effects::undo::UndoPlan;
use branchyard::effects::{ApprovalAsk, EffectEntry};
use branchyard::stage::Reason;
use branchyard::{BranchStatus, Yard};
use branchyard_client::effects_api::{
    ApprovalAnswerRequest, ApprovalList, Decision, DecisionKind, DecisionList, EffectActionRequest,
    EffectDetail, EffectList, Ground, GroundBranch, GroundFile, UndoReport, UndoRequest, Waits,
};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/decisions", get(decisions))
        .route("/v1/repos/{repo}/ground", get(ground))
        .route("/v1/repos/{repo}/approvals", get(approvals))
        .route("/v1/repos/{repo}/approvals/{id}/allow", post(allow))
        .route("/v1/repos/{repo}/approvals/{id}/deny", post(deny))
        .route("/v1/repos/{repo}/effects", get(effects))
        .route("/v1/repos/{repo}/effects/reconcile", post(reconcile))
        .route("/v1/repos/{repo}/effects/{id}", get(effect))
        .route("/v1/repos/{repo}/effects/{id}/promote", post(promote))
        .route(
            "/v1/repos/{repo}/branches/{branch}/undo",
            get(undo_plan).post(undo),
        )
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

/// `key=value` pairs of a query.
fn query(raw: Option<&str>, key: &str) -> Option<String> {
    raw.unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .map(branchyard_client::http::decode_form)
}

/// `GET /v1/repos/{repo}/decisions`: what waits for a person, in the order
/// to take it ([`DecisionList`]).
async fn decisions(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<DecisionList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let decisions = sdk(blocking(move || decide(&yard)).await?)?;
    Ok(Json(DecisionList { decisions }))
}

/// The decisions of `yard`'s branches, one for each branch whose stage
/// puts it in Needs you or Ready ([`decision_of`]): those that keep an
/// agent parked first, oldest wait first, a plan after the waits that have
/// a time; then those delivery waits on; then the rest, each oldest branch
/// first.
fn decide(yard: &Yard) -> Result<Vec<Decision>, branchyard::Error> {
    let mut asking = yard.asking()?;
    let mut decisions = Vec::new();
    for b in yard.branches()? {
        let Some((kind, waits)) = b.stage.and_then(|s| decision_of(s.reason, &b.status)) else {
            continue;
        };
        let mut d = Decision {
            kind,
            branch: b.name.clone(),
            waits,
            since_ms: None,
            status: b.status.clone(),
            candidate: b.candidate.clone(),
            approval: None,
            question: None,
            workspace: None,
            workspace_title: None,
            session: None,
            session_name: None,
            phase: None,
            output: None,
        };
        match (kind, &b.awaiting) {
            (DecisionKind::Approval, Some(awaiting)) => {
                d.since_ms = Some(awaiting.since_ms);
                d.approval = Some(yard.approval(&awaiting.id)?);
            }
            (DecisionKind::Question, _) => {
                // Asked after `asking` was read: the next read lists it.
                let Some(message) = asking.remove(&b.name) else {
                    continue;
                };
                d.since_ms = Some(message.at_ms);
                d.question = Some(message);
            }
            _ => {}
        }
        decisions.push((b.created_at, d));
    }
    decisions.sort_by_key(|(created, d)| {
        let rank = match d.waits {
            Waits::Agent => 0,
            Waits::Delivery => 1,
            Waits::Nobody => 2,
        };
        (rank, d.since_ms.unwrap_or(u64::MAX), *created)
    });
    Ok(decisions.into_iter().map(|(_, d)| d).collect())
}

/// The decision a branch whose stage has `reason` and whose status is
/// `status` is, and who it keeps waiting; `None` for a branch in the
/// Running or Done lane.
fn decision_of(reason: Reason, status: &BranchStatus) -> Option<(DecisionKind, Waits)> {
    Some(match reason {
        Reason::Approval => (DecisionKind::Approval, Waits::Agent),
        Reason::Question => match status {
            BranchStatus::Running => (DecisionKind::Question, Waits::Agent),
            _ => (DecisionKind::Question, Waits::Nobody),
        },
        Reason::AwaitingPlanApproval => (DecisionKind::Plan, Waits::Agent),
        Reason::Stalled => (DecisionKind::Stall, Waits::Nobody),
        Reason::Failed | Reason::Interrupted | Reason::BudgetExceeded | Reason::Blocked => {
            (DecisionKind::Stop, Waits::Nobody)
        }
        Reason::Ready => (DecisionKind::Merge, Waits::Delivery),
        Reason::Parent
        | Reason::Running
        | Reason::WaitingOnChildren
        | Reason::Waiting
        | Reason::NoChanges
        | Reason::Merged
        | Reason::Discarded => return None,
    })
}

/// `GET /v1/repos/{repo}/ground`: which files are in whose hands
/// ([`Ground`]). It reads every open branch's diff when asked, one `git
/// diff` per branch, so a call costs what those reads cost; nothing is
/// stored for it.
async fn ground(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Ground>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let ground = sdk(blocking(move || survey(&yard)).await?)?;
    Ok(Json(ground))
}

/// Whether `info` still holds its files: not merged, not discarded, and
/// not one of the goal judge's scratch branches (`judge-…`, removed once
/// it answers).
fn open(info: &branchyard::BranchInfo) -> bool {
    !matches!(
        info.status,
        BranchStatus::Merged { .. } | BranchStatus::Discarded { .. }
    ) && !info.name.starts_with("judge-")
}

/// The ground of `yard`'s open branches, from each one's diff. Only
/// listing the branches can fail the call; a diff that cannot be read
/// skips its branch.
fn survey(yard: &Yard) -> Result<Ground, branchyard::Error> {
    let held = yard.branches()?.into_iter().filter(open).map(|b| {
        let files = yard
            .branch(&b.name)
            .and_then(|branch| branch.diff())
            .map(|diff| branchyard::diff_files(&diff));
        (b.name, b.status, files)
    });
    Ok(tally(held))
}

/// Group each branch's changed paths by path: paths sorted, each path's
/// branches sorted by name, and the branches whose files are an error
/// listed, sorted, in `skipped`.
fn tally<E>(
    held: impl IntoIterator<Item = (String, BranchStatus, Result<Vec<String>, E>)>,
) -> Ground {
    let mut by_path: BTreeMap<String, Vec<GroundBranch>> = BTreeMap::new();
    let mut skipped = Vec::new();
    for (name, status, files) in held {
        let Ok(files) = files else {
            skipped.push(name);
            continue;
        };
        for path in files {
            by_path.entry(path).or_default().push(GroundBranch {
                name: name.clone(),
                status: status.clone(),
            });
        }
    }
    skipped.sort();
    let files = by_path
        .into_iter()
        .map(|(path, mut branches)| {
            branches.sort_by(|a, b| a.name.cmp(&b.name));
            GroundFile { path, branches }
        })
        .collect();
    Ground { files, skipped }
}

async fn approvals(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<ApprovalList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let all = query(raw.as_deref(), "all").is_some_and(|v| v == "true" || v == "1");
    let approvals = sdk(blocking(move || yard.approvals(!all)).await?)?;
    Ok(Json(ApprovalList { approvals }))
}

/// The surface an answer came through: `api`, unless the request says the
/// companion page or `by watch` sent it.
fn surface(request: &ApprovalAnswerRequest) -> Result<String, ApiError> {
    match request.surface.as_deref() {
        None => Ok("api".into()),
        Some(s @ ("api" | "companion" | "watch")) => Ok(s.to_owned()),
        Some(other) => Err(ApiError::bad_request(format!(
            "{other:?} is not a surface; use api, companion or watch"
        ))),
    }
}

async fn answer(
    app: Shared,
    repo: String,
    id: String,
    caller: Caller,
    request: ApprovalAnswerRequest,
    allow: bool,
) -> Result<Json<ApprovalAsk>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let surface = surface(&request)?;
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.answer_approval(&id, allow, &by, &surface, request.reason.as_deref())
    })
    .await?)?))
}

async fn allow(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<ApprovalAnswerRequest>,
) -> Result<Json<ApprovalAsk>, ApiError> {
    answer(app, repo, id, caller, request, true).await
}

async fn deny(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<ApprovalAnswerRequest>,
) -> Result<Json<ApprovalAsk>, ApiError> {
    answer(app, repo, id, caller, request, false).await
}

async fn effects(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<EffectList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let branch = query(raw.as_deref(), "branch");
    let effects = sdk(blocking(move || yard.effects(branch.as_deref())).await?)?;
    Ok(Json(EffectList { effects }))
}

async fn effect(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<EffectDetail>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let detail = sdk(blocking(move || {
        let entry = yard.effect(&id)?;
        let events = yard.effect_history(&entry.id)?;
        Ok(EffectDetail { entry, events })
    })
    .await?)?;
    Ok(Json(detail))
}

async fn promote(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<EffectActionRequest>,
) -> Result<Json<EffectEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.promote_effect(&id, &by, "api")
    })
    .await?)?))
}

async fn reconcile(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<EffectActionRequest>,
) -> Result<Json<Reconciled>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    Ok(Json(
        sdk(blocking(move || yard.reconcile_effects()).await?)?,
    ))
}

async fn undo_plan(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<UndoPlan>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let to = match query(raw.as_deref(), "to") {
        Some(to) => to
            .parse()
            .map_err(|_| ApiError::bad_request(format!("to={to} is not a turn")))?,
        None => 0,
    };
    Ok(Json(sdk(
        blocking(move || yard.undo_plan(&branch, to)).await?
    )?))
}

async fn undo(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<UndoRequest>,
) -> Result<Json<UndoReport>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = caller.name().to_owned();
    let report = sdk(blocking(move || {
        let plan = yard.undo_plan(&branch, request.to)?;
        let chosen = match request.only.is_empty() {
            true => plan.default_choice(),
            false => request.only.clone(),
        };
        let outcomes = yard.undo_effects(&plan, &chosen, &by, "api")?;
        Ok(UndoReport { plan, outcomes })
    })
    .await?)?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::{decision_of, query, tally, DecisionKind, Reason, Waits};
    use branchyard::stage::{Lane, STAGES};
    use branchyard::BranchStatus;

    #[test]
    fn a_branch_is_one_decision_exactly_when_it_needs_you_or_is_ready() {
        for (reason, lane, _) in STAGES {
            assert_eq!(
                decision_of(reason, &BranchStatus::Running).is_some(),
                matches!(lane, Lane::NeedsYou | Lane::Ready),
                "{reason:?}"
            );
        }
        let blocked = BranchStatus::Blocked { reason: "x".into() };
        assert_eq!(
            decision_of(Reason::Blocked, &blocked),
            Some((DecisionKind::Stop, Waits::Nobody))
        );
        assert_eq!(
            decision_of(
                Reason::AwaitingPlanApproval,
                &BranchStatus::AwaitingPlanApproval
            ),
            Some((DecisionKind::Plan, Waits::Agent))
        );
        assert_eq!(
            decision_of(Reason::Question, &BranchStatus::Ready),
            Some((DecisionKind::Question, Waits::Nobody))
        );
    }

    #[test]
    fn query_values_decode_like_every_other_call_site() {
        assert_eq!(
            query(Some("a=1&k=x+y%2Bz%41"), "k").as_deref(),
            Some("x y+zA")
        );
        assert_eq!(query(Some("k=%zz%4"), "k").as_deref(), Some("%zz%4"));
        assert_eq!(query(None, "k"), None);
    }

    #[test]
    fn ground_groups_paths_and_skips_a_branch_whose_diff_failed() {
        let files =
            |paths: &[&str]| Ok::<_, String>(paths.iter().map(|p| (*p).to_owned()).collect());
        let ground = tally([
            (
                "lib".to_owned(),
                BranchStatus::Running,
                files(&["shared.txt", "b.txt"]),
            ),
            (
                "app".to_owned(),
                BranchStatus::Ready,
                files(&["shared.txt"]),
            ),
            (
                "gone".to_owned(),
                BranchStatus::Ready,
                Err("no such commit".to_owned()),
            ),
            ("empty".to_owned(), BranchStatus::Waiting, files(&[])),
        ]);
        let shape: Vec<(&str, Vec<&str>)> = ground
            .files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    f.branches.iter().map(|b| b.name.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [("b.txt", vec!["lib"]), ("shared.txt", vec!["app", "lib"])]
        );
        assert_eq!(ground.files[1].branches[0].status, BranchStatus::Ready);
        assert_eq!(ground.skipped, ["gone"]);
    }
}
