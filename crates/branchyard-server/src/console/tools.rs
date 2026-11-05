//! One API for connectors, skills and helpers.
//!
//! Every tool passes through the same states: available (the server has
//! it), allowed (policy, [`super::policy`]), ready (set up for this person),
//! on by default (the workspace's choice) and chosen (this session's
//! `tools`). The server computes them for the caller; admission checks
//! again. See `docs/tools-model.md`.
//!
//! - `GET /v1/tools?workspace=ID`: every tool, with where it stands for the
//!   caller. `workspace` is optional and adds the workspace's defaults.
//! - `PUT /v1/workspaces/{workspace}/tools`: the workspace's defaults,
//!   guarded by a revision, for its owner or an editor. (`GET` on the same
//!   path is in [`super::connections`] and returns them as `defaults`.)
//! - A session start's `tools` field, which [`expand`] turns into the
//!   `connectors`, `skills` and `helpers` fields the start already takes.
//!
//! A built-in skill is ready when allowed: installing skills per person
//! comes later. A helper is ready when an environment offered to the tenant
//! can run its harness; signing in per person comes later.

use std::collections::BTreeSet;

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Extension, Json, Router};
use branchyard::connectors::{GrantEntry, GrantMode};
use branchyard_client::workspace_api::{
    ConnectionState, GatewayState, PutToolDefaults, ToolAction, ToolCatalogue, ToolChoice,
    ToolDefaults, ToolEntry, ToolKind, ToolPolicy, ToolState,
};
use branchyard_provision::built_in::{self, Catalogue};
use serde_json::{json, Value};

use super::{auth, connections, policy, presets};
use crate::api::{App, Caller, Shared};
use crate::console_store::Collection;
use crate::error::ApiError;

/// The most tools a workspace default or a session start lists.
const MAX_CHOICES: usize = 256;

pub(crate) fn router() -> Router<Shared> {
    Router::new().route("/v1/tools", get(catalogue))
}

