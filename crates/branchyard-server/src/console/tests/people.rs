//! The helper limit through `tools`, a person's own policy, and installing a
//! helper's harness. Hermetic: a mock gateway, the fake agent, and a fake
//! `npm` on an install `PATH` of the test's own.

use super::*;

use branchyard::connectors::GrantEntry;
use branchyard_testkit::MockHttp;

fn fixture(mock: &MockHttp) -> Fixture {
    let url = format!("http://{}", mock.addr());
    Fixture::with(true, move |config, root| {
        config.connectors = Some(super::connections::connectors(
            root,
            &url,
            &["atlassian", "github", "linear"],
        ));
        config.allow_delegation = true;
        config.ceilings.insert(
            "owner".into(),
            branchyard::Ceiling {
                connectors: Some(vec![
                    GrantEntry::parse("atlassian:write").unwrap(),
                    GrantEntry::parse("github:read").unwrap(),
                ]),
                ..Default::default()
            },
        );
        config
            .principals
            .get_mut("editor")
            .unwrap()
            .scopes
            .remove("admin");
    })
}

impl Fixture {
    async fn put(
        &self,
        path: &str,
        token: &str,
        policy: Value,
        revision: Option<u64>,
    ) -> (StatusCode, Value) {
        let mut body = json!({ "policy": policy });
        if let Some(revision) = revision {
            body["revision"] = json!(revision);
        }
        self.json(Method::PUT, path, token, None, body).await
    }

    async fn tools(&self, token: &str) -> Value {
        let (status, list) = self
            .json(Method::GET, "/v1/tools", token, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{list}");
        list
    }
}

fn row<'a>(list: &'a Value, kind: &str, id: &str) -> &'a Value {
    list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == kind && t["id"] == id)
        .unwrap_or_else(|| panic!("no {kind} {id}"))
}

fn version(fixture: &Fixture) -> u64 {
    crate::console::policy::current_version(&fixture.app, "default").unwrap()
}

