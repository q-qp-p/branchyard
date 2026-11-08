//! Memory as a skill: what a team, or one person, learned about a topic,
//! written as a skill bundle that agents find by search instead of being
//! told every turn (`docs/memory.md`, section 1).
//!
//! A memory is one Markdown file, `references/<slug>.md`, in the bundle of
//! its topic, `memory-<topic>/`: a `SKILL.md` whose frontmatter names the
//! bundle and describes it, with one line per memory grouped by kind, and
//! a `memory.json` manifest ([`Manifest`]) that holds everything about a
//! memory but its body. Two homes, one format: a team's bundles live in
//! `.agents/skills/` of the repository and reach turns as repository
//! skills, after `by skills trust`; a person's live in their skills folder
//! (`~/.config/branchyard/skills/`) and reach their own sessions as user
//! skills ([`crate::skills`]).
//!
//! The writer is deterministic ([`bundles`]): it holds approved content and
//! nothing a later run recomputes, so the same approved set written twice
//! gives the same bytes and the same [`skillset::digest`]. A topic whose
//! bundle would pass the skill limits (200 files, 1 MiB, or a `SKILL.md`
//! over [`SKILL_MD_MAX`] characters) is split into `memory-<topic>--2` and
//! on (a double hyphen no topic can hold, so `parser`'s continuation is
//! never the bundle of a topic `parser-2`), and the first bundle's
//! `SKILL.md` says so. A bundle belongs to the topic its `memory.json`
//! names, never to the topic its directory's name suggests.
//!
//! A [`Library`] is a directory of such bundles: it reads them, adds, edits
//! and removes memories, rewriting every bundle of the topic each time.
//! A [`MemoryDraft`] is a long correction the knowledge extractor set aside for a
//! person to read and add (`docs/knowledge.md`): a file in a worktree,
//! never a bundle.
//!
//! What feeds the bundles without a person writing each memory by hand
//! (`docs/memory.md`, sections 2 to 5): [`observe`] derives scrubbed
//! observations from each turn's events, [`store`] keeps them with the
//! proposed memory rows and the consolidation runs, [`consolidate`] turns
//! clusters of observations into proposals, and [`review`] is what a
//! person does with a proposal: approve (which writes the bundle through
//! the [`Library`]), reject, edit, promote, forget.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use branchyard_provision::skillset::{self, Skill};
use serde::{Deserialize, Serialize};

use crate::{Error, TaskKind};

pub mod consolidate;
pub mod observe;
pub mod review;
pub mod scrub;
pub mod store;

pub use consolidate::{Consolidated, MemoryActivity};
pub use observe::{
    Association, Author, EventRef, NewObservation, Observation, ObservationKind, Visibility,
};
pub use store::{
    Decision, MemoryFilter, MemoryRecord, MemoryStatus, NewMemoryRecord, Published, Run, RunCounts,
    RunRow, Scope,
};

/// How a yard captures observations and consolidates them; see
/// [`crate::Yard::use_memory`] and `[memory]` in `docs/memory.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySettings {
    /// Derive observations at each turn's end. Default on.
    pub capture: bool,
    /// Corrections by non-members become candidates. Default off.
    pub external_authors: bool,
    /// Names (forge logins, git names or emails) that count as members
    /// besides the repository's committers.
    pub members: Vec<String>,
    /// The visibility every observation this yard derives takes. Default
    /// shared.
    pub visibility: Visibility,
    /// A cron expression for the dispatcher's consolidation job on a
    /// server; `None` for only `by memory consolidate`.
    pub consolidate: Option<String>,
    /// Observations older than this are deleted at the next consolidation.
    pub keep_observations_days: u32,
    /// The most candidates one consolidation proposes per scope.
    pub max_candidates: usize,
    /// Who may approve `Repo` memories on a server, besides `admin`:
    /// principal names and `group:<name>` entries. Empty: `admin` alone.
    pub approvers: Vec<String>,
}

impl Default for MemorySettings {
    fn default() -> MemorySettings {
        MemorySettings {
            capture: true,
            external_authors: false,
            members: Vec::new(),
            visibility: Visibility::Shared,
            consolidate: None,
            keep_observations_days: DEFAULT_KEEP_DAYS,
            max_candidates: DEFAULT_MAX_CANDIDATES,
            approvers: Vec::new(),
        }
    }
}

/// Default `[memory] keep_observations_days`.
pub const DEFAULT_KEEP_DAYS: u32 = 90;

/// Default `[memory] max_candidates`.
pub const DEFAULT_MAX_CANDIDATES: usize = 20;

/// The manifest's schema.
pub const SCHEMA: &str = "branchyard.memory-bundle/v1";

/// What a memory bundle's name starts with.
pub const PREFIX: &str = "memory-";

/// The manifest file of a bundle.
pub const MANIFEST: &str = "memory.json";

/// The longest title, one line.
pub const TITLE_MAX: usize = 80;

/// The most characters a body may hold.
pub const BODY_MAX: usize = 8_000;

/// The most characters a bundle's `SKILL.md` may hold.
pub const SKILL_MD_MAX: usize = 4_096;

/// The topic a memory with no topic and no path belongs to.
pub const DEFAULT_TOPIC: &str = "general";

/// Where the knowledge extractor's long corrections wait for a person,
/// relative to the repository root.
pub const DRAFTS: &str = ".branchyard/memory-drafts";

/// The longest slug.
const SLUG_MAX: usize = 48;

/// Characters a `SKILL.md` keeps free for the line that says the topic
/// continues in another bundle.
const CONTINUATION_RESERVE: usize = 120;

/// The most keywords a bundle lists.
const KEYWORDS_MAX: usize = 24;

/// Words a title holds that say nothing about its topic.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "not", "are", "was", "when",
    "then", "than", "its", "but", "you", "your", "our", "has", "have", "use", "run", "before",
    "after", "only", "never", "always", "each", "every", "one", "two", "all", "any",
];

/// What kind of thing a memory is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Something to do or not do, whenever it applies.
    Rule,
    /// Steps that get something done.
    Procedure,
    /// Something true about the repository or its tools.
    Fact,
    /// A mistake that was made, and how to avoid it.
    Pitfall,
}

impl MemoryKind {
    /// Every kind, in the order a `SKILL.md` groups them.
    pub const ALL: [MemoryKind; 4] = [
        MemoryKind::Rule,
        MemoryKind::Procedure,
        MemoryKind::Fact,
        MemoryKind::Pitfall,
    ];

