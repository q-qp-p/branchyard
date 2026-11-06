//! Holding a turn to the bundles its manifest admitted.
//!
//! A session's [`TurnManifest`] records, when it is resolved, the digest of
//! each built-in skill it was given and of the bundle behind each connector
//! it was granted. A later turn may run on another host, or on this one
//! after its tool store, its packager or its own build changed; it must not
//! quietly use what it finds. Before the harness starts the engine checks:
//!
//! - **a skill**: the digest of the skill this build embeds
//!   ([`crate::toolstore::skill_digest`]) is the one the manifest recorded,
//!   and a tool-store entry the turn would link (`skills/<name>@<digest>`)
//!   holds files that hash to it;
//! - **a connector**: the bundle hash the packager serves now is the one the
//!   manifest recorded, and a tool-store package the turn would link holds
//!   files that hash to the digest the store's index gave when the manifest
//!   was resolved (`content`).
//!
//! A difference fails the turn with a [`Mismatch`], whose text starts with
//! [`BUNDLE_MISMATCH`] and names the entry, the digest admitted and the one
//! found. It is recorded as a warning event with the same text and ends the
//! turn `failed`. A copy is checked by the digest of its source, never by
//! reading the copy again; a store entry is hashed once per process and
//! afterwards only when a stat of its files changes
//! (`connectors::packager::hash_dir_cached`). The way past a
//! mismatch is to resolve the manifest again: change the session's tools, or
//! wake a kept sandbox, which resolves it afresh (`docs/connectors.md`).

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::OnceLock;

use crate::manifest::{ManifestSkill, TurnManifest};
use crate::toolstore::{Kind, ToolStore};

/// The kind of failure a turn reports when it holds a bundle other than
/// the one its manifest admitted. A failed turn's reason, and the warning
/// before it, start with this word.
pub const BUNDLE_MISMATCH: &str = "bundle_mismatch";

/// A skill or connector bundle that is not the one the manifest admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// Skill or connector.
    pub kind: Kind,
    /// The skill's name or the connector's id.
    pub id: String,
    /// The digest the manifest recorded.
    pub expected: String,
    /// The digest this host holds.
    pub found: String,
    /// Where the difference is.
    pub why: String,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::Skill => "skill",
            Kind::Connector => "connector",
        };
        write!(
            f,
            "{BUNDLE_MISMATCH}: {kind} {}: the session was admitted with {} but this host has \
             {} ({}); the turn did not start. Change the session's tools or start a new \
             session to admit what this host holds",
            self.id, self.expected, self.found, self.why
        )
    }
}

/// The sandbox image a provider's receipt names is not the one the manifest
/// admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMismatch {
    /// The image reference the manifest recorded.
    pub reference: String,
    /// The digest the manifest recorded.
    pub expected: String,
    /// The digest the provider's receipt names, or what it named instead.
    pub found: String,
}

impl fmt::Display for ImageMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{BUNDLE_MISMATCH}: image {}: the session was admitted with {} but the sandbox the \
             provider made is from {}; the turn did not start. Start a new session to admit the \
             image the provider now uses",
            self.reference, self.expected, self.found
        )
    }
}

/// Check that the image a provider's receipt names (`receipt`, its image
/// reference) has the digest `manifest` admitted. A manifest with no image
/// or no digest, and a provider that keeps no receipt (`None`), check nothing.
pub(crate) fn verify_image(
    manifest: &TurnManifest,
    receipt: Option<&str>,
) -> Result<(), ImageMismatch> {
    let (Some(image), Some(receipt)) = (&manifest.image, receipt) else {
        return Ok(());
    };
    let Some(expected) = &image.digest else {
        return Ok(());
    };
    match crate::images::digest_of(receipt) {
        Some(found) if found == expected => Ok(()),
        found => Err(ImageMismatch {
            reference: image.reference.clone(),
            expected: expected.clone(),
            found: found.map_or_else(|| format!("{receipt} (no digest)"), str::to_owned),
        }),
    }
}

