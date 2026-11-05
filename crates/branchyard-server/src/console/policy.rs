//! Who may use which tools: the policy model.
//!
//! An administrator stores a [`ToolPolicy`] at three levels, in the console
//! store (`Collection::Policies`, so SQLite and PostgreSQL alike):
//!
//! - `tenant:<tenant>`: the tenant's;
//! - `group:<tenant>:<group>`: one group's, for callers whose principal
//!   lists the group;
//! - `principal:<tenant>:<subject>`: one person's, by the stable subject
//!   ([`crate::config::Principal::connector_subject`]).
//!
//! A caller's policy is the tenant's policy narrowed by what their groups
//! and their own record allow together ([`resolve`]), then by the server's
//! own limits ([`with_server`]: the `ceilings` configuration and
//! `allow_delegation`). Every step only narrows. See `docs/server.md`,
//! "Tool policy".
//!
//! - `GET /v1/admin/policies`: the tenant's stored policies.
//! - `PUT /v1/admin/policies/{key}`: create or replace one, guarded by its
//!   revision.
//! - `DELETE /v1/admin/policies/{key}`: remove one.
//!
//! All three need the `admin` scope and act on the caller's own tenant,
//! which no request names.

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Extension, Json, Router};
use branchyard::connectors::{intersect, GrantEntry, GrantMode};
use branchyard_client::workspace_api::{
    PolicyList, PolicyRecord, PutPolicyRequest, ToolKind, ToolPolicy,
};
use serde_json::{json, Value};

use crate::api::{App, Caller, Shared};
use crate::console_store::Collection;
use crate::error::ApiError;

/// Policies one tenant may store, so a list stays a page.
const MAX_POLICIES: usize = 1000;
/// Entries one list in a policy may hold.
const MAX_ENTRIES: usize = 256;

pub(crate) fn router() -> Router<Shared> {
    Router::new().route("/v1/admin/policies", get(list)).route(
        "/v1/admin/policies/{key}",
        get(read).put(put).delete(remove),
    )
}

// ---------------------------------------------------------------------------
// The pure model.
// ---------------------------------------------------------------------------