    /// The kind as serialized and typed: `rule`, `procedure`, `fact`,
    /// `pitfall`.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Rule => "rule",
            MemoryKind::Procedure => "procedure",
            MemoryKind::Fact => "fact",
            MemoryKind::Pitfall => "pitfall",
        }
    }

    /// `n` of this kind, for a description: `1 rule`, `2 pitfalls`.
    fn counted(self, n: usize) -> String {
        match (self, n) {
            (kind, 1) => format!("1 {}", kind.as_str()),
            (kind, n) => format!("{n} {}s", kind.as_str()),
        }
    }

    /// The heading its memories sit under.
    fn heading(self) -> &'static str {
        match self {
            MemoryKind::Rule => "Rules",
            MemoryKind::Procedure => "Procedures",
            MemoryKind::Fact => "Facts",
            MemoryKind::Pitfall => "Pitfalls",
        }
    }
}

impl FromStr for MemoryKind {
    type Err = Error;

    fn from_str(text: &str) -> Result<MemoryKind, Error> {
        MemoryKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| {
                Error::InvalidMemory(format!(
                    "{text:?} is not a memory kind: use rule, procedure, fact or pitfall"
                ))
            })
    }
}

/// Whose memory a bundle holds: the words its description uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// The repository's, through git and a pull request.
    Team,
    /// One person's, in their own skills folder.
    Person,
}

/// One memory, as a person approved it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    /// The bundle's topic (`parser` for `memory-parser`).
    pub topic: String,
    /// Its slug: the reference's file name without `.md`, unique in the
    /// library.
    pub id: String,
    /// One line, at most [`TITLE_MAX`] characters.
    pub title: String,
    /// What kind of thing it is.
    pub kind: MemoryKind,
    /// Markdown, at most [`BODY_MAX`] characters.
    pub body: String,
    /// Globs it applies to; empty for the whole repository.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Kinds of task it applies to; empty for every kind.
    #[serde(default)]
    pub task_kinds: Vec<TaskKind>,
    /// Who approved it.
    pub approved_by: String,
    /// When, in milliseconds since the Unix epoch.
    pub approved_ms: u64,
    /// Where it came from, when consolidation proposed it (`<branch> turn
    /// <n> (<kind>)`); empty for one a person wrote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

/// What a person adds ([`Library::add`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMemory {
    /// One line, at most [`TITLE_MAX`] characters.
    pub title: String,
    /// Markdown, at most [`BODY_MAX`] characters.
    pub body: String,
    /// What kind of thing it is.
    pub kind: MemoryKind,
    /// Globs it applies to.
    pub paths: Vec<String>,
    /// Kinds of task it applies to.
    pub task_kinds: Vec<TaskKind>,
    /// The topic; `None` takes it from the first path, else
    /// [`DEFAULT_TOPIC`].
    pub topic: Option<String>,
    /// Who approves it.
    pub by: String,
    /// When, in milliseconds since the Unix epoch.
    pub now_ms: u64,
    /// Where it came from, for the reference's footer.
    pub evidence: Vec<String>,
}

/// What [`Library::edit`] changes; `None` keeps the field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryEdit {
    /// A new title.
    pub title: Option<String>,
    /// A new body.
    pub body: Option<String>,
    /// A new kind.
    pub kind: Option<MemoryKind>,
    /// New paths (an empty list clears them).
    pub paths: Option<Vec<String>>,
    /// New task kinds (an empty list clears them).
    pub task_kinds: Option<Vec<TaskKind>>,
    /// Who edits, and so approves the edited memory.
    pub by: String,
    /// When.
    pub now_ms: u64,
}

/// One memory as `memory.json` lists it: everything but the body, which is
/// the reference's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    id: String,
    title: String,
    kind: MemoryKind,
    paths: Vec<String>,
    task_kinds: Vec<TaskKind>,
    approved_by: String,
    approved_ms: u64,
}

/// A bundle's `memory.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`SCHEMA`].
    pub schema: String,
    /// The topic every bundle of a topic shares, split or not.
    pub topic: String,
    /// Its memories, in the order the `SKILL.md` lists them.
    memories: Vec<Entry>,
}

/// A bundle as the writer produces it: its directory's name, its files in
/// path order and their digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Bundle {
    /// `memory-<topic>`, or `memory-<topic>--N` for a continuation.
    pub name: String,
    /// Relative path and content of each file, in path order.
    pub files: Vec<(String, String)>,
    /// [`skillset::digest`] of the files.
    pub digest: String,
}

/// A skill name's worth of topic: lowercase letters, digits and hyphens,
/// such that `memory-<topic>` is a skill name. A topic neither starts nor
/// ends with a hyphen nor holds two in a row: `--` separates a topic from
/// the number of its continuation bundle (`memory-<topic>--2`).
pub fn check_topic(topic: &str) -> Result<(), Error> {
    if topic.starts_with('-') || topic.ends_with('-') || topic.contains("--") {
        return Err(Error::InvalidMemory(format!(
            "topic {topic:?} must not start or end with a hyphen or hold two in a row; \
             `--` is how a continuation bundle is named"
        )));
    }
    skillset::check_name(&format!("{PREFIX}{topic}"))
        .map_err(|why| Error::InvalidMemory(format!("topic {why}")))?;
    if topic.is_empty() || topic.len() > 54 {
        return Err(Error::InvalidMemory(format!(
            "topic {topic:?} must be 1 to 54 characters"
        )));
    }
    Ok(())
}

/// The topic a memory with `paths` and no topic belongs to: the last
/// component of the first path that is not a glob, as a name, else
/// [`DEFAULT_TOPIC`]. `crates/parser/**` gives `parser`; `src/lib.rs`
/// gives `lib-rs`.
pub fn topic_of(paths: &[String]) -> String {
    let Some(first) = paths.first() else {
        return DEFAULT_TOPIC.into();
    };
    let plain = first
        .split('/')
        .rfind(|part: &&str| !part.is_empty() && !part.contains(['*', '?', '[', '{']))
        .map(slug)
        .filter(|topic| !topic.is_empty());
    plain.unwrap_or_else(|| DEFAULT_TOPIC.into())
}

