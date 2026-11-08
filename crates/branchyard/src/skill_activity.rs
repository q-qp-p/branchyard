//! What a turn did with its skills, as events on the branch
//! (`docs/skill-registry.md`, section 7): a search of the tool tree, a
//! skill, reference, connector or helper shown, a staged skill's script run.
//!
//! The guest-side `tools` script cannot write to the branch's log, so it
//! appends one JSON line per `search` and `show` to [`LOG_FILE`] in the
//! turn's home (`.branchyard/turn/skills.jsonl`, 0600), and the engine reads
//! the file once the harness is gone (`ingest`), records each line as an
//! [`Activity::Skill`], and removes the file. A line that does not parse is
//! one warning. The digest and source of what was opened come from the
//! turn's manifest ([`TurnManifest::mounted`]), never from the guest. An
//! invocation ([`SkillActivity::Invoked`]) is the engine's own, from a
//! `tool_started` whose target names a path under a staged skill's
//! `scripts/` ([`invoked`]).
//!
//! These events are a local measure, for `by skills usage` and the
//! skill-discovery eval, and never an input to ranking: the file is
//! guest-writable, so a turn can forge an open, which is harmless for a
//! count and would not be for a ranking factor. The query's words are not
//! recorded, only their count and the ids found (open decision 5 of
//! `docs/skill-registry.md`).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::TurnManifest;
use crate::skills::Source;
use crate::Activity;

/// Where the `tools` script notes each search and show, relative to the
/// turn's home.
pub const LOG_FILE: &str = ".branchyard/turn/skills.jsonl";

/// The most lines the ingest reads from one turn's log; a guest that writes
/// more wrote a loop, and the rest is dropped with a warning.
pub const MAX_LINES: usize = 10_000;

/// What a turn did with a skill, recorded as [`Activity::Skill`].
/// Serialized as an object tagged by `type`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillActivity {
    /// `tools search` ran: the query's term count and the ids it ranked
    /// first (`kind:id`), never the words.
    Searched {
        /// Distinct terms in the query.
        terms: u32,
        /// The ids shown, in rank order.
        results: Vec<String>,
        /// The `--kind` the search was narrowed to, when it was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Whether it ranked by BM25 alone (`--plain`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        plain: bool,
    },
    /// A skill, reference, connector, operation or helper was shown
    /// (`tools show`): the progressive-disclosure level read, and the bytes
    /// printed.
    Opened {
        /// The id as the tree names it (`pptx`, `pptx/references/layouts.md`,
        /// `github.issues.list`).
        id: String,
        /// The mounted skill's digest from the manifest, a connector's bundle
        /// hash, or empty when the manifest names neither (a helper).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        digest: String,
        /// The skill's source, for a skill or one of its references.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
        /// `skill`, `reference`, `connector`, `operation` or `helper`.
        kind: String,
        /// 1 for a connector, operation or helper line, 2 for a skill's
        /// `SKILL.md`, 3 for one reference.
        level: u8,
        /// Whether the body was fetched for this open (R5); always false
        /// where the body is staged or linked.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fetched: bool,
        /// Bytes printed.
        bytes: u64,
        /// How long a fetch took (R5); absent without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fetch_ms: Option<u64>,
    },
    /// A script of a staged skill ran: the harness's `tool_started` named a
    /// path under it.
    Invoked {
        /// The skill's id as the tree names it.
        id: String,
        /// The script's path in the skill (`scripts/run.py`).
        path: String,
    },
}

impl SkillActivity {
    /// One line for people, as `by log` prints it.
    pub fn describe(&self) -> String {
        match self {
            SkillActivity::Searched {
                terms,
                results,
                kind,
                plain,
            } => {
                let narrowed = kind
                    .as_deref()
                    .map(|k| format!(" --kind {k}"))
                    .unwrap_or_default();
                let plain = if *plain { " --plain" } else { "" };
                let found = match results.is_empty() {
                    true => "nothing".to_owned(),
                    false => results
                        .iter()
                        .map(|id| id.split_once(':').map_or(id.as_str(), |(_, id)| id))
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                format!("skill search ({terms} terms{narrowed}{plain}) → {found}")
            }
            SkillActivity::Opened {
                id, level, bytes, ..
            } => format!("skill opened {id} (level {level}, {} bytes)", group(*bytes)),
            SkillActivity::Invoked { id, path } => format!("skill script {id}/{path} ran"),
        }
    }
}

/// `n` with thousands separators.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What [`ingest`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Ingested {
    /// The events to record, in the order the guest wrote them.
    pub activities: Vec<SkillActivity>,
    /// Lines that did not parse, each as one warning text.
    pub warnings: Vec<String>,
}

