//! `by skills ls|trust|untrust|add|remove`, and the decisions every new
//! local branch takes about its repository's skills and your own. See
//! docs/knowledge-skills.md.
//!
//! `ls` shows the built-in skills, the repository's
//! (`.agents/skills/<name>/SKILL.md`) at HEAD with whether you trusted
//! them, and your own (`~/.config/branchyard/skills/<name>/`, the local
//! `user` source) with any withheld by name; with a branch, or inside a
//! harness for its own, the set fixed when that branch was created, staged
//! or withheld and why. `trust` records the set digest at HEAD; `untrust`
//! forgets it. `add` copies a skill directory into your folder and `remove`
//! deletes one; a harness on a branch can do neither.
//!
//! Trust is per user, per repository root and per set digest, kept in the
//! trust file `by workspace trust` uses (`trusted-workspaces.json`, or
//! `BRANCHYARD_TRUST_FILE`), in its `skills` map. A new branch stages the
//! repository's skills at its base commit only when their set digest is
//! the one trusted ([`gate`]); a change withholds them until trusted again.
//! A harness on a branch (`BRANCHYARD_BRANCH` set) can never trust them,
//! and a server stages them only when its operator allows them
//! (`allow_repository_skills`), so `--remote` is refused.

use std::path::{Path, PathBuf};

use branchyard::skills::{self, Found, RepositorySet, UserSet, REPOSITORY_SKILLS};
use branchyard::skills::{SkillEntry, Source};
use branchyard::{BranchSkills, SkillGate, Yard};
use branchyard_workspace::Git;
use serde::Serialize;
use serde_json::{json, Value};

use crate::args::SkillsAction;
use crate::commands::{self, print, Failure, Outcome, Target};
use crate::errors::Report;
use crate::setup_cmd::plural;
use crate::workspace_cmd::{self, in_harness, TrustSection};

/// What a person decided about the repository's skills at HEAD.
enum Trust {
    Trusted,
    Untrusted,
    /// Trusted once, as set `trusted`, and changed since.
    Changed {
        trusted: String,
    },
}

/// The repository's skills at HEAD, and what was decided about them.
enum AtHead {
    /// No skill directory.
    Nothing,
    /// Skills that cannot be used, and why.
    Refused(String),
    Skills {
        set: RepositorySet,
        trust: Trust,
    },
}