/// The items of `a` that `b` also holds, in `a`'s order.
fn common(a: &[String], b: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in a {
        if b.contains(item) && !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// Every item of `lists`, once, in first-seen order.
fn pooled<T: Clone + PartialEq>(lists: &[&Vec<T>]) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in lists.iter().flat_map(|list| list.iter()) {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// What both policies allow. A field one of them leaves unset (no limit) is
/// the other's.
pub(crate) fn intersect_policy(a: &ToolPolicy, b: &ToolPolicy) -> ToolPolicy {
    ToolPolicy {
        connectors: match (&a.connectors, &b.connectors) {
            (Some(a), Some(b)) => Some(intersect(a, b)),
            (a, b) => a.clone().or_else(|| b.clone()),
        },
        skills: match (&a.skills, &b.skills) {
            (Some(a), Some(b)) => Some(common(a, b)),
            (a, b) => a.clone().or_else(|| b.clone()),
        },
        helpers: match (&a.helpers, &b.helpers) {
            (Some(a), Some(b)) => Some(common(a, b)),
            (a, b) => a.clone().or_else(|| b.clone()),
        },
        delegation: match (a.delegation, b.delegation) {
            (Some(a), Some(b)) => Some(a && b),
            (a, b) => a.or(b),
        },
    }
}

/// What any of `policies` allows, for the policies that speak to a field:
/// the pooled lists, and delegation when any allows it. A policy that
/// leaves a field unset adds nothing to that field, so naming only
/// `delegation` on a group never lifts a limit another group sets on
/// skills. `None` when there are no policies.
pub(crate) fn union_policy(policies: &[&ToolPolicy]) -> Option<ToolPolicy> {
    if policies.is_empty() {
        return None;
    }
    fn pool<T: Clone + PartialEq>(lists: Vec<&Vec<T>>) -> Option<Vec<T>> {
        (!lists.is_empty()).then(|| pooled(&lists))
    }
    let delegations: Vec<bool> = policies.iter().filter_map(|p| p.delegation).collect();
    Some(ToolPolicy {
        connectors: pool(
            policies
                .iter()
                .filter_map(|p| p.connectors.as_ref())
                .collect(),
        ),
        skills: pool(policies.iter().filter_map(|p| p.skills.as_ref()).collect()),
        helpers: pool(policies.iter().filter_map(|p| p.helpers.as_ref()).collect()),
        delegation: match delegations.is_empty() {
            true => None,
            false => Some(delegations.contains(&true)),
        },
    })
}

/// A caller's stored policy: the tenant's, narrowed by the union of their
/// groups' and their own, when any of those exist.
pub(crate) fn resolve(
    tenant: Option<&ToolPolicy>,
    groups: &[&ToolPolicy],
    principal: Option<&ToolPolicy>,
) -> ToolPolicy {
    let below: Vec<&ToolPolicy> = groups.iter().copied().chain(principal).collect();
    let mut policy = match (tenant, union_policy(&below)) {
        (Some(tenant), Some(below)) => intersect_policy(tenant, &below),
        (Some(only), None) => only.clone(),
        (None, Some(only)) => only,
        (None, None) => ToolPolicy::default(),
    };
    policy.connectors = policy.connectors.map(simplified);
    policy
}

/// `entries` without any that another entry for the same connector,
/// account and operations already covers with at least its mode and
/// confirmation.
fn simplified(entries: Vec<GrantEntry>) -> Vec<GrantEntry> {
    let covered = |a: &GrantEntry, b: &GrantEntry| {
        a.connector == b.connector
            && a.account == b.account
            && a.operations == b.operations
            && a.mode <= b.mode
            && a.confirm <= b.confirm
    };
    let mut kept: Vec<GrantEntry> = Vec::new();
    for entry in entries {
        if kept.iter().any(|k| covered(&entry, k)) {
            continue;
        }
        kept.retain(|k| !covered(k, &entry));
        kept.push(entry);
    }
    kept
}

/// `policy` within the server's own limits: the `ceilings` configuration's
/// connectors for this caller, and whether the server delegates at all.
pub(crate) fn with_server(
    policy: &ToolPolicy,
    ceiling: Option<&[GrantEntry]>,
    allow_delegation: bool,
) -> ToolPolicy {
    let server = ToolPolicy {
        connectors: ceiling.map(<[GrantEntry]>::to_vec),
        delegation: (!allow_delegation).then_some(false),
        ..ToolPolicy::default()
    };
    intersect_policy(policy, &server)
}

/// Whether sessions may delegate under `policy`.
pub(crate) fn delegation_allowed(policy: &ToolPolicy) -> bool {
    policy.delegation != Some(false)
}

/// Whether `policy` lists `id` among its skills (unset lists every one).
pub(crate) fn skill_allowed(policy: &ToolPolicy, id: &str) -> bool {
    policy
        .skills
        .as_ref()
        .is_none_or(|allowed| allowed.iter().any(|s| s == id))
}

/// Whether `policy` allows delegating to `harness`: delegation is on and
/// the harness is listed (unset lists every one).
pub(crate) fn helper_allowed(policy: &ToolPolicy, harness: &str) -> bool {
    delegation_allowed(policy)
        && policy
            .helpers
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|h| h == harness))
}

/// The most `ceiling` allows of `connector`: `write`, `read`, or nothing.
/// No ceiling allows `write`.
pub(crate) fn connector_mode(ceiling: Option<&[GrantEntry]>, connector: &str) -> Option<GrantMode> {
    let Some(ceiling) = ceiling else {
        return Some(GrantMode::Write);
    };
    let asked = GrantEntry {
        mode: GrantMode::Write,
        ..GrantEntry::read(connector)
    };
    intersect(std::slice::from_ref(&asked), ceiling)
        .iter()
        .map(|e| e.mode)
        .max()
}

// ---------------------------------------------------------------------------
// Keys and the store.
// ---------------------------------------------------------------------------

/// The tenant policy's key.
pub(crate) fn tenant_key(tenant: &str) -> String {
    format!("tenant:{tenant}")
}

/// A group policy's key.
pub(crate) fn group_key(tenant: &str, group: &str) -> String {
    format!("group:{tenant}:{group}")
}

/// A person's policy key.
pub(crate) fn principal_key(tenant: &str, subject: &str) -> String {
    format!("principal:{tenant}:{subject}")
}

/// The key of the tenant's policy version: a counter in the same
/// collection, so SQLite and PostgreSQL keep it with the policies it counts.
/// It rises by one in the same transaction as every `PUT` and `DELETE` of
/// any of the tenant's policy records, and is never a policy itself.
pub(crate) fn version_key(tenant: &str) -> String {
    format!("{VERSION_PREFIX}{tenant}")
}

const VERSION_PREFIX: &str = "version:";

/// Whether a stored `Policies` row is the counter, not a policy.
fn is_version(id: &str) -> bool {
    id.starts_with(VERSION_PREFIX)
}

/// The tenant's policy version: 0 until a policy is first stored, then the
/// number of changes made to its policies. A session records it when it is
/// admitted; a later claim compares ([`recheck`]).
pub(crate) fn current_version(app: &App, tenant: &str) -> Result<u64, ApiError> {
    Ok(app
        .registry
        .console_get(Collection::Policies, &version_key(tenant))?
        .and_then(|row| row.body["version"].as_u64())
        .unwrap_or(0))
}

/// The change that raises the tenant's policy version by one, to apply with
/// the policy's own.
fn bump(app: &App, caller: &Caller) -> Result<crate::console_store::Mutation, ApiError> {
    let key = version_key(caller.tenant());
    Ok(
        match app.registry.console_get(Collection::Policies, &key)? {
            Some(current) => {
                let next = current.body["version"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(1);
                super::update(current, json!({ "version": next }))
            }
            None => super::insert(super::new(
                Collection::Policies,
                &key,
                caller,
                json!({ "version": 1 }),
            )),
        },
    )
}

fn invalid_key(why: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "policy_key_invalid", why)
}

/// Refuse a key that is not one of the three forms for `tenant`.
pub(crate) fn check_key(tenant: &str, key: &str) -> Result<(), ApiError> {
    if key == tenant_key(tenant) {
        return Ok(());
    }
    if let Some(group) = key.strip_prefix(&group_key(tenant, "")) {
        return crate::config::check_group_name(group).map_err(|why| invalid_key(&why));
    }
    if let Some(subject) = key.strip_prefix(&principal_key(tenant, "")) {
        if !subject.is_empty() && subject.len() <= 512 && !subject.chars().any(char::is_control) {
            return Ok(());
        }
        return Err(invalid_key(
            "the subject is 1 to 512 characters with no control characters",
        ));
    }
    Err(invalid_key(
        "a policy key is tenant:<tenant>, group:<tenant>:<group> or principal:<tenant>:<subject>, \
         naming your own tenant",
    ))
}

fn stored(app: &App, caller: &Caller, key: &str) -> Result<Option<(ToolPolicy, u64)>, ApiError> {
    let Some(record) = app.registry.console_get(Collection::Policies, key)? else {
        return Ok(None);
    };
    if record.tenant != caller.tenant() {
        return Err(ApiError::internal(format!(
            "policy {key} belongs to another tenant"
        )));
    }
    let policy = serde_json::from_value(record.body)
        .map_err(|e| ApiError::internal(format!("stored policy {key} is unreadable: {e}")))?;
    Ok(Some((policy, record.revision)))
}

/// What applies to one caller, kept in its parts so a refusal can say which
/// limit it met.
pub(crate) struct Resolved {
    /// The stored policy for the caller: tenant, narrowed by groups and
    /// person.
    pub store: ToolPolicy,
    /// The `ceilings` configuration's connector ceiling for this caller.
    pub server_ceiling: Option<Vec<GrantEntry>>,
    /// Whether the server delegates at all.
    pub allow_delegation: bool,
}

impl Resolved {
    /// Everything together: what the caller may actually choose.
    pub fn effective(&self) -> ToolPolicy {
        with_server(
            &self.store,
            self.server_ceiling.as_deref(),
            self.allow_delegation,
        )
    }
}

/// The caller's stored policy and the server's limits. Reads the store, so
/// run it where blocking is fine.
pub(crate) fn resolved(app: &App, caller: &Caller) -> Result<Resolved, ApiError> {
    let tenant = caller.tenant();
    let tenant_policy = stored(app, caller, &tenant_key(tenant))?.map(|(p, _)| p);
    let mut groups = Vec::new();
    for group in &caller.0.groups {
        if let Some((policy, _)) = stored(app, caller, &group_key(tenant, group))? {
            groups.push(policy);
        }
    }
    let person = stored(
        app,
        caller,
        &principal_key(tenant, &caller.0.connector_subject()),
    )?
    .map(|(p, _)| p);
    let group_refs: Vec<&ToolPolicy> = groups.iter().collect();
    Ok(Resolved {
        store: resolve(tenant_policy.as_ref(), &group_refs, person.as_ref()),
        server_ceiling: super::connections::server_ceiling(app, caller),
        allow_delegation: app.config.allow_delegation,
    })
}

/// A session's tools against the policy as it stands now. A session records
/// the tenant's policy version when it is admitted. When its operation is
/// claimed and the version has moved, this runs the check again over the
/// tools the session was admitted with (`tools`, the session row's view):
/// a connector the policy no longer lists, or lists for reading only where
/// the session writes; a built-in skill, or all of them when none was
/// chosen, that the policy no longer allows; helpers turned off or a helper
/// harness no longer listed. A refusal is `policy_changed`, with the
/// detail of what is no longer allowed; the person starts again with tools
/// the policy allows. Reads the store, so run it where blocking is fine.
pub(crate) fn recheck(app: &App, caller: &Caller, tools: &Value) -> Result<(), ApiError> {
    let changed = |error: ApiError| {
        let mut out = ApiError::new(
            StatusCode::FORBIDDEN,
            "policy_changed",
            format!(
                "the tool policy changed after this session was admitted: {}",
                error.body.message
            ),
        );
        if let Some(detail) = error.body.detail {
            out = out.detail(detail);
        }
        out
    };
    let resolved = resolved(app, caller)?;
    let store = &resolved.store;
    let grant: Vec<GrantEntry> = tools["connectors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let mut entry = GrantEntry {
                mode: serde_json::from_value(c["mode"].clone()).ok()?,
                account: c["account"].as_str().map(str::to_owned),
                ..GrantEntry::read(c["connector"].as_str()?)
            };
            // A row written before operations and confirm were recorded
            // is checked as every operation, without confirmation.
            if let Some(ops) = c["operations"].as_array() {
                entry.operations = ops
                    .iter()
                    .filter_map(|op| op.as_str().map(str::to_owned))
                    .collect();
            }
            if let Ok(confirm) = serde_json::from_value(c["confirm"].clone()) {
                entry.confirm = confirm;
            }
            Some(entry)
        })
        .collect();
    check_connectors(store, &grant).map_err(changed)?;
    let catalogue = branchyard_provision::built_in::catalogue().map_err(ApiError::internal)?;
    let skills: Vec<String> = match tools["skills"].as_array() {
        Some(chosen) => chosen
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect(),
        None => catalogue.skills.iter().map(|s| s.id.clone()).collect(),
    };
    if let Some(id) = skills.iter().find(|id| !skill_allowed(store, id)) {
        return Err(changed(not_allowed(ToolKind::Skill, id, None)));
    }
    if let Some(helpers) = tools["helpers"]["harnesses"].as_array() {
        if !delegation_allowed(store) || !resolved.allow_delegation {
            return Err(changed(not_allowed(ToolKind::Helper, "*", None)));
        }
        if let Some(id) = helpers
            .iter()
            .filter_map(Value::as_str)
            .find(|h| !helper_allowed(store, h))
        {
            return Err(changed(not_allowed(ToolKind::Helper, id, None)));
        }
    }
    Ok(())
}

