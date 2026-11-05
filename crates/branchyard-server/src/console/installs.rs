//! Install a helper's harness on the server's own host.
//!
//! - `POST /v1/tools/helpers/{id}/install` (scope `admin`): accept an
//!   install of helper `id`'s harness and answer `202` with its status.
//! - `GET /v1/tools/helpers/{id}/install` (scope `admin`): the latest
//!   install's status: `queued`, `running`, `succeeded` or `failed`, with
//!   the command, the end of its output and why it failed.
//!
//! The command is the first one in the harness catalog
//! (`catalog/harnesses.toml`) whose programs are on the install `PATH`, the
//! plan `by harnesses install` makes ([`branchyard::inventory::plan`]),
//! run by `/bin/sh` as the server's user. The route is closed until the
//! operator sets `harness_install.allow`. It installs on this server's
//! host only: a worker on another machine keeps what it has, and the
//! response says `host: "server"`. When it succeeds the helper's row in
//! `GET /v1/tools` moves from `not_ready` to `ready`.
//!
//! The status lives in the server process, one entry per harness: a restart
//! forgets it, and a second `POST` while one is running returns the running
//! one. Each attempt is also appended to the harness log
//! (`<data_dir>/harness-events.jsonl`), the file `by harnesses` keeps.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path as FsPath;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use branchyard::inventory::{self, HarnessEvent, HarnessLog, InstallAction};
use branchyard_client::workspace_api::{HelperInstall, InstallState};
use branchyard_support::LockExt as _;

use super::presets;
use crate::api::{App, Caller, Shared};
use crate::error::ApiError;

/// How long an install command may run before it is stopped.
const LIMIT: Duration = Duration::from_secs(15 * 60);
/// The most characters of output a status keeps.
const TAIL: usize = 2000;

/// Each harness's latest install, in this process.
static JOBS: LazyLock<Mutex<BTreeMap<String, HelperInstall>>> = LazyLock::new(Default::default);

pub(crate) fn router() -> Router<Shared> {
    Router::new().route("/v1/tools/helpers/{id}/install", post(start).get(status))
}

fn job(harness: &str) -> Option<HelperInstall> {
    JOBS.lock_recovering("helper installs")
        .get(harness)
        .cloned()
}

fn change(harness: &str, edit: impl FnOnce(&mut HelperInstall)) {
    if let Some(install) = JOBS.lock_recovering("helper installs").get_mut(harness) {
        edit(install);
    }
}

fn failed(install: &mut HelperInstall, problem: String) {
    install.state = InstallState::Failed;
    install.finished_at_ms = Some(branchyard_support::time::now_ms());
    install.problem = Some(problem);
}

async fn status(
    Path(id): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<HelperInstall>, ApiError> {
    super::scope(&caller, "admin")?;
    job(&id).map(Json).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no install of {id} has been started on this server"),
        )
    })
}

async fn start(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, ApiError> {
    super::scope(&caller, "admin")?;
    crate::api::blocking(move || accept(&app, &caller, &id))
        .await?
        .map(|install| {
            let mut response = (StatusCode::ACCEPTED, Json(install.clone())).into_response();
            if let Ok(location) =
                HeaderValue::from_str(&format!("/v1/tools/helpers/{}/install", install.harness))
            {
                response.headers_mut().insert(header::LOCATION, location);
            }
            response
        })
}

/// Check the request and start the install on a thread of its own, or hand
/// back the one already under way.
fn accept(app: &Shared, caller: &Caller, id: &str) -> Result<HelperInstall, ApiError> {
    if !app.config.harness_install.allow {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "harness_install_disabled",
            "This server does not install harnesses from the console. An operator can allow it \
             with harness_install.allow in the server configuration.",
        ));
    }
    super::tools::known_harnesses(std::slice::from_ref(&id.to_owned()))?;
    let offered = presets::helper_harnesses(app, caller.tenant())?;
    let Some(helper) = offered.iter().find(|h| h.id == id) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{id} is not a helper offered to your tenant"),
        ));
    };
    if helper.ready {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_installed",
            format!("{id} is already installed on this server"),
        ));
    }
    if !helper.install {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "install_unsupported",
            format!(
                "{id} runs somewhere other than this server's own host, so the console cannot \
                 install it"
            ),
        ));
    }
    let catalog = inventory::harness_of(id)
        .ok_or_else(|| unsupported(id, "it is not in the harness catalog"))?;
    let path = install_path(app);
    let commands = branchyard_controls::catalog::harness(catalog)
        .map(|e| e.install.clone())
        .unwrap_or_default();
    let plan = inventory::plan(
        catalog,
        InstallAction::Install,
        None,
        &tools_on(&path, &commands),
    )
    .map_err(|why| unsupported(id, &why))?;
    let created = {
        let mut jobs = JOBS.lock_recovering("helper installs");
        if let Some(running) = jobs.get(id).filter(|j| !j.state.is_terminal()) {
            return Ok(running.clone());
        }
        let created = HelperInstall {
            harness: id.to_owned(),
            state: InstallState::Queued,
            host: "server".to_owned(),
            command: Some(plan.command.clone()),
            created_at_ms: branchyard_support::time::now_ms(),
            finished_at_ms: None,
            problem: None,
            output_tail: None,
        };
        jobs.insert(id.to_owned(), created.clone());
        created
    };
    let work = Work {
        app: app.clone(),
        harness: id.to_owned(),
        command: plan.command,
        path,
    };
    let panicked = id.to_owned();
    let spawned = branchyard_support::spawn_named(
        format!("install-{id}"),
        move |report| {
            change(&panicked, |install| {
                failed(install, format!("the install stopped: {}", report.message));
            });
        },
        move || work.run(),
    );
    if let Err(error) = spawned {
        change(id, |install| {
            failed(install, format!("could not start the install: {error}"));
        });
        return Err(ApiError::internal(format!(
            "could not start the install: {error}"
        )));
    }
    Ok(created)
}

