// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/claude/provision.py,
// the model aliases of harnesses/claude/config.yaml, and the model tests of
// harnesses/claude/provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// An API key is not set in the environment with its fingerprint approved
// in `.claude.json`, as Scion does: it is written to a 0600 file in the
// private home that `apiKeyHelper` in `~/.claude/settings.json` prints, so
// the harness's tool commands do not inherit it. The workspace is marked
// trusted without dropping other projects; a credentials file is written
// from the CLAUDE_AUTH secret rather than mounted; no model is set unless
// one is asked for (Scion defaults to "opus"); telemetry goes to the
// endpoint the task names, without Scion's cloud-provider checks, which
// belong to its own collector; MCP servers and instructions stay on the
// driver's session channel (`--mcp-config` with a 0600 file, plugin or
// system prompt, or ACP), so Scion's `.claude.json` MCP merge and
// CLAUDE.md projection are not used; the version probe (`claude
// --version`) is dropped, since planning runs no processes. Scion's launch
// flags (`--dangerously-skip-permissions`) are not ported: Branchyard's
// drivers route every permission request. Vertex AI also takes Claude
// Code's own names for the project and region (`ANTHROPIC_VERTEX_PROJECT_ID`,
// `CLOUD_ML_REGION`), which come first; a project without a region selects
// nothing rather than refusing, unless `vertex-ai` is asked for; and every
// other method sets `CLAUDE_CODE_USE_VERTEX=0`.

//! Claude Code: `claude-code-stream-json` and `claude-code-acp`.
//!
//! An API key reaches Claude Code through `apiKeyHelper`, a command in
//! `~/.claude/settings.json` whose output is the key, here `cat` of a 0600
//! file in the private home. Claude Code 2.1.283, and the 2.1.280 that
//! claude-agent-acp 0.81.2 runs (it loads user settings), use a helper from
//! user settings without the workspace trust or key approval an
//! environment key needs, and the key never enters the environment its
//! Bash tool and MCP servers inherit. With a helper,
//! `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST` is not set: 2.1.283 ignores
//! `apiKeyHelper` when it is. An OAuth token has no such setting and goes
//! in the environment. Where the host has bwrap and socat, the plan sets
//! `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`, and Claude Code hides the token
//! from its Bash commands and MCP servers (and sandboxes Bash); elsewhere
//! the turn warns that they can read it (Claude Code 2.1.318, 4 November
//! 2026). The Vertex AI project and region are not credentials and stay
//! visible either way.
//!
//! An isolated harness keeps Claude Code's Vertex AI switch from the
//! engine's environment, so every method but `vertex-ai` sets
//! `CLAUDE_CODE_USE_VERTEX=0`: an inherited switch would otherwise win
//! over the credential given here. With no method chosen, nothing is set
//! and the inherited switch, if any, applies. A project given without a
//! region (a `GOOGLE_CLOUD_PROJECT` meant for Gemini CLI, say) chooses
//! nothing: it is reported unused, with a warning naming the region it
//! lacked.
//!
//! A gateway's token (`ANTHROPIC_AUTH_TOKEN`, the `auth-token` method, or a
//! [provider login](crate::logins) such as Z.ai's) takes the same helper:
//! Claude Code sends what `apiKeyHelper` prints as both `x-api-key` and
//! `Authorization: Bearer`, which is what the variable would send, and it
//! stays out of the tools' environment. `ANTHROPIC_BASE_URL`, given as a
//! secret beside any method, goes into the environment, where Claude Code
//! reads its endpoint: an isolated run's environment has no `ANTHROPIC*`
//! variable of yours.
//!
//! With the stream-json driver, MCP servers go in a 0600 file passed to
//! `--mcp-config` by path, never on the command line.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unused, Context, Credential, EnvVar, McpConfigFile, Plan,
    Protocol, Provisioner, Refused, Via,
};

const CLAUDE_JSON: &str = ".claude.json";
pub(crate) const CREDENTIALS: &str = ".claude/.credentials.json";
const SETTINGS: &str = ".claude/settings.json";
/// The API key or gateway token `apiKeyHelper` prints.
pub(crate) const API_KEY_FILE: &str = ".branchyard/credentials/anthropic-api-key";
/// The endpoint, given as a secret beside the credential.
pub const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// The stream-json driver's MCP configuration, in a private home.
pub(crate) const MCP_CONFIG: &str = ".branchyard/claude-mcp.json";