/// Refuse `entry`s that go beyond `policy`'s connector ceiling in any
/// part: a connector it does not list, a write where it allows reads, an
/// operation outside its globs, a confirmation it does not allow, or an
/// account other than the one it names.
pub(crate) fn check_connectors(policy: &ToolPolicy, grant: &[GrantEntry]) -> Result<(), ApiError> {
    for entry in grant {
        let fitted = fit_entry(policy, entry)?;
        let covered = fitted.iter().any(|f| {
            f.mode == entry.mode
                && f.confirm == entry.confirm
                && entry.operations.iter().all(|op| f.operations.contains(op))
        });
        if !covered {
            return Err(
                not_allowed(ToolKind::Connector, &entry.connector, None).detail(json!({
                    "kind": ToolKind::Connector,
                    "id": entry.connector,
                    "allowed": fitted,
                })),
            );
        }
    }
    Ok(())
}

/// `grant` narrowed to `policy`'s connector ceiling: each entry keeps only
/// the operations, confirmation and account the ceiling allows. An entry
/// the ceiling allows nothing of, or allows only reads of where it writes,
/// is refused by name rather than dropped or quietly made read only.
pub(crate) fn fit_connectors(
    policy: &ToolPolicy,
    grant: &[GrantEntry],
) -> Result<Vec<GrantEntry>, ApiError> {
    let mut out: Vec<GrantEntry> = Vec::new();
    for entry in grant {
        for fitted in fit_entry(policy, entry)? {
            if !out.contains(&fitted) {
                out.push(fitted);
            }
        }
    }
    Ok(out)
}

