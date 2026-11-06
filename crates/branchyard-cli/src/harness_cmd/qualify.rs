//! `by harnesses update ID --qualify` and `by harnesses qualified`: a new
//! release installed into a private prefix, checked without a model, and
//! pinned only if it passes. The checks, the pin and its file are the SDK's
//! (`branchyard::qualified`); this asks for permission, runs the install,
//! adds the hook-install check, optionally runs the recorded scenarios with
//! a model, renders and logs. See docs/harness-lifecycle.md, "Qualifying an
//! update".

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use branchyard::inventory::{self, DetectOptions, HarnessEvent, InstallAction, Permission};
use branchyard::qualified::{self, Candidate, Check, QualifiedPin};
use branchyard_controls::catalog;
use serde_json::{json, Value};

use super::{
    detect, entry, interactive, not_in_harness, policy, record as log_event, user_dir, Machine,
};
use crate::args::HarnessChange;
use crate::commands::{print, Env, Failure, Outcome, Target};
use crate::errors::Report;
use crate::json;
use crate::render::{table, Cell, Column, Tone};

/// The variable naming the `branchyard-qualify` program `--with-model` runs.
pub(super) const RUNNER_ENV: &str = "BRANCHYARD_QUALIFY";

/// The pin `by harnesses update ID` installs when no `--version` is given:
/// a qualified pin newer than the version the default profile was checked
/// against.
pub(super) fn pin_to_install(id: &str) -> Option<QualifiedPin> {
    qualified::qualified_pin(id).filter(|pin| qualified::newer_than_checked(id, &pin.version))
}

/// Where candidate releases are installed: `candidates/ID/VERSION` in the
/// user's cache directory.
fn candidate_prefix(id: &str, version: &str) -> Result<PathBuf, Failure> {
    let cache = dirs::cache_dir().ok_or_else(|| {
        Report::new(
            "config",
            "this machine has no cache directory to install into",
        )
        .next("set XDG_CACHE_HOME or HOME, then run this again")
    })?;
    Ok(cache
        .join("branchyard")
        .join("candidates")
        .join(id)
        .join(version))
}

/// Where the dated record of a run with a model is written.
fn record_path(profile: &str, version: &str) -> PathBuf {
    user_dir()
        .join("qualification")
        .join(format!("{profile}-{version}.json"))
}

/// A release number is a word of letters, digits, `.`, `+` and `-` that
/// starts with a digit: it goes into a shell command.
fn valid_version(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

/// The `branchyard-qualify` program: `BRANCHYARD_QUALIFY`, else beside `by`,
/// else on `PATH`.
fn runner() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(RUNNER_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("branchyard-qualify")))
        .filter(|p| p.is_file());
    beside.or_else(|| {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join("branchyard-qualify"))
                .find(|p| p.is_file())
        })
    })
}

/// The hook and plugin files Branchyard installs for the harness still
/// parse, and its manifest still detects the program the release installs.
fn plugin_check(id: &str, bin: &Path) -> Check {
    let name = bin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match branchyard_setup::install::manifest(id) {
        Err(why) => Check::fail("plugin", format!("the packaged install files: {why}")),
        Ok(None) => Check::pass(
            "plugin",
            format!("Branchyard installs no hook or plugin files for {id}"),
        ),
        Ok(Some(manifest)) if !manifest.binaries.contains(&name) => Check::fail(
            "plugin",
            format!(
                "the install manifest detects {id} by {} but the release installs `{name}`",
                manifest.binaries.join(" or ")
            ),
        ),
        Ok(Some(manifest)) => Check::pass(
            "plugin",
            format!(
                "{} packaged file(s) parse; the manifest detects `{name}`",
                manifest.files.len()
            ),
        ),
    }
}

/// `by harnesses update ID --qualify`.
pub(super) fn run(
    env: &Env,
    target: &Target,
    as_json: bool,
    on: Option<&str>,
    change: &HarnessChange,
    with_model: bool,
) -> Outcome {
    let harness = entry(change.id.as_str())?;
    not_in_harness("qualify")?;
    qualify(env, target, as_json, on, change, with_model, harness)
}