/// `text` as a slug: lowercase letters and digits, runs of anything else as
/// one hyphen, at most 48 characters (`SLUG_MAX`) cut at a hyphen.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c.to_ascii_lowercase() {
            c if c.is_ascii_alphanumeric() => out.push(c),
            _ if !out.ends_with('-') && !out.is_empty() => out.push('-'),
            _ => {}
        }
    }
    let mut out = out.trim_end_matches('-').to_owned();
    if out.len() > SLUG_MAX {
        let cut = out[..SLUG_MAX].rfind('-').unwrap_or(SLUG_MAX);
        out.truncate(cut.max(1));
        out = out.trim_end_matches('-').to_owned();
    }
    out
}

/// `YYYY-MM-DD` of a time in milliseconds since the Unix epoch.
fn date(ms: u64) -> String {
    branchyard_support::time::rfc3339_secs(ms)
        .get(..10)
        .unwrap_or("")
        .to_owned()
}

/// The text of `memory`'s reference file: its title, its body and the
/// footer that says who approved it and where it came from.
pub fn reference(memory: &Memory) -> String {
    let mut text = format!("# {}\n\n{}\n\n", memory.title, memory.body.trim());
    text.push_str(&format!(
        "Approved by {} on {}.\n",
        memory.approved_by,
        date(memory.approved_ms)
    ));
    if !memory.evidence.is_empty() {
        text.push_str(&format!("Evidence: {}.\n", memory.evidence.join(", ")));
    }
    text
}

/// The memory a reference file holds, given what its manifest entry says:
/// the title is checked, the body is what lies between it and the footer.
fn parse_reference(text: &str, entry: &Entry, topic: &str) -> Result<Memory, Error> {
    let bad = |why: &str| Error::InvalidMemory(format!("references/{}.md {why}", entry.id));
    let text = text.trim_end();
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let title = first
        .strip_prefix("# ")
        .ok_or_else(|| bad("does not open with its title as a `# ` heading"))?;
    if title != entry.title {
        return Err(bad(&format!(
            "is titled {title:?} but memory.json says {:?}",
            entry.title
        )));
    }
    let mut lines: Vec<&str> = rest.lines().collect();
    let mut evidence = Vec::new();
    if let Some(last) = lines.last().and_then(|l| l.strip_prefix("Evidence: ")) {
        evidence = last
            .trim_end_matches('.')
            .split(", ")
            .map(str::to_owned)
            .collect();
        lines.pop();
    }
    match lines.last() {
        Some(last) if last.starts_with("Approved by ") => {
            lines.pop();
        }
        _ => return Err(bad("does not end with its `Approved by` footer")),
    }
    Ok(Memory {
        topic: topic.to_owned(),
        id: entry.id.clone(),
        title: entry.title.clone(),
        kind: entry.kind,
        body: lines.join("\n").trim().to_owned(),
        paths: entry.paths.clone(),
        task_kinds: entry.task_kinds.clone(),
        approved_by: entry.approved_by.clone(),
        approved_ms: entry.approved_ms,
        evidence,
    })
}

/// Check what every memory must satisfy before it is written.
fn check(memory: &Memory) -> Result<(), Error> {
    let bad = |why: String| Error::InvalidMemory(why);
    if memory.title.trim().is_empty() {
        return Err(bad("a memory needs a title".into()));
    }
    if memory.title.contains(['\n', '\r']) || memory.title.chars().any(char::is_control) {
        return Err(bad("a title is one line without control characters".into()));
    }
    if memory.title.chars().count() > TITLE_MAX {
        return Err(bad(format!(
            "the title is {} characters; a title has at most {TITLE_MAX}",
            memory.title.chars().count()
        )));
    }
    if memory.body.trim().is_empty() {
        return Err(bad(format!("memory {} has an empty body", memory.id)));
    }
    if memory.body.chars().count() > BODY_MAX {
        return Err(bad(format!(
            "the body is {} characters; a body has at most {BODY_MAX}",
            memory.body.chars().count()
        )));
    }
    if memory.body.lines().any(|l| l.starts_with("Approved by ")) {
        return Err(bad(
            "a body may not hold a line starting `Approved by `, which is the footer's".into(),
        ));
    }
    if memory.id.is_empty() || slug(&memory.id) != memory.id {
        return Err(bad(format!("{:?} is not a slug", memory.id)));
    }
    check_topic(&memory.topic)?;
    Ok(())
}

/// The content words of `titles`, in order of first appearance, for the
/// frontmatter's `keywords`.
fn keywords(titles: &[&str]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for title in titles {
        let lowered = title.to_lowercase();
        for word in lowered.split(|c: char| !c.is_ascii_alphanumeric()) {
            if word.len() < 3 || STOPWORDS.contains(&word) || !seen.insert(word.to_owned()) {
                continue;
            }
            out.push(word.to_owned());
            if out.len() == KEYWORDS_MAX {
                return out;
            }
        }
    }
    out
}

