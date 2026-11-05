//! The sandbox's credential proxy against a mock server and a mock gateway:
//! calls carry the turn's token and never the harness's; the token is
//! renewed before it expires; a refused fetch is said, not hidden; and the
//! workload identity path reads the metadata server's ID token. No network
//! beyond loopback, no model, no cloud.

use std::io::Read as _;
use std::sync::{Arc, Mutex};

use branchyard_testkit::{wait, MockHttp, Response};
use serde_json::json;

use super::*;

const SECRET: &str = "per-turn-secret";

fn config(server: &MockHttp, gateway: &MockHttp, identity: IdentityKind) -> ProxyConfig {
    ProxyConfig {
        listen: "127.0.0.1:0".into(),
        server: server.url(),
        upstream: format!("{}/mcp", gateway.url()),
        manifest: "blake3:abc".into(),
        branch: "app/feat".into(),
        turn: 3,
        identity,
        secret: None,
    }
}

/// A server that issues `tok-1`, `tok-2`, ... each living `life_s` seconds,
/// and notes when each fetch arrived.
fn server(life_s: u64) -> (MockHttp, Arc<Mutex<Vec<u64>>>) {
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let seen = arrivals.clone();
    let server = MockHttp::start(move |request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/turns/token");
        let body = request.json();
        assert_eq!(
            body,
            json!({"manifest": "blake3:abc", "branch": "app/feat", "turn": 3})
        );
        let now = branchyard_support::time::now_ms();
        let mut arrivals = seen.lock().unwrap();
        arrivals.push(now);
        Response::json(
            200,
            &json!({
                "token": format!("tok-{}", arrivals.len()),
                "expires_at": now / 1000 + life_s,
                "expires_in": life_s,
            }),
        )
    });
    (server, arrivals)
}

/// A gateway that answers every call with the `Authorization` it saw.
fn gateway() -> MockHttp {
    MockHttp::start(|request| {
        Response::json(
            200,
            &json!({
                "path": request.path,
                "authorization": request.header("authorization"),
                "x_api_key": request.header("x-api-key"),
                "accept": request.header("x-harness-header"),
            }),
        )
    })
}

/// A call to the proxy, with `headers`, and the gateway's answer.
fn call(proxy: &CredentialProxy, path: &str, headers: &[(&str, &str)]) -> (u16, serde_json::Value) {
    let target = Target::parse(&format!("http://{}", proxy.address())).unwrap();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    let mut response = upstream::send(&target, "POST", path, &headers, b"{}").unwrap();
    let mut body = String::new();
    response.body.read_to_string(&mut body).unwrap();
    (
        response.status,
        serde_json::from_str(&body).unwrap_or(json!(body)),
    )
}