async fn catalogue(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<ToolCatalogue>, ApiError> {
    super::scope(&caller, "read")?;
    let workspace = super::routes::query(raw).remove("workspace");
    crate::api::blocking(move || build(&app, &caller, workspace.as_deref()))
        .await?
        .map(Json)
}

// ---------------------------------------------------------------------------
// The catalogue.
// ---------------------------------------------------------------------------

/// Whether `policy` lets a session choose skill `id`. A skill brings the
/// skills it needs whether or not the policy lists them: a built-in skill is
/// guidance, not access.
pub(super) fn skill_usable(catalogue: &Catalogue, policy: &ToolPolicy, id: &str) -> bool {
    policy::skill_allowed(policy, id) && catalogue.skills.iter().any(|s| s.id == id)
}

/// Why helpers are off for the caller, or `None` when they are on.
pub(super) fn helper_state(resolved: &policy::Resolved) -> Option<String> {
    if !resolved.allow_delegation {
        return Some("Helpers are turned off on this server.".to_owned());
    }
    (!policy::delegation_allowed(&resolved.store))
        .then(|| "Your administrator has not turned helpers on for you.".to_owned())
}

struct Facts {
    allowed: bool,
    ready: bool,
    default: bool,
}

impl Facts {
    fn state(&self) -> ToolState {
        match (self.allowed, self.ready, self.default) {
            (false, _, _) => ToolState::NotAllowed,
            (true, false, _) => ToolState::NotReady,
            (true, true, false) => ToolState::Ready,
            (true, true, true) => ToolState::Default,
        }
    }
}

fn entry(kind: ToolKind, id: &str, name: String, facts: &Facts) -> ToolEntry {
    ToolEntry {
        kind,
        id: id.to_owned(),
        name,
        description: None,
        icon: None,
        topic: None,
        state: facts.state(),
        allowed: facts.allowed,
        ready: facts.ready,
        default: facts.default,
        max_mode: None,
        accounts: Vec::new(),
        action: None,
        reason: None,
    }
}

fn build(app: &App, caller: &Caller, workspace: Option<&str>) -> Result<ToolCatalogue, ApiError> {
    let defaults = match workspace {
        Some(id) => {
            auth::authorize_workspace(app, caller, id, "read")?;
            defaults(app, caller, id)?
        }
        None => ToolDefaults::default(),
    };
    let on = |kind: ToolKind, id: &str| defaults.tools.iter().any(|t| t.kind == kind && t.id == id);
    let resolved = policy::resolved(app, caller)?;
    let effective = resolved.effective();
    let list = connections::connections(app, caller, &resolved);
    let mut tools = Vec::new();
    for c in &list.connections {
        let allowed = c.max_mode.is_some();
        let facts = Facts {
            allowed,
            ready: allowed && c.state == ConnectionState::Connected,
            default: on(ToolKind::Connector, &c.connector),
        };
        let mut tool = entry(ToolKind::Connector, &c.connector, c.name.clone(), &facts);
        tool.description = c.description.clone();
        tool.icon = c.icon.clone();
        tool.max_mode = c.max_mode;
        tool.accounts = c.accounts.clone();
        tool.reason = c.reason.clone();
        tool.action = match (allowed, c.state) {
            (false, _) => Some(ToolAction::AskAdmin),
            (_, ConnectionState::NotConnected | ConnectionState::NeedsReconnect) => {
                Some(ToolAction::Connect)
            }
            _ => None,
        };
        tools.push(tool);
    }
    let skills = built_in::catalogue().map_err(ApiError::internal)?;
    for skill in &skills.skills {
        let allowed = skill_usable(&skills, &effective, &skill.id);
        let facts = Facts {
            allowed,
            ready: allowed,
            default: on(ToolKind::Skill, &skill.id),
        };
        let mut tool = entry(ToolKind::Skill, &skill.id, skill.id.clone(), &facts);
        tool.description = Some(skill.description.clone());
        tool.topic = skills
            .topics
            .iter()
            .find(|t| t.id == skill.topic)
            .map(|t| t.title.clone());
        if !allowed {
            tool.action = Some(ToolAction::AskAdmin);
            tool.reason = Some(format!("Your administrator has not allowed {}.", skill.id));
        }
        tools.push(tool);
    }
    let off = helper_state(&resolved);
    for helper in presets::helper_harnesses(app, caller.tenant())? {
        let allowed = off.is_none() && policy::helper_allowed(&effective, &helper.id);
        let facts = Facts {
            allowed,
            ready: allowed && helper.ready,
            default: on(ToolKind::Helper, &helper.id),
        };
        let mut tool = entry(ToolKind::Helper, &helper.id, helper.id.clone(), &facts);
        if !allowed {
            tool.action = Some(ToolAction::AskAdmin);
            tool.reason = Some(off.clone().unwrap_or_else(|| {
                format!(
                    "Your administrator has not allowed {} as a helper.",
                    helper.id
                )
            }));
        } else if !helper.ready {
            tool.action = helper.install.then_some(ToolAction::Install);
            tool.reason = helper.reason;
        }
        tools.push(tool);
    }
    Ok(ToolCatalogue {
        gateway: list.gateway,
        reason: match list.gateway {
            GatewayState::Ready => None,
            _ => list.reason,
        },
        max_children: connections::MAX_HELPERS,
        tools,
    })
}

// ---------------------------------------------------------------------------
// Workspace defaults.
// ---------------------------------------------------------------------------

/// The tools `workspace`'s sessions start with, and the revision to replace.
pub(super) fn defaults(
    app: &App,
    caller: &Caller,
    workspace: &str,
) -> Result<ToolDefaults, ApiError> {
    let Some(record) = app
        .registry
        .console_get(Collection::ToolDefaults, workspace)?
    else {
        return Ok(ToolDefaults::default());
    };
    if record.tenant != caller.tenant() {
        return Err(ApiError::internal("tool defaults belong to another tenant"));
    }
    let tools = serde_json::from_value(record.body["tools"].clone())
        .map_err(|e| ApiError::internal(format!("stored tool defaults are unreadable: {e}")))?;
    Ok(ToolDefaults {
        tools,
        revision: record.revision,
    })
}

/// `PUT /v1/workspaces/{workspace}/tools`.
pub(super) async fn put_defaults(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
    crate::api::JsonBody(request, _): crate::api::JsonBody<PutToolDefaults>,
) -> Result<Json<ToolDefaults>, ApiError> {
    crate::api::blocking(move || {
        auth::authorize_workspace(&app, &caller, &wid, "run")?;
        validate(&request.tools)?;
        let body = json!({ "workspace_id": wid, "tools": request.tools });
        let (mutation, revision) = match app.registry.console_get(Collection::ToolDefaults, &wid)? {
            Some(current) => {
                if current.tenant != caller.tenant() || request.revision != current.revision {
                    return Err(super::revision_conflict());
                }
                let revision = current.revision.saturating_add(1);
                (super::update(current, body), revision)
            }
            None => {
                if request.revision != 0 {
                    return Err(super::revision_conflict());
                }
                (
                    super::insert(super::new(Collection::ToolDefaults, &wid, &caller, body)),
                    1,
                )
            }
        };
        app.registry.console_mutate(&[mutation])?;
        Ok(Json(ToolDefaults {
            tools: request.tools,
            revision,
        }))
    })
    .await?
}

/// Refuse a harness id this server has no profile for.
pub(super) fn known_harnesses(ids: &[String]) -> Result<(), ApiError> {
    match ids.iter().find(|h| {
        branchyard_harness::profiles::by_id(h).is_none()
            && branchyard_harness::profiles::default_for(h).is_none()
    }) {
        Some(unknown) => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "harness_unavailable",
            format!("{unknown} is not a harness this server knows"),
        )
        .detail(json!({ "harness": unknown }))),
        None => Ok(()),
    }
}