/// One skill as `ls` lists it; its description only when read from a
/// commit, as a branch's record keeps none.
#[derive(Serialize)]
struct Listed {
    label: String,
    name: String,
    digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl Listed {
    fn new(entry: &SkillEntry, description: Option<&str>) -> Listed {
        Listed {
            label: entry.label(),
            name: entry.name.clone(),
            digest: entry.digest.clone(),
            description: description.map(str::to_owned),
        }
    }

    fn at_head(set: &RepositorySet) -> Vec<Listed> {
        set.skills
            .iter()
            .map(|skill| Listed::new(&skill.entry(Source::Repository), Some(&skill.description)))
            .collect()
    }

    fn fixed(skills: &BranchSkills) -> Vec<Listed> {
        skills
            .entries
            .iter()
            .map(|e| Listed::new(e, None))
            .collect()
    }
}

/// The trust file's key for the repository at `root`.
fn key(root: &Path) -> String {
    workspace_cmd::canonical(root).display().to_string()
}

/// The first 12 characters of a digest or commit.
fn short(text: &str) -> &str {
    text.get(..12).unwrap_or(text)
}

/// Your own skills folder: `skills/` beside the user configuration file
/// (`~/.config/branchyard/skills/`).
pub(crate) fn user_skills_dir() -> PathBuf {
    skills::user_dir(&crate::setup_io::user_file())
}

/// The folder a branch created here reads your own skills from: yours,
/// outside any branch; none for a harness on a branch, whose children
/// inherit their parent's.
pub(crate) fn user_skills_for_new_branch() -> Option<PathBuf> {
    match in_harness() {
        Some(_) => None,
        None => Some(user_skills_dir()),
    }
}

fn at_head(root: &Path) -> Result<AtHead, Failure> {
    Ok(match skills::read(root, "HEAD")? {
        Found::Nothing => AtHead::Nothing,
        Found::Refused(why) => AtHead::Refused(why),
        Found::Skills(set) => {
            let trust = match workspace_cmd::trusted_digest(TrustSection::Skills, &key(root))? {
                Some(digest) if digest == set.digest => Trust::Trusted,
                Some(digest) => Trust::Changed { trusted: digest },
                None => Trust::Untrusted,
            };
            AtHead::Skills { set, trust }
        }
    })
}

/// Whether a new branch of the repository at `root` stages its skills:
/// the set digest you trusted, or withheld and why. A harness can never
/// trust them, and a trust file that cannot be read withholds them rather
/// than stopping the run (`by skills ls` reports it).
pub(crate) fn gate(root: &Path) -> SkillGate {
    if in_harness().is_some() {
        return SkillGate::Withheld(
            "a harness running on a branch cannot trust repository skills; a person trusts \
             them with by skills trust, outside any branch"
                .into(),
        );
    }
    match workspace_cmd::trusted_digest(TrustSection::Skills, &key(root)) {
        Ok(Some(digest)) => SkillGate::Trusted(digest),
        Ok(None) => SkillGate::Withheld(
            "you have not trusted this repository's skills; review them with by skills ls, \
             then run by skills trust"
                .into(),
        ),
        Err(failure) => SkillGate::Withheld(format!(
            "your trust decisions could not be read ({failure}); by skills ls says why"
        )),
    }
}

/// `by skills ACTION`.
pub fn main(target: &Target, action: &SkillsAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Report::usage(
            "by skills reads a local repository's skills and your trust decisions; a server \
             stages a repository's skills only when its operator allows them \
             (allow_repository_skills)",
        )
        .next("run by skills without --remote")
        .into());
    }
    match action {
        SkillsAction::Ls { branch } => {
            let yard = commands::open()?;
            match branch.clone().or_else(in_harness) {
                Some(branch) => ls_branch(&yard, &branch, json),
                None => ls_here(&yard, json),
            }
        }
        SkillsAction::Trust => trust(json),
        SkillsAction::Untrust => untrust(json),
        SkillsAction::Add { path, name } => add(path, name.as_deref(), json),
        SkillsAction::Remove { name } => remove(name, json),
    }
}

/// `by skills add PATH [--name NAME]`: copy the skill directory into your
/// folder, replacing one of the same name.
fn add(path: &Path, name: Option<&str>, json: bool) -> Outcome {
    refuse_in_harness("add a skill to your folder")?;
    let skill_md = path.join("SKILL.md");
    let text = std::fs::read_to_string(&skill_md).map_err(|e| {
        Report::usage(format!(
            "{} is not a skill directory: {}: {e}",
            path.display(),
            skill_md.display()
        ))
        .next("point at the directory holding SKILL.md")
    })?;
    let front = skills::frontmatter(&text).map_err(|why| {
        Report::usage(format!("{}: SKILL.md {why}", path.display()))
            .next("give SKILL.md frontmatter with a name and a description")
    })?;
    let name = name.unwrap_or(&front.name);
    if skills::reserved(name) {
        return Err(Report::usage(format!(
            "{name} is a built-in skill's name, which is reserved"
        ))
        .next("install it under another name with --name")
        .into());
    }
    let skill = match name == front.name {
        true => skills::read_directory(name, path),
        // Installed under another name: the frontmatter is rewritten to it.
        false => skills::read_directory(&front.name, path).and_then(|skill| {
            let files = skill
                .files
                .into_iter()
                .map(|(file, content)| match file.as_str() {
                    "SKILL.md" => (file, rename_frontmatter(&content, &front.name, name)),
                    _ => (file, content),
                })
                .collect();
            skills::Skill::new(name, files)
        }),
    }
    .map_err(|why| Report::usage(why).next("fix the skill, then by skills add again"))?;
    let dir = user_skills_dir();
    let target = dir.join(&skill.name);
    let replaced = target.exists();
    let fail = |what: &str, e: std::io::Error| {
        Report::new("io", format!("could not {what} {}: {e}", target.display()))
            .next("check the folder's permissions")
    };
    if replaced {
        std::fs::remove_dir_all(&target).map_err(|e| fail("replace", e))?;
    }
    for (file, content) in &skill.files {
        let file = target.join(file);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| fail("create", e))?;
        }
        std::fs::write(&file, content).map_err(|e| fail("write", e))?;
    }
    let entry = skill.entry(Source::User);
    if json {
        return print(&to_json(&json!({
            "name": skill.name,
            "label": entry.label(),
            "digest": skill.digest,
            "description": skill.description,
            "dir": target,
            "replaced": replaced,
        })));
    }
    print(&format!(
        "{} {} ({}) in {}: {}\nnew branches you start here stage it, after the repository's \
         skills and before the built-in ones\n",
        if replaced { "replaced" } else { "added" },
        entry.label(),
        short(&skill.digest),
        target.display(),
        skill.description
    ))
}