/// Scion's `model_aliases` for Claude.
pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("small", "haiku"),
    ("medium", "claude-sonnet-5"),
    ("large", "claude-opus-5-5"),
    ("extra-large", "claude-fable-5-1"),
];

/// Shorthand spellings, as Scion's `config.NormalizeModelAlias` accepts.
pub const MODEL_ALIAS_SHORTHAND: &[(&str, &str)] = &[
    ("s", "small"),
    ("m", "medium"),
    ("l", "large"),
    ("xl", "extra-large"),
];

/// The size aliases that resolve.
pub const KNOWN_MODEL_ALIASES: &[&str] = &["small", "medium", "large", "extra-large"];

pub const AUTH: Spec = Spec {
    harness: "claude-code",
    methods: &[
        Method::env("api-key", &["ANTHROPIC_API_KEY"], "set ANTHROPIC_API_KEY"),
        Method::env(
            "auth-token",
            &["ANTHROPIC_AUTH_TOKEN"],
            "set ANTHROPIC_AUTH_TOKEN, with ANTHROPIC_BASE_URL for a gateway",
        ),
        Method::env(
            "oauth-token",
            &["CLAUDE_CODE_OAUTH_TOKEN"],
            "set CLAUDE_CODE_OAUTH_TOKEN (generate with `claude setup-token`)",
        ),
        Method::file(
            "auth-file",
            "CLAUDE_AUTH",
            "provide CLAUDE_AUTH, the content of ~/.claude/.credentials.json",
        ),
        Method::env_all(
            "vertex-ai",
            &[VERTEX_PROJECT],
            VERTEX_REGION,
            "give a project (--secret ANTHROPIC_VERTEX_PROJECT_ID or GOOGLE_CLOUD_PROJECT) and a \
             region (--secret CLOUD_ML_REGION, GOOGLE_CLOUD_LOCATION or GOOGLE_CLOUD_REGION), \
             with Application Default Credentials the harness can reach (GCLOUD_ADC, or a Google \
             VM's metadata server), for Vertex AI",
        ),
    ],
};

/// The secrets `vertex-ai` takes the project from, Claude Code's own name
/// first.
const VERTEX_PROJECT: &[&str] = &["ANTHROPIC_VERTEX_PROJECT_ID", "GOOGLE_CLOUD_PROJECT"];

/// The secrets `vertex-ai` takes the region from, Claude Code's own name
/// first.
const VERTEX_REGION: &[&str] = &[
    "CLOUD_ML_REGION",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_CLOUD_REGION",
];

/// Lower-case, and expand `s`, `m`, `l` and `xl`.
pub fn normalize_model_alias(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    MODEL_ALIAS_SHORTHAND
        .iter()
        .find(|(short, _)| *short == lowered)
        .map_or(lowered, |(_, full)| (*full).to_owned())
}

/// A size alias through `aliases`; anything else, normalized, unchanged.
pub fn resolve_model_alias(raw: &str, aliases: &[(&str, &str)]) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let normalized = normalize_model_alias(raw);
    if !KNOWN_MODEL_ALIASES.contains(&normalized.as_str()) {
        return normalized;
    }
    aliases
        .iter()
        .find(|(alias, _)| *alias == normalized)
        .map_or(normalized.clone(), |(_, model)| (*model).to_owned())
}

/// Where Claude Code reads its managed settings on Linux and WSL, and where
/// a sandboxed turn's are written; see `docs/provisioning.md#managed-settings`.
/// Claude Code's switch that hides its own credentials from the commands
/// its tools run, and sandboxes Bash.
pub const ENV_SCRUB: &str = "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB";

/// The warning a turn records when the OAuth token stays visible to tool
/// commands.
pub const OAUTH_IN_TOOLS: &str = "the Claude Code OAuth token is in the harness's environment, so \
     its Bash commands and MCP servers can read it: Claude Code hides it only with \
     CLAUDE_CODE_SUBPROCESS_ENV_SCRUB, which needs bwrap and socat on this host; install both, \
     or give an API key, which reaches Claude Code through a file";

pub const MANAGED_SETTINGS: &str = "/etc/claude-code/managed-settings.json";