/// Refuse a list of choices that is too long, repeats a tool, gives a skill
/// or helper a mode or account, names an invalid connector, or names a
/// skill or harness that does not exist. Policy and readiness are not
/// checked: those are admission's.
pub(super) fn validate(choices: &[ToolChoice]) -> Result<(), ApiError> {
    if choices.len() > MAX_CHOICES {
        return Err(ApiError::bad_request(format!(
            "choose at most {MAX_CHOICES} tools"
        )));
    }
    let mut seen = BTreeSet::new();
    for choice in choices {
        if choice.id.trim().is_empty() || choice.id.len() > 128 {
            return Err(ApiError::bad_request("a tool id is 1 to 128 characters"));
        }
        if !seen.insert((choice.kind, choice.id.as_str())) {
            return Err(ApiError::bad_request(format!(
                "{} is listed twice",
                choice.id
            )));
        }
        match choice.kind {
            ToolKind::Connector => {
                grant(choice).check().map_err(ApiError::bad_request)?;
            }
            ToolKind::Skill | ToolKind::Helper => {
                if choice.mode.is_some() || choice.account.is_some() {
                    return Err(ApiError::bad_request(format!(
                        "{}: mode and account belong to connectors",
                        choice.id
                    )));
                }
            }
        }
    }
    let ids = |kind: ToolKind| -> Vec<String> {
        choices
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| c.id.clone())
            .collect()
    };
    built_in::check(&ids(ToolKind::Skill))
        .map_err(|why| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "skill_unknown", why))?;
    known_harnesses(&ids(ToolKind::Helper))
}

// ---------------------------------------------------------------------------
// Session start.
// ---------------------------------------------------------------------------

fn grant(choice: &ToolChoice) -> GrantEntry {
    GrantEntry {
        mode: choice.mode.unwrap_or(GrantMode::Read),
        account: choice.account.clone(),
        ..GrantEntry::read(&choice.id)
    }
}