/// [`run`], once the harness is known and no branch is running it.
fn qualify(
    env: &Env,
    target: &Target,
    as_json: bool,
    on: Option<&str>,
    change: &HarnessChange,
    with_model: bool,
    harness: &catalog::HarnessEntry,
) -> Outcome {
    let id = change.id.as_str();
    if let Target::Remote(_) = target {
        return Err(
            Report::usage("a server's workers never install harnesses themselves")
                .next(format!(
                    "qualify it on a machine, without --remote: by harnesses update {id} --qualify"
                ))
                .into(),
        );
    }
    if !matches!(Machine::parse(on)?, Machine::Local) {
        return Err(
            Report::usage("--qualify installs and runs the release on this machine")
                .next(format!("drop --on: by harnesses update {id} --qualify"))
                .into(),
        );
    }
    let latest = crate::live_catalog::latest(id);
    let version = match (&change.version, &latest) {
        (Some(version), _) => version.clone(),
        (None, Some(latest)) => latest.latest.clone(),
        (None, None) => return Err(Report::usage(format!(
            "there is no release of {id} to qualify"
        ))
        .line(
            "why",
            "no --version was given and `by catalog refresh` has not cached npm's latest",
        )
        .next(format!(
            "by harnesses update {id} --qualify --version VERSION"
        ))
        .next("by catalog refresh --only harnesses (caches npm's latest releases)")
        .into()),
    };
    if !valid_version(&version) {
        return Err(
            Report::usage(format!("{version:?} is not a release number"))
                .next(format!(
                    "by harnesses update {id} --qualify --version 1.2.3"
                ))
                .into(),
        );
    }
    let Some(profile) = qualified::profile_of(id) else {
        return Err(Report::new(
            "unsupported",
            format!("{id} has no driver profile, so there is no handshake to qualify it with"),
        )
        .next("by harnesses --profiles (lists the harnesses Branchyard drives)")
        .into());
    };
    let prefix = candidate_prefix(id, &version)?;
    let side = qualified::side_install(id, &version, &prefix).map_err(|refusal| {
        let report = Report::new(refusal.kind, format!("not qualifying {id} {version}: {}", refusal.why));
        match refusal.kind {
            "unsupported" => report
                .next(format!("by harnesses update {id} --version {version} --yes (updates in place, unqualified)"))
                .next(format!("install {version} by hand, then by harnesses --refresh")),
            _ => report.next("by harnesses --all (lists the harnesses in the catalog)"),
        }
    })?;
    let options = DetectOptions {
        only: Some(vec![id.to_owned()]),
        ..DetectOptions::from_env()
    };
    let before = detect(env, &Machine::Local, &options, true)?;
    if !before.tools.contains_key("npm") {
        return Err(Report::new(
            "tool_unavailable",
            format!("npm is not on this machine's PATH, and {id} {version} installs with it"),
        )
        .next("install Node.js and npm, then run this again")
        .into());
    }
    let current = before.get(id).and_then(|c| c.version.clone());
    let event = |outcome: &str, detail: Option<String>, passed: bool| HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: "local".into(),
        harness: id.to_owned(),
        action: InstallAction::Qualify,
        by: "by harnesses".into(),
        command: Some(side.command.clone()),
        outcome: outcome.into(),
        version_before: current.clone(),
        version_after: passed.then(|| version.clone()),
        detail,
    };
    let policy = policy(env)?;
    match policy.permits(id, change.yes, interactive(env)) {
        Permission::Run => {}
        Permission::Ask => {
            let answer = crate::console::terminal_prompt(&format!(
                "by: install {}@{version} into {} to qualify it, by running `{}`? [y/N] ",
                side.package,
                prefix.display(),
                side.command
            ))?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
                log_event(event("refused", Some("not confirmed".into()), false));
                return Err(Report::new(
                    "denied",
                    format!("not confirmed; {id} {version} was not qualified"),
                )
                .next(format!(
                    "by harnesses update {id} --qualify --yes (runs `{}` without asking)",
                    side.command
                ))
                .into());
            }
        }
        Permission::Refused(why) => {
            log_event(event("refused", Some(why.clone()), false));
            let mut report =
                super::refused_install(&policy, id, InstallAction::Update, &side.command, &why);
            // The refusal names `update ID --yes`, which would update in place.
            let plain = format!("by harnesses update {id} --yes");
            for next in &mut report.next {
                *next = next.replace(&plain, &format!("by harnesses update {id} --qualify --yes"));
            }
            return Err(report.into());
        }
    }

    // A clean prefix each time: a release qualified earlier is not this one.
    if prefix.exists() {
        std::fs::remove_dir_all(&prefix)?;
    }
    std::fs::create_dir_all(&prefix)?;
    eprintln!("by: on this machine: {}", side.command);
    let (status, output) = match inventory::run_local(&side.command) {
        Ok(ran) => ran,
        Err(why) => (None, why),
    };
    let bin = qualified::installed_binary(id, &prefix);
    let mut checks: Vec<Check> = Vec::new();
    match (&bin, status) {
        (Some(bin), Some(0)) => checks.push(Check::pass(
            "install",
            format!(
                "npm installed {}@{version}; `{}`",
                side.package,
                bin.display()
            ),
        )),
        (_, status) => checks.push(Check::fail(
            "install",
            match status {
                Some(0) => format!(
                    "npm succeeded but installed none of {} under {}/node_modules/.bin",
                    harness.binaries.join(", "),
                    prefix.display()
                ),
                Some(code) => format!("npm exited with status {code}: {}", last_lines(&output)),
                None => format!("npm did not run to the end: {}", last_lines(&output)),
            },
        )),
    }
    let integrity = qualified::lock_integrity(&prefix, &side.package).or_else(|| {
        latest
            .as_ref()
            .filter(|l| l.latest == version)
            .and_then(|l| l.integrity.clone())
    });
    if let Some(bin) = bin.as_ref().filter(|_| checks[0].passed) {
        let keep_env: Vec<String> = inventory::credential_variables(id)
            .into_iter()
            .filter(|name| std::env::var_os(name).is_some())
            .collect();
        let workdir = prefix.join(".qualify");
        checks.extend(qualified::run_checks(&Candidate {
            harness: id,
            version: &version,
            bin,
            workdir: &workdir,
            keep_env: &keep_env,
        }));
        checks.push(plugin_check(id, bin));
    }
    let record = record_path(profile.id, &version);
    let mut live_record = None;
    if with_model {
        match checks.iter().all(|c| c.passed) {
            false => eprintln!(
                "by: --with-model skipped: a check failed, and the run would spend credentials"
            ),
            true => {
                let live = match bin.as_deref() {
                    Some(bin) => live_run(id, profile.id, &side, bin, &record, &prefix),
                    None => Check::fail("live", "no binary to run"),
                };
                if live.passed {
                    live_record = Some(record.display().to_string());
                }
                checks.push(live);
            }
        }
    }
    let passed = checks.iter().all(|c| c.passed);
    let file = qualified::qualified_file();
    let pin =
        passed.then(|| {
            let carried = qualified::read_pins(&file)
                .unwrap_or_default()
                .into_iter()
                .find(|p| p.id == id && p.version == version)
                .and_then(|p| p.live_record)
                .filter(|r| Path::new(r).is_file());
            QualifiedPin {
                id: id.to_owned(),
                version: version.clone(),
                package: side.package.clone(),
                profile: profile.id.to_owned(),
                integrity,
                qualified_at: branchyard_support::time::rfc3339_secs(
                    branchyard_support::time::now_ms(),
                ),
                live_record: live_record.clone().or(carried),
                checks: checks.clone(),
            }
        });
    if let Some(pin) = &pin {
        qualified::store_pin(&file, pin).map_err(|why| {
            Report::new("io", format!("could not record the pin: {why}"))
                .next("check that the directory of your user configuration is writable")
        })?;
    }
    let failing: Vec<String> = checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    log_event(event(
        match passed {
            true => "qualified",
            false => "failed",
        },
        Some(match (&failing[..], &pin) {
            ([], Some(pin)) => format!("{}; pinned in {}", evidence_text(pin), file.display()),
            _ => failing.join("; "),
        }),
        passed,
    ));
    let done = Qualified {
        id,
        version: &version,
        profile: profile.id,
        package: &side.package,
        prefix: &prefix,
        command: &side.command,
        current: current.as_deref(),
        checks: &checks,
        pin: pin.as_ref(),
        file: &file,
    };
    done.conclude(as_json)
}

