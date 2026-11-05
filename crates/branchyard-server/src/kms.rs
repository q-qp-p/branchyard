//! Signing gateway tokens with Cloud KMS.
//!
//! The private key never leaves KMS: each token's signing input goes to
//! `cryptoKeyVersions/V:asymmetricSign` on an `EC_SIGN_ED25519` key version
//! (Ed25519 signs the data itself, not a digest), and the public keys come
//! from `cryptoKeyVersions/V/publicKey`, which this module turns into the
//! JWKS served at `GET /.well-known/jwks.json`. The first configured key
//! version signs; every one is published, so a rotation adds the new
//! version first and drops the old one after its tokens expire. Requests
//! carry the server's service-account token from [`crate::gcp::MetadataToken`]
//! (scope `cloud-platform`). See `docs/server.md#signing-with-cloud-kms`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD as B64_STD, URL_SAFE_NO_PAD as B64};
use base64::Engine as _;
use branchyard::connectors::{jws_finish, jws_input, Claims, TokenSigner};
use branchyard::Error;
use branchyard_client::http;
use branchyard_support::LockExt as _;
use serde_json::{json, Value};

/// The KMS API's address when none is configured.
pub const DEFAULT_BASE_URL: &str = "https://cloudkms.googleapis.com";
/// The OAuth scope the requests need.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// How long one KMS request may take.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Largest response read.
const MAX_BODY: usize = 64 * 1024;
/// The DER prefix of an Ed25519 `SubjectPublicKeyInfo`; the 32-byte public
/// key follows.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Where a request's access token comes from.
pub type TokenSource = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// One KMS key version: its resource name and the `kid` it signs under.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyVersion {
    name: String,
    kid: String,
}

impl KeyVersion {
    /// Check `name` is `projects/P/locations/L/keyRings/R/cryptoKeys/K/
    /// cryptoKeyVersions/V` and derive the `kid`: `kms-R-K-V`.
    fn parse(name: &str) -> Result<KeyVersion, String> {
        let parts: Vec<&str> = name.split('/').collect();
        let shape = [
            "projects",
            "locations",
            "keyRings",
            "cryptoKeys",
            "cryptoKeyVersions",
        ];
        let well_formed = parts.len() == 10
            && shape
                .iter()
                .enumerate()
                .all(|(i, label)| parts[2 * i] == *label)
            && parts.iter().skip(1).step_by(2).all(|p| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            });
        if !well_formed {
            return Err(format!(
                "{name:?} is not a Cloud KMS key version name (projects/P/locations/L/\
                 keyRings/R/cryptoKeys/K/cryptoKeyVersions/V)"
            ));
        }
        Ok(KeyVersion {
            name: name.to_owned(),
            kid: format!("kms-{}-{}-{}", parts[5], parts[7], parts[9]),
        })
    }
}

/// Cloud KMS as a [`TokenSigner`].
pub struct KmsSigner {
    base_url: String,
    versions: Vec<KeyVersion>,
    token: TokenSource,
    jwks: Mutex<Option<Value>>,
}