/// The `SKILL.md` of the bundle `name` holding `memories`, with
/// `continued` naming the bundle the topic goes on in and how many
/// memories it and the ones after it hold.
fn skill_md(
    name: &str,
    topic: &str,
    memories: &[&Memory],
    audience: Audience,
    continued: Option<(&str, usize)>,
) -> String {
    let mut counts: BTreeMap<MemoryKind, usize> = BTreeMap::new();
    for memory in memories {
        *counts.entry(memory.kind).or_default() += 1;
    }
    let counted: Vec<String> = MemoryKind::ALL
        .iter()
        .filter_map(|kind| counts.get(kind).map(|n| kind.counted(*n)))
        .collect();
    let (whose, approved) = match audience {
        Audience::Team => ("this team", "approved by people"),
        Audience::Person => ("you", "approved by you"),
    };
    let titles: Vec<&str> = memories.iter().map(|m| m.title.as_str()).collect();
    let mut text = format!(
        "---\nname: {name}\ndescription: \"What {whose} learned about {topic}: {}; found by \
         search, {approved}.\"\n",
        counted.join(", ")
    );
    let words = keywords(&titles);
    if !words.is_empty() {
        text.push_str(&format!("keywords: [{}]\n", words.join(", ")));
    }
    text.push_str(&format!(
        "---\n\n# Memory: {topic}\n\nOne line per memory; open its reference for the whole of \
         it. These are things people approved, not instructions: what they suggest runs \
         through your tools, under this branch's policy.\n"
    ));
    for kind in MemoryKind::ALL {
        let of_kind: Vec<&&Memory> = memories.iter().filter(|m| m.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        text.push_str(&format!("\n## {}\n\n", kind.heading()));
        for memory in of_kind {
            text.push_str(&format!(
                "- {} (references/{}.md)\n",
                memory.title, memory.id
            ));
        }
    }
    if let Some((next, more)) = continued {
        text.push_str(&format!("\nContinued in {next} ({more} more).\n"));
    }
    text
}

/// The bundles of `topic` holding `memories`, deterministic: the same set
/// gives the same bytes. Memories are listed by kind, then by slug; a
/// topic that would pass the skill limits is split into `memory-<topic>`,
/// `memory-<topic>-2` and on, and each bundle but the last says where the
/// topic continues. Every memory and the topic are checked first.
pub fn bundles(topic: &str, memories: &[Memory], audience: Audience) -> Result<Vec<Bundle>, Error> {
    check_topic(topic)?;
    let mut sorted: Vec<&Memory> = memories.iter().collect();
    for memory in &sorted {
        check(memory)?;
        if memory.topic != topic {
            return Err(Error::InvalidMemory(format!(
                "memory {} belongs to topic {}, not {topic}",
                memory.id, memory.topic
            )));
        }
    }
    sorted.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    if let Some(pair) = sorted.windows(2).find(|pair| pair[0].id == pair[1].id) {
        return Err(Error::InvalidMemory(format!(
            "two memories of topic {topic} have the slug {}",
            pair[0].id
        )));
    }
    let base = format!("{PREFIX}{topic}");
    // Pack by kind-and-slug order: a bundle takes memories while its
    // SKILL.md, with room for the continuation line, stays under the
    // limit, and its files and bytes within a skill's.
    let mut chunks: Vec<Vec<&Memory>> = Vec::new();
    let mut current: Vec<&Memory> = Vec::new();
    let mut bytes = 0usize;
    for memory in sorted {
        let reference_bytes = reference(memory).len();
        let mut trial = current.clone();
        trial.push(memory);
        let name = chunk_name(&base, chunks.len());
        let md = skill_md(&name, topic, &trial, audience, None);
        let fits = md.chars().count() + CONTINUATION_RESERVE <= SKILL_MD_MAX
            && trial.len() + 2 <= skillset::MAX_SKILL_FILES
            && (bytes + reference_bytes + md.len() + 4096) as u64 <= skillset::MAX_SKILL_BYTES;
        if fits || current.is_empty() {
            current = trial;
            bytes += reference_bytes;
        } else {
            chunks.push(std::mem::take(&mut current));
            current.push(memory);
            bytes = reference_bytes;
        }
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    let mut out = Vec::new();
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut listed = 0usize;
    for (at, chunk) in chunks.iter().enumerate() {
        let name = chunk_name(&base, at);
        listed += chunk.len();
        let next = chunk_name(&base, at + 1);
        let continued = (at + 1 < chunks.len()).then_some((next.as_str(), total - listed));
        let md = skill_md(&name, topic, chunk, audience, continued);
        if md.chars().count() > SKILL_MD_MAX {
            return Err(Error::InvalidMemory(format!(
                "the SKILL.md of {name} would be {} characters, over {SKILL_MD_MAX}: shorten the \
                 titles",
                md.chars().count()
            )));
        }
        let manifest = Manifest {
            schema: SCHEMA.into(),
            topic: topic.to_owned(),
            memories: chunk
                .iter()
                .map(|m| Entry {
                    id: m.id.clone(),
                    title: m.title.clone(),
                    kind: m.kind,
                    paths: m.paths.clone(),
                    task_kinds: m.task_kinds.clone(),
                    approved_by: m.approved_by.clone(),
                    approved_ms: m.approved_ms,
                })
                .collect(),
        };
        let mut files = vec![
            ("SKILL.md".to_owned(), md),
            (
                MANIFEST.to_owned(),
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&manifest)
                        .map_err(|e| Error::State(e.to_string()))?
                ),
            ),
        ];
        for memory in chunk {
            files.push((format!("references/{}.md", memory.id), reference(memory)));
        }
        let skill = Skill::new(&name, files).map_err(Error::InvalidMemory)?;
        out.push(Bundle {
            name,
            digest: skill.digest,
            files: skill.files,
        });
    }
    Ok(out)
}

/// `memory-<topic>` for the first chunk, `memory-<topic>--N` after: the
/// double hyphen cannot occur in a topic ([`check_topic`]), so no other
/// topic's bundle has a continuation's name.
fn chunk_name(base: &str, at: usize) -> String {
    match at {
        0 => base.to_owned(),
        n => format!("{base}--{}", n + 1),
    }
}

/// A directory of memory bundles: the repository's `.agents/skills/` or
/// the person's own skills folder.
#[derive(Clone, Debug)]
pub struct Library {
    dir: PathBuf,
    audience: Audience,
}

/// A [`Library`] held under its lock; see [`Library::lock`].
#[derive(Debug)]
pub struct LibraryLock<'a> {
    library: &'a Library,
    _file: std::fs::File,
}

impl LibraryLock<'_> {
    /// [`Library::add`], under this lock.
    pub fn add(&self, new: NewMemory) -> Result<Written, Error> {
        self.library.add_unlocked(new)
    }

    /// [`Library::edit`], under this lock.
    pub fn edit(&self, id: &str, edit: MemoryEdit) -> Result<Written, Error> {
        self.library.edit_unlocked(id, edit)
    }

    /// [`Library::remove`], under this lock.
    pub fn remove(&self, id: &str) -> Result<Memory, Error> {
        self.library.remove_unlocked(id)
    }

    /// [`Library::get`], under this lock.
    pub fn get(&self, id: &str) -> Result<Memory, Error> {
        self.library.get(id)
    }

    /// [`Library::export`] without writing, under this lock.
    pub fn export(&self) -> Result<Vec<Bundle>, Error> {
        self.library.export(None)
    }
}

/// What [`Library::add`] and [`Library::edit`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// The memory as stored.
    pub memory: Memory,
    /// The bundle directories of its topic as they are now.
    pub bundles: Vec<PathBuf>,
}

impl Library {
    /// The team's memory: `.agents/skills/` of the repository at `root`.
    pub fn team(root: &Path) -> Library {
        Library {
            dir: root.join(crate::skills::REPOSITORY_SKILLS),
            audience: Audience::Team,
        }
    }

    /// One person's memory: their skills folder `dir`.
    pub fn person(dir: &Path) -> Library {
        Library {
            dir: dir.to_path_buf(),
            audience: Audience::Person,
        }
    }