#[tokio::test]
async fn the_helper_limit_rides_in_tools_and_policy_caps_it() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let wid = fixture.workspace().await;
    let route = format!("/v1/workspaces/{wid}/sessions");
    let start = |key: &'static str, tools: Value| {
        let fixture = &fixture;
        let route = route.clone();
        async move {
            fixture
                .json(
                    Method::POST,
                    &route,
                    OWNER,
                    Some(key),
                    json!({"name":key,"prompt":"Hello","harness":"gemini-cli","access":"writer","tools":tools}),
                )
                .await
        }
    };
    let chosen = |started: &Value| {
        fixture
            .app
            .registry
            .console_get(
                Collection::Sessions,
                started["session_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
            .body["tools"]["helpers"]
            .clone()
    };
    // The catalogue says what the person may ask for.
    let list = fixture.tools(OWNER).await;
    assert_eq!(
        (
            list["max_children"].as_u64(),
            list["default_children"].as_u64()
        ),
        (Some(16), Some(4))
    );

    let (status, started) = start(
        "two",
        json!([
            {"kind":"helper","id":"*","max_children":2},
            {"kind":"helper","id":"claude-code-acp"}
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(
        chosen(&started),
        json!({"harnesses":["claude-code-acp"],"max_children":2})
    );
    // The entry alone gives helpers on the session's own harness.
    let (status, alone) = start(
        "alone",
        json!([{"kind":"helper","id":"*","max_children":3}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alone}");
    assert_eq!(chosen(&alone), json!({"harnesses":[],"max_children":3}));
    // No number: the default.
    let (status, plain) = start("plain", json!([{"kind":"helper","id":"claude-code-acp"}])).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(chosen(&plain), json!({"harnesses":["claude-code-acp"]}));

    // Bad shapes.
    for (key, entry) in [
        ("zero", json!({"kind":"helper","id":"*","max_children":0})),
        (
            "seventeen",
            json!({"kind":"helper","id":"*","max_children":17}),
        ),
        (
            "on-a-harness",
            json!({"kind":"helper","id":"claude-code-acp","max_children":2}),
        ),
        (
            "on-a-skill",
            json!({"kind":"skill","id":"docx","max_children":2}),
        ),
    ] {
        let (status, error) = start(key, json!([entry])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {error}");
        assert_eq!(error["error"]["code"], "invalid_request", "{key}: {error}");
    }

    // The administrator caps it: the tenant's limit narrows the person's.
    let (status, put) = fixture
        .put(
            "/v1/admin/policies/tenant:default",
            OWNER,
            json!({"max_helpers":3}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let list = fixture.tools(OWNER).await;
    assert_eq!(
        (
            list["max_children"].as_u64(),
            list["default_children"].as_u64()
        ),
        (Some(3), Some(3))
    );
    let (status, error) = start("over", json!([{"kind":"helper","id":"*","max_children":5}])).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "tool_not_allowed");
    assert_eq!(error["error"]["detail"]["max_children"], 3);
    assert_eq!(error["error"]["detail"]["id"], "*");
    let (status, ok) = start("at", json!([{"kind":"helper","id":"*","max_children":3}])).await;
    assert_eq!(status, StatusCode::OK, "{ok}");
    // Left unset it is the default, or the cap when that is lower.
    let (status, low) = start(
        "low-default",
        json!([{"kind":"helper","id":"claude-code-acp"}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{low}");
    assert_eq!(
        chosen(&low)["max_children"],
        Value::Null,
        "recorded as chosen"
    );
    // A group's looser limit is pooled and the tenant's still caps it.
    let (status, put) = fixture
        .put(
            "/v1/admin/policies/principal:default:owner",
            OWNER,
            json!({"max_helpers":9}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(fixture.tools(OWNER).await["max_children"], 3);
    // Out-of-range limits are not stored.
    for bad in [0, 17] {
        let (status, error) = fixture
            .put(
                "/v1/admin/policies/tenant:default",
                OWNER,
                json!({"max_helpers":bad}),
                Some(1),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {error}");
    }
    // An old session recorded above a lowered cap fails its recheck.
    let caller =
        crate::api::Caller::authenticated(fixture.app.config.principals["owner"].clone(), None);
    let tools = json!({"helpers":{"harnesses":[],"max_children":8},"skills":["docx"]});
    let error = crate::console::policy::recheck(&fixture.app, &caller, &tools).unwrap_err();
    assert_eq!(error.body.code, "policy_changed");
    assert_eq!(error.body.detail.unwrap()["max_children"], 3);
}

#[tokio::test]
async fn a_person_narrows_themselves_and_never_beyond_the_administrator() {
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let me = "/v1/me/policy";

    // Nothing stored: an empty policy at revision 0, keyed for the caller.
    let (status, mine) = fixture
        .json(Method::GET, me, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(mine["revision"], 0);
    assert_eq!(mine["policy"], json!({}));
    assert!(
        mine["key"].as_str().unwrap().starts_with("self:default:"),
        "{mine}"
    );

    let before = fixture.tools(OWNER).await;
    assert_eq!(row(&before, "skill", "docx")["allowed"], true);
    assert_eq!(row(&before, "connector", "atlassian")["allowed"], true);
    assert_eq!(row(&before, "helper", "claude-code-acp")["allowed"], true);

    // Switch a skill, a connector and a helper off for myself.
    let v0 = version(&fixture);
    let off = json!({"off":[
        {"kind":"skill","id":"docx"},
        {"kind":"connector","id":"atlassian"},
        {"kind":"helper","id":"claude-code-acp"}
    ]});
    let (status, saved) = fixture.put(me, OWNER, off.clone(), None).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["revision"], 1);
    assert_eq!(
        version(&fixture),
        v0 + 1,
        "the tenant's policy version rose"
    );
    let list = fixture.tools(OWNER).await;
    for (kind, id) in [
        ("skill", "docx"),
        ("connector", "atlassian"),
        ("helper", "claude-code-acp"),
    ] {
        let tool = row(&list, kind, id);
        assert_eq!(tool["allowed"], false, "{kind} {id}: {tool}");
        assert_eq!(tool["state"], "not_allowed", "{kind} {id}");
        assert_eq!(tool["off_by_me"], true, "{kind} {id}");
        assert_eq!(tool["action"], "turn_on", "{kind} {id}");
        assert!(
            tool["reason"].as_str().unwrap().contains("turned this off")
                || tool["reason"].as_str().unwrap().contains("turned off"),
            "{tool}"
        );
    }
    assert_eq!(
        row(&list, "skill", "pptx")["allowed"],
        true,
        "the rest stays"
    );
    // Another person is unaffected.
    assert_eq!(
        row(&fixture.tools(EDITOR).await, "skill", "docx")["allowed"],
        true
    );
    // The record is the caller's alone and the administrator's list omits it.
    let (_, listed) = fixture
        .json(Method::GET, "/v1/admin/policies", OWNER, None, Value::Null)
        .await;
    assert_eq!(listed["policies"], json!([]), "{listed}");
    let (status, error) = fixture
        .put(
            "/v1/admin/policies/self:default:owner",
            OWNER,
            json!({}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["error"]["code"], "policy_key_invalid");

    // A start meets it by name, and says it is the person's own doing.
    let wid = fixture.workspace().await;
    let (status, error) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some("off-skill"),
            json!({"name":"x","prompt":"Hi","harness":"gemini-cli","access":"writer","tools":[{"kind":"skill","id":"docx"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "tool_not_allowed");
    assert_eq!(error["error"]["detail"]["off"], true);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("turned off for you"),
        "{error}"
    );
    let (status, error) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions"),
            OWNER,
            Some("off-connector"),
            json!({"name":"y","prompt":"Hi","harness":"gemini-cli","access":"writer","tools":[{"kind":"connector","id":"atlassian"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["detail"]["off"], true, "{error}");

    // Revision-guarded like the administrator's records.
    let (status, error) = fixture.put(me, OWNER, off.clone(), Some(7)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "revision_conflict");
    let (status, bad) = fixture
        .put(
            me,
            OWNER,
            json!({"off":[{"kind":"skill","id":"no-such-skill"}]}),
            Some(1),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");

    // Never wider: listing what the administrator withheld grants nothing.
    let (status, put) = fixture
        .put(
            "/v1/admin/policies/tenant:default",
            OWNER,
            json!({"skills":["pptx"],"connectors":["github:read"]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let (status, wide) = fixture
        .put(
            me,
            OWNER,
            json!({"skills":["docx","pptx","xlsx"],"connectors":["github:write","linear:write"],"delegation":true}),
            Some(1),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{wide}");
    let list = fixture.tools(OWNER).await;
    assert_eq!(row(&list, "skill", "pptx")["allowed"], true);
    for id in ["docx", "xlsx"] {
        let tool = row(&list, "skill", id);
        assert_eq!(tool["allowed"], false, "{id}");
        assert_eq!(
            tool["action"], "ask_admin",
            "the administrator's, not the person's: {tool}"
        );
        assert_eq!(tool["off_by_me"], Value::Null, "{tool}");
    }
    let github = row(&list, "connector", "github");
    assert_eq!(
        github["max_mode"], "read",
        "still the administrator's ceiling"
    );
    assert_eq!(row(&list, "connector", "linear")["allowed"], false);

    // Self-service needs the `run` scope, and the administrator's record
    // is still the one that is edited under /v1/admin.
    let (status, theirs) = fixture.put(me, FOREIGN, json!({}), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "another tenant keeps its own: {theirs}"
    );
    assert_ne!(theirs["key"], saved["key"]);
}

#[tokio::test]
async fn the_order_is_tenant_groups_person_then_self_and_a_change_raises_the_version() {
    use crate::console::policy::{intersect_policy, narrowed_by_self, resolve};
    use branchyard_client::workspace_api::{ToolKind, ToolPolicy, ToolRef};
    let skills = |items: &[&str]| ToolPolicy {
        skills: Some(items.iter().map(|s| (*s).to_owned()).collect()),
        ..Default::default()
    };
    let tenant = skills(&["docx", "pptx", "xlsx"]);
    let group = skills(&["pptx", "xlsx"]);
    let person = skills(&["xlsx", "pdf"]);
    let admin = resolve(Some(&tenant), &[&group], Some(&person));
    assert_eq!(
        admin.skills,
        Some(vec!["pptx".to_owned(), "xlsx".to_owned()])
    );
    // Self narrows after: wider lists change nothing, `off` removes.
    let wide = skills(&["docx", "pptx", "xlsx", "pdf"]);
    assert_eq!(narrowed_by_self(&admin, Some(&wide)).skills, admin.skills);
    let off = ToolPolicy {
        off: Some(vec![ToolRef {
            kind: ToolKind::Skill,
            id: "xlsx".into(),
        }]),
        ..Default::default()
    };
    let narrowed = narrowed_by_self(&admin, Some(&off));
    assert!(!crate::console::policy::skill_allowed(&narrowed, "xlsx"));
    assert!(
        crate::console::policy::skill_allowed(&admin, "xlsx"),
        "the ceiling is untouched"
    );
    assert_eq!(narrowed_by_self(&admin, None), admin);
    // Limits take the lesser at every step, and the pool takes the widest.
    let capped = |n| ToolPolicy {
        max_helpers: Some(n),
        ..Default::default()
    };
    assert_eq!(
        intersect_policy(&capped(5), &capped(2)).max_helpers,
        Some(2)
    );
    assert_eq!(
        resolve(Some(&capped(4)), &[&capped(9), &capped(2)], None).max_helpers,
        Some(4)
    );
    assert_eq!(
        resolve(None, &[&capped(9), &capped(2)], None).max_helpers,
        Some(9)
    );
    assert_eq!(
        narrowed_by_self(&capped(4), Some(&capped(2))).max_helpers,
        Some(2)
    );
    assert_eq!(
        narrowed_by_self(&capped(4), Some(&capped(9))).max_helpers,
        Some(4)
    );
    // A tool is off in the pool only when every record that speaks to `off` says so.
    let a = ToolPolicy {
        off: Some(vec![ToolRef {
            kind: ToolKind::Skill,
            id: "a".into(),
        }]),
        ..Default::default()
    };
    let b = ToolPolicy {
        off: Some(vec![ToolRef {
            kind: ToolKind::Skill,
            id: "b".into(),
        }]),
        ..Default::default()
    };
    assert_eq!(resolve(None, &[&a, &b], None).off, Some(vec![]));
    assert_eq!(resolve(None, &[&a, &skills(&["x"])], None).off, a.off);
    // Raising: a person's change, like the administrator's, moves the version
    // once, and a refused one does not.
    let mock = super::connections::gateway();
    let fixture = fixture(&mock);
    let start = version(&fixture);
    let (status, _) = fixture
        .put(
            "/v1/me/policy",
            OWNER,
            json!({"off":[{"kind":"skill","id":"docx"}]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(version(&fixture), start + 1);
    let (status, _) = fixture
        .put("/v1/me/policy", OWNER, json!({}), Some(9))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        version(&fixture),
        start + 1,
        "a refused change does not move it"
    );
    let (status, _) = fixture
        .put("/v1/me/policy", OWNER, json!({}), Some(1))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(version(&fixture), start + 2);
    assert_eq!(
        row(&fixture.tools(OWNER).await, "skill", "docx")["allowed"],
        true,
        "turned back on"
    );
}

// --- Installing a helper's harness ---------------------------------------

/// A server whose only helper is `gemini-cli`, a harness it does not have,
/// with a fake `npm` that "installs" it by copying the fake agent to the
/// path the server's configuration says the harness lives at. `argv` records
/// what the fake `npm` was given.
fn installer(allow: bool, exit: i32) -> (Fixture, std::path::PathBuf) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(std::path::PathBuf::new()));
    let seen = log.clone();
    let fixture = Fixture::with(true, move |config, root| {
        let bin = root.join("fakebin");
        let installed = root.join("installed").join("gemini");
        std::fs::create_dir_all(&bin).unwrap();
        let argv = root.join("npm-argv.txt");
        let script = format!(
            "#!/bin/sh\necho \"$@\" > {argv}\necho installing\n[ {exit} -eq 0 ] || exit {exit}\nmkdir -p {dir}\ncp {agent} {installed}\n",
            argv = argv.display(),
            dir = installed.parent().unwrap().display(),
            agent = branchyard_testkit::fake_agent_here().display(),
            installed = installed.display(),
        );
        let npm = bin.join("npm");
        std::fs::write(&npm, script).unwrap();
        std::fs::set_permissions(&npm, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        config
            .harness_commands
            .insert("gemini-cli".into(), vec![installed.display().to_string()]);
        config.allow_delegation = true;
        config
            .principals
            .get_mut("editor")
            .unwrap()
            .scopes
            .remove("admin");
        config.harness_install.allow = allow;
        config.harness_install.path = Some(format!("{}:/usr/bin:/bin", bin.display()));
        config.execution_presets = serde_json::from_value(json!([
            {"id":"local","label":"This server","provider":{"kind":"local"},
             "harnesses":[{"harness":"gemini-cli"}]}
        ]))
        .unwrap();
        config.validate().unwrap();
        *seen.lock().unwrap() = argv;
    });
    let argv = log.lock().unwrap().clone();
    (fixture, argv)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_administrator_installs_a_helpers_harness_and_its_row_becomes_ready() {
    let (fixture, argv) = installer(true, 0);
    let list = fixture.tools(OWNER).await;
    let helper = row(&list, "helper", "gemini-cli");
    assert_eq!(helper["state"], "not_ready", "{helper}");
    assert_eq!(helper["action"], "install");
    assert_eq!(helper["installable"], true);
    // A person without the admin scope is not offered it, and is refused.
    let editor = fixture.tools(EDITOR).await;
    let seen = row(&editor, "helper", "gemini-cli");
    assert!(
        seen["action"].is_null() && seen["installable"].is_null(),
        "{seen}"
    );
    assert!(
        seen["reason"].as_str().unwrap().contains("administrator"),
        "{seen}"
    );
    let route = "/v1/tools/helpers/gemini-cli/install";
    let (status, error) = fixture
        .json(Method::POST, route, EDITOR, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "scope_required");
    let (status, none) = fixture
        .json(Method::GET, route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{none}");

    let (status, accepted) = fixture
        .json(Method::POST, route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["harness"], "gemini-cli");
    assert_eq!(accepted["host"], "server");
    assert!(
        accepted["command"]
            .as_str()
            .unwrap()
            .starts_with("npm install -g @google/gemini-cli"),
        "{accepted}"
    );
    let done = branchyard_testkit::wait::until("the install to end", || {
        let state = block_on(fixture.json(Method::GET, route, OWNER, None, Value::Null));
        (state.1["state"] == "succeeded" || state.1["state"] == "failed").then_some(state.1)
    });
    assert_eq!(done["state"], "succeeded", "{done}");
    assert!(done["output_tail"].as_str().unwrap().contains("installing"));
    assert!(std::fs::read_to_string(argv)
        .unwrap()
        .starts_with("install -g @google/gemini-cli"));
    // The row moved from not_ready to ready, with nothing to do.
    let list = fixture.tools(OWNER).await;
    let helper = row(&list, "helper", "gemini-cli");
    assert_eq!(helper["state"], "ready", "{helper}");
    assert!(
        helper["action"].is_null() && helper["installable"].is_null(),
        "{helper}"
    );
    // The attempt is in the harness log.
    let events = branchyard::inventory::HarnessLog::new(
        fixture.app.config.data_dir.join("harness-events.jsonl"),
    )
    .read()
    .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.harness == "gemini-cli" && e.by == "console" && e.outcome == "verified"),
        "{events:?}"
    );
    // Installed already: nothing more to do.
    let (status, error) = fixture
        .json(Method::POST, route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "already_installed");
}

/// Run one future to its end from a synchronous poll, on this runtime.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_install_says_why_and_the_row_stays_not_ready() {
    let (fixture, _) = installer(true, 3);
    let route = "/v1/tools/helpers/gemini-cli/install";
    let (status, accepted) = fixture
        .json(Method::POST, route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let done = branchyard_testkit::wait::until("the install to end", || {
        let state = block_on(fixture.json(Method::GET, route, OWNER, None, Value::Null));
        (state.1["state"] == "succeeded" || state.1["state"] == "failed").then_some(state.1)
    });
    assert_eq!(done["state"], "failed", "{done}");
    assert!(
        done["problem"].as_str().unwrap().contains("status 3"),
        "{done}"
    );
    assert!(done["finished_at_ms"].is_u64());
    let list = fixture.tools(OWNER).await;
    assert_eq!(row(&list, "helper", "gemini-cli")["state"], "not_ready");
    // It may be tried again.
    let (status, again) = fixture
        .json(Method::POST, route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installing_is_closed_until_the_operator_allows_it_and_refuses_what_it_cannot_do() {
    let (fixture, argv) = installer(false, 0);
    let route = "/v1/tools/helpers/gemini-cli/install";
    let list = fixture.tools(OWNER).await;
    let helper = row(&list, "helper", "gemini-cli");
    assert_eq!(helper["state"], "not_ready");
    assert!(
        helper["action"].is_null() && helper["installable"].is_null(),
        "{helper}"
    );
    assert!(
        helper["reason"]
            .as_str()
            .unwrap()
            .contains("turned off on this server"),
        "{helper}"
    );
    let (status, error) = fixture
        .json(Method::POST, route, OWNER, None, Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "harness_install_disabled");
    assert!(!argv.exists(), "nothing ran");

    let (fixture, _) = installer(true, 0);
    for (id, code, expected) in [
        (
            "no-such-harness",
            "harness_unavailable",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // A harness the server knows but no preset offers its tenant.
        ("codex", "not_found", StatusCode::NOT_FOUND),
    ] {
        let (status, error) = fixture
            .json(
                Method::POST,
                &format!("/v1/tools/helpers/{id}/install"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, expected, "{id}: {error}");
        assert_eq!(error["error"]["code"], code, "{id}: {error}");
    }
}
