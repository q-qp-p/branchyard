//! The turn manifest: everything a session was admitted with, resolved once
//! and kept with the branch.
//!
//! When a session starts the engine resolves its [`TurnManifest`]: who it
//! acts for, its connector grants and the digest of each bundle that serves
//! them, the skills chosen (or all), the helpers envelope, the models and
//! network it may reach, and the tenant's policy version at admission. It is
//! stored with the branch, and every turn of the session uses it. It is
//! resolved again only when the person changes the session's tools, so a
//! turn never silently picks up a different set. The manifest has an id, its
//! digest ([`TurnManifest::digest`]): the blake3 of its canonical JSON.
//!
//! A turn in a private home finds the manifest at
//! [`MANIFEST_FILE`] (named by `BRANCHYARD_MANIFEST`), and its gateway
//! token carries the digest as `by_manifest`, which Anvil writes to every
//! audit line. The file holds no secret. See `docs/connectors.md` and
//! `docs/tools-model.md`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connectors::GrantEntry;
use crate::skills::{ForTurn, Source, UserSet};
use crate::{Envelope, Error, Yard};

/// The repository's and the person's skills a turn stages, read for the
/// turn ([`crate::skills::for_turn`], [`crate::skills::user_for_turn`]),
/// which the manifest records beside the built-in ones.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sources<'a> {
    /// What the branch stages from its repository.
    pub repository: &'a ForTurn,
    /// The person's own skills; `None` for a branch created without a
    /// folder.
    pub user: Option<&'a UserSet>,
}

impl Sources<'_> {
    /// The manifest entries of both sources: the repository's staged
    /// skills (none when withheld), then the person's.
    pub fn entries(&self) -> Vec<ManifestSkill> {
        let repository = match self.repository {
            ForTurn::Staged(set) => set.entries(),
            ForTurn::Nothing | ForTurn::Withheld(_) => Vec::new(),
        };
        let user = self.user.map(UserSet::entries).unwrap_or_default();
        repository
            .into_iter()
            .chain(user)
            .map(|entry| ManifestSkill {
                id: entry.label(),
                digest: entry.digest,
                source: entry.source,
            })
            .collect()
    }
}

/// The manifest file, relative to the harness's home.
pub const MANIFEST_FILE: &str = ".branchyard/manifest.json";

/// The variable naming [`MANIFEST_FILE`] as the harness sees it.
pub const ENV_MANIFEST: &str = "BRANCHYARD_MANIFEST";

/// The manifest's format version.
pub const VERSION: u32 = 1;

/// Who the session acts for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestActor {
    /// The token's `sub`.
    pub subject: String,
    /// The token's `by_tenant`.
    pub tenant: String,
}

/// The built-in skills a session was given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestSkills {
    /// Every built-in skill (no choice was made).
    All,
    /// Only these, by id.
    Chosen(Vec<String>),
}

/// One connector bundle the session's grant is served from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestConnector {
    /// The connector's id.
    pub id: String,
    /// The packager's hash of its bundle (`Bundle::hash`). Empty when the
    /// gateway did not serve it when the manifest was resolved; a turn
    /// then checks nothing against it.
    pub digest: String,
    /// The BLAKE3 hash of the package's files as the shared tool store's
    /// index listed them when the manifest was resolved
    /// ([`crate::toolstore`]); absent when the store had no entry. A turn
    /// that links the store's package checks it against this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// One skill the session's tool tree mounts, chosen or not, and the content
/// hash of its files as the host that admitted it read them
/// (`docs/skill-registry.md`, section 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSkill {
    /// The skill's id as the tree names it: a built-in skill's name, or
    /// `repository:<name>` and `user:<name>` for the other sources.
    pub id: String,
    /// [`crate::toolstore::skill_digest`] for a built-in skill (the digest
    /// in its tool-store path, `skills/<name>@<digest>`), and
    /// [`skillset::digest`](branchyard_provision::skillset::digest) for the
    /// rest: both BLAKE3 over the skill's files.
    pub digest: String,
    /// Where it came from. Absent in a manifest resolved before R2, when
    /// every entry was a built-in skill, and written only for the rest.
    #[serde(default, skip_serializing_if = "Source::is_built_in")]
    pub source: Source,
}

