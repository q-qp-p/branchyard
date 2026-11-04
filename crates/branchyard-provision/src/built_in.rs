//! The built-in skills' catalogue, and a turn's choice among them.
//!
//! `plugins/branchyard/capabilities.json` lists the built-in skills, each
//! with a topic and the skills it needs. A task may stage only some of them
//! ([`crate::Provisioning::skills`]): the chosen ids and everything they
//! need. The root and topic indexes a harness reads are then rebuilt from
//! the catalogue in the format `tools/artifact_assets.py` writes, so a
//! harness is never pointed at a skill it was not given. Without a choice,
//! [`indexes`] returns the shipped indexes byte for byte.
//!
//! A built-in skill is guidance, not access: choosing fewer narrows what a
//! harness is told about, not what its policy lets it do.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::skills;

/// One built-in skill, as the catalogue describes it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltIn {
    /// The skill's id, which is its directory.
    pub id: String,
    /// One sentence on what it does.
    pub description: String,
    /// The topic it is listed under.
    #[serde(default = "documents")]
    pub topic: String,
    /// The skills it needs, staged with it.
    #[serde(default)]
    pub requires: Vec<String>,
}

/// One topic of the root index.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    /// The topic's id, which is its directory under `topics/`.
    pub id: String,
    /// Its heading.
    pub title: String,
    /// One sentence on what its skills are for.
    pub description: String,
}

/// The whole catalogue.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Catalogue {
    /// Every built-in skill, in catalogue order.
    pub skills: Vec<BuiltIn>,
    /// Every topic, in index order.
    pub topics: Vec<Topic>,
}

fn documents() -> String {
    "documents".to_owned()
}

/// The embedded catalogue.
pub fn catalogue() -> Result<Catalogue, String> {
    serde_json::from_str(skills::CAPABILITIES)
        .map_err(|e| format!("invalid embedded capabilities: {e}"))
}

/// The ids `asked` names and every skill they need, in no order. An id the
/// catalogue does not list is refused by name.
pub fn closure(asked: &[String]) -> Result<BTreeSet<String>, String> {
    let catalogue = catalogue()?;
    let mut chosen = BTreeSet::new();
    let mut pending: Vec<String> = asked.to_vec();
    while let Some(id) = pending.pop() {
        let skill = catalogue
            .skills
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| unknown(&id, &catalogue))?;
        if chosen.insert(id) {
            pending.extend(skill.requires.iter().cloned());
        }
    }
    Ok(chosen)
}

fn unknown(id: &str, catalogue: &Catalogue) -> String {
    let known: Vec<&str> = catalogue.skills.iter().map(|s| s.id.as_str()).collect();
    format!(
        "skill {id:?} is not a built-in skill; the built-in skills are {}",
        known.join(", ")
    )
}

/// Refuse a choice that names a skill the catalogue does not have.
pub fn check(asked: &[String]) -> Result<(), String> {
    closure(asked).map(|_| ())
}

/// The indexes for a turn: the root index, and each topic index by its
/// path under the skills directory, `None` for a topic with no chosen
/// skill (its file is removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Indexes {
    /// `INDEX.md`.
    pub root: String,
    /// `topics/<id>/INDEX.md`, with its text or `None`.
    pub topics: Vec<(String, Option<String>)>,
}

/// The indexes for `chosen` (every skill when `None`), in the format of
/// `tools/artifact_assets.py`.
pub fn indexes(chosen: Option<&BTreeSet<String>>) -> Result<Indexes, String> {
    let catalogue = catalogue()?;
    let picked = |skill: &&BuiltIn| chosen.is_none_or(|set| set.contains(&skill.id));
    let mut root = String::from(
        "# Branchyard skills\n\nChoose a topic, then read only the relevant skill and \
         supporting files. Keep your native planning and subagents.\n\n",
    );
    let mut topics = Vec::new();
    for topic in &catalogue.topics {
        let path = format!("topics/{}/INDEX.md", topic.id);
        let listed: Vec<&BuiltIn> = catalogue
            .skills
            .iter()
            .filter(|s| s.topic == topic.id)
            .filter(picked)
            .collect();
        if listed.is_empty() && chosen.is_some() {
            topics.push((path, None));
            continue;
        }
        root.push_str(&format!(
            "- [{}](topics/{}/INDEX.md): {}\n",
            topic.title, topic.id, topic.description
        ));
        let mut text = format!(
            "# {}\n\n{}\n\n[All topics](../../INDEX.md)\n\n",
            topic.title, topic.description
        );
        for skill in listed {
            text.push_str(&format!(
                "- [{id}](../../{id}/INDEX.md): {}\n",
                skill.description,
                id = skill.id
            ));
        }
        topics.push((path, Some(text.trim_end().to_owned() + "\n")));
    }
    if !skills::UPSTREAM_LOCK.is_empty() {
        root.push_str("\n[Upstream sources and licenses](upstream.lock.json)\n");
    }
    Ok(Indexes {
        root: root.trim_end().to_owned() + "\n",
        topics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_choice_the_indexes_are_the_shipped_ones() {
        let all = indexes(None).unwrap();
        assert_eq!(all.root, skills::INDEX);
        assert_eq!(all.topics.len(), skills::TOPIC_INDEXES.len());
        for (path, text) in skills::TOPIC_INDEXES {
            let built = all.topics.iter().find(|(p, _)| p == path).unwrap();
            assert_eq!(built.1.as_deref(), Some(*text), "{path}");
        }
    }

    #[test]
    fn a_choice_brings_what_it_needs_and_lists_only_that() {
        let chosen = closure(&["parallel-deck".to_owned()]).unwrap();
        // parallel-deck needs pptx, which needs artifact-tools.
        assert_eq!(
            chosen.iter().map(String::as_str).collect::<Vec<_>>(),
            ["artifact-tools", "parallel-deck", "pptx"]
        );
        let built = indexes(Some(&chosen)).unwrap();
        assert!(built.root.contains("topics/workflows/INDEX.md"));
        assert!(built.root.contains("topics/documents/INDEX.md"));
        assert!(!built.root.contains("topics/design/INDEX.md"));
        let documents = built
            .topics
            .iter()
            .find(|(p, _)| p == "topics/documents/INDEX.md")
            .and_then(|(_, t)| t.clone())
            .unwrap();
        assert!(documents.contains("[pptx]"));
        assert!(!documents.contains("[docx]"));
        let design = built
            .topics
            .iter()
            .find(|(p, _)| p == "topics/design/INDEX.md")
            .unwrap();
        assert_eq!(design.1, None);
    }

    #[test]
    fn an_unknown_skill_is_refused_by_name() {
        let error = check(&["docx".to_owned(), "nope".to_owned()]).unwrap_err();
        assert!(error.contains("\"nope\""), "{error}");
        assert!(error.contains("docx"), "{error}");
        check(&[]).unwrap();
    }
}