/// Turn a start's `tools` into the `connectors`, `skills` and `helpers`
/// fields of its body, and drop `tools`. The list is the whole choice: a
/// kind it leaves out is not chosen, so no `skill` entries stage no
/// built-in skills and no `helper` entries give no helpers. Refused beside
/// any of those three fields. A body without `tools` is untouched.
pub(super) fn expand(body: &mut Value) -> Result<(), ApiError> {
    let Some(object) = body.as_object_mut() else {
        return Ok(());
    };
    let Some(listed) = object.remove("tools").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    // An empty `connectors` is what an unset one serializes as; an empty
    // `skills` is a choice (none).
    let present = |key: &str| match object.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) if key == "connectors" => !items.is_empty(),
        Some(_) => true,
    };
    if present("connectors") || present("skills") || present("helpers") {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "tools_conflict",
            "give tools, or connectors, skills and helpers, not both",
        ));
    }
    let choices: Vec<ToolChoice> = serde_json::from_value(listed)
        .map_err(|e| ApiError::bad_request(format!("invalid tools: {e}")))?;
    validate(&choices)?;
    let ids = |kind: ToolKind| -> Vec<&str> {
        choices
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| c.id.as_str())
            .collect()
    };
    let connectors: Vec<GrantEntry> = choices
        .iter()
        .filter(|c| c.kind == ToolKind::Connector)
        .map(grant)
        .collect();
    object.insert(
        "connectors".into(),
        serde_json::to_value(connectors).map_err(|e| ApiError::internal(e.to_string()))?,
    );
    object.insert("skills".into(), json!(ids(ToolKind::Skill)));
    let helpers = ids(ToolKind::Helper);
    if !helpers.is_empty() {
        object.insert("helpers".into(), json!({ "harnesses": helpers }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(kind: ToolKind, id: &str) -> ToolChoice {
        ToolChoice {
            kind,
            id: id.into(),
            mode: None,
            account: None,
        }
    }

    #[test]
    fn expanding_tools_fills_the_three_fields_and_leaves_other_bodies_alone() {
        let mut untouched = json!({"name":"x","connectors":["github"]});
        expand(&mut untouched).unwrap();
        assert_eq!(untouched, json!({"name":"x","connectors":["github"]}));

        let mut body = json!({"name":"x","tools":[
            {"kind":"connector","id":"github","mode":"write","account":"work"},
            {"kind":"connector","id":"linear"},
            {"kind":"skill","id":"docx"},
            {"kind":"helper","id":"claude-code-acp"}
        ]});
        expand(&mut body).unwrap();
        assert!(body.get("tools").is_none());
        assert_eq!(body["connectors"][0]["connector"], "github");
        assert_eq!(body["connectors"][0]["mode"], "write");
        assert_eq!(body["connectors"][0]["account"], "work");
        assert_eq!(body["connectors"][1]["mode"], "read", "read by default");
        assert_eq!(body["skills"], json!(["docx"]));
        assert_eq!(body["helpers"], json!({"harnesses":["claude-code-acp"]}));

        let mut bare = json!({"tools":[]});
        expand(&mut bare).unwrap();
        assert_eq!(bare["skills"], json!([]), "no skill entries stage none");
        assert_eq!(bare["connectors"], json!([]));
        assert!(bare.get("helpers").is_none());
    }

    #[test]
    fn tools_beside_the_old_fields_are_refused_but_empty_ones_are_not() {
        for old in [
            json!({"connectors":["github"]}),
            json!({"skills":["docx"]}),
            json!({"helpers":{"harnesses":[]}}),
            json!({"skills":[]}),
        ] {
            let mut body = json!({"tools":[]});
            body.as_object_mut()
                .unwrap()
                .extend(old.as_object().unwrap().clone());
            let error = expand(&mut body).unwrap_err();
            assert_eq!(error.body.code, "tools_conflict", "{old}");
        }
        let mut body = json!({"tools":[],"connectors":[],"skills":null,"helpers":null});
        expand(&mut body).unwrap();
    }

    #[test]
    fn choices_are_checked_for_shape_and_existence() {
        let bad = |choices: Vec<ToolChoice>| validate(&choices).unwrap_err().body.code;
        assert_eq!(
            bad(vec![
                choice(ToolKind::Skill, "docx"),
                choice(ToolKind::Skill, "docx")
            ]),
            "invalid_request"
        );
        assert_eq!(
            bad(vec![choice(ToolKind::Skill, "no-such-skill")]),
            "skill_unknown"
        );
        assert_eq!(
            bad(vec![choice(ToolKind::Helper, "no-such-harness")]),
            "harness_unavailable"
        );
        assert_eq!(
            bad(vec![choice(ToolKind::Connector, "no spaces")]),
            "invalid_request"
        );
        let mut mode = choice(ToolKind::Skill, "docx");
        mode.mode = Some(GrantMode::Write);
        assert_eq!(bad(vec![mode]), "invalid_request");
        let mut account = choice(ToolKind::Connector, "github");
        account.account = Some("no spaces".into());
        assert_eq!(bad(vec![account]), "invalid_request");
        // The same id under two kinds is two tools.
        assert!(validate(&[
            choice(ToolKind::Connector, "docx"),
            choice(ToolKind::Skill, "docx")
        ])
        .is_ok());
    }

    #[test]
    fn a_skill_is_usable_when_listed_and_brings_what_it_needs() {
        let catalogue = built_in::catalogue().unwrap();
        let needing = catalogue
            .skills
            .iter()
            .find(|s| !s.requires.is_empty())
            .expect("the catalogue has a skill that requires another");
        let alone = ToolPolicy {
            skills: Some(vec![needing.id.clone()]),
            ..ToolPolicy::default()
        };
        assert!(skill_usable(&catalogue, &alone, &needing.id));
        assert!(
            !skill_usable(&catalogue, &alone, &needing.requires[0]),
            "what it needs comes along but is not chosen"
        );
        assert!(skill_usable(
            &catalogue,
            &ToolPolicy::default(),
            &needing.id
        ));
        let unlisted = ToolPolicy {
            skills: Some(vec![]),
            ..ToolPolicy::default()
        };
        assert!(!skill_usable(&catalogue, &unlisted, &needing.id));
        assert!(!skill_usable(
            &catalogue,
            &ToolPolicy::default(),
            "no-such-skill"
        ));
    }
}
