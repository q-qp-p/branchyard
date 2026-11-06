//! `by images`: toolchain images for the sandbox providers (build, list,
//! show, pin), and `by harnesses --image`, what an image holds from its
//! labels. The labels, the context and the records are the SDK's
//! (`branchyard::images`); this picks the repository, asks docker, renders
//! and edits the configuration. See docs/images.md.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use branchyard::images::{
    self, AnvilInstall, ContextPlan, DockerLabels, ImageLabelSource, ImageLabels, ImageRecord,
    ImageStore, LabelChain, RecordedLabels,
};
use branchyard::inventory;
use branchyard_controls::catalog;
use branchyard_setup::config::ProjectConfig;
use serde_json::{json, Value};

use crate::args::{ImagesAction, ImagesBuild};
use crate::commands::{print, Env, Failure, Outcome};
use crate::errors::{self, Report};
use crate::json;
use crate::render::{table, Cell, Column, Style};

/// The repository `by images` acts on: where its records and project file
/// are, and the configuration that applies.
struct Repo {
    root: PathBuf,
    project_file: PathBuf,
    config: ProjectConfig,
}

/// The repository `by images` acts on from the directory `cwd`.
fn repo_at(cwd: &Path) -> Result<Repo, Failure> {
    let located = branchyard_setup::config::locate(cwd);
    let root = located
        .project
        .parent()
        .map_or_else(|| cwd.to_path_buf(), Path::to_path_buf);
    let config = branchyard::configure::configuration(cwd)
        .map_err(|e| errors::config(&e.to_string()))?
        .map(|effective| effective.config)
        .unwrap_or_default();
    Ok(Repo {
        root,
        project_file: located.project,
        config,
    })
}

/// Where `by` reads an image's labels from, in order: what `by images`
/// recorded here, the local docker daemon, the image's registry. One route
/// asks about each image once.
pub fn label_source(root: &Path) -> Arc<dyn ImageLabelSource> {
    Arc::new(images::CachedLabels::new(Arc::new(LabelChain::new(vec![
        Arc::new(RecordedLabels::new(root)),
        Arc::new(DockerLabels),
        Arc::new(crate::registry_labels::RegistryLabels),
    ]))))
}

/// The label source the router consults for `options`, when its provider is a
/// sandbox that names an image; `None` otherwise.
pub fn route_source(
    root: &Path,
    options: &branchyard::TaskOptions,
) -> Option<Arc<dyn ImageLabelSource>> {
    options
        .provider
        .as_ref()
        .and_then(branchyard::Provider::image)
        .map(|_| label_source(root))
}

/// `by images ACTION`.
pub fn main(env: &Env, action: &ImagesAction, as_json: bool) -> Outcome {
    match action {
        ImagesAction::Build(_) => crate::harness_cmd::not_in_harness("build images with")?,
        ImagesAction::Pin { .. } => crate::harness_cmd::not_in_harness("pin images for")?,
        ImagesAction::List | ImagesAction::Show { .. } => {}
    }
    main_in(&std::env::current_dir()?, env, action, as_json)
}

/// [`main`] for the repository `cwd` is in.
fn main_in(cwd: &Path, env: &Env, action: &ImagesAction, as_json: bool) -> Outcome {
    match action {
        ImagesAction::Build(build) => build_image(cwd, build, as_json),
        ImagesAction::List => list(cwd, env, as_json),
        ImagesAction::Show { reference } => show(cwd, reference, as_json),
        ImagesAction::Pin {
            reference,
            provider,
        } => pin(cwd, reference, provider.as_deref(), as_json),
    }
}

// Build.