/// The variables [`plan`] sets to turn Claude Code's telemetry export off.
/// Pinned in managed settings too, so a setting in the home cannot turn it
/// back on.
#[cfg(test)]
const TELEMETRY_OFF: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "0"),
    ("OTEL_METRICS_EXPORTER", "none"),
    ("OTEL_LOGS_EXPORTER", "none"),
    ("OTEL_TRACES_EXPORTER", "none"),
];

pub(crate) struct Claude;

impl Provisioner for Claude {
    fn harness(&self) -> &'static str {
        "claude-code"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("claude")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        // Claude Code reads its reasoning effort from this variable (as
        // `--effort` and the `effortLevel` setting do); Branchyard's level
        // names are among its values.
        if let Some(effort) = context.effort {
            plan.set_env(EnvVar::plain("CLAUDE_CODE_EFFORT_LEVEL", effort.as_str()));
        }
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        let resolved = AUTH.select(&given, context.auth.as_deref())?;
        let secret = |name: &str| context.secret(name).unwrap_or_default().to_owned();
        if let Some(resolved) = resolved {
            plan.auth = Some(resolved.method.to_owned());
            match resolved.method {
                "api-key" | "auth-token" => {
                    let name = resolved.env()?;
                    needs_private_home(context, "the API key file")?;
                    plan.edit(API_KEY_FILE, true, Edit::Put(secret(name)));
                    plan.edit(
                        SETTINGS,
                        true,
                        Edit::Json {
                            edits: vec![JsonEdit::Set(
                                path(&["apiKeyHelper"]),
                                json!(helper(context)),
                            )],
                            comment_lines: false,
                        },
                    );
                    plan.deliver(
                        name,
                        Via::Helper {
                            path: API_KEY_FILE.into(),
                            setting: format!("apiKeyHelper in {SETTINGS}"),
                        },
                        false,
                    );
                }
                // Claude Code has no file or helper for a token from
                // `claude setup-token`: `.credentials.json` holds a whole
                // claude.ai login (CLAUDE_AUTH).
                //
                // In its environment the token reaches every command the
                // Bash tool and MCP servers run, unless Claude Code scrubs
                // their environment, which on Linux needs bwrap and socat
                // and also sandboxes Bash: writes outside the working tree
                // and its temporary directory are refused (Claude Code
                // 2.1.318, 4 November 2026).
                "oauth-token" => {
                    let scrubbed = context.subprocess_isolation;
                    plan.secret_env(
                        "CLAUDE_CODE_OAUTH_TOKEN",
                        "CLAUDE_CODE_OAUTH_TOKEN",
                        &secret("CLAUDE_CODE_OAUTH_TOKEN"),
                        !scrubbed,
                    );
                    if scrubbed {
                        plan.set_env(EnvVar::plain(ENV_SCRUB, "1"));
                    } else {
                        plan.warnings.push(OAUTH_IN_TOOLS.into());
                    }
                }
                "auth-file" => {
                    needs_private_home(context, "the credentials file")?;
                    let content = secret("CLAUDE_AUTH");
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        return Err(Refused("the CLAUDE_AUTH secret is not valid JSON".into()));
                    }
                    plan.edit(CREDENTIALS, true, Edit::Put(content));
                    plan.deliver(
                        "CLAUDE_AUTH",
                        Via::File {
                            path: CREDENTIALS.into(),
                        },
                        false,
                    );
                }
                "vertex-ai" => {
                    let project = VERTEX_PROJECT
                        .iter()
                        .copied()
                        .find(|name| context.secret(name).is_some());
                    let (Some(project), Some(region)) = (project, resolved.env_key) else {
                        return Err(Refused(
                            "vertex-ai was chosen without a project and a region".into(),
                        ));
                    };
                    plan.secret_env(
                        project,
                        "ANTHROPIC_VERTEX_PROJECT_ID",
                        &secret(project),
                        true,
                    );
                    plan.secret_env(region, "CLOUD_ML_REGION", &secret(region), true);
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
            let vertex = resolved.method == "vertex-ai";
            plan.set_env(EnvVar::plain(
                "CLAUDE_CODE_USE_VERTEX",
                if vertex { "1" } else { "0" },
            ));
            // Suppress model upgrade and fallback dialogs: the provider is
            // chosen here. Not with the helper: Claude Code 2.1.283 then
            // ignores `apiKeyHelper` and reports it is not logged in.
            if !helped(resolved.method) {
                plan.set_env(EnvVar::plain("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"));
            }
            if context.private_home {
                trust_workspace(context, &mut plan);
            }
        }
        // An API key an earlier turn wrote, and its helper, go when this
        // turn authenticates another way (or not at all).
        let had_key = context
            .installed
            .credentials
            .iter()
            .any(|c| c.path() == API_KEY_FILE);
        if had_key && !plan.auth.as_deref().is_some_and(helped) {
            plan.edit(API_KEY_FILE, true, Edit::Remove);
            plan.edit(
                SETTINGS,
                false,
                Edit::Json {
                    edits: vec![JsonEdit::RemoveIf(
                        path(&["apiKeyHelper"]),
                        json!(helper(context)),
                    )],
                    comment_lines: false,
                },
            );
        }
        // The endpoint, for whichever credential: a gateway's.
        if let Some(url) = context.secret(BASE_URL) {
            plan.secret_env(BASE_URL, BASE_URL, url, true);
        }
        mcp_config(context, &mut plan);
        let mut read = AUTH.names();
        read.push(BASE_URL);
        plan.unused_secrets = unused(context, &read);
        if plan.auth.is_none() {
            for gap in AUTH.incomplete(&given) {
                plan.unused_secrets
                    .extend(gap.given.iter().map(|name| (*name).to_owned()));
                plan.warnings.push(format!(
                    "{} did not use {}: {} also needs {}",
                    self.harness(),
                    gap.given.join(", "),
                    gap.method,
                    gap.missing
                ));
            }
        }
        if let Some(telemetry) = &context.telemetry {
            let on = telemetry.enabled;
            let exporter = if on { "otlp" } else { "none" };
            let endpoint = telemetry.endpoint();
            for (name, value) in [
                ("CLAUDE_CODE_ENABLE_TELEMETRY", if on { "1" } else { "0" }),
                ("OTEL_METRICS_EXPORTER", exporter),
                ("OTEL_LOGS_EXPORTER", exporter),
                ("OTEL_TRACES_EXPORTER", "none"),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", endpoint),
                ("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", endpoint),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ] {
                plan.set_env(EnvVar::plain(name, value));
            }
        }
        if let Some(model) = &context.model {
            let model = resolve_model_alias(model, MODEL_ALIASES);
            // The stream-json driver passes it as `--model` too, which
            // its own settings cannot override.
            if context.protocol == Protocol::ClaudeStreamJson {
                plan.session.model = Some(model.clone());
            }
            plan.set_env(EnvVar::plain("ANTHROPIC_MODEL", model));
        }
        Ok(plan)
    }
}