fn unsupported(id: &str, why: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "install_unsupported",
        format!("the console cannot install {id}: {why}"),
    )
}

/// What the install thread needs of the server, owned.
struct Work {
    app: Shared,
    harness: String,
    command: String,
    path: OsString,
}

impl Work {
    fn run(self) {
        let started = branchyard_support::time::now_ms();
        change(&self.harness, |install| {
            install.state = InstallState::Running;
        });
        let (status, output) = match run(&self.command, &self.path) {
            Ok(ran) => ran,
            Err(why) => (None, why),
        };
        presets::forget_installed();
        let found = presets::is_installed(&self.app, &self.harness);
        let problem = match (status, &found) {
            (Some(0), Ok(true)) => None,
            (Some(0), Ok(false)) => Some(format!(
                "{} is still not found after the install (is the install directory on PATH?)",
                self.harness
            )),
            (Some(0), Err(error)) => Some(format!(
                "could not check for {} afterwards: {}",
                self.harness, error.body.message
            )),
            (Some(code), _) => Some(format!("the install command exited with status {code}")),
            (None, _) => Some("the install command did not run to the end".to_owned()),
        };
        let tail = tail(&output);
        change(&self.harness, |install| {
            install.output_tail = Some(tail.clone());
            match &problem {
                None => {
                    install.state = InstallState::Succeeded;
                    install.finished_at_ms = Some(branchyard_support::time::now_ms());
                }
                Some(problem) => failed(install, problem.clone()),
            }
        });
        let log = HarnessLog::new(self.app.config.data_dir.join("harness-events.jsonl"));
        branchyard_support::best_effort(
            "record a console harness install",
            log.append(&HarnessEvent {
                at_ms: started,
                on: "server".to_owned(),
                harness: self.harness.clone(),
                action: InstallAction::Install,
                by: "console".to_owned(),
                command: Some(self.command.clone()),
                outcome: match problem {
                    None => "verified".to_owned(),
                    Some(_) => "failed".to_owned(),
                },
                version_before: None,
                version_after: None,
                detail: problem,
            }),
        );
    }
}

/// The `PATH` install commands run with: the configured one, else the
/// server's own.
fn install_path(app: &App) -> OsString {
    match &app.config.harness_install.path {
        Some(path) => OsString::from(path),
        None => std::env::var_os("PATH").unwrap_or_default(),
    }
}

/// The programs of `commands` (the first word of each pipeline stage) that
/// are executable files on `path`, by name.
fn tools_on(path: &OsString, commands: &[String]) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for command in commands {
        for stage in command.split('|') {
            let Some(program) = stage.split_whitespace().next() else {
                continue;
            };
            if found.contains_key(program) || program.contains('/') {
                continue;
            }
            if let Some(at) = std::env::split_paths(path)
                .map(|dir| dir.join(program))
                .find(|candidate| executable(candidate))
            {
                found.insert(program.to_owned(), at.display().to_string());
            }
        }
    }
    found
}

fn executable(path: &FsPath) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run `command` with `/bin/sh` and `path` as its `PATH`, stdout and stderr
/// together, stopping it after [`LIMIT`].
fn run(command: &str, path: &OsString) -> Result<(Option<i32>, String), String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec 2>&1; {command}"))
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run /bin/sh: {e}"))?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(out) = stdout.as_mut() {
            let mut bytes = Vec::new();
            branchyard_support::best_effort("read install output", out.read_to_end(&mut bytes));
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    });
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() >= deadline => {
                branchyard_support::best_effort("stop a slow install", child.kill());
                branchyard_support::best_effort("reap a slow install", child.wait());
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(format!("could not wait for the install: {error}")),
        }
    };
    let output = reader.join().unwrap_or_default();
    Ok((status, output))
}

/// The end of `output`, at most [`TAIL`] characters.
fn tail(output: &str) -> String {
    let skip = output.chars().count().saturating_sub(TAIL);
    output.chars().skip(skip).collect()
}

/// Forget every install, so a test starts from none.
#[cfg(test)]
pub(super) fn reset() {
    JOBS.lock_recovering("helper installs").clear();
}
