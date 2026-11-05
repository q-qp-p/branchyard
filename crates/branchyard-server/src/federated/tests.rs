use super::*;
use branchyard_testkit::{MockHttp, Response};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

const IAP_AUDIENCE: &str = "/projects/123456/global/backendServices/789";
const OIDC_ISSUER: &str = "https://sts.example.test";
const OIDC_AUDIENCE: &str =
    "//iam.googleapis.com/locations/global/workforcePools/eng/providers/okta";
const RSA_KID: &str = "fake-google-key";

fn now() -> u64 {
    branchyard_support::time::now_ms() / 1000
}

/// An ES256 key generated for the test, with its JWKS and a signer.
struct EsKey {
    der: Vec<u8>,
    jwks: Value,
}

impl EsKey {
    fn new(kid: &str) -> EsKey {
        let rng = SystemRandom::new();
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .unwrap()
            .as_ref()
            .to_vec();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng).unwrap();
        let point = pair.public_key().as_ref();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let jwks = json!({"keys": [{
            "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": kid,
            "x": b64.encode(&point[1..33]), "y": b64.encode(&point[33..65]),
        }]});
        EsKey { der, jwks }
    }

    fn sign(&self, kid: &str, claims: &Value) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::ES256);
        header.kid = Some(kid.into());
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_ec_der(&self.der),
        )
        .unwrap()
    }
}

fn rsa_sign(kid: &str, claims: &Value) -> String {
    let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
    header.kid = Some(kid.into());
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
        "../../../branchyard-sync/src/auth/testdata/rsa-test-key.pem"
    ))
    .unwrap();
    jsonwebtoken::encode(&header, claims, &key).unwrap()
}

fn rsa_jwks() -> Value {
    serde_json::from_str(include_str!("../testdata/google-jwks.json")).unwrap()
}

fn iap_claims() -> Value {
    json!({
        "iss": IAP_ISSUER, "aud": IAP_AUDIENCE, "sub": "accounts.google.com:1111",
        "email": "alice@example.com", "iat": now(), "exp": now() + 600,
    })
}

fn oidc_claims() -> Value {
    json!({
        "iss": OIDC_ISSUER, "aud": OIDC_AUDIENCE, "sub": "workforce-subject-1",
        "email": "bob@example.com", "groups": ["eng@example.com", "all@example.com"],
        "iat": now(), "exp": now() + 600,
    })
}

fn bindings() -> Vec<IdentityBinding> {
    let binding = |matches: &str, name: &str, tenant: &str, scopes: &[&str]| IdentityBinding {
        matches: matches.into(),
        name: Some(name.into()),
        tenant: Some(tenant.into()),
        scopes: Some(scopes.iter().map(|s| s.to_string()).collect()),
        repos: None,
    };
    vec![
        binding(
            "carol@example.com",
            "carol-admin",
            "ops",
            &["read", "admin"],
        ),
        binding(
            "group:eng@example.com",
            "engineer",
            "engineering",
            &["read", "run"],
        ),
        binding("domain:example.com", "staff", "staff", &["read"]),
    ]
}

struct FakeToken(AtomicUsize);

impl AccessToken for FakeToken {
    fn token(&self) -> Result<String, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("meta-token".into())
    }
}

fn iap_config(keys: &MockHttp) -> IapConfig {
    IapConfig {
        audience: IAP_AUDIENCE.into(),
        keys_url: Some(keys.url()),
        tenant: None,
        scopes: None,
        bindings: bindings(),
        groups_from: None,
        groups_url: None,
    }
}

fn oidc_config(keys: &MockHttp) -> OidcIssuerConfig {
    OidcIssuerConfig {
        issuer: OIDC_ISSUER.into(),
        audience: OIDC_AUDIENCE.into(),
        jwks_url: keys.url(),
        subject_claim: None,
        email_claim: None,
        groups_claim: None,
        tenant: None,
        scopes: None,
        bindings: bindings(),
        groups_from: None,
        groups_url: None,
    }
}

fn federation(iap: Option<IapConfig>, issuers: Vec<OidcIssuerConfig>) -> Federation {
    Federation::new(iap, issuers, Arc::new(FakeToken(AtomicUsize::new(0))))
}

fn serve_keys(jwks: Value) -> MockHttp {
    MockHttp::start(move |_| Response::json(200, &jwks))
}