/// What a qualification found, ready to print.
struct Qualified<'a> {
    id: &'a str,
    version: &'a str,
    profile: &'a str,
    package: &'a str,
    prefix: &'a Path,
    command: &'a str,
    /// The version installed before, which qualifying leaves alone.
    current: Option<&'a str>,
    checks: &'a [Check],
    /// The pin written, when every check passed.
    pin: Option<&'a QualifiedPin>,
    file: &'a Path,
}

impl Qualified<'_> {
    fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// The result as `--json` prints it, and the error carries it.
    fn value(&self) -> Value {
        json!({
            "harness": self.id,
            "version": self.version,
            "profile": self.profile,
            "package": self.package,
            "prefix": self.prefix.display().to_string(),
            "command": self.command,
            "qualified": self.passed(),
            "evidence": self.pin.map(QualifiedPin::evidence),
            "checks": self.checks,
            "pin": self.pin,
            "pin_file": self.file.display().to_string(),
        })
    }

    /// A pass prints; a failure is a `command_failed` report naming every check.
    fn conclude(&self, as_json: bool) -> Outcome {
        let (id, version) = (self.id, self.version);
        if self.passed() {
            if as_json {
                return print(&json::text(&self.value()));
            }
            let mut text = format!(
                "qualified {id} {version} ({})\n",
                self.pin.map(evidence_text).unwrap_or_default()
            );
            text.push_str(&checks_text(self.checks));
            text.push_str(&format!(
                "pinned in {}; {id}{} is untouched.\nnext: by harnesses update {id} (installs {version})\n",
                self.file.display(),
                self.current.map(|c| format!(" {c}")).unwrap_or_default()
            ));
            return print(&text);
        }
        let mut report = Report::new(
            "command_failed",
            format!("{id} {version} did not qualify; nothing was pinned or changed"),
        );
        for check in self.checks {
            report = report.line(
                "",
                format!(
                    "{}  {:<9} {}",
                    if check.passed { "pass" } else { "FAIL" },
                    check.name,
                    check.detail
                ),
            );
        }
        Err(report
            .next(format!(
                "by harnesses update {id} --qualify --version VERSION (tries another release)"
            ))
            .next("by harnesses qualified (lists what is pinned)")
            .field("result", self.value())
            .into())
    }
}