/// The harnesses an image is built with when none is named: the user's
/// `[harnesses] allow`, else those `[fleet]` and `[defaults]` name.
fn default_harnesses(config: &ProjectConfig) -> Vec<String> {
    let mut ids: Vec<String> = match config.harnesses.allow.is_empty() {
        false => config.harnesses.allow.clone(),
        true => {
            let mut named = Vec::new();
            if let Ok(fleet) = crate::defaults::fleet(config) {
                for entry in fleet.entries.values() {
                    named.extend(entry.candidates.iter().map(|c| c.harness.clone()));
                }
            }
            named.extend(config.defaults.harness.clone());
            named
        }
    };
    ids = ids
        .into_iter()
        .map(|id| inventory::harness_of(&id).map_or(id, str::to_owned))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn build_image(cwd: &Path, ask: &ImagesBuild, as_json: bool) -> Outcome {
    build_image_with(cwd, ask, as_json, images::docker_on_path().is_some())
}

/// [`build_image`], with `docker` saying whether to build the image or only
/// write its context and the command that builds it.
fn build_image_with(cwd: &Path, ask: &ImagesBuild, as_json: bool, docker: bool) -> Outcome {
    let repo = repo_at(cwd)?;
    let provider = ask
        .provider
        .as_deref()
        .and_then(images::provider_kind)
        .unwrap_or("container");
    let mut ids = ask.harness.clone();
    if ids.is_empty() {
        ids = default_harnesses(&repo.config);
    }
    if ids.is_empty() {
        return Err(Report::usage("no harness to put in the image")
            .line(
                "why",
                "no --harness was given, and neither [harnesses] allow, [fleet] nor [defaults] harness names one",
            )
            .next("by images build --harness codex --harness gemini-cli")
            .into());
    }
    ids.sort();
    ids.dedup();
    for id in &ids {
        if catalog::harness(id).is_none() {
            return Err(Report::new(
                "unknown_harness",
                format!("{id} is not a harness Branchyard knows"),
            )
            .next("by harnesses --all (lists them)")
            .into());
        }
    }
    // Every harness pinned, or refused together with why.
    let mut pins = Vec::new();
    let mut refused = Vec::new();
    for id in &ids {
        match images::pin_harness(id, ask.allow_unpinned) {
            Ok(pin) => pins.push(pin),
            Err(why) => refused.push(why),
        }
    }
    if !refused.is_empty() {
        let mut report = Report::new(
            "unsupported",
            format!("{} harness(es) cannot go in the image", refused.len()),
        );
        for why in refused {
            report = report.line("", why);
        }
        return Err(report
            .next("by images build --allow-unpinned ... (records them as unpinned)")
            .next("or leave them out with --harness")
            .into());
    }

    let by_binary = match &ask.by_binary {
        Some(path) => path.clone(),
        None if cfg!(target_os = "linux") => std::env::current_exe()?,
        None => {
            return Err(Report::usage(
                "this `by` is not a Linux binary, so it cannot go in a Linux image",
            )
            .next("by images build --by-binary PATH (a Linux `by`)")
            .into())
        }
    };
    if !by_binary.is_file() {
        return Err(
            Report::usage(format!("{} is not a file", by_binary.display()))
                .next("by images build --by-binary PATH")
                .into(),
        );
    }

    // The label set fixes the tag and the directory, so a rebuild of the
    // same toolchain writes the same context.
    let mut plan = ContextPlan {
        provider: provider.to_owned(),
        base: ask
            .base
            .clone()
            .unwrap_or_else(|| images::DEFAULT_BASE.to_owned()),
        harnesses: pins,
        by_version: env!("CARGO_PKG_VERSION").to_owned(),
        anvil: None,
    };
    let short = |plan: &ContextPlan| {
        let hash = plan.labels().toolchain();
        hash.trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect::<String>()
    };
    // Anvil is part of the toolchain when `[connectors] anvil` names it; its
    // digest is in the labels, so it is installed before the tag is chosen.
    let staging = repo.root.join(".branchyard/images/.staging");
    let mut anvil_note = None;
    let mut anvil_dir = None;
    if let Some(command) = &repo.config.connectors.anvil {
        let words = branchyard_setup::config::split_words(command)
            .map_err(|e| errors::config(&format!("connectors.anvil: {e}")))?;
        std::fs::create_dir_all(&staging)?;
        let prefix = staging.join("anvil");
        match images::prepare_anvil(&words, &prefix) {
            AnvilInstall::Installed(digest) => {
                plan.anvil = Some(digest);
                anvil_dir = Some(prefix);
            }
            AnvilInstall::Skipped(why) => {
                eprintln!("by: note: Anvil's toolchain is not in the image: {why}");
                anvil_note = Some(why);
            }
        }
    }
    let hash = short(&plan);
    let tag = ask
        .tag
        .clone()
        .unwrap_or_else(|| format!("branchyard-toolchain:{hash}"));
    let out = match &ask.out {
        Some(dir) => dir.clone(),
        None => repo
            .root
            .join(".branchyard/images")
            .join(format!("{provider}-{hash}")),
    };
    images::write_context(&out, &plan, &by_binary, anvil_dir.as_deref())
        .map_err(|why| Report::new("io", why))?;
    if staging.exists() {
        branchyard_support::best_effort(
            "remove the staged Anvil toolchain",
            std::fs::remove_dir_all(&staging),
        );
    }
    let labels = plan.labels();
    let command_line = [
        "docker",
        "buildx",
        "build",
        "--tag",
        &tag,
        if ask.push { "--push" } else { "--load" },
        &out.display().to_string(),
    ]
    .map(branchyard_recipe::quote)
    .join(" ");

    let mut built = None;
    if docker {
        eprintln!(
            "by: building {tag} from {} ({})",
            out.display(),
            labels.held()
        );
        let result = images::build(&tag, &out, ask.push)
            .map_err(|why| Report::new("command_failed", why))?;
        let reference = match &result.digest {
            Some(digest) => format!("{tag}@{digest}"),
            None => tag.clone(),
        };
        ImageStore::new(&repo.root)
            .add(ImageRecord {
                reference: reference.clone(),
                tag: tag.clone(),
                digest: result.digest.clone(),
                image_id: result.image_id.clone(),
                provider: provider.to_owned(),
                labels: labels.all(),
                created: branchyard_support::time::rfc3339(branchyard_support::time::now_ms()),
                pushed: ask.push,
            })
            .map_err(|why| Report::new("io", why))?;
        built = Some((reference, result));
    }

    let done = BuildDone {
        provider,
        tag: &tag,
        out: &out,
        plan: &plan,
        labels: &labels,
        anvil_note: anvil_note.as_deref(),
        built: built.as_ref(),
        push: ask.push,
        command_line: &command_line,
    };
    match as_json {
        true => print(&json::text(&done.json())),
        false => print(&done.text()),
    }
}

/// What `by images build` made, for `--json` or a sentence.
struct BuildDone<'a> {
    provider: &'a str,
    tag: &'a str,
    out: &'a Path,
    plan: &'a ContextPlan,
    labels: &'a ImageLabels,
    anvil_note: Option<&'a str>,
    /// The reference and what docker said, when docker built it.
    built: Option<&'a (String, images::Built)>,
    push: bool,
    /// The build to run where docker is.
    command_line: &'a str,
}

impl BuildDone<'_> {
    fn json(&self) -> Value {
        let (reference, digest) = match self.built {
            Some((reference, result)) => (Some(reference.clone()), result.digest.clone()),
            None => (None, None),
        };
        json!({
            "provider": self.provider,
            "tag": self.tag,
            "context": self.out.display().to_string(),
            "base": self.plan.base,
            "labels": self.labels.all(),
            "harnesses": self.plan.harnesses.iter().map(|h| json!({
                "id": h.id, "version": h.version, "evidence": h.evidence,
                "command": h.command, "unpinned": h.unpinned,
            })).collect::<Vec<_>>(),
            "anvil": match (&self.plan.anvil, self.anvil_note) {
                (Some(digest), _) => json!({"state": "installed", "digest": digest}),
                (None, Some(why)) => json!({"state": "skipped", "reason": why}),
                (None, None) => Value::Null,
            },
            "built": self.built.is_some(),
            "pushed": self.built.is_some() && self.push,
            "reference": reference,
            "digest": digest,
            "command": self.built.is_none().then_some(self.command_line),
        })
    }

    fn text(&self) -> String {
        let provider = self.provider;
        let mut text = format!(
            "context for a {provider} toolchain image: {}\n  toolchain {}\n",
            self.out.display(),
            self.labels.toolchain()
        );
        for h in &self.plan.harnesses {
            let how = match &h.unpinned {
                Some(why) => format!("unpinned ({why})"),
                None => format!("{} ({})", h.version, h.evidence),
            };
            text.push_str(&format!("  {} {how}\n", h.id));
        }
        text.push_str(&format!("  by {}\n", env!("CARGO_PKG_VERSION")));
        match (&self.plan.anvil, self.anvil_note) {
            (Some(digest), _) => {
                text.push_str(&format!("  anvil {digest} at {}\n", images::ANVIL_PREFIX))
            }
            (None, Some(why)) => text.push_str(&format!("  anvil skipped: {why}\n")),
            (None, None) => {}
        }
        match self.built {
            Some((reference, result)) => {
                text.push_str(&format!("built {reference}\n"));
                match &result.digest {
                    Some(_) => text.push_str(&format!(
                        "  next: by images pin {reference} --provider {}\n",
                        provider.replace('_', "-")
                    )),
                    None => text.push_str(
                        "  no registry digest yet: push it (by images build --push) before pinning; \
                         the container and agent-sandbox providers take only a digest\n",
                    ),
                }
            }
            None => text.push_str(&format!(
                "docker is not on PATH, so nothing was built. Build it where docker is:\n  {}\n",
                self.command_line
            )),
        }
        text
    }
}