/// Whether `method` reaches Claude Code through `apiKeyHelper`.
fn helped(method: &str) -> bool {
    matches!(method, "api-key" | "auth-token")
}

/// The `apiKeyHelper` command: print the key file, as the harness sees it.
fn helper(context: &Context) -> String {
    let file = context.home_path(API_KEY_FILE);
    format!("cat '{}'", file.replace('\'', r"'\''"))
}

/// The stream-json driver's MCP servers as a 0600 file: in a private home,
/// written by the plan (and removed when a later turn has none); otherwise
/// placed by the caller for the turn. Other protocols pass them over stdin.
fn mcp_config(context: &Context, plan: &mut Plan) {
    if context.protocol != Protocol::ClaudeStreamJson {
        return;
    }
    let recorded = context
        .installed
        .credentials
        .iter()
        .any(|c| matches!(c, Credential::File { path } if path == MCP_CONFIG));
    if context.mcp_servers.is_empty() && context.remote_mcp_servers.is_empty() {
        if recorded {
            plan.edit(MCP_CONFIG, true, Edit::Remove);
        }
        return;
    }
    let content = branchyard_harness::claude_code::mcp_config(
        &context.mcp_servers,
        &context.remote_mcp_servers,
    );
    let path = match context.private_home {
        true => {
            plan.edit(MCP_CONFIG, true, Edit::Put(content.clone()));
            Some(context.home_path(MCP_CONFIG))
        }
        false => None,
    };
    plan.session.mcp_config = Some(McpConfigFile { path, content });
}

