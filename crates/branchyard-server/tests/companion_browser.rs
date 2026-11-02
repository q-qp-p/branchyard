//! The companion page in headless Chromium against a real server: it pairs
//! from a link, sees a branch appear and settle over the event stream,
//! sends a follow-up from the branch page, shows each turn's own cost on its
//! receipt, decides from the deck, shows which files two open branches
//! hold, and lists a problem on its home page only while there is one.
//! Driven by Playwright
//! (`companion_browser.js`); skipped, saying so, when Node or Playwright's
//! module is not installed. `BY_TEST_PLAYWRIGHT` names the module's
//! directory; otherwise `$(npm root -g)/playwright` is tried, with
//! `PLAYWRIGHT_BROWSERS_PATH` (default `/opt/pw-browsers` when present).

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use branchyard::effects::{Approval, ApprovalPolicy, ApprovalSettings};
use branchyard::{Envelope, MessageKind};
use branchyard_client::api::{OperationState, PolicySpec, SendRequest, SpawnRequest, TaskRequest};
use branchyard_client::new_key;
use branchyard_server::companion::{link, store};
use branchyard_testkit::wait;
use common::{await_operation, costly_task, run, task, Fixture, Server};

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

/// What the fake agent replies on the `report` branch: the markdown a
/// harness writes, with a JSON fence, a web link and a relative one.
const MARKDOWN_REPLY: &str = "\
Done. I changed **two files** and kept `Record` in line order.

## What changed
- `src/record.rs` keeps fields in a `Vec`
- `tests/parser.rs` covers the repeated key
  - nested note

| Plan | Lost % |
|---|---:|
| Starter | 21.7 |

