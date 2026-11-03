// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/codex/provision.py,
// the model aliases of harnesses/codex/config.yaml, and the tests of
// harnesses/codex/provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// `model_reasoning_effort` is placed at the top level before the first
// table (Scion appends it after the last table, where TOML reads it as a
// key of that table); reasoning effort is taken as a level name, with
// Scion's 0-100 mapping in `Effort::from_level`; `[otel]` is reconciled
// only when telemetry is asked for, with the collector the task names,
// and the OpenTelemetry environment is always "production" (Scion reads it
// from its own variables); `CODEX_HOME` is set only in a private home;
// MCP servers and instructions stay on the driver's session channel
// (thread configuration and `developerInstructions`, or ACP), so Scion's
// `[mcp_servers.*]` writer and AGENTS.md projection are not used; the
// model is set in `config.toml` only for the ACP profile, whose driver has
// no model parameter (not from Scion); the model gateway becomes a
// `model_providers` entry (not from Scion either). Scion's launch flags
// (`--dangerously-bypass-approvals-and-sandbox`, `--sandbox
// danger-full-access`) and its seed `approval_policy = "never"` are not
// ported: Branchyard's drivers route every approval.

//! Codex: `codex-app-server` and `codex-acp`.
//!
//! On the model gateway ([`Context::model_gateway`]), the gateway is a
//! provider of Codex's own in `config.toml`, `[model_providers.branchyard]`,
//! and `model_provider` names it: codex-cli 0.157.1 took neither
//! `OPENAI_BASE_URL` nor `OPENAI_API_KEY` from its environment (a turn on
//! the gateway went to `api.openai.com` with no key, 16 October 2026), and
//! a provider entry is how Codex is pointed at any OpenAI-compatible
//! endpoint. Its key is the variable the engine sets to the turn's token,
//! and its wire API is the Responses API, the only one codex-cli 0.157.1
//! accepts (`wire_api = "chat"` is refused: "no longer supported", its
//! discussion 7782), so the route for Codex's model must name a backend
//! that serves `/responses` (OpenAI's, Z.ai's `https://api.z.ai/api/v1`), not
//! a chat-completions-only endpoint such as Gemini's.
//!
//! A [provider login](crate::logins) is written the same way under the
//! provider's own ID (`[model_providers.zai]`, `base_url =
//! "https://api.z.ai/api/v1"`, `env_key = "ZAI_API_KEY"`), with the key in
//! that variable: codex-cli 0.164.0 ran GLM-4.6 so with no OpenAI login (25
//! October 2026).

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{Edit, TomlEdit};
use crate::permissions::Fallback;
use crate::{
    needs_private_home, pass_session, toml, unused, Context, EnvVar, Plan, Protocol, Provisioner,
    Refused, Telemetry, Via,
};

pub(crate) const AUTH_FILE: &str = ".codex/auth.json";
pub(crate) const CONFIG: &str = ".codex/config.toml";

/// The provider written for the model gateway, and the `model_provider`.
pub const GATEWAY_PROVIDER: &str = "branchyard";

/// The permission profile Branchyard writes into a private `CODEX_HOME`
/// and makes the default, so the thread's sandbox is the profile's.
pub const PERMISSION_PROFILE: &str = "branchyard";

/// A path the profile denies reading. It need not exist: a profile with a
/// deny entry is what stops Codex retrying an approved command outside its
/// sandbox without asking again (codex-cli 0.166.0, 3 November 2026).
const DENY_SENTINEL: &str = ".branchyard-no-read";

/// The `[permissions.branchyard.filesystem]` table: read everywhere, write
/// in the workspace roots, and the deny entry that disables the retry.
fn permission_profile(workspace: &str) -> String {
    let sentinel = format!("{}/{DENY_SENTINEL}", workspace.trim_end_matches('/'));
    format!(
        "[permissions.{PERMISSION_PROFILE}.filesystem]\n\":root\" = \"read\"\n\
         \":workspace_roots\" = \"write\"\n{} = \"deny\"\n",
        toml::string(&sentinel)
    )
}