impl std::fmt::Debug for KmsSigner {
    /// Key version names only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KmsSigner")
            .field(
                "versions",
                &self.versions.iter().map(|v| &v.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Check the key version names of `connectors.signing`: well formed, at
/// least one, none twice.
pub fn check_versions(names: &[String]) -> Result<(), String> {
    if names.is_empty() {
        return Err("connectors.signing.kms_key_versions names no key version".into());
    }
    let mut kids = BTreeSet::new();
    for name in names {
        let version = KeyVersion::parse(name).map_err(|e| format!("connectors.signing: {e}"))?;
        if !kids.insert(version.kid) {
            return Err(format!(
                "connectors.signing names the key version {name} twice"
            ));
        }
    }
    Ok(())
}

impl KmsSigner {
    /// A signer for `key_versions` (the first signs), reaching KMS at
    /// `base_url` (default [`DEFAULT_BASE_URL`]) with tokens from `token`.
    pub fn new(
        base_url: Option<&str>,
        key_versions: &[String],
        token: TokenSource,
    ) -> Result<KmsSigner, String> {
        check_versions(key_versions)?;
        Ok(KmsSigner {
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_owned(),
            versions: key_versions
                .iter()
                .map(|n| KeyVersion::parse(n))
                .collect::<Result<_, _>>()?,
            token,
            jwks: Mutex::new(None),
        })
    }

    /// As [`KmsSigner::new`], with the access token of the service account
    /// this process runs as, from the metadata server.
    pub fn with_metadata_token(
        base_url: Option<&str>,
        key_versions: &[String],
    ) -> Result<KmsSigner, String> {
        let source = crate::gcp::MetadataToken::new(&[SCOPE]);
        KmsSigner::new(base_url, key_versions, Arc::new(move || source.token()))
    }

    /// One call to KMS: `method` on `path` with a JSON `body`, and the JSON
    /// answer.
    fn call(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> Result<Value, String> {
        let endpoint = http::Endpoint::parse(&self.base_url)?;
        let tls = if endpoint.tls {
            Some(http::tls_config(None)?)
        } else {
            None
        };
        let shown = format!("Cloud KMS at {}:{}", endpoint.host, endpoint.port);
        let token = (self.token)().map_err(|e| format!("no access token for {shown}: {e}"))?;
        let stream = http::connect(&endpoint, tls.as_ref(), TIMEOUT)
            .map_err(|e| format!("cannot reach {shown}: {e}"))?;
        let target = format!("{}{path}", endpoint.prefix);
        let mut headers = vec![
            ("Authorization", format!("Bearer {token}")),
            ("Accept", "application/json".to_owned()),
        ];
        if body.is_some() {
            headers.push(("Content-Type", "application/json".to_owned()));
        }
        let response = http::send(
            stream,
            &endpoint,
            &http::Request {
                method,
                target: &target,
                headers: &headers,
                body: body.as_deref(),
            },
        )
        .map_err(|e| format!("{shown}: {e}"))?;
        let status = response.status;
        let bytes = response
            .read_body(MAX_BODY)
            .map_err(|e| format!("{shown}: {e}"))?;
        let answer: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let why = match answer.pointer("/error/message").and_then(Value::as_str) {
                Some(message) => message.to_owned(),
                None => String::from_utf8_lossy(&bytes)
                    .trim()
                    .chars()
                    .take(200)
                    .collect(),
            };
            return Err(format!("{shown} answered {status}: {why}"));
        }
        if answer.is_null() {
            return Err(format!("{shown} answered something that is not JSON"));
        }
        Ok(answer)
    }

    fn sign_input(&self, version: &KeyVersion, input: &str) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(&json!({ "data": B64_STD.encode(input) }))
            .map_err(|e| e.to_string())?;
        let answer = self.call(
            "POST",
            &format!("/v1/{}:asymmetricSign", version.name),
            Some(body),
        )?;
        let signature = answer
            .get("signature")
            .and_then(Value::as_str)
            .ok_or("Cloud KMS answered a signature response with no signature")?;
        let bytes = B64_STD
            .decode(signature)
            .map_err(|e| format!("Cloud KMS answered a signature that is not base64: {e}"))?;
        if bytes.len() != 64 {
            return Err(format!(
                "Cloud KMS answered a {}-byte signature; an Ed25519 signature has 64 bytes \
                 (is {} an EC_SIGN_ED25519 key?)",
                bytes.len(),
                version.name
            ));
        }
        Ok(bytes)
    }

    fn public_jwk(&self, version: &KeyVersion) -> Result<Value, String> {
        let answer = self.call("GET", &format!("/v1/{}/publicKey", version.name), None)?;
        if let Some(algorithm) = answer.get("algorithm").and_then(Value::as_str) {
            if algorithm != "EC_SIGN_ED25519" {
                return Err(format!(
                    "{} is a {algorithm} key; gateway tokens need EC_SIGN_ED25519",
                    version.name
                ));
            }
        }
        let pem = answer
            .get("pem")
            .and_then(Value::as_str)
            .ok_or("Cloud KMS answered a public key response with no pem")?;
        let x = ed25519_from_pem(pem).map_err(|e| format!("{}: {e}", version.name))?;
        Ok(json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": version.kid,
            "use": "sig",
            "alg": "EdDSA",
            "x": B64.encode(x),
        }))
    }
}

/// The 32-byte Ed25519 public key in a PEM `PUBLIC KEY`.
fn ed25519_from_pem(pem: &str) -> Result<[u8; 32], String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("-----"))
        .collect();
    let der = B64_STD
        .decode(body)
        .map_err(|e| format!("the public key PEM is not base64: {e}"))?;
    let key = der
        .strip_prefix(&ED25519_SPKI_PREFIX[..])
        .ok_or("the public key is not an Ed25519 SubjectPublicKeyInfo")?;
    key.try_into()
        .map_err(|_| "the Ed25519 public key is not 32 bytes".to_owned())
}

impl TokenSigner for KmsSigner {
    fn sign(&self, claims: &Claims) -> Result<String, Error> {
        let version = &self.versions[0];
        let input = jws_input(&version.kid, claims)?;
        let signature = self
            .sign_input(version, &input)
            .map_err(|e| Error::State(format!("could not sign with Cloud KMS: {e}")))?;
        Ok(jws_finish(&input, &signature))
    }