#[tokio::test]
async fn iap_assertion_admits_a_bound_identity() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let fed = federation(Some(iap_config(&keys)), Vec::new());
    let token = key.sign("iap-1", &iap_claims());
    let principal = fed.verify_iap(Some(&token)).await.unwrap().unwrap();
    // alice matches only the domain binding.
    assert_eq!(principal.name, "staff");
    assert_eq!(principal.tenant, "staff");
    assert!(principal.allows("read") && !principal.allows("run"));
    assert_eq!(
        principal.origin,
        AuthenticationOrigin::Iap {
            email: "alice@example.com".into(),
            sub: "accounts.google.com:1111".into(),
            groups_at_ms: None,
        }
    );
    assert!(principal.connector_subject().starts_with("iap:"));
    let serialized = serde_json::to_string(&principal).unwrap();
    assert!(!serialized.contains(&token));
    assert_eq!(
        serde_json::from_str::<Principal>(&serialized).unwrap(),
        principal
    );
}

#[tokio::test]
async fn iap_first_matching_binding_wins_and_email_beats_domain() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let fed = federation(Some(iap_config(&keys)), Vec::new());
    let mut claims = iap_claims();
    claims["email"] = json!("Carol@Example.com");
    let principal = fed
        .verify_iap(Some(&key.sign("iap-1", &claims)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(principal.name, "carol-admin");
    assert!(principal.allows("admin"));
}

#[tokio::test]
async fn iap_refuses_every_bad_assertion() {
    let key = EsKey::new("iap-1");
    let other = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let fed = federation(Some(iap_config(&keys)), Vec::new());
    let cases: Vec<(&str, String, Refusal)> = vec![
        (
            "wrong audience",
            {
                let mut c = iap_claims();
                c["aud"] = json!("/projects/123456/global/backendServices/other");
                key.sign("iap-1", &c)
            },
            Refusal::Unauthorized,
        ),
        (
            "expired",
            {
                let mut c = iap_claims();
                c["iat"] = json!(now() - 7200);
                c["exp"] = json!(now() - 3600);
                key.sign("iap-1", &c)
            },
            Refusal::Unauthorized,
        ),
        (
            "issued in the future",
            {
                let mut c = iap_claims();
                c["iat"] = json!(now() + 3600);
                c["exp"] = json!(now() + 7200);
                key.sign("iap-1", &c)
            },
            Refusal::Unauthorized,
        ),
        (
            "missing iat",
            {
                let mut c = iap_claims();
                c.as_object_mut().unwrap().remove("iat");
                key.sign("iap-1", &c)
            },
            Refusal::Unauthorized,
        ),
        (
            "wrong issuer",
            {
                let mut c = iap_claims();
                c["iss"] = json!("https://accounts.google.com");
                key.sign("iap-1", &c)
            },
            Refusal::Unauthorized,
        ),
        (
            "unknown kid",
            key.sign("rotated", &iap_claims()),
            Refusal::Unauthorized,
        ),
        (
            "signed by another key",
            other.sign("iap-1", &iap_claims()),
            Refusal::Unauthorized,
        ),
        ("not a jwt", "opaque".into(), Refusal::Unauthorized),
        (
            "no email",
            {
                let mut c = iap_claims();
                c.as_object_mut().unwrap().remove("email");
                key.sign("iap-1", &c)
            },
            Refusal::Unauthorized,
        ),
        (
            "no binding",
            {
                let mut c = iap_claims();
                c["email"] = json!("mallory@elsewhere.test");
                key.sign("iap-1", &c)
            },
            Refusal::NotBound,
        ),
    ];
    for (what, token, expected) in cases {
        let got = fed.verify_iap(Some(&token)).await.unwrap().unwrap_err();
        assert_eq!(got, expected, "{what}");
    }
    // An RS256 assertion is not an IAP assertion.
    let rsa = rsa_sign(RSA_KID, &iap_claims());
    assert_eq!(
        fed.verify_iap(Some(&rsa)).await.unwrap().unwrap_err(),
        Refusal::Unauthorized
    );
}

#[tokio::test]
async fn iap_header_is_ignored_when_iap_is_not_configured() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let fed = federation(None, vec![oidc_config(&keys)]);
    let token = key.sign("iap-1", &iap_claims());
    assert!(fed.verify_iap(Some(&token)).await.is_none());
    assert!(!fed.iap_enabled());
    let configured = federation(Some(iap_config(&keys)), Vec::new());
    assert!(configured.verify_iap(None).await.is_none());
    assert!(keys.requests().is_empty(), "no key fetch without a header");
}