#[test]
fn a_call_goes_to_the_gateway_with_the_turns_token_and_not_the_harnesss() {
    let (server, _) = server(300);
    let gateway = gateway();
    let proxy = CredentialProxy::start(
        &config(&server, &gateway, IdentityKind::Secret),
        Some(SECRET.into()),
        true,
    )
    .unwrap();
    // The harness holds no token, and a stray one it makes up is replaced.
    let (status, seen) = call(
        &proxy,
        "/mcp/tools?x=1",
        &[
            ("Authorization", "Bearer harness-made-this-up"),
            ("X-Api-Key", "also-made-up"),
            ("X-Harness-Header", "kept"),
        ],
    );
    assert_eq!(status, 200);
    assert_eq!(seen["authorization"], "Bearer tok-1", "{seen}");
    assert_eq!(seen["x_api_key"], json!(null), "{seen}");
    assert_eq!(seen["accept"], "kept", "ordinary headers pass: {seen}");
    // `/mcp` maps onto the gateway's own URL, query kept.
    assert_eq!(seen["path"], "/mcp/tools?x=1");
    // The fetch proved the turn with its secret, on a header of its own.
    let fetch = server.requests();
    assert_eq!(fetch.len(), 1, "one fetch serves every call");
    assert_eq!(fetch[0].header("x-branchyard-turn-secret"), Some(SECRET));
    assert_eq!(fetch[0].header("authorization"), None);
    assert_eq!(call(&proxy, "/mcp", &[]).1["authorization"], "Bearer tok-1");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn the_token_is_renewed_before_it_expires() {
    // Tokens live two seconds: renewal at their half.
    let (server, arrivals) = server(2);
    let gateway = gateway();
    let proxy = CredentialProxy::start(
        &config(&server, &gateway, IdentityKind::Secret),
        Some(SECRET.into()),
        true,
    )
    .unwrap();
    let first = arrivals.lock().unwrap()[0];
    wait::until("a second fetch", || arrivals.lock().unwrap().len() >= 2);
    let second = arrivals.lock().unwrap()[1];
    // The second came in the first token's life, not after its lapse.
    let lapse = (first / 1000 + 2) * 1000;
    assert!(
        second < lapse,
        "renewed at {second}, first lapsed at {lapse}"
    );
    // And the proxy calls with what it fetched last.
    let newest = wait::until("a call on a renewed token", || {
        let seen = call(&proxy, "/mcp", &[]).1;
        let token = seen["authorization"].as_str()?.to_owned();
        (token != "Bearer tok-1").then_some(token)
    });
    assert!(newest.starts_with("Bearer tok-"), "{newest}");
}

#[test]
fn a_refused_fetch_is_said_at_start_and_on_a_call() {
    let server = MockHttp::start(|_| {
        Response::json(
            409,
            &json!({"error": {"code": "turn_not_running", "message": "the branch is not running a turn"}}),
        )
    });
    let gateway = gateway();
    let config = config(&server, &gateway, IdentityKind::Secret);
    // Asked to hold a first token, it will not start without one.
    let refused = CredentialProxy::start(&config, Some(SECRET.into()), true)
        .err()
        .expect("no start without a token");
    assert!(
        refused.contains("turn_not_running") && refused.contains("409"),
        "{refused}"
    );
    // A sidecar starts and keeps trying; its callers are told why not.
    let proxy = CredentialProxy::start(&config, Some(SECRET.into()), false).unwrap();
    let (status, answer) = call(&proxy, "/mcp", &[]);
    assert_eq!(status, 502);
    assert_eq!(
        answer["error"]["code"], "credential_unavailable",
        "{answer}"
    );
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("turn_not_running"),
        "{answer}"
    );
    assert!(gateway.requests().is_empty(), "nothing reached the gateway");
    // Nothing is guessed about a secret that is not there.
    let missing = CredentialProxy::start(&config, None, true).err().unwrap();
    assert!(missing.contains(ENV_SECRET), "{missing}");
}

#[test]
fn a_workload_identity_proves_the_turn_with_the_metadata_servers_id_token() {
    let metadata = MockHttp::start(|request| {
        assert_eq!(request.header("metadata-flavor"), Some("Google"));
        assert!(
            request
                .path
                .starts_with("/computeMetadata/v1/instance/service-accounts/default/identity?"),
            "{}",
            request.path
        );
        assert!(
            request.path.contains("audience=https%3A%2F%2Fby%2Eexample"),
            "{}",
            request.path
        );
        Response::new(200, "google-id-token\n")
    });
    let (server, _) = server(300);
    let gateway = gateway();
    let host = metadata.url().trim_start_matches("http://").to_owned();
    let proxy = CredentialProxy::start_at(
        &config(
            &server,
            &gateway,
            IdentityKind::Workload {
                audience: "https://by.example".into(),
            },
        ),
        Some(SECRET.into()),
        true,
        Some(host),
    )
    .unwrap();
    // The identity is in addition to the secret, never instead of it.
    let fetch = &server.requests()[0];
    assert_eq!(
        fetch.header("authorization"),
        Some("Bearer google-id-token")
    );
    assert_eq!(fetch.header("x-branchyard-turn-secret"), Some(SECRET));
    assert_eq!(call(&proxy, "/mcp", &[]).1["authorization"], "Bearer tok-1");
}