/// `text`, a `SKILL.md`, with its frontmatter `name: from` line naming `to`.
fn rename_frontmatter(text: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut in_front = false;
    let mut renamed = false;
    for (at, line) in text.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_end();
        if at == 0 && trimmed == "---" {
            in_front = true;
        } else if in_front && matches!(trimmed, "---" | "...") {
            in_front = false;
        } else if in_front && !renamed {
            let value = line.strip_prefix("name:").map(str::trim);
            if value.is_some_and(|v| v.trim_matches(['"', '\'']) == from) {
                out.push_str(&format!("name: {to}\n"));
                renamed = true;
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// `by skills remove NAME`: delete the skill from your folder.
fn remove(name: &str, json: bool) -> Outcome {
    refuse_in_harness("remove a skill from your folder")?;
    let dir = user_skills_dir();
    if let Err(why) = skills::Skill::new(name, Vec::new()) {
        if why.contains("is not a skill name") {
            return Err(Report::usage(why)
                .next("by skills ls lists your skills under user")
                .into());
        }
    }
    let target = dir.join(name);
    if !target.join("SKILL.md").is_file() {
        return Err(
            Report::usage(format!("no skill {name} in {}", dir.display()))
                .next("by skills ls lists your skills under user")
                .into(),
        );
    }
    std::fs::remove_dir_all(&target).map_err(|e| {
        Report::new("io", format!("could not remove {}: {e}", target.display()))
            .next("check the folder's permissions")
    })?;
    if json {
        return print(&to_json(
            &json!({ "name": name, "dir": target, "removed": true }),
        ));
    }
    print(&format!(
        "removed user:{name} from {}; new branches you start here no longer stage it\n",
        dir.display()
    ))
}

/// A harness on a branch decides nothing about what a turn may see.
fn refuse_in_harness(what: &str) -> Result<(), Failure> {
    match in_harness() {
        Some(_) => Err(Report::new(
            "denied",
            format!(
                "a harness running on a branch cannot {what}; what a turn may see is decided by \
                 people, outside any branch"
            ),
        )
        .next("run the command in your own terminal")
        .into()),
        None => Ok(()),
    }
}

/// Your own skills as a new branch here would read them, for `ls`.
fn user_here(json: bool, dir: &Path) -> (Value, String) {
    let set: UserSet = skills::read_user(dir);
    let listed: Vec<Listed> = set
        .skills
        .iter()
        .map(|skill| Listed::new(&skill.entry(Source::User), Some(&skill.description)))
        .collect();
    let value = match json {
        true => json!({ "dir": dir, "skills": listed, "withheld": set.withheld }),
        false => Value::Null,
    };
    let mut text = match listed.is_empty() && set.withheld.is_empty() {
        true => format!(
            "user        none: no {}/<name>/SKILL.md (by skills add PATH, by memory add --mine)\n",
            dir.display()
        ),
        false => format!(
            "user        {} in {}\n",
            plural(listed.len(), "skill"),
            dir.display()
        ),
    };
    text.push_str(&skill_lines(&listed));
    for withheld in &set.withheld {
        text.push_str(&format!(
            "  withheld {}: {}\n",
            withheld.name, withheld.reason
        ));
    }
    (value, text)
}

fn to_json(value: &Value) -> String {
    format!("{value:#}\n")
}

/// Say on standard error when `.agents/skills` has changes not committed:
/// only committed skills count. Whether it has.
fn note_uncommitted(root: &Path) -> Result<bool, Failure> {
    let status = Git::new(root)
        .args(["status", "--porcelain", "--", REPOSITORY_SKILLS])
        .run()
        .map_err(|e| {
            Report::new(
                "git",
                format!("could not read the status of {REPOSITORY_SKILLS}: {e}"),
            )
        })?;
    let uncommitted = !status.trim().is_empty();
    if uncommitted {
        eprintln!("by: {REPOSITORY_SKILLS} has uncommitted changes; only committed skills count");
    }
    Ok(uncommitted)
}

/// The built-in skills' line: how many, and their names.
fn built_in_line(built_in: &[SkillEntry]) -> String {
    let names: Vec<&str> = built_in.iter().map(|e| e.name.as_str()).collect();
    format!(
        "built-in    {} skills: {}\n",
        built_in.len(),
        names.join(", ")
    )
}

/// One indented line per skill: its label, short digest and, when known,
/// its description.
fn skill_lines(skills: &[Listed]) -> String {
    let width = skills.iter().map(|s| s.label.len()).max().unwrap_or(0);
    skills
        .iter()
        .map(|s| {
            let line = format!("  {:<width$}  {}", s.label, short(&s.digest));
            match &s.description {
                Some(text) => format!("{line}  {text}\n"),
                None => format!("{line}\n"),
            }
        })
        .collect()
}

fn ls_here(yard: &Yard, json: bool) -> Outcome {
    let root = workspace_cmd::canonical(yard.root());
    let uncommitted = note_uncommitted(&root)?;
    let built_in = skills::built_in()?;
    let head = at_head(&root)?;
    let trust_file = workspace_cmd::trust_file();
    let user_dir = user_skills_dir();
    let (user_json, user_text) = user_here(json, &user_dir);
    if json {
        let repository = match &head {
            AtHead::Nothing => json!({ "state": "none", "skills": [] }),
            AtHead::Refused(reason) => {
                json!({ "state": "refused", "reason": reason, "skills": [] })
            }
            AtHead::Skills { set, trust } => {
                let (state, trusted_set) = match trust {
                    Trust::Trusted => ("trusted", None),
                    Trust::Untrusted => ("untrusted", None),
                    Trust::Changed { trusted } => ("changed", Some(trusted)),
                };
                let mut value = json!({
                    "state": state,
                    "commit": set.commit,
                    "set": set.digest,
                    "skills": Listed::at_head(set),
                });
                if let Some(trusted) = trusted_set {
                    value["trusted_set"] = json!(trusted);
                }
                value
            }
        };
        return print(&to_json(&json!({
            "root": root,
            "built_in": built_in,
            "repository": repository,
            "user": user_json,
            "uncommitted": uncommitted,
            "trust_file": trust_file,
        })));
    }
    let mut text = format!("root        {}\n", root.display());
    text.push_str(&built_in_line(&built_in));
    text.push_str(&user_text);
    match &head {
        AtHead::Nothing => text.push_str(&format!(
            "repository  none: no {REPOSITORY_SKILLS}/<name>/SKILL.md at HEAD \
             (docs/knowledge-skills.md)\n"
        )),
        AtHead::Refused(reason) => text.push_str(&format!(
            "repository  refused: {reason}; fix it and commit, then by skills trust\n"
        )),
        AtHead::Skills { set, trust } => {
            text.push_str(&format!(
                "repository  {} at commit {} in {REPOSITORY_SKILLS}, set {}\n",
                plural(set.skills.len(), "skill"),
                short(&set.commit),
                short(&set.digest)
            ));
            text.push_str(&skill_lines(&Listed::at_head(set)));
            let state = match trust {
                Trust::Trusted => format!(
                    "trusted ({}): new branches here stage them",
                    trust_file.display()
                ),
                Trust::Untrusted => "not trusted: new branches here withhold them; review them, \
                                     then by skills trust"
                    .to_owned(),
                Trust::Changed { trusted } => format!(
                    "changed since you trusted set {}: new branches here withhold them; review \
                     them, then by skills trust",
                    short(trusted)
                ),
            };
            text.push_str(&format!("trust       {state}\n"));
        }
    }
    print(&text)
}

fn ls_branch(yard: &Yard, branch: &str, json: bool) -> Outcome {
    let fixed = yard.branch_skills(branch)?;
    let built_in = skills::built_in()?;
    if json {
        let repository = match &fixed {
            None => json!({ "state": "none", "skills": [] }),
            Some(fixed) => {
                let mut value = json!({
                    "state": if fixed.withheld.is_some() { "withheld" } else { "staged" },
                    "commit": fixed.commit,
                    "set": fixed.digest,
                    "skills": Listed::fixed(fixed),
                });
                if let Some(reason) = &fixed.withheld {
                    value["reason"] = json!(reason);
                }
                value
            }
        };
        return print(&to_json(&json!({
            "branch": branch,
            "built_in": built_in,
            "repository": repository,
        })));
    }
    let mut text = format!("branch      {branch}\n");
    text.push_str(&built_in_line(&built_in));
    match &fixed {
        None => text.push_str(&format!(
            "repository  none: the commit it started from has no {REPOSITORY_SKILLS}\n"
        )),
        Some(fixed) => text.push_str(&branch_set(fixed)),
    }
    print(&text)
}

/// The text of a branch's repository skills.
fn branch_set(fixed: &BranchSkills) -> String {
    let mut text = format!(
        "repository  {} from commit {}, set {}, fixed when the branch was created\n",
        plural(fixed.entries.len(), "skill"),
        short(&fixed.commit),
        short(&fixed.digest)
    );
    text.push_str(&skill_lines(&Listed::fixed(fixed)));
    match &fixed.withheld {
        None => text.push_str("state       staged\n"),
        Some(reason) => text.push_str(&format!("state       withheld: {reason}\n")),
    }
    text
}

fn trust(json: bool) -> Outcome {
    if in_harness().is_some() {
        return Err(Report::new(
            "denied",
            "a harness running on a branch cannot trust repository skills; trusting is a \
             person's decision, made outside any branch",
        )
        .next("run by skills trust in your own terminal")
        .into());
    }
    let root = workspace_cmd::canonical(commands::open()?.root());
    note_uncommitted(&root)?;
    let set = match at_head(&root)? {
        AtHead::Nothing => {
            return Err(Report::new(
                "config",
                format!("{} has no skills to trust at HEAD", root.display()),
            )
            .next(format!(
                "add {REPOSITORY_SKILLS}/<name>/SKILL.md and commit it (docs/knowledge-skills.md)"
            ))
            .into())
        }
        AtHead::Refused(reason) => {
            return Err(Report::new("config", reason)
                .next("fix it and commit, then by skills trust")
                .into())
        }
        AtHead::Skills { set, .. } => set,
    };
    workspace_cmd::record_trust(TrustSection::Skills, &key(&root), &set.digest)?;
    let labels: Vec<String> = set.entries().iter().map(SkillEntry::label).collect();
    if json {
        return print(&to_json(&json!({
            "root": root,
            "commit": set.commit,
            "set": set.digest,
            "skills": labels,
            "trusted": true,
            "trust_file": workspace_cmd::trust_file(),
        })));
    }
    print(&format!(
        "trusted {} at commit {} (set {}): {}\nnew branches of {} stage them; after a change, run \
         by skills trust again\n",
        plural(labels.len(), "repository skill"),
        short(&set.commit),
        short(&set.digest),
        labels.join(", "),
        root.display()
    ))
}

fn untrust(json: bool) -> Outcome {
    let root = workspace_cmd::canonical(commands::open()?.root());
    let removed = workspace_cmd::forget_trust(TrustSection::Skills, &key(&root))?;
    if json {
        return print(&to_json(&json!({ "root": root, "removed": removed })));
    }
    match removed {
        true => print(&format!(
            "{}'s repository skills are no longer trusted; new branches withhold them\n",
            root.display()
        )),
        false => print(&format!(
            "{}'s repository skills were not trusted\n",
            root.display()
        )),
    }
}
