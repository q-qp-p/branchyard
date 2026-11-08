//! Tool policy, workspace defaults, the one tool catalogue and the session
//! start's `tools` field, against a mock of Anvil's connect routes.
//! Hermetic: no gateway, no model.

use super::*;

use branchyard::connectors::GrantEntry;
use branchyard_testkit::MockHttp;

/// The server of `connections.rs`'s tests, plus: `owner` is in the `eng`
/// group, and `editor` lacks the `admin` scope.
fn fixture(mock: &MockHttp) -> Fixture {
    let url = format!("http://{}", mock.addr());
    Fixture::with(true, move |config, root| {
        config.connectors = Some(super::connections::connectors(
            root,
            &url,
            &["atlassian", "github", "linear", "slack"],
        ));
        config.allow_delegation = true;
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
        config
            .principals
            .get_mut("owner")
            .unwrap()
            .groups
            .insert("eng".into());
        config
            .principals
            .get_mut("editor")
            .unwrap()
            .scopes
            .remove("admin");
    })
}

impl Fixture {
    async fn put_policy(
        &self,
        token: &str,
        key: &str,
        policy: Value,
        revision: Option<u64>,
    ) -> (StatusCode, Value) {
        let mut body = json!({ "policy": policy });
        if let Some(revision) = revision {
            body["revision"] = json!(revision);
        }
        self.json(
            Method::PUT,
            &format!("/v1/admin/policies/{key}"),
            token,
            None,
            body,
        )
        .await
    }

    async fn catalogue(&self, wid: Option<&str>) -> Vec<Value> {
        let path = match wid {
            Some(wid) => format!("/v1/tools?workspace={wid}"),
            None => "/v1/tools".to_owned(),
        };
        let (status, list) = self
            .json(Method::GET, &path, OWNER, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{list}");
        list["tools"].as_array().unwrap().clone()
    }
}

fn tool<'a>(tools: &'a [Value], kind: &str, id: &str) -> &'a Value {
    tools
        .iter()
        .find(|t| t["kind"] == kind && t["id"] == id)
        .unwrap_or_else(|| panic!("no {kind} {id} in the catalogue"))
}