    /// The public keys of every key version, fetched once and kept.
    fn jwks(&self) -> Result<Value, Error> {
        let mut cached = self.jwks.lock_recovering("kms jwks");
        if let Some(set) = cached.as_ref() {
            return Ok(set.clone());
        }
        let keys = self
            .versions
            .iter()
            .map(|v| self.public_jwk(v))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::State(format!("could not read the Cloud KMS public keys: {e}")))?;
        let set = json!({ "keys": keys });
        *cached = Some(set.clone());
        Ok(set)
    }

    fn kid(&self) -> Result<String, Error> {
        Ok(self.versions[0].kid.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::connectors::keys::verify;
    use branchyard::connectors::GrantEntry;
    use branchyard_testkit::{MockHttp, Request, Response as MockResponse};
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use std::collections::BTreeMap;

    const V1: &str =
        "projects/p/locations/global/keyRings/ring/cryptoKeys/gateway/cryptoKeyVersions/1";
    const V2: &str =
        "projects/p/locations/global/keyRings/ring/cryptoKeys/gateway/cryptoKeyVersions/2";

    fn pair(seed: u8) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
    }

    fn pem(pair: &Ed25519KeyPair) -> String {
        let mut der = ED25519_SPKI_PREFIX.to_vec();
        der.extend_from_slice(pair.public_key().as_ref());
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            B64_STD.encode(der)
        )
    }

    /// A fake KMS holding one Ed25519 key per version name.
    fn fake_kms(keys: BTreeMap<String, u8>) -> MockHttp {
        MockHttp::start(move |request: &Request| {
            let path = request.path.as_str();
            let version = |suffix: &str| {
                path.strip_prefix("/v1/")
                    .and_then(|p| p.strip_suffix(suffix))
                    .and_then(|name| keys.get(name))
                    .map(|seed| pair(*seed))
            };
            if request.method == "POST" {
                let Some(key) = version(":asymmetricSign") else {
                    return MockResponse::json(404, &json!({"error": {"message": "no such key"}}));
                };
                let data = request.json()["data"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let bytes = B64_STD.decode(data).unwrap();
                return MockResponse::json(
                    200,
                    &json!({"signature": B64_STD.encode(key.sign(&bytes).as_ref())}),
                );
            }
            match version("/publicKey") {
                Some(key) => MockResponse::json(
                    200,
                    &json!({"pem": pem(&key), "algorithm": "EC_SIGN_ED25519"}),
                ),
                None => MockResponse::json(403, &json!({"error": {"message": "denied"}})),
            }
        })
    }

    fn signer(mock: &MockHttp, versions: &[&str]) -> KmsSigner {
        let names: Vec<String> = versions.iter().map(|v| (*v).to_owned()).collect();
        KmsSigner::new(
            Some(&mock.url()),
            &names,
            Arc::new(|| Ok("ya29.test".to_owned())),
        )
        .unwrap()
    }

    fn claims() -> Claims {
        Claims {
            iss: "https://by.example".into(),
            aud: "https://gw.example/mcp".into(),
            sub: "server".into(),
            iat: 1_700_000_000,
            exp: 1_700_003_600,
            jti: "j".into(),
            by_tenant: "t".into(),
            by_branch: "app/b".into(),
            by_turn: "1".into(),
            by_grants: vec![GrantEntry::parse("github").unwrap()],
            by_purpose: None,
            by_models: None,
            by_network: None,
            by_delegation: None,
            by_manifest: None,
        }
    }

    #[test]
    fn signs_the_input_not_a_digest_and_the_token_verifies_against_the_jwks() {
        let mock = fake_kms(BTreeMap::from([(V1.to_owned(), 7)]));
        let kms = signer(&mock, &[V1]);
        let token = kms.sign(&claims()).unwrap();
        let sent = mock.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].path, format!("/v1/{V1}:asymmetricSign"));
        assert_eq!(sent[0].header("authorization"), Some("Bearer ya29.test"));
        assert_eq!(sent[0].header("content-type"), Some("application/json"));
        // `data` is the standard base64 of the JWS signing input itself.
        let data = B64_STD
            .decode(sent[0].json()["data"].as_str().unwrap())
            .unwrap();
        let input = String::from_utf8(data).unwrap();
        assert!(token.starts_with(&format!("{input}.")), "{token}");
        assert_eq!(sent[0].json().as_object().unwrap().len(), 1);

        let jwks = kms.jwks().unwrap();
        let verified = verify(&jwks, &token).unwrap();
        assert_eq!(verified["iss"], "https://by.example");
        assert_eq!(verified["by_grants"][0]["connector"], "github");
        assert_eq!(kms.kid().unwrap(), "kms-ring-gateway-1");
        let header: Value =
            serde_json::from_slice(&B64.decode(token.split('.').next().unwrap()).unwrap()).unwrap();
        assert_eq!(header["kid"], "kms-ring-gateway-1");
        assert_eq!(header["alg"], "EdDSA");
        // Another key's signature does not verify.
        let other = fake_kms(BTreeMap::from([(V1.to_owned(), 9)]));
        let stranger = signer(&other, &[V1]);
        assert!(verify(&jwks, &stranger.sign(&claims()).unwrap()).is_err());
        mock.finish();
        other.finish();
    }

    #[test]
    fn publishes_every_version_fetches_once_and_signs_with_the_first() {
        let mock = fake_kms(BTreeMap::from([(V1.to_owned(), 1), (V2.to_owned(), 2)]));
        let kms = signer(&mock, &[V2, V1]);
        let jwks = kms.jwks().unwrap();
        let keys = jwks["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["kid"], "kms-ring-gateway-2");
        assert_eq!(keys[1]["kid"], "kms-ring-gateway-1");
        assert_eq!(keys[0]["kty"], "OKP");
        assert_eq!(keys[0]["crv"], "Ed25519");
        assert_eq!(
            keys[0]["x"],
            B64.encode(pair(2).public_key().as_ref()),
            "the JWK carries the PEM's key"
        );
        assert_eq!(mock.requests().len(), 2);
        // Cached: the second read makes no request.
        assert_eq!(kms.jwks().unwrap(), jwks);
        assert_eq!(mock.requests().len(), 2);
        let token = kms.sign(&claims()).unwrap();
        assert_eq!(kms.kid().unwrap(), "kms-ring-gateway-2");
        assert!(verify(&jwks, &token).is_ok());
        let last = mock.requests().pop().unwrap();
        assert_eq!(last.path, format!("/v1/{V2}:asymmetricSign"));
        mock.finish();
    }

    #[test]
    fn errors_say_what_kms_answered() {
        let mock = fake_kms(BTreeMap::new());
        let kms = signer(&mock, &[V1]);
        let error = kms.sign(&claims()).unwrap_err().to_string();
        assert!(
            error.contains("404") && error.contains("no such key"),
            "{error}"
        );
        let error = kms.jwks().unwrap_err().to_string();
        assert!(error.contains("403") && error.contains("denied"), "{error}");
        mock.finish();

        let wrong = MockHttp::start(|request: &Request| {
            if request.method == "POST" {
                MockResponse::json(200, &json!({"signature": B64_STD.encode([1u8; 32])}))
            } else {
                MockResponse::json(
                    200,
                    &json!({"pem": "x", "algorithm": "RSA_SIGN_PKCS1_2048_SHA256"}),
                )
            }
        });
        let kms = signer(&wrong, &[V1]);
        let error = kms.sign(&claims()).unwrap_err().to_string();
        assert!(error.contains("64 bytes"), "{error}");
        let error = kms.jwks().unwrap_err().to_string();
        assert!(error.contains("EC_SIGN_ED25519"), "{error}");
        wrong.finish();

        let garbled = MockHttp::start(|_| MockResponse::json(200, &json!({"pem": "not a key"})));
        let error = signer(&garbled, &[V1]).jwks().unwrap_err().to_string();
        assert!(
            error.contains("PEM") || error.contains("SubjectPublicKeyInfo"),
            "{error}"
        );
        garbled.finish();

        // No token: the call is not made.
        let mock = fake_kms(BTreeMap::new());
        let kms = KmsSigner::new(
            Some(&mock.url()),
            &[V1.to_owned()],
            Arc::new(|| Err("no metadata server".to_owned())),
        )
        .unwrap();
        let error = kms.sign(&claims()).unwrap_err().to_string();
        assert!(error.contains("no metadata server"), "{error}");
        assert!(mock.requests().is_empty());
        mock.finish();
    }

    #[test]
    fn key_version_names_are_checked() {
        assert!(check_versions(&[]).is_err());
        assert!(check_versions(&[V1.to_owned(), V2.to_owned()]).is_ok());
        let twice = check_versions(&[V1.to_owned(), V1.to_owned()]).unwrap_err();
        assert!(twice.contains("twice"), "{twice}");
        for bad in [
            "projects/p/locations/l/keyRings/r/cryptoKeys/k",
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/",
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1/x",
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1?x=y",
            "folders/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1",
        ] {
            assert!(check_versions(&[bad.to_owned()]).is_err(), "{bad}");
        }
    }
}
