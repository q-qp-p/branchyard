//! Connections and session tools from the console, against a mock of
//! Anvil's connect routes (`/connect/status`, `/connect/start`,
//! `/connect/api-key`, as `packages/cli/src/commands/serve-fleet-http.ts`
//! in Anvil answers them). Hermetic: no gateway, no model.

use super::*;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use branchyard::connectors::GrantEntry;
use branchyard_testkit::{MockHttp, Response};

/// A connectors section pointing at `gateway`, serving `bundles`.
fn connectors(
    root: &std::path::Path,
    gateway: &str,
    bundles: &[&str],
) -> crate::config::ConnectorsConfig {
    let dir = root.join("bundles");
    for id in bundles {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        std::fs::write(dir.join(id).join("air.yaml"), format!("service: {id}\n")).unwrap();
    }
    crate::config::ConnectorsConfig {
        gateway: format!("{gateway}/mcp"),
        sandbox_gateway: None,
        issuer: Some("https://by.example".into()),
        signing_key: None,
        audit_file: None,
        bundles: dir,
        anvil: vec!["/bin/false".into()],
        run_gateway: false,
        listen: None,
        vault_key: None,
        effects_proxy: None,
        effects_listen: None,
        effects_sandbox_host: None,
    }
}

/// A gateway where the person has Atlassian connected and nothing else;
/// GitHub signs in with OAuth and Linear takes a key.
fn gateway() -> MockHttp {
    MockHttp::start(|request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/connect/status") => Response::json(
                200,
                &json!({"sub":"x","connections":[
                    {"connector":"atlassian","account":"default","kind":"oauth","status":"active","updatedAt":"2026-11-04T00:00:00Z"},
                    {"connector":"github","account":"old","kind":"oauth","status":"needs_reconnect","updatedAt":"2026-11-01T00:00:00Z"}
                ]}),
            ),
            ("POST", "/connect/start") if body["connector"] == "github" => Response::json(
                200,
                &json!({"kind":"oauth","connector":"github","url":"https://github.com/login/oauth/authorize?state=s","expires_at":1}),
            ),
            ("POST", "/connect/start") => Response::json(
                200,
                &json!({"kind":"static","connector":body["connector"],"submit":"/connect/api-key"}),
            ),
            ("POST", "/connect/api-key") => Response::json(
                200,
                &json!({"connected":true,"connector":body["connector"],"account":"default"}),
            ),
            _ => Response::json(
                404,
                &json!({"error":{"code":"not_found","message":"No such route."}}),
            ),
        }
    })
}

/// The claims of the bearer token a request carried.
fn claims(request: &branchyard_testkit::Request) -> Value {
    let token = request
        .header("authorization")
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap();
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap()
}

fn configured(mock: &MockHttp, delegation: bool) -> Fixture {
    let url = format!("http://{}", mock.addr());
    Fixture::with(true, move |config, root| {
        config.connectors = Some(connectors(
            root,
            &url,
            &["atlassian", "github", "linear", "slack"],
        ));
        config.allow_delegation = delegation;
        config.ceilings.insert(
            "owner".into(),
            branchyard::Ceiling {
                connectors: Some(vec![
                    GrantEntry::parse("atlassian:write").unwrap(),
                    GrantEntry::parse("github:read").unwrap(),
                    GrantEntry::parse("linear:write").unwrap(),
                ]),
                ..Default::default()
            },
        );
    })
}