/// Read the turn's skill log under `home` ([`LOG_FILE`]), turn each line
/// into a [`SkillActivity`] with the digest and source `manifest` gives
/// what was opened, and remove the file. A home without one yields nothing.
/// The caller records what comes back.
pub(crate) fn ingest(home: &Path, manifest: &TurnManifest) -> io::Result<Ingested> {
    let path = home.join(LOG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ingested::default()),
        Err(e) => return Err(e),
    };
    let mut out = Ingested::default();
    for (number, line) in text.lines().enumerate() {
        if number >= MAX_LINES {
            out.warnings.push(format!(
                "skill log: {} lines after the first {MAX_LINES} were not read",
                text.lines().count() - MAX_LINES
            ));
            break;
        }
        match parse_line(line, manifest) {
            Ok(Some(activity)) => out.activities.push(activity),
            Ok(None) => {}
            Err(why) => out
                .warnings
                .push(format!("skill log line {}: {why}", number + 1)),
        }
    }
    std::fs::remove_file(&path)?;
    // The directory goes with the log when nothing else of the guest's is
    // in it.
    if let Some(dir) = path.parent() {
        if std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none()) {
            std::fs::remove_dir(dir)?;
        }
    }
    Ok(out)
}

/// One line of the log as the script writes it (`tools.py`, `note`), or
/// `None` for a blank one.
fn parse_line(line: &str, manifest: &TurnManifest) -> Result<Option<SkillActivity>, String> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .ok_or("no action")?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| value.get(key).and_then(Value::as_u64);
    Ok(Some(match action {
        "searched" => SkillActivity::Searched {
            terms: u32::try_from(number("terms").ok_or("no terms")?).unwrap_or(u32::MAX),
            results: value
                .get("results")
                .and_then(Value::as_array)
                .ok_or("no results")?
                .iter()
                .filter_map(Value::as_str)
                .take(50)
                .map(|id| cut(id, 200))
                .collect(),
            kind: text("kind"),
            plain: value.get("plain").and_then(Value::as_bool).unwrap_or(false),
        },
        "opened" => {
            let id = cut(&text("id").ok_or("no id")?, 200);
            let kind = text("kind").ok_or("no kind")?;
            let (digest, source) = identity(manifest, &id, &kind);
            SkillActivity::Opened {
                id,
                digest,
                source,
                kind,
                level: u8::try_from(number("level").ok_or("no level")?)
                    .map_err(|_| "level out of range".to_owned())?,
                fetched: false,
                bytes: number("bytes").unwrap_or(0),
                fetch_ms: None,
            }
        }
        other => return Err(format!("unknown action {other:?}")),
    }))
}

/// The first `max` characters of `text`.
fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The digest and source of what `id` names, from the manifest: a skill's
/// (or its reference's skill's) mounted entry, a connector's bundle hash
/// (an operation's too), nothing for a helper or an id the manifest does
/// not know.
fn identity(manifest: &TurnManifest, id: &str, kind: &str) -> (String, Option<Source>) {
    match kind {
        "skill" | "reference" => {
            let skill = id.split("/references/").next().unwrap_or(id);
            match manifest.mounted_skill(skill) {
                Some(entry) => (entry.digest.clone(), Some(entry.source)),
                None => (String::new(), None),
            }
        }
        "connector" | "operation" => {
            let connector = id.split('.').next().unwrap_or(id);
            let digest = manifest
                .connectors
                .iter()
                .find(|c| c.id == connector)
                .map(|c| c.digest.clone())
                .unwrap_or_default();
            (digest, None)
        }
        _ => (String::new(), None),
    }
}