#[test]
fn an_identity_without_the_turns_secret_does_not_start_and_a_settings_file_can_hold_it() {
    let metadata = MockHttp::start(|_| Response::new(200, "google-id-token\n"));
    let (server, _) = server(300);
    let gateway = gateway();
    let host = metadata.url().trim_start_matches("http://").to_owned();
    let workload = || IdentityKind::Workload {
        audience: "https://by.example".into(),
    };
    let missing = CredentialProxy::start_at(
        &config(&server, &gateway, workload()),
        None,
        true,
        Some(host.clone()),
    )
    .err()
    .unwrap();
    assert!(missing.contains(ENV_SECRET), "{missing}");
    assert!(
        server.requests().is_empty(),
        "nothing was asked of the server"
    );
    // A sidecar's secret comes from its settings file.
    let mut settings = config(&server, &gateway, workload());
    settings.secret = Some(SECRET.into());
    let proxy = CredentialProxy::start_at(&settings, None, true, Some(host)).unwrap();
    assert_eq!(
        server.requests()[0].header("x-branchyard-turn-secret"),
        Some(SECRET)
    );
    drop(proxy);
}

#[test]
fn a_stopped_proxy_stops_renewing() {
    let (server, arrivals) = server(2);
    let gateway = gateway();
    let proxy = CredentialProxy::start(
        &config(&server, &gateway, IdentityKind::Secret),
        Some(SECRET.into()),
        true,
    )
    .unwrap();
    let address = proxy.address();
    drop(proxy);
    // Dropping joins its threads: nothing fetches after it, and nothing
    // listens.
    let fetched = arrivals.lock().unwrap().len();
    assert!(std::net::TcpStream::connect(address).is_err());
    assert_eq!(arrivals.lock().unwrap().len(), fetched);
}

#[test]
fn a_sidecar_follows_its_settings_file_from_turn_to_turn_and_serves_none_once_it_is_gone() {
    // A server that issues a token naming the turn and secret it was asked
    // with, so the test sees which turn each call carried.
    let server = MockHttp::start(|request| {
        let body = request.json();
        let secret = request.header("x-branchyard-turn-secret").unwrap_or("");
        Response::json(
            200,
            &json!({
                "token": format!("turn-{}-{secret}", body["turn"]),
                "expires_at": branchyard_support::time::now_ms() / 1000 + 300,
                "expires_in": 300,
            }),
        )
    });
    let gateway = gateway();
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("credential-proxy.json");
    let write = |turn: u32, secret: &str| {
        let mut settings = config(&server, &gateway, IdentityKind::Secret);
        settings.turn = turn;
        settings.secret = Some(secret.into());
        std::fs::write(&file, serde_json::to_string(&settings).unwrap()).unwrap();
        settings
    };
    let first = write(3, "s3");
    let proxy = CredentialProxy::start_following(&file, &first, true, None).unwrap();
    assert_eq!(
        call(&proxy, "/mcp", &[]).1["authorization"],
        "Bearer turn-3-s3"
    );
    assert_eq!(server.requests().len(), 1, "the held token serves the turn");
    // The turn ends: the engine removes the file, and the token held for it
    // is no longer used, though it has minutes left.
    std::fs::remove_file(&file).unwrap();
    let (status, _) = call(&proxy, "/mcp", &[]);
    assert_ne!(status, 200, "no turn runs, so no call is forwarded");
    assert_eq!(server.requests().len(), 1, "and nothing is fetched");
    // The pod is kept and the next turn writes its own settings: the proxy
    // fetches that turn's token with that turn's secret.
    write(4, "s4");
    assert_eq!(
        call(&proxy, "/mcp", &[]).1["authorization"],
        "Bearer turn-4-s4"
    );
    let fetched = server.requests();
    assert_eq!(fetched.len(), 2);
    assert_eq!(fetched[1].json()["turn"], 4);
    assert_eq!(fetched[1].header("x-branchyard-turn-secret"), Some("s4"));
}
