//! Sign-in through Google Cloud IAP and through federated OIDC issuers.
//!
//! Two kinds of credential end here. IAP puts a signed assertion in
//! `x-goog-iap-jwt-assertion` (ES256, one fixed Google issuer). A federated
//! issuer, such as a Workforce Identity Federation pool, a Workload Identity
//! Federation provider or any OIDC identity provider, signs a bearer ID token
//! (RS256 or ES256). Both are checked against keys fetched with the
//! workspace's HTTP client and cached, then mapped to a [`Principal`] by the
//! first matching binding. A token that verifies but matches no binding is
//! refused with `identity_not_bound`; a token that fails verification is never
//! retried as a bearer.
//!
//! Groups come from the token's groups claim. When the token carries none and
//! `groups_from: cloud_identity` is configured, they come from the Cloud
//! Identity Groups API with the server's own metadata-server token, cached for
//! five minutes. See `docs/identity.md`.

use crate::config::{
    check_principal, check_scope_name, check_tenant_name, AuthenticationOrigin, Principal,
    DEFAULT_TENANT,
};
use crate::error::ApiError;
use axum::http::StatusCode;
use base64::Engine;
use branchyard_client::http;
use branchyard_support::LockExt;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The header Google Cloud IAP puts a signed assertion in.
pub const IAP_HEADER: &str = "x-goog-iap-jwt-assertion";
/// The issuer of every IAP assertion.
pub const IAP_ISSUER: &str = "https://cloud.google.com/iap";
const IAP_KEYS_URL: &str = "https://www.gstatic.com/iap/verify/public_key-jwk";
const CLOUD_IDENTITY_URL: &str = "https://cloudidentity.googleapis.com";
const GROUPS_SCOPE: &str = "https://www.googleapis.com/auth/cloud-identity.groups.readonly";
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_GROUPS: usize = 512;
const MAX_GROUP_PAGES: usize = 10;
const CLOCK_SKEW_SECS: u64 = 10;
const KEY_TTL_MS: u64 = 60 * 60 * 1000;
/// Unknown key ids refetch the key set at most this often.
const KEY_REFETCH_MS: u64 = 60 * 1000;
const GROUPS_TTL_MS: u64 = 5 * 60 * 1000;
const GROUPS_CACHE_MAX: usize = 10_000;
const FETCH_DEADLINE: Duration = Duration::from_secs(10);

/// Where groups come from when the token carries none.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupsFrom {
    /// The Cloud Identity Groups API, called with the server's metadata token.
    CloudIdentity,
}

/// One rule mapping a verified identity to server authorization policy.
/// Bindings are tried in order and the first match wins.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityBinding {
    /// An exact email address, `domain:example.com`, or `group:eng@example.com`.
    /// Email, domain and group names compare without regard to case.
    #[serde(rename = "match")]
    pub matches: String,
    /// The principal's name. Defaults to the email, else the subject.
    #[serde(default)]
    pub name: Option<String>,
    /// The tenant. Defaults to the provider's `tenant`.
    #[serde(default)]
    pub tenant: Option<String>,
    /// The scopes. Defaults to the provider's `scopes`.
    #[serde(default)]
    pub scopes: Option<BTreeSet<String>>,
    /// A repository allowlist. Absent allows every repository of the tenant.
    #[serde(default)]
    pub repos: Option<BTreeSet<String>>,
}

/// Sign-in behind Google Cloud IAP.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IapConfig {
    /// The exact IAP audience: `/projects/NUMBER/global/backendServices/ID`,
    /// or `/projects/NUMBER/apps/PROJECT` for App Engine and Cloud Run.
    pub audience: String,
    /// Where to fetch the signing keys. Defaults to Google's published IAP
    /// key set. Set it only to a mirror or, in tests, a local server.
    #[serde(default)]
    pub keys_url: Option<String>,
    /// The tenant of a binding that names none. Defaults to the default tenant.
    #[serde(default)]
    pub tenant: Option<String>,
    /// The scopes of a binding that names none. Defaults to `read`.
    #[serde(default)]
    pub scopes: Option<BTreeSet<String>>,
    /// Who may sign in, and as what.
    pub bindings: Vec<IdentityBinding>,
    /// Where to look up groups. IAP assertions carry no groups claim.
    #[serde(default)]
    pub groups_from: Option<GroupsFrom>,
    /// The Cloud Identity base URL. Defaults to Google's. For tests.
    #[serde(default)]
    pub groups_url: Option<String>,
}