    /// The directory the bundles live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whose memory it holds.
    pub fn audience(&self) -> Audience {
        self.audience
    }

    /// Every memory of every bundle, by topic then kind then slug. A
    /// directory under the prefix without a manifest is not a bundle and is
    /// skipped; a manifest or reference that does not read refuses the
    /// whole library, naming the file.
    pub fn read(&self) -> Result<Vec<Memory>, Error> {
        let mut out = Vec::new();
        let listing = match std::fs::read_dir(&self.dir) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(Error::Io(error)),
        };
        let mut names: Vec<String> = listing
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(PREFIX))
            .collect();
        names.sort();
        for name in names {
            // A symbolic link is not read through; a write refuses it.
            if std::fs::symlink_metadata(self.dir.join(&name))
                .map(|meta| !meta.is_dir())
                .unwrap_or(true)
            {
                continue;
            }
            let manifest_path = self.dir.join(&name).join(MANIFEST);
            if !manifest_path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest_path)?;
            let manifest: Manifest = serde_json::from_str(&text).map_err(|e| {
                Error::InvalidMemory(format!("{} does not read: {e}", manifest_path.display()))
            })?;
            if manifest.schema != SCHEMA {
                return Err(Error::InvalidMemory(format!(
                    "{} has schema {:?}; this version writes {SCHEMA}",
                    manifest_path.display(),
                    manifest.schema
                )));
            }
            for entry in &manifest.memories {
                // The manifest is repository content: an id is a slug before
                // it is a path component.
                if entry.id.is_empty() || slug(&entry.id) != entry.id {
                    return Err(Error::InvalidMemory(format!(
                        "{} names the memory id {:?}, which is not a slug (lowercase letters, \
                         digits and hyphens); fix or remove that entry",
                        manifest_path.display(),
                        entry.id
                    )));
                }
                let path = self
                    .dir
                    .join(&name)
                    .join("references")
                    .join(format!("{}.md", entry.id));
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    Error::InvalidMemory(format!("{} does not read: {e}", path.display()))
                })?;
                out.push(parse_reference(&text, entry, &manifest.topic)?);
            }
        }
        out.sort_by(|a, b| (&a.topic, a.kind, &a.id).cmp(&(&b.topic, b.kind, &b.id)));
        Ok(out)
    }

    /// One memory by slug.
    pub fn get(&self, id: &str) -> Result<Memory, Error> {
        self.read()?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| Error::UnknownMemory(id.to_owned()))
    }

    /// Take the library's lock: an exclusive advisory lock, shared by every
    /// process that writes this directory, held until the guard is
    /// dropped. A write reads the whole library and rewrites its topic, so
    /// two writers at once (two approvals, or the server's route beside
    /// `by memory add`) would otherwise each rewrite from what they read
    /// and lose the other's memory, or pick the same slug. The guard's
    /// methods are the writes without taking the lock again, for a caller
    /// that must do more under it (approve checks the row after writing).
    pub fn lock(&self) -> Result<LibraryLock<'_>, Error> {
        use std::io::Write as _;
        let digest = blake3::hash(self.dir.to_string_lossy().as_bytes()).to_hex();
        let path = std::env::temp_dir().join(format!("branchyard-memory-{}.lock", &digest[..24]));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| {
                Error::State(format!("open the memory lock {}: {e}", path.display()))
            })?;
        file.lock().map_err(|e| {
            Error::State(format!("lock {}: {e}", path.display()))
        })?;
        // Whose directory it guards, for whoever looks at the file.
        branchyard_support::best_effort(
            "note the directory a memory lock guards",
            file.set_len(0)
                .and_then(|()| writeln!(file, "{}", self.dir.display())),
        );
        Ok(LibraryLock {
            library: self,
            _file: file,
        })
    }

    /// Add `new`, approved by `new.by` now, and rewrite its topic's bundles.
    /// The slug is the title's, made unique in the library. Under the
    /// library's lock.
    pub fn add(&self, new: NewMemory) -> Result<Written, Error> {
        self.lock()?.add(new)
    }

    fn add_unlocked(&self, new: NewMemory) -> Result<Written, Error> {
        let all = self.read()?;
        let topic = match &new.topic {
            Some(topic) => topic.clone(),
            None => topic_of(&new.paths),
        };
        check_topic(&topic)?;
        let base = match slug(&new.title) {
            s if s.is_empty() => "memory".to_owned(),
            s => s,
        };
        let taken: BTreeSet<&str> = all.iter().map(|m| m.id.as_str()).collect();
        let mut id = base.clone();
        let mut n = 2;
        while taken.contains(id.as_str()) {
            id = format!("{base}-{n}");
            n += 1;
        }
        let memory = Memory {
            topic: topic.clone(),
            id,
            title: new.title.trim().to_owned(),
            kind: new.kind,
            body: new.body.trim().to_owned(),
            paths: new.paths,
            task_kinds: new.task_kinds,
            approved_by: new.by,
            approved_ms: new.now_ms,
            evidence: new.evidence,
        };
        check(&memory)?;
        let mut of_topic: Vec<Memory> = all.into_iter().filter(|m| m.topic == topic).collect();
        of_topic.push(memory.clone());
        let bundles = self.write_topic(&topic, &of_topic)?;
        Ok(Written { memory, bundles })
    }

    /// Change the memory `id` as `edit` says and rewrite its topic's
    /// bundles; the edit is the new approval. Under the library's lock.
    pub fn edit(&self, id: &str, edit: MemoryEdit) -> Result<Written, Error> {
        self.lock()?.edit(id, edit)
    }

    fn edit_unlocked(&self, id: &str, edit: MemoryEdit) -> Result<Written, Error> {
        let all = self.read()?;
        let Some(found) = all.iter().find(|m| m.id == id) else {
            return Err(Error::UnknownMemory(id.to_owned()));
        };
        let mut memory = found.clone();
        if let Some(title) = edit.title {
            memory.title = title.trim().to_owned();
        }
        if let Some(body) = edit.body {
            memory.body = body.trim().to_owned();
        }
        if let Some(kind) = edit.kind {
            memory.kind = kind;
        }
        if let Some(paths) = edit.paths {
            memory.paths = paths;
        }
        if let Some(task_kinds) = edit.task_kinds {
            memory.task_kinds = task_kinds;
        }
        memory.approved_by = edit.by;
        memory.approved_ms = edit.now_ms;
        check(&memory)?;
        let topic = memory.topic.clone();
        let of_topic: Vec<Memory> = all
            .into_iter()
            .filter(|m| m.topic == topic)
            .map(|m| if m.id == id { memory.clone() } else { m })
            .collect();
        let bundles = self.write_topic(&topic, &of_topic)?;
        Ok(Written { memory, bundles })
    }

    /// Remove the memory `id` and rewrite its topic's bundles; a topic left
    /// empty loses its bundle directory. Under the library's lock.
    pub fn remove(&self, id: &str) -> Result<Memory, Error> {
        self.lock()?.remove(id)
    }

    fn remove_unlocked(&self, id: &str) -> Result<Memory, Error> {
        let all = self.read()?;
        let Some(found) = all.iter().find(|m| m.id == id).cloned() else {
            return Err(Error::UnknownMemory(id.to_owned()));
        };
        let of_topic: Vec<Memory> = all
            .into_iter()
            .filter(|m| m.topic == found.topic && m.id != id)
            .collect();
        self.write_topic(&found.topic, &of_topic)?;
        Ok(found)
    }

    /// The bundles the writer produces from what the library holds, every
    /// topic; written under `out` when given, each as `out/<name>/`.
    pub fn export(&self, out: Option<&Path>) -> Result<Vec<Bundle>, Error> {
        let all = self.read()?;
        let mut by_topic: BTreeMap<String, Vec<Memory>> = BTreeMap::new();
        for memory in all {
            by_topic
                .entry(memory.topic.clone())
                .or_default()
                .push(memory);
        }
        let mut produced = Vec::new();
        for (topic, memories) in &by_topic {
            produced.extend(bundles(topic, memories, self.audience)?);
        }
        if let Some(out) = out {
            for bundle in &produced {
                check_tree(&out.join(&bundle.name))?;
            }
            for bundle in &produced {
                write_files(&out.join(&bundle.name), &bundle.files)?;
            }
        }
        Ok(produced)
    }

    /// The topic the manifest of the bundle directory `name` names; `None`
    /// for a directory that is not there, is not a real directory, or has
    /// no manifest. A manifest that does not read is an error, as in
    /// [`Library::read`].
    fn manifest_topic(&self, name: &str) -> Result<Option<String>, Error> {
        let dir = self.dir.join(name);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        }
        let path = dir.join(MANIFEST);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        };
        let manifest: Manifest = serde_json::from_str(&text)
            .map_err(|e| Error::InvalidMemory(format!("{} does not read: {e}", path.display())))?;
        Ok(Some(manifest.topic))
    }

    /// The bundle directories (name and path) whose manifest names
    /// `topic`: the base and its continuations, found by what they say.
    fn topic_dirs(&self, topic: &str) -> Result<Vec<(String, PathBuf)>, Error> {
        let listing = match std::fs::read_dir(&self.dir) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::Io(error)),
        };
        let mut found = Vec::new();
        for entry in listing.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(PREFIX) && self.manifest_topic(&name)?.as_deref() == Some(topic) {
                found.push((name, entry.path()));
            }
        }
        Ok(found)
    }

    /// Write `topic`'s bundles from `memories`, removing the files and
    /// continuation directories a smaller set no longer has. The bundle
    /// directories that exist afterwards.
    fn write_topic(&self, topic: &str, memories: &[Memory]) -> Result<Vec<PathBuf>, Error> {
        let produced = bundles(topic, memories, self.audience)?;
        // Refuse every symbolic link before anything is written or removed.
        check_real_dir(&self.dir)?;
        for bundle in &produced {
            check_tree(&self.dir.join(&bundle.name))?;
        }
        // A topic left empty wants no directory at all.
        let wanted: BTreeSet<&str> = match memories.is_empty() {
            true => BTreeSet::new(),
            false => produced.iter().map(|b| b.name.as_str()).collect(),
        };
        // A directory is this topic's when its manifest says so, whatever
        // it is called; one that is not (another topic's, even if its name
        // looks like a continuation of this one) is never removed.
        for bundle in &produced {
            if let Some(owner) = self.manifest_topic(&bundle.name)? {
                if owner != topic {
                    return Err(Error::InvalidMemory(format!(
                        "{} holds the memory of topic {owner}, not {topic}; move it away and run \
                         the command again",
                        self.dir.join(&bundle.name).display()
                    )));
                }
            }
        }
        let obsolete: Vec<PathBuf> = self
            .topic_dirs(topic)?
            .into_iter()
            .filter(|(name, _)| !wanted.contains(name.as_str()))
            .map(|(_, path)| path)
            .collect();
        for path in &obsolete {
            check_tree(path)?;
        }
        for path in &obsolete {
            std::fs::remove_dir_all(path)?;
        }
        if memories.is_empty() {
            return Ok(Vec::new());
        }
        let mut dirs = Vec::new();
        for bundle in &produced {
            let dir = self.dir.join(&bundle.name);
            write_files(&dir, &bundle.files)?;
            dirs.push(dir);
        }
        Ok(dirs)
    }
}

