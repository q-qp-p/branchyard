//! A server issues a running turn's gateway token to its sandbox.
//!
//! A sandbox's credential proxy ([`super::sandbox_proxy`]) asks for the
//! turn's token with `POST /v1/turns/token`, naming the turn by its manifest
//! digest, branch and number. [`Yard::issue_turn_token`] decides: the
//! branch must exist, be running that turn, and have been admitted with that
//! manifest, and the caller must be who the sandbox claims to be ([`Proof`]).
//! The token then carries the claims of the turn's *recorded* grant (the
//! person's ceiling applied, as the turn's own token is), never anything the
//! request says, and lives [`super::RENEWED_TTL`] (five minutes) at most.
//! The server route authenticates a Google workload identity and calls this;
//! a per-turn secret is checked here against the hash the engine recorded.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{keys, Gateway, MAX_TTL};
use crate::models::gateway::same;
use crate::state::Record;
use crate::{BranchStatus, Yard};

/// The route that issues a turn's token.
pub const TOKEN_PATH: &str = "/v1/turns/token";

/// The header that carries a per-turn secret, instead of a Google ID token.
pub const SECRET_HEADER: &str = "X-Branchyard-Turn-Secret";

/// What a sandbox asks: the token of this turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRequest {
    /// The digest of the turn manifest the turn was admitted with
    /// (`BRANCHYARD_MANIFEST`, the token's `by_manifest`).
    pub manifest: String,
    /// The branch as tokens name it: `<repo>/<branch>` on a server.
    pub branch: String,
    /// The turn's number.
    pub turn: u32,
}

/// The token issued, and when it lapses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    /// The signed gateway token.
    pub token: String,
    /// When it expires, in seconds since the Unix epoch.
    pub expires_at: u64,
    /// How many seconds it lives from now.
    pub expires_in: u64,
}

/// What a running turn recorded so that its sandbox can prove it holds the
/// turn's secret: the hash of it, and the turn it is for. The secret itself
/// is in the sandbox's proxy only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCredential {
    /// The blake3 hash of the secret, in hex.
    pub secret_blake3: String,
    /// The manifest digest it is bound to.
    pub manifest: String,
    /// The turn it is for.
    pub turn: u32,
}

impl TurnCredential {
    /// A new secret for a turn, and the record of it.
    pub(crate) fn new(manifest: &str, turn: u32) -> Result<(String, TurnCredential), String> {
        let secret = format!(
            "{}{}",
            keys::random_id().map_err(|e| e.to_string())?,
            keys::random_id().map_err(|e| e.to_string())?
        );
        let credential = TurnCredential {
            secret_blake3: hash(&secret),
            manifest: manifest.to_owned(),
            turn,
        };
        Ok((secret, credential))
    }

    fn admits(&self, secret: &str, request: &TokenRequest) -> bool {
        same(&hash(secret), &self.secret_blake3)
            && same(&request.manifest, &self.manifest)
            && request.turn == self.turn
    }
}

fn hash(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// Who asks, as the caller proved it.
#[derive(Clone, Copy, Debug)]
pub enum Proof<'a> {
    /// A verified workload identity (a Google service account the server
    /// lists), restricted to one tenant when the list says so.
    Identity {
        /// The account's email, for the audit line.
        service_account: &'a str,
        /// The tenant the account is for; any when `None`.
        tenant: Option<&'a str>,
    },
    /// The per-turn secret the engine gave the sandbox.
    Secret(&'a str),
}

/// Why a token was not issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    /// The branch is not on this server.
    UnknownTurn,
    /// The manifest digest is not the turn's.
    ManifestMismatch,
    /// The branch is not running a turn.
    NotRunning,
    /// The turn is not the one that is running.
    TurnMismatch,
    /// The turn was granted no connectors.
    NoGrant,
    /// The per-turn secret is not this turn's.
    BadSecret,
    /// The service account is for another tenant.
    TenantMismatch,
    /// This yard has no gateway, or cannot sign.
    Unavailable,
}

impl RefusalKind {
    /// The error code the route answers with.
    pub fn code(self) -> &'static str {
        match self {
            RefusalKind::UnknownTurn => "unknown_turn",
            RefusalKind::ManifestMismatch => "manifest_mismatch",
            RefusalKind::NotRunning => "turn_not_running",
            RefusalKind::TurnMismatch => "turn_mismatch",
            RefusalKind::NoGrant => "no_connector_grant",
            RefusalKind::BadSecret => "invalid_credential",
            RefusalKind::TenantMismatch => "tenant_mismatch",
            RefusalKind::Unavailable => "token_unavailable",
        }
    }
}

/// A refused request, with a message that names the reason and no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Which refusal.
    pub kind: RefusalKind,
    /// What to tell the caller.
    pub message: String,
}

fn refuse<T>(kind: RefusalKind, message: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal {
        kind,
        message: message.into(),
    })
}