/// The `[projects."<workspace>"]` table that marks the working tree
/// untrusted, so Codex loads no rules or hooks from the repository. Without
/// it, codex-cli 0.166.0 trusts the project the thread starts in.
fn untrusted_project(workspace: &str) -> (String, String) {
    let name = format!("projects.{}", toml::string(workspace));
    let table = format!("[{name}]\ntrust_level = \"untrusted\"\n");
    (name, table)
}

/// The `[model_providers.branchyard]` table for a gateway at `url`.
fn gateway_provider(url: &str) -> String {
    format!(
        "[model_providers.{GATEWAY_PROVIDER}]\nname = \"Branchyard model gateway\"\nbase_url = {}\n\
         env_key = \"OPENAI_API_KEY\"\nwire_api = \"responses\"\n",
        toml::string(&format!("{}/openai/v1", url.trim_end_matches('/')))
    )
}

/// Scion's `model_aliases` for Codex.
pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("small", "gpt-5.6-luna"),
    ("medium", "gpt-5.6-terra"),
    ("large", "gpt-5.6-sol"),
    ("extra-large", "gpt-6-astra"),
];

pub const AUTH: Spec = Spec {
    harness: "codex",
    methods: &[
        Method::env(
            "api-key",
            &["CODEX_API_KEY", "OPENAI_API_KEY"],
            "set CODEX_API_KEY or OPENAI_API_KEY",
        ),
        Method::file(
            "auth-file",
            "CODEX_AUTH",
            "provide CODEX_AUTH, the content of ~/.codex/auth.json",
        ),
    ],
};

/// The `[otel]` table: export to the collector, never the user's prompts.
pub fn otel_section(telemetry: &Telemetry) -> String {
    if !telemetry.enabled {
        return "[otel]\nexporter = \"none\"\nmetrics_exporter = \"none\"\ntrace_exporter = \"none\"\n"
            .into();
    }
    let endpoint = toml::escape(telemetry.endpoint());
    let exporter = "otlp-grpc";
    [
        "[otel]".to_owned(),
        "environment = \"production\"".to_owned(),
        "log_user_prompt = false".to_owned(),
        format!("metrics_exporter.\"{exporter}\".endpoint = \"{endpoint}\""),
        format!("exporter.\"{exporter}\".endpoint = \"{endpoint}\""),
        format!("trace_exporter.\"{exporter}\".endpoint = \"{endpoint}\""),
    ]
    .join("\n")
        + "\n"
}

/// The edits that keep Codex asking Branchyard: the permission profile as
/// the default, the project untrusted, and web search off unless the policy
/// allows everything, since Codex runs a search without asking anyone.
fn composition_edits(context: &Context) -> Vec<TomlEdit> {
    let (project, project_table) = untrusted_project(&context.workspace);
    let mut edits = vec![
        TomlEdit::SetKey {
            key: "default_permissions".into(),
            literal: toml::string(PERMISSION_PROFILE),
        },
        TomlEdit::RemoveTables(format!("permissions.{PERMISSION_PROFILE}")),
        TomlEdit::AppendTable(permission_profile(&context.workspace)),
        TomlEdit::RemoveTables(project),
        TomlEdit::AppendTable(project_table),
    ];
    let allows_everything = context
        .permissions
        .as_ref()
        .is_some_and(|p| p.fallback == Fallback::Allow && p.rules.iter().all(|r| r.allow));
    edits.push(if allows_everything {
        TomlEdit::RemoveKey("web_search".into())
    } else {
        TomlEdit::SetKey {
            key: "web_search".into(),
            literal: toml::string("disabled"),
        }
    });
    edits
}

pub(crate) struct Codex;

