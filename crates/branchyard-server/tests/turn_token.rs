//! `POST /v1/turns/token`: a running turn's sandbox fetches its gateway
//! token with the turn's secret and, where the server lists them, its
//! workload identity. A real server, turns held open by FIFOs, Google's
//! signing keys served by a mock and ID tokens signed with the test key; no
//! cloud, no network beyond loopback. Covers an allow-listed identity
//! without the turn's secret, with another turn's secret and with a guessed
//! one, the secret alone where identities are listed, an unknown service
//! account, a wrong audience, a wrong manifest digest, a wrong turn, a
//! service account for another tenant, a body that tries to carry claims,
//! and a turn that has finished.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use branchyard::connectors::GrantEntry;
use branchyard::{Activity, Provisioning};
use branchyard_server::config::{ConnectorsConfig, SandboxIdentity};
use branchyard_testkit::{wait, MockHttp, Response};
use common::{await_operation, get, post, raw, started, task, Fixture, Server};
use serde_json::{json, Value};

const KEY: &[u8] = include_bytes!("../../branchyard-sync/src/auth/testdata/rsa-test-key.pem");
const JWKS: &str = include_str!("../src/testdata/google-jwks.json");
const ISSUER: &str = "https://by.example";
const SANDBOX: &str = "sandbox@proj.iam.gserviceaccount.com";
const GLOBEX: &str = "globex@proj.iam.gserviceaccount.com";

const FAKE_ANVIL: &str = r##"#!/bin/sh
case "$1 $2" in
"package harness") mkdir -p "$5" && echo "# $(basename "$3")" > "$5/SKILL.md" ;;
"connectors index") echo "# Connectors" > "$6" ;;
*) echo "fake anvil: $*" >&2; exit 2 ;;
esac
"##;

fn config(
    f: &Fixture,
    jwks: &MockHttp,
    identities: Vec<SandboxIdentity>,
) -> branchyard_server::config::Config {
    let bundles = f.dir.join("bundles");
    fs::create_dir_all(bundles.join("github")).unwrap();
    fs::write(bundles.join("github/air.yaml"), "service: github\n").unwrap();
    let anvil = f.dir.join("fake-anvil");
    fs::write(&anvil, FAKE_ANVIL).unwrap();
    fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = f.config();
    config.connectors = Some(ConnectorsConfig {
        gateway: "http://127.0.0.1:9/mcp".into(),
        sandbox_gateway: None,
        issuer: Some(ISSUER.into()),
        signing_key: None,
        signing_kms: None,
        audit_file: None,
        bundles,
        anvil: vec![anvil.display().to_string()],
        run_gateway: false,
        listen: None,
        vault_key: None,
        effects_proxy: None,
        effects_listen: None,
        effects_sandbox_host: None,
        sandbox_server: Some("http://127.0.0.1:1".into()),
        sandbox_audience: None,
        sandbox_identities: identities,
        sandbox_jwks_url: Some(jwks.url()),
        sandbox_proxy_image: None,
        sandbox_by: None,
    });
    config
}

fn google_keys() -> MockHttp {
    let jwks: Value = serde_json::from_str(JWKS).unwrap();
    MockHttp::start(move |_| Response::json(200, &jwks))
}

/// A Google ID token as the metadata server would sign it for `email`.
fn id_token(email: &str, audience: &str) -> String {
    let now = branchyard_support::time::now_ms() / 1000;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("fake-google-key".into());
    jsonwebtoken::encode(
        &header,
        &json!({
            "iss": "https://accounts.google.com", "sub": "1234567890", "aud": audience,
            "iat": now, "exp": now + 3600, "email": email, "email_verified": true,
        }),
        &jsonwebtoken::EncodingKey::from_rsa_pem(KEY).unwrap(),
    )
    .unwrap()
}

/// A token request and its answer: status, error code (or empty), body.
fn fetch(server: &Server, bearer: Option<&str>, extra: &str, body: &Value) -> (u16, String, Value) {
    let (status, _, text) = raw(
        server.addr,
        &post("/v1/turns/token", bearer, extra, &body.to_string()),
    );
    let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let code = answer["error"]["code"].as_str().unwrap_or("").to_owned();
    (status, code, answer)
}

fn request(manifest: &str, branch: &str, turn: u32) -> Value {
    json!({"manifest": manifest, "branch": branch, "turn": turn})
}

const SECRET_HEADER: &str = "X-Branchyard-Turn-Secret";

/// A turn held open until its gate is written, with the connector grant.
struct Turn {
    gate: std::path::PathBuf,
    op: String,
    digest: String,
    secret: String,
}