#[tokio::test]
async fn without_a_gateway_the_console_says_so_and_refuses_connectors() {
    let fixture = Fixture::new(true);
    let (status, list) = fixture
        .json(Method::GET, "/v1/connections", OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["gateway"], "not_configured");
    assert!(list["reason"].as_str().unwrap().contains("administrator"));
    let wid = fixture.workspace().await;
    let (status, tools) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/tools"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    let skills: Vec<&str> = tools["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert!(
        skills.contains(&"docx") && skills.contains(&"pptx"),
        "{skills:?}"
    );
    assert_eq!(tools["helpers"]["allowed"], false);
    assert!(tools["helpers"]["reason"].is_string());
    let route = format!("/v1/workspaces/{wid}/sessions");
    let (status, error) = fixture
        .json(
            Method::POST,
            &route,
            OWNER,
            Some("no-gateway"),
            json!({"name":"Jira","prompt":"Hello","harness":"gemini-cli","access":"writer","connectors":["atlassian"]}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "connectors_not_configured");
    let (status, error) = fixture
        .json(
            Method::POST,
            &route,
            OWNER,
            Some("helpers-off"),
            json!({"name":"Helpers","prompt":"Hello","harness":"gemini-cli","access":"writer","helpers":{"harnesses":["gemini-cli"]}}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "delegation_not_allowed");
}

#[tokio::test]
async fn connections_report_each_state_and_sign_as_the_caller() {
    let mock = gateway();
    let fixture = configured(&mock, false);
    let (status, list) = fixture
        .json(Method::GET, "/v1/connections", OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["gateway"], "ready");
    let by_id = |id: &str| {
        list["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["connector"] == id)
            .cloned()
            .unwrap()
    };
    let atlassian = by_id("atlassian");
    assert_eq!(atlassian["state"], "connected");
    assert_eq!(atlassian["name"], "Atlassian");
    assert_eq!(atlassian["max_mode"], "write");
    assert_eq!(atlassian["accounts"][0]["account"], "default");
    let github = by_id("github");
    assert_eq!(github["state"], "needs_reconnect");
    assert_eq!(github["max_mode"], "read");
    assert!(github["reason"].as_str().unwrap().contains("Reconnect"));
    assert_eq!(by_id("linear")["state"], "not_connected");
    let slack = by_id("slack");
    assert_eq!(slack["state"], "blocked");
    assert!(slack.get("max_mode").is_none());
    // The gateway was asked with a connect token for this caller.
    let asked = mock.requests();
    let status_call = asked.iter().find(|r| r.path == "/connect/status").unwrap();
    let token = claims(status_call);
    assert_eq!(token["by_purpose"], "connect");
    assert_eq!(token["sub"], "owner");
    assert_eq!(token["by_tenant"], "default");
    assert!(token["by_grants"].as_array().is_none_or(Vec::is_empty));
    assert!(token["exp"].as_u64().unwrap() - token["iat"].as_u64().unwrap() <= 600);

    let (status, started) = fixture
        .json(
            Method::POST,
            "/v1/connections/github/connect",
            OWNER,
            None,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["kind"], "oauth");
    assert!(started["url"]
        .as_str()
        .unwrap()
        .starts_with("https://github.com/"));
    assert!(started.to_string().find("Bearer").is_none());
    let (status, started) = fixture
        .json(
            Method::POST,
            "/v1/connections/linear/connect",
            OWNER,
            None,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["kind"], "key");
    let (status, stored) = fixture
        .json(
            Method::POST,
            "/v1/connections/linear/key",
            OWNER,
            None,
            json!({"api_key":"lin_api_secret_value"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(stored["connected"], true);
    assert!(!stored.to_string().contains("lin_api_secret_value"));
    let forwarded = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/connect/api-key")
        .unwrap();
    assert_eq!(forwarded.json()["api_key"], "lin_api_secret_value");
    // Refused before the gateway is asked: blocked, not served, bad names.
    for (path, code) in [
        ("/v1/connections/slack/connect", "connector_over_ceiling"),
        ("/v1/connections/notion/connect", "connector_not_served"),
    ] {
        let (status, error) = fixture
            .json(Method::POST, path, OWNER, None, json!({}))
            .await;
        assert!(status.is_client_error(), "{path}: {error}");
        assert_eq!(error["error"]["code"], code, "{path}");
    }
    let (status, _) = fixture
        .json(
            Method::POST,
            "/v1/connections/github/connect",
            OWNER,
            None,
            json!({"account":"no spaces"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_session_takes_only_connectors_the_caller_can_use() {
    let mock = gateway();
    let fixture = configured(&mock, true);
    let wid = fixture.workspace().await;
    let route = format!("/v1/workspaces/{wid}/sessions");
    let start = |key: &'static str, extra: Value| {
        let mut body =
            json!({"name":key,"prompt":"Hello","harness":"gemini-cli","access":"writer"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let fixture = &fixture;
        let route = route.clone();
        async move {
            fixture
                .json(Method::POST, &route, OWNER, Some(key), body)
                .await
        }
    };
    for (key, extra, status, code) in [
        (
            "not-connected",
            json!({"connectors":["linear"]}),
            StatusCode::CONFLICT,
            "connector_not_connected",
        ),
        (
            "reconnect",
            json!({"connectors":["github"]}),
            StatusCode::CONFLICT,
            "connector_not_connected",
        ),
        (
            "blocked",
            json!({"connectors":["slack"]}),
            StatusCode::FORBIDDEN,
            "connector_over_ceiling",
        ),
        (
            "read-only",
            json!({"connectors":["github:write"]}),
            StatusCode::FORBIDDEN,
            "connector_over_ceiling",
        ),
        (
            "not-served",
            json!({"connectors":["notion"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "connector_not_served",
        ),
        (
            "both",
            json!({"connectors":["atlassian"],"provision":{"connectors":["atlassian"]}}),
            StatusCode::BAD_REQUEST,
            "connectors_conflict",
        ),
        (
            "skill",
            json!({"skills":["docx","nope"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "skill_unknown",
        ),
        (
            "helper",
            json!({"helpers":{"harnesses":["no-such-harness"]}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "harness_unavailable",
        ),
    ] {
        let (got, error) = start(key, extra).await;
        assert_eq!(got, status, "{key}: {error}");
        assert_eq!(error["error"]["code"], code, "{key}");
    }
    let (status, detail) = start("not-connected", json!({"connectors":["linear"]})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(detail["error"]["detail"]["action"], "connect");
    assert_eq!(detail["error"]["detail"]["connector"], "linear");
    assert!(fixture
        .app
        .registry
        .console_list(Collection::Sessions, "default")
        .unwrap()
        .is_empty());

    let (status, started) = start(
        "chosen",
        json!({"connectors":["atlassian:write"],"skills":["pptx"],"helpers":{"harnesses":["claude-code-acp"],"max_children":2}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let sid = started["session_id"].as_str().unwrap();
    let row = fixture
        .app
        .registry
        .console_get(Collection::Sessions, sid)
        .unwrap()
        .unwrap();
    assert_eq!(row.body["tools"]["connectors"][0]["connector"], "atlassian");
    assert_eq!(row.body["tools"]["connectors"][0]["mode"], "write");
    assert_eq!(row.body["tools"]["skills"], json!(["pptx"]));
    assert_eq!(row.body["tools"]["helpers"]["max_children"], 2);
    let operation = fixture
        .app
        .registry
        .get(started["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let text = serde_json::to_string(&operation).unwrap();
    assert!(!text.contains("lin_api"), "{text}");
}
