//! `by tools publish|pull`: the shared, content-addressed tool store
//! (docs/tools-model.md, section 4).
//!
//! `publish` builds the store's entries for the built-in skills and the
//! configured connector bundles (running Anvil's packager), writes
//! `store.json`, and, on request, pushes each entry to Artifact Registry
//! with `oras` and mirrors the store to Cloud Storage with `gcloud storage
//! rsync`. `pull` fills a local store from either, then checks every entry
//! against the index. Neither command needs a repository to pull; both read
//! `[tools] store` when `--store` is not given.

use std::path::{Path, PathBuf};

use branchyard::configure;
use branchyard::connectors::packager::Packager;
use branchyard::connectors::AnvilPackager;
use branchyard::toolstore::{self, Index, Programs};
use serde_json::json;

use crate::commands::{print, Failure, Outcome, Target};
use crate::errors::Report;
use crate::{setup_io, workspace_cmd};

pub const TOOLS_EXAMPLES: &str = "\
Examples:
  by tools publish --store /srv/store                  # build skills and connector packages
  by tools publish --store /srv/store --gcs gs://acme-tools/store
  by tools publish --store /srv/store --oci us-docker.pkg.dev/acme/tools/store
  by tools pull --from gs://acme-tools/store --store /opt/branchyard/store
  by tools pull --oci us-docker.pkg.dev/acme/tools/store

The store is read-only to turns: they link its entries instead of copying
them. [tools] store in branchyard.toml names it; see docs/tools-model.md.
--oci needs the oras CLI on PATH; --gcs and --from gs:// need gcloud.";

/// `by tools ...`.
#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ToolsAction {
    /// Build the store's entries and index; push them with --oci, mirror them with --gcs
    ///
    /// Writes skills/NAME@DIGEST for each built-in skill and connectors/ID@HASH
    /// for each bundle under [connectors] bundles (packaged by Anvil), then
    /// store.json. Entries already there are kept.
    Publish {
        /// The store directory (default: [tools] store)
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
        /// Push each entry and the index to REGISTRY/REPOSITORY with oras
        #[arg(long, value_name = "REGISTRY/REPO")]
        oci: Option<String>,
        /// Mirror the store to gs://BUCKET/PREFIX with gcloud storage rsync
        #[arg(long, value_name = "GS_URL")]
        gcs: Option<String>,
    },
    /// Fill a local store from a bucket or a registry, then check every entry
    ///
    /// Reads store.json, fetches the entries the store lacks and refuses any
    /// whose files do not match the digest the index records.
    #[command(group(
        clap::ArgGroup::new("source").required(true).args(["from", "oci"])
    ))]
    Pull {
        /// Pull from gs://BUCKET/PREFIX with gcloud storage rsync
        #[arg(long, value_name = "GS_URL")]
        from: Option<String>,
        /// Pull from REGISTRY/REPOSITORY with oras
        #[arg(long, value_name = "REGISTRY/REPO")]
        oci: Option<String>,
        /// The store directory (default: [tools] store)
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },
}

pub fn main(target: &Target, action: &ToolsAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Report::usage(
            "by tools fills this machine's store; a server's store is set in its configuration \
             and filled where the server runs",
        )
        .next("run by tools without --remote")
        .into());
    }
    if workspace_cmd::in_harness().is_some() {
        return Err(Report::new(
            "denied",
            "a harness running on a branch cannot change the shared tool store",
        )
        .into());
    }
    let cwd = std::env::current_dir()?;
    let root = setup_io::project_root(&cwd);
    let config = configure::configuration(&root)?
        .map(|effective| effective.config)
        .unwrap_or_default();
    match action {
        ToolsAction::Publish { store, oci, gcs } => {
            let store = store_dir(store.as_deref(), &config)?;
            publish(&root, &config, &store, oci.as_deref(), gcs.as_deref(), json)
        }
        ToolsAction::Pull { from, oci, store } => {
            let store = store_dir(store.as_deref(), &config)?;
            pull(&store, from.as_deref(), oci.as_deref(), json)
        }
    }
}

