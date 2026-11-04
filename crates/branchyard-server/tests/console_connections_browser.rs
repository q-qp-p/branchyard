//! Connections and a session's tools in headless Chromium, against a real
//! server whose connector gateway is a mock of Anvil's connect routes: a
//! paired person sees each connection's state on the Connections page,
//! connects GitHub through a sign-in tab and Linear with a key (which the
//! page never shows again), then starts a Work session that may use
//! Atlassian with changes and GitHub read only, one how-to and one helper,
//! named from its prompt; the session's facts list them, and the next
//! session's draft remembers the connections. The page and the form fit
//! 390 pixels. Driven by Playwright (`console_connections_browser.js`);
//! skipped, saying so, when Node or Playwright's module is not installed.
//! `BY_TEST_PLAYWRIGHT` names the module's directory; otherwise
//! `$(npm root -g)/playwright` is tried, with `PLAYWRIGHT_BROWSERS_PATH`
//! (default `/opt/pw-browsers` when present). `BY_SCREENSHOTS=DIR` saves the
//! Connections page and the Tools step. Hermetic: the gateway is a mock
//! shaped after Anvil's `serve-fleet-http.ts`; no model is called.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard_server::companion::{link, store};
use branchyard_testkit::{MockHttp, Response};
use common::{fake_agent, Fixture, Server, TOKEN};
use serde_json::{json, Value};

/// The key the page types for Linear.
const LINEAR_KEY: &str = "lin_api_hermetic_secret_value";

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

/// The gateway: Atlassian connected; GitHub connects once its sign-in
/// page is opened; Linear once a key is stored.
fn gateway() -> (MockHttp, Arc<AtomicBool>) {
    let github = Arc::new(AtomicBool::new(false));
    let linear = Arc::new(AtomicBool::new(false));
    let keyed = linear.clone();
    let signed = github.clone();
    let mock = MockHttp::start(move |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let path = request.path.split('?').next().unwrap_or_default();
        match (request.method.as_str(), path) {
            ("GET", "/connect/status") => {
                let mut connections = vec![
                    json!({"connector":"atlassian","account":"default","kind":"oauth","status":"active","updatedAt":"2026-11-04T00:00:00Z"}),
                ];
                if signed.load(Ordering::SeqCst) {
                    connections.push(json!({"connector":"github","account":"default","kind":"oauth","status":"active","updatedAt":"2026-11-04T00:00:00Z"}));
                }
                if keyed.load(Ordering::SeqCst) {
                    connections.push(json!({"connector":"linear","account":"default","kind":"static","status":"active","updatedAt":"2026-11-04T00:00:00Z"}));
                }
                Response::json(200, &json!({"sub":"x","connections":connections}))
            }
            ("POST", "/connect/start") if body["connector"] == "github" => {
                let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
                Response::json(
                    200,
                    &json!({"kind":"oauth","connector":"github","url":format!("http://{host}/oauth/github?state=s"),"expires_at":1}),
                )
            }
            ("POST", "/connect/start") => Response::json(
                200,
                &json!({"kind":"static","connector":body["connector"],"submit":"/connect/api-key"}),
            ),
            ("POST", "/connect/api-key") if body["api_key"] == LINEAR_KEY => {
                keyed.store(true, Ordering::SeqCst);
                Response::json(
                    200,
                    &json!({"connected":true,"connector":"linear","account":"default"}),
                )
            }
            ("GET", "/oauth/github") => {
                signed.store(true, Ordering::SeqCst);
                Response::new(200, "Connected github. You can close this window.\n")
                    .header("content-type", "text/plain")
            }
            _ => Response::json(
                404,
                &json!({"error":{"code":"not_found","message":"No such route."}}),
            ),
        }
    });
    (mock, linear)
}

#[test]
fn connections_and_session_tools_in_the_browser() {
    let Some(module) = playwright() else {
        eprintln!("skipped: no Playwright module (set BY_TEST_PLAYWRIGHT)");
        return;
    };
    let f = Fixture::new();
    let (mock, linear) = gateway();
    let bundles = f.dir.join("bundles");
    for id in ["atlassian", "github", "linear"] {
        std::fs::create_dir_all(bundles.join(id)).unwrap();
        std::fs::write(
            bundles.join(id).join("air.yaml"),
            format!("service: {id}\n"),
        )
        .unwrap();
    }
    let path = f.dir.join("server.json");
    std::fs::write(
        &path,
        json!({
            "data_dir": f.data,
            "repos": { "app": f.root },
            "tokens": [{ "name": "tester", "token": TOKEN }],
            "app": { "push": false },
            "managed_knowledge": { "enabled": true },
            "allow_delegation": true,
            "connectors": {
                "gateway": format!("{}/mcp", mock.url()),
                "issuer": "https://by.example",
                "bundles": bundles,
                "anvil": ["/bin/false"],
            },
            "execution_presets": [
                {"id": "local", "label": "Local workers",
                 "description": "Runs on this Branchyard server.",
                 "provider": {"kind": "local"},
                 "harnesses": [{"harness": "gemini-cli"}]}
            ],
        })
        .to_string(),
    )
    .unwrap();
    let mut config =
        branchyard_server::cli::resolve(&["--config".into(), path.display().to_string()]).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.harness_commands.insert(
        "gemini-cli".into(),
        vec![fake_agent().display().to_string()],
    );
    config.shutdown_grace = Duration::from_secs(10);
    config.poll_interval = Duration::from_millis(100);
    config.log_requests = false;
    config.inventory_source = Some(branchyard_server::ops::InventorySource(Arc::new(|| None)));

    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("owner-browser".into()),
        tenant: "default".into(),
        scopes: vec!["read".into(), "run".into(), "merge".into(), "admin".into()],
        repos: None,
        ttl: None,
        code_ttl: None,
        public_url: None,
        qr: false,
    };
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &request,
        branchyard_support::time::now_ms(),
    )
    .unwrap();

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/console_connections_browser.js"
        ))
        .env("PLAYWRIGHT", &module)
        .env("BY_URL", server.url())
        .env("BY_CODE", &code)
        .env("BY_LINEAR_KEY", LINEAR_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = browsers {
        command.env("PLAYWRIGHT_BROWSERS_PATH", path);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("skipped: cannot run node: {e}");
            return;
        }
    };
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut expect = |want: &str| {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        if line.trim() != want {
            let status = child.wait().unwrap();
            panic!("the page script said {line:?} (exit {status}), not {want}");
        }
    };
    for step in [
        "LOADED",
        "CONNECTIONS",
        "OAUTH",
        "KEY",
        "TOOLS",
        "STARTED",
        "PHONE",
        "DONE",
    ] {
        expect(step);
    }
    assert!(child.wait().unwrap().success());
    assert!(linear.load(Ordering::SeqCst), "the key reached the gateway");
    // The key went to the gateway once, and nowhere the server keeps.
    let stored = mock
        .requests()
        .into_iter()
        .filter(|r| r.body_text().contains(LINEAR_KEY))
        .count();
    assert_eq!(stored, 1);
    drop(server);
    for entry in walk(&f.data) {
        let bytes = std::fs::read(&entry).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(LINEAR_KEY),
            "{} holds the key",
            entry.display()
        );
    }
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out
}