See [the logfmt spec](https://brandur.org/logfmt) and [a local file](src/record.rs).

```json
{\"summary\": \"Two files changed.\", \"files\": 2, \"ok\": true}
```

```sh
cargo test --test parser
```
";

#[test]
fn the_page_pairs_follows_the_stream_and_sends() {
    let Some(module) = playwright() else {
        eprintln!("skipped: no Playwright module (set BY_TEST_PLAYWRIGHT)");
        return;
    };
    let f = Fixture::new();
    let mut config = f.config();
    config.app.enabled = true;
    common::costly_harness(&mut config);
    // No push service is reachable here; the page's in-page notices remain.
    config.app.push = false;
    // A delegating parent runs a child that asks it a question. The child's
    // `ask` is Branchyard's own MCP server, which the fake agent starts
    // from the `by` the engine names, so the real `by` is built here.
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
    // approval the page decides.
    config.approvals = ApprovalSettings {
        person: Some(ApprovalPolicy {
            rules: [("write *".to_owned(), Approval::Ask)].into(),
            ..ApprovalPolicy::default()
        }),
        ..ApprovalSettings::default()
    };
    // An operation requiring a label no worker carries is a problem as soon
    // as it is queued, so the page's problems strip has one to show.
    config.unclaimable_after = Duration::ZERO;
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("browser".into()),
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

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/companion_browser.js"
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
    expect("LOADED");
    // A branch made elsewhere appears on the open page.
    let op = run(&server.client(), &task("WRITE a.txt=1", "first"));
    assert!(op.state.is_terminal());
    writeln!(stdin, "go").unwrap();
    expect("SEEN first");
    expect("FILTERED");
    let background = "review-quarterly-research-evidence-and-customer-interviews-for-final-report";
    let op = run(&server.client(), &task("WRITE c.txt=3", background));
    assert!(op.state.is_terminal());
    writeln!(stdin, "refresh").unwrap();
    expect("FILTER PRESERVED");
    expect("SENT");
    // A reply written as markdown, with a JSON fence, as a harness would
    // write one; the page renders it as elements.
    let reply = f.dir.join("reply.md");
    std::fs::write(&reply, MARKDOWN_REPLY).unwrap();
    let op = run(
        &server.client(),
        &task(&format!("REPLY_FILE {}", reply.display()), "report"),
    );
    assert!(op.state.is_terminal());
    writeln!(stdin, "report").unwrap();
    expect("RENDERED");
    // Two turns of a harness that reports its session's running total: each
    // receipt shows the turn's own cost.
    let op = run(&server.client(), &costly_task("SPEND=0.25", "costly"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let again = server
        .client()
        .repo("app")
        .send(
            "costly",
            &SendRequest {
                prompt: "SPEND=0.5".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    let again = await_operation(&server.client(), &again.id);
    assert_eq!(again.state, OperationState::Succeeded, "{again:?}");
    writeln!(stdin, "costly").unwrap();
    expect("COSTED");
    // A turn the policy parks on an approval before it writes: the page
    // decides it. The task finishes once the page has allowed it.
    let asker = server
        .client()
        .repo("app")
        .submit_task(&task("PERMISSION WRITE d.txt=4", "asker"), &new_key())
        .unwrap();
    writeln!(stdin, "asker").unwrap();
    expect("DECIDED");
    let done = await_operation(&server.client(), &asker.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    // A delegating parent, and a child that asks it which parser to use and
    // waits for the answer. The page's deck shows the question and a person
    // answers it; the child's wait ends with that text.
    let boss = run(
        &server.client(),
        &TaskRequest {
            delegation: Some(Envelope::depth(2)),
            ..task("say hi", "boss")
        },
    );
    assert_eq!(boss.state, OperationState::Succeeded, "{boss:?}");
    let kid = server
        .client()
        .repo("app")
        .spawn(
            "boss",
            &SpawnRequest {
                prompt: r#"MCP ask {"text": "Which parser?", "wait_seconds": 60}"#.into(),
                name: Some("kid".into()),
                policy: PolicySpec::allow_all(),
                ..SpawnRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    // The question is in the parent's inbox, and nothing has answered it.
    let boss_inbox =
        |client: &branchyard_client::Client| client.repo("app").inbox("boss").unwrap().messages;
    let question = wait::until("the child's question to reach its parent", || {
        boss_inbox(&server.client())
            .into_iter()
            .find(|m| m.kind == MessageKind::Question)
            .ok_or_else(|| "no question yet".to_owned())
    });
    assert_eq!(
        (question.from.as_str(), question.text.as_str()),
        ("kid", "Which parser?")
    );
    writeln!(stdin, "question").unwrap();
    expect("ASKED");
    // The page shows it, and the child stays parked: nothing has answered.
    assert!(
        !server
            .client()
            .operation(&kid.id)
            .unwrap()
            .state
            .is_terminal(),
        "the child finished before it was answered"
    );
    assert!(server
        .client()
        .repo("app")
        .inbox("kid")
        .unwrap()
        .messages
        .is_empty());
    writeln!(stdin, "answer").unwrap();
    expect("ANSWERED");
    let done = await_operation(&server.client(), &kid.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    // The answer went to the child as a reply to that question, from the
    // parent the person answered as, and the child's wait ended with it.
    let answers = server.client().repo("app").inbox("kid").unwrap().messages;
    let [answer] = answers.as_slice() else {
        panic!("the child's inbox is {answers:?}, not one answer");
    };
    assert_eq!(answer.kind, MessageKind::Answer);
    assert_eq!(answer.in_reply_to, Some(question.id));
    assert_eq!(
        (answer.from.as_str(), answer.text.as_str()),
        ("boss", "Use the second parser.")
    );
    let said = server
        .client()
        .repo("app")
        .events("kid", 0)
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.activity {
            branchyard::Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<String>();
    assert!(
        said.contains("Use the second parser."),
        "the child's wait ended without the answer: {said}"
    );

    // Another open branch writes one of first's files: the page's ground
    // plan names it on first's page and on first's merge card.
    let op = run(&server.client(), &task("WRITE a.txt=rival", "rival"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    writeln!(stdin, "ground").unwrap();
    expect("GROUND");

    let mut gpu = task("WRITE gpu.txt=1", "needs-gpu");
    gpu.require_labels = vec!["gpu".into()];
    let stuck = server
        .client()
        .repo("app")
        .submit_task(&gpu, &new_key())
        .unwrap();
    wait::until("the operation to be listed as a problem", || {
        server
            .client()
            .problems()
            .unwrap()
            .problems
            .iter()
            .any(|p| p.key == stuck.id)
    });
    writeln!(stdin, "problem {}", stuck.id).unwrap();
    expect("PROBLEM");

    expect("DONE");
    assert!(child.wait().unwrap().success());

    // The send went through the API as the paired principal.
    let repo = server.client().repo("app");
    let branch = repo.branch("first").unwrap();
    assert_eq!(branch.turns, 2);
    let events = repo.events("first", 0).unwrap();
    assert!(events.events.iter().any(|e| matches!(
        &e.activity,
        branchyard::Activity::Prompt(p) if p.contains("WRITE b.txt=2")
    )));
}