/// A federated OIDC issuer whose bearer ID tokens are accepted.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcIssuerConfig {
    /// The exact `iss` claim.
    pub issuer: String,
    /// The exact audience this server expects in `aud`.
    pub audience: String,
    /// Where the issuer publishes its signing keys (a JWKS document).
    pub jwks_url: String,
    /// The claim that names the subject. Defaults to `sub`.
    #[serde(default)]
    pub subject_claim: Option<String>,
    /// The claim that holds the email. Defaults to `email`.
    #[serde(default)]
    pub email_claim: Option<String>,
    /// The claim that holds the groups. Defaults to `groups`.
    #[serde(default)]
    pub groups_claim: Option<String>,
    /// The tenant of a binding that names none. Defaults to the default tenant.
    #[serde(default)]
    pub tenant: Option<String>,
    /// The scopes of a binding that names none. Defaults to `read`.
    #[serde(default)]
    pub scopes: Option<BTreeSet<String>>,
    /// Who may sign in, and as what.
    pub bindings: Vec<IdentityBinding>,
    /// Where to look up groups when the token carries none.
    #[serde(default)]
    pub groups_from: Option<GroupsFrom>,
    /// The Cloud Identity base URL. Defaults to Google's. For tests.
    #[serde(default)]
    pub groups_url: Option<String>,
}

fn check_bindings(label: &str, bindings: &[IdentityBinding]) -> Result<(), String> {
    if bindings.is_empty() {
        return Err(format!("{label} needs at least one binding"));
    }
    for binding in bindings {
        let m = binding.matches.as_str();
        let value = m
            .strip_prefix("domain:")
            .or_else(|| m.strip_prefix("group:"))
            .unwrap_or(m);
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(format!("{label}: binding match {m:?} is not usable"));
        }
        if !m.contains(':') && !m.contains('@') {
            return Err(format!(
                "{label}: binding match {m:?} must be an email, domain:NAME or group:EMAIL"
            ));
        }
        if let Some(tenant) = &binding.tenant {
            check_tenant_name(tenant)?;
        }
        for scope in binding.scopes.iter().flatten() {
            check_scope_name(scope)?;
        }
        if let Some(name) = &binding.name {
            if name.is_empty() || name.len() > 128 {
                return Err(format!("{label}: binding name {name:?} is not usable"));
            }
        }
    }
    Ok(())
}

fn check_url(label: &str, url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(());
    }
    Err(format!(
        "{label}: {url:?} is not an http:// or https:// URL"
    ))
}

/// Validate the sign-in configuration before any token is accepted.
pub fn validate(iap: Option<&IapConfig>, issuers: &[OidcIssuerConfig]) -> Result<(), String> {
    if let Some(iap) = iap {
        if !iap.audience.starts_with("/projects/") || iap.audience.len() > 512 {
            return Err(
                "iap.audience must be /projects/NUMBER/global/backendServices/ID or /projects/NUMBER/apps/PROJECT"
                    .into(),
            );
        }
        for url in iap.keys_url.iter().chain(&iap.groups_url) {
            check_url("iap", url)?;
        }
        if let Some(tenant) = &iap.tenant {
            check_tenant_name(tenant)?;
        }
        for scope in iap.scopes.iter().flatten() {
            check_scope_name(scope)?;
        }
        check_bindings("iap", &iap.bindings)?;
    }
    let mut seen = BTreeSet::new();
    for issuer in issuers {
        let label = format!("oidc_issuers[{}]", issuer.issuer);
        if issuer.issuer.is_empty() || issuer.audience.is_empty() {
            return Err("oidc_issuers needs an issuer and an audience".into());
        }
        if !seen.insert((&issuer.issuer, &issuer.audience)) {
            return Err(format!("{label} is listed twice for one audience"));
        }
        check_url(&label, &issuer.jwks_url)?;
        for url in issuer.groups_url.iter() {
            check_url(&label, url)?;
        }
        if let Some(tenant) = &issuer.tenant {
            check_tenant_name(tenant)?;
        }
        for scope in issuer.scopes.iter().flatten() {
            check_scope_name(scope)?;
        }
        check_bindings(&label, &issuer.bindings)?;
    }
    Ok(())
}

