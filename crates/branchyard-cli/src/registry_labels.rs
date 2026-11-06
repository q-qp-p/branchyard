//! Reading a sandbox image's labels from its registry, without pulling it:
//! the manifest, then the image configuration blob it names (OCI
//! distribution API v2), through Branchyard's own HTTP client
//! (`branchyard_client::http`). Anonymous access and the registry's own
//! bearer-token handshake (`WWW-Authenticate: Bearer realm=...`) are
//! followed; no credential of the user's is read or sent. See docs/images.md.

use std::collections::BTreeMap;
use std::time::Duration;

use branchyard::images::{ImageLabelSource, ImageRef};
use branchyard_client::http;
use serde_json::Value;

/// The largest manifest or configuration read.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Manifest types asked for, in the order a registry prefers them.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

/// Labels read from the image's registry.
#[derive(Clone, Copy, Debug, Default)]
pub struct RegistryLabels;

/// One answer: status, headers (names lower-cased) and body.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A GET of `url` with `headers`.
fn get(url: &str, headers: &[(&str, String)]) -> Result<Reply, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("{url:?}: {e}"))?;
    let endpoint = http::Endpoint::parse(&parsed.origin().ascii_serialization())?;
    let tls = match endpoint.tls {
        true => Some(http::tls_config(None)?),
        false => None,
    };
    let target = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    let stream = http::connect(&endpoint, tls.as_ref(), Duration::from_secs(30))
        .map_err(|e| format!("{url}: {e}"))?;
    let response = http::send(
        stream,
        &endpoint,
        &http::Request {
            method: "GET",
            target: &target,
            headers,
            body: None,
        },
    )
    .map_err(|e| format!("{url}: {e}"))?;
    let status = response.status;
    let headers = response
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    let body = response
        .read_body(MAX_BODY)
        .map_err(|e| format!("{url}: {e}"))?;
    Ok(Reply {
        status,
        headers,
        body,
    })
}

/// The `key="value"` parameters of a `Bearer` challenge.
fn challenge(header: &str) -> Option<BTreeMap<String, String>> {
    let rest = header.trim().strip_prefix("Bearer ")?;
    let mut out = BTreeMap::new();
    for part in rest.split(',') {
        if let Some((key, value)) = part.trim().split_once('=') {
            out.insert(
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            );
        }
    }
    Some(out)
}

/// A GET that answers a bearer challenge once, and follows one redirect.
fn authorized_get(url: &str, accept: &str) -> Result<Reply, String> {
    let plain = [("Accept", accept.to_owned())];
    let mut reply = get(url, &plain)?;
    if reply.status == 401 {
        let params = reply
            .header("www-authenticate")
            .and_then(challenge)
            .ok_or_else(|| format!("{url} answered 401 without a bearer challenge"))?;
        let realm = params
            .get("realm")
            .ok_or_else(|| format!("{url}: the registry's challenge names no realm"))?;
        let mut token_url = realm.clone();
        let mut joiner = '?';
        for key in ["service", "scope"] {
            if let Some(value) = params.get(key) {
                token_url.push_str(&format!("{joiner}{key}={}", http::encode(value)));
                joiner = '&';
            }
        }
        let granted = get(&token_url, &[("Accept", "application/json".to_owned())])?;
        if granted.status != 200 {
            return Err(format!(
                "{realm} answered {} to the token request",
                granted.status
            ));
        }
        let body: Value = serde_json::from_slice(&granted.body)
            .map_err(|e| format!("{realm} did not answer JSON: {e}"))?;
        let token = body["token"]
            .as_str()
            .or_else(|| body["access_token"].as_str())
            .ok_or_else(|| format!("{realm} answered no token"))?;
        reply = get(
            url,
            &[
                ("Accept", accept.to_owned()),
                ("Authorization", format!("Bearer {token}")),
            ],
        )?;
    }
    if matches!(reply.status, 301 | 302 | 303 | 307 | 308) {
        // A blob is often served from a CDN; its location carries its own
        // authorization, so none of ours goes with the request.
        let location = reply
            .header("location")
            .ok_or_else(|| format!("{url} redirected without a location"))?
            .to_owned();
        reply = get(&location, &plain)?;
    }
    Ok(reply)
}

fn labels_of(config: &Value) -> BTreeMap<String, String> {
    config["config"]["Labels"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

impl ImageLabelSource for RegistryLabels {
    fn labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, String> {
        let image = ImageRef::parse(reference)?;
        let base = format!("{}/v2/{}", image.base_url(), image.repository);
        let manifest_url = format!("{base}/manifests/{}", image.reference);
        let mut reply = authorized_get(&manifest_url, MANIFEST_TYPES)?;
        match reply.status {
            200 => {}
            404 => return Ok(None),
            status => return Err(format!("{manifest_url} answered {status}")),
        }
        let mut manifest: Value = serde_json::from_slice(&reply.body)
            .map_err(|e| format!("{manifest_url} did not answer JSON: {e}"))?;
        // An index lists one manifest per platform: the first Linux one.
        if let Some(entries) = manifest["manifests"].as_array() {
            let chosen = entries
                .iter()
                .find(|m| m["platform"]["os"] == "linux")
                .or_else(|| entries.first())
                .and_then(|m| m["digest"].as_str())
                .ok_or_else(|| format!("{manifest_url} lists no manifest"))?
                .to_owned();
            let url = format!("{base}/manifests/{chosen}");
            reply = authorized_get(&url, MANIFEST_TYPES)?;
            if reply.status != 200 {
                return Err(format!("{url} answered {}", reply.status));
            }
            manifest = serde_json::from_slice(&reply.body)
                .map_err(|e| format!("{url} did not answer JSON: {e}"))?;
        }
        let config = manifest["config"]["digest"]
            .as_str()
            .ok_or_else(|| format!("{manifest_url} names no image configuration"))?;
        let config_url = format!("{base}/blobs/{config}");
        let blob = authorized_get(&config_url, "application/json")?;
        if blob.status != 200 {
            return Err(format!("{config_url} answered {}", blob.status));
        }
        let config: Value = serde_json::from_slice(&blob.body)
            .map_err(|e| format!("{config_url} did not answer JSON: {e}"))?;
        Ok(Some(labels_of(&config)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_challenge_is_split_into_its_parameters() {
        let found = challenge(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:me/tc:pull""#,
        )
        .unwrap();
        assert_eq!(found["realm"], "https://auth.example/token");
        assert_eq!(found["service"], "registry.example");
        assert_eq!(found["scope"], "repository:me/tc:pull");
        assert!(challenge("Basic realm=x").is_none());
    }

    #[test]
    fn labels_come_from_the_configs_config() {
        let config = serde_json::json!({"config": {"Labels": {"a": "b", "n": 1}}});
        assert_eq!(labels_of(&config).len(), 1);
        assert!(labels_of(&serde_json::json!({})).is_empty());
    }
}