/// The scenarios with a model: `branchyard-qualify` against the candidate,
/// writing `record`; the `live` check reads what it wrote.
fn live_run(
    id: &str,
    profile: &str,
    side: &qualified::SideInstall,
    bin: &Path,
    record: &Path,
    prefix: &Path,
) -> Check {
    let Some(runner) = runner() else {
        return Check::fail(
            "live",
            format!(
                "branchyard-qualify was not found (set {RUNNER_ENV}, or put it beside `by` or on PATH); \
                 build it with `cargo build -p branchyard-qualify`"
            ),
        );
    };
    let workdir = prefix.join(".qualify-live");
    if let Some(dir) = record.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Check::fail("live", format!("{}: {e}", dir.display()));
        }
    }
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        return Check::fail("live", format!("{}: {e}", workdir.display()));
    }
    let mut command = Command::new(&runner);
    command
        .args(["--profile", profile, "--command"])
        .arg(bin)
        .arg("--workdir")
        .arg(&workdir)
        .arg("--report")
        .arg(record)
        .args([
            "--installed_from",
            &format!("npm {}@{}", side.package, side.version),
        ]);
    for name in inventory::credential_variables(id) {
        if std::env::var_os(&name).is_some() {
            command.args(["--keep-env", &name]);
        }
    }
    eprintln!(
        "by: running the qualification scenarios of {profile} with a model: {}",
        runner.display()
    );
    let status = command.stdin(Stdio::null()).stdout(Stdio::null()).status();
    let text = branchyard_support::best_effort(
        "read the qualification record",
        std::fs::read_to_string(record),
    )
    .unwrap_or_default();
    match status {
        Err(e) => Check::fail("live", format!("could not run {}: {e}", runner.display())),
        Ok(_) if text.is_empty() => Check::fail(
            "live",
            format!(
                "{} wrote no record to {}",
                runner.display(),
                record.display()
            ),
        ),
        Ok(_) => qualified::live_check(&text, profile, &side.version, record),
    }
}