/// Why a presented credential was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The credential did not verify.
    Unauthorized,
    /// It verified, but no binding admits the identity.
    NotBound,
    /// A dependency (key set, groups lookup) could not be reached.
    Unavailable(String),
}

impl From<Refusal> for ApiError {
    fn from(refusal: Refusal) -> ApiError {
        match refusal {
            Refusal::Unauthorized => ApiError::unauthorized(),
            Refusal::NotBound => ApiError::new(
                StatusCode::FORBIDDEN,
                "identity_not_bound",
                "this identity is not bound to a Branchyard principal",
            ),
            Refusal::Unavailable(why) => {
                ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "identity_unavailable", why)
            }
        }
    }
}

/// A source of an OAuth access token for the Cloud Identity Groups API.
/// Blocking: it is called from a blocking task.
pub trait AccessToken: Send + Sync {
    /// A bearer token valid for the groups-read scope.
    fn token(&self) -> Result<String, String>;
}

/// The server's own token from the GCE/GKE metadata server (Workload Identity
/// on GKE answers the same route). Cached until shortly before it expires.
pub struct MetadataAccessToken {
    url: String,
    cached: Mutex<Option<(String, u64)>>,
}

impl MetadataAccessToken {
    /// A source that asks the metadata server at `url`, or the standard one.
    pub fn new(url: Option<&str>) -> MetadataAccessToken {
        MetadataAccessToken {
            url: url.unwrap_or(METADATA_TOKEN_URL).to_owned(),
            cached: Mutex::new(None),
        }
    }
}

impl AccessToken for MetadataAccessToken {
    fn token(&self) -> Result<String, String> {
        let now = branchyard_support::time::now_ms();
        if let Some((token, expires)) = &*self.cached.lock_recovering("metadata access token") {
            if *expires > now {
                return Ok(token.clone());
            }
        }
        let url = format!("{}?scopes={}", self.url, http::encode(GROUPS_SCOPE));
        let body = get(&url, vec![("Metadata-Flavor", "Google".to_owned())])?;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| "the metadata server did not answer JSON".to_owned())?;
        let token = value
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or("the metadata server answered no access_token")?
            .to_owned();
        let lifetime = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_sub(60);
        *self.cached.lock_recovering("metadata access token") =
            Some((token.clone(), now + lifetime * 1000));
        Ok(token)
    }
}

