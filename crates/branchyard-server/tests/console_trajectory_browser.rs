//! The console's Trajectory page in headless Chromium against a real
//! server and the fake ACP agent: a delegating branch with three turns (tool
//! calls, a delegation to two helpers one of which fails, and a steer), and
//! a branch whose permission request a person denies, drawn as a waterfall from its
//! timeline with its helpers as nested lanes; a step's content read lazily
//! from the event page (the overview makes no such request); keyboard
//! navigation, labels and the two viewports; a comparison of the helpers; a
//! further turn arriving over the event stream without a second timeline
//! read. The page script also mounts the component on synthetic timelines
//! to check the geometry exactly (durations in proportion, a long idle gap
//! folded, many calls clustered, a wait still open). Driven by Playwright
//! (`console_trajectory_browser.js`); skipped, saying so, when Node or
//! Playwright's module is not installed. `BY_TEST_PLAYWRIGHT` names the
//! module's directory; otherwise `$(npm root -g)/playwright` is tried, with
//! `PLAYWRIGHT_BROWSERS_PATH` (default `/opt/pw-browsers` when present).
//! `BY_SCREENSHOTS=DIR` saves the page at 1440 by 900 and 390 by 844, light
//! and dark.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use branchyard::effects::{Approval, ApprovalPolicy, ApprovalSettings};
use branchyard::Envelope;
use branchyard_client::api::{OperationState, SendRequest, TaskRequest};
use branchyard_client::effects_api::ApprovalAnswerRequest;
use branchyard_client::new_key;
use branchyard_server::companion::{link, store};
use branchyard_testkit::wait;
use common::{await_operation, run, Fixture, Server};

/// A task for the fake agent in a private home, as the other console
/// browser tests make one: the person's approval policy binds it only there.
fn task(prompt: &str, name: &str) -> TaskRequest {
    TaskRequest {
        isolated: true,
        ..common::task(prompt, name)
    }
}

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

/// The prompt of the lead's second turn: it delegates to two helpers, one
/// that finishes and one whose harness exits mid-turn, and waits for both.
const DELEGATE: &str = r#"MCP spawn {"prompt": "TOOLS Grep,Read WRITE h.txt=1", "name": "helper", "harness": "gemini-cli"}
MCP spawn {"prompt": "EXIT", "name": "flaky", "harness": "gemini-cli"}
MCP wait helper
MCP wait flaky"#;

#[test]
fn the_trajectory_page_shows_what_an_agent_did_step_by_step() {
    let Some(module) = playwright() else {
        eprintln!("skipped: no Playwright module (set BY_TEST_PLAYWRIGHT)");
        return;
    };
    let f = Fixture::new();
    let mut config = f.config();
    config.app.enabled = true;
    config.app.push = false;
    // The lead delegates through Branchyard's own MCP server, which the
    // fake agent starts from the `by` the engine names.
    config.allow_delegation = true;
    config.by_path = Some(
        branchyard_testkit::built(
            "branchyard-cli",
            "by",
            &std::env::current_exe().expect("the path of the test executable"),
        )
        .to_path_buf(),
    );
    // A person's policy that asks before a write, so a turn parks on an
    // approval for the test to deny.
    config.approvals = ApprovalSettings {
        person: Some(ApprovalPolicy {
            rules: [("write *".to_owned(), Approval::Ask)].into(),
            ..ApprovalPolicy::default()
        }),
        ..ApprovalSettings::default()
    };
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("trajectory-browser".into()),
        tenant: "default".into(),
        scopes: vec!["read".into(), "run".into(), "merge".into()],
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

    let client = server.client();
    let repo = client.repo("app");
    // A send to a branch, as the operation it starts.
    let send = |branch: &str, prompt: &str| -> String {
        repo.send(
            branch,
            &SendRequest {
                prompt: prompt.into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap()
        .id
    };
    // Turn 1: three tool calls.
    let lead = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::depth(2)),
            ..task("TOOLS Read,Edit,Bash plan the work", "lead")
        },
    );
    assert_eq!(lead.state, OperationState::Succeeded, "{lead:?}");
    // Turn 2: two helpers, one of which fails.
    let delegated = await_operation(&client, &send("lead", DELEGATE));
    assert_eq!(delegated.state, OperationState::Succeeded, "{delegated:?}");
    wait::until("both helpers to settle", || {
        let settled = ["helper", "flaky"].iter().all(|name| {
            repo.branch(name)
                .map(|b| !matches!(b.status, branchyard::BranchStatus::Running))
                .unwrap_or(false)
        });
        settled
            .then_some(())
            .ok_or_else(|| "still running".to_owned())
    });
    // Turn 3: a person steers a running turn.
    let steered = send("lead", "AWAIT_STEER");
    wait::until("the steer to be taken", || {
        repo.steer("lead", "also mind the docs")
            .map_err(|e| e.to_string())
    });
    let steered = await_operation(&client, &steered);
    assert_eq!(steered.state, OperationState::Succeeded, "{steered:?}");
    // Another branch: a write the policy asks about, which the person
    // denies. (A delegating branch is not asked: its parent decides.)
    let parked = repo
        .submit_task(&task("PERMISSION WRITE d.txt=4", "asker"), &new_key())
        .unwrap();
    let ask = wait::until("the write to wait for an answer", || {
        let op = client.operation(&parked.id).map_err(|e| e.to_string())?;
        assert!(
            !op.state.is_terminal(),
            "the turn ended before it asked: {op:?}"
        );
        repo.approvals(false)
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "no approval yet".to_owned())
    });
    repo.answer_approval(&ask.id, false, &ApprovalAnswerRequest::default())
        .unwrap();
    let denied = await_operation(&client, &parked.id);
    assert_eq!(denied.state, OperationState::Succeeded, "{denied:?}");

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/console_trajectory_browser.js"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/companion/assets/trajectory.js"
        ))
        .env("PLAYWRIGHT", &module)
        .env("BY_URL", server.url())
        .env("BY_CODE", &code)
        .stdin(Stdio::piped())
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
    let mut stdin = child.stdin.take().unwrap();
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
        "MODEL",
        "LOADED",
        "OVERVIEW",
        "LAZY",
        "KEYBOARD",
        "LANES",
        "COMPARE",
        "PERMISSION",
        "SYNTHETIC",
        "LIVE READY",
    ] {
        expect(step);
    }
    // A fourth turn, recorded while the page is open: three more calls.
    let more = await_operation(&client, &send("lead", "TOOLS Glob,Grep,Bash live"));
    assert_eq!(more.state, OperationState::Succeeded, "{more:?}");
    writeln!(stdin, "live").unwrap();
    for step in ["LIVE", "VIEWPORTS", "DONE"] {
        expect(step);
    }
    assert!(child.wait().unwrap().success());

    // What the page read is what the server holds: the timeline names the
    // lead's four turns and the helpers' lanes come from their own.
    let lead = repo.timeline("lead").unwrap();
    assert_eq!(lead.turns.len(), 4);
    assert!(lead.turns.iter().all(|t| t.startup.is_some()));
    assert!(lead.turns[0].tools.len() == 3);
    let flaky = repo.timeline("flaky").unwrap();
    assert!(matches!(
        flaky.turns[0].outcome,
        Some(branchyard::BranchStatus::Failed { .. })
    ));
}