/// An invocation, when a `tool_started` target names a path under a staged
/// skill's `scripts/`: `skills/<id>/scripts/<file>` in the tool tree, or
/// `capabilities/<id>/scripts/<file>` under the home, as a command word or
/// a file path; a repository's or a person's skill under `repository/` or
/// `user/` gets its source label. `None` for any other target.
pub fn invoked(target: &branchyard_harness::ToolTarget) -> Option<SkillActivity> {
    for word in target
        .value
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
    {
        let Some(after) = ["/skills/", "skills/", "/capabilities/", "capabilities/"]
            .iter()
            .filter_map(|marker| word.find(marker).map(|at| &word[at + marker.len()..]))
            .find(|rest| rest.contains("/scripts/"))
        else {
            continue;
        };
        let Some((skill, script)) = after.split_once("/scripts/") else {
            continue;
        };
        let path = format!(
            "scripts/{}",
            script.trim_end_matches(|c: char| !c.is_alphanumeric())
        );
        if script.is_empty() || skill.is_empty() {
            return None;
        }
        let id = match skill.split_once('/') {
            Some((source, name)) if source == "repository" || source == "user" => {
                format!("{source}:{name}")
            }
            Some(_) => return None,
            None => skill.to_owned(),
        };
        return Some(SkillActivity::Invoked {
            id: cut(&id, 200),
            path: cut(&path, 200),
        });
    }
    None
}

