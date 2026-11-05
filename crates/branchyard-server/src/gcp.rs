//! Google Cloud credentials for the server: an OAuth access token for the
//! service account the server runs as, from the GCE metadata server (GKE,
//! Compute Engine, Cloud Run). The database login uses it
//! (`database_auth: "gcp_iam"`); Cloud KMS, IAP and Cloud Identity will
//! ask the same type for tokens with their scopes.

use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use branchyard_client::http;
use branchyard_support::LockExt as _;
use serde::Deserialize;

/// The metadata server's host when `GCE_METADATA_HOST` is not set.
const DEFAULT_HOST: &str = "metadata.google.internal";
/// The token is refreshed this long before it expires.
const REFRESH_BEFORE_MS: u64 = 5 * 60 * 1000;
/// How long one request to the metadata server may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Largest token response read.
const MAX_BODY: usize = 64 * 1024;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// A cached access token for the default service account, fetched from
/// the metadata server and renewed five minutes before it expires.
pub struct MetadataToken {
    host: String,
    scopes: Vec<String>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    cached: std::sync::Mutex<Option<(String, u64)>>,
}

impl MetadataToken {
    /// A token source for `scopes` (none: the service account's default
    /// scopes), at `GCE_METADATA_HOST` or `metadata.google.internal`.
    pub fn new(scopes: &[&str]) -> MetadataToken {
        let host = std::env::var("GCE_METADATA_HOST")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned());
        MetadataToken::at(&host, scopes)
    }

    /// A token source at `host` (`name`, `name:port` or `http://name:port`).
    pub fn at(host: &str, scopes: &[&str]) -> MetadataToken {
        MetadataToken::with_clock(host, scopes, Arc::new(branchyard_support::time::now_ms))
    }

    /// As [`MetadataToken::at`], reading time (milliseconds since the Unix
    /// epoch) from `clock`: for tests.
    pub fn with_clock(
        host: &str,
        scopes: &[&str],
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> MetadataToken {
        MetadataToken {
            host: host.trim().to_owned(),
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
            clock,
            cached: std::sync::Mutex::new(None),
        }
    }

    /// A valid access token, fetched when there is none or it expires
    /// within five minutes. Blocks while it fetches.
    pub fn token(&self) -> Result<String, String> {
        let mut cached = self.cached.lock_recovering("gcp token");
        let now = (self.clock)();
        if let Some((token, expires)) = cached.as_ref() {
            if now + REFRESH_BEFORE_MS < *expires {
                return Ok(token.clone());
            }
        }
        let fresh = self.fetch()?;
        let expires = now.saturating_add(fresh.expires_in.saturating_mul(1000));
        *cached = Some((fresh.access_token.clone(), expires));
        Ok(fresh.access_token)
    }

    fn fetch(&self) -> Result<TokenResponse, String> {
        let base = if self.host.contains("://") {
            self.host.clone()
        } else {
            format!("http://{}", self.host)
        };
        let endpoint = http::Endpoint::parse(&base)?;
        let mut target = "/computeMetadata/v1/instance/service-accounts/default/token".to_owned();
        if !self.scopes.is_empty() {
            target.push_str("?scopes=");
            target.push_str(&http::encode(&self.scopes.join(",")));
        }
        let shown = format!("the metadata server at {}:{}", endpoint.host, endpoint.port);
        let stream = http::connect(&endpoint, None, TIMEOUT)
            .map_err(|e| format!("cannot reach {shown}: {e}"))?;
        let response = http::send(
            stream,
            &endpoint,
            &http::Request {
                method: "GET",
                target: &target,
                headers: &[("Metadata-Flavor", "Google".to_owned())],
                body: None,
            },
        )
        .map_err(|e| format!("{shown}: {e}"))?;
        let status = response.status;
        let mut body = Vec::new();
        let mut reader = response.body;
        (&mut reader)
            .take(MAX_BODY as u64)
            .read_to_end(&mut body)
            .map_err(|e| format!("{shown}: {e}"))?;
        if status != 200 {
            let text = String::from_utf8_lossy(&body);
            let text: String = text.trim().chars().take(200).collect();
            return Err(format!("{shown} answered {status} for a token: {text}"));
        }
        serde_json::from_slice(&body)
            .map_err(|e| format!("{shown} answered a token response that is not usable: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::{MockHttp, Response as MockResponse};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn token_body(token: &str, expires_in: u64) -> MockResponse {
        MockResponse::json(
            200,
            &serde_json::json!({"access_token": token, "expires_in": expires_in,
                                 "token_type": "Bearer"}),
        )
    }

    fn clocked(host: &str, scopes: &[&str]) -> (MetadataToken, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let reader = Arc::clone(&now);
        let source = MetadataToken::with_clock(
            host,
            scopes,
            Arc::new(move || reader.load(Ordering::SeqCst)),
        );
        (source, now)
    }

    #[test]
    fn sends_the_flavor_header_and_the_scopes() {
        let mock = MockHttp::start(|_| token_body("ya29.one", 3600));
        let (source, _) = clocked(
            &mock.url(),
            &["https://www.googleapis.com/auth/cloud-platform"],
        );
        assert_eq!(source.token().unwrap(), "ya29.one");
        let seen = mock.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].header("metadata-flavor"), Some("Google"));
        assert!(
            seen[0]
                .path
                .starts_with("/computeMetadata/v1/instance/service-accounts/default/token?scopes="),
            "{}",
            seen[0].path
        );
        mock.finish();
    }

    #[test]
    fn accepts_a_host_without_a_scheme() {
        let mock = MockHttp::start(|_| token_body("ya29.one", 3600));
        let (source, _) = clocked(&mock.addr().to_string(), &[]);
        assert_eq!(source.token().unwrap(), "ya29.one");
        assert_eq!(
            mock.requests()[0].path,
            "/computeMetadata/v1/instance/service-accounts/default/token"
        );
        mock.finish();
    }

    #[test]
    fn caches_until_five_minutes_before_expiry_then_refreshes() {
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        let mock = MockHttp::start(move |_| {
            let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
            token_body(&format!("ya29.{n}"), 3600)
        });
        let (source, now) = clocked(&mock.url(), &[]);
        assert_eq!(source.token().unwrap(), "ya29.1");
        // 54 minutes later: still more than five minutes left.
        now.fetch_add(54 * 60 * 1000, Ordering::SeqCst);
        assert_eq!(source.token().unwrap(), "ya29.1");
        assert_eq!(mock.requests().len(), 1);
        // 56 minutes after the fetch: inside the five-minute margin.
        now.fetch_add(2 * 60 * 1000, Ordering::SeqCst);
        assert_eq!(source.token().unwrap(), "ya29.2");
        assert_eq!(mock.requests().len(), 2);
        mock.finish();
    }

    #[test]
    fn an_error_status_is_reported_with_its_text_and_not_cached() {
        let mock = MockHttp::start(|_| MockResponse::new(403, "no service account"));
        let (source, _) = clocked(&mock.url(), &[]);
        let error = source.token().unwrap_err();
        assert!(error.contains("answered 403"), "{error}");
        assert!(error.contains("no service account"), "{error}");
        assert!(source.token().is_err());
        assert_eq!(mock.requests().len(), 2);
        mock.finish();
    }

    #[test]
    fn a_body_that_is_not_a_token_is_reported() {
        let mock = MockHttp::start(|_| MockResponse::new(200, "<html>"));
        let (source, _) = clocked(&mock.url(), &[]);
        let error = source.token().unwrap_err();
        assert!(error.contains("not usable"), "{error}");
        mock.finish();
    }

    #[test]
    fn an_unreachable_server_is_reported() {
        let (source, _) = clocked("127.0.0.1:1", &[]);
        let error = source.token().unwrap_err();
        assert!(
            error.contains("cannot reach the metadata server"),
            "{error}"
        );
    }
}