/// One entry narrowed to the ceiling. A ceiling entry with no account
/// covers every account of its connector.
fn fit_entry(policy: &ToolPolicy, entry: &GrantEntry) -> Result<Vec<GrantEntry>, ApiError> {
    let Some(ceiling) = policy.connectors.as_deref() else {
        return Ok(vec![entry.clone()]);
    };
    let fitted: Vec<GrantEntry> = ceiling
        .iter()
        .filter(|c| c.connector == entry.connector)
        .filter(|c| c.account.is_none() || c.account == entry.account)
        .flat_map(|c| {
            let c = GrantEntry {
                account: entry.account.clone(),
                ..c.clone()
            };
            intersect(std::slice::from_ref(entry), std::slice::from_ref(&c))
        })
        .collect();
    let mode = fitted.iter().map(|e| e.mode).max();
    match mode {
        None => Err(not_allowed(ToolKind::Connector, &entry.connector, None)),
        Some(GrantMode::Read) if entry.mode == GrantMode::Write => Err(not_allowed(
            ToolKind::Connector,
            &entry.connector,
            Some(GrantMode::Read),
        )),
        Some(_) => Ok(fitted
            .into_iter()
            .filter(|e| e.mode == entry.mode)
            .collect()),
    }
}

/// `tool_not_allowed`: the administrator's policy does not allow the tool.
/// `detail` is `{kind, id}`, and `max_mode` for a connector allowed less.
pub(crate) fn not_allowed(kind: ToolKind, id: &str, max_mode: Option<GrantMode>) -> ApiError {
    let name = match kind {
        ToolKind::Connector => super::connections::display(id),
        _ => id.to_owned(),
    };
    let message = match max_mode {
        Some(_) => format!("Your administrator allows read only for {name}."),
        None => format!("Your administrator has not allowed {name}."),
    };
    let mut detail = json!({ "kind": kind, "id": id });
    if let Some(mode) = max_mode {
        detail["max_mode"] = json!(mode);
    }
    ApiError::new(StatusCode::FORBIDDEN, "tool_not_allowed", message).detail(detail)
}