/// GET `url` through the workspace HTTP client. Blocking. The body is bounded
/// and a non-200 answer is an error that never echoes the body.
fn get(url: &str, mut headers: Vec<(&str, String)>) -> Result<Vec<u8>, String> {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let endpoint = http::Endpoint::parse(base)?;
    let tls = match endpoint.tls {
        true => Some(http::tls_config(None)?),
        false => None,
    };
    let path = match endpoint.prefix.is_empty() {
        true => "/".to_owned(),
        false => endpoint.prefix.clone(),
    };
    let target = match query {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    headers.push(("Accept", "application/json".to_owned()));
    let stream = http::connect(&endpoint, tls.as_ref(), FETCH_DEADLINE)
        .map_err(|e| format!("could not reach {}: {e}", endpoint))?;
    let response = http::send(
        stream,
        &endpoint,
        &http::Request {
            method: "GET",
            target: &target,
            headers: &headers,
            body: None,
        },
    )
    .map_err(|e| format!("{endpoint}: {e}"))?;
    if response.status != 200 {
        return Err(format!("{endpoint} answered {}", response.status));
    }
    response
        .read_body(MAX_BODY_BYTES)
        .map_err(|e| format!("{endpoint}: {e}"))
}

async fn fetch(url: String, headers: Vec<(&'static str, String)>) -> Result<Vec<u8>, String> {
    let work = tokio::task::spawn_blocking(move || get(&url, headers));
    match tokio::time::timeout(FETCH_DEADLINE + Duration::from_secs(5), work).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(format!("fetch task failed: {e}")),
        Err(_) => Err("fetch timed out".into()),
    }
}

struct CachedKeys {
    fetched_ms: u64,
    keys: JwkSet,
}

struct CachedGroups {
    fetched_ms: u64,
    groups: BTreeSet<String>,
}

/// Verifies IAP assertions and federated ID tokens, and maps them to principals.
pub struct Federation {
    iap: Option<IapConfig>,
    issuers: Vec<OidcIssuerConfig>,
    keys: Mutex<HashMap<String, CachedKeys>>,
    groups: Mutex<HashMap<String, CachedGroups>>,
    access: Arc<dyn AccessToken>,
}

/// What a provider contributes to a binding: the defaults it supplies and
/// the claims it reads.
struct Provider<'a> {
    tenant: Option<&'a str>,
    scopes: Option<&'a BTreeSet<String>>,
    bindings: &'a [IdentityBinding],
    groups_from: Option<GroupsFrom>,
    groups_url: Option<&'a str>,
}

/// A verified token's identity, before binding.
struct Identity {
    subject: String,
    email: Option<String>,
    claim_groups: Option<BTreeSet<String>>,
}

impl Federation {
    /// A verifier for the configured providers, looking groups up with `access`.
    pub fn new(
        iap: Option<IapConfig>,
        issuers: Vec<OidcIssuerConfig>,
        access: Arc<dyn AccessToken>,
    ) -> Federation {
        Federation {
            iap,
            issuers,
            keys: Mutex::new(HashMap::new()),
            groups: Mutex::new(HashMap::new()),
            access,
        }
    }

    /// Whether IAP sign-in is on.
    pub fn iap_enabled(&self) -> bool {
        self.iap.is_some()
    }

    /// Authenticate by the IAP header. `None` when IAP is not configured or
    /// the request carries no assertion; the request then proceeds to its
    /// other credentials. Once the header is present and IAP is configured a
    /// failure is final.
    pub async fn verify_iap(&self, assertion: Option<&str>) -> Option<Result<Principal, Refusal>> {
        let config = self.iap.as_ref()?;
        let token = assertion?.trim();
        Some(self.admit_iap(config, token).await)
    }

