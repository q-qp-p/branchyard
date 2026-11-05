//! Connections and the tools a session may choose.
//!
//! The connector gateway (Anvil) holds every upstream credential; this
//! server never does. For the console it signs the caller's own connect
//! token (`by_purpose: "connect"`, ten minutes, the caller's subject and
//! tenant, which the gateway hashes into the vault's owner) and asks the
//! gateway on the caller's behalf: which accounts are connected
//! (`GET /connect/status`), where to sign in (`POST /connect/start`), or
//! to store an API key (`POST /connect/api-key`). The token never reaches
//! the browser, and a key passes through once and is not kept or logged.
//!
//! - `GET /v1/connections`: every connector the gateway serves, with the
//!   caller's accounts, the most the caller's ceiling allows, and a plain
//!   reason when one cannot be used.
//! - `POST /v1/connections/{connector}/connect`: an authorization URL, or
//!   word that the connector takes a key.
//! - `POST /v1/connections/{connector}/key`: store a key.
//! - `GET /v1/workspaces/{workspace}/tools`: what the session form offers:
//!   connections, built-in skills and delegation.
//!
//! See `docs/connectors.md`, "From the console".

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use branchyard::connectors::{GrantEntry, GrantMode, CONNECT_TTL};
use branchyard_client::http;
use branchyard_client::workspace_api::{
    ApiKeyRequest, ConnectRequest, ConnectStart, ConnectedAccount, Connection, ConnectionList,
    ConnectionState, GatewayState, HelperOptions, SessionTools, SkillOption, ToolKind,
};
use serde_json::{json, Value};

use super::{auth, policy, tools};
use crate::api::{App, Caller, Shared};
use crate::error::ApiError;

/// How long one call to the gateway may take.
const GATEWAY_WAIT: Duration = Duration::from_secs(10);
/// The largest gateway answer read.
const MAX_ANSWER: usize = 1024 * 1024;
/// The most children `helpers.max_children` may ask for.
pub(super) const MAX_HELPERS: u32 = 16;
/// Children a session may have when `helpers` names no number.
pub(super) const DEFAULT_HELPERS: u32 = 4;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/connections", get(list))
        .route("/v1/connections/{connector}/connect", post(connect))
        .route("/v1/connections/{connector}/key", post(key))
        .route(
            "/v1/workspaces/{workspace}/tools",
            get(session_tools).put(tools::put_defaults),
        )
}