// ---------------------------------------------------------------------------
// The admin routes.
// ---------------------------------------------------------------------------

fn record(key: &str, policy: ToolPolicy, revision: u64) -> PolicyRecord {
    PolicyRecord {
        key: key.to_owned(),
        policy,
        revision,
    }
}

async fn list(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<PolicyList>, ApiError> {
    super::scope(&caller, "admin")?;
    let mut policies = Vec::new();
    for row in app
        .registry
        .console_list(Collection::Policies, caller.tenant())?
        .into_iter()
        .filter(|row| !is_version(&row.id))
    {
        let policy = serde_json::from_value(row.body).map_err(|e| {
            ApiError::internal(format!("stored policy {} is unreadable: {e}", row.id))
        })?;
        policies.push(record(&row.id, policy, row.revision));
    }
    let version = current_version(&app, caller.tenant())?;
    Ok(Json(PolicyList { policies, version }))
}

async fn read(
    State(app): State<Shared>,
    Path(key): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<PolicyRecord>, ApiError> {
    super::scope(&caller, "admin")?;
    check_key(caller.tenant(), &key)?;
    let (policy, revision) = stored(&app, &caller, &key)?.ok_or_else(|| no_policy(&key))?;
    Ok(Json(record(&key, policy, revision)))
}

fn no_policy(key: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no policy is stored at {key}"),
    )
}

/// Refuse a policy that names something the server cannot honour.
fn validate(policy: &ToolPolicy) -> Result<(), ApiError> {
    let too_many =
        |what: &str| ApiError::bad_request(format!("a policy lists at most {MAX_ENTRIES} {what}"));
    if let Some(connectors) = &policy.connectors {
        if connectors.len() > MAX_ENTRIES {
            return Err(too_many("connectors"));
        }
        for entry in connectors {
            entry.check().map_err(ApiError::bad_request)?;
        }
    }
    if let Some(skills) = &policy.skills {
        if skills.len() > MAX_ENTRIES {
            return Err(too_many("skills"));
        }
        branchyard_provision::built_in::check(skills)
            .map_err(|why| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "skill_unknown", why))?;
    }
    if let Some(helpers) = &policy.helpers {
        if helpers.len() > MAX_ENTRIES {
            return Err(too_many("helpers"));
        }
        super::tools::known_harnesses(helpers)?;
    }
    Ok(())
}