    async fn admit_iap(&self, config: &IapConfig, token: &str) -> Result<Principal, Refusal> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(Refusal::Unauthorized);
        }
        let keys_url = config.keys_url.as_deref().unwrap_or(IAP_KEYS_URL);
        let claims = self
            .verify_jwt(
                token,
                keys_url,
                IAP_ISSUER,
                &config.audience,
                &[Algorithm::ES256],
            )
            .await?;
        let subject = string_claim(&claims, "sub").ok_or(Refusal::Unauthorized)?;
        let email = string_claim(&claims, "email").ok_or(Refusal::Unauthorized)?;
        let identity = Identity {
            subject: subject.clone(),
            email: Some(email.clone()),
            claim_groups: groups_claim(&claims, "groups"),
        };
        let provider = Provider {
            tenant: config.tenant.as_deref(),
            scopes: config.scopes.as_ref(),
            bindings: &config.bindings,
            groups_from: config.groups_from,
            groups_url: config.groups_url.as_deref(),
        };
        self.bind(
            &provider,
            identity,
            AuthenticationOrigin::Iap {
                email,
                sub: subject,
            },
        )
        .await
    }

    /// Authenticate an `Authorization` bearer ID token from a configured
    /// issuer. `None` when the header is not a bearer JWT naming one of the
    /// configured issuers, so static and paired tokens are untouched. A token
    /// that names a configured issuer and fails verification is refused.
    pub async fn verify_oidc(&self, header: Option<&str>) -> Option<Result<Principal, Refusal>> {
        if self.issuers.is_empty() {
            return None;
        }
        let token = header?.strip_prefix("Bearer ")?.trim();
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return None;
        }
        let issuer = unverified_issuer(token)?;
        let candidates: Vec<&OidcIssuerConfig> =
            self.issuers.iter().filter(|c| c.issuer == issuer).collect();
        if candidates.is_empty() {
            return None;
        }
        let mut last = Refusal::Unauthorized;
        for config in candidates {
            match self.admit_oidc(config, token).await {
                Ok(principal) => return Some(Ok(principal)),
                Err(refusal) => last = refusal,
            }
        }
        Some(Err(last))
    }

    async fn admit_oidc(
        &self,
        config: &OidcIssuerConfig,
        token: &str,
    ) -> Result<Principal, Refusal> {
        let claims = self
            .verify_jwt(
                token,
                &config.jwks_url,
                &config.issuer,
                &config.audience,
                &[Algorithm::RS256, Algorithm::ES256],
            )
            .await?;
        let subject = string_claim(&claims, config.subject_claim.as_deref().unwrap_or("sub"))
            .ok_or(Refusal::Unauthorized)?;
        // An unverified email is not an identity.
        let verified = claims
            .get("email_verified")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let email = string_claim(&claims, config.email_claim.as_deref().unwrap_or("email"))
            .filter(|_| verified);
        let groups_claim_name = config.groups_claim.as_deref().unwrap_or("groups");
        let identity = Identity {
            subject: subject.clone(),
            email,
            claim_groups: groups_claim(&claims, groups_claim_name),
        };
        let provider = Provider {
            tenant: config.tenant.as_deref(),
            scopes: config.scopes.as_ref(),
            bindings: &config.bindings,
            groups_from: config.groups_from,
            groups_url: config.groups_url.as_deref(),
        };
        self.bind(
            &provider,
            identity,
            AuthenticationOrigin::Oidc {
                issuer: config.issuer.clone(),
                sub: subject,
            },
        )
        .await
    }

    /// Resolve groups, then the first matching binding, into a principal.
    async fn bind(
        &self,
        provider: &Provider<'_>,
        identity: Identity,
        origin: AuthenticationOrigin,
    ) -> Result<Principal, Refusal> {
        let groups = match (identity.claim_groups, provider.groups_from, &identity.email) {
            (Some(groups), _, _) => groups,
            (None, Some(GroupsFrom::CloudIdentity), Some(email)) => {
                self.cloud_identity_groups(provider.groups_url, email)
                    .await?
            }
            (None, _, _) => BTreeSet::new(),
        };
        let binding = provider
            .bindings
            .iter()
            .find(|binding| binding_matches(&binding.matches, identity.email.as_deref(), &groups))
            .ok_or(Refusal::NotBound)?;
        let principal = Principal {
            name: binding
                .name
                .clone()
                .or_else(|| identity.email.clone())
                .unwrap_or_else(|| identity.subject.clone()),
            tenant: binding
                .tenant
                .as_deref()
                .or(provider.tenant)
                .unwrap_or(DEFAULT_TENANT)
                .to_owned(),
            scopes: binding
                .scopes
                .as_ref()
                .or(provider.scopes)
                .cloned()
                .unwrap_or_else(|| ["read".to_owned()].into_iter().collect()),
            repos: binding.repos.clone(),
            groups,
            origin,
        };
        // The name is bounded and the tenant checked in config; a token's own
        // email or subject can still be unusable as a name.
        check_principal(&principal).map_err(|_| Refusal::NotBound)?;
        Ok(principal)
    }

    async fn verify_jwt(
        &self,
        token: &str,
        keys_url: &str,
        issuer: &str,
        audience: &str,
        allowed: &[Algorithm],
    ) -> Result<Map<String, Value>, Refusal> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| Refusal::Unauthorized)?;
        if !allowed.contains(&header.alg) {
            return Err(Refusal::Unauthorized);
        }
        let kid = header.kid.ok_or(Refusal::Unauthorized)?;
        let jwk = self.key(keys_url, &kid).await?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| Refusal::Unauthorized)?;
        let mut validation = Validation::new(header.alg);
        validation.leeway = CLOCK_SKEW_SECS;
        validation.validate_nbf = true;
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let claims = jsonwebtoken::decode::<Map<String, Value>>(token, &key, &validation)
            .map_err(|_| Refusal::Unauthorized)?
            .claims;
        let now = branchyard_support::time::now_ms() / 1000;
        let issued = claims
            .get("iat")
            .and_then(Value::as_u64)
            .ok_or(Refusal::Unauthorized)?;
        let expires = claims
            .get("exp")
            .and_then(Value::as_u64)
            .ok_or(Refusal::Unauthorized)?;
        if issued > now.saturating_add(CLOCK_SKEW_SECS) || expires < issued {
            return Err(Refusal::Unauthorized);
        }
        Ok(claims)
    }

    /// The key `kid` from the set at `url`. The set is cached for an hour and
    /// refetched, at most once a minute, when a key id is not in it.
    async fn key(&self, url: &str, kid: &str) -> Result<Jwk, Refusal> {
        let now = branchyard_support::time::now_ms();
        {
            let cache = self.keys.lock_recovering("federated key cache");
            if let Some(cached) = cache.get(url) {
                let age = now.saturating_sub(cached.fetched_ms);
                if let Some(jwk) = cached.keys.find(kid) {
                    if age < KEY_TTL_MS {
                        return Ok(jwk.clone());
                    }
                } else if age < KEY_REFETCH_MS {
                    return Err(Refusal::Unauthorized);
                }
            }
        }
        let body = fetch(url.to_owned(), Vec::new())
            .await
            .map_err(|e| Refusal::Unavailable(format!("could not fetch signing keys: {e}")))?;
        let keys: JwkSet = serde_json::from_slice(&body)
            .map_err(|_| Refusal::Unavailable("the signing key set is not a JWKS".into()))?;
        let found = keys.find(kid).cloned();
        self.keys.lock_recovering("federated key cache").insert(
            url.to_owned(),
            CachedKeys {
                fetched_ms: branchyard_support::time::now_ms(),
                keys,
            },
        );
        found.ok_or(Refusal::Unauthorized)
    }

    /// Transitive group memberships of `email`, cached for five minutes.
    async fn cloud_identity_groups(
        &self,
        base: Option<&str>,
        email: &str,
    ) -> Result<BTreeSet<String>, Refusal> {
        let now = branchyard_support::time::now_ms();
        let cache_key = format!("{}\n{}", base.unwrap_or(""), email.to_ascii_lowercase());
        if let Some(cached) = self
            .groups
            .lock_recovering("federated group cache")
            .get(&cache_key)
        {
            if now.saturating_sub(cached.fetched_ms) < GROUPS_TTL_MS {
                return Ok(cached.groups.clone());
            }
        }
        let unavailable = |why: String| Refusal::Unavailable(format!("group lookup failed: {why}"));
        let access = self.access.clone();
        let token = tokio::task::spawn_blocking(move || access.token())
            .await
            .map_err(|e| unavailable(e.to_string()))?
            .map_err(unavailable)?;
        let base = base.unwrap_or(CLOUD_IDENTITY_URL).trim_end_matches('/');
        let query = format!(
            "member_key_id == '{}' && 'cloudidentity.googleapis.com/groups.discussion_forum' in labels",
            email.replace('\'', "")
        );
        let mut groups = BTreeSet::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_GROUP_PAGES {
            let mut url = format!(
                "{base}/v1/groups/-/memberships:searchTransitiveGroups?query={}",
                http::encode(&query)
            );
            if let Some(next) = &page_token {
                url.push_str(&format!("&pageToken={}", http::encode(next)));
            }
            let body = fetch(url, vec![("Authorization", format!("Bearer {token}"))])
                .await
                .map_err(unavailable)?;
            let page: Value = serde_json::from_slice(&body)
                .map_err(|_| unavailable("the answer was not JSON".into()))?;
            for membership in page
                .get("memberships")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = membership
                    .pointer("/groupKey/id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                {
                    if groups.len() < MAX_GROUPS {
                        groups.insert(id.to_owned());
                    }
                }
            }
            page_token = page
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            if page_token.is_none() {
                break;
            }
        }
        let mut cache = self.groups.lock_recovering("federated group cache");
        if cache.len() >= GROUPS_CACHE_MAX {
            cache.clear();
        }
        cache.insert(
            cache_key,
            CachedGroups {
                fetched_ms: branchyard_support::time::now_ms(),
                groups: groups.clone(),
            },
        );
        Ok(groups)
    }
}