#[tokio::test]
async fn policy_routes_need_admin_and_act_only_on_the_callers_tenant() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let policy = json!({"skills":["docx"]});
    // No admin scope: refused on every route.
    let (status, error) = fixture
        .put_policy(EDITOR, "tenant:default", policy.clone(), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "scope_required");
    assert_eq!(error["error"]["detail"]["scope"], "admin");
    for (method, path) in [
        (Method::GET, "/v1/admin/policies"),
        (Method::GET, "/v1/admin/policies/tenant:default"),
        (Method::DELETE, "/v1/admin/policies/tenant:default"),
    ] {
        let (status, error) = fixture
            .json(method.clone(), path, EDITOR, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {error}");
    }

    // Create, then replace only at the stored revision.
    let (status, created) = fixture
        .put_policy(OWNER, "tenant:default", policy.clone(), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["revision"], 1);
    assert_eq!(created["key"], "tenant:default");
    assert_eq!(created["policy"], policy);
    let (status, error) = fixture
        .put_policy(OWNER, "tenant:default", policy.clone(), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "revision_conflict");
    let (status, error) = fixture
        .put_policy(OWNER, "tenant:default", policy.clone(), Some(7))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let (status, error) = fixture
        .put_policy(OWNER, "group:default:eng", policy.clone(), Some(1))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a new key takes no revision: {error}"
    );
    let wider = json!({"skills":["docx","pptx"],"delegation":false});
    let (status, replaced) = fixture
        .put_policy(OWNER, "tenant:default", wider.clone(), Some(1))
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["revision"], 2);
    let (status, error) = fixture
        .put_policy(OWNER, "tenant:default", wider.clone(), Some(1))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "stale: {error}");

    // Invalid policies change nothing.
    for (key, bad, code) in [
        (
            "tenant:default",
            json!({"skills":["nope"]}),
            "skill_unknown",
        ),
        (
            "tenant:default",
            json!({"helpers":["no-such-harness"]}),
            "harness_unavailable",
        ),
    ] {
        let (status, error) = fixture.put_policy(OWNER, key, bad, Some(2)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["error"]["code"], code);
    }
    let (status, _) = fixture
        .put_policy(
            OWNER,
            "tenant:default",
            json!({"connectors":["not a connector"]}),
            Some(2),
        )
        .await;
    assert!(status.is_client_error());
    let (status, error) = fixture
        .put_policy(OWNER, "tenant:default", json!({"skills":"docx"}), Some(2))
        .await;
    assert!(status.is_client_error(), "{error}");

    // The tenant comes from the caller: another tenant's key is invalid, and
    // a body cannot name one.
    for key in [
        "tenant:foreign",
        "group:foreign:eng",
        "principal:foreign:owner",
        "default",
    ] {
        let (status, error) = fixture.put_policy(OWNER, key, policy.clone(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {error}");
        assert_eq!(error["error"]["code"], "policy_key_invalid", "{key}");
    }
    let (status, error) = fixture
        .json(
            Method::PUT,
            "/v1/admin/policies/tenant:default",
            OWNER,
            None,
            json!({"policy":policy,"revision":2,"tenant":"foreign"}),
        )
        .await;
    assert!(status.is_client_error(), "{error}");
    let (status, error) = fixture
        .put_policy(FOREIGN, "tenant:default", policy.clone(), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let (status, mine) = fixture
        .put_policy(FOREIGN, "tenant:foreign", json!({"skills":[]}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    // The tenant's version counter sits beside its policies; it is not one.
    let stored = |tenant: &str| {
        fixture
            .app
            .registry
            .console_list(Collection::Policies, tenant)
            .unwrap()
            .into_iter()
            .filter(|row| !row.id.starts_with("version:"))
            .collect::<Vec<_>>()
    };
    assert_eq!(stored("default").len(), 1);
    assert_eq!(stored("default")[0].revision, 2);
    assert_eq!(stored("foreign").len(), 1);

    // Each tenant lists and reads only its own.
    let (_, list) = fixture
        .json(Method::GET, "/v1/admin/policies", OWNER, None, Value::Null)
        .await;
    assert_eq!(list["policies"].as_array().unwrap().len(), 1);
    assert_eq!(list["policies"][0]["key"], "tenant:default");
    assert_eq!(list["policies"][0]["policy"], wider);
    let (_, list) = fixture
        .json(
            Method::GET,
            "/v1/admin/policies",
            FOREIGN,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(list["policies"][0]["key"], "tenant:foreign");
    assert_eq!(list["policies"].as_array().unwrap().len(), 1);
    let (status, _) = fixture
        .json(
            Method::GET,
            "/v1/admin/policies/tenant:default",
            FOREIGN,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Delete is revision-guarded, and the key is gone afterwards.
    let path = "/v1/admin/policies/tenant:default";
    let (status, error) = fixture
        .json(
            Method::DELETE,
            &format!("{path}?revision=1"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let (status, deleted) = fixture
        .json(
            Method::DELETE,
            &format!("{path}?revision=2"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    for method in [Method::GET, Method::DELETE] {
        let (status, error) = fixture.json(method, path, OWNER, None, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
    }
    assert!(stored("default").is_empty());
}

#[tokio::test]
async fn workspace_defaults_are_stored_by_revision_for_owners_and_editors() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let wid = fixture.workspace().await;
    let route = format!("/v1/workspaces/{wid}/tools");
    let (status, tools) = fixture
        .json(Method::GET, &route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["defaults"], json!({"tools":[],"revision":0}));

    let chosen = json!([
        {"kind":"connector","id":"atlassian","mode":"write"},
        {"kind":"skill","id":"docx"},
        {"kind":"helper","id":"claude-code-acp"}
    ]);
    let put = |token: &'static str, tools: Value, revision: u64| {
        let fixture = &fixture;
        let route = route.clone();
        async move {
            fixture
                .json(
                    Method::PUT,
                    &route,
                    token,
                    None,
                    json!({"tools":tools,"revision":revision}),
                )
                .await
        }
    };
    let (status, saved) = put(OWNER, chosen.clone(), 0).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["revision"], 1);
    assert_eq!(saved["tools"], chosen);
    let (status, error) = put(OWNER, chosen.clone(), 0).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "revision_conflict");
    let (status, error) = put(OWNER, json!([]), 5).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let (_, tools) = fixture
        .json(Method::GET, &route, OWNER, None, Value::Null)
        .await;
    assert_eq!(
        tools["defaults"]["tools"], chosen,
        "the conflict changed nothing"
    );

    // Invalid choices are refused before they are stored.
    for (bad, status, code) in [
        (
            json!([{"kind":"skill","id":"nope"}]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "skill_unknown",
        ),
        (
            json!([{"kind":"helper","id":"nope"}]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "harness_unavailable",
        ),
        (
            json!([{"kind":"skill","id":"docx","mode":"write"}]),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json!([{"kind":"skill","id":"docx"},{"kind":"skill","id":"docx"}]),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
    ] {
        let (got, error) = put(OWNER, bad.clone(), 1).await;
        assert_eq!(got, status, "{bad}: {error}");
        assert_eq!(error["error"]["code"], code, "{bad}");
    }

    // An editor may; a viewer may not; a stranger is turned away.
    let detail = fixture.detail(&wid).await;
    for (subject, role, key) in [("editor", "editor", "m1"), ("tokenonly", "viewer", "m2")] {
        let (status, member) = fixture
            .json(
                Method::POST,
                &format!("/v1/workspaces/{wid}/members/{subject}"),
                OWNER,
                Some(key),
                json!({"revision":fixture.detail(&wid).await["revision"],"role":role}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{member} {detail}");
    }
    let (status, saved) = put(EDITOR, json!([{"kind":"skill","id":"pptx"}]), 1).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["revision"], 2);
    let (status, error) = put(TOKENONLY, json!([]), 2).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    let (status, _) = put(FOREIGN, json!([]), 2).await;
    assert!(status.is_client_error());
    let (_, tools) = fixture
        .json(Method::GET, &route, TOKENONLY, None, Value::Null)
        .await;
    assert_eq!(
        tools["defaults"],
        json!({"tools":[{"kind":"skill","id":"pptx"}],"revision":2}),
        "a viewer reads them"
    );
}

#[tokio::test]
async fn the_catalogue_reports_each_tool_at_the_first_state_it_fails() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let wid = fixture.workspace().await;
    // Nothing stored: the server's ceilings alone decide.
    let tools = fixture.catalogue(Some(&wid)).await;
    let slack = tool(&tools, "connector", "slack");
    assert_eq!(slack["state"], "not_allowed");
    assert_eq!(slack["allowed"], false);
    assert_eq!(slack["ready"], false);
    assert_eq!(slack["action"], "ask_admin");
    assert!(slack["max_mode"].is_null());
    let atlassian = tool(&tools, "connector", "atlassian");
    assert_eq!(atlassian["state"], "ready");
    assert_eq!(atlassian["max_mode"], "write");
    assert_eq!(atlassian["default"], false);
    assert!(atlassian["action"].is_null() && atlassian["reason"].is_null());
    let linear = tool(&tools, "connector", "linear");
    assert_eq!(linear["state"], "not_ready");
    assert_eq!(linear["allowed"], true);
    assert_eq!(linear["action"], "connect");
    assert_eq!(tool(&tools, "connector", "github")["action"], "connect");
    assert_eq!(tool(&tools, "connector", "github")["max_mode"], "read");
    let docx = tool(&tools, "skill", "docx");
    assert_eq!(
        (docx["state"].clone(), docx["ready"].clone()),
        (json!("ready"), json!(true))
    );
    assert!(docx["topic"].is_string() && docx["description"].is_string());
    let helper = tool(&tools, "helper", "claude-code-acp");
    assert_eq!(helper["allowed"], true);

    // The tenant narrows; the `eng` group (which `owner` is in) speaks to
    // skills only, so it narrows skills within the tenant's.
    for (key, policy) in [
        (
            "tenant:default",
            json!({
                "connectors":["atlassian:write","github:write","linear:write","slack:write"],
                "skills":["docx","pptx"],
                "helpers":["claude-code-acp"]
            }),
        ),
        ("group:default:eng", json!({"skills":["pptx","xlsx"]})),
    ] {
        let (status, put) = fixture.put_policy(OWNER, key, policy, None).await;
        assert_eq!(status, StatusCode::OK, "{put}");
    }
    let (status, defaults) = fixture
        .json(
            Method::PUT,
            &format!("/v1/workspaces/{wid}/tools"),
            OWNER,
            None,
            json!({"tools":[{"kind":"connector","id":"atlassian"},{"kind":"skill","id":"pptx"}],"revision":0}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{defaults}");
    let tools = fixture.catalogue(Some(&wid)).await;
    assert_eq!(tool(&tools, "skill", "pptx")["state"], "default");
    assert_eq!(tool(&tools, "skill", "pptx")["default"], true);
    assert_eq!(
        tool(&tools, "skill", "docx")["state"],
        "not_allowed",
        "outside the group's"
    );
    assert_eq!(tool(&tools, "skill", "docx")["action"], "ask_admin");
    assert_eq!(
        tool(&tools, "skill", "xlsx")["state"],
        "not_allowed",
        "outside the tenant's"
    );
    assert_eq!(tool(&tools, "connector", "atlassian")["state"], "default");
    assert_eq!(
        tool(&tools, "connector", "slack")["state"],
        "not_allowed",
        "the server's ceiling still applies"
    );
    assert_eq!(tool(&tools, "connector", "github")["max_mode"], "read");
    // Without a workspace there are no defaults.
    let tools = fixture.catalogue(None).await;
    assert_eq!(tool(&tools, "skill", "pptx")["state"], "ready");

    // The person's own record joins the group's.
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "principal:default:owner",
            json!({"skills":["docx"]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let tools = fixture.catalogue(None).await;
    assert_eq!(tool(&tools, "skill", "docx")["allowed"], true);
    assert_eq!(tool(&tools, "skill", "pptx")["allowed"], true);
    assert_eq!(tool(&tools, "skill", "xlsx")["allowed"], false);

    // Turning delegation off turns every helper off, with the reason.
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "principal:default:owner",
            json!({"delegation":false}),
            Some(1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let tools = fixture.catalogue(None).await;
    let helper = tool(&tools, "helper", "claude-code-acp");
    assert_eq!(helper["state"], "not_allowed");
    assert!(helper["reason"].as_str().unwrap().contains("helpers"));
    let (_, session) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/tools"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(session["helpers"]["allowed"], false);

    // Another tenant's policies never reach this caller.
    let (status, _) = fixture
        .put_policy(FOREIGN, "tenant:foreign", json!({"skills":[]}), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let tools = fixture.catalogue(None).await;
    assert_eq!(tool(&tools, "skill", "pptx")["allowed"], true);
    // A workspace the caller is not in is not described.
    let (status, error) = fixture
        .json(
            Method::GET,
            &format!("/v1/tools?workspace={wid}"),
            FOREIGN,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
}

#[tokio::test]
async fn a_start_with_tools_expands_and_is_refused_beyond_policy() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
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
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "tenant:default",
            json!({
                "connectors":["atlassian:read","github:read"],
                "skills":["docx"],
                "helpers":["claude-code-acp"]
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");

    let refused = |detail: &Value, kind: &str, id: &str| {
        assert_eq!(detail["error"]["code"], "tool_not_allowed", "{detail}");
        assert_eq!(detail["error"]["detail"]["kind"], kind, "{detail}");
        assert_eq!(detail["error"]["detail"]["id"], id, "{detail}");
    };
    let (status, error) = start("skill", json!({"tools":[{"kind":"skill","id":"pptx"}]})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "skill", "pptx");
    let (status, error) = start(
        "write",
        json!({"tools":[{"kind":"connector","id":"atlassian","mode":"write"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "connector", "atlassian");
    assert_eq!(error["error"]["detail"]["max_mode"], "read");
    let (status, error) = start(
        "linear",
        json!({"tools":[{"kind":"connector","id":"linear"}]}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "not in the tenant's list: {error}"
    );
    refused(&error, "connector", "linear");
    let (status, error) = start(
        "helper",
        json!({"tools":[{"kind":"helper","id":"gemini-cli"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "helper", "gemini-cli");
    // The old fields meet the same policy.
    let (status, error) = start("old-skill", json!({"skills":["pptx"]})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "skill", "pptx");
    let (status, error) = start(
        "old-helper",
        json!({"helpers":{"harnesses":["gemini-cli"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "helper", "gemini-cli");
    let (status, error) = start("provision", json!({"provision":{"connectors":["linear"]}})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "connector", "linear");
    // Both forms at once.
    let (status, error) = start("both", json!({"tools":[],"skills":["docx"]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["error"]["code"], "tools_conflict");
    // Bad shapes.
    let (status, error) = start(
        "unknown-skill",
        json!({"tools":[{"kind":"skill","id":"nope"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["error"]["code"], "skill_unknown");
    let (status, _) = start("bad-kind", json!({"tools":[{"kind":"widget","id":"x"}]})).await;
    assert!(status.is_client_error());
    assert!(
        fixture
            .app
            .registry
            .console_list(Collection::Sessions, "default")
            .unwrap()
            .is_empty(),
        "no refused start left a session"
    );

    // Within policy: expanded into the three paths.
    let (status, started) = start(
        "chosen",
        json!({"tools":[
            {"kind":"connector","id":"atlassian"},
            {"kind":"skill","id":"docx"},
            {"kind":"helper","id":"claude-code-acp"}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let row = |started: &Value| {
        fixture
            .app
            .registry
            .console_get(
                Collection::Sessions,
                started["session_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
    };
    let chosen = row(&started);
    assert_eq!(
        chosen.body["tools"]["connectors"][0]["connector"],
        "atlassian"
    );
    assert_eq!(chosen.body["tools"]["connectors"][0]["mode"], "read");
    assert_eq!(chosen.body["tools"]["skills"], json!(["docx"]));
    // The policy's skills travel with the session as the set a turn may
    // mount beside the chosen ones (docs/skill-registry.md, R2).
    assert_eq!(chosen.body["tools"]["allowed_skills"], json!(["docx"]));
    assert_eq!(
        chosen.body["tools"]["helpers"]["harnesses"],
        json!(["claude-code-acp"])
    );
    // The same request replays; the expansion is not part of its identity.
    let (status, replayed) = start(
        "chosen",
        json!({"tools":[
            {"kind":"connector","id":"atlassian"},
            {"kind":"skill","id":"docx"},
            {"kind":"helper","id":"claude-code-acp"}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["session_id"], started["session_id"]);

    // A list with no skill entries, like an unset choice, keeps the skills
    // policy allows: skills narrow, they are never taken away by omission.
    let (status, none) = start("none", json!({"tools":[]})).await;
    assert_eq!(status, StatusCode::OK, "{none}");
    assert_eq!(row(&none).body["tools"]["skills"], json!(["docx"]));
    let (status, plain) = start("plain", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(row(&plain).body["tools"]["skills"], json!(["docx"]));

    // Delegation off by policy refuses helpers by name.
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "principal:default:owner",
            json!({"delegation":false}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let (status, error) = start(
        "no-delegation",
        json!({"tools":[{"kind":"helper","id":"claude-code-acp"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    refused(&error, "helper", "*");
    // Policy applies at once, with no restart: lifting it admits again.
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "principal:default:owner",
            json!({"delegation":true}),
            Some(1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let (status, ok) = start(
        "delegation-back",
        json!({"tools":[{"kind":"helper","id":"claude-code-acp"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ok}");
}

#[tokio::test]
async fn the_policy_version_rises_with_every_change_and_is_never_a_policy() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let tenant = "default";
    let version = || crate::console::policy::current_version(&fixture.app, tenant).unwrap();
    let list = || async {
        let (status, list) = fixture
            .json(Method::GET, "/v1/admin/policies", OWNER, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{list}");
        list
    };
    assert_eq!(version(), 0, "no policy, no version");
    assert_eq!(list().await["version"], 0);

    let (status, put) = fixture
        .put_policy(OWNER, "tenant:default", json!({"skills":["docx"]}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(version(), 1);
    // A group's and a person's policy count for the tenant too.
    let (status, put) = fixture
        .put_policy(OWNER, "group:default:eng", json!({"skills":["pptx"]}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(version(), 2);
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "tenant:default",
            json!({"skills":["docx","pptx"]}),
            Some(1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(version(), 3);
    // A refused change (stale revision) does not move it.
    let (status, _) = fixture
        .put_policy(OWNER, "tenant:default", json!({}), Some(1))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(version(), 3);
    let (status, gone) = fixture
        .json(
            Method::DELETE,
            "/v1/admin/policies/group:default:eng",
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{gone}");
    assert_eq!(version(), 4);

    // The counter is not listed as a policy and cannot be addressed as one.
    let listed = list().await;
    assert_eq!(listed["version"], 4);
    let keys: Vec<&str> = listed["policies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["tenant:default"]);
    let (status, error) = fixture
        .json(
            Method::GET,
            "/v1/admin/policies/version:default",
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    // Another tenant's counter is its own.
    assert_eq!(
        crate::console::policy::current_version(&fixture.app, "foreign").unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claimed_start_is_checked_again_when_the_policy_moved_since_admission() {
    struct Gated {
        native: ReservedFakeAgentPorts,
        open: Arc<std::sync::atomic::AtomicBool>,
    }
    impl crate::ops::Executor for Gated {
        fn execute(
            &self,
            operation: &crate::store::StoredOperation,
            work: &Value,
        ) -> crate::ops::Finished {
            if matches!(
                serde_json::from_value::<crate::work::Work>(work.clone()),
                Ok(crate::work::Work::Console {
                    command: crate::console::ConsoleWork::Start { .. },
                    ..
                })
            ) {
                branchyard_testkit::wait::until("the test to move the policy", || {
                    self.open.load(std::sync::atomic::Ordering::SeqCst)
                });
            }
            crate::ops::Executor::execute(&self.native, operation, work)
        }
    }
    let mock = super::connections::gateway();
    let fixture = {
        let url = format!("http://{}", mock.addr());
        Fixture::with(false, move |config, root| {
            config.connectors = Some(super::connections::connectors(root, &url, &["github"]));
        })
    };
    let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
    fixture
        .app
        .registry
        .start(Arc::new(Gated {
            native: ReservedFakeAgentPorts(fixture.app.clone()),
            open: open.clone(),
        }))
        .unwrap();
    let wid = fixture.workspace().await;
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "tenant:default",
            json!({"skills":["docx","pptx"]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let route = format!("/v1/workspaces/{wid}/sessions");
    let start = |key: &'static str, tools: Value| {
        let (fixture, route) = (&fixture, route.clone());
        async move {
            let (status, started) = fixture
                .json(
                    Method::POST,
                    &route,
                    OWNER,
                    Some(key),
                    json!({"name":key,"prompt":"Hello","harness":"gemini-cli",
                           "access":"writer","tools":tools}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{started}");
            started
        }
    };
    let dropped = start("dropped", json!([{"kind":"skill","id":"pptx"}])).await;
    let kept = start("kept", json!([{"kind":"skill","id":"docx"}])).await;
    let row = |started: &Value| {
        fixture
            .app
            .registry
            .console_get(
                Collection::Sessions,
                started["session_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
    };
    assert_eq!(row(&dropped).body["policy_version"], 1);
    assert_eq!(row(&kept).body["policy_version"], 1);

    // While both wait, the administrator takes pptx away.
    let (status, put) = fixture
        .put_policy(OWNER, "tenant:default", json!({"skills":["docx"]}), Some(1))
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    open.store(true, std::sync::atomic::Ordering::SeqCst);

    let refused = fixture.finished(dropped["id"].as_str().unwrap());
    assert_eq!(
        refused.state,
        branchyard_client::api::OperationState::Failed,
        "{refused:?}"
    );
    let error = refused.error.unwrap();
    assert_eq!(error.code, "policy_changed", "{error:?}");
    assert!(error.message.contains("pptx"), "{error:?}");
    assert_eq!(
        row(&dropped).body["policy_version"],
        1,
        "a refused session keeps the version it was admitted under"
    );

    // The other session's tools are still allowed: it ran, took the new
    // version, and its manifest records it.
    let ran = fixture.finished(kept["id"].as_str().unwrap());
    assert_eq!(
        ran.state,
        branchyard_client::api::OperationState::Succeeded,
        "{ran:?}"
    );
    assert_eq!(row(&kept).body["policy_version"], 2);
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &wid)
        .unwrap()
        .unwrap();
    let repo = crate::console::runtime::resolve(&fixture.app, &workspace).unwrap();
    let sid = kept["session_id"].as_str().unwrap();
    let manifest = repo.yard.branch(sid).unwrap().manifest().unwrap().unwrap();
    assert_eq!(manifest.policy_version, 2);
    // The console shows the manifest's digest on the session.
    let (status, detail) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let shown = detail["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sid)
        .unwrap();
    assert_eq!(shown["manifest"], manifest.digest());
    assert_eq!(shown["policy_version"], 2);
}

#[tokio::test]
async fn a_recheck_names_what_the_policy_no_longer_allows() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let caller =
        crate::api::Caller::authenticated(fixture.app.config.principals["owner"].clone(), None);
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "tenant:default",
            json!({"connectors":["github:read"],"skills":["docx"],"helpers":["claude-code-acp"]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let check = |tools: Value| crate::console::policy::recheck(&fixture.app, &caller, &tools);
    let allowed = json!({
        "connectors":[{"connector":"github","mode":"read","account":null}],
        "skills":["docx"],
        "helpers":{"harnesses":["claude-code-acp"],"max_children":2},
    });
    check(allowed.clone()).unwrap();
    let refused = |tools: Value, kind: &str, id: &str| {
        let error = check(tools).unwrap_err();
        assert_eq!(error.body.code, "policy_changed", "{error:?}");
        let detail = error.body.detail.unwrap();
        assert_eq!(detail["kind"], kind, "{detail}");
        assert_eq!(detail["id"], id, "{detail}");
    };
    let with = |key: &str, value: Value| {
        let mut tools = allowed.clone();
        tools[key] = value;
        tools
    };
    refused(
        with(
            "connectors",
            json!([{"connector":"github","mode":"write","account":null}]),
        ),
        "connector",
        "github",
    );
    refused(
        with(
            "connectors",
            json!([{"connector":"linear","mode":"read","account":null}]),
        ),
        "connector",
        "linear",
    );
    refused(with("skills", json!(["pptx"])), "skill", "pptx");
    // No skill chosen means every skill, and the policy lists one.
    assert_eq!(
        check(with("skills", Value::Null)).unwrap_err().body.code,
        "policy_changed"
    );
    refused(
        with("helpers", json!({"harnesses":["gemini-cli"]})),
        "helper",
        "gemini-cli",
    );
    // Delegation switched off refuses every helper.
    let (status, put) = fixture
        .put_policy(
            OWNER,
            "principal:default:owner",
            json!({"delegation":false}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    refused(allowed, "helper", "*");
}

// ---------------------------------------------------------------------------
// Workers hold a session to the policy it was admitted under.
// ---------------------------------------------------------------------------

/// Holds every queued start until `open`, so a test can change what a
/// worker finds when it claims one.
struct HeldStarts {
    native: ReservedFakeAgentPorts,
    open: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::ops::Executor for HeldStarts {
    fn execute(
        &self,
        operation: &crate::store::StoredOperation,
        work: &Value,
    ) -> crate::ops::Finished {
        if matches!(
            serde_json::from_value::<crate::work::Work>(work.clone()),
            Ok(crate::work::Work::Console {
                command: crate::console::ConsoleWork::Start { .. },
                ..
            })
        ) {
            branchyard_testkit::wait::until("the test to open the queue", || {
                self.open.load(std::sync::atomic::Ordering::SeqCst)
            });
        }
        crate::ops::Executor::execute(&self.native, operation, work)
    }
}

fn session_row(fixture: &Fixture, started: &Value) -> crate::console_store::Record {
    fixture
        .app
        .registry
        .console_get(
            Collection::Sessions,
            started["session_id"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap()
}

/// Change a session's stored row, as another process's admission or a
/// stale one would have written it.
fn edit_session(fixture: &Fixture, started: &Value, edit: impl FnOnce(&mut Value)) {
    let row = session_row(fixture, started);
    let mut body = row.body.clone();
    edit(&mut body);
    fixture
        .app
        .registry
        .console_mutate(&[crate::console::update(row, body)])
        .unwrap();
}

async fn start_with_skill(fixture: &Fixture, wid: &str, key: &'static str) -> Value {
    let (status, started) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some(key),
            json!({"name":key,"prompt":"Hello","harness":"gemini-cli","access":"writer",
                   "tools":[{"kind":"skill","id":"docx"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    started
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_records_the_server_ceiling_it_applied() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let wid = fixture.workspace().await;
    let started = start_with_skill(&fixture, &wid, "recorded").await;
    let row = session_row(&fixture, &started);
    // The ceiling the fixture's server sets for its owner, and its delegation.
    assert_eq!(row.body["server_ceiling"]["allow_delegation"], true);
    let recorded: Vec<String> = row.body["server_ceiling"]["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}:{}",
                c["connector"].as_str().unwrap(),
                c["mode"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(recorded, ["atlassian:write", "github:read", "linear:write"]);
    // It is the process's own, as `Ceiling::of_this_server` reads it.
    let caller =
        crate::api::Caller::authenticated(fixture.app.config.principals["owner"].clone(), None);
    assert_eq!(
        crate::console::policy::Ceiling::admitted(&row.body),
        Some(crate::console::policy::Ceiling::of_this_server(
            &fixture.app,
            &caller
        ))
    );
}

#[test]
fn a_claiming_process_holds_the_session_to_the_ceiling_it_was_admitted_under() {
    use crate::console::policy::{claim_ceiling, Ceiling};
    let read = |text: &str| GrantEntry::parse(text).unwrap();
    let ceiling = |connectors: Option<Vec<&str>>, delegation: bool| Ceiling {
        connectors: connectors.map(|list| list.into_iter().map(read).collect()),
        allow_delegation: delegation,
    };
    let tools = |connector: &str, mode: &str| {
        json!({"connectors":[{"connector":connector,"mode":mode,"account":null}],
               "skills":["docx"]})
    };
    let helpers = json!({"skills":["docx"],"helpers":{"harnesses":["gemini-cli"]}});

    // A worker with a looser configuration changes nothing: the session
    // holds to what it was admitted under.
    let admitted = ceiling(Some(vec!["github:read"]), false);
    let looser = ceiling(None, true);
    let effective = claim_ceiling(&looser, Some(&admitted), &tools("github", "read")).unwrap();
    assert_eq!(effective, admitted);
    // A row that breaks its own ceiling is refused, whatever the worker
    // allows, and is not offered re-admission.
    for broken in [
        tools("github", "write"),
        tools("linear", "read"),
        helpers.clone(),
    ] {
        let error = claim_ceiling(&looser, Some(&admitted), &broken).unwrap_err();
        assert_eq!(error.body.code, "policy_changed", "{error:?}");
        assert!(
            error
                .body
                .detail
                .as_ref()
                .is_none_or(|d| d.get("readmit").is_none()),
            "{error:?}"
        );
    }

    // A worker whose configuration is narrower refuses what it cannot
    // hold, and asks for re-admission.
    let wide = ceiling(None, true);
    let narrow = ceiling(Some(vec!["github:read"]), false);
    let fits = tools("github", "read");
    assert_eq!(
        claim_ceiling(&narrow, Some(&wide), &fits).unwrap(),
        ceiling(Some(vec!["github:read"]), false),
        "a session that still fits runs, under the narrower ceiling"
    );
    for (excess, id) in [
        (tools("linear", "read"), "linear"),
        (tools("github", "write"), "github"),
    ] {
        let error = claim_ceiling(&narrow, Some(&wide), &excess).unwrap_err();
        assert_eq!(error.body.code, "policy_changed", "{error:?}");
        let detail = error.body.detail.unwrap();
        assert_eq!(detail["readmit"], true, "{detail}");
        assert_eq!(detail["id"], id, "{detail}");
        assert!(
            error.body.message.contains("readmit"),
            "{}",
            error.body.message
        );
    }
    let error = claim_ceiling(&narrow, Some(&wide), &helpers).unwrap_err();
    assert_eq!(error.body.detail.unwrap()["readmit"], true);

    // A session admitted before ceilings were recorded is held to the
    // claiming process's own.
    assert_eq!(claim_ceiling(&narrow, None, &fits).unwrap(), narrow);
    assert!(claim_ceiling(&narrow, None, &tools("linear", "read")).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_claims_against_the_recorded_ceiling_not_its_own_configuration() {
    let mock = super::connections::gateway();
    let fixture = {
        let url = format!("http://{}", mock.addr());
        Fixture::with(false, move |config, root| {
            config.connectors = Some(super::connections::connectors(root, &url, &["github"]));
            // This server offers delegation and sets no connector ceiling:
            // looser than any session below was admitted under.
            config.allow_delegation = true;
        })
    };
    let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
    fixture
        .app
        .registry
        .start(Arc::new(HeldStarts {
            native: ReservedFakeAgentPorts(fixture.app.clone()),
            open: open.clone(),
        }))
        .unwrap();
    let wid = fixture.workspace().await;
    let widened = start_with_skill(&fixture, &wid, "widened").await;
    let admitted = start_with_skill(&fixture, &wid, "admitted").await;
    let legacy = start_with_skill(&fixture, &wid, "legacy").await;
    let version = fixture
        .app
        .registry
        .console_get(
            Collection::Sessions,
            admitted["session_id"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap()
        .body["policy_version"]
        .clone();
    // `widened` was admitted by a server that offered no delegation, yet
    // its row asks for helpers: this worker's own delegation does not lend
    // them to it.
    edit_session(&fixture, &widened, |body| {
        body["server_ceiling"] = json!({"connectors": null, "allow_delegation": false});
        body["tools"]["helpers"] = json!({"harnesses":["gemini-cli"],"max_children":1});
    });
    // `admitted` carries the ceiling of a server like this one.
    edit_session(&fixture, &admitted, |body| {
        body["server_ceiling"] = json!({"connectors": null, "allow_delegation": true});
    });
    // `legacy` was admitted before ceilings were recorded.
    edit_session(&fixture, &legacy, |body| {
        body.as_object_mut().unwrap().remove("server_ceiling");
    });
    open.store(true, std::sync::atomic::Ordering::SeqCst);

    let refused = fixture.finished(widened["id"].as_str().unwrap());
    let error = refused.error.unwrap();
    assert_eq!(error.code, "policy_changed", "{error:?}");
    assert_eq!(error.detail.as_ref().unwrap()["kind"], "helper");
    for ran in [&admitted, &legacy] {
        let done = fixture.finished(ran["id"].as_str().unwrap());
        assert_eq!(
            done.state,
            branchyard_client::api::OperationState::Succeeded,
            "{done:?}"
        );
    }
    // Nothing moved: no policy changed, so no version was rewritten.
    assert_eq!(
        session_row(&fixture, &admitted).body["policy_version"],
        version
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_narrower_worker_refuses_and_asks_for_re_admission_which_checks_the_tools_again() {
    let mock = super::connections::gateway();
    let fixture = {
        let url = format!("http://{}", mock.addr());
        Fixture::with(false, move |config, root| {
            config.connectors = Some(super::connections::connectors(root, &url, &["github"]));
            // This worker's configuration lets its owner read github only.
            config.ceilings.insert(
                "owner".into(),
                branchyard::Ceiling {
                    connectors: Some(vec![GrantEntry::parse("github:read").unwrap()]),
                    ..Default::default()
                },
            );
        })
    };
    let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
    fixture
        .app
        .registry
        .start(Arc::new(HeldStarts {
            native: ReservedFakeAgentPorts(fixture.app.clone()),
            open: open.clone(),
        }))
        .unwrap();
    let wid = fixture.workspace().await;
    let started = start_with_skill(&fixture, &wid, "wide").await;
    // The server that admitted it set no connector ceiling, and its tools
    // include slack, which this worker's ceiling does not.
    edit_session(&fixture, &started, |body| {
        body["server_ceiling"] = json!({"connectors": null, "allow_delegation": false});
        body["tools"]["connectors"] =
            json!([{"connector":"slack","mode":"read","account":null,"operations":["*"]}]);
    });
    open.store(true, std::sync::atomic::Ordering::SeqCst);
    let refused = fixture.finished(started["id"].as_str().unwrap());
    let error = refused.error.unwrap();
    assert_eq!(error.code, "policy_changed", "{error:?}");
    assert_eq!(error.detail.as_ref().unwrap()["readmit"], true, "{error:?}");
    assert!(error.message.contains("readmit"), "{error:?}");

    // Re-admission runs the check again from the stored tools on the
    // process asked: slack is still not allowed here, and nothing changes.
    let route = format!(
        "/v1/workspaces/{wid}/sessions/{}/readmit",
        started["session_id"].as_str().unwrap()
    );
    let before = session_row(&fixture, &started);
    let (status, refused) = fixture
        .json(Method::POST, &route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "policy_changed");
    assert_eq!(session_row(&fixture, &started).revision, before.revision);

    // With the tools back within this process's ceiling, it records this
    // process's ceiling and the version now.
    edit_session(&fixture, &started, |body| {
        body["tools"]["connectors"] =
            json!([{"connector":"github","mode":"read","account":null,"operations":["*"]}]);
        body["policy_version"] = json!(0);
    });
    let (status, done) = fixture
        .json(Method::POST, &route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let row = session_row(&fixture, &started);
    assert_eq!(
        row.body["server_ceiling"],
        json!(crate::console::policy::Ceiling {
            connectors: Some(vec![GrantEntry::parse("github:read").unwrap()]),
            allow_delegation: false,
        }),
        "{}",
        row.body["server_ceiling"]
    );
    assert_eq!(done["policy_version"], row.body["policy_version"]);
    assert!(row.body["readmitted_at_ms"].as_u64().is_some());

    // Only the person the session belongs to can re-admit it.
    let (status, _) = fixture
        .json(Method::POST, &route, FOREIGN, None, Value::Null)
        .await;
    assert!(status.is_client_error(), "{status}");
}