// List and show.

fn records(repo: &Repo) -> Result<Vec<ImageRecord>, Failure> {
    ImageStore::new(&repo.root)
        .list()
        .map_err(|why| Report::new("state", why).into())
}

fn held(record: &ImageRecord) -> String {
    match record.toolchain() {
        Some(labels) => labels.held(),
        None => "no Branchyard labels".to_owned(),
    }
}

fn list(cwd: &Path, env: &Env, as_json: bool) -> Outcome {
    let repo = repo_at(cwd)?;
    let records = records(&repo)?;
    if as_json {
        return print(&json::text(&json!({ "images": records })));
    }
    print(&list_text(&records, env.style()))
}

/// The table of `records`, or what to run when there are none.
fn list_text(records: &[ImageRecord], style: Style) -> String {
    if records.is_empty() {
        return "no image built or pinned here. `by images build --harness ID` writes one.\n"
            .to_owned();
    }
    let columns = [
        Column {
            header: "REFERENCE",
            max: 72,
            right: false,
        },
        Column {
            header: "PROVIDER",
            max: 14,
            right: false,
        },
        Column {
            header: "HOLDS",
            max: 60,
            right: false,
        },
        Column {
            header: "PUSHED",
            max: 6,
            right: false,
        },
        Column {
            header: "CREATED",
            max: 24,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = records
        .iter()
        .map(|r| {
            vec![
                Cell::plain(&r.reference),
                Cell::plain(&r.provider),
                Cell::plain(held(r)),
                Cell::plain(if r.pushed { "yes" } else { "no" }),
                Cell::plain(&r.created),
            ]
        })
        .collect();
    table(&columns, &rows, style)
}

fn show(cwd: &Path, reference: &str, as_json: bool) -> Outcome {
    let repo = repo_at(cwd)?;
    let record = ImageStore::new(&repo.root)
        .find(reference)
        .map_err(|why| Report::new("state", why))?
        .ok_or_else(|| {
            Report::new(
                "not_found",
                format!("no image {reference} is recorded here"),
            )
            .next("by images list")
        })?;
    if as_json {
        return print(&json::text(&json!(record)));
    }
    print(&show_text(&record))
}

/// One record: where it is, and what it holds from its labels.
fn show_text(record: &ImageRecord) -> String {
    let mut text = format!(
        "{}\n  provider {}\n  tag {}\n  digest {}\n  pushed {}\n  created {}\n",
        record.reference,
        record.provider,
        record.tag,
        record.digest.as_deref().unwrap_or("none yet"),
        if record.pushed { "yes" } else { "no" },
        record.created
    );
    text.push_str(&labels_text(record.toolchain().as_ref(), &record.labels));
    text
}

fn labels_text(
    labels: Option<&ImageLabels>,
    raw: &std::collections::BTreeMap<String, String>,
) -> String {
    let Some(labels) = labels else {
        return "  no dev.branchyard labels: what it holds is unknown\n".to_owned();
    };
    let mut text = String::new();
    for (id, version) in &labels.harnesses {
        text.push_str(&format!("  holds {id} {version}\n"));
    }
    if let Some(by) = &labels.by {
        text.push_str(&format!("  by {by}\n"));
    }
    if let Some(anvil) = &labels.anvil {
        text.push_str(&format!("  anvil {anvil}\n"));
    }
    if let Some(toolchain) = raw.get(images::LABEL_TOOLCHAIN) {
        text.push_str(&format!("  toolchain {toolchain}\n"));
    }
    text
}

// Pin.

fn pin(cwd: &Path, reference: &str, provider: Option<&str>, as_json: bool) -> Outcome {
    let Some(digest) = images::digest_of(reference) else {
        return Err(Report::usage(format!(
            "{reference} is a tag, not a digest, and a tag moves"
        ))
        .line(
            "why",
            "a pinned image is named by REF@sha256:DIGEST, which no one can repoint",
        )
        .next("by images build --push (prints the REF@sha256:DIGEST it pushed)")
        .next("docker buildx imagetools inspect REF (shows a pushed image's digest)")
        .into());
    };
    branchyard_setup::config::validate_sandbox_image(reference)
        .map_err(|why| Report::usage(format!("{reference} {why}")))?;
    let repo = repo_at(cwd)?;
    let store = ImageStore::new(&repo.root);
    let known = store
        .find(reference)
        .map_err(|why| Report::new("state", why))?;
    let kind = provider
        .and_then(images::provider_kind)
        .map(str::to_owned)
        .or_else(|| known.as_ref().map(|r| r.provider.clone()))
        .or_else(|| {
            repo.config
                .defaults
                .provider
                .and_then(|p| serde_json::to_value(p).ok())
                .and_then(|p| {
                    p.as_str()
                        .and_then(images::provider_kind)
                        .map(str::to_owned)
                })
        })
        .ok_or_else(|| {
            Report::usage(format!("which provider is {reference} for?"))
                .next(format!("by images pin {reference} --provider container"))
        })?;

    // The labels, if they can be read, so a turn's manifest can record the toolchain.
    let mut note = None;
    let mut adopted = false;
    if known.is_none() {
        let source = LabelChain::new(vec![
            Arc::new(DockerLabels),
            Arc::new(crate::registry_labels::RegistryLabels),
        ]);
        match source.labels(reference) {
            Ok(Some(labels)) => {
                store
                    .add(ImageRecord {
                        reference: reference.to_owned(),
                        tag: reference
                            .rsplit_once('@')
                            .map_or(reference, |(t, _)| t)
                            .to_owned(),
                        digest: Some(digest.to_owned()),
                        image_id: None,
                        provider: kind.clone(),
                        labels,
                        created: branchyard_support::time::rfc3339(
                            branchyard_support::time::now_ms(),
                        ),
                        pushed: true,
                    })
                    .map_err(|why| Report::new("io", why))?;
                adopted = true;
            }
            Ok(None) => {
                note = Some(format!(
                    "could not read {reference}'s labels (no docker image, no registry answer); \
                     routing treats it as unlabelled until `by harnesses --image` can"
                ))
            }
            Err(why) => note = Some(format!("could not read {reference}'s labels: {why}")),
        }
    }

    let own_table = match kind.as_str() {
        "microsandbox" => repo.config.microsandbox.is_some(),
        "container" => repo.config.container.is_some(),
        "agent_sandbox" => repo.config.agent_sandbox.is_some(),
        _ => false,
    };
    let (key, shadowed) = write_pin(&repo.project_file, &kind, reference, own_table)?;
    if let Some(note) = &note {
        eprintln!("by: note: {note}");
    }
    if let Some(shadow) = &shadowed {
        eprintln!("by: note: {shadow}");
    }
    if as_json {
        return print(&json::text(&json!({
            "reference": reference,
            "provider": kind,
            "file": repo.project_file.display().to_string(),
            "key": key,
            "labels_recorded": adopted || known.is_some(),
            "note": note,
            "shadowed": shadowed,
        })));
    }
    print(&format!(
        "{key} = {reference}\n  in {}\n  `--provider {} ` without --image now uses it\n",
        repo.project_file.display(),
        kind.replace('_', "-")
    ))
}

/// Put `reference` where provider `kind` reads its image: its own
/// `[kind] image` where the project file already has one, else
/// `[images] kind`. The key written, and a note when another file's table
/// would still win.
fn write_pin(
    file: &Path,
    kind: &str,
    reference: &str,
    merged_table: bool,
) -> Result<(String, Option<String>), Failure> {
    // A project file that does not exist yet is an empty one; any other read error is the failure.
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(errors::config(&format!("{}: {e}", file.display()))
                .next("by config path")
                .into())
        }
    };
    let mut document: toml_edit::DocumentMut = text.parse().map_err(|error| {
        errors::config(&format!("{} does not parse: {error}", file.display()))
            .next("by config path")
    })?;
    let own = document
        .get(kind)
        .and_then(|table| table.get("image"))
        .is_some();
    let (table, key) = match own {
        true => (kind, "image"),
        false => ("images", kind),
    };
    if document.get(table).is_none() {
        // A header of its own (`[images]`), not an inline table.
        document[table] = toml_edit::table();
    }
    document[table][key] = toml_edit::value(reference);
    let updated = document.to_string();
    branchyard_setup::config::parse(&updated).map_err(|error| {
        errors::config(&format!(
            "{} would not load with the pin: {error}",
            file.display()
        ))
        .next("by config path")
    })?;
    crate::setup_io::write_file(file, &updated, 0o644)?;
    let shadow = (!own && merged_table).then(|| {
        format!(
            "another file's [{kind}] image still takes precedence over [images] {kind}; \
             change it there (by config path)"
        )
    });
    Ok((format!("[{table}] {key}"), shadow))
}