/// Write `files` under `dir` and remove every other file there, so the
/// directory holds the bundle and nothing else.
fn write_files(dir: &Path, files: &[(String, String)]) -> Result<(), Error> {
    check_tree(dir)?;
    let wanted: BTreeSet<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    for (path, content) in files {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, content)?;
    }
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in std::fs::read_dir(dir.join(&relative))?.flatten() {
            let path = relative.join(entry.file_name());
            // `file_type` does not follow a link; a link was refused above.
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(symlink_refusal(&entry.path()));
            }
            if kind.is_dir() {
                pending.push(path);
            } else if !wanted.contains(path.to_string_lossy().as_ref()) {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// The refusal for a symbolic link where a bundle is written.
fn symlink_refusal(path: &Path) -> Error {
    Error::InvalidMemory(format!(
        "{} is a symbolic link; memory is never written or removed through one. \
         Replace it with a real directory or file, or remove it, and run the command again",
        path.display()
    ))
}

/// `path` is a real directory, or does not exist; a symbolic link, or
/// anything else that is not a directory, is refused.
fn check_real_dir(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(symlink_refusal(path)),
        Ok(meta) if !meta.is_dir() => Err(Error::InvalidMemory(format!(
            "{} is not a directory; move it away and run the command again",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

/// `dir` and everything under it holds no symbolic link (a missing `dir`
/// is fine). Walks with `symlink_metadata`, so it never leaves `dir`.
fn check_tree(dir: &Path) -> Result<(), Error> {
    check_real_dir(dir)?;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(here) = pending.pop() {
        let listing = match std::fs::read_dir(&here) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in listing.flatten() {
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(symlink_refusal(&entry.path()));
            }
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

/// A long correction the knowledge extractor set aside
/// ([`crate::Distilled::drafts`]): a file under [`DRAFTS`] of the worktree
/// for a person to read, edit and add with `by memory add --from`. Never
/// a bundle.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDraft {
    /// The file's name without `.md`: the title's slug.
    pub slug: String,
    /// The first [`TITLE_MAX`] characters of the text, cut at a word.
    pub title: String,
    /// The whole correction, which was over the knowledge limit.
    pub text: String,
    /// The path glob the correction was about, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The branch it came from.
    pub branch: String,
    /// The turn it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// `send`, `steer`, `review` or `pull_request`.
    pub via: String,
}

impl MemoryDraft {
    /// A draft of `text` from `branch`.
    pub fn new(
        text: &str,
        path: Option<String>,
        branch: &str,
        turn: Option<u32>,
        via: &str,
    ) -> MemoryDraft {
        let text = text.trim();
        let first_line = text
            .lines()
            .next()
            .unwrap_or("")
            .trim_end_matches(['.', ':']);
        let title = if first_line.chars().count() <= TITLE_MAX {
            first_line.to_owned()
        } else {
            let cut: String = first_line.chars().take(TITLE_MAX).collect();
            let cut = match cut.rfind(' ') {
                Some(at) if at > TITLE_MAX / 2 => cut[..at].to_owned(),
                _ => cut,
            };
            cut.trim_end_matches([':', '.', ',', ';']).to_owned()
        };
        MemoryDraft {
            slug: match slug(&title) {
                s if s.is_empty() => "draft".into(),
                s => s,
            },
            title,
            text: text.to_owned(),
            path,
            branch: branch.to_owned(),
            turn,
            via: via.to_owned(),
        }
    }

    /// The draft's file: frontmatter a person edits, then the text.
    pub fn render(&self) -> String {
        let mut out = format!("---\ntitle: {}\nkind: rule\n", self.title);
        if let Some(path) = &self.path {
            out.push_str(&format!("paths: [{path}]\n"));
        }
        out.push_str(&format!(
            "source: {} turn {} ({})\n---\n\n{}\n",
            self.branch,
            self.turn.map_or("?".to_owned(), |t| t.to_string()),
            self.via,
            self.text
        ));
        out
    }
}

/// What a draft file says, for `by memory add --from`: its title, kind and
/// paths from the frontmatter, and its text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftFile {
    /// `title:`.
    pub title: Option<String>,
    /// `kind:`.
    pub kind: Option<MemoryKind>,
    /// `paths:`, a flow list.
    pub paths: Vec<String>,
    /// The text after the frontmatter; the whole file without one.
    pub body: String,
}

/// Parse a draft file, or any Markdown file: frontmatter between `---`
/// lines when present (`title`, `kind`, `paths`; other keys skipped), then
/// the body. A file whose first line is a `# ` heading and that has no
/// frontmatter takes the heading as its title.
pub fn parse_draft(text: &str) -> Result<DraftFile, Error> {
    let mut draft = DraftFile::default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---\n") else {
        let body = text.trim();
        if let Some((first, rest)) = body.split_once('\n') {
            if let Some(title) = first.strip_prefix("# ") {
                draft.title = Some(title.trim().to_owned());
                draft.body = rest.trim().to_owned();
                return Ok(draft);
            }
        }
        draft.body = body.to_owned();
        return Ok(draft);
    };
    let Some((front, body)) = rest
        .split_once("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|front| (front, "")))
    else {
        return Err(Error::InvalidMemory(
            "the draft's frontmatter has no closing --- line".into(),
        ));
    };
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "title" => draft.title = Some(value.trim_matches('"').to_owned()),
            "kind" if !value.is_empty() => draft.kind = Some(value.parse()?),
            "paths" => {
                draft.paths = value
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|p| p.trim().trim_matches('"').to_owned())
                    .filter(|p| !p.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    draft.body = body.trim().to_owned();
    Ok(draft)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bundle_tests {
    use super::*;

    fn memory(topic: &str, id: &str, kind: MemoryKind) -> Memory {
        Memory {
            topic: topic.into(),
            id: id.into(),
            title: format!("Title of {id}"),
            kind,
            body: format!("Body of {id}.\n\nSecond paragraph."),
            paths: vec!["crates/parser/**".into()],
            task_kinds: vec![TaskKind::Tests],
            approved_by: "ann".into(),
            approved_ms: 1_762_500_000_000,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn slugs_topics_and_kinds_read_as_people_write_them() {
        assert_eq!(
            slug("Run the parser benches with the 1 MB fixture!"),
            "run-the-parser-benches-with-the-1-mb-fixture"
        );
        assert_eq!(slug("  --Weird__ spacing--  "), "weird-spacing");
        assert_eq!(slug(""), "");
        assert!(slug(&"word ".repeat(40)).len() <= SLUG_MAX);
        assert_eq!(topic_of(&["crates/parser/**".into()]), "parser");
        assert_eq!(topic_of(&["src/lib.rs".into()]), "lib-rs");
        assert_eq!(topic_of(&["**/*.md".into()]), DEFAULT_TOPIC);
        assert_eq!(topic_of(&[]), DEFAULT_TOPIC);
        assert_eq!(
            "pitfall".parse::<MemoryKind>().unwrap(),
            MemoryKind::Pitfall
        );
        assert!(matches!(
            "habit".parse::<MemoryKind>(),
            Err(Error::InvalidMemory(_))
        ));
        assert!(check_topic("parser").is_ok());
        assert!(check_topic("Parser").is_err());
        assert!(check_topic(&"a".repeat(55)).is_err());
        assert_eq!(date(1_762_500_000_000), "2025-11-07");
    }

    #[test]
    fn a_reference_round_trips_through_its_text() {
        let mut m = memory("parser", "bench-fixture", MemoryKind::Procedure);
        m.evidence = vec![
            "b1 turn 2 (correction)".into(),
            "b2 turn 1 (check_failed)".into(),
        ];
        let text = reference(&m);
        assert!(text.starts_with("# Title of bench-fixture\n\nBody of bench-fixture.\n\nSecond paragraph.\n\nApproved by ann on 2025-11-07.\nEvidence: b1 turn 2 (correction), b2 turn 1 (check_failed).\n"));
        let entry = Entry {
            id: m.id.clone(),
            title: m.title.clone(),
            kind: m.kind,
            paths: m.paths.clone(),
            task_kinds: m.task_kinds.clone(),
            approved_by: m.approved_by.clone(),
            approved_ms: m.approved_ms,
        };
        assert_eq!(parse_reference(&text, &entry, "parser").unwrap(), m);
        let refused = |text: &str, needle: &str| {
            let why = parse_reference(text, &entry, "parser")
                .unwrap_err()
                .to_string();
            assert!(why.contains(needle), "{why:?} lacks {needle:?}");
        };
        refused("no heading\n", "does not open with its title");
        refused("# Other\n\nx\n\nApproved by a on d.\n", "memory.json says");
        refused("# Title of bench-fixture\n\nx\n", "Approved by");
    }

    #[test]
    fn the_skill_md_describes_counts_keywords_and_groups_by_kind() {
        let memories = vec![
            memory("parser", "b-proc", MemoryKind::Procedure),
            memory("parser", "a-rule", MemoryKind::Rule),
            memory("parser", "c-pit", MemoryKind::Pitfall),
        ];
        let [bundle] = bundles("parser", &memories, Audience::Team)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(bundle.name, "memory-parser");
        let paths: Vec<&str> = bundle.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "SKILL.md",
                "memory.json",
                "references/a-rule.md",
                "references/b-proc.md",
                "references/c-pit.md"
            ]
        );
        let md = &bundle.files[0].1;
        assert!(md.starts_with("---\nname: memory-parser\ndescription: \"What this team learned about parser: 1 rule, 1 procedure, 1 pitfall; found by search, approved by people.\"\nkeywords: [title, rule, proc, pit]\n---\n"), "{md}");
        let rules = md
            .find("## Rules\n\n- Title of a-rule (references/a-rule.md)")
            .unwrap();
        let procedures = md.find("## Procedures\n\n- Title of b-proc").unwrap();
        let pitfalls = md.find("## Pitfalls\n\n- Title of c-pit").unwrap();
        assert!(rules < procedures && procedures < pitfalls);
        assert!(!md.contains("Continued in"));
        assert!(!md.contains("## Facts"));
        let front = skillset::frontmatter(md).unwrap();
        assert_eq!(front.name, "memory-parser");
        let manifest: Manifest = serde_json::from_str(&bundle.files[1].1).unwrap();
        assert_eq!(manifest.schema, SCHEMA);
        assert_eq!(manifest.topic, "parser");
        assert_eq!(
            manifest
                .memories
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["a-rule", "b-proc", "c-pit"]
        );
        assert_eq!(manifest.memories[0].task_kinds, [TaskKind::Tests]);
        let mine = bundles("parser", &memories, Audience::Person).unwrap();
        assert!(mine[0].files[0].1.contains("What you learned about parser: 1 rule, 1 procedure, 1 pitfall; found by search, approved by you."));
        assert_ne!(mine[0].digest, bundle.digest);
    }

    #[test]
    fn checks_name_what_is_wrong() {
        let bad = |edit: fn(&mut Memory), needle: &str| {
            let mut m = memory("parser", "x", MemoryKind::Fact);
            edit(&mut m);
            let why = bundles("parser", &[m], Audience::Team)
                .unwrap_err()
                .to_string();
            assert!(why.contains(needle), "{why:?} lacks {needle:?}");
        };
        bad(|m| m.title = String::new(), "needs a title");
        bad(|m| m.title = "a\nb".into(), "one line");
        bad(|m| m.title = "t".repeat(81), "at most 80");
        bad(|m| m.body = " ".into(), "empty body");
        bad(|m| m.body = "b".repeat(8001), "at most 8000");
        bad(|m| m.body = "Approved by me on a day.".into(), "footer");
        bad(|m| m.id = "Not A Slug".into(), "is not a slug");
        bad(|m| m.topic = "other".into(), "belongs to topic other");
        let two = vec![
            memory("parser", "x", MemoryKind::Fact),
            memory("parser", "x", MemoryKind::Rule),
        ];
        assert!(bundles("parser", &two, Audience::Team)
            .unwrap_err()
            .to_string()
            .contains("have the slug x"));
        assert!(bundles("Bad Topic", &[], Audience::Team).is_err());
        assert_eq!(
            bundles("empty", &[], Audience::Team).unwrap()[0]
                .files
                .len(),
            2
        );
    }

    #[test]
    fn drafts_render_and_parse_back() {
        let long = format!(
            "Always run the parser benches with the 1 MB fixture: {}",
            "x ".repeat(400)
        );
        let draft = MemoryDraft::new(
            &long,
            Some("crates/parser/**".into()),
            "fix-bench",
            Some(2),
            "steer",
        );
        assert_eq!(
            draft.title,
            "Always run the parser benches with the 1 MB fixture: x x x x x x x x x x x x x"
        );
        assert_eq!(draft.slug, "always-run-the-parser-benches-with-the-1-mb");
        let text = draft.render();
        assert!(text.starts_with("---\ntitle: Always run the parser benches with the 1 MB fixture: x x x x x x x x x x x x x\nkind: rule\npaths: [crates/parser/**]\nsource: fix-bench turn 2 (steer)\n---\n\nAlways run"));
        let parsed = parse_draft(&text).unwrap();
        assert_eq!(parsed.title.as_deref(), Some(draft.title.as_str()));
        assert_eq!(parsed.kind, Some(MemoryKind::Rule));
        assert_eq!(parsed.paths, ["crates/parser/**"]);
        assert_eq!(parsed.body, long.trim());
        let plain = parse_draft("# A heading\n\nbody here\n").unwrap();
        assert_eq!(plain.title.as_deref(), Some("A heading"));
        assert_eq!(plain.body, "body here");
        let bare = parse_draft("just text\n").unwrap();
        assert_eq!((bare.title, bare.body.as_str()), (None, "just text"));
        assert!(parse_draft("---\ntitle: open\n").is_err());
        assert!(parse_draft("---\nkind: habit\n---\nx\n").is_err());
        let short = MemoryDraft::new("Short.", None, "b", None, "send");
        assert_eq!(short.title, "Short");
        assert!(short.render().contains("source: b turn ? (send)"));
    }
}