impl ManifestSkill {
    /// The entry for a built-in skill.
    pub fn built_in(id: impl Into<String>, digest: impl Into<String>) -> ManifestSkill {
        ManifestSkill {
            id: id.into(),
            digest: digest.into(),
            source: Source::BuiltIn,
        }
    }
}

/// The sandbox image a session was admitted with (docs/images.md).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestImage {
    /// The image reference as the provider was given it, `REF@sha256:...`
    /// where it is pinned.
    pub reference: String,
    /// Its `sha256:` digest, when the reference is pinned by one. A turn
    /// whose provider receipt names another digest fails
    /// `bundle_mismatch` (checked by the engine before the harness starts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Its `dev.branchyard.toolchain` label, when Branchyard could read the
    /// image's labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
}

/// What a session was admitted with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnManifest {
    /// [`VERSION`].
    pub version: u32,
    /// Who the session acts for.
    pub actor: ManifestActor,
    /// The connector grant, after the person's ceiling.
    #[serde(default)]
    pub grants: Vec<GrantEntry>,
    /// The built-in skills chosen, or all.
    pub skills: ManifestSkills,
    /// The bundle behind each granted connector.
    #[serde(default)]
    pub connectors: Vec<ManifestConnector>,
    /// Every skill the session's tool tree mounts, chosen or not, with its
    /// digest: the built-in skills staged (every allowed one where linking
    /// is free, else the chosen ones), the repository's trusted skills and
    /// the person's own (`docs/skill-registry.md`, section 4). A turn that
    /// holds a skill with another digest, or links a store entry whose files
    /// hash to another, fails `bundle_mismatch` ([`crate::bundles`]). Empty
    /// in a manifest resolved before skills were recorded, which checks
    /// nothing. Read from `skill_digests`, the field's name before R2.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "skill_digests"
    )]
    pub mounted: Vec<ManifestSkill>,
    /// The sandbox image of a sandboxed session: absent for a local
    /// harness, a recipe's machine and a Substrate actor, which name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ManifestImage>,
    /// What the session may delegate; absent when it may not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<Envelope>,
    /// The models it may call through the model gateway, as globs; absent
    /// when its branch is not on the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// Its network policy: `open`, `none`, or the rules as `by show` prints
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The tenant's tool-policy version when the session was admitted (or
    /// last checked), 0 where there is no server.
    pub policy_version: u64,
    /// Milliseconds since the Unix epoch when it was resolved.
    pub created_at_ms: u64,
}

impl TurnManifest {
    /// The canonical JSON: keys sorted, no whitespace.
    pub fn canonical(&self) -> String {
        match serde_json::to_value(self) {
            Ok(value) => canonical(&value),
            // Every field serializes; keep the digest total all the same.
            Err(_) => String::new(),
        }
    }

    /// The manifest's id: `blake3:<hex>` over [`TurnManifest::canonical`].
    pub fn digest(&self) -> String {
        format!(
            "blake3:{}",
            blake3::hash(self.canonical().as_bytes()).to_hex()
        )
    }

    /// What the person chose, as against what the world resolved around it:
    /// a change here is a change of tools, which resolves the manifest
    /// again. Bundle digests, the built-in skills' digests, the policy
    /// version and the time are not. The repository's and the person's own
    /// skills are: a person who trusts another set, or edits their own
    /// folder, chose other tools, so their entries count with their digests
    /// ([`ManifestSkill`]). Which built-in skills are mounted (their ids,
    /// not their digests) counts too: a policy narrowed since the manifest
    /// was written mounts fewer, and that is other tools.
    fn tools(&self) -> Value {
        let mut built_in: Vec<&str> = self
            .mounted
            .iter()
            .filter(|skill| skill.source == Source::BuiltIn)
            .map(|skill| skill.id.as_str())
            .collect();
        built_in.sort_unstable();
        let custom: Vec<&ManifestSkill> = self
            .mounted
            .iter()
            .filter(|skill| skill.source != Source::BuiltIn)
            .collect();
        serde_json::json!({
            "actor": self.actor,
            "grants": self.grants,
            "skills": self.skills,
            "custom": custom,
            "built_in": built_in,
            "helpers": self.helpers,
            "models": self.models,
            "network": self.network,
        })
    }