impl Yard {
    /// The gateway token of the turn `branch` is running, for a sandbox that
    /// proved itself with `proof`, or why not. See the module's rules.
    pub fn issue_turn_token(
        &self,
        branch: &str,
        request: &TokenRequest,
        proof: Proof<'_>,
    ) -> Result<TokenResponse, Refusal> {
        let Some(gateway) = self.connectors() else {
            return refuse(
                RefusalKind::Unavailable,
                "this repository has no connector gateway",
            );
        };
        let Ok(record) = self.store().read(branch) else {
            return refuse(RefusalKind::UnknownTurn, "no such branch");
        };
        // Who first: nothing below is told to a caller that is not the turn's
        // sandbox, so a wrong secret says no more than a missing branch.
        if let Proof::Secret(secret) = proof {
            let admitted = record
                .credential
                .as_ref()
                .is_some_and(|c| c.admits(secret, request));
            if !admitted {
                return refuse(
                    RefusalKind::BadSecret,
                    "the secret is not this turn's (or the turn has ended)",
                );
            }
        }
        if record.info.status != BranchStatus::Running {
            return refuse(RefusalKind::NotRunning, "the branch is not running a turn");
        }
        if record.info.turns + 1 != request.turn {
            return refuse(
                RefusalKind::TurnMismatch,
                format!("turn {} is not the turn that is running", request.turn),
            );
        }
        let recorded = record.manifest.as_ref().map(|m| m.digest());
        if !recorded
            .as_deref()
            .is_some_and(|digest| same(digest, &request.manifest))
        {
            return refuse(
                RefusalKind::ManifestMismatch,
                "the manifest is not the one this turn was admitted with",
            );
        }
        let tenant = record
            .actor
            .as_ref()
            .map_or(gateway.tenant.as_str(), |a| a.tenant.as_str());
        if let Proof::Identity {
            tenant: Some(allowed),
            ..
        } = proof
        {
            if allowed != tenant {
                return refuse(
                    RefusalKind::TenantMismatch,
                    "the service account is not for this turn's tenant",
                );
            }
        }
        let (scoped, _) = crate::access::scoped(self, &record);
        issue(&gateway, &scoped, request)
    }
}

/// Sign the token of `scoped`'s turn from its recorded grant.
fn issue(
    gateway: &Gateway,
    scoped: &Record,
    request: &TokenRequest,
) -> Result<TokenResponse, Refusal> {
    let grants = scoped
        .provision
        .as_ref()
        .map(|p| p.connectors.clone())
        .unwrap_or_default();
    if grants.is_empty() {
        return refuse(RefusalKind::NoGrant, "the turn was granted no connectors");
    }
    let now = branchyard_support::time::now_ms() / 1000;
    let ttl: Duration = gateway.token_ttl.min(MAX_TTL);
    let expires_at = now + ttl.as_secs().max(1);
    let actor = scoped.actor.clone().unwrap_or_else(|| super::Actor {
        subject: gateway.subject.clone(),
        tenant: gateway.tenant.clone(),
    });
    let scopes = crate::access::TokenScopes::of(scoped);
    let unavailable = |why: String| Refusal {
        kind: RefusalKind::Unavailable,
        message: format!("could not sign the gateway token: {why}"),
    };
    let claims = super::turn_claims(super::TurnToken {
        issuer: &gateway.issuer,
        audience: &gateway.url,
        actor,
        by_branch: gateway.by_branch(&scoped.info.name),
        turn: request.turn,
        window: (now, expires_at),
        grants,
        scopes: &scopes,
        manifest: Some(&request.manifest),
    })
    .map_err(unavailable)?;
    let token = gateway
        .token_signer()
        .and_then(|signer| signer.sign(&claims))
        .map_err(|e| unavailable(e.to_string()))?;
    Ok(TokenResponse {
        token,
        expires_at,
        expires_in: expires_at.saturating_sub(now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(manifest: &str, turn: u32) -> TokenRequest {
        TokenRequest {
            manifest: manifest.into(),
            branch: "app/b".into(),
            turn,
        }
    }

    #[test]
    fn a_secret_admits_only_its_own_turn_and_manifest_and_is_kept_as_a_hash() {
        let (secret, credential) = TurnCredential::new("blake3:aa", 2).unwrap();
        assert!(secret.len() >= 40, "{secret}");
        assert!(!format!("{credential:?}").contains(&secret));
        assert!(credential.admits(&secret, &request("blake3:aa", 2)));
        assert!(!credential.admits(&secret, &request("blake3:bb", 2)));
        assert!(!credential.admits(&secret, &request("blake3:aa", 3)));
        assert!(!credential.admits("not-the-secret", &request("blake3:aa", 2)));
        // Another turn's secret is another secret.
        let (other, _) = TurnCredential::new("blake3:aa", 2).unwrap();
        assert_ne!(other, secret);
        assert!(!credential.admits(&other, &request("blake3:aa", 2)));
    }

    #[test]
    fn every_refusal_has_its_own_code() {
        let kinds = [
            RefusalKind::UnknownTurn,
            RefusalKind::ManifestMismatch,
            RefusalKind::NotRunning,
            RefusalKind::TurnMismatch,
            RefusalKind::NoGrant,
            RefusalKind::BadSecret,
            RefusalKind::TenantMismatch,
            RefusalKind::Unavailable,
        ];
        let codes: std::collections::BTreeSet<_> = kinds.iter().map(|k| k.code()).collect();
        assert_eq!(codes.len(), kinds.len());
    }
}
