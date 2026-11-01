//! `GET /v1/projects/{project}/decisions`: a workspace's finished output and
//! its stopped sessions are what waits for a person, and each is gone once
//! the person has dealt with it.

use super::*;

/// A CSV of two lines and the manifest that names it, written by a session
/// whose fake agent runs shell commands.
const PUBLISH_CSV: &str = concat!(
    "SH mkdir -p .cache/outputs && printf 'ticket,state\\n1,open\\n' > .cache/outputs/backlog.csv && ",
    "printf %s '{\"schema_version\":1,\"files\":[{\"path\":\"backlog.csv\",\"media_type\":\"text/csv\",",
    "\"sha256\":\"c780e97aaf57552bc8d1ffe53265d5cba4121024f09dc7d0c08f7518ad7a8485\"}]}' ",
    "> .cache/outputs/artifacts.json && printf '\\n```json\\n{\"answer\":\"counted\"}\\n```\\n'"
);

fn deliverable() -> branchyard::ResultContract {
    let mut contract = branchyard::ResultContract::deliverable(json!({
        "type":"object","required":["answer"],"properties":{"answer":{"type":"string"}},"additionalProperties":false
    }))
    .unwrap();
    contract.capture = Some(branchyard::ResultCapture::new(
        ".cache/outputs/artifacts.json",
    ));
    contract
}

async fn start(fixture: &Fixture, wid: &str, key: &str, body: Value) -> Value {
    let (status, started) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some(key),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    fixture.finished(started["id"].as_str().unwrap());
    started
}

async fn decisions(fixture: &Fixture, project: &str, token: &str) -> (StatusCode, Value) {
    fixture
        .json(
            Method::GET,
            &format!("/v1/projects/{project}/decisions"),
            token,
            None,
            Value::Null,
        )
        .await
}

fn kinds(list: &Value) -> Vec<(String, String)> {
    list["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["kind"].as_str().unwrap().to_owned(),
                d["session_name"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_output_and_a_failed_session_wait_until_they_are_dealt_with() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let project = fixture.detail(&wid).await["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, none) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(status, StatusCode::OK, "{none}");
    assert_eq!(
        none["decisions"],
        json!([]),
        "a new workspace waits on nobody"
    );

    let writer = start(
        &fixture,
        &wid,
        "writer",
        json!({"name":"Analyst","prompt":PUBLISH_CSV,"harness":"gemini-cli","access":"writer","result_contract":deliverable()}),
    )
    .await;
    let (_, artifacts) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/artifacts"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    let aid = artifacts["artifacts"][0]["id"].as_str().unwrap().to_owned();
    let (status, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(
        kinds(&list),
        [("output".into(), "Analyst".into())],
        "{list}"
    );
    let d = &list["decisions"][0];
    assert_eq!(d["waits"], "delivery");
    assert_eq!(d["workspace"], wid.as_str());
    assert_eq!(d["workspace_title"], "Evidence synthesis");
    assert_eq!(d["session"], writer["session_id"]);
    assert_eq!(d["branch"], writer["session_id"]);
    assert_eq!(d["status"]["state"], "no_changes");
    assert_eq!(
        d["output"],
        json!({"id":aid,"name":"backlog.csv","media_type":"text/csv","size":20,"turn":1}),
        "{d}"
    );
    assert!(d["since_ms"].as_u64().unwrap() > 0);

    // Another person's token, and one that is no member, see nothing of it.
    let (status, _) = decisions(&fixture, &project, FOREIGN).await;
    assert!(
        matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
        "{status}"
    );
    let (status, outsider) = decisions(&fixture, &project, EDITOR).await;
    assert_eq!(status, StatusCode::OK, "{outsider}");
    assert_eq!(outsider["decisions"], json!([]));
    let (status, _) = decisions(&fixture, "no-such-project", OWNER).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A session whose agent exits mid-turn stopped: nobody waits on it, so
    // it follows the output.
    start(
        &fixture,
        &wid,
        "flaky",
        json!({"name":"Flaky","prompt":"EXIT","harness":"gemini-cli","access":"writer"}),
    )
    .await;
    let (_, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(
        kinds(&list),
        [
            ("output".into(), "Analyst".into()),
            ("session".into(), "Flaky".into())
        ],
        "{list}"
    );
    let stopped = &list["decisions"][1];
    assert_eq!(stopped["waits"], "nobody");
    assert_eq!(stopped["status"]["state"], "failed");
    assert!(
        stopped["phase"].as_str().unwrap().starts_with("Failed"),
        "{stopped}"
    );
    assert!(stopped["output"].is_null());

    // Accepting the output settles it; the stopped session stays.
    let current = fixture.detail(&wid).await;
    let (status, accepted) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/finalize"),
            OWNER,
            Some("accept"),
            json!({"revision":current["revision"],"artifacts":[aid]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let (_, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(kinds(&list), [("session".into(), "Flaky".into())], "{list}");

    // Starting another session means the person moved on from the failed one.
    start(
        &fixture,
        &wid,
        "next",
        json!({"name":"Next","prompt":"WRITE a.txt=1","harness":"gemini-cli","access":"writer"}),
    )
    .await;
    let (_, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(list["decisions"], json!([]), "{list}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_session_waits_until_it_is_resumed() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let project = fixture.detail(&wid).await["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, started) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some("hang"),
            json!({"name":"Long reader","prompt":"HANG","harness":"gemini-cli","access":"writer"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let sid = started["session_id"].as_str().unwrap().to_owned();
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &wid)
        .unwrap()
        .unwrap();
    let repo = super::super::runtime::resolve(&fixture.app, &workspace).unwrap();
    branchyard_testkit::wait::until("the hanging turn to run", || {
        repo.yard
            .branch(&sid)
            .is_ok_and(|b| b.info().status == branchyard::BranchStatus::Running)
    });
    let (_, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(
        list["decisions"],
        json!([]),
        "a running session waits on nobody: {list}"
    );

    let path = format!("/v1/workspaces/{wid}/sessions/{sid}");
    let (status, cancel) = fixture
        .json(
            Method::POST,
            &format!("{path}/cancel"),
            OWNER,
            Some("cancel"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cancel}");
    fixture.finished(cancel["id"].as_str().unwrap());
    fixture.finished(started["id"].as_str().unwrap());
    let (_, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(
        kinds(&list),
        [("session".into(), "Long reader".into())],
        "{list}"
    );
    let d = &list["decisions"][0];
    assert_eq!(
        (&d["waits"], &d["status"]["state"], &d["phase"]),
        (
            &json!("nobody"),
            &json!("interrupted"),
            &json!("Interrupted")
        ),
        "{d}"
    );

    // Resuming it, as the card's first action does, settles it.
    let (status, resumed) = fixture
        .json(
            Method::POST,
            &format!("{path}/messages"),
            OWNER,
            Some("resume"),
            json!({"prompt":"WHOAMI","action":"resume"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    fixture.finished(resumed["id"].as_str().unwrap());
    let (_, list) = decisions(&fixture, &project, OWNER).await;
    assert_eq!(list["decisions"], json!([]), "{list}");
}