// `by harnesses --image`.

/// What the image `reference` holds, from its labels, without starting it.
pub fn holds(reference: &str, as_json: bool) -> Outcome {
    holds_in(&std::env::current_dir()?, reference, as_json)
}

/// [`holds`] for the repository `cwd` is in.
fn holds_in(cwd: &Path, reference: &str, as_json: bool) -> Outcome {
    let repo = repo_at(cwd)?;
    let source = label_source(&repo.root);
    let raw = match source.labels(reference) {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            return Err(Report::new(
                "not_found",
                format!("no label source holds {reference}"),
            )
            .line(
                "why",
                "it is not recorded here, not in the local docker daemon and its registry did not answer for it",
            )
            .next("docker pull REF, or by images pin REF@sha256:DIGEST (reads its labels)")
            .into())
        }
        Err(why) => return Err(Report::new("io", why).into()),
    };
    let labels = ImageLabels::from_map(&raw)
        .map_err(|why| Report::new("invalid_request", format!("{reference}: {why}")))?;
    if as_json {
        return print(&json::text(&match &labels {
            Some(labels) => json!({
                "image": reference,
                "labelled": true,
                "harnesses": labels.harnesses,
                "by": labels.by,
                "anvil": labels.anvil,
                "toolchain": raw.get(images::LABEL_TOOLCHAIN),
            }),
            None => json!({"image": reference, "labelled": false}),
        }));
    }
    let Some(labels) = labels else {
        return print(&format!(
            "{reference} carries no dev.branchyard labels: it was not built by `by images build`, \
             so what it holds is unknown. The router does not leave a harness out of it.\n"
        ));
    };
    let mut text = format!("{reference}\n");
    text.push_str(&labels_text(Some(&labels), &raw));
    print(&text)
}

#[cfg(test)]
mod tests {
    //! `by images` in this process, on a throwaway repository and a `docker`
    //! that is a shell script. The integration tests in `tests/images.rs` run
    //! the built `by`; these call the command code itself. No docker daemon,
    //! no registry beyond a loopback mock, no model.