async fn list(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ConnectionList>, ApiError> {
    super::scope(&caller, "read")?;
    crate::api::blocking(move || {
        let resolved = policy::resolved(&app, &caller)?;
        Ok(Json(connections(&app, &caller, &resolved)))
    })
    .await?
}

async fn session_tools(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<SessionTools>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    crate::api::blocking(move || {
        let resolved = policy::resolved(&app, &caller)?;
        let effective = resolved.effective();
        let skills = branchyard_provision::built_in::catalogue().map_err(ApiError::internal)?;
        let topic = |id: &str| {
            skills
                .topics
                .iter()
                .find(|t| t.id == id)
                .map_or_else(|| id.to_owned(), |t| t.title.clone())
        };
        let options = skills
            .skills
            .iter()
            .filter(|s| tools::skill_usable(&skills, &effective, &s.id))
            .map(|s| SkillOption {
                id: s.id.clone(),
                description: s.description.clone(),
                topic: topic(&s.topic),
                requires: s.requires.clone(),
            })
            .collect();
        let helpers = tools::helper_state(&resolved);
        Ok(Json(SessionTools {
            connections: connections(&app, &caller, &resolved),
            skills: options,
            helpers: HelperOptions {
                allowed: helpers.is_none(),
                reason: helpers,
                max_children: policy::helper_limit(&effective),
            },
            defaults: tools::defaults(&app, &caller, &wid)?,
        }))
    })
    .await?
}

async fn connect(
    State(app): State<Shared>,
    Path(connector): Path<String>,
    Extension(caller): Extension<Caller>,
    crate::api::JsonBody(request, _): crate::api::JsonBody<ConnectRequest>,
) -> Result<Json<ConnectStart>, ApiError> {
    super::scope(&caller, "run")?;
    crate::api::blocking(move || {
        let gateway = usable(&app, &caller, &connector, request.account.as_deref())?;
        let mut body = json!({ "connector": connector });
        if let Some(account) = &request.account {
            body["account"] = json!(account);
        }
        let answer = gateway.call("POST", "/connect/start", Some(&body))?;
        match answer["kind"].as_str() {
            Some("oauth") => {
                let url = answer["url"]
                    .as_str()
                    .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                    .ok_or_else(|| unexpected("an authorization URL"))?;
                Ok(Json(ConnectStart::Oauth {
                    url: url.to_owned(),
                    expires_at: answer["expires_at"].as_u64(),
                }))
            }
            Some("static") => Ok(Json(ConnectStart::Key)),
            _ => Err(unexpected("a sign-in kind")),
        }
    })
    .await?
}

async fn key(
    State(app): State<Shared>,
    Path(connector): Path<String>,
    Extension(caller): Extension<Caller>,
    crate::api::JsonBody(request, _): crate::api::JsonBody<ApiKeyRequest>,
) -> Result<Json<Value>, ApiError> {
    super::scope(&caller, "run")?;
    if request.api_key.trim().is_empty() || request.api_key.len() > 8192 {
        return Err(ApiError::bad_request(
            "paste the key, at most 8192 characters",
        ));
    }
    crate::api::blocking(move || {
        let gateway = usable(&app, &caller, &connector, request.account.as_deref())?;
        let mut body = json!({ "connector": connector, "api_key": request.api_key });
        if let Some(account) = &request.account {
            body["account"] = json!(account);
        }
        let answer = gateway.call("POST", "/connect/api-key", Some(&body))?;
        Ok(Json(json!({
            "connected": answer["connected"].as_bool().unwrap_or(false),
            "connector": connector,
            "account": answer["account"],
        })))
    })
    .await?
}

/// The gateway, after checking that `connector` is one it serves and the
/// caller's ceiling allows, and that `account` is a valid name.
fn usable(
    app: &App,
    caller: &Caller,
    connector: &str,
    account: Option<&str>,
) -> Result<Client, ApiError> {
    let entry = GrantEntry {
        account: account.map(str::to_owned),
        ..GrantEntry::read(connector)
    };
    entry.check().map_err(ApiError::bad_request)?;
    let gateway = Client::for_caller(app, caller).ok_or_else(not_configured)?;
    let served = served(app).map_err(|why| unreachable_error(&why))?;
    if !served.iter().any(|id| id == connector) {
        return Err(not_served(connector));
    }
    let resolved = policy::resolved(app, caller)?;
    if policy::connector_mode(resolved.server_ceiling.as_deref(), connector).is_none() {
        return Err(blocked(connector));
    }
    policy::check_connectors(&resolved.store, &[GrantEntry::read(connector)])?;
    Ok(gateway)
}

/// The caller's connections, never failing: a gateway that does not
/// answer is reported in the list.
pub(super) fn connections(
    app: &App,
    caller: &Caller,
    resolved: &policy::Resolved,
) -> ConnectionList {
    let Some(gateway) = Client::for_caller(app, caller) else {
        return ConnectionList {
            gateway: GatewayState::NotConfigured,
            reason: Some(
                "Connections are not set up on this server. Ask your administrator to turn \
                 them on."
                    .into(),
            ),
            connections: Vec::new(),
        };
    };
    let served = match served(app) {
        Ok(served) => served,
        Err(why) => {
            tracing::warn!(error = %why, "could not list the gateway's connectors");
            return unreachable_list();
        }
    };
    let status = gateway.call("GET", "/connect/status", None);
    let (state, reason, accounts) = match &status {
        Ok(answer) => (GatewayState::Ready, None, answer["connections"].clone()),
        Err(error) => {
            tracing::warn!(error = %error.body.message, "the connector gateway did not answer");
            (
                GatewayState::Unreachable,
                Some(UNREACHABLE.to_owned()),
                Value::Null,
            )
        }
    };
    let effective = resolved.effective();
    let connections = served
        .iter()
        .map(|id| {
            let off = policy::is_off(&effective, ToolKind::Connector, id);
            connection(id, &accounts, state, effective.connectors.as_deref(), off)
        })
        .collect();
    ConnectionList {
        gateway: state,
        reason,
        connections,
    }
}

const UNREACHABLE: &str = "We could not reach the connection service. Try again in a moment.";

fn unreachable_list() -> ConnectionList {
    ConnectionList {
        gateway: GatewayState::Unreachable,
        reason: Some(UNREACHABLE.into()),
        connections: Vec::new(),
    }
}

fn connection(
    id: &str,
    accounts: &Value,
    gateway: GatewayState,
    ceiling: Option<&[GrantEntry]>,
    off: bool,
) -> Connection {
    let listed = branchyard_controls::catalog::connectors()
        .iter()
        .find(|c| c.id == id);
    let name = listed.map_or_else(|| title(id), |c| c.name.clone());
    let mine: Vec<ConnectedAccount> = accounts
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["connector"] == id)
        .filter_map(|a| {
            Some(ConnectedAccount {
                account: a["account"].as_str()?.to_owned(),
                state: match a["status"].as_str() {
                    Some("active") => ConnectionState::Connected,
                    _ => ConnectionState::NeedsReconnect,
                },
                kind: a["kind"].as_str().map(str::to_owned),
                updated_at: a["updatedAt"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let max_mode = match off {
        true => None,
        false => policy::connector_mode(ceiling, id),
    };
    let state = if max_mode.is_none() {
        ConnectionState::Blocked
    } else if gateway != GatewayState::Ready {
        ConnectionState::Unknown
    } else if mine.iter().any(|a| a.state == ConnectionState::Connected) {
        ConnectionState::Connected
    } else if mine.is_empty() {
        ConnectionState::NotConnected
    } else {
        ConnectionState::NeedsReconnect
    };
    let reason = match state {
        ConnectionState::Blocked if off => Some(format!("{name} is turned off for you.")),
        ConnectionState::Blocked => Some(format!("Your administrator has not allowed {name}.")),
        ConnectionState::NotConnected => Some(format!("Connect {name} to use it.")),
        ConnectionState::NeedsReconnect => Some(format!(
            "{name} signed you out. Reconnect to keep using it."
        )),
        ConnectionState::Unknown => Some(UNREACHABLE.to_owned()),
        ConnectionState::Connected => None,
    };
    Connection {
        connector: id.to_owned(),
        icon: icon(id),
        name,
        description: listed.map(|c| c.description.clone()),
        state,
        accounts: mine,
        max_mode,
        reason,
    }
}

/// The console's connector icons, generated by `tools/connector_icons.py`.
const ICONS: &str = include_str!("../companion/assets/connector-icons.svg");

/// The sprite symbol drawing `id`'s mark, when the sprite has one.
fn icon(id: &str) -> Option<String> {
    let symbol = format!("ci-{id}");
    ICONS
        .contains(&format!("<symbol id=\"{symbol}\""))
        .then_some(symbol)
}

/// A connector's display name when the catalog has none: the icon's
/// title (`GitHub`), else `shipping_v2` as `Shipping v2`.
fn title(id: &str) -> String {
    let symbol = format!("<symbol id=\"ci-{id}\" viewBox=\"0 0 24 24\"><title>");
    if let Some(start) = ICONS.find(&symbol).map(|at| at + symbol.len()) {
        if let Some(name) = ICONS[start..].split('<').next().filter(|n| !n.is_empty()) {
            return name.to_owned();
        }
    }
    let text = id.replace(['_', '-'], " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// The connector IDs the gateway serves: its bundle root, as `by gateway
/// status` lists it.
fn served(app: &App) -> Result<Vec<String>, String> {
    let c = app
        .config
        .connectors
        .as_ref()
        .ok_or("this server has no connector gateway")?;
    Ok(branchyard::connectors::packager::discover(&c.bundles)?
        .into_iter()
        .map(|b| b.id)
        .collect())
}

/// The caller's ceiling in the server's configuration, if the operator set
/// one.
pub(super) fn server_ceiling(app: &App, caller: &Caller) -> Option<Vec<GrantEntry>> {
    let ceilings = app.config.actor_policy(&app.config.ceilings);
    ceilings
        .get(&caller.0.connector_subject())
        .or_else(|| ceilings.get(caller.name()))
        .and_then(|c| c.connectors.clone())
}

/// Refuse a session's connectors that this caller cannot use now, naming
/// the first and what to do about it: not served, over the server's
/// ceiling or the caller's policy, or not connected. A gateway that does not
/// answer refuses too.
pub(super) fn admit(
    app: &App,
    caller: &Caller,
    resolved: &policy::Resolved,
    grant: &[GrantEntry],
) -> Result<(), ApiError> {
    if grant.is_empty() {
        return Ok(());
    }
    let gateway = Client::for_caller(app, caller).ok_or_else(not_configured)?;
    let served = served(app).map_err(|why| unreachable_error(&why))?;
    for entry in grant {
        if !served.contains(&entry.connector) {
            return Err(not_served(&entry.connector));
        }
        match policy::connector_mode(resolved.server_ceiling.as_deref(), &entry.connector) {
            None => return Err(blocked(&entry.connector)),
            Some(GrantMode::Read) if entry.mode == GrantMode::Write => {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "connector_over_ceiling",
                    format!(
                        "Your administrator allows read only for {}.",
                        display(&entry.connector)
                    ),
                )
                .detail(json!({"connector": entry.connector, "max_mode": "read"})))
            }
            Some(_) => {}
        }
    }
    policy::check_connectors(&resolved.store, grant)?;
    let status = gateway.call("GET", "/connect/status", None)?;
    for entry in grant {
        let account = entry.account.as_deref().unwrap_or("default");
        let usable = status["connections"].as_array().is_some_and(|all| {
            all.iter().any(|a| {
                a["connector"] == entry.connector.as_str()
                    && a["account"] == account
                    && a["status"] == "active"
            })
        });
        if !usable {
            let name = display(&entry.connector);
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "connector_not_connected",
                format!("Connect {name} before a session can use it."),
            )
            .detail(json!({
                "connector": entry.connector,
                "account": account,
                "action": "connect",
            })));
        }
    }
    Ok(())
}

pub(super) fn display(connector: &str) -> String {
    branchyard_controls::catalog::connectors()
        .iter()
        .find(|c| c.id == connector)
        .map_or_else(|| title(connector), |c| c.name.clone())
}

fn not_configured() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "connectors_not_configured",
        "Connections are not set up on this server. Ask your administrator to turn them on \
         (docs/connectors.md).",
    )
}

fn not_served(connector: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "connector_not_served",
        format!("This server does not offer {} yet.", display(connector)),
    )
    .detail(json!({ "connector": connector }))
}