    /// The entries of `source` in [`TurnManifest::mounted`].
    pub fn mounted_from(&self, source: Source) -> impl Iterator<Item = &ManifestSkill> {
        self.mounted.iter().filter(move |s| s.source == source)
    }

    /// The mounted entry named `id`, as the tool tree names it.
    pub fn mounted_skill(&self, id: &str) -> Option<&ManifestSkill> {
        self.mounted.iter().find(|s| s.id == id)
    }

    /// The same tools as `other`.
    pub fn same_tools(&self, other: &TurnManifest) -> bool {
        canonical(&self.tools()) == canonical(&other.tools())
    }

    /// Write the manifest into `home` at [`MANIFEST_FILE`], 0644 (it holds
    /// no secret), by rename, unless the file already holds exactly this;
    /// whether it wrote. A turn that changed nothing writes nothing.
    pub(crate) fn write(&self, home: &Path) -> Result<bool, Error> {
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::State(e.to_string()))?;
        let path = home.join(MANIFEST_FILE);
        if std::fs::read(&path).is_ok_and(|current| current == text.as_bytes()) {
            return Ok(false);
        }
        crate::connectors::keys::write_atomic(&path, text.as_bytes(), 0o644)?;
        Ok(true)
    }
}

/// `value` as JSON with every object's keys sorted and no whitespace.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// The manifest `record` runs this turn with: the stored one when the tools
/// are those it was resolved for, else one resolved now. `record` is the
/// branch as scoped by the person's ceiling. `policy_version` is the
/// server's, when it gave one: a manifest that otherwise stands takes it.
/// `resumed` is a turn that wakes the branch's kept (paused) sandbox: the
/// manifest is resolved again whatever it holds, so a tool deselected or
/// disallowed while the sandbox slept is gone and the bundles are those
/// this host serves now (`docs/tools-model.md`, step 7). `sources` are the
/// repository's and the person's skills as this turn read them.
pub(crate) fn settle(
    yard: &Yard,
    record: &crate::state::Record,
    scopes: &crate::access::TokenScopes,
    policy_version: Option<u64>,
    resumed: bool,
    sources: Sources<'_>,
) -> TurnManifest {
    let stored = record.manifest.as_ref();
    let mut fresh = resolve(yard, record, scopes, policy_version.unwrap_or(0), sources);
    if resumed {
        if policy_version.is_none() {
            fresh.policy_version = stored.map_or(0, |s| s.policy_version);
        }
        return fresh;
    }
    match stored {
        Some(stored) if stored.same_tools(&fresh) => {
            let mut kept = stored.clone();
            if let Some(version) = policy_version {
                kept.policy_version = version;
            }
            kept
        }
        Some(stored) if policy_version.is_none() => {
            // A change of tools with no word from a server keeps the
            // policy version the session was admitted under.
            fresh.policy_version = stored.policy_version;
            fresh
        }
        _ => fresh,
    }
}