/// Mark the workspace trusted in `.claude.json`, keeping other projects.
fn trust_workspace(context: &Context, plan: &mut Plan) {
    let project = |key: &str| path(&["projects", &context.workspace, key]);
    plan.edit(
        CLAUDE_JSON,
        false,
        Edit::Json {
            edits: vec![
                JsonEdit::Set(project("hasTrustDialogAccepted"), json!(true)),
                JsonEdit::Default(project("projectOnboardingSeenCount"), json!(1)),
            ],
            comment_lines: false,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // From Scion's claude/provision_test.py, ModelResolutionTest, with its
    // alias table.
    const TEST_ALIASES: &[(&str, &str)] = &[
        ("small", "haiku"),
        ("medium", "sonnet"),
        ("large", "opus"),
        ("extra-large", "fable"),
    ];

    #[test]
    fn size_alias_resolves_through_config_model_aliases() {
        assert_eq!(resolve_model_alias("medium", TEST_ALIASES), "sonnet");
    }

    #[test]
    fn alias_matching_is_case_insensitive_and_accepts_shorthand() {
        for (raw, want) in [
            ("Medium", "sonnet"),
            ("L", "opus"),
            ("XL", "fable"),
            ("SMALL", "haiku"),
            ("extra-large", "fable"),
        ] {
            assert_eq!(resolve_model_alias(raw, TEST_ALIASES), want, "{raw}");
        }
    }

    #[test]
    fn shorthand_set_matches_go_normalize_model_alias() {
        assert_eq!(
            MODEL_ALIAS_SHORTHAND,
            &[
                ("s", "small"),
                ("m", "medium"),
                ("l", "large"),
                ("xl", "extra-large")
            ]
        );
        assert_eq!(
            KNOWN_MODEL_ALIASES,
            &["small", "medium", "large", "extra-large"]
        );
        for raw in ["xlarge", "extra_large"] {
            assert_eq!(resolve_model_alias(raw, TEST_ALIASES), raw);
        }
    }

    #[test]
    fn unmapped_alias_falls_back_to_the_tier_name() {
        assert_eq!(resolve_model_alias("large", &[("small", "haiku")]), "large");
    }

    #[test]
    fn missing_aliases_fall_back_to_tier_name() {
        assert_eq!(resolve_model_alias("large", &[]), "large");
    }

    #[test]
    fn custom_non_tier_alias_keys_are_ignored() {
        assert_eq!(resolve_model_alias("fast", &[("fast", "haiku")]), "fast");
    }

    #[test]
    fn concrete_model_passes_through_unchanged() {
        assert_eq!(
            resolve_model_alias("claude-sonnet-4-5", TEST_ALIASES),
            "claude-sonnet-4-5"
        );
    }

    /// Reasoning effort reaches Claude Code as the variable it reads, under
    /// the level's own name; it used to be refused because Scion's
    /// provisioner, which this one follows, had no setting for it.
    #[test]
    fn effort_is_claude_codes_effort_level() {
        for (effort, level) in [
            (crate::Effort::Low, "low"),
            (crate::Effort::High, "high"),
            (crate::Effort::Xhigh, "xhigh"),
        ] {
            let mut context = crate::Context::new(
                "claude-code",
                crate::Protocol::ClaudeStreamJson,
                "/home/branch",
                "/work",
            );
            context.effort = Some(effort);
            let plan = crate::plan(&context).unwrap();
            let set = plan
                .env
                .iter()
                .find(|var| var.name == "CLAUDE_CODE_EFFORT_LEVEL");
            assert_eq!(set.map(|var| var.value.as_str()), Some(level));
        }
    }

    /// Telemetry turned off sets every variable that turns it off.
    #[test]
    fn pinned_telemetry_is_what_the_plan_sets_when_off() {
        let mut context = crate::Context::new(
            "claude-code",
            crate::Protocol::ClaudeStreamJson,
            "/home/branch",
            "/work",
        );
        context.telemetry = Some(crate::Telemetry {
            enabled: false,
            endpoint: None,
        });
        let plan = crate::plan(&context).unwrap();
        for (name, value) in TELEMETRY_OFF {
            let set = plan.env.iter().find(|var| var.name == *name);
            assert_eq!(set.map(|var| var.value.as_str()), Some(*value), "{name}");
        }
    }

    /// A pinned Vertex AI version passes through the alias table as given;
    /// only the size aliases are mapped.
    #[test]
    fn a_pinned_model_version_is_passed_as_given() {
        for pinned in ["claude-sonnet-4-5@20250929", "claude-haiku-4-5@20251001"] {
            assert_eq!(resolve_model_alias(pinned, MODEL_ALIASES), pinned);
        }
        assert_eq!(
            resolve_model_alias("large", MODEL_ALIASES),
            "claude-opus-5-5"
        );
    }
}
