// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338:
// harnesses/gemini-cli/provision.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// The OAuth credentials file is written from the GEMINI_OAUTH_CREDS secret
// rather than mounted; Vertex AI takes the project and location from the
// secrets given, not the container's environment, and the region only as
// GOOGLE_CLOUD_LOCATION (Scion also sets GOOGLE_CLOUD_REGION, which Gemini
// CLI 0.65.0's bundle reads only to label a Cloud Functions resource);
// settings.json is merged key by key and refused, not reset, when it
// cannot be parsed; telemetry
// goes to the collector the task names and is reconciled only when asked
// for; the native system prompt (GEMINI_SYSTEM_MD) and GEMINI.md
// projection are not used, because the ACP driver passes instructions in
// the session. Scion's `--yolo` launch flag and `"yolo": true` seed setting
// are not ported: Branchyard's ACP driver routes every permission request.

//! Gemini CLI: `gemini-cli-acp`.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, EnvVar, Plan, Provisioner,
    Refused, Via,
};

const SETTINGS: &str = ".gemini/settings.json";

/// The policy file a private home gets, passed with `--policy`.
pub(crate) const POLICY: &str = ".gemini/branchyard/policy.toml";

/// One rule above every user-tier rule Gemini CLI ships or a home holds:
/// every tool call asks the client. `--policy` replaces the home's
/// `~/.gemini/policies` and the settings' `policyPaths`, so in a private
/// home no user rule outranks it; administrator rules still do (Gemini CLI
/// 0.66.0, 3 November 2026).
pub const ASK_EVERY_CALL: &str = "# Written by Branchyard: every tool call asks its client.\n\
     [[rule]]\ntoolName = \"*\"\ndecision = \"ask_user\"\npriority = 360\n";
pub(crate) const OAUTH_CREDS: &str = ".gemini/oauth_creds.json";

/// The variable Gemini CLI reads Vertex AI's region from.
const LOCATION: &str = "GOOGLE_CLOUD_LOCATION";

/// The secrets `vertex-ai` takes the region from, as [`LOCATION`]: its own
/// name first, then Scion's other one.
const REGION: [&str; 2] = [LOCATION, "GOOGLE_CLOUD_REGION"];

pub const AUTH: Spec = Spec {
    harness: "gemini-cli",
    methods: &[
        Method::env(
            "api-key",
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            "set GEMINI_API_KEY or GOOGLE_API_KEY",
        ),
        Method::file(
            "auth-file",
            "GEMINI_OAUTH_CREDS",
            "provide GEMINI_OAUTH_CREDS, the content of ~/.gemini/oauth_creds.json",
        ),
        Method::env(
            "vertex-ai",
            &["GOOGLE_CLOUD_PROJECT"],
            "set GOOGLE_CLOUD_PROJECT (with ADC available to the harness) for Vertex AI",
        ),
    ],
};

/// Scion's map from a method to Gemini CLI's `security.auth.selectedType`.
pub fn selected_type(method: &str) -> Option<&'static str> {
    match method {
        "api-key" => Some("gemini-api-key"),
        "auth-file" => Some("oauth-personal"),
        "vertex-ai" => Some("vertex-ai"),
        _ => None,
    }
}

pub(crate) struct Gemini;