#[tokio::test]
async fn signing_keys_are_cached_and_unknown_kids_do_not_hammer_the_issuer() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let fed = federation(Some(iap_config(&keys)), Vec::new());
    for _ in 0..3 {
        fed.verify_iap(Some(&key.sign("iap-1", &iap_claims())))
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(keys.requests().len(), 1);
    for _ in 0..3 {
        fed.verify_iap(Some(&key.sign("rotated", &iap_claims())))
            .await
            .unwrap()
            .unwrap_err();
    }
    assert_eq!(
        keys.requests().len(),
        1,
        "an unknown kid refetches at most once a minute"
    );
}

#[tokio::test]
async fn an_unreachable_key_set_is_unavailable_not_unauthorized() {
    let key = EsKey::new("iap-1");
    let keys = MockHttp::start(|_| Response::new(500, "broken"));
    let fed = federation(Some(iap_config(&keys)), Vec::new());
    let refusal = fed
        .verify_iap(Some(&key.sign("iap-1", &iap_claims())))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(refusal, Refusal::Unavailable(_)), "{refusal:?}");
}

#[tokio::test]
async fn oidc_admits_a_bound_identity_with_groups_from_the_claim() {
    let keys = serve_keys(rsa_jwks());
    let fed = federation(None, vec![oidc_config(&keys)]);
    let token = rsa_sign(RSA_KID, &oidc_claims());
    let principal = fed
        .verify_oidc(Some(&format!("Bearer {token}")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(principal.name, "engineer");
    assert_eq!(principal.tenant, "engineering");
    assert_eq!(
        principal.groups,
        ["all@example.com".to_owned(), "eng@example.com".to_owned()]
            .into_iter()
            .collect()
    );
    assert_eq!(
        principal.origin,
        AuthenticationOrigin::Oidc {
            issuer: OIDC_ISSUER.into(),
            sub: "workforce-subject-1".into(),
            email: Some("bob@example.com".into()),
            groups_at_ms: None,
        }
    );
    let subject = principal.connector_subject();
    assert!(subject.starts_with("oidc:"));
    let mut other_issuer = principal.clone();
    other_issuer.origin = AuthenticationOrigin::Oidc {
        issuer: "https://other.test".into(),
        sub: "workforce-subject-1".into(),
        email: None,
        groups_at_ms: None,
    };
    assert_ne!(subject, other_issuer.connector_subject());
    let mut iap = principal.clone();
    iap.origin = AuthenticationOrigin::Iap {
        email: "bob@example.com".into(),
        sub: "workforce-subject-1".into(),
        groups_at_ms: None,
    };
    assert_ne!(subject, iap.connector_subject());
}

#[tokio::test]
async fn oidc_reads_configured_claims() {
    let keys = serve_keys(rsa_jwks());
    let mut config = oidc_config(&keys);
    config.subject_claim = Some("oid".into());
    config.email_claim = Some("upn".into());
    config.groups_claim = Some("roles".into());
    let fed = federation(None, vec![config]);
    let mut claims = oidc_claims();
    let map = claims.as_object_mut().unwrap();
    map.remove("groups");
    map.remove("email");
    map.insert("oid".into(), json!("object-7"));
    map.insert("upn".into(), json!("dora@example.com"));
    map.insert("roles".into(), json!("eng@example.com"));
    let principal = fed
        .verify_oidc(Some(&format!("Bearer {}", rsa_sign(RSA_KID, &claims))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(principal.name, "engineer");
    assert!(matches!(
        principal.origin,
        AuthenticationOrigin::Oidc { ref sub, .. } if sub == "object-7"
    ));
}

#[tokio::test]
async fn oidc_ignores_an_unverified_email() {
    let keys = serve_keys(rsa_jwks());
    let fed = federation(None, vec![oidc_config(&keys)]);
    let mut claims = oidc_claims();
    claims.as_object_mut().unwrap().remove("groups");
    claims["email_verified"] = json!(false);
    let got = fed
        .verify_oidc(Some(&format!("Bearer {}", rsa_sign(RSA_KID, &claims))))
        .await
        .unwrap();
    assert_eq!(got.unwrap_err(), Refusal::NotBound);
}

#[tokio::test]
async fn oidc_refuses_every_bad_token_and_ignores_other_issuers() {
    let keys = serve_keys(rsa_jwks());
    let fed = federation(None, vec![oidc_config(&keys)]);
    let bearer = |claims: &Value, kid: &str| format!("Bearer {}", rsa_sign(kid, claims));
    let mut wrong_aud = oidc_claims();
    wrong_aud["aud"] = json!("someone-else");
    let mut expired = oidc_claims();
    expired["iat"] = json!(now() - 7200);
    expired["exp"] = json!(now() - 3600);
    let mut unbound = oidc_claims();
    unbound["email"] = json!("mallory@elsewhere.test");
    unbound.as_object_mut().unwrap().remove("groups");
    for (what, header, expected) in [
        (
            "wrong audience",
            bearer(&wrong_aud, RSA_KID),
            Refusal::Unauthorized,
        ),
        ("expired", bearer(&expired, RSA_KID), Refusal::Unauthorized),
        (
            "unknown kid",
            bearer(&oidc_claims(), "rotated"),
            Refusal::Unauthorized,
        ),
        ("no binding", bearer(&unbound, RSA_KID), Refusal::NotBound),
    ] {
        let got = fed.verify_oidc(Some(&header)).await.unwrap().unwrap_err();
        assert_eq!(got, expected, "{what}");
    }
    // Wrong issuer: the token is somebody else's, so it is left for the other
    // credential kinds rather than refused here.
    let mut foreign = oidc_claims();
    foreign["iss"] = json!("https://attacker.test");
    assert!(fed
        .verify_oidc(Some(&bearer(&foreign, RSA_KID)))
        .await
        .is_none());
    // An ES256 token for this issuer is accepted only with a matching key.
    let es = EsKey::new(RSA_KID);
    let signed = format!("Bearer {}", es.sign(RSA_KID, &oidc_claims()));
    assert_eq!(
        fed.verify_oidc(Some(&signed)).await.unwrap().unwrap_err(),
        Refusal::Unauthorized
    );
    for opaque in [None, Some("Bearer static-token"), Some("Basic abc")] {
        assert!(fed.verify_oidc(opaque).await.is_none());
    }
}

#[tokio::test]
async fn oidc_accepts_es256_from_its_jwks() {
    let key = EsKey::new("es-1");
    let keys = serve_keys(key.jwks.clone());
    let fed = federation(None, vec![oidc_config(&keys)]);
    let header = format!("Bearer {}", key.sign("es-1", &oidc_claims()));
    let principal = fed.verify_oidc(Some(&header)).await.unwrap().unwrap();
    assert_eq!(principal.name, "engineer");
}

fn identity_server(pages: bool) -> MockHttp {
    MockHttp::start(move |request| {
        if request.header("authorization") != Some("Bearer meta-token")
            || !request.path.contains("searchTransitiveGroups")
        {
            return Response::new(403, "denied");
        }
        let second = request.path.contains("pageToken=page2");
        match (pages, second) {
            (true, false) => Response::json(
                200,
                &json!({"memberships": [{"groupKey": {"id": "all@example.com"}}], "nextPageToken": "page2"}),
            ),
            (true, true) => Response::json(
                200,
                &json!({"memberships": [{"groupKey": {"id": "eng@example.com"}}]}),
            ),
            (false, _) => Response::json(
                200,
                &json!({"memberships": [{"groupKey": {"id": "eng@example.com"}}]}),
            ),
        }
    })
}

#[tokio::test]
async fn groups_come_from_cloud_identity_when_the_token_has_none() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let identity = identity_server(true);
    let mut config = iap_config(&keys);
    config.groups_from = Some(GroupsFrom::CloudIdentity);
    config.groups_url = Some(identity.url());
    let token = Arc::new(FakeToken(AtomicUsize::new(0)));
    let fed = Federation::new(Some(config), Vec::new(), token.clone());
    let assertion = key.sign("iap-1", &iap_claims());
    let principal = fed.verify_iap(Some(&assertion)).await.unwrap().unwrap();
    assert_eq!(principal.name, "engineer");
    assert_eq!(principal.groups.len(), 2, "both pages are read");
    let requests = identity.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].path.contains("alice%40example.com"));
    // A second sign-in inside five minutes hits the cache.
    fed.verify_iap(Some(&assertion)).await.unwrap().unwrap();
    assert_eq!(identity.requests().len(), 2);
    assert_eq!(token.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_groups_claim_beats_cloud_identity() {
    let keys = serve_keys(rsa_jwks());
    let identity = identity_server(false);
    let mut config = oidc_config(&keys);
    config.groups_from = Some(GroupsFrom::CloudIdentity);
    config.groups_url = Some(identity.url());
    let fed = federation(None, vec![config]);
    let header = format!("Bearer {}", rsa_sign(RSA_KID, &oidc_claims()));
    fed.verify_oidc(Some(&header)).await.unwrap().unwrap();
    assert!(identity.requests().is_empty());
}

#[tokio::test]
async fn a_failed_group_lookup_refuses_instead_of_guessing() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let identity = MockHttp::start(|_| Response::new(403, "no"));
    let mut config = iap_config(&keys);
    config.groups_from = Some(GroupsFrom::CloudIdentity);
    config.groups_url = Some(identity.url());
    let fed = federation(Some(config), Vec::new());
    let refusal = fed
        .verify_iap(Some(&key.sign("iap-1", &iap_claims())))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(refusal, Refusal::Unavailable(_)), "{refusal:?}");
}

#[test]
fn bindings_match_by_email_domain_and_group() {
    let groups: BTreeSet<String> = ["Eng@Example.com".to_owned()].into_iter().collect();
    let none = BTreeSet::new();
    assert!(binding_matches("a@b.test", Some("A@B.test"), &none));
    assert!(!binding_matches("a@b.test", Some("a@b.test.evil"), &none));
    assert!(binding_matches("domain:b.test", Some("x@B.test"), &none));
    assert!(!binding_matches(
        "domain:b.test",
        Some("x@sub.b.test"),
        &none
    ));
    assert!(!binding_matches("domain:b.test", None, &none));
    assert!(binding_matches("group:eng@example.com", None, &groups));
    assert!(!binding_matches("group:ops@example.com", None, &groups));
}

#[test]
fn configuration_is_validated() {
    let keys = IapConfig {
        audience: IAP_AUDIENCE.into(),
        keys_url: None,
        tenant: None,
        scopes: None,
        bindings: bindings(),
        groups_from: None,
        groups_url: None,
    };
    validate(Some(&keys), &[]).unwrap();
    let mut bad = keys.clone();
    bad.audience = "https://not-an-iap-audience".into();
    assert!(validate(Some(&bad), &[]).is_err());
    let mut bad = keys.clone();
    bad.bindings.clear();
    assert!(validate(Some(&bad), &[]).is_err());
    let mut bad = keys.clone();
    bad.bindings[0].matches = "carol".into();
    assert!(validate(Some(&bad), &[]).is_err());
    let mut bad = keys.clone();
    bad.bindings[0].matches = "domain:".into();
    assert!(validate(Some(&bad), &[]).is_err());
    let mut bad = keys.clone();
    bad.bindings[0].tenant = Some("a/b".into());
    assert!(validate(Some(&bad), &[]).is_err());
    let issuer = OidcIssuerConfig {
        issuer: OIDC_ISSUER.into(),
        audience: OIDC_AUDIENCE.into(),
        jwks_url: "https://sts.example.test/jwks".into(),
        subject_claim: None,
        email_claim: None,
        groups_claim: None,
        tenant: None,
        scopes: None,
        bindings: bindings(),
        groups_from: None,
        groups_url: None,
    };
    validate(None, std::slice::from_ref(&issuer)).unwrap();
    assert!(validate(None, &[issuer.clone(), issuer.clone()]).is_err());
    let mut bad = issuer;
    bad.jwks_url = "ftp://x".into();
    assert!(validate(None, &[bad]).is_err());
    let parsed: IapConfig = serde_json::from_value(json!({
        "audience": IAP_AUDIENCE, "groups_from": "cloud_identity",
        "bindings": [{"match": "domain:example.com"}]
    }))
    .unwrap();
    assert_eq!(parsed.groups_from, Some(GroupsFrom::CloudIdentity));
    assert!(serde_json::from_value::<IapConfig>(json!({
        "audience": IAP_AUDIENCE, "bindings": [], "surprise": 1
    }))
    .is_err());
}

#[test]
fn static_principals_cannot_take_a_federated_prefix() {
    for name in ["iap:abc", "oidc:abc"] {
        assert!(
            check_principal(&Principal::default_for(name)).is_err(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn captured_callers_are_rechecked_against_current_bindings() {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let config = iap_config(&keys);
    let fed = federation(Some(config.clone()), Vec::new());
    let principal = fed
        .verify_iap(Some(&key.sign("iap-1", &iap_claims())))
        .await
        .unwrap()
        .unwrap();
    assert!(still_allows(Some(&config), &[], &principal));
    assert!(!still_allows(None, &[], &principal));
    let mut narrowed = config.clone();
    narrowed
        .bindings
        .retain(|b| b.matches == "carol@example.com");
    narrowed.bindings[0].scopes = Some(["read".to_owned()].into_iter().collect());
    assert!(!still_allows(Some(&narrowed), &[], &principal));
}

#[tokio::test]
async fn a_recheck_uses_the_binding_that_admitted_the_identity() {
    let keys = serve_keys(rsa_jwks());
    let config = oidc_config(&keys);
    let fed = federation(None, vec![config.clone()]);
    let token = rsa_sign(RSA_KID, &oidc_claims());
    let principal = fed
        .verify_oidc(Some(&format!("Bearer {token}")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(principal.name, "engineer");
    assert!(still_allows(
        None,
        std::slice::from_ref(&config),
        &principal
    ));
    // The group binding that admitted bob is revoked; another binding,
    // for someone else, carries the same tenant and scopes. Bob now falls
    // to the domain binding, which allows less, so his work is refused.
    let mut revoked = config.clone();
    revoked
        .bindings
        .retain(|b| b.matches != "group:eng@example.com");
    revoked.bindings.insert(
        0,
        IdentityBinding {
            matches: "dave@example.com".into(),
            name: Some("engineer".into()),
            tenant: Some("engineering".into()),
            scopes: Some(["read".to_owned(), "run".to_owned()].into_iter().collect()),
            repos: None,
        },
    );
    assert!(!still_allows(None, &[revoked.clone()], &principal));
    // No binding matching at all refuses as well.
    revoked.bindings.retain(|b| b.matches == "dave@example.com");
    assert!(!still_allows(None, &[revoked], &principal));
}

/// A Cloud Identity that reports `eng@example.com` until `removed` is set,
/// then no groups.
fn membership_server(removed: Arc<std::sync::atomic::AtomicBool>) -> MockHttp {
    MockHttp::start(move |request| {
        if request.header("authorization") != Some("Bearer meta-token") {
            return Response::new(403, "denied");
        }
        match removed.load(Ordering::SeqCst) {
            true => Response::json(200, &json!({})),
            false => Response::json(
                200,
                &json!({"memberships": [{"groupKey": {"id": "eng@example.com"}}]}),
            ),
        }
    })
}

/// An IAP-signed-in principal whose groups Cloud Identity gave, with the
/// configuration that admitted it and the server it asked.
async fn engineer_from_cloud_identity(
    removed: &Arc<std::sync::atomic::AtomicBool>,
) -> (IapConfig, MockHttp, Principal) {
    let key = EsKey::new("iap-1");
    let keys = serve_keys(key.jwks.clone());
    let identity = membership_server(removed.clone());
    let mut config = iap_config(&keys);
    config.groups_from = Some(GroupsFrom::CloudIdentity);
    config.groups_url = Some(identity.url());
    let fed = federation(Some(config.clone()), Vec::new());
    let principal = fed
        .verify_iap(Some(&key.sign("iap-1", &iap_claims())))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(principal.name, "engineer");
    // `keys` must outlive the sign-in only.
    drop(keys);
    (config, identity, principal)
}

fn groups_at(principal: &Principal) -> Option<u64> {
    match &principal.origin {
        AuthenticationOrigin::Iap { groups_at_ms, .. }
        | AuthenticationOrigin::Oidc { groups_at_ms, .. } => *groups_at_ms,
        _ => None,
    }
}

fn captured_at(principal: &Principal, at: Option<u64>) -> Principal {
    let mut out = principal.clone();
    match &mut out.origin {
        AuthenticationOrigin::Iap { groups_at_ms, .. }
        | AuthenticationOrigin::Oidc { groups_at_ms, .. } => *groups_at_ms = at,
        _ => {}
    }
    out
}

#[tokio::test]
async fn sign_in_records_when_cloud_identity_gave_the_groups() {
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (_, _, principal) = engineer_from_cloud_identity(&removed).await;
    let at = groups_at(&principal).expect("the lookup time is recorded");
    assert!(at <= branchyard_support::time::now_ms());
    // It survives the durable round trip a queued operation makes.
    let back: Principal =
        serde_json::from_str(&serde_json::to_string(&principal).unwrap()).unwrap();
    assert_eq!(groups_at(&back), Some(at));
    // Groups from a token's claim carry no lookup time.
    let keys = serve_keys(rsa_jwks());
    let fed = federation(None, vec![oidc_config(&keys)]);
    let header = format!("Bearer {}", rsa_sign(RSA_KID, &oidc_claims()));
    let claimed = fed.verify_oidc(Some(&header)).await.unwrap().unwrap();
    assert_eq!(groups_at(&claimed), None);
}

#[tokio::test]
async fn a_person_removed_from_a_group_loses_it_when_old_groups_are_read_again() {
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (config, identity, principal) = engineer_from_cloud_identity(&removed).await;
    let now = branchyard_support::time::now_ms();
    let old = captured_at(&principal, Some(now - GROUPS_TTL_MS - 1));
    // A worker's own federation: its cache is empty, as another process's is.
    let worker = federation(Some(config.clone()), Vec::new());
    let before = identity.requests().len();

    // Still a member: the lookup runs once and the engineer binding stands.
    assert_eq!(worker.still_allows_fresh(&old, now), Ok(true));
    assert_eq!(identity.requests().len(), before + 1);
    // The answer is cached, so a second recheck asks nobody.
    assert_eq!(worker.still_allows_fresh(&old, now), Ok(true));
    assert_eq!(identity.requests().len(), before + 1);

    // Removed from the group: the next worker to ask finds the domain
    // binding, which allows less than the engineer's `run`.
    removed.store(true, Ordering::SeqCst);
    let later = federation(Some(config.clone()), Vec::new());
    assert_eq!(later.still_allows_fresh(&old, now), Ok(false));
    // Judged on the groups captured at sign-in alone, he would still pass.
    assert!(still_allows(Some(&config), &[], &old));
}

#[tokio::test]
async fn groups_younger_than_the_cache_are_not_read_again() {
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (config, identity, principal) = engineer_from_cloud_identity(&removed).await;
    removed.store(true, Ordering::SeqCst);
    let now = branchyard_support::time::now_ms();
    let young = captured_at(&principal, Some(now - 1_000));
    let worker = federation(Some(config), Vec::new());
    let before = identity.requests().len();
    assert_eq!(worker.still_allows_fresh(&young, now), Ok(true));
    assert_eq!(
        identity.requests().len(),
        before,
        "no lookup for young groups"
    );
}

#[tokio::test]
async fn a_failed_lookup_on_a_recheck_refuses_instead_of_guessing() {
    let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut config, _, principal) = engineer_from_cloud_identity(&removed).await;
    let broken = MockHttp::start(|_| Response::new(503, "down"));
    config.groups_url = Some(broken.url());
    let now = branchyard_support::time::now_ms();
    let old = captured_at(&principal, Some(now - GROUPS_TTL_MS - 1));
    let worker = federation(Some(config), Vec::new());
    let refusal = worker.still_allows_fresh(&old, now).unwrap_err();
    assert!(matches!(refusal, Refusal::Unavailable(_)), "{refusal:?}");
    let api: ApiError = refusal.into();
    assert_eq!(api.body.code, "identity_unavailable");
}

#[tokio::test]
async fn claim_groups_are_checked_as_captured() {
    let keys = serve_keys(rsa_jwks());
    let identity = identity_server(false);
    let mut config = oidc_config(&keys);
    config.groups_from = Some(GroupsFrom::CloudIdentity);
    config.groups_url = Some(identity.url());
    let fed = federation(None, vec![config.clone()]);
    let header = format!("Bearer {}", rsa_sign(RSA_KID, &oidc_claims()));
    let principal = fed.verify_oidc(Some(&header)).await.unwrap().unwrap();
    assert_eq!(groups_at(&principal), None);
    let worker = federation(None, vec![config]);
    assert_eq!(
        worker.still_allows_fresh(&principal, u64::MAX / 2),
        Ok(true)
    );
    assert!(
        identity.requests().is_empty(),
        "a claim's groups are not looked up"
    );
}

mod http {
    use super::*;
    use crate::config::{Config, Token};

    async fn start(mut config: Config) -> crate::Running {
        config.listen = "127.0.0.1:0".parse().unwrap();
        config.inventory = false;
        crate::start(config).await.unwrap()
    }

    async fn status(
        server: &crate::Running,
        path: &str,
        iap: Option<&str>,
        bearer: Option<&str>,
    ) -> (u16, String) {
        let mut request = reqwest::Client::new().get(format!("{}{path}", server.url()));
        if let Some(iap) = iap {
            request = request.header(IAP_HEADER, iap);
        }
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iap_is_authoritative_when_configured_and_ignored_otherwise() {
        let key = EsKey::new("iap-1");
        let keys = serve_keys(key.jwks.clone());
        let repo = branchyard_testkit::Repo::init("by");
        let static_token = "0123456789abcdef0123456789abcdef";
        let mut config = Config::new(repo.dir.join("data"));
        config.repos = vec![("app".into(), repo.root.clone())];
        config.tokens = vec![Token {
            name: "ci".into(),
            secret: static_token.into(),
        }];
        let good = key.sign("iap-1", &iap_claims());
        let mut unbound_claims = iap_claims();
        unbound_claims["email"] = json!("mallory@elsewhere.test");
        let unbound = key.sign("iap-1", &unbound_claims);

        // IAP not configured: the header means nothing, the bearer decides.
        let off = start(config.clone()).await;
        assert_eq!(status(&off, "/v1/repos", Some(&good), None).await.0, 401);
        assert_eq!(
            status(&off, "/v1/repos", Some(&good), Some(static_token))
                .await
                .0,
            200
        );
        off.shutdown();
        off.wait().await;

        config.iap = Some(iap_config(&keys));
        let on = start(config).await;
        assert_eq!(status(&on, "/v1/repos", Some(&good), None).await.0, 200);
        // A good bearer does not rescue a bad assertion.
        assert_eq!(
            status(&on, "/v1/repos", Some("garbage"), Some(static_token))
                .await
                .0,
            401
        );
        let (code, body) = status(&on, "/v1/repos", Some(&unbound), Some(static_token)).await;
        assert_eq!(code, 403);
        assert!(body.contains("identity_not_bound"), "{body}");
        // Without the header the static bearer still works.
        assert_eq!(
            status(&on, "/v1/repos", None, Some(static_token)).await.0,
            200
        );
        // Federated identities do not pair devices.
        assert_eq!(status(&on, "/v1/app/me", Some(&good), None).await.0, 403);
        on.shutdown();
        on.wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn federated_bearer_tokens_are_admitted_alongside_static_ones() {
        let keys = serve_keys(rsa_jwks());
        let repo = branchyard_testkit::Repo::init("by");
        let static_token = "0123456789abcdef0123456789abcdef";
        let mut config = Config::new(repo.dir.join("data"));
        config.repos = vec![("app".into(), repo.root.clone())];
        config.tokens = vec![Token {
            name: "ci".into(),
            secret: static_token.into(),
        }];
        config.oidc_issuers = vec![oidc_config(&keys)];
        let server = start(config).await;
        let good = rsa_sign(RSA_KID, &oidc_claims());
        assert_eq!(status(&server, "/v1/repos", None, Some(&good)).await.0, 200);
        assert_eq!(
            status(&server, "/v1/repos", None, Some(static_token))
                .await
                .0,
            200
        );
        let mut wrong = oidc_claims();
        wrong["aud"] = json!("someone-else");
        assert_eq!(
            status(&server, "/v1/repos", None, Some(&rsa_sign(RSA_KID, &wrong)))
                .await
                .0,
            401
        );
        server.shutdown();
        server.wait().await;
    }
}