    use super::*;
    use crate::testing::{env, report_of, with_path};
    use branchyard_testkit::{MockHttp, Response, Scratch};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    fn write_exec(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A repository directory, a directory of fake programs and a stand-in
    /// for the Linux `by` that goes into the image.
    struct Fixture {
        dir: Scratch,
        repo: PathBuf,
        bin: PathBuf,
        state: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = Scratch::new("images-unit");
            let (repo, bin, state) = (dir.join("repo"), dir.join("bin"), dir.join("docker-state"));
            for d in [&repo, &bin, &state] {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(dir.join("by-linux"), "#!/bin/sh\necho by\n").unwrap();
            Fixture {
                dir,
                repo,
                bin,
                state,
            }
        }

        /// `docker` in `bin`: logs its argv, builds anything (a push leaves
        /// a digest in the metadata file) and inspects only what
        /// `inspect.json` holds. `fail` in the state directory fails a build.
        fn docker(&self) {
            write_exec(
                &self.bin.join("docker"),
                &format!(
                    r#"#!/bin/sh
state='{}'
echo "$*" >> "$state/calls"
if [ "$1" = buildx ]; then
  if [ -f "$state/fail" ]; then
    echo "ERROR: failed to solve: the base image could not be pulled" >&2
    exit 1
  fi
  meta=''
  push=''
  while [ $# -gt 0 ]; do
    case "$1" in
      --metadata-file) meta="$2"; shift ;;
      --push) push=1 ;;
    esac
    shift
  done
  if [ -n "$push" ] && [ -n "$meta" ]; then
    echo '{{"containerimage.digest":"sha256:{}"}}' > "$meta"
  fi
  exit 0
fi
if [ "$1" = image ]; then
  if [ -f "$state/inspect.json" ]; then
    cat "$state/inspect.json"
    exit 0
  fi
  echo "Error response from daemon: No such image: $3" >&2
  exit 1
fi
exit 1
"#,
                    self.state.display(),
                    "f".repeat(64)
                ),
            );
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.state.join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn config(&self, body: &str) {
            std::fs::write(self.repo.join("branchyard.toml"), body).unwrap();
        }

        fn ask(&self, harnesses: &[&str]) -> ImagesBuild {
            ImagesBuild {
                provider: None,
                harness: harnesses.iter().map(|h| (*h).to_owned()).collect(),
                tag: Some("registry.test/team/tc:1".into()),
                push: false,
                out: Some(self.dir.join("context")),
                allow_unpinned: false,
                base: None,
                by_binary: Some(self.dir.join("by-linux")),
            }
        }

        fn records(&self) -> Vec<ImageRecord> {
            ImageStore::new(&self.repo).list().unwrap()
        }

        fn build(&self, ask: &ImagesBuild, docker: bool) -> Outcome {
            build_image_with(&self.repo, ask, true, docker)
        }

        /// A record of an image holding codex, as `by images build` writes.
        fn record(&self, reference: &str, provider: &str) -> ImageRecord {
            let plan = ContextPlan {
                provider: provider.into(),
                base: images::DEFAULT_BASE.into(),
                harnesses: vec![images::pin_harness("codex", false).unwrap()],
                by_version: "0.4.0".into(),
                anvil: None,
            };
            let record = ImageRecord {
                reference: reference.into(),
                tag: reference
                    .rsplit_once('@')
                    .map_or(reference, |(t, _)| t)
                    .into(),
                digest: images::digest_of(reference).map(str::to_owned),
                image_id: None,
                provider: provider.into(),
                labels: plan.labels().all(),
                created: "2026-01-02T03:04:05Z".into(),
                pushed: true,
            };
            ImageStore::new(&self.repo).add(record.clone()).unwrap();
            record
        }
    }

    fn actions(fixture: &Fixture) -> [ImagesAction; 4] {
        [
            ImagesAction::Build(fixture.ask(&["codex"])),
            ImagesAction::List,
            ImagesAction::Show {
                reference: "nothing".into(),
            },
            ImagesAction::Pin {
                reference: "registry.test/tc:1".into(),
                provider: None,
            },
        ]
    }

    #[test]
    fn each_action_goes_to_its_command() {
        let f = Fixture::new();
        f.docker();
        let [build, list, show, pin] = actions(&f);
        with_path(&f.bin, || {
            main_in(&f.repo, &env(), &build, true).unwrap();
            main_in(&f.repo, &env(), &list, false).unwrap();
            let shown = report_of(main_in(&f.repo, &env(), &show, false).unwrap_err());
            assert_eq!(shown.kind, "not_found");
            let pinned = report_of(main_in(&f.repo, &env(), &pin, false).unwrap_err());
            assert_eq!(pinned.kind, "usage");
        });
        // Docker was on PATH, so the build ran and was recorded.
        assert_eq!(f.records().len(), 1);
        assert!(f.calls()[0].starts_with("buildx build --tag registry.test/team/tc:1"));
    }

    #[test]
    fn without_docker_a_build_writes_the_context_and_the_command_that_builds_it() {
        let f = Fixture::new();
        let mut ask = f.ask(&["pi", "codex"]);
        f.build(&ask, false).unwrap();
        let out = f.dir.join("context");
        let dockerfile = std::fs::read_to_string(out.join("Dockerfile")).unwrap();
        assert!(
            dockerfile.contains("dev.branchyard.harness.codex"),
            "{dockerfile}"
        );
        assert!(
            dockerfile.contains("dev.branchyard.harness.pi"),
            "{dockerfile}"
        );
        assert!(out.join("by").is_file());
        // Nothing was built, so nothing is recorded.
        assert!(f.records().is_empty());
        // Text output takes the same road.
        ask.provider = Some("agent-sandbox".into());
        ask.out = None;
        ask.tag = None;
        build_image_with(&f.repo, &ask, false, false).unwrap();
        let contexts: Vec<_> = std::fs::read_dir(f.repo.join(".branchyard/images"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(contexts.len(), 1, "{contexts:?}");
        assert!(contexts[0].starts_with("agent_sandbox-"), "{contexts:?}");
    }

    #[test]
    fn a_build_through_docker_records_the_image_it_made() {
        let f = Fixture::new();
        f.docker();
        let mut ask = f.ask(&["codex"]);
        with_path(&f.bin, || f.build(&ask, true)).unwrap();
        // Loaded, never pushed, and docker knows no digest for it.
        let records = f.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reference, "registry.test/team/tc:1");
        assert_eq!(records[0].digest, None);
        assert!(!records[0].pushed);
        assert!(records[0].toolchain().is_some());
        assert!(f.calls()[0].contains("--load"), "{:?}", f.calls());

        // Pushed, the digest comes back through the metadata file.
        ask.push = true;
        with_path(&f.bin, || f.build(&ask, true)).unwrap();
        let reference = format!("registry.test/team/tc:1@sha256:{}", "f".repeat(64));
        let pushed = ImageStore::new(&f.repo).find(&reference).unwrap().unwrap();
        assert!(pushed.pushed);
        assert_eq!(pushed.digest, Some(digest('f')));
        assert!(
            f.calls().iter().any(|c| c.contains("--push")),
            "{:?}",
            f.calls()
        );
        // The metadata file is the build's, not the context's.
        assert!(!f.dir.join("context/.metadata.json").exists());
    }

    #[test]
    fn a_loaded_image_reports_the_digest_and_id_docker_holds() {
        let f = Fixture::new();
        f.docker();
        let id = digest('1');
        std::fs::write(
            f.state.join("inspect.json"),
            serde_json::json!([{
                "Id": id,
                "RepoDigests": [format!("registry.test/team/tc@{}", digest('2'))],
                "Config": {"Labels": {"a": "b"}},
            }])
            .to_string(),
        )
        .unwrap();
        let ask = f.ask(&["codex"]);
        with_path(&f.bin, || f.build(&ask, true)).unwrap();
        let record = &f.records()[0];
        assert_eq!(record.digest, Some(digest('2')));
        assert_eq!(record.image_id, Some(id));
        assert_eq!(
            record.reference,
            format!("registry.test/team/tc:1@{}", digest('2'))
        );
    }

    #[test]
    fn a_failed_build_is_a_command_failure_with_docker_s_last_words() {
        let f = Fixture::new();
        f.docker();
        std::fs::write(f.state.join("fail"), "").unwrap();
        let ask = f.ask(&["codex"]);
        let failure = with_path(&f.bin, || f.build(&ask, true)).unwrap_err();
        let report = report_of(failure);
        assert_eq!(report.kind, "command_failed");
        assert!(report.message.contains("failed to solve"), "{report:?}");
        assert!(f.records().is_empty());
    }

    #[test]
    fn a_build_that_names_no_harness_or_a_wrong_one_is_refused() {
        let f = Fixture::new();
        let none = report_of(f.build(&f.ask(&[]), false).unwrap_err());
        assert_eq!(none.kind, "usage");
        assert!(none.message.contains("no harness to put in the image"));
        assert!(none.next[0].contains("by images build --harness"));

        let unknown = report_of(f.build(&f.ask(&["codex", "nope"]), false).unwrap_err());
        assert_eq!(unknown.kind, "unknown_harness");
        assert!(unknown.message.contains("nope"));

        let mut ask = f.ask(&["codex"]);
        ask.by_binary = Some(f.dir.join("missing"));
        let missing = report_of(f.build(&ask, false).unwrap_err());
        assert_eq!(missing.kind, "usage");
        assert!(missing.message.contains("is not a file"), "{missing:?}");
    }

    #[test]
    fn a_piped_installer_is_refused_unless_it_is_allowed_and_then_labelled_unpinned() {
        let f = Fixture::new();
        let mut ask = f.ask(&["claude-code", "codex"]);
        let refused = report_of(f.build(&ask, false).unwrap_err());
        assert_eq!(refused.kind, "unsupported");
        assert!(refused.message.contains("1 harness(es)"), "{refused:?}");
        assert!(refused.next.iter().any(|n| n.contains("--allow-unpinned")));
        assert!(!f.dir.join("context").exists());

        ask.allow_unpinned = true;
        f.build(&ask, false).unwrap();
        let dockerfile = std::fs::read_to_string(f.dir.join("context/Dockerfile")).unwrap();
        assert!(
            dockerfile.contains("dev.branchyard.harness.claude-code=\"unpinned\""),
            "{dockerfile}"
        );
    }

    #[test]
    fn the_harnesses_default_to_the_allow_list_then_the_fleet_and_defaults() {
        let parse = |text: &str| branchyard_setup::config::parse(text).unwrap();
        assert!(default_harnesses(&ProjectConfig::default()).is_empty());
        let allowed = parse("[harnesses]\nallow = [\"pi\", \"codex\", \"pi\"]\n");
        assert_eq!(default_harnesses(&allowed), ["codex", "pi"]);
        let defaults = parse("[defaults]\nharness = \"codex\"\n");
        assert_eq!(default_harnesses(&defaults), ["codex"]);
        // A profile ID is its harness.
        let profile = parse("[defaults]\nharness = \"codex-app-server\"\n");
        assert_eq!(default_harnesses(&profile), ["codex"]);

        // And a build with none named takes them from the project file.
        let f = Fixture::new();
        f.config("[defaults]\nharness = \"codex\"\n");
        f.build(&f.ask(&[]), false).unwrap();
        let dockerfile = std::fs::read_to_string(f.dir.join("context/Dockerfile")).unwrap();
        assert!(dockerfile.contains("dev.branchyard.harness.codex"));
    }

    #[test]
    fn anvil_goes_into_the_image_when_it_installs_and_is_noted_when_it_does_not() {
        let f = Fixture::new();
        let anvil = f.bin.join("anvil");
        write_exec(
            &anvil,
            "#!/bin/sh\n[ \"$2\" = \"--help\" ] && exit 0\nmkdir -p \"$3/v1\" && ln -sfn \"$3/v1\" \"$3/current\"\n",
        );
        f.config(&format!("[connectors]\nanvil = \"{}\"\n", anvil.display()));
        f.build(&f.ask(&["codex"]), false).unwrap();
        let dockerfile = std::fs::read_to_string(f.dir.join("context/Dockerfile")).unwrap();
        assert!(dockerfile.contains("COPY anvil /opt/anvil"), "{dockerfile}");
        assert!(
            dockerfile.contains("dev.branchyard.anvil=\"v1\""),
            "{dockerfile}"
        );
        assert!(!f.repo.join(".branchyard/images/.staging").exists());

        // An Anvil without `install`: the image is built without it.
        write_exec(&anvil, "#!/bin/sh\nexit 1\n");
        let mut ask = f.ask(&["codex"]);
        ask.out = Some(f.dir.join("bare"));
        f.build(&ask, false).unwrap();
        let bare = std::fs::read_to_string(f.dir.join("bare/Dockerfile")).unwrap();
        assert!(!bare.contains("/opt/anvil"), "{bare}");

        // A command line that does not split is a configuration error.
        f.config("[connectors]\nanvil = \"'unclosed\"\n");
        let bad = report_of(f.build(&f.ask(&["codex"]), false).unwrap_err());
        assert_eq!(bad.kind, "config");
    }

    fn done<'a>(
        plan: &'a ContextPlan,
        labels: &'a ImageLabels,
        anvil_note: Option<&'a str>,
        built: Option<&'a (String, images::Built)>,
        out: &'a Path,
    ) -> BuildDone<'a> {
        BuildDone {
            provider: "agent_sandbox",
            tag: "registry.test/tc:1",
            out,
            plan,
            labels,
            anvil_note,
            built,
            push: true,
            command_line: "docker buildx build --tag t --push /ctx",
        }
    }