/// `--store`, else `[tools] store`.
fn store_dir(
    flag: Option<&Path>,
    config: &branchyard::config::ProjectConfig,
) -> Result<PathBuf, Failure> {
    flag.map(Path::to_path_buf)
        .or_else(|| config.tools.store.as_ref().map(PathBuf::from))
        .ok_or_else(|| {
            Report::new("config", "no tool store is named")
                .next("pass --store DIR")
                .next("set [tools] store in branchyard.toml (docs/tools-model.md)")
                .into()
        })
}

fn failed(what: &str, why: String) -> Failure {
    Report::new("io", format!("{what}: {why}"))
        .next("fix the cause and run the command again; entries already written are kept")
        .into()
}

fn publish(
    root: &Path,
    config: &branchyard::config::ProjectConfig,
    store: &Path,
    oci: Option<&str>,
    gcs: Option<&str>,
    json: bool,
) -> Outcome {
    // Refuse a bad destination before building anything.
    if let Some(location) = gcs {
        toolstore::check_gcs(location).map_err(|e| Failure::from(Report::usage(e)))?;
    }
    if let Some(base) = oci {
        toolstore::check_oci(base).map_err(|e| Failure::from(Report::usage(e)))?;
    }
    let bundles = configure::bundles(root, &config.connectors);
    let packager = match bundles.is_dir() {
        true => Some((
            AnvilPackager {
                command: configure::anvil_command(&config.connectors)?,
                root: bundles.clone(),
            },
            root.join(".branchyard/tools-publish-cache"),
        )),
        false => None,
    };
    let index = toolstore::publish(
        store,
        packager
            .as_ref()
            .map(|(packager, cache)| (packager as &dyn Packager, cache.as_path())),
    )
    .map_err(|e| failed("could not build the store", e))?;
    if let Some(location) = gcs {
        toolstore::mirror_to_gcs(&Programs::default(), store, location)
            .map_err(|e| failed("could not mirror the store", e))?;
    }
    if let Some(base) = oci {
        toolstore::push_oci(&Programs::default(), store, base)
            .map_err(|e| failed("could not push the store", e))?;
    }
    report(
        "published",
        store,
        &index,
        &[("gcs", gcs), ("oci", oci)],
        packager.is_none().then(|| bundles.display().to_string()),
        json,
    )
}

fn pull(store: &Path, from: Option<&str>, oci: Option<&str>, json: bool) -> Outcome {
    let index = match (from, oci) {
        (Some(location), None) => toolstore::pull_gcs(&Programs::default(), location, store),
        (None, Some(base)) => toolstore::pull_oci(&Programs::default(), base, store),
        _ => {
            return Err(
                Report::usage("name one source: --from gs://... or --oci REF")
                    .next("by tools pull --from gs://BUCKET/PREFIX")
                    .into(),
            )
        }
    }
    .map_err(|e| failed("could not pull the store", e))?;
    report(
        "pulled",
        store,
        &index,
        &[("from", from), ("oci", oci)],
        None,
        json,
    )
}

fn report(
    verb: &str,
    store: &Path,
    index: &Index,
    places: &[(&str, Option<&str>)],
    no_bundles: Option<String>,
    json: bool,
) -> Outcome {
    let skills = index
        .entries
        .iter()
        .filter(|e| e.kind == toolstore::Kind::Skill)
        .count();
    let connectors = index.entries.len() - skills;
    if json {
        let mut value = json!({
            "store": store,
            "skills": skills,
            "connectors": connectors,
            "entries": index.entries,
        });
        for (key, place) in places {
            if let Some(place) = place {
                value[*key] = json!(place);
            }
        }
        return print(&format!("{value:#}\n"));
    }
    let mut text = format!(
        "{verb} {skills} skill{} and {connectors} connector package{} in {}\n",
        if skills == 1 { "" } else { "s" },
        if connectors == 1 { "" } else { "s" },
        store.display()
    );
    for (key, place) in places {
        if let Some(place) = place {
            text.push_str(&format!("  {key}: {place}\n"));
        }
    }
    if let Some(missing) = no_bundles {
        text.push_str(&format!(
            "  no connector packages: {missing} is not a directory (set [connectors] bundles)\n"
        ));
    }
    print(&text)
}
