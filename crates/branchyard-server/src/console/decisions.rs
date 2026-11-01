//! What waits for a person in a project's workspaces: `GET
//! /v1/projects/{project}/decisions`, the [`DecisionList`] a repository's
//! `GET /v1/repos/{repo}/decisions` answers for branches.
//!
//! A knowledge workspace delivers a document, not a diff, so its decisions
//! are an `output` waiting to be accepted (delivery waits on the person) and
//! a `session` that stopped (nobody waits on it). A session whose plan waits
//! for approval parks its agent and comes first. The rules are the ones the
//! workspace page and `finalize` already apply: an output counts while its
//! session's current turn finished with a valid deliverable result, nothing
//! more is queued for the session and no acceptance names the output.
use super::*;
use crate::api::Shared;
use crate::console_store::Collection;
use axum::{
    extract::{Path, State},
    Extension, Json,
};
use branchyard::{BranchStatus, ResultContract};
use branchyard_client::effects_api::{Decision, DecisionKind, DecisionList, DecisionOutput, Waits};
use std::collections::BTreeSet;

pub(super) async fn decisions(
    State(app): State<Shared>,
    Path(project): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<DecisionList>, ApiError> {
    let row = super::get(&app, Collection::Projects, &project)?;
    super::routes::project_access(&app, &caller, &row, "read")?;
    let decisions = crate::api::blocking(move || decide(&app, &caller, &row)).await??;
    Ok(Json(DecisionList { decisions }))
}

/// The decisions of every ready workspace of `project` the caller may read,
/// in the order to take them: a parked plan first, then outputs oldest
/// first, then the stopped sessions oldest first.
fn decide(app: &App, caller: &Caller, project: &Record) -> Result<Vec<Decision>, ApiError> {
    // A repository project's stopped sessions are branches of its
    // repository, which that repository's own decisions list as `stop`.
    let knowledge = project.body["source"]["kind"] == "knowledge";
    let mut parked = Vec::new();
    let mut outputs = Vec::new();
    let mut stops = Vec::new();
    let pending = app.registry.unfinished(caller.tenant())?;
    let rows = app
        .registry
        .console_list(Collection::Workspaces, caller.tenant())?;
    for workspace in rows
        .iter()
        .filter(|w| w.body["project_id"] == project.id && w.body["state"] == "ready")
    {
        match auth::authorize_workspace(app, caller, &workspace.id, "read") {
            Ok(_) => {}
            Err(error) if error.status.is_server_error() => return Err(error),
            Err(_) => continue,
        }
        let sessions = super::runtime::session_views(app, caller, workspace)?;
        if sessions.is_empty() {
            continue;
        }
        let repo = super::runtime::resolve(app, workspace)?;
        let accepted: BTreeSet<String> =
            related(app, Collection::Acceptance, caller, &workspace.id)?
                .iter()
                .filter_map(|r| r.body["artifact_id"].as_str().map(str::to_owned))
                .collect();
        let title = workspace.body["title"].as_str().unwrap_or_default();
        let base = |view: &Value, status: BranchStatus, kind, waits| Decision {
            kind,
            branch: view["branch"].as_str().unwrap_or_default().to_owned(),
            waits,
            since_ms: None,
            status,
            candidate: None,
            approval: None,
            question: None,
            workspace: Some(workspace.id.clone()),
            workspace_title: Some(title.to_owned()),
            session: view["id"].as_str().map(str::to_owned),
            session_name: view["name"].as_str().map(str::to_owned),
            phase: None,
            output: None,
        };
        let created = |view: &Value| view["created_at_ms"].as_u64().unwrap_or(0);
        let newest = sessions.iter().map(created).max().unwrap_or(0);
        for view in &sessions {
            if knowledge {
                if let Some((status, waits, phase, moves_on)) = stopped(view) {
                    // A person who started another session has moved on
                    // from one that failed: Try again starts a new one and
                    // leaves the failed one in the list.
                    if !moves_on || created(view) >= newest {
                        let mut d = base(view, status, DecisionKind::Session, waits);
                        d.phase = Some(phase);
                        let entry = (created(view), d.session.clone(), d);
                        if waits == Waits::Agent {
                            parked.push(entry);
                        } else {
                            stops.push(entry);
                        }
                    }
                }
            }
        }
        let by_branch = |branch: &str| {
            sessions
                .iter()
                .find(|view| view["branch"].as_str() == Some(branch))
        };
        let mut seen = BTreeSet::new();
        for view in &sessions {
            let reader = text(view, "branch")?;
            for artifact in sources::native_artifacts(&repo, reader)? {
                if !seen.insert(artifact.id.clone()) || accepted.contains(&artifact.id) {
                    continue;
                }
                let Some(producer) = by_branch(&artifact.publisher_branch) else {
                    continue;
                };
                if !delivered(producer, artifact.turn)
                    || pending.iter().any(|op| {
                        op.operation.repo == repo.name
                            && op.operation.branches.contains(&artifact.publisher_branch)
                    })
                    || !sources::branch_owned(
                        app,
                        caller,
                        workspace,
                        &repo,
                        &artifact.publisher_branch,
                    )?
                {
                    continue;
                }
                let status = serde_json::from_value(producer["status_detail"].clone())
                    .unwrap_or(BranchStatus::Ready);
                let mut d = base(producer, status, DecisionKind::Output, Waits::Delivery);
                let at = artifact.created_at.saturating_mul(1000);
                d.since_ms = Some(at);
                d.output = Some(DecisionOutput {
                    id: artifact.id.clone(),
                    name: artifact.name.clone(),
                    media_type: artifact.media_type.clone(),
                    size: artifact.size,
                    turn: artifact.turn,
                });
                outputs.push((at, Some(artifact.id), d));
            }
        }
    }
    for list in [&mut parked, &mut outputs, &mut stops] {
        list.sort_by(|x, y| (x.0, &x.1).cmp(&(y.0, &y.1)));
    }
    Ok(parked
        .into_iter()
        .chain(outputs)
        .chain(stops)
        .map(|(_, _, d)| d)
        .collect())
}

/// Whether `session`'s turn `turn` is its current one and ended with a valid
/// deliverable result: what `finalize` requires before it accepts an output.
fn delivered(session: &Value, turn: u64) -> bool {
    let finished = matches!(
        session["status_detail"]["state"].as_str(),
        Some("ready" | "no_changes" | "merged")
    );
    let result = &session["result"];
    finished
        && session["turns"].as_u64() == Some(turn)
        && !session["result_event"].is_null()
        && session["result_error"].is_null()
        && !result.is_null()
        && serde_json::from_value::<ResultContract>(session["result_contract"].clone())
            .is_ok_and(|c| c.is_deliverable() && c.validate(result).is_ok())
}

/// A session that needs a person: its status, who waits on it, the words
/// the workspace page uses for it, and whether starting another session
/// settles it. `None` for a session that runs, finished or has not begun.
fn stopped(session: &Value) -> Option<(BranchStatus, Waits, String, bool)> {
    let with = |label: &str, detail: Option<&str>| match detail {
        Some(detail) if !detail.is_empty() => format!("{label}: {detail}"),
        _ => label.to_owned(),
    };
    if session["status_detail"].is_null() {
        // No branch yet: the operation that should have started it did not.
        let message = session["error"]["message"].as_str();
        return match session["status"].as_str() {
            Some("failed") => Some((
                BranchStatus::Failed {
                    reason: message.unwrap_or("it did not start").to_owned(),
                },
                Waits::Nobody,
                with("Did not start", message),
                true,
            )),
            Some("interrupted") => Some((
                BranchStatus::Interrupted,
                Waits::Nobody,
                with("Did not start", message),
                true,
            )),
            _ => None,
        };
    }
    let status: BranchStatus = serde_json::from_value(session["status_detail"].clone()).ok()?;
    match &status {
        BranchStatus::Failed { reason } => {
            let phase = with("Failed", Some(reason));
            Some((status, Waits::Nobody, phase, true))
        }
        BranchStatus::Blocked { reason } => {
            let phase = with("Blocked", Some(reason));
            Some((status, Waits::Nobody, phase, true))
        }
        BranchStatus::BudgetExceeded { limit } => {
            let phase = with("Over its budget", Some(limit));
            Some((status, Waits::Nobody, phase, true))
        }
        BranchStatus::Interrupted => Some((status, Waits::Nobody, "Interrupted".to_owned(), false)),
        BranchStatus::AwaitingPlanApproval => Some((
            status,
            Waits::Agent,
            "Its plan waits for approval".to_owned(),
            false,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_that_stopped_says_why_and_a_running_one_is_not_a_decision() {
        let failed =
            json!({"status":"failed","status_detail":{"state":"failed","reason":"harness exited"}});
        let (_, waits, phase, moves_on) = stopped(&failed).expect("a failed session");
        assert_eq!(
            (waits, phase.as_str(), moves_on),
            (Waits::Nobody, "Failed: harness exited", true)
        );
        let never = json!({"status":"failed","error":{"message":"no such harness"}});
        let (status, _, phase, _) = stopped(&never).expect("a session that did not start");
        assert_eq!(phase, "Did not start: no such harness");
        assert!(matches!(status, BranchStatus::Failed { .. }));
        let (status, waits, phase, moves_on) =
            stopped(&json!({"status_detail":{"state":"interrupted"}})).expect("interrupted");
        assert_eq!(
            (status, waits, phase.as_str(), moves_on),
            (
                BranchStatus::Interrupted,
                Waits::Nobody,
                "Interrupted",
                false
            )
        );
        let (_, waits, _, _) =
            stopped(&json!({"status_detail":{"state":"awaiting_plan_approval"}}))
                .expect("a parked plan");
        assert_eq!(waits, Waits::Agent);
        for running in [
            json!({"status":"running"}),
            json!({"status":"succeeded"}),
            json!({"status_detail":{"state":"running"}}),
            json!({"status_detail":{"state":"ready"}}),
        ] {
            assert!(stopped(&running).is_none(), "{running}");
        }
    }

    #[test]
    fn an_output_needs_its_sessions_current_finished_result() {
        let contract = json!({"purpose":"deliverable","schema":{"type":"object"}});
        let mut session = json!({
            "status_detail":{"state":"ready"}, "turns":2, "result_event":5, "result_error":null,
            "result":{"summary":"x"}, "result_contract":contract,
        });
        assert!(delivered(&session, 2));
        assert!(!delivered(&session, 1), "an earlier turn's output");
        session["status_detail"] = json!({"state":"running"});
        assert!(!delivered(&session, 2), "a turn still running");
        session["status_detail"] = json!({"state":"ready"});
        session["result_error"] = json!("did not match");
        assert!(!delivered(&session, 2), "a result that failed its contract");
    }
}
