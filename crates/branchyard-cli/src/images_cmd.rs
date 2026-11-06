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
use crate::render::{table, Cell, Column};

/// The repository `by images` acts on: where its records and project file
/// are, and the configuration that applies.
struct Repo {
    root: PathBuf,
    project_file: PathBuf,
    config: ProjectConfig,
}

fn repo() -> Result<Repo, Failure> {
    let cwd = std::env::current_dir()?;
    let located = branchyard_setup::config::locate(&cwd);
    let root = located
        .project
        .parent()
        .map_or_else(|| cwd.clone(), Path::to_path_buf);
    let config = branchyard::configure::configuration(&cwd)
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
        ImagesAction::Build(build) => {
            crate::harness_cmd::not_in_harness("build images with")?;
            build_image(build, as_json)
        }
        ImagesAction::List => list(env, as_json),
        ImagesAction::Show { reference } => show(reference, as_json),
        ImagesAction::Pin {
            reference,
            provider,
        } => {
            crate::harness_cmd::not_in_harness("pin images for")?;
            pin(reference, provider.as_deref(), as_json)
        }
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

fn build_image(ask: &ImagesBuild, as_json: bool) -> Outcome {
    let repo = repo()?;
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
    if images::docker_on_path().is_some() {
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

    if as_json {
        let (reference, digest) = match &built {
            Some((reference, result)) => (Some(reference.clone()), result.digest.clone()),
            None => (None, None),
        };
        return print(&json::text(&json!({
            "provider": provider,
            "tag": tag,
            "context": out.display().to_string(),
            "base": plan.base,
            "labels": labels.all(),
            "harnesses": plan.harnesses.iter().map(|h| json!({
                "id": h.id, "version": h.version, "evidence": h.evidence,
                "command": h.command, "unpinned": h.unpinned,
            })).collect::<Vec<_>>(),
            "anvil": match (&plan.anvil, &anvil_note) {
                (Some(digest), _) => json!({"state": "installed", "digest": digest}),
                (None, Some(why)) => json!({"state": "skipped", "reason": why}),
                (None, None) => Value::Null,
            },
            "built": built.is_some(),
            "pushed": built.is_some() && ask.push,
            "reference": reference,
            "digest": digest,
            "command": (built.is_none()).then_some(command_line.clone()),
        })));
    }
    let mut text = format!(
        "context for a {provider} toolchain image: {}\n  toolchain {}\n",
        out.display(),
        labels.toolchain()
    );
    for h in &plan.harnesses {
        let how = match &h.unpinned {
            Some(why) => format!("unpinned ({why})"),
            None => format!("{} ({})", h.version, h.evidence),
        };
        text.push_str(&format!("  {} {how}\n", h.id));
    }
    text.push_str(&format!("  by {}\n", env!("CARGO_PKG_VERSION")));
    match (&plan.anvil, &anvil_note) {
        (Some(digest), _) => {
            text.push_str(&format!("  anvil {digest} at {}\n", images::ANVIL_PREFIX))
        }
        (None, Some(why)) => text.push_str(&format!("  anvil skipped: {why}\n")),
        (None, None) => {}
    }
    match &built {
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
            "docker is not on PATH, so nothing was built. Build it where docker is:\n  {command_line}\n",
        )),
    }
    print(&text)
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

fn list(env: &Env, as_json: bool) -> Outcome {
    let repo = repo()?;
    let records = records(&repo)?;
    if as_json {
        return print(&json::text(&json!({ "images": records })));
    }
    if records.is_empty() {
        return print(
            "no image built or pinned here. `by images build --harness ID` writes one.\n",
        );
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
    print(&table(&columns, &rows, env.style()))
}

fn show(reference: &str, as_json: bool) -> Outcome {
    let repo = repo()?;
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
    print(&text)
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

fn pin(reference: &str, provider: Option<&str>, as_json: bool) -> Outcome {
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
    let repo = repo()?;
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
    let repo = repo()?;
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