async fn put(
    State(app): State<Shared>,
    Path(key): Path<String>,
    Extension(caller): Extension<Caller>,
    crate::api::JsonBody(request, _): crate::api::JsonBody<PutPolicyRequest>,
) -> Result<Json<PolicyRecord>, ApiError> {
    super::scope(&caller, "admin")?;
    check_key(caller.tenant(), &key)?;
    validate(&request.policy)?;
    crate::api::blocking(move || {
        let body =
            serde_json::to_value(&request.policy).map_err(|e| ApiError::internal(e.to_string()))?;
        let (mutation, revision) = match app.registry.console_get(Collection::Policies, &key)? {
            Some(current) => {
                if current.tenant != caller.tenant() {
                    return Err(no_policy(&key));
                }
                if request.revision != Some(current.revision) {
                    return Err(super::revision_conflict());
                }
                let revision = current.revision.saturating_add(1);
                (super::update(current, body), revision)
            }
            None => {
                if request.revision.is_some_and(|r| r != 0) {
                    return Err(super::revision_conflict());
                }
                if app
                    .registry
                    .console_list(Collection::Policies, caller.tenant())?
                    .iter()
                    .filter(|row| !is_version(&row.id))
                    .count()
                    >= MAX_POLICIES
                {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "policy_limit",
                        format!("a tenant stores at most {MAX_POLICIES} policies"),
                    ));
                }
                (
                    super::insert(super::new(Collection::Policies, &key, &caller, body)),
                    1,
                )
            }
        };
        app.registry
            .console_mutate(&[mutation, bump(&app, &caller)?])?;
        Ok(Json(record(&key, request.policy, revision)))
    })
    .await?
}

