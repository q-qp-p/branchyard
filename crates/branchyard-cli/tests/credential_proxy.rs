//! `by credential-proxy`, the proxy a sandbox runs beside its harness,
//! through the built binary: it prints `listening ADDR` when it holds its
//! first token, forwards calls to the gateway with the turn's token and not
//! the harness's, reads a sidecar's settings from a file it waits for, and
//! fails with the server's reason when it cannot get a token. A mock server
//! and a mock gateway on loopback; nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use branchyard_testkit::{wait, MockHttp, Response, Scratch};
use serde_json::json;

const SECRET: &str = "the-turns-secret";

fn by() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_by"));
    branchyard_testkit::hermetic(&mut command);
    command
}

/// A server that issues `tok-N` for the turn `app/feat` #2, and checks that
/// the proxy proved itself with the secret.
fn server() -> MockHttp {
    MockHttp::start(|request| {
        assert_eq!(request.path, "/v1/turns/token");
        assert_eq!(
            request.json(),
            json!({"manifest": "blake3:abc", "branch": "app/feat", "turn": 2})
        );
        assert_eq!(request.header("x-branchyard-turn-secret"), Some(SECRET));
        let now = branchyard_support::time::now_ms() / 1000;
        Response::json(
            200,
            &json!({"token": "tok-1", "expires_at": now + 300, "expires_in": 300}),
        )
    })
}

fn gateway() -> MockHttp {
    MockHttp::start(|request| {
        Response::json(
            200,
            &json!({"authorization": request.header("authorization")}),
        )
    })
}

/// A running proxy and the lines it prints.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Running {
    fn start(mut command: Command) -> Running {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Running { child, lines }
    }

    /// The address from its `listening ADDR` line.
    fn address(&self) -> String {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("the proxy says it is listening");
        line.strip_prefix("listening ")
            .unwrap_or_else(|| panic!("not a ready line: {line}"))
            .to_owned()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        branchyard_support::best_effort("kill the proxy", self.child.kill());
        branchyard_support::best_effort("reap the proxy", self.child.wait());
    }
}

/// One call to `address`, with a token of the harness's own making.
fn call(address: &str) -> serde_json::Value {
    let target = format!("http://{address}");
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    use std::io::Write as _;
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: {target}\r\nAuthorization: Bearer harness-own-token\r\n\
         Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1;
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not JSON: {response}"))
}

#[test]
fn it_forwards_with_the_turns_token_when_given_its_settings_on_the_command_line() {
    let (server, gateway) = (server(), gateway());
    let mut command = by();
    command
        .args(["credential-proxy", "--listen", "127.0.0.1:0", "--server"])
        .arg(server.url())
        .arg("--upstream")
        .arg(format!("{}/mcp", gateway.url()))
        .args([
            "--manifest",
            "blake3:abc",
            "--branch",
            "app/feat",
            "--turn",
            "2",
        ])
        .args(["--identity", "secret"])
        .env("BRANCHYARD_CREDENTIAL_SECRET", SECRET);
    let proxy = Running::start(command);
    let address = proxy.address();
    // The secret is in the environment of the process and in no argument.
    assert_eq!(call(&address)["authorization"], "Bearer tok-1");
    let seen = gateway.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/mcp");
}

#[test]
fn a_sidecar_waits_for_its_settings_and_then_serves() {
    let scratch = Scratch::new("credential-proxy-sidecar");
    let (server, gateway) = (server(), gateway());
    let settings = scratch.join(".branchyard/credential-proxy.json");
    let mut command = by();
    command
        .args(["credential-proxy", "--wait", "--config"])
        .arg(&settings)
        .env("BRANCHYARD_CREDENTIAL_SECRET", SECRET);
    let mut proxy = Running::start(command);
    // No settings yet: it waits, it neither fails nor listens.
    assert!(
        wait::try_until_for(Duration::from_millis(500), || proxy
            .child
            .try_wait()
            .ok()
            .flatten())
        .is_err(),
        "it should still be waiting"
    );
    assert!(proxy.lines.try_recv().is_err());
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        json!({
            "listen": "127.0.0.1:0", "server": server.url(),
            "upstream": format!("{}/mcp", gateway.url()),
            "manifest": "blake3:abc", "branch": "app/feat", "turn": 2,
            "identity": {"kind": "secret"}
        })
        .to_string(),
    )
    .unwrap();
    let address = proxy.address();
    assert_eq!(call(&address)["authorization"], "Bearer tok-1");
}

#[test]
fn a_refused_token_ends_the_proxy_with_the_servers_reason() {
    let server = MockHttp::start(|_| {
        Response::json(
            409,
            &json!({"error": {"code": "turn_not_running", "message": "the branch is not running a turn"}}),
        )
    });
    let output = by()
        .args(["credential-proxy", "--listen", "127.0.0.1:0", "--server"])
        .arg(server.url())
        .args(["--upstream", "http://127.0.0.1:9/mcp"])
        .args([
            "--manifest",
            "blake3:abc",
            "--branch",
            "app/feat",
            "--turn",
            "2",
        ])
        .env("BRANCHYARD_CREDENTIAL_SECRET", SECRET)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("turn_not_running"), "{said}");
    assert!(
        output.stdout.is_empty(),
        "no ready line for a refused token"
    );

    // A secret that was never given is said, not guessed.
    let output = by()
        .args(["credential-proxy", "--listen", "127.0.0.1:0", "--server"])
        .arg(server.url())
        .args(["--upstream", "http://127.0.0.1:9/mcp"])
        .args([
            "--manifest",
            "blake3:abc",
            "--branch",
            "app/feat",
            "--turn",
            "2",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("BRANCHYARD_CREDENTIAL_SECRET"), "{said}");
}