/// Start a gated turn on `name` and give it the secret its sandbox would
/// hold: the engine records the hash of a secret for each turn whose
/// credentials a sandbox holds (a Microsandbox guest, a GKE sidecar), which
/// this local turn has not, so the record is written the way the engine
/// writes it.
fn turn(f: &Fixture, server: &Server, name: &'static str) -> Turn {
    let client = server.client();
    let gate = f.dir.join(format!("gate-{name}"));
    assert!(std::process::Command::new("mkfifo")
        .arg(&gate)
        .status()
        .unwrap()
        .success());
    let mut held = task(&format!("SH cat {}", gate.display()), name);
    held.isolated = true;
    held.provision = Some(Provisioning {
        connectors: vec![GrantEntry::parse("github:read").unwrap()],
        ..Provisioning::default()
    });
    let op = client
        .repo("app")
        .submit_task(&held, &branchyard_client::new_key())
        .unwrap();
    wait::until("the turn to start", || started(&client, "app", name));
    let digest = wait::until("the provisioned manifest", || {
        client
            .repo("app")
            .events(name, 0)
            .unwrap()
            .events
            .iter()
            .find_map(|e| match &e.activity {
                Activity::Provisioned { manifest, .. } => manifest.clone(),
                _ => None,
            })
    });
    let secret = format!("secret-of-{name}-{}", "0123456789abcdef".repeat(3));
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    let text: String = db
        .query_row("SELECT record FROM branches WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap();
    let mut record: Value = serde_json::from_str(&text).unwrap();
    record["credential"] = json!({
        "secret_blake3": blake3::hash(secret.as_bytes()).to_hex().to_string(),
        "manifest": digest,
        "turn": 1,
    });
    db.execute(
        "UPDATE branches SET record = ?1 WHERE name = ?2",
        rusqlite::params![record.to_string(), name],
    )
    .unwrap();
    Turn {
        gate,
        op: op.id,
        digest,
        secret,
    }
}

#[test]
fn a_sandbox_fetches_its_turns_token_with_the_turns_secret_and_its_identity_only_while_the_turn_runs(
) {
    let f = Fixture::new();
    let keys = google_keys();
    let server = Server::start(config(
        &f,
        &keys,
        vec![
            SandboxIdentity {
                service_account: SANDBOX.into(),
                tenant: None,
            },
            SandboxIdentity {
                service_account: GLOBEX.into(),
                tenant: Some("globex".into()),
            },
        ],
    ));
    let client = server.client();
    // Two turns, whose sandboxes share one Workload Identity service
    // account: `gated` is the one asked about, `other` is another person's.
    let gated = turn(&f, &server, "gated");
    let other = turn(&f, &server, "other");
    assert_ne!(gated.secret, other.secret);

    let good = id_token(SANDBOX, ISSUER);
    let body = request(&gated.digest, "app/gated", 1);
    let with = |secret: &str| format!("{SECRET_HEADER}: {secret}\r\n");
    let ask = |bearer: Option<&str>, secret: Option<&str>, body: &Value| {
        fetch(&server, bearer, &secret.map(with).unwrap_or_default(), body)
    };

    // Nothing, or only the secret where identities are listed, or only an
    // identity (even an allow-listed one, with the turn's real digest, branch
    // and number, all of which the harness can read): no way in.
    assert_eq!(ask(None, None, &body).1, "auth_required");
    let (status, code, _) = ask(None, Some(&gated.secret), &body);
    assert_eq!((status, code.as_str()), (401, "auth_required"));
    let (status, code, _) = ask(Some(&good), None, &body);
    assert_eq!(
        (status, code.as_str()),
        (401, "auth_required"),
        "an identity alone"
    );
    // An allow-listed identity with a secret nobody recorded, and with
    // another running turn's secret.
    let (status, code, _) = ask(Some(&good), Some("guess"), &body);
    assert_eq!((status, code.as_str()), (401, "invalid_credential"));
    let (status, code, _) = ask(Some(&good), Some(&other.secret), &body);
    assert_eq!((status, code.as_str()), (401, "invalid_credential"));
    // The other turn's secret is that turn's own, with the same identity.
    let (status, code, _) = ask(
        Some(&good),
        Some(&other.secret),
        &request(&other.digest, "app/other", 1),
    );
    assert_eq!((status, code.as_str()), (200, ""));

    // Who: someone who is not Google, someone Google vouches for that the
    // server does not list, and a token meant for another server.
    let secret = Some(gated.secret.as_str());
    assert_eq!(
        ask(Some("not-a-google-token"), secret, &body).1,
        "invalid_credential"
    );
    let (status, code, _) = ask(
        Some(&id_token("intruder@proj.iam.gserviceaccount.com", ISSUER)),
        secret,
        &body,
    );
    assert_eq!((status, code.as_str()), (403, "identity_not_allowed"));
    let elsewhere = id_token(SANDBOX, "https://another.example");
    assert_eq!(ask(Some(&elsewhere), secret, &body).1, "invalid_credential");

    // Which turn: the secret is bound to its manifest and number; another
    // tenant's account; a branch that is not there; a body that is not a
    // branch.
    let (status, code, _) = ask(Some(&good), secret, &request("blake3:00", "app/gated", 1));
    assert_eq!((status, code.as_str()), (401, "invalid_credential"));
    let (status, code, _) = ask(Some(&good), secret, &request(&gated.digest, "app/gated", 2));
    assert_eq!((status, code.as_str()), (401, "invalid_credential"));
    let (status, code, _) = ask(Some(&id_token(GLOBEX, ISSUER)), secret, &body);
    assert_eq!((status, code.as_str()), (403, "tenant_mismatch"));
    let (status, code, _) = ask(
        Some(&good),
        secret,
        &request(&gated.digest, "app/nobody", 1),
    );
    assert_eq!((status, code.as_str()), (404, "unknown_turn"));
    let (status, code, _) = ask(
        Some(&good),
        secret,
        &request(&gated.digest, "nope/gated", 1),
    );
    assert_eq!((status, code.as_str()), (404, "unknown_turn"));
    let (status, code, _) = ask(Some(&good), secret, &request(&gated.digest, "gated", 1));
    assert_eq!((status, code.as_str()), (400, "invalid_request"));
    // Claims are never taken from the request: a body that offers some is
    // refused outright.
    let mut greedy = body.clone();
    greedy["grants"] = json!(["github:write"]);
    let (status, ..) = ask(Some(&good), secret, &greedy);
    assert!((400..500).contains(&status), "{status}");

    // The turn's own sandbox: a five-minute token from the recorded grant.
    let (status, code, answer) = ask(Some(&good), secret, &body);
    assert_eq!((status, code.as_str()), (200, ""), "{answer}");
    assert_eq!(answer["expires_in"], 300);
    let (_, _, jwks) = raw(server.addr, &get("/.well-known/jwks.json", None));
    let jwks: Value = serde_json::from_str(&jwks).unwrap();
    let claims =
        branchyard::connectors::keys::verify(&jwks, answer["token"].as_str().unwrap()).unwrap();
    assert_eq!(claims["iss"], ISSUER);
    assert_eq!(claims["aud"], "http://127.0.0.1:9/mcp");
    assert_eq!(claims["sub"], "tester");
    assert_eq!(claims["by_tenant"], "default");
    assert_eq!(claims["by_branch"], "app/gated");
    assert_eq!(claims["by_turn"], "1");
    assert_eq!(claims["by_manifest"], gated.digest.as_str());
    assert_eq!(claims["by_grants"].as_array().unwrap().len(), 1, "{claims}");
    assert_eq!(claims["by_grants"][0]["connector"], "github");
    assert_eq!(claims["by_grants"][0]["mode"], "read");
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        300
    );
    assert_eq!(claims["exp"], answer["expires_at"]);

    // The turns end; the same call is refused, secret and identity both
    // right, because the engine drops the secret's hash with the turn.
    for turn in [&gated, &other] {
        fs::write(&turn.gate, "go").unwrap();
        assert!(await_operation(&client, &turn.op).state.is_terminal());
    }
    let (status, code, _) = ask(Some(&good), secret, &body);
    assert_eq!((status, code.as_str()), (401, "invalid_credential"));
}