impl From<SkillActivity> for Activity {
    fn from(activity: SkillActivity) -> Activity {
        Activity::Skill(Box::new(activity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        ManifestActor, ManifestConnector, ManifestSkill, ManifestSkills, VERSION,
    };
    use branchyard_harness::ToolTarget;

    fn manifest() -> TurnManifest {
        TurnManifest {
            version: VERSION,
            actor: ManifestActor {
                subject: "s".into(),
                tenant: "local".into(),
            },
            grants: Vec::new(),
            skills: ManifestSkills::All,
            connectors: vec![ManifestConnector {
                id: "github".into(),
                digest: "bundle-1".into(),
                content: None,
            }],
            mounted: vec![
                ManifestSkill::built_in("pptx", "d-pptx"),
                ManifestSkill {
                    id: "user:memory-parser".into(),
                    digest: "d-mine".into(),
                    source: Source::User,
                },
            ],
            image: None,
            helpers: None,
            models: None,
            network: None,
            policy_version: 0,
            created_at_ms: 1,
        }
    }

    #[test]
    fn the_log_becomes_events_with_the_manifests_digests_and_is_removed() {
        let home = tempfile::tempdir().unwrap();
        let log = home.path().join(LOG_FILE);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(
            &log,
            concat!(
                r#"{"at_ms":1,"action":"searched","terms":3,"results":["skill:pptx","reference:pptx/references/layouts.md"],"kind":null,"plain":false}"#,
                "\n",
                r#"{"at_ms":2,"action":"opened","id":"pptx/references/layouts.md","kind":"reference","level":3,"bytes":4120}"#,
                "\n",
                r#"{"at_ms":3,"action":"opened","id":"user:memory-parser","kind":"skill","level":2,"bytes":900}"#,
                "\n",
                r#"{"at_ms":4,"action":"opened","id":"github.issues.list","kind":"operation","level":1,"bytes":0}"#,
                "\n",
                r#"{"at_ms":5,"action":"opened","id":"codex","kind":"helper","level":1,"bytes":0}"#,
                "\n",
                "this is not json\n",
                r#"{"action":"danced"}"#,
                "\n\n",
                r#"{"at_ms":6,"action":"searched","terms":1,"results":[],"kind":"reference","plain":true}"#,
                "\n",
            ),
        )
        .unwrap();
        let ingested = ingest(home.path(), &manifest()).unwrap();
        assert_eq!(ingested.warnings.len(), 2, "{:?}", ingested.warnings);
        assert!(ingested.warnings[0].starts_with("skill log line 6: not JSON"));
        assert!(ingested.warnings[1].contains("unknown action \"danced\""));
        let got = ingested.activities;
        assert_eq!(got.len(), 6);
        assert_eq!(
            got[0],
            SkillActivity::Searched {
                terms: 3,
                results: vec![
                    "skill:pptx".into(),
                    "reference:pptx/references/layouts.md".into()
                ],
                kind: None,
                plain: false,
            }
        );
        let SkillActivity::Opened {
            id,
            digest,
            source,
            kind,
            level,
            bytes,
            fetched,
            fetch_ms,
        } = &got[1]
        else {
            panic!("{:?}", got[1]);
        };
        assert_eq!(id, "pptx/references/layouts.md");
        assert_eq!(
            (digest.as_str(), *source),
            ("d-pptx", Some(Source::BuiltIn))
        );
        assert_eq!((kind.as_str(), *level, *bytes), ("reference", 3, 4120));
        assert!(!fetched && fetch_ms.is_none());
        match &got[2] {
            SkillActivity::Opened { digest, source, .. } => {
                assert_eq!((digest.as_str(), *source), ("d-mine", Some(Source::User)));
            }
            other => panic!("{other:?}"),
        }
        match &got[3] {
            SkillActivity::Opened { digest, source, .. } => {
                assert_eq!((digest.as_str(), *source), ("bundle-1", None));
            }
            other => panic!("{other:?}"),
        }
        match &got[4] {
            SkillActivity::Opened { digest, source, .. } => {
                assert_eq!((digest.as_str(), *source), ("", None));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            got[5].describe(),
            "skill search (1 terms --kind reference --plain) → nothing"
        );
        assert_eq!(
            got[0].describe(),
            "skill search (3 terms) → pptx, pptx/references/layouts.md"
        );
        assert_eq!(
            got[1].describe(),
            "skill opened pptx/references/layouts.md (level 3, 4,120 bytes)"
        );
        assert!(!log.exists(), "read once");
        assert!(
            !log.parent().unwrap().exists(),
            "its directory goes with it"
        );
        // Nothing to read is nothing recorded.
        assert_eq!(
            ingest(home.path(), &manifest()).unwrap(),
            Ingested::default()
        );
    }

    #[test]
    fn a_long_log_is_cut_with_a_warning_and_a_crowded_directory_is_left() {
        let home = tempfile::tempdir().unwrap();
        let log = home.path().join(LOG_FILE);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(log.parent().unwrap().join("other"), "theirs").unwrap();
        let line = r#"{"action":"searched","terms":1,"results":[]}"#;
        let many: String = std::iter::repeat_n(line, MAX_LINES + 3)
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(&log, many).unwrap();
        let ingested = ingest(home.path(), &manifest()).unwrap();
        assert_eq!(ingested.activities.len(), MAX_LINES);
        assert_eq!(
            ingested.warnings,
            ["skill log: 3 lines after the first 10000 were not read"]
        );
        assert!(!log.exists());
        assert!(log.parent().unwrap().join("other").is_file());
    }

    #[test]
    fn a_scripts_path_in_a_tool_target_is_an_invocation() {
        let command = |text: &str| ToolTarget::command(text).unwrap();
        assert_eq!(
            invoked(&command("python3 tools/skills/pptx/scripts/run.py --help")),
            Some(SkillActivity::Invoked {
                id: "pptx".into(),
                path: "scripts/run.py".into(),
            })
        );
        assert_eq!(
            invoked(&command(
                "python3 $HOME/.branchyard/capabilities/repository/style/scripts/lint.py a.md"
            )),
            Some(SkillActivity::Invoked {
                id: "repository:style".into(),
                path: "scripts/lint.py".into(),
            })
        );
        assert_eq!(
            invoked(
                &ToolTarget::file("tools/skills/user/memory-parser/scripts/bench.sh", "").unwrap()
            ),
            Some(SkillActivity::Invoked {
                id: "user:memory-parser".into(),
                path: "scripts/bench.sh".into(),
            })
        );
        assert_eq!(
            invoked(&command("python3 tools/skills/pptx/scripts/run.py"))
                .unwrap()
                .describe(),
            "skill script pptx/scripts/run.py ran"
        );
        // A read of a skill, or a script elsewhere, is not an invocation.
        assert_eq!(invoked(&command("cat tools/skills/pptx/SKILL.md")), None);
        assert_eq!(invoked(&command("python3 scripts/run.py")), None);
        assert_eq!(invoked(&command("ls tools/skills/")), None);
        assert_eq!(invoked(&ToolTarget::skill("pptx").unwrap()), None);
    }

    #[test]
    fn the_event_serializes_tagged_by_type_without_empty_fields() {
        let event: Activity = SkillActivity::Opened {
            id: "codex".into(),
            digest: String::new(),
            source: None,
            kind: "helper".into(),
            level: 1,
            fetched: false,
            bytes: 0,
            fetch_ms: None,
        }
        .into();
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(
            text,
            r#"{"skill":{"type":"opened","id":"codex","kind":"helper","level":1,"bytes":0}}"#
        );
        let back: Activity = serde_json::from_str(&text).unwrap();
        assert_eq!(back, event);
        assert_eq!(group(1_234_567), "1,234,567");
        assert_eq!(group(12), "12");
    }
}
