//! `POST /v1/turns/token`: a running turn's sandbox fetches its gateway
//! token.
//!
//! The sandbox's credential proxy
//! (`branchyard::connectors::sandbox_proxy`) calls this every few minutes
//! with the turn it serves (`manifest`, `branch` as `<repo>/<branch>`, and
//! `turn`) and proves who it is with the turn's own secret in
//! `X-Branchyard-Turn-Secret`, always: the manifest digest, the branch, the
//! turn number and a shared service account are none of them secret, so
//! none is enough alone. Where `connectors.sandbox_identities` lists
//! Google service accounts (a GKE Workload Identity service account or a
//! GCE VM's), the request must also carry a Google-signed ID token for this
//! server's audience of a listed account, as a bearer token. A request with
//! only an identity, or only the secret where identities are listed, is
//! refused `auth_required`. The route is outside the bearer-token
//! middleware for that reason: it authenticates itself.
//!
//! What is issued, and refused, is `branchyard::Yard::issue_turn_token`'s: a
//! five-minute token with the claims of the turn's recorded grant, for a
//! turn that is running and admitted with that manifest, and never anything
//! the request body says. See `docs/server.md#turn-tokens-for-sandboxes`.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};

use branchyard::connectors::{
    Identity, Proof, RefusalKind, TokenRequest, TokenResponse, SECRET_HEADER, TOKEN_PATH,
};

use crate::api::{blocking, JsonBody, Shared};
use crate::auth::SandboxAuth;
use crate::error::ApiError;

pub(crate) fn router() -> Router<Shared> {
    Router::new().route(TOKEN_PATH, post(issue))
}

/// Whether `path` is this route, which the authentication middleware lets
/// through to authenticate itself.
pub(crate) fn is_token(path: &str) -> bool {
    path == TOKEN_PATH
}

fn status(kind: RefusalKind) -> StatusCode {
    match kind {
        RefusalKind::UnknownTurn => StatusCode::NOT_FOUND,
        RefusalKind::ManifestMismatch | RefusalKind::NotRunning | RefusalKind::TurnMismatch => {
            StatusCode::CONFLICT
        }
        RefusalKind::NoGrant | RefusalKind::TenantMismatch => StatusCode::FORBIDDEN,
        RefusalKind::BadSecret => StatusCode::UNAUTHORIZED,
        RefusalKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Who the caller is, from its headers: the turn's secret, and a listed
/// workload identity where the server lists any.
struct Caller {
    secret: String,
    identity: Option<Workload>,
}

struct Workload {
    service_account: String,
    tenant: Option<String>,
}

async fn authenticate(app: &Shared, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let Some(secret) = headers
        .get(SECRET_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 512)
    else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "auth_required",
            "present the turn's secret in X-Branchyard-Turn-Secret (and, where this server lists \
             sandbox identities, the sandbox's Google ID token as a bearer token)",
        ));
    };
    let secret = secret.to_owned();
    if !app.credentials.requires_sandbox_identity() {
        return Ok(Caller {
            secret,
            identity: None,
        });
    }
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if authorization.is_none() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "auth_required",
            "this server lists sandbox identities: present the sandbox's Google ID token as a \
             bearer token besides the turn's secret",
        ));
    }
    match app.credentials.verify_sandbox(authorization).await {
        Some(SandboxAuth::Allowed(identity)) => Ok(Caller {
            secret,
            identity: Some(Workload {
                service_account: identity.service_account,
                tenant: identity.tenant,
            }),
        }),
        Some(SandboxAuth::Unlisted(account)) => {
            tracing::warn!(service_account = %account, "a service account this server does not list asked for a turn token");
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "identity_not_allowed",
                "this service account is not in connectors.sandbox_identities",
            ))
        }
        Some(SandboxAuth::Invalid) | None => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credential",
            "the bearer token is not a valid Google ID token for this server's sandbox audience",
        )),
    }
}

/// `POST /v1/turns/token`.
async fn issue(
    State(app): State<Shared>,
    headers: HeaderMap,
    JsonBody(request, _): JsonBody<TokenRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    if app.config.connectors.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such route",
        ));
    }
    let caller = authenticate(&app, &headers).await?;
    let Some((repo, branch)) = request.branch.split_once('/') else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "branch is <repo>/<branch>",
        ));
    };
    let unknown = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            RefusalKind::UnknownTurn.code(),
            "no such branch",
        )
    };
    let yard = app.repos.get(repo).ok_or_else(unknown)?.yard.clone();
    let branch = branch.to_owned();
    let body = request.clone();
    let who = match &caller.identity {
        Some(identity) => identity.service_account.clone(),
        None => "turn secret".to_owned(),
    };
    let issued = blocking(move || {
        let proof = Proof {
            secret: &caller.secret,
            identity: caller.identity.as_ref().map(|w| Identity {
                service_account: &w.service_account,
                tenant: w.tenant.as_deref(),
            }),
        };
        yard.issue_turn_token(&branch, &body, proof)
    })
    .await?;
    match issued {
        Ok(token) => {
            // The audit line names the turn and who asked, never a token.
            tracing::info!(
                branch = %request.branch,
                turn = request.turn,
                manifest = %request.manifest,
                caller = %who,
                "issued a turn token to a sandbox"
            );
            Ok(Json(token))
        }
        Err(refusal) => Err(ApiError::new(
            status(refusal.kind),
            refusal.kind.code(),
            refusal.message,
        )),
    }
}