#[test]
fn a_server_that_lists_no_service_accounts_takes_the_turns_secret_alone_and_no_google_token() {
    let f = Fixture::new();
    let keys = google_keys();
    let server = Server::start(config(&f, &keys, Vec::new()));
    let client = server.client();
    let gated = turn(&f, &server, "gated");
    let other = turn(&f, &server, "other");
    let body = request(&gated.digest, "app/gated", 1);
    let with = |secret: &str| format!("{SECRET_HEADER}: {secret}\r\n");
    // No secret: refused, with or without a Google token (an unlisted
    // server admits none, and a token is no substitute for the secret).
    assert_eq!(fetch(&server, None, "", &body).1, "auth_required");
    let (status, code, _) = fetch(&server, Some(&id_token(SANDBOX, ISSUER)), "", &body);
    assert_eq!((status, code.as_str()), (401, "auth_required"));
    // Another turn's secret, and a guess.
    for secret in [other.secret.as_str(), "guess"] {
        let (status, code, _) = fetch(&server, None, &with(secret), &body);
        assert_eq!((status, code.as_str()), (401, "invalid_credential"));
    }
    // This turn's own.
    let (status, code, answer) = fetch(&server, None, &with(&gated.secret), &body);
    assert_eq!((status, code.as_str()), (200, ""), "{answer}");
    assert_eq!(answer["expires_in"], 300);
    for turn in [&gated, &other] {
        fs::write(&turn.gate, "go").unwrap();
        assert!(await_operation(&client, &turn.op).state.is_terminal());
    }
    // And the route does not exist on a server with no gateway.
    let unconfigured = Fixture::new();
    let plain = Server::start(unconfigured.config());
    let (status, ..) = fetch(&plain, None, &with("x"), &body);
    assert_eq!(status, 404);
}