/// The digest of every skill this build embeds, by name, computed once.
fn embedded() -> &'static HashMap<String, String> {
    static DIGESTS: OnceLock<HashMap<String, String>> = OnceLock::new();
    DIGESTS.get_or_init(|| {
        crate::capabilities::catalogued()
            .map(|skills| {
                skills
                    .iter()
                    .map(|skill| (skill.name.to_owned(), crate::toolstore::skill_digest(skill)))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The digests of the built-in skills a turn stages for `only` (the task's
/// choice, `None` for every skill): those and the skills they need, sorted
/// by name.
pub(crate) fn skill_digests(only: Option<&[String]>) -> Result<Vec<ManifestSkill>, String> {
    let chosen = only
        .map(branchyard_provision::built_in::closure)
        .transpose()?;
    let mut out: Vec<ManifestSkill> = embedded()
        .iter()
        .filter(|(name, _)| {
            chosen
                .as_ref()
                .is_none_or(|set| set.contains(name.as_str()))
        })
        .map(|(id, digest)| ManifestSkill {
            id: id.clone(),
            digest: digest.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Check the skills `manifest` admitted against this host: the embedded
/// skill, and the tool-store entry the turn would link, when `store` is
/// the store the turn sees.
pub(crate) fn verify_skills(
    manifest: &TurnManifest,
    store: Option<&ToolStore>,
) -> Result<(), Mismatch> {
    for skill in &manifest.skill_digests {
        let held = embedded().get(&skill.id);
        if held != Some(&skill.digest) {
            return Err(Mismatch {
                kind: Kind::Skill,
                id: skill.id.clone(),
                expected: skill.digest.clone(),
                found: held.cloned().unwrap_or_else(|| "none".to_owned()),
                why: "this build embeds other files for the skill".to_owned(),
            });
        }
        if let Some(dir) = store.and_then(|s| s.skill_entry(&skill.id, &skill.digest)) {
            entry_holds(Kind::Skill, &skill.id, &skill.digest, &dir)?;
        }
    }
    Ok(())
}

/// Check that the bundle the packager serves for connector `id`
/// (`served`, its hash) is the one `manifest` admitted. A manifest that
/// recorded none checks nothing.
pub(crate) fn verify_served(
    manifest: &TurnManifest,
    id: &str,
    served: &str,
) -> Result<(), Mismatch> {
    let Some(admitted) = manifest
        .connectors
        .iter()
        .find(|c| c.id == id && !c.digest.is_empty())
    else {
        return Ok(());
    };
    match admitted.digest == served {
        true => Ok(()),
        false => Err(Mismatch {
            kind: Kind::Connector,
            id: id.to_owned(),
            expected: admitted.digest.clone(),
            found: served.to_owned(),
            why: "the gateway's packager serves another bundle".to_owned(),
        }),
    }
}

/// Check that the tool-store package `dir` the turn would link for
/// connector `id` holds the files `manifest` admitted. Nothing is
/// checked when the manifest recorded no content digest for it.
pub(crate) fn verify_linked(manifest: &TurnManifest, id: &str, dir: &Path) -> Result<(), Mismatch> {
    let Some(expected) = manifest
        .connectors
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| c.content.as_deref())
    else {
        return Ok(());
    };
    entry_holds(Kind::Connector, id, expected, dir)
}

/// Whether the files in `dir` hash to `expected`.
fn entry_holds(kind: Kind, id: &str, expected: &str, dir: &Path) -> Result<(), Mismatch> {
    match crate::connectors::packager::hash_dir_cached(dir) {
        Ok((found, _)) if found == expected => Ok(()),
        Ok((found, _)) => Err(Mismatch {
            kind,
            id: id.to_owned(),
            expected: expected.to_owned(),
            found,
            why: format!("the files in {} are not the ones admitted", dir.display()),
        }),
        Err(error) => Err(Mismatch {
            kind,
            id: id.to_owned(),
            expected: expected.to_owned(),
            found: "unreadable".to_owned(),
            why: format!("{} could not be read: {error}", dir.display()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestActor, ManifestConnector, ManifestSkills, VERSION};

    fn manifest(skills: &[&str]) -> TurnManifest {
        let only: Vec<String> = skills.iter().map(|s| (*s).to_owned()).collect();
        TurnManifest {
            version: VERSION,
            actor: ManifestActor {
                subject: "local:ada".into(),
                tenant: "local".into(),
            },
            grants: Vec::new(),
            skills: ManifestSkills::Chosen(only.clone()),
            connectors: vec![ManifestConnector {
                id: "github".into(),
                digest: "bundle-1".into(),
                content: Some("content-1".into()),
            }],
            skill_digests: skill_digests(Some(&only)).unwrap(),
            image: None,
            helpers: None,
            models: None,
            network: None,
            policy_version: 0,
            created_at_ms: 1,
        }
    }

    #[test]
    fn a_manifest_records_the_digest_of_each_staged_skill_and_its_needs() {
        let all = skill_digests(None).unwrap();
        assert!(all.len() > 3, "every built-in skill: {all:?}");
        let one = skill_digests(Some(&["docx".to_owned()])).unwrap();
        assert!(one.iter().any(|s| s.id == "docx"), "{one:?}");
        assert!(one.len() < all.len());
        let docx = branchyard_provision::skills::skill("docx").expect("docx is built in");
        let recorded = one.iter().find(|s| s.id == "docx").unwrap();
        assert_eq!(recorded.digest, crate::toolstore::skill_digest(docx));
        assert!(skill_digests(Some(&["no-such-skill".to_owned()])).is_err());
    }

    #[test]
    fn an_image_receipt_of_another_digest_is_a_mismatch_naming_both() {
        use crate::manifest::ManifestImage;
        let admitted = format!("sha256:{}", "a".repeat(64));
        let mut m = manifest(&[]);
        // A session with no image, or one admitted by tag, checks nothing.
        verify_image(&m, Some("x@sha256:other")).unwrap();
        m.image = Some(ManifestImage {
            reference: "r:1".into(),
            digest: None,
            toolchain: None,
        });
        verify_image(&m, Some("x@sha256:other")).unwrap();
        m.image = Some(ManifestImage {
            reference: format!("r:1@{admitted}"),
            digest: Some(admitted.clone()),
            toolchain: None,
        });
        verify_image(&m, Some(&format!("r:1@{admitted}"))).unwrap();
        // A provider that keeps no receipt says nothing against it.
        verify_image(&m, None).unwrap();
        let other = format!("sha256:{}", "b".repeat(64));
        let mismatch = verify_image(&m, Some(&format!("r:1@{other}"))).unwrap_err();
        let text = mismatch.to_string();
        assert!(text.starts_with("bundle_mismatch: image r:1@"), "{text}");
        assert!(text.contains(&admitted) && text.contains(&other), "{text}");
        // A receipt with no digest at all is not the digest admitted either.
        let bare = verify_image(&m, Some("r:1")).unwrap_err();
        assert!(bare.found.contains("no digest"), "{bare}");
    }

    #[test]
    fn a_skill_other_than_the_admitted_one_names_both_digests() {
        let mut admitted = manifest(&["docx"]);
        verify_skills(&admitted, None).unwrap();
        let index = admitted
            .skill_digests
            .iter()
            .position(|s| s.id == "docx")
            .unwrap();
        let held = admitted.skill_digests[index].digest.clone();
        admitted.skill_digests[index].digest = "0".repeat(64);
        let mismatch = verify_skills(&admitted, None).unwrap_err();
        assert_eq!(mismatch.kind, Kind::Skill);
        assert_eq!(mismatch.id, "docx");
        assert_eq!(mismatch.expected, "0".repeat(64));
        assert_eq!(mismatch.found, held);
        let text = mismatch.to_string();
        assert!(text.starts_with(BUNDLE_MISMATCH), "{text}");
        assert!(
            text.contains("skill docx") && text.contains(&held),
            "{text}"
        );
        // A skill this build does not embed at all is no match either.
        let mut gone = manifest(&["docx"]);
        gone.skill_digests.push(ManifestSkill {
            id: "from-the-future".into(),
            digest: "abc".into(),
        });
        let missing = verify_skills(&gone, None).unwrap_err();
        assert_eq!(
            (missing.id.as_str(), missing.found.as_str()),
            ("from-the-future", "none")
        );
    }

    #[test]
    fn a_store_entry_whose_files_changed_is_not_linked() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolStore::new(dir.path());
        crate::toolstore::publish(dir.path(), None).unwrap();
        let admitted = manifest(&["docx"]);
        verify_skills(&admitted, Some(&store)).unwrap();
        // Another host's store holds other files under the same name.
        let docx = admitted
            .skill_digests
            .iter()
            .find(|s| s.id == "docx")
            .unwrap();
        let entry = dir.path().join(format!("skills/docx@{}", docx.digest));
        std::fs::write(entry.join("SKILL.md"), "# a different docx\n").unwrap();
        let mismatch = verify_skills(&admitted, Some(&store)).unwrap_err();
        assert_eq!(mismatch.expected, docx.digest);
        assert_ne!(mismatch.found, docx.digest);
        assert!(
            mismatch.why.contains(&entry.display().to_string()),
            "{mismatch}"
        );
        // A turn that cannot see the store never links, so never reads it.
        verify_skills(&admitted, None).unwrap();
    }

    #[test]
    fn a_served_bundle_and_a_linked_package_are_held_to_the_manifest() {
        let admitted = manifest(&[]);
        verify_served(&admitted, "github", "bundle-1").unwrap();
        let mismatch = verify_served(&admitted, "github", "bundle-2").unwrap_err();
        assert_eq!(mismatch.expected, "bundle-1");
        assert_eq!(mismatch.found, "bundle-2");
        assert!(mismatch.to_string().contains("connector github"));
        // A connector the manifest records no digest for is not checked.
        let mut blank = manifest(&[]);
        blank.connectors[0].digest.clear();
        verify_served(&blank, "github", "anything").unwrap();
        verify_served(&admitted, "linear", "anything").unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "# github\n").unwrap();
        let held = crate::connectors::packager::hash_dir(dir.path()).unwrap();
        let mut linked = manifest(&[]);
        linked.connectors[0].content = Some(held.clone());
        verify_linked(&linked, "github", dir.path()).unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "# someone else's github\n").unwrap();
        let mismatch = verify_linked(&linked, "github", dir.path()).unwrap_err();
        assert_eq!(mismatch.expected, held);
        assert_ne!(mismatch.found, held);
        // No content recorded: nothing to hold the package to.
        verify_linked(&manifest(&[]).with_no_content(), "github", dir.path()).unwrap();
        verify_linked(&linked, "missing", dir.path()).unwrap();
    }

    impl TurnManifest {
        fn with_no_content(mut self) -> TurnManifest {
            self.connectors[0].content = None;
            self
        }
    }
}
