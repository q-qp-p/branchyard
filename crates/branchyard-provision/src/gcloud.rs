//! Google Cloud's Application Default Credentials (ADC), for every harness.
//!
//! Claude Code, Gemini CLI and OpenCode reach Vertex AI with ADC. Google's
//! client libraries look for the file `GOOGLE_APPLICATION_CREDENTIALS`
//! names, then the one `gcloud auth application-default login` writes
//! under `~/.config/gcloud`, then a Google VM's metadata server. A private
//! home hides the second, so the [`SECRET`] secret carries that file, or a
//! service account key, into it: written 0600 at [`PATH`], and named by
//! `GOOGLE_APPLICATION_CREDENTIALS` in the harness's environment, whichever
//! harness runs. The variable holds the file's path, not the credential.
//! A later turn without the secret removes the file a turn wrote.

use crate::edit::Edit;
use crate::{needs_private_home, Context, EnvVar, Plan, Refused, Via};

/// The secret holding the credentials file's content.
pub const SECRET: &str = "GCLOUD_ADC";

/// The file gcloud writes, under its configuration directory.
pub const FILE: &str = "application_default_credentials.json";

/// Where it is written, relative to the home: [`FILE`] under gcloud's
/// default configuration directory, `~/.config/gcloud`.
pub const PATH: &str = ".config/gcloud/application_default_credentials.json";

/// The variable that names it.
pub const VARIABLE: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// Write the credentials into the home and name them, when the secret is
/// given; remove the ones an earlier turn wrote, when it is not.
pub(crate) fn provision(context: &Context, plan: &mut Plan) -> Result<(), Refused> {
    let Some(content) = context.secret(SECRET) else {
        if context
            .installed
            .credentials
            .iter()
            .any(|c| c.path() == PATH)
        {
            plan.edit(PATH, true, Edit::Remove);
        }
        return Ok(());
    };
    needs_private_home(context, "Application Default Credentials")?;
    if serde_json::from_str::<serde_json::Value>(content).is_err() {
        return Err(Refused(format!("the {SECRET} secret is not valid JSON")));
    }
    plan.edit(PATH, true, Edit::Put(content.to_owned()));
    plan.set_env(EnvVar::plain(VARIABLE, context.home_path(PATH)));
    plan.deliver(SECRET, Via::File { path: PATH.into() }, false);
    plan.unused_secrets.retain(|name| name != SECRET);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Protocol, Secret};

    #[test]
    fn the_path_is_gclouds_file_under_its_default_directory() {
        assert_eq!(PATH, format!(".config/gcloud/{FILE}"));
    }

    #[test]
    fn credentials_that_are_not_json_are_refused_without_their_content() {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        context.secrets = vec![Secret::new(SECRET, "refresh-token-SECRET")];
        let refused = crate::plan(&context).unwrap_err().0;
        assert_eq!(refused, "the GCLOUD_ADC secret is not valid JSON");
    }
}