fn string_claim(claims: &Map<String, Value>, name: &str) -> Option<String> {
    claims
        .get(name)?
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
        .map(str::to_owned)
}

/// The groups a claim holds: an array of strings or one string. `None` when
/// the claim is absent, so a token without it can fall back to Cloud Identity.
fn groups_claim(claims: &Map<String, Value>, name: &str) -> Option<BTreeSet<String>> {
    let items: Vec<&str> = match claims.get(name)? {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    Some(
        items
            .into_iter()
            .filter(|g| !g.is_empty() && g.len() <= 256)
            .take(MAX_GROUPS)
            .map(str::to_owned)
            .collect(),
    )
}

fn binding_matches(rule: &str, email: Option<&str>, groups: &BTreeSet<String>) -> bool {
    if let Some(group) = rule.strip_prefix("group:") {
        return groups.iter().any(|g| g.eq_ignore_ascii_case(group));
    }
    let Some(email) = email else { return false };
    match rule.strip_prefix("domain:") {
        Some(domain) => email
            .rsplit_once('@')
            .is_some_and(|(_, d)| d.eq_ignore_ascii_case(domain)),
        None => email.eq_ignore_ascii_case(rule),
    }
}

/// The `iss` claim of a token whose signature has not been checked. It only
/// selects which configured issuer verifies the token; it grants nothing.
fn unverified_issuer(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("iss")?.as_str().map(str::to_owned)
}

/// Whether a principal's grants are still no broader than a current binding
/// of its provider allows. Used to recheck captured callers.
pub fn still_allows(
    iap: Option<&IapConfig>,
    issuers: &[OidcIssuerConfig],
    principal: &Principal,
) -> bool {
    let within =
        |tenant: Option<&str>, scopes: Option<&BTreeSet<String>>, bindings: &[IdentityBinding]| {
            bindings.iter().any(|binding| {
                let ceiling = Principal {
                    tenant: binding
                        .tenant
                        .as_deref()
                        .or(tenant)
                        .unwrap_or(DEFAULT_TENANT)
                        .to_owned(),
                    scopes: binding
                        .scopes
                        .as_ref()
                        .or(scopes)
                        .cloned()
                        .unwrap_or_else(|| ["read".to_owned()].into_iter().collect()),
                    repos: binding.repos.clone(),
                    ..principal.clone()
                };
                principal.within_policy(&ceiling)
            })
        };
    match principal.root_origin() {
        Some(AuthenticationOrigin::Iap { .. }) => {
            iap.is_some_and(|c| within(c.tenant.as_deref(), c.scopes.as_ref(), &c.bindings))
        }
        Some(AuthenticationOrigin::Oidc { issuer, .. }) => issuers.iter().any(|c| {
            &c.issuer == issuer && within(c.tenant.as_deref(), c.scopes.as_ref(), &c.bindings)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