fn blocked(connector: &str) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "connector_over_ceiling",
        format!("Your administrator has not allowed {}.", display(connector)),
    )
    .detail(json!({ "connector": connector }))
}

fn unreachable_error(why: &str) -> ApiError {
    tracing::warn!(error = %why, "the connector gateway did not answer");
    ApiError::new(StatusCode::BAD_GATEWAY, "gateway_unreachable", UNREACHABLE)
}

fn unexpected(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "gateway_unexpected",
        format!("The connection service did not return {what}."),
    )
}

/// The gateway's connect routes, called as one person.
struct Client {
    gateway: branchyard::connectors::Gateway,
    endpoint: http::Endpoint,
    subject: String,
}

impl Client {
    /// The client for `caller`, or `None` without a gateway. The token's
    /// subject and tenant are the ones the caller's console turns carry,
    /// so the account it connects is the one those turns use.
    fn for_caller(app: &App, caller: &Caller) -> Option<Client> {
        let mut gateway = crate::connectors::gateway_for(&app.config, "console")?;
        gateway.tenant = caller.tenant().to_owned();
        let mut endpoint = http::Endpoint::parse(&gateway.url).ok()?;
        // The connect routes sit at the gateway's root, beside `/mcp`.
        endpoint.prefix = String::new();
        Some(Client {
            gateway,
            endpoint,
            subject: caller.0.connector_subject(),
        })
    }