/// Resolve a manifest for `record` now.
fn resolve(
    yard: &Yard,
    record: &crate::state::Record,
    scopes: &crate::access::TokenScopes,
    policy_version: u64,
    sources: Sources<'_>,
) -> TurnManifest {
    let spec = record.provision.as_ref();
    // The built-in skills the turn stages: every allowed one where the
    // provider mounts them all, else the chosen ones (`crate::capabilities`).
    let choice = crate::capabilities::Choice {
        chosen: spec.and_then(|p| p.skills.as_deref()),
        allowed: spec.and_then(|p| p.allowed_skills.as_deref()),
        mount_ready: crate::providers::of(record.provider.as_ref()).mounts_ready_skills(yard),
    };
    let mut mounted = choice
        .staged()
        .and_then(|staged| crate::bundles::skill_digests(staged.as_ref()))
        .unwrap_or_default();
    mounted.extend(sources.entries());
    let gateway = yard.connectors();
    let actor = match &record.actor {
        Some(actor) => ManifestActor {
            subject: actor.subject.clone(),
            tenant: actor.tenant.clone(),
        },
        None => ManifestActor {
            subject: gateway
                .as_ref()
                .map_or_else(crate::connectors::local_subject, |g| g.subject.clone()),
            tenant: gateway
                .as_ref()
                .map_or_else(|| "local".to_owned(), |g| g.tenant.clone()),
        },
    };
    let grants = spec.map(|p| p.connectors.clone()).unwrap_or_default();
    let served = match (&gateway, grants.is_empty()) {
        (Some(gateway), false) => gateway.packager.served().unwrap_or_default(),
        _ => Vec::new(),
    };
    // The store's index, when it has one: a store without `store.json` is a
    // store that names no digests, and a manifest then records none for it
    // (a turn checks nothing it was not given). One that is unreadable is
    // said, not hidden.
    let listed = yard
        .tool_store()
        .filter(|store| store.root.join(crate::toolstore::INDEX_FILE).is_file())
        .and_then(|store| {
            branchyard_support::best_effort(
                "read the tool store's index for the manifest's package digests",
                crate::toolstore::read_index(&store.root),
            )
        });
    let connectors = branchyard_provision::connectors::connectors(&grants)
        .into_iter()
        .map(|id| {
            let digest = served
                .iter()
                .find(|b| b.id == id)
                .map(|b| b.hash.clone())
                .unwrap_or_default();
            let content = listed.as_ref().and_then(|index| {
                index
                    .entries
                    .iter()
                    .find(|e| {
                        e.kind == crate::toolstore::Kind::Connector
                            && e.id == id
                            && e.key() == digest
                    })
                    .map(|e| e.digest.clone())
            });
            ManifestConnector {
                digest,
                content,
                id,
            }
        })
        .collect();
    TurnManifest {
        version: VERSION,
        actor,
        grants,
        skills: match spec.and_then(|p| p.skills.clone()) {
            Some(chosen) => ManifestSkills::Chosen(chosen),
            None => ManifestSkills::All,
        },
        connectors,
        mounted,
        image: crate::providers::of(record.provider.as_ref())
            .image()
            .map(|reference| {
                crate::images::manifest_image(yard.image_labels().as_ref(), &reference)
            }),
        helpers: record.grant.as_ref().map(|g| g.envelope.clone()),
        models: scopes.models.clone(),
        network: scopes.network.as_ref().map(|n| n.policy.clone()),
        policy_version,
        created_at_ms: branchyard_support::time::now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> TurnManifest {
        TurnManifest {
            version: VERSION,
            actor: ManifestActor {
                subject: "oidc:ada".into(),
                tenant: "acme".into(),
            },
            grants: vec![GrantEntry::parse("github:read:issues.*").unwrap()],
            skills: ManifestSkills::Chosen(vec!["docx".into()]),
            connectors: vec![ManifestConnector {
                id: "github".into(),
                digest: "h1".into(),
                content: None,
            }],
            mounted: Vec::new(),
            image: None,
            helpers: None,
            models: Some(vec!["claude-*".into()]),
            network: Some("open".into()),
            policy_version: 3,
            created_at_ms: 1_000,
        }
    }

    #[test]
    fn the_digest_is_over_sorted_compact_json_and_changes_with_any_field() {
        let m = manifest();
        let text = m.canonical();
        assert!(text.starts_with("{\"actor\":{\"subject\":\"oidc:ada\",\"tenant\":\"acme\"}"));
        assert!(m.digest().starts_with("blake3:"));
        assert_eq!(m.digest().len(), "blake3:".len() + 64);
        assert_eq!(m.digest(), manifest().digest());
        let mut other = manifest();
        other.policy_version = 4;
        assert_ne!(m.digest(), other.digest());
        // The form on disk and the form hashed read back the same.
        let back: TurnManifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.digest(), m.digest());
    }

    #[test]
    fn only_a_change_of_tools_is_a_change_of_tools() {
        let m = manifest();
        let mut later = manifest();
        later.policy_version = 9;
        later.created_at_ms = 2_000;
        later.connectors[0].digest = "h2".into();
        assert!(m.same_tools(&later));
        let mut skills = manifest();
        skills.skills = ManifestSkills::All;
        assert!(!m.same_tools(&skills));
        let mut grants = manifest();
        grants.grants = vec![GrantEntry::parse("github:write").unwrap()];
        assert!(!m.same_tools(&grants));
        // A built-in skill's digest is the world's; the person's own skill, or
        // the repository's, is the person's choice of tools.
        let mut built_in = manifest();
        built_in.mounted.push(ManifestSkill::built_in("docx", "d1"));
        let mut rebuilt = built_in.clone();
        rebuilt.mounted[0].digest = "d2".into();
        assert!(built_in.same_tools(&rebuilt), "a new digest is the world's");
        // Which built-in skills are mounted is a change of tools: a policy
        // narrowed since mounts fewer.
        assert!(!m.same_tools(&built_in));
        let mut fewer = built_in.clone();
        fewer.mounted.clear();
        assert!(!built_in.same_tools(&fewer));
        assert!(m.same_tools(&fewer));
        let mut mine = manifest();
        mine.mounted.push(ManifestSkill {
            id: "user:memory-parser".into(),
            digest: "u1".into(),
            source: Source::User,
        });
        assert!(!m.same_tools(&mine));
        let mut edited = mine.clone();
        edited.mounted[0].digest = "u2".into();
        assert!(!mine.same_tools(&edited), "an edited folder is other tools");
        let mut repository = manifest();
        repository.mounted.push(ManifestSkill {
            id: "repository:style".into(),
            digest: "r1".into(),
            source: Source::Repository,
        });
        assert!(!m.same_tools(&repository));
        assert_eq!(mine.mounted_from(Source::User).count(), 1);
        assert_eq!(mine.mounted_from(Source::BuiltIn).count(), 0);
        assert_eq!(
            mine.mounted_skill("user:memory-parser").unwrap().digest,
            "u1"
        );
    }

    /// A manifest stored before R2 named the field `skill_digests` and gave
    /// no source; it reads back as built-in entries of `mounted`, and a
    /// built-in entry is still written without a source.
    #[test]
    fn mounted_reads_the_old_field_and_writes_a_source_for_the_other_sources_only() {
        let old = serde_json::json!({
            "version": 1,
            "actor": {"subject": "s", "tenant": "t"},
            "skills": "all",
            "skill_digests": [{"id": "docx", "digest": "d1"}],
            "policy_version": 0,
            "created_at_ms": 1
        });
        let read: TurnManifest = serde_json::from_value(old).unwrap();
        assert_eq!(read.mounted, [ManifestSkill::built_in("docx", "d1")]);
        let mut m = manifest();
        m.mounted = vec![
            ManifestSkill::built_in("docx", "d1"),
            ManifestSkill {
                id: "repository:style".into(),
                digest: "r1".into(),
                source: Source::Repository,
            },
        ];
        let text = serde_json::to_string(&m).unwrap();
        assert!(text.contains(r#""mounted":[{"id":"docx","digest":"d1"},{"id":"repository:style","digest":"r1","source":"repository"}]"#), "{text}");
        assert!(!text.contains("skill_digests"));
        let back: TurnManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn the_file_holds_the_manifest_and_nothing_secret() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest();
        m.write(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILE)).unwrap();
        let back: TurnManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back.digest(), m.digest());
        assert!(!text.to_lowercase().contains("token"), "{text}");
    }
}
