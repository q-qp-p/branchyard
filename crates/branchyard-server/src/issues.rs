//! Starting from an issue (`TaskRequest::issue`, `StartSessionRequest::issue`,
//! `GET /v1/repos/{repo}/issue`, `GET /v1/workspaces/{id}/issue`).
//!
//! The server does not fetch issues itself: it runs the operator-selected
//! `by` (`by_path`, else `by` on `PATH`) as `by issue --json REF`, in the
//! repository, with the server's own environment, so an issue resolves
//! exactly as `by run --issue` resolves it, with the same `gh` login, the
//! same Linear, Jira and GitLab credentials, `[trackers]` settings and
//! connector gateway, and one implementation of the prompt, the branch name
//! and the link (`docs/pull-requests.md`). Nothing from the request but the
//! reference and the extra instructions reaches that command, as arguments
//! after `--`, never through a shell. `BRANCHYARD_*` variables are removed
//! from its environment so a server started inside a branch cannot make it
//! act as that branch.
//!
//! The reference is the caller's, and the command runs with the server's
//! credentials, so a reference that is a URL is checked first, before
//! anything runs ([`check_reference`]): it must name the repository's own
//! GitHub issues, Linear, or a Jira or GitLab site the operator set in
//! `JIRA_URL` or `GITLAB_URL`. Anything else is refused with
//! `issue_unavailable`. `by issue --no-url-site` repeats the check where
//! the credentials are used and ignores `[trackers] url` from the
//! repository's `branchyard.toml`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;

/// How long fetching one issue may take.
const DEADLINE: Duration = Duration::from_secs(60);
/// The longest reference taken.
const MAX_REFERENCE: usize = 512;
/// How much of the command's complaint is quoted back: one line.
const MAX_COMPLAINT: usize = 200;
/// The longest additional instructions taken.
const MAX_PROMPT: usize = 32 << 10;

/// An issue as `by issue --json` resolved it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Resolved {
    /// The link recorded on the branch: `number`, `url`, `title`, and for
    /// another tracker `tracker` and `key`.
    pub issue: Value,
    /// The branch name `by run --issue` would give it.
    pub branch: String,
    /// The prompt: the issue under a header, then any extra instructions.
    pub prompt: String,
}

impl Resolved {
    /// The issue's title, for a session's default name.
    pub fn title(&self) -> &str {
        self.issue["title"].as_str().unwrap_or_default()
    }
}

/// The `ref` of a query string (`?ref=%2312`), decoded.
pub(crate) fn reference_of(raw: Option<&str>) -> Option<String> {
    raw?.split('&')
        .filter_map(|part| part.split_once('='))
        .find(|(key, _)| *key == "ref")
        .map(|(_, value)| branchyard_client::http::decode_form(value))
        .filter(|value| !value.trim().is_empty())
}

fn unavailable(reference: &str, why: impl Into<String>, next: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "issue_unavailable",
        format!("{} (next: {next})", why.into()),
    )
    .detail(json!({ "reference": reference, "next": next }))
}

/// The sites an operator set for the trackers a URL can name.
#[derive(Clone, Debug, Default)]
pub(crate) struct Sites {
    /// `JIRA_URL`.
    pub jira: Option<String>,
    /// `GITLAB_URL`.
    pub gitlab: Option<String>,
    /// The repository's `origin` as `(host, owner, repo)`.
    pub origin: Option<(String, String, String)>,
}

impl Sites {
    /// The server's environment and the repository at `root`.
    async fn of(root: &Path) -> Sites {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let remote = tokio::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(root)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
        Sites {
            jira: var("JIRA_URL"),
            gitlab: var("GITLAB_URL"),
            origin: remote.as_deref().and_then(origin_of),
        }
    }
}