    #[test]
    fn the_build_report_says_what_was_built_and_what_to_run_next() {
        let plan = ContextPlan {
            provider: "agent_sandbox".into(),
            base: images::DEFAULT_BASE.into(),
            harnesses: vec![
                images::pin_harness("codex", false).unwrap(),
                images::pin_harness("claude-code", true).unwrap(),
            ],
            by_version: "0.4.0".into(),
            anvil: None,
        };
        let labels = plan.labels();
        let out = Path::new("/ctx");

        // Docker absent: the command to run where it is.
        let unbuilt = done(&plan, &labels, None, None, out);
        let text = unbuilt.text();
        assert!(text.starts_with("context for a agent_sandbox toolchain image: /ctx\n"));
        assert!(text.contains("  claude-code unpinned ("), "{text}");
        assert!(text.contains("  codex "), "{text}");
        assert!(text.contains("docker is not on PATH"), "{text}");
        assert!(text.ends_with("  docker buildx build --tag t --push /ctx\n"));
        let value = unbuilt.json();
        assert_eq!(value["built"], false);
        assert_eq!(value["pushed"], false);
        assert_eq!(value["anvil"], Value::Null);
        assert_eq!(value["command"], "docker buildx build --tag t --push /ctx");
        assert_eq!(value["harnesses"][0]["id"], "codex");
        assert!(value["harnesses"][1]["unpinned"].is_string());

        // Built and pushed: the digest, and the pin command with a dash.
        let pushed = (
            format!("registry.test/tc:1@{}", digest('a')),
            images::Built {
                digest: Some(digest('a')),
                image_id: None,
            },
        );
        let built = done(&plan, &labels, Some("no anvil here"), Some(&pushed), out);
        let text = built.text();
        assert!(text.contains(&format!("built {}\n", pushed.0)), "{text}");
        assert!(text.contains("by images pin"), "{text}");
        assert!(text.contains("--provider agent-sandbox"), "{text}");
        assert!(text.contains("anvil skipped: no anvil here"), "{text}");
        let value = built.json();
        assert_eq!(value["built"], true);
        assert_eq!(value["pushed"], true);
        assert_eq!(value["digest"], digest('a'));
        assert_eq!(value["command"], Value::Null);
        assert_eq!(value["anvil"]["state"], "skipped");

        // Built but loaded only: no digest to pin yet.
        let loaded = (
            "registry.test/tc:1".to_owned(),
            images::Built {
                digest: None,
                image_id: Some(digest('b')),
            },
        );
        let text = done(&plan, &labels, None, Some(&loaded), out).text();
        assert!(text.contains("no registry digest yet"), "{text}");

        // Anvil installed.
        let mut with_anvil = plan.clone();
        with_anvil.anvil = Some("v9".into());
        let labels = with_anvil.labels();
        let report = done(&with_anvil, &labels, None, None, out);
        assert!(report.text().contains("anvil v9 at /opt/anvil"));
        assert_eq!(report.json()["anvil"]["digest"], "v9");
    }