impl Provisioner for Gemini {
    fn harness(&self) -> &'static str {
        "gemini-cli"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("gemini-cli")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        if context.effort.is_some() {
            return Err(unsupported(
                context,
                "reasoning effort",
                "Scion's Gemini CLI provisioner has no setting for it",
            ));
        }
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let mut settings = Vec::new();
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            plan.auth = Some(resolved.method.to_owned());
            let secret = |name: &str| context.secret(name).unwrap_or_default().to_owned();
            match resolved.method {
                "api-key" => {
                    let key = resolved.env()?;
                    // Not verified offline whether Gemini CLI's shell
                    // tool filters it: assumed to pass it on.
                    plan.secret_env(key, key, &secret(key), true);
                }
                "auth-file" => {
                    let content = secret("GEMINI_OAUTH_CREDS");
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        return Err(Refused(
                            "the GEMINI_OAUTH_CREDS secret is not valid JSON".into(),
                        ));
                    }
                    plan.edit(OAUTH_CREDS, true, Edit::Put(content));
                    plan.deliver(
                        "GEMINI_OAUTH_CREDS",
                        Via::File {
                            path: OAUTH_CREDS.into(),
                        },
                        false,
                    );
                }
                "vertex-ai" => {
                    plan.secret_env(
                        "GOOGLE_CLOUD_PROJECT",
                        "GOOGLE_CLOUD_PROJECT",
                        &secret("GOOGLE_CLOUD_PROJECT"),
                        true,
                    );
                    if let Some(name) = REGION.iter().find(|n| context.secret(n).is_some()) {
                        plan.secret_env(name, LOCATION, &secret(name), true);
                    }
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
            let selected = selected_type(resolved.method).ok_or_else(|| {
                Refused(format!(
                    "auth method {:?} has no Gemini auth type",
                    resolved.method
                ))
            })?;
            settings.push(JsonEdit::Set(
                path(&["security", "auth", "selectedType"]),
                json!(selected),
            ));
        }
        let mut names = AUTH.names();
        names.extend(REGION);
        plan.unused_secrets = unused(context, &names);
        if let Some(telemetry) = &context.telemetry {
            let on = telemetry.enabled;
            let endpoint = telemetry.endpoint();
            for (name, value) in [
                (
                    "GEMINI_TELEMETRY_ENABLED",
                    if on { "true" } else { "false" },
                ),
                ("GEMINI_TELEMETRY_TARGET", "local"),
                ("GEMINI_TELEMETRY_OTLP_ENDPOINT", endpoint),
                ("GEMINI_TELEMETRY_OTLP_PROTOCOL", "grpc"),
                ("GEMINI_TELEMETRY_LOG_PROMPTS", "false"),
                ("GEMINI_TELEMETRY_TRACES_ENABLED", "false"),
                ("GEMINI_TELEMETRY_USE_COLLECTOR", "false"),
                ("GEMINI_TELEMETRY_OUTFILE", ""),
            ] {
                plan.set_env(EnvVar::plain(name, value));
            }
            settings.push(JsonEdit::Set(
                path(&["telemetry"]),
                json!({
                    "enabled": on, "target": "local", "otlpEndpoint": endpoint,
                    "otlpProtocol": "grpc", "logPrompts": false, "traces": false,
                    "useCollector": false,
                }),
            ));
        }
        if let Some(model) = &context.model {
            settings.push(JsonEdit::Set(path(&["model", "name"]), json!(model)));
        }
        // The mode that asks: `autoEdit` and `yolo` approve on their own, and
        // the driver refuses a session that reports another.
        plan.session
            .args
            .extend(["--approval-mode".into(), "default".into()]);
        if context.private_home {
            plan.edit(POLICY, false, Edit::Put(ASK_EVERY_CALL.into()));
            plan.session
                .args
                .extend(["--policy".into(), context.home_path(POLICY)]);
        }
        if !settings.is_empty() {
            needs_private_home(context, "Gemini CLI's settings.json")?;
            plan.edit(
                SETTINGS,
                false,
                Edit::Json {
                    edits: settings,
                    comment_lines: false,
                },
            );
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protocol;

    /// Every Gemini CLI session asks in `default`; a home Branchyard
    /// prepares also gets the policy file that sends every call to it, the
    /// one `--policy` names.
    #[test]
    fn the_session_asks_and_a_private_home_gets_the_policy() {
        let mut context = Context::new("gemini-cli", Protocol::Acp, "/h", "/w");
        let plan = Gemini.plan(&context).unwrap();
        assert_eq!(plan.session.args, ["--approval-mode", "default"]);
        assert!(plan.files.is_empty());

        context.private_home = true;
        let plan = Gemini.plan(&context).unwrap();
        assert_eq!(
            plan.session.args,
            [
                "--approval-mode",
                "default",
                "--policy",
                "/h/.gemini/branchyard/policy.toml"
            ]
        );
        let file = plan.files.iter().find(|f| f.path == POLICY).unwrap();
        assert!(!file.secret);
        assert_eq!(file.edits, [Edit::Put(ASK_EVERY_CALL.into())]);
        for line in [
            "toolName = \"*\"",
            "decision = \"ask_user\"",
            "priority = 360",
        ] {
            assert!(ASK_EVERY_CALL.contains(line), "{line}");
        }
    }
}