/// `(host, owner, repo)` of a remote: `https://host/owner/repo(.git)`,
/// `ssh://git@host/owner/repo` or `git@host:owner/repo`.
fn origin_of(remote: &str) -> Option<(String, String, String)> {
    let (authority, path) = match remote.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => remote.split_once(':')?,
    };
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    let mut parts = path.trim_matches('/').split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some((host, owner.to_ascii_lowercase(), repo.to_ascii_lowercase()))
}

fn same_site(configured: Option<&String>, url: &url::Url) -> bool {
    let Some(site) = configured.and_then(|site| url::Url::parse(site.trim()).ok()) else {
        return false;
    };
    site.scheme() == url.scheme()
        && site.host_str().map(str::to_ascii_lowercase)
            == url.host_str().map(str::to_ascii_lowercase)
        && site.port_or_known_default() == url.port_or_known_default()
}

/// Refuse a URL reference that does not name a place the server may read
/// with its own credentials. A reference that is not a URL (`#12`,
/// `linear:ENG-1`) is not this check's business.
pub(crate) fn check_reference(reference: &str, sites: &Sites) -> Result<(), ApiError> {
    let lower = reference.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Ok(());
    }
    let refuse = |why: &str| {
        Err(unavailable(
            reference,
            why,
            "use #N or N for this repository's issues, or ask the operator to set JIRA_URL or \
             GITLAB_URL for the tracker's site (docs/pull-requests.md)",
        ))
    };
    let Ok(url) = url::Url::parse(reference) else {
        return refuse("the issue URL is not a URL");
    };
    if !url.username().is_empty() || url.password().is_some() {
        return refuse("the issue URL carries credentials");
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if url.scheme() == "https" && (host == "linear.app" || host.ends_with(".linear.app")) {
        return Ok(());
    }
    if same_site(sites.jira.as_ref(), &url) || same_site(sites.gitlab.as_ref(), &url) {
        return Ok(());
    }
    if let Some((remote_host, owner, repo)) = &sites.origin {
        let parts: Vec<String> = url
            .path_segments()
            .map(|p| {
                p.filter(|p| !p.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        if host == *remote_host
            && parts.len() >= 3
            && parts[0] == *owner
            && parts[1] == *repo
            && parts[2] == "issues"
        {
            return Ok(());
        }
    }
    refuse("the issue URL names a site or repository this server is not set up to read")
}

/// Fetch `reference` by running `by issue --json` in `root`, with `extra`
/// added to the prompt.
pub(crate) async fn resolve(
    by: &Path,
    root: &Path,
    reference: &str,
    extra: &str,
) -> Result<Resolved, ApiError> {
    let reference = reference.trim();
    if reference.is_empty() || reference.len() > MAX_REFERENCE || reference.contains('\0') {
        return Err(ApiError::bad_request(
            "issue is empty, longer than 512 bytes, or not text",
        ));
    }
    if extra.len() > MAX_PROMPT {
        return Err(ApiError::bad_request(format!(
            "the additional instructions for an issue are longer than {MAX_PROMPT} bytes"
        )));
    }
    check_reference(reference, &Sites::of(root).await)?;
    let mut command = tokio::process::Command::new(by);
    command
        .args(["issue", "--json", "--no-url-site", "--"])
        .arg(reference)
        .arg(extra)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("BRANCHYARD_") {
            command.env_remove(name);
        }
    }
    command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
    let output = match tokio::time::timeout(DEADLINE, command.output()).await {
        Err(_) => {
            return Err(unavailable(
                reference,
                "fetching the issue took longer than 60 seconds",
                "retry; check that the issue's tracker is reachable from the server",
            ))
        }
        Ok(Err(_)) => {
            return Err(unavailable(
                reference,
                "the server could not run `by issue`",
                "ask the operator to set by_path to a by binary (docs/server.md)",
            ))
        }
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        // One line, without control characters or anything URL-shaped: a
        // reason, not what the command printed.
        let said: String = said
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("by: "))
            .unwrap_or_default()
            .split_whitespace()
            .filter(|word| !word.contains("://"))
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_COMPLAINT)
            .collect();
        return Err(unavailable(
            reference,
            if said.is_empty() {
                "the issue could not be fetched".to_owned()
            } else {
                format!("the issue could not be fetched: {said}")
            },
            "check the reference, and that the server's gh login or tracker credentials can read \
             it (docs/pull-requests.md)",
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| ApiError::internal(format!("by issue printed something unexpected: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::Scratch;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `by` that answers `issue --json` from the script's body.
    fn fake_by(dir: &Scratch, body: &str) -> std::path::PathBuf {
        let path = dir.join("by");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_reference_is_the_decoded_ref_of_the_query() {
        assert_eq!(reference_of(Some("ref=%2312")).as_deref(), Some("#12"));
        assert_eq!(
            reference_of(Some("x=1&ref=linear%3AENG-1")).as_deref(),
            Some("linear:ENG-1")
        );
        assert_eq!(reference_of(Some("ref=")), None);
        assert_eq!(reference_of(None), None);
    }

    #[tokio::test]
    async fn an_issue_resolves_through_by_with_its_arguments_after_a_double_dash() {
        let dir = Scratch::new("issues");
        let by = fake_by(
            &dir,
            r#"printf '%s\n' "$*" > "$(dirname "$0")/args"
printf '{"issue": {"number": 12, "url": "u", "title": "T"}, "branch": "issue-12-t", "prompt": "P"}'"#,
        );
        let got = resolve(&by, dir.path(), "#12", "more").await.unwrap();
        assert_eq!(got.branch, "issue-12-t");
        assert_eq!(got.prompt, "P");
        assert_eq!(got.title(), "T");
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(args.trim(), "issue --json --no-url-site -- #12 more");
    }

    #[tokio::test]
    async fn an_issue_that_cannot_be_fetched_is_refused_with_its_reason_and_a_next_step() {
        let dir = Scratch::new("issues");
        let by = fake_by(
            &dir,
            "echo 'by: fetching' >&2; echo 'error: Linear needs LINEAR_API_KEY' >&2; exit 3",
        );
        let error = resolve(&by, dir.path(), "linear:ENG-1", "")
            .await
            .unwrap_err();
        let text = format!("{error:?}");
        assert!(text.contains("issue_unavailable"), "{text}");
        assert!(text.contains("Linear needs LINEAR_API_KEY"), "{text}");
        assert!(!text.contains("by: fetching"), "{text}");
        assert!(text.contains("next:"), "{text}");
    }

    /// A fake `by` that records that it ran, and answers like a real one.
    fn recording_by(dir: &Scratch) -> std::path::PathBuf {
        fake_by(
            dir,
            r#"echo "$*" >> "$(dirname "$0")/ran"
env | grep '^BRANCHYARD_' > "$(dirname "$0")/env"
printf '{"issue": {"number": 1, "url": "u", "title": "T"}, "branch": "b", "prompt": "P"}'"#,
        )
    }

    fn ran(dir: &Scratch) -> String {
        std::fs::read_to_string(dir.join("ran")).unwrap_or_default()
    }

    #[tokio::test]
    async fn nothing_that_is_not_a_reference_is_run() {
        let dir = Scratch::new("issues");
        let by = recording_by(&dir);
        for bad in ["", "   ", &"x".repeat(513)] {
            assert!(resolve(&by, dir.path(), bad, "").await.is_err());
        }
        let too_long = "y".repeat(MAX_PROMPT + 1);
        let error = resolve(&by, dir.path(), "#1", &too_long).await.unwrap_err();
        assert!(format!("{error:?}").contains("400"), "{error:?}");
        assert_eq!(
            ran(&dir),
            "",
            "`by` ran for something that is not a reference"
        );
        let missing = dir.join("no-such-by");
        let error = resolve(&missing, dir.path(), "#1", "").await.unwrap_err();
        assert!(format!("{error:?}").contains("by_path"));
    }

    #[tokio::test]
    async fn a_url_naming_another_host_is_refused_before_by_runs() {
        let dir = Scratch::new("issues");
        let by = recording_by(&dir);
        for url in [
            "http://attacker.example/browse/ENG-1",
            "https://attacker.example/g/p/-/issues/1",
            "https://github.com/other/private/issues/1",
            "https://user:pw@linear.app/acme/issue/ENG-1",
            "https://linear.app.attacker.example/acme/issue/ENG-1",
        ] {
            let error = resolve(&by, dir.path(), url, "").await.unwrap_err();
            let text = format!("{error:?}");
            assert!(text.contains("issue_unavailable"), "{url}: {text}");
            assert!(text.contains("next:"), "{url}: {text}");
            assert!(text.contains("422"), "{url}: {text}");
        }
        assert_eq!(ran(&dir), "", "`by` ran with a caller-chosen host");
    }

    #[test]
    fn a_url_is_accepted_only_for_a_site_the_operator_or_the_repository_set() {
        let sites = Sites {
            jira: Some("https://jira.example.com/".into()),
            gitlab: Some("http://gitlab.internal:8080".into()),
            origin: origin_of("ssh://git@github.com/Acme/Widgets.git"),
        };
        for ok in [
            "#12",
            "linear:ENG-1",
            "https://linear.app/acme/issue/ENG-1/x",
            "https://jira.example.com/browse/ENG-1",
            "http://gitlab.internal:8080/g/p/-/issues/3",
            "https://github.com/acme/widgets/issues/9",
        ] {
            assert!(check_reference(ok, &sites).is_ok(), "{ok}");
        }
        for bad in [
            "http://jira.example.com/browse/ENG-1",
            "https://jira.example.com:8443/browse/ENG-1",
            "http://gitlab.internal/g/p/-/issues/3",
            "https://github.com/acme/other/issues/9",
            "https://github.com/acme/widgets/pull/9",
            "https://ghe.example.com/acme/widgets/issues/9",
        ] {
            assert!(check_reference(bad, &sites).is_err(), "{bad}");
        }
        assert!(check_reference("https://jira.example.com/browse/E-1", &Sites::default()).is_err());
        assert_eq!(
            origin_of("https://github.com/acme/widgets"),
            Some(("github.com".into(), "acme".into(), "widgets".into()))
        );
        assert_eq!(
            origin_of("ssh://git@ghe.example.com:2222/acme/widgets.git"),
            Some(("ghe.example.com".into(), "acme".into(), "widgets".into()))
        );
        assert_eq!(
            origin_of("git@github.com:acme/widgets.git"),
            Some(("github.com".into(), "acme".into(), "widgets".into()))
        );
    }

    #[tokio::test]
    async fn the_commands_complaint_is_one_short_line_without_urls() {
        let dir = Scratch::new("issues");
        let by = fake_by(
            &dir,
            "echo 'error: https://x.example/?token=abc refused ok' >&2; echo second line >&2; exit 1",
        );
        let error = resolve(&by, dir.path(), "#1", "").await.unwrap_err();
        let text = format!("{error:?}");
        assert!(text.contains("refused ok"), "{text}");
        assert!(
            !text.contains("token=abc") && !text.contains("second line"),
            "{text}"
        );
    }

    /// A variable of the kind the server must not pass on, set once for
    /// the test binary under a name no other test reads.
    fn leaking_variable() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| std::env::set_var("BRANCHYARD_ISSUES_TEST_LEAK", "branch-x"));
    }

    #[tokio::test]
    async fn by_issue_runs_without_the_servers_branchyard_variables() {
        let dir = Scratch::new("issues");
        let by = recording_by(&dir);
        leaking_variable();
        assert!(std::env::var("BRANCHYARD_ISSUES_TEST_LEAK").is_ok());
        let got = resolve(&by, dir.path(), "#1", "").await.unwrap();
        assert_eq!(got.branch, "b");
        let env = std::fs::read_to_string(dir.join("env")).unwrap();
        assert_eq!(env.trim(), "", "BRANCHYARD_* reached `by issue`: {env}");
    }
}