impl Provisioner for Codex {
    fn harness(&self) -> &'static str {
        "codex"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("codex")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            needs_private_home(context, "Codex's auth.json")?;
            plan.auth = Some(resolved.method.to_owned());
            let content = match resolved.method {
                "api-key" => {
                    let key = resolved.env()?;
                    let value = context.secret(key).unwrap_or_default();
                    // Scion's json.dump(indent=2): its key order, not sorted.
                    let payload = json!({"auth_mode": "apikey", "OPENAI_API_KEY": value});
                    format!(
                        "{{\n  \"auth_mode\": {},\n  \"OPENAI_API_KEY\": {}\n}}\n",
                        payload["auth_mode"], payload["OPENAI_API_KEY"]
                    )
                }
                "auth-file" => {
                    let content = context.secret("CODEX_AUTH").unwrap_or_default();
                    if content.trim().is_empty() {
                        return Err(Refused("the CODEX_AUTH secret is empty".into()));
                    }
                    if serde_json::from_str::<serde_json::Value>(content).is_err() {
                        return Err(Refused("the CODEX_AUTH secret is not valid JSON".into()));
                    }
                    content.to_owned()
                }
                _ => unreachable!("every method of AUTH is handled"),
            };
            plan.edit(AUTH_FILE, true, Edit::Put(content));
            let secret = resolved.secret()?;
            plan.deliver(
                secret,
                Via::File {
                    path: AUTH_FILE.into(),
                },
                false,
            );
        }
        plan.unused_secrets = unused(context, &AUTH.names());

        let mut toml_edits = Vec::new();
        if let Some(effort) = context.effort {
            toml_edits.push(TomlEdit::RemoveKey("reasoning_effort".into()));
            toml_edits.push(TomlEdit::SetKey {
                key: "model_reasoning_effort".into(),
                literal: toml::string(effort.as_str()),
            });
        }
        if let Some(model) = &context.model {
            let model = crate::claude::resolve_model_alias(model, MODEL_ALIASES);
            match context.protocol {
                Protocol::CodexAppServer => plan.session.model = Some(model),
                _ => toml_edits.push(TomlEdit::SetKey {
                    key: "model".into(),
                    literal: toml::string(&model),
                }),
            }
        }
        if let Some(telemetry) = &context.telemetry {
            toml_edits.push(TomlEdit::RemoveTables("otel".into()));
            toml_edits.push(TomlEdit::AppendTable(otel_section(telemetry)));
        }
        // The gateway as a provider; a branch is on the gateway for its
        // whole life, so nothing is taken back.
        if let Some(gateway) = context.model_gateway.as_ref().filter(|g| g.openai) {
            toml_edits.push(TomlEdit::RemoveTables(format!(
                "model_providers.{GATEWAY_PROVIDER}"
            )));
            toml_edits.push(TomlEdit::SetKey {
                key: "model_provider".into(),
                literal: toml::string(GATEWAY_PROVIDER),
            });
            toml_edits.push(TomlEdit::AppendTable(gateway_provider(&gateway.url)));
        }
        // A provider login: the provider as one of Codex's own, as for the
        // gateway, with its key in the variable the entry names. Codex's
        // shell may pass that on to its commands: taken to reach them.
        if let Some(login) = crate::logins::active(context) {
            if let Some(edits) = crate::logins::codex_provider_edits(login) {
                if let Some(crate::logins::Config::CodexProvider { env_key, .. }) = login.config {
                    if let Some(value) = context.secret(env_key) {
                        plan.secret_env(env_key, env_key, value, true);
                    }
                }
                toml_edits.extend(edits);
            }
        }
        // Composition with Codex's own permissions, in a private home only:
        // the user's home is theirs to configure. See
        // docs/harness-integration.md#composing-with-a-harnesss-own-permissions.
        if context.private_home && context.protocol == Protocol::CodexAppServer {
            toml_edits.extend(composition_edits(context));
            plan.session.permission_profile = Some(PERMISSION_PROFILE.into());
        }
        if !toml_edits.is_empty() {
            needs_private_home(context, "Codex's config.toml")?;
            plan.edit(CONFIG, false, Edit::Toml(toml_edits));
        }
        if context.private_home && plan.touches_home_or_env() {
            plan.set_env(EnvVar::plain("CODEX_HOME", context.home_path(".codex")));
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::apply_all;
    use crate::Effort;

    fn reconcile(
        current: Option<&str>,
        effort: Option<Effort>,
        telemetry: Option<Telemetry>,
    ) -> String {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        context.effort = effort;
        context.telemetry = telemetry;
        let plan = Codex.plan(&context).unwrap();
        let file = plan.files.iter().find(|f| f.path == CONFIG).unwrap();
        apply_all(&file.edits, current).unwrap().unwrap()
    }

    fn on(endpoint: &str) -> Option<Telemetry> {
        Some(Telemetry {
            enabled: true,
            endpoint: Some(endpoint.into()),
        })
    }

    // From Scion's codex/provision_test.py.

    #[test]
    fn native_otel_routes_only_to_the_configured_receiver() {
        let section = otel_section(&on("http://127.0.0.1:14317").unwrap());
        for line in [
            "metrics_exporter.\"otlp-grpc\".endpoint = \"http://127.0.0.1:14317\"",
            "exporter.\"otlp-grpc\".endpoint = \"http://127.0.0.1:14317\"",
            "trace_exporter.\"otlp-grpc\".endpoint = \"http://127.0.0.1:14317\"",
            "log_user_prompt = false",
        ] {
            assert!(section.contains(line), "{line}");
        }
        assert!(!section.contains("statsig"));
    }

    #[test]
    fn disabled_otel_disables_all_exporters() {
        let out = reconcile(
            Some("[otel.exporter.\"otlp-grpc\"]\nendpoint = \"https://external.invalid:443\"\n"),
            None,
            Some(Telemetry {
                enabled: false,
                endpoint: None,
            }),
        );
        for line in [
            "exporter = \"none\"",
            "metrics_exporter = \"none\"",
            "trace_exporter = \"none\"",
        ] {
            assert!(out.contains(line), "{line}");
        }
        assert!(!out.contains("external.invalid"));
    }

    #[test]
    fn the_model_gateway_is_a_provider_in_config_toml() {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        context.model_gateway = Some(crate::models::ModelGateway {
            url: "http://127.0.0.1:41100/".into(),
            anthropic: true,
            openai: true,
        });
        let plan = Codex.plan(&context).unwrap();
        let file = plan.files.iter().find(|f| f.path == CONFIG).unwrap();
        let edits = match &file.edits[..] {
            [Edit::Toml(edits)] => edits.clone(),
            other => panic!("{other:?}"),
        };
        assert!(edits.contains(&TomlEdit::SetKey {
            key: "model_provider".into(),
            literal: "\"branchyard\"".into(),
        }));
        assert!(edits.contains(&TomlEdit::AppendTable(
            "[model_providers.branchyard]\nname = \"Branchyard model gateway\"\n\
             base_url = \"http://127.0.0.1:41100/openai/v1\"\nenv_key = \"OPENAI_API_KEY\"\n\
             wire_api = \"responses\"\n"
                .into()
        )));
        // The key reaches Codex through CODEX_HOME's config, not a file.
        assert!(plan.env.iter().any(|v| v.name == "CODEX_HOME"));
        // A gateway serving Anthropic alone is nothing Codex can use.
        context.model_gateway = Some(crate::models::ModelGateway {
            url: "http://127.0.0.1:41100".into(),
            anthropic: true,
            openai: false,
        });
        let plan = Codex.plan(&context).unwrap();
        let file = plan.files.iter().find(|f| f.path == CONFIG).unwrap();
        let out = apply_all(&file.edits, None).unwrap().unwrap();
        assert!(!out.contains("model_provider"), "{out}");
    }

    #[test]
    fn reasoning_effort_is_written_as_model_reasoning_effort() {
        let out = reconcile(None, Some(Effort::Medium), None);
        assert!(out.contains("model_reasoning_effort = \"medium\""));
    }

    #[test]
    fn no_effort_writes_no_reasoning_key() {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        let out = reconcile(None, None, None);
        assert!(!out.contains("reasoning_effort"), "{out}");
        // Outside a private home nothing is written at all.
        context.private_home = false;
        let plan = Codex.plan(&context).unwrap();
        assert!(plan.files.is_empty() && plan.env.is_empty());
    }

    #[test]
    fn replaces_existing_reasoning_effort_keys() {
        for current in [
            "reasoning_effort = \"low\"\nother_key = \"value\"\n",
            "model_reasoning_effort = \"medium\"\nother_key = \"value\"\n",
            "model_reasoning_effort = \"medium\"\nreasoning_effort = \"low\"\nother_key = \"value\"\n",
        ] {
            let out = reconcile(Some(current), Some(Effort::High), None);
            assert!(out.contains("model_reasoning_effort = \"high\""), "{out}");
            assert_eq!(out.matches("model_reasoning_effort").count(), 1, "{out}");
            assert!(!out.contains("\"low\"") && !out.contains("\"medium\""), "{out}");
            assert!(out.contains("other_key = \"value\""), "{out}");
        }
    }

    #[test]
    fn reasoning_effort_levels_map_as_scion_maps_them() {
        for (level, effort) in [
            (0, Effort::Low),
            (25, Effort::Low),
            (26, Effort::Medium),
            (50, Effort::Medium),
            (51, Effort::High),
            (75, Effort::High),
            (76, Effort::Xhigh),
            (100, Effort::Xhigh),
            (-10, Effort::Low),
            (150, Effort::Xhigh),
        ] {
            assert_eq!(Effort::from_level(level), effort, "{level}");
        }
    }

    fn composed(
        permissions: Option<crate::permissions::Permissions>,
        current: Option<&str>,
    ) -> (Plan, String) {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        context.permissions = permissions;
        let plan = Codex.plan(&context).unwrap();
        let file = plan.files.iter().find(|f| f.path == CONFIG).unwrap();
        let out = apply_all(&file.edits, current).unwrap().unwrap();
        (plan, out)
    }

    /// The configuration verified with codex-cli 0.166.0: Branchyard's
    /// profile replaces the user's default, its deny entry is present, and
    /// the working tree is untrusted even where the copied config trusted it.
    #[test]
    fn a_private_home_makes_branchyards_profile_the_default() {
        let user = "default_permissions = \":danger-no-sandbox\"\n\
                    [projects.\"/w\"]\ntrust_level = \"trusted\"\n";
        let (plan, out) = composed(None, Some(user));
        assert_eq!(
            plan.session.permission_profile.as_deref(),
            Some("branchyard")
        );
        for line in [
            "default_permissions = \"branchyard\"",
            "[permissions.branchyard.filesystem]",
            "\":root\" = \"read\"",
            "\":workspace_roots\" = \"write\"",
            "\"/w/.branchyard-no-read\" = \"deny\"",
            "[projects.\"/w\"]\ntrust_level = \"untrusted\"",
            "web_search = \"disabled\"",
        ] {
            assert!(out.contains(line), "{line} in\n{out}");
        }
        assert!(!out.contains("danger-no-sandbox"), "{out}");
        assert!(!out.contains("\"trusted\""), "{out}");
        // A second turn in the same home writes the same file.
        let (_, again) = composed(None, Some(&out));
        assert_eq!(again, out);
    }

    #[test]
    fn web_search_stays_when_the_policy_allows_everything() {
        let everything = crate::permissions::Permissions::new(Fallback::Allow);
        let (_, out) = composed(Some(everything), Some("web_search = \"live\"\n"));
        assert!(!out.contains("web_search"), "{out}");
        let (_, out) = composed(
            Some(crate::permissions::Permissions::new(Fallback::Deny)),
            None,
        );
        assert!(out.contains("web_search = \"disabled\""), "{out}");
    }

    /// The user's own home is theirs: nothing is written there and no
    /// profile is named, so a restricting policy is refused before the run.
    #[test]
    fn the_users_own_home_gets_no_profile() {
        let context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        let plan = Codex.plan(&context).unwrap();
        assert!(plan.files.iter().all(|f| f.path != CONFIG));
        assert_eq!(plan.session.permission_profile, None);
        let mut acp = Context::new("codex", Protocol::Acp, "/h", "/w");
        acp.private_home = true;
        assert_eq!(Codex.plan(&acp).unwrap().session.permission_profile, None);
    }
}