async fn remove(
    State(app): State<Shared>,
    Path(key): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    super::scope(&caller, "admin")?;
    check_key(caller.tenant(), &key)?;
    let guard = super::routes::query(raw)
        .remove("revision")
        .map(|r| {
            r.parse::<u64>()
                .map_err(|_| ApiError::bad_request("revision must be a number"))
        })
        .transpose()?;
    let current = app
        .registry
        .console_get(Collection::Policies, &key)?
        .filter(|r| r.tenant == caller.tenant())
        .ok_or_else(|| no_policy(&key))?;
    if guard.is_some_and(|revision| revision != current.revision) {
        return Err(super::revision_conflict());
    }
    app.registry.console_mutate(&[
        crate::console_store::Mutation::Delete {
            collection: Collection::Policies,
            id: key.clone(),
            tenant: caller.tenant().to_owned(),
            expected: current.revision,
        },
        bump(&app, &caller)?,
    ])?;
    Ok(Json(json!({ "deleted": key })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> GrantEntry {
        GrantEntry::parse(text).unwrap()
    }

    fn connectors(entries: &[&str]) -> Option<Vec<GrantEntry>> {
        Some(entries.iter().map(|e| entry(e)).collect())
    }

    fn names(items: &[&str]) -> Option<Vec<String>> {
        Some(items.iter().map(|i| (*i).to_owned()).collect())
    }

    #[test]
    fn unset_levels_impose_no_limit() {
        let none = resolve(None, &[], None);
        assert_eq!(none, ToolPolicy::default());
        assert!(delegation_allowed(&none));
        assert!(skill_allowed(&none, "docx"));
        assert!(helper_allowed(&none, "claude-code"));
        assert_eq!(
            connector_mode(none.connectors.as_deref(), "x"),
            Some(GrantMode::Write)
        );
        let tenant = ToolPolicy {
            skills: names(&["docx"]),
            ..Default::default()
        };
        assert_eq!(resolve(Some(&tenant), &[], None), tenant, "tenant alone");
        assert_eq!(resolve(None, &[&tenant], None), tenant, "group alone");
        assert_eq!(resolve(None, &[], Some(&tenant)), tenant, "person alone");
    }

    #[test]
    fn intersecting_connectors_takes_the_lesser_mode_and_drops_the_rest() {
        let a = ToolPolicy {
            connectors: connectors(&["github:write", "linear:write"]),
            ..Default::default()
        };
        let b = ToolPolicy {
            connectors: connectors(&["github:read", "slack:write"]),
            ..Default::default()
        };
        let both = intersect_policy(&a, &b);
        assert_eq!(both.connectors, connectors(&["github:read"]));
        assert_eq!(intersect_policy(&b, &a), both, "order does not matter");
        let open = ToolPolicy::default();
        assert_eq!(intersect_policy(&a, &open), a);
        assert_eq!(intersect_policy(&open, &a), a);
        let empty = ToolPolicy {
            connectors: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(intersect_policy(&a, &empty).connectors, Some(vec![]));
    }

    #[test]
    fn intersecting_connectors_narrows_operations_and_keeps_accounts_apart() {
        let wide = ToolPolicy {
            connectors: connectors(&["github:write"]),
            ..Default::default()
        };
        let issues = ToolPolicy {
            connectors: connectors(&["github:write:issues.*"]),
            ..Default::default()
        };
        assert_eq!(intersect_policy(&wide, &issues), issues);
        let work = ToolPolicy {
            connectors: connectors(&["github@work:write"]),
            ..Default::default()
        };
        assert_eq!(
            intersect_policy(&wide, &work).connectors,
            Some(vec![]),
            "an account entry never matches the default account"
        );
    }

    #[test]
    fn intersecting_lists_keeps_what_both_name_and_delegation_needs_both() {
        let a = ToolPolicy {
            skills: names(&["docx", "pptx", "xlsx"]),
            helpers: names(&["claude-code", "gemini-cli"]),
            delegation: Some(true),
            ..Default::default()
        };
        let b = ToolPolicy {
            skills: names(&["xlsx", "docx", "pdf"]),
            helpers: names(&[]),
            delegation: Some(false),
            ..Default::default()
        };
        let both = intersect_policy(&a, &b);
        assert_eq!(both.skills, names(&["docx", "xlsx"]));
        assert_eq!(both.helpers, names(&[]));
        assert_eq!(both.delegation, Some(false));
        assert_eq!(
            intersect_policy(&a, &ToolPolicy::default()).delegation,
            Some(true)
        );
    }

    #[test]
    fn groups_and_the_person_pool_what_they_allow_and_the_tenant_caps_it() {
        let tenant = ToolPolicy {
            connectors: connectors(&["github:write", "linear:read"]),
            skills: names(&["docx", "pptx"]),
            ..Default::default()
        };
        let readers = ToolPolicy {
            connectors: connectors(&["github:read"]),
            skills: names(&["docx"]),
            ..Default::default()
        };
        let writers = ToolPolicy {
            connectors: connectors(&["github:write", "slack:write"]),
            skills: names(&["xlsx"]),
            ..Default::default()
        };
        let person = ToolPolicy {
            connectors: connectors(&["linear:write"]),
            ..Default::default()
        };
        let one = resolve(Some(&tenant), &[&readers], None);
        assert_eq!(one.connectors, connectors(&["github:read"]));
        assert_eq!(one.skills, names(&["docx"]));
        let two = resolve(Some(&tenant), &[&readers, &writers], Some(&person));
        assert_eq!(
            two.connectors,
            connectors(&["github:write", "linear:read"]),
            "github from the writers, linear from the person, both capped by the tenant; slack is outside it"
        );
        assert_eq!(
            two.skills,
            names(&["docx"]),
            "xlsx is outside the tenant's skills; the person names none, so adds none"
        );
    }

    #[test]
    fn a_policy_silent_on_a_field_does_not_lift_a_siblings_limit() {
        let limits = ToolPolicy {
            skills: names(&["docx"]),
            ..Default::default()
        };
        let delegates = ToolPolicy {
            delegation: Some(true),
            ..Default::default()
        };
        let merged = union_policy(&[&limits, &delegates]).unwrap();
        assert_eq!(merged.skills, names(&["docx"]));
        assert_eq!(merged.delegation, Some(true));
        assert_eq!(union_policy(&[]), None);
        let off = ToolPolicy {
            delegation: Some(false),
            ..Default::default()
        };
        assert_eq!(union_policy(&[&off]).unwrap().delegation, Some(false));
        assert_eq!(
            union_policy(&[&off, &delegates]).unwrap().delegation,
            Some(true),
            "any group that allows delegation allows it"
        );
    }

    #[test]
    fn an_empty_list_allows_nothing_and_no_list_allows_everything() {
        let none = ToolPolicy {
            skills: names(&[]),
            helpers: names(&[]),
            connectors: Some(vec![]),
            ..Default::default()
        };
        assert!(!skill_allowed(&none, "docx"));
        assert!(!helper_allowed(&none, "claude-code"));
        assert_eq!(connector_mode(none.connectors.as_deref(), "github"), None);
        let listed = ToolPolicy {
            helpers: names(&["claude-code"]),
            ..Default::default()
        };
        assert!(helper_allowed(&listed, "claude-code"));
        assert!(!helper_allowed(&listed, "gemini-cli"));
        let off = ToolPolicy {
            delegation: Some(false),
            ..Default::default()
        };
        assert!(!helper_allowed(&off, "claude-code"), "delegation off");
    }

    #[test]
    fn the_server_narrows_last() {
        let stored = ToolPolicy {
            connectors: connectors(&["github:write", "linear:write"]),
            delegation: Some(true),
            ..Default::default()
        };
        let server = [entry("github:read")];
        let effective = with_server(&stored, Some(&server), true);
        assert_eq!(effective.connectors, connectors(&["github:read"]));
        assert_eq!(effective.delegation, Some(true));
        let off = with_server(&stored, None, false);
        assert_eq!(off.connectors, stored.connectors, "no ceiling, no change");
        assert_eq!(off.delegation, Some(false), "the server flag wins");
        let unset = with_server(&ToolPolicy::default(), None, true);
        assert_eq!(unset, ToolPolicy::default());
        let open = with_server(&ToolPolicy::default(), Some(&server), true);
        assert_eq!(
            open.connectors,
            connectors(&["github:read"]),
            "the ceiling applies when the store says nothing"
        );
    }

    #[test]
    fn connector_checks_name_the_first_connector_over_the_policy() {
        let policy = ToolPolicy {
            connectors: connectors(&["github:read", "linear:write"]),
            ..Default::default()
        };
        assert!(check_connectors(&policy, &[entry("github"), entry("linear:write")]).is_ok());
        let over = check_connectors(&policy, &[entry("github:write")]).unwrap_err();
        assert_eq!(over.body.code, "tool_not_allowed");
        let detail = over.body.detail.unwrap();
        assert_eq!(detail["kind"], "connector");
        assert_eq!(detail["id"], "github");
        assert_eq!(detail["max_mode"], "read");
        let missing = check_connectors(&policy, &[entry("slack")]).unwrap_err();
        let detail = missing.body.detail.unwrap();
        assert_eq!(detail["id"], "slack");
        assert!(detail.get("max_mode").is_none());
    }

    #[test]
    fn connector_checks_hold_operations_confirm_and_account_to_the_policy() {
        let policy = ToolPolicy {
            connectors: connectors(&["github:read:issues.*"]),
            ..Default::default()
        };
        // Every operation is wider than the policy's issues.* and is refused
        // by the check; fitting narrows it to what the policy allows.
        let wide = entry("github:read");
        assert_eq!(
            check_connectors(&policy, std::slice::from_ref(&wide))
                .unwrap_err()
                .body
                .code,
            "tool_not_allowed"
        );
        let fitted = fit_connectors(&policy, &[wide]).unwrap();
        assert_eq!(fitted, [entry("github:read:issues.*")]);
        assert!(check_connectors(&policy, &fitted).is_ok());
        assert!(check_connectors(&policy, &[entry("github:read:issues.comments.*")]).is_ok());
        // A confirmation the policy does not allow is narrowed away.
        let confirming = GrantEntry {
            confirm: branchyard::connectors::Confirm::Allow,
            ..entry("github:read:issues.*")
        };
        assert!(check_connectors(&policy, std::slice::from_ref(&confirming)).is_err());
        assert_eq!(
            fit_connectors(&policy, &[confirming]).unwrap()[0].confirm,
            branchyard::connectors::Confirm::Deny
        );
        // An unnamed account in the policy covers each account; a named
        // one covers only itself.
        let work = GrantEntry {
            account: Some("work".into()),
            ..entry("github:read:issues.*")
        };
        assert!(check_connectors(&policy, std::slice::from_ref(&work)).is_ok());
        let pinned = ToolPolicy {
            connectors: Some(vec![GrantEntry {
                account: Some("home".into()),
                ..entry("github:read")
            }]),
            ..Default::default()
        };
        assert!(fit_connectors(&pinned, &[work]).is_err());
        // Operations outside the policy's globs leave nothing to fit.
        assert!(fit_connectors(&policy, &[entry("github:read:pulls.*")]).is_err());
    }

    #[test]
    fn keys_name_the_callers_own_tenant_in_one_of_three_forms() {
        for ok in [
            "tenant:acme",
            "group:acme:eng@example.com",
            "group:acme:a:b",
            "principal:acme:google:abc123",
        ] {
            assert!(check_key("acme", ok).is_ok(), "{ok}");
        }
        for bad in [
            "tenant:other",
            "group:other:eng",
            "group:acme:",
            "principal:acme:",
            "principal:other:me",
            "acme",
            "",
            "tenant:acme:extra",
            "group:acme:bad\nname",
        ] {
            let error = check_key("acme", bad).unwrap_err();
            assert_eq!(error.body.code, "policy_key_invalid", "{bad}");
        }
    }

    #[test]
    fn a_policy_serializes_without_its_unset_fields() {
        let policy = ToolPolicy {
            skills: names(&["docx"]),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&policy).unwrap(),
            json!({"skills":["docx"]})
        );
        let back: ToolPolicy = serde_json::from_value(json!({})).unwrap();
        assert_eq!(back, ToolPolicy::default());
        assert!(serde_json::from_value::<ToolPolicy>(json!({"skill":[]})).is_err());
    }
}