    /// Call `path` with the caller's connect token; the answer's JSON, or
    /// the gateway's own refusal as an error.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let token = self
            .gateway
            .connect_token(Some(&self.subject), CONNECT_TTL)
            .map_err(|e| ApiError::internal(format!("could not sign a connect token: {e}")))?;
        let encoded = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let mut headers = vec![
            ("authorization", format!("Bearer {token}")),
            ("accept", "application/json".to_owned()),
        ];
        if encoded.is_some() {
            headers.push(("content-type", "application/json".to_owned()));
        }
        let tls = match self.endpoint.tls {
            true => Some(http::tls_config(None).map_err(|e| unreachable_error(&e))?),
            false => None,
        };
        let answer = (|| {
            let stream = http::connect(&self.endpoint, tls.as_ref(), GATEWAY_WAIT)?;
            let response = http::send(
                stream,
                &self.endpoint,
                &http::Request {
                    method,
                    target: path,
                    headers: &headers,
                    body: encoded.as_deref(),
                },
            )?;
            let status = response.status;
            Ok::<_, std::io::Error>((status, response.read_body(MAX_ANSWER)?))
        })();
        let (status, bytes) = answer.map_err(|e| unreachable_error(&e.to_string()))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(value);
        }
        let message = value["error"]["message"]
            .as_str()
            .unwrap_or("the connection service refused the request")
            .to_owned();
        let code = value["error"]["code"].as_str().unwrap_or("gateway_refused");
        tracing::warn!(status, code, "the connector gateway refused a connect call");
        Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "gateway_refused",
            format!("The connection service said: {message}"),
        )
        .detail(json!({ "status": status, "code": code })))
    }
}