    #[test]
    fn the_list_is_a_table_of_records_or_the_way_to_make_one() {
        let f = Fixture::new();
        let style = Style { color: false };
        assert!(list_text(&[], style).starts_with("no image built or pinned here."));
        list(&f.repo, &env(), false).unwrap();
        list(&f.repo, &env(), true).unwrap();

        let reference = format!("registry.test/team/tc:1@{}", digest('d'));
        let record = f.record(&reference, "container");
        let mut bare = f.record("registry.test/other:2", "microsandbox");
        bare.labels.clear();
        bare.pushed = false;
        let text = list_text(&[record, bare], style);
        for needle in [
            "REFERENCE",
            "PROVIDER",
            "HOLDS",
            "PUSHED",
            "CREATED",
            &reference[..60],
            "codex ",
            "no Branchyard labels",
            "2026-01-02T03:04:05Z",
        ] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        list(&f.repo, &env(), false).unwrap();
        list(&f.repo, &env(), true).unwrap();
    }

    #[test]
    fn a_record_file_that_does_not_parse_is_a_state_error() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.repo.join(".branchyard")).unwrap();
        std::fs::write(f.repo.join(".branchyard/images.json"), "{not json").unwrap();
        for failure in [
            list(&f.repo, &env(), false).unwrap_err(),
            show(&f.repo, "x", false).unwrap_err(),
        ] {
            assert_eq!(report_of(failure).kind, "state");
        }
    }

    #[test]
    fn show_finds_a_record_by_reference_tag_or_digest_and_prints_what_it_holds() {
        let f = Fixture::new();
        let reference = format!("registry.test/team/tc:1@{}", digest('d'));
        let record = f.record(&reference, "container");
        for query in [reference.as_str(), "registry.test/team/tc:1"] {
            show(&f.repo, query, false).unwrap();
            show(&f.repo, query, true).unwrap();
        }
        show(&f.repo, &digest('d'), false).unwrap();
        let text = show_text(&record);
        assert!(text.starts_with(&format!("{reference}\n  provider container\n")));
        assert!(text.contains("  holds codex "), "{text}");
        assert!(text.contains("  by 0.4.0\n"), "{text}");
        assert!(text.contains("  toolchain sha256:"), "{text}");
        assert!(text.contains("  pushed yes\n"));

        let mut plain = record.clone();
        plain.digest = None;
        plain.pushed = false;
        plain.labels.clear();
        let text = show_text(&plain);
        assert!(text.contains("  digest none yet\n"), "{text}");
        assert!(text.contains("  pushed no\n"), "{text}");
        assert!(text.ends_with("  no dev.branchyard labels: what it holds is unknown\n"));

        let missing = report_of(show(&f.repo, "nope", false).unwrap_err());
        assert_eq!(missing.kind, "not_found");
        assert_eq!(missing.next, ["by images list"]);
    }

    #[test]
    fn labels_text_lists_harnesses_by_anvil_and_the_toolchain_hash() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "dev.branchyard.harness.codex".to_owned(),
            "0.1.0".to_owned(),
        );
        raw.insert("dev.branchyard.anvil".to_owned(), "v3".to_owned());
        let labels = ImageLabels::from_map(&raw).unwrap().unwrap();
        raw.insert(images::LABEL_TOOLCHAIN.to_owned(), "sha256:abc".to_owned());
        let text = labels_text(Some(&labels), &raw);
        assert_eq!(
            text,
            "  holds codex 0.1.0\n  anvil v3\n  toolchain sha256:abc\n"
        );
        assert!(labels_text(None, &raw).contains("what it holds is unknown"));
    }

    #[test]
    fn a_pin_refuses_a_tag_and_a_short_digest_before_touching_anything() {
        let f = Fixture::new();
        let tag =
            report_of(pin(&f.repo, "registry.test/tc:1", Some("container"), false).unwrap_err());
        assert_eq!(tag.kind, "usage");
        assert!(tag.message.contains("is a tag, not a digest"));
        assert_eq!(tag.next.len(), 2);
        let short = report_of(
            pin(
                &f.repo,
                "registry.test/tc@sha256:abc",
                Some("container"),
                false,
            )
            .unwrap_err(),
        );
        assert_eq!(short.kind, "usage");
        assert!(!f.repo.join("branchyard.toml").exists());
    }

    #[test]
    fn a_pin_writes_the_key_the_provider_reads() {
        let f = Fixture::new();
        f.docker();
        let first = format!("registry.test/team/tc:1@{}", digest('d'));
        // No docker image and no registry that answers: the pin is written, and says why not more.
        let mock = MockHttp::start(|_| Response::new(404, ""));
        let host = mock.url().trim_start_matches("http://").to_owned();
        let unknown = format!("{host}/team/none@{}", digest('9'));
        with_path(&f.bin, || {
            pin(&f.repo, &first, Some("container"), true).unwrap();
            pin(&f.repo, &unknown, Some("agent-sandbox"), true).unwrap();
        });
        mock.finish();
        let toml = std::fs::read_to_string(f.repo.join("branchyard.toml")).unwrap();
        assert!(toml.contains("[images]"), "{toml}");
        assert!(toml.contains(&format!("container = \"{first}\"")), "{toml}");
        assert!(
            toml.contains(&format!("agent_sandbox = \"{unknown}\"")),
            "{toml}"
        );
        branchyard_setup::config::parse(&toml).unwrap();

        // The provider's own table wins where the file has one.
        let second = format!("registry.test/team/tc:2@{}", digest('e'));
        f.config(&format!(
            "[container]\nimage = \"old@{}\"\ncpus = 2\n",
            digest('a')
        ));
        with_path(&f.bin, || pin(&f.repo, &second, Some("container"), false)).unwrap();
        let toml = std::fs::read_to_string(f.repo.join("branchyard.toml")).unwrap();
        assert!(
            toml.contains(&second) && toml.contains("cpus = 2"),
            "{toml}"
        );
        assert!(!toml.contains("[images]"), "{toml}");

        // Nothing says which provider.
        let lost = with_path(&f.bin, || pin(&f.repo, &second, None, false)).unwrap_err();
        let lost = report_of(lost);
        assert_eq!(lost.kind, "usage");
        assert!(lost.message.contains("which provider"), "{lost:?}");
    }

    #[test]
    fn a_pin_keeps_a_recorded_image_and_adopts_one_only_docker_knows() {
        let f = Fixture::new();
        f.docker();
        let recorded = format!("registry.test/team/tc:1@{}", digest('d'));
        f.record(&recorded, "agent_sandbox");
        // The recorded provider is the default; its record stays.
        with_path(&f.bin, || pin(&f.repo, &recorded, None, true)).unwrap();
        assert_eq!(f.records().len(), 1);
        let toml = std::fs::read_to_string(f.repo.join("branchyard.toml")).unwrap();
        assert!(toml.contains("agent_sandbox = "), "{toml}");

        // Built elsewhere, held by the daemon: its labels are recorded.
        let theirs = format!("elsewhere.test/team/tc:9@{}", digest('7'));
        let mut raw = BTreeMap::new();
        raw.insert(
            "dev.branchyard.harness.codex".to_owned(),
            "0.157.1".to_owned(),
        );
        raw.insert("dev.branchyard.by".to_owned(), "0.4.0".to_owned());
        let toolchain = ImageLabels::from_map(&raw).unwrap().unwrap().toolchain();
        raw.insert(images::LABEL_TOOLCHAIN.to_owned(), toolchain.clone());
        std::fs::write(
            f.state.join("inspect.json"),
            serde_json::json!([{"Id": digest('5'), "Config": {"Labels": raw}}]).to_string(),
        )
        .unwrap();
        with_path(&f.bin, || pin(&f.repo, &theirs, Some("container"), false)).unwrap();
        let adopted = ImageStore::new(&f.repo).find(&theirs).unwrap().unwrap();
        assert_eq!(adopted.provider, "container");
        assert_eq!(adopted.digest, Some(digest('7')));
        assert_eq!(adopted.labels[images::LABEL_TOOLCHAIN], toolchain);
        assert!(adopted.pushed);
    }

    #[test]
    fn a_pin_reports_a_registry_that_errors() {
        let f = Fixture::new();
        f.docker();
        let mock = MockHttp::start(|_| Response::new(500, "boom"));
        let host = mock.url().trim_start_matches("http://").to_owned();
        let reference = format!("{host}/team/tc@{}", digest('4'));
        with_path(&f.bin, || pin(&f.repo, &reference, Some("container"), true)).unwrap();
        mock.finish();
        assert!(f.records().is_empty());
        assert!(f.repo.join("branchyard.toml").is_file());
    }

    #[test]
    fn the_pin_file_is_edited_in_place_and_refused_when_it_cannot_be() {
        let f = Fixture::new();
        let file = f.repo.join("branchyard.toml");
        let reference = format!("registry.test/tc:1@{}", digest('c'));
        // A file that is not there yet is an empty one.
        let (key, shadow) = write_pin(&file, "microsandbox", &reference, false).unwrap();
        assert_eq!(key, "[images] microsandbox");
        assert_eq!(shadow, None);
        // Another file's table still wins over `[images]`: say so.
        let (key, shadow) = write_pin(&file, "container", &reference, true).unwrap();
        assert_eq!(key, "[images] container");
        assert!(shadow.unwrap().contains("another file's [container] image"));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("microsandbox") && text.contains("container"),
            "{text}"
        );

        std::fs::write(&file, "= not toml").unwrap();
        let broken = write_pin(&file, "container", &reference, false).unwrap_err();
        assert_eq!(report_of(broken).kind, "config");
        // A directory where the file should be is not a missing file.
        let dir = f.dir.join("a-directory");
        std::fs::create_dir(&dir).unwrap();
        let unreadable = write_pin(&dir, "container", &reference, false).unwrap_err();
        assert_eq!(report_of(unreadable).kind, "config");
    }

    #[test]
    fn what_an_image_holds_comes_from_the_records_the_daemon_or_the_registry() {
        let f = Fixture::new();
        f.docker();
        let reference = format!("registry.test/team/tc:1@{}", digest('d'));
        f.record(&reference, "container");
        holds_in(&f.repo, &reference, false).unwrap();
        holds_in(&f.repo, &reference, true).unwrap();

        // Recorded without Branchyard's labels: said so, in both forms.
        let mut plain = f.record("registry.test/plain:1", "container");
        plain.labels.clear();
        ImageStore::new(&f.repo).add(plain).unwrap();
        holds_in(&f.repo, "registry.test/plain:1", false).unwrap();
        holds_in(&f.repo, "registry.test/plain:1", true).unwrap();

        // Labels that contradict themselves are an invalid request.
        let mut odd = f.record("registry.test/odd:1", "container");
        odd.labels
            .insert(images::LABEL_TOOLCHAIN.into(), "sha256:wrong".into());
        ImageStore::new(&f.repo).add(odd).unwrap();
        let invalid = with_path(&f.bin, || holds_in(&f.repo, "registry.test/odd:1", false));
        assert_eq!(report_of(invalid.unwrap_err()).kind, "invalid_request");

        // No source holds it.
        let mock = MockHttp::start(|_| Response::new(404, ""));
        let host = mock.url().trim_start_matches("http://").to_owned();
        let missing = with_path(&f.bin, || {
            holds_in(&f.repo, &format!("{host}/team/none:1"), false)
        });
        mock.finish();
        let missing = report_of(missing.unwrap_err());
        assert_eq!(missing.kind, "not_found");
        assert!(missing.message.contains("no label source holds"));
    }

    #[test]
    fn the_router_asks_for_labels_only_for_a_sandbox_that_names_an_image() {
        let f = Fixture::new();
        let options = branchyard::TaskOptions::default();
        assert!(route_source(&f.repo, &options).is_none());
        let reference = format!("registry.test/team/tc:1@{}", digest('d'));
        let record = f.record(&reference, "container");
        let sandboxed = branchyard::TaskOptions {
            // From its JSON form: only crates/branchyard/src/providers names
            // the variants (tests/provider_seam.rs).
            provider: Some(
                serde_json::from_value(
                    serde_json::json!({"kind": "container", "image": reference}),
                )
                .unwrap(),
            ),
            ..branchyard::TaskOptions::default()
        };
        let source = route_source(&f.repo, &sandboxed).expect("a source for a sandbox");
        assert_eq!(source.labels(&reference).unwrap(), Some(record.labels));
    }
}