fn last_lines(output: &str) -> String {
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" / ")
}

fn evidence_text(pin: &QualifiedPin) -> String {
    match &pin.live_record {
        Some(record) => format!("live: recorded run {record}"),
        None => format!(
            "hermetic: {} passed, no model run",
            pin.checks
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn checks_text(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            format!(
                "  {}  {:<9} {}\n",
                if c.passed { "pass" } else { "FAIL" },
                c.name,
                c.detail
            )
        })
        .collect()
}

/// `by harnesses qualified`.
pub(super) fn list(env: &Env, as_json: bool) -> Outcome {
    list_in(env, &qualified::qualified_file(), as_json)
}

/// [`list`] for the pins in `file`.
fn list_in(env: &Env, file: &Path, as_json: bool) -> Outcome {
    let pins = qualified::read_pins(file).map_err(|why| {
        Report::new(
            "config",
            format!("could not read the qualified pins: {why}"),
        )
        .next("fix or remove the file, then run this again")
    })?;
    if as_json {
        let values: Vec<Value> = pins
            .iter()
            .map(|pin| {
                let mut value = serde_json::to_value(pin).unwrap_or(Value::Null);
                value["evidence"] = json!(pin.evidence());
                value
            })
            .collect();
        return print(&json::text(&Value::Array(values)));
    }
    if pins.is_empty() {
        return print(
            "no harness version has been qualified here; `by harnesses update ID --qualify` \
             installs a release beside the current one and checks it\n",
        );
    }
    let columns = [
        Column {
            header: "HARNESS",
            max: 16,
            right: false,
        },
        Column {
            header: "VERSION",
            max: 20,
            right: false,
        },
        Column {
            header: "QUALIFIED",
            max: 20,
            right: false,
        },
        Column {
            header: "EVIDENCE",
            max: 9,
            right: false,
        },
        Column {
            header: "CHECKS OR RECORD",
            max: 70,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = pins
        .iter()
        .map(|pin| {
            vec![
                Cell::plain(&pin.id),
                Cell::plain(&pin.version),
                Cell::plain(
                    pin.qualified_at
                        .get(..16)
                        .unwrap_or(&pin.qualified_at)
                        .replace('T', " "),
                ),
                Cell::toned(
                    pin.evidence(),
                    match pin.live_record {
                        Some(_) => Tone::Green,
                        None => Tone::Yellow,
                    },
                ),
                Cell::plain(match &pin.live_record {
                    Some(record) => record.clone(),
                    None => pin
                        .checks
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                }),
            ]
        })
        .collect();
    print(&format!(
        "{}{} pinned in {}\n",
        table(&columns, &rows, env.style()),
        pins.len(),
        file.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{env, remote, report_of, with_env};
    use branchyard_testkit::Scratch;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_release_number_is_one_safe_word() {
        for ok in ["0.170.0", "1.4.12", "0.2.1-rc.5", "2.0.0+build.7"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in [
            "",
            "latest",
            "1.0; rm -rf ~",
            "1.0 --global",
            "$(x)",
            "v1.0.0",
            "1.0\n2",
        ] {
            assert!(!valid_version(bad), "{bad:?}");
        }
    }

    fn change(id: &str, version: Option<&str>) -> HarnessChange {
        HarnessChange {
            id: id.into(),
            version: version.map(str::to_owned),
            yes: true,
        }
    }

    fn pin(id: &str, live: Option<&str>) -> QualifiedPin {
        QualifiedPin {
            id: id.into(),
            version: "0.170.0".into(),
            package: "@openai/codex".into(),
            profile: "codex-app-server".into(),
            integrity: None,
            qualified_at: "2026-11-05T10:00:00Z".into(),
            live_record: live.map(str::to_owned),
            checks: vec![
                Check::pass("version", "reports 0.170.0"),
                Check::pass("handshake", "initialized"),
            ],
        }
    }

    fn attempt(target: &Target, on: Option<&str>, change: &HarnessChange) -> Outcome {
        let harness = catalog::harness(&change.id).expect("a catalog harness");
        qualify(&env(), target, false, on, change, false, harness)
    }

    #[test]
    fn a_qualification_runs_on_this_machine_for_one_release_number() {
        let local = Target::Local;
        let remote = report_of(attempt(&remote(), None, &change("codex", None)).unwrap_err());
        assert_eq!(remote.kind, "usage");
        assert!(
            remote.message.contains("never install harnesses"),
            "{remote:?}"
        );
        assert!(remote.next[0].contains("by harnesses update codex --qualify"));

        let elsewhere = report_of(
            attempt(
                &local,
                Some("ssh://vm.test"),
                &change("codex", Some("1.0.0")),
            )
            .unwrap_err(),
        );
        assert!(
            elsewhere.message.contains("on this machine"),
            "{elsewhere:?}"
        );
        assert!(elsewhere.next[0].contains("drop --on"));
        let nowhere = attempt(&local, Some("elsewhere"), &change("codex", Some("1.0.0")));
        assert_eq!(report_of(nowhere.unwrap_err()).kind, "usage");

        for bad in ["1.0; touch pwned", "latest", ""] {
            let said = report_of(attempt(&local, None, &change("codex", Some(bad))).unwrap_err());
            assert!(
                said.message.contains("is not a release number"),
                "{bad}: {said:?}"
            );
        }
    }

    #[test]
    fn a_harness_that_cannot_be_installed_beside_the_current_one_is_refused_with_a_way_out() {
        let local = Target::Local;
        let mut refusals = std::collections::BTreeMap::new();
        for entry in catalog::harnesses() {
            if qualified::profile_of(&entry.id).is_none() {
                let said = attempt(&local, None, &change(&entry.id, Some("1.0.0")));
                let said = report_of(said.unwrap_err());
                assert_eq!(said.kind, "unsupported", "{}", entry.id);
                assert!(said.message.contains("no driver profile"), "{said:?}");
                refusals.insert("profile", entry.id.clone());
                continue;
            }
            let prefix = Scratch::new("qualify-unit");
            if let Err(refusal) = qualified::side_install(&entry.id, "1.0.0", prefix.path()) {
                let said = report_of(
                    attempt(&local, None, &change(&entry.id, Some("1.0.0"))).unwrap_err(),
                );
                assert_eq!(said.kind, refusal.kind, "{}", entry.id);
                assert!(said.message.contains("not qualifying"), "{said:?}");
                if refusal.kind == "unsupported" {
                    assert!(
                        said.next[0].contains("(updates in place, unqualified)"),
                        "{said:?}"
                    );
                    refusals.insert("piped", entry.id.clone());
                }
            }
        }
        // Claude Code installs by a piped script, so it cannot be put beside the current one.
        assert!(refusals.contains_key("piped"), "{refusals:?}");
    }

    #[test]
    fn the_candidate_goes_under_the_cache_and_the_record_under_the_user_directory() {
        let prefix = candidate_prefix("codex", "0.170.0").unwrap();
        assert!(
            prefix.ends_with("branchyard/candidates/codex/0.170.0"),
            "{prefix:?}"
        );
        let record = record_path("codex-app-server", "0.170.0");
        assert!(
            record.ends_with("qualification/codex-app-server-0.170.0.json"),
            "{record:?}"
        );
    }

    #[test]
    fn the_hook_files_are_checked_against_the_binary_the_release_installs() {
        let mut checked = 0;
        for entry in catalog::harnesses() {
            let Ok(Some(manifest)) = branchyard_setup::install::manifest(&entry.id) else {
                continue;
            };
            let right = Path::new("/prefix/node_modules/.bin").join(&manifest.binaries[0]);
            let passed = plugin_check(&entry.id, &right);
            assert!(passed.passed, "{}: {passed:?}", entry.id);
            assert!(
                passed.detail.contains("packaged file(s) parse"),
                "{passed:?}"
            );
            let wrong = plugin_check(&entry.id, Path::new("/prefix/node_modules/.bin/other-name"));
            assert!(!wrong.passed, "{}: {wrong:?}", entry.id);
            assert!(wrong
                .detail
                .contains("but the release installs `other-name`"));
            checked += 1;
        }
        assert!(checked > 0, "some harness packages hook files");
        // One with nothing packaged has nothing to check.
        let bare = catalog::harnesses()
            .iter()
            .find(|e| matches!(branchyard_setup::install::manifest(&e.id), Ok(None)));
        if let Some(bare) = bare {
            let passed = plugin_check(&bare.id, Path::new("/x/y"));
            assert!(
                passed.passed && passed.detail.contains("installs no hook"),
                "{passed:?}"
            );
        }
    }

    #[test]
    fn evidence_checks_and_output_read_as_a_few_plain_lines() {
        assert_eq!(last_lines(""), "");
        assert_eq!(last_lines("one\n\n  \ntwo\n"), "one / two");
        assert_eq!(last_lines("a\nb\nc\nd\ne\n"), "c / d / e");
        assert_eq!(
            evidence_text(&pin("codex", None)),
            "hermetic: version, handshake passed, no model run"
        );
        assert_eq!(
            evidence_text(&pin("codex", Some("/records/r.json"))),
            "live: recorded run /records/r.json"
        );
        let checks = vec![Check::pass("version", "ok"), Check::fail("flags", "no --x")];
        assert_eq!(
            checks_text(&checks),
            "  pass  version   ok\n  FAIL  flags     no --x\n"
        );
    }

    fn done<'a>(
        checks: &'a [Check],
        pin: Option<&'a QualifiedPin>,
        file: &'a Path,
        current: Option<&'a str>,
    ) -> Qualified<'a> {
        Qualified {
            id: "codex",
            version: "0.170.0",
            profile: "codex-app-server",
            package: "@openai/codex",
            prefix: Path::new("/cache/codex/0.170.0"),
            command: "npm install --prefix /cache/codex/0.170.0 @openai/codex@0.170.0",
            current,
            checks,
            pin,
            file,
        }
    }

    #[test]
    fn a_pass_prints_and_a_failure_names_every_check() {
        let file = Path::new("/config/qualified.toml");
        let pinned = pin("codex", None);
        let checks = pinned.checks.clone();
        let passed = done(&checks, Some(&pinned), file, Some("0.157.1"));
        assert!(passed.passed());
        let value = passed.value();
        assert_eq!(value["qualified"], true);
        assert_eq!(value["evidence"], "hermetic");
        assert_eq!(value["pin_file"], "/config/qualified.toml");
        assert_eq!(value["pin"]["version"], "0.170.0");
        passed.conclude(true).unwrap();
        passed.conclude(false).unwrap();
        done(&checks, Some(&pinned), file, None)
            .conclude(false)
            .unwrap();

        let failing = vec![
            Check::pass("install", "npm installed"),
            Check::fail("handshake", "no answer"),
        ];
        let failed = done(&failing, None, file, Some("0.157.1"));
        assert!(!failed.passed());
        assert_eq!(failed.value()["qualified"], false);
        assert_eq!(failed.value()["evidence"], Value::Null);
        for as_json in [true, false] {
            let report = report_of(failed.conclude(as_json).unwrap_err());
            assert_eq!(report.kind, "command_failed");
            assert!(report
                .message
                .contains("did not qualify; nothing was pinned"));
            assert_eq!(report.lines.len(), 2);
            assert!(report.lines[0].1.starts_with("pass  install"), "{report:?}");
            assert!(
                report.lines[1].1.starts_with("FAIL  handshake"),
                "{report:?}"
            );
            assert_eq!(report.next.len(), 2);
            assert_eq!(report.fields[0].0, "result");
            assert_eq!(report.fields[0].1["harness"], "codex");
        }
    }

    #[test]
    fn the_listing_is_a_table_json_or_the_way_to_start() {
        let dir = Scratch::new("qualified-unit");
        let file = dir.join("qualified.toml");
        list_in(&env(), &file, false).unwrap();
        list_in(&env(), &file, true).unwrap();

        qualified::store_pin(&file, &pin("codex", Some("/records/codex.json"))).unwrap();
        qualified::store_pin(&file, &pin("qwen-code", None)).unwrap();
        list_in(&env(), &file, false).unwrap();
        list_in(&env(), &file, true).unwrap();
        let pins = qualified::read_pins(&file).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].evidence(), "live");
        assert_eq!(pins[1].evidence(), "hermetic");

        std::fs::write(&file, "[[pin]\nnot toml").unwrap();
        let broken = report_of(list_in(&env(), &file, false).unwrap_err());
        assert_eq!(broken.kind, "config");
        assert!(broken.message.contains("could not read the qualified pins"));
    }

    #[test]
    fn the_scenarios_with_a_model_run_the_runner_and_read_the_record_it_wrote() {
        let dir = Scratch::new("qualify-live-unit");
        let side = qualified::side_install("qwen-code", "0.5.1", &dir.join("prefix")).unwrap();
        let bin = dir.join("qwen");
        let record = dir.join("records/qwen-code-acp-0.5.1.json");
        let live = |runner: &Path| {
            with_env(RUNNER_ENV, runner, || {
                live_run(
                    "qwen-code",
                    "qwen-code-acp",
                    &side,
                    &bin,
                    &record,
                    &dir.join("prefix"),
                )
            })
        };
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };

        // A runner that is not there.
        let missing = live(&dir.join("no-such-runner"));
        assert!(
            !missing.passed && missing.detail.contains("could not run"),
            "{missing:?}"
        );
        // A runner that writes no record.
        let silent = live(&script("silent", "exit 0\n"));
        assert!(
            !silent.passed && silent.detail.contains("wrote no record"),
            "{silent:?}"
        );
        // A record that is not for this release.
        let wrong = live(&script(
            "wrong",
            "while [ $# -gt 0 ]; do case $1 in --report) r=$2; shift 2;; *) shift;; esac; done\n\
             echo '{\"profile\":\"qwen-code-acp\",\"harness_version\":\"9.9.9\",\"scenarios\":[]}' > \"$r\"\n",
        ));
        assert!(
            !wrong.passed && wrong.detail.contains("harness_version"),
            "{wrong:?}"
        );
        // A record in which every scenario passed.
        let good = live(&script(
            "good",
            "while [ $# -gt 0 ]; do case $1 in --report) r=$2; shift 2;; *) shift;; esac; done\n\
             echo '{\"profile\":\"qwen-code-acp\",\"harness_version\":\"0.5.1\",\"date\":\"2026-11-05\",\"scenarios\":[{\"name\":\"fresh_turn\",\"passed\":true}]}' > \"$r\"\n",
        ));
        assert!(good.passed, "{good:?}");
        assert!(record.is_file());
    }

    #[test]
    fn the_runner_is_the_variable_then_beside_by_then_on_the_path() {
        let dir = Scratch::new("qualify-runner-unit");
        let named = dir.join("named-runner");
        assert_eq!(with_env(RUNNER_ENV, &named, runner), Some(named));
        // Blank means unset; whatever is found after that is a file or nothing.
        let found = with_env(RUNNER_ENV, Path::new(""), runner);
        assert!(found.is_none_or(|path| path.is_file()));
    }
}
