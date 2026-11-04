//! Deciding one permission request, and the named permission presets.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DecisionSource, Fallback, PermissionDecision, PermissionRequest, Policy, Rule};

pub(crate) fn decide(
    policy: &Policy,
    branch: &str,
    request: &PermissionRequest,
    root: Option<&Path>,
) -> (PermissionDecision, DecisionSource) {
    let matches = |rule: &Rule| {
        let (name, spec) = split_rule(&rule.tool);
        let tool = match name.strip_suffix('*') {
            Some(prefix) => request.tool.starts_with(prefix),
            None => request.tool == name,
        };
        tool && spec.is_none_or(|spec| input_matches(spec, &request.input, rule.allow, root))
            && rule
                .delegation_by
                .as_deref()
                .is_none_or(|by| is_delegation_command(&request.input, by))
    };
    let deny = || PermissionDecision::Deny {
        message: "Denied by Branchyard policy.".into(),
    };
    match policy.rules.iter().find(|rule| matches(rule)) {
        Some(rule) => {
            let decision = if rule.allow {
                PermissionDecision::Allow
            } else {
                deny()
            };
            let pattern = match &rule.delegation_by {
                Some(by) => format!("delegation commands of {}", by.display()),
                None => rule.tool.clone(),
            };
            (decision, DecisionSource::Rule { pattern })
        }
        None => match &policy.fallback {
            Fallback::Allow => (PermissionDecision::Allow, DecisionSource::Default),
            Fallback::Deny => (deny(), DecisionSource::Default),
            Fallback::Ask(ask) => (ask(branch, request), DecisionSource::Asked),
        },
    }
}

/// A rule's tool and, for `Tool(spec)`, what it says of the call's input:
/// `Bash(git *)`, `Edit(src/**)`, `WebFetch(domain:example.com)`.
pub(crate) fn split_rule(tool: &str) -> (&str, Option<&str>) {
    match tool.strip_suffix(')').and_then(|t| t.split_once('(')) {
        Some((name, spec)) if !name.is_empty() && !name.contains(['(', ')']) => (name, Some(spec)),
        _ => (tool, None),
    }
}

/// Whether a call's `input` is what `spec` names: a shell command's
/// (`command`), a file's (`file_path`, `path`, `notebook_path`, `filePath`),
/// or a URL's (`url`). A call that carries none of them does not match.
fn input_matches(spec: &str, input: &Value, allow: bool, root: Option<&Path>) -> bool {
    if let Some(command) = input["command"].as_str() {
        return command_matches(spec, command, allow);
    }
    let path = ["file_path", "path", "notebook_path", "filePath"]
        .iter()
        .find_map(|key| input[*key].as_str());
    if let Some(path) = path {
        return path_matches(spec, path, root);
    }
    if let Some(url) = input["url"].as_str() {
        return url_matches(spec, url);
    }
    false
}

/// A shell command against `spec`, a glob over the whole command (`git *`
/// also matches `git` alone; `npm run test:*`, Claude Code's prefix form,
/// is `npm run test` and anything after it). A compound command is split on
/// `&&`, `||`, `;`, `|` and newlines: an allowing rule matches only when
/// every part does and the command substitutes nothing (`$(`, a backtick),
/// so `git status && rm -rf x` is not allowed by `Bash(git *)`; a denying
/// rule matches when any part does.
fn command_matches(spec: &str, command: &str, allow: bool) -> bool {
    let spec = match spec.strip_suffix(":*") {
        Some(prefix) => format!("{prefix} *"),
        None => spec.to_owned(),
    };
    let one = |part: &str| {
        let part = part.trim();
        glob(&spec, part, false)
            || spec
                .strip_suffix(" *")
                .is_some_and(|bare| glob(bare, part, false))
    };
    let parts: Vec<&str> = command
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    match allow {
        true => {
            !command.contains("$(")
                && !command.contains('`')
                && !parts.is_empty()
                && parts.iter().all(|part| one(part))
        }
        false => parts.iter().any(|part| one(part)) || one(command),
    }
}

/// A file path against `spec`, a glob where `*` stays within one path
/// component and `**` crosses them. An absolute `spec` is matched against
/// the absolute path; a relative one against the path relative to `root`
/// (the directory the harness works in), so a path outside it never
/// matches a relative rule. `..` is resolved before matching.
fn path_matches(spec: &str, path: &str, root: Option<&Path>) -> bool {
    let absolute = |p: &Path| match (p.is_absolute(), root) {
        (true, _) => Some(p.to_path_buf()),
        (false, Some(root)) => Some(root.join(p)),
        (false, None) => None,
    };
    let given = Path::new(path);
    match spec.starts_with('/') {
        true => absolute(given)
            .map(|p| lexical(&p))
            .is_some_and(|p| glob(spec, &p.display().to_string(), true)),
        false => match (absolute(given), root) {
            (Some(p), Some(root)) => lexical(&p)
                .strip_prefix(lexical(root))
                .is_ok_and(|rel| glob(spec, &rel.display().to_string(), true)),
            _ => {
                let rel = lexical(given);
                !rel.starts_with("..") && glob(spec, &rel.display().to_string(), true)
            }
        },
    }
}

/// A URL against `spec`: `domain:example.com` matches that host and its
/// subdomains; anything else is a glob over the whole URL.
fn url_matches(spec: &str, url: &str) -> bool {
    match spec.strip_prefix("domain:") {
        Some(domain) => {
            let host = url_host(url);
            let domain = domain.to_ascii_lowercase();
            host == domain || host.ends_with(&format!(".{domain}"))
        }
        None => glob(spec, url, false),
    }
}

/// A URL's host, lower-cased, without credentials or a port.
fn url_host(url: &str) -> String {
    let authority = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host.split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// `path` with `.` dropped and `..` taken back, without touching the disk.
fn lexical(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// A glob: `*` matches any run of characters (within one `/`-separated
/// component when `paths`), `**` any run across them.
fn glob(pattern: &str, text: &str, paths: bool) -> bool {
    fn go(p: &[u8], t: &[u8], paths: bool) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) => {
                let (crosses, rest) = match rest.split_first() {
                    Some((b'*', after)) => (true, after.strip_prefix(b"/").unwrap_or(after)),
                    _ => (!paths, rest),
                };
                (0..=t.len())
                    .any(|i| (crosses || !t[..i].contains(&b'/')) && go(rest, &t[i..], paths))
            }
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..], paths),
        }
    }
    go(pattern.as_bytes(), text.as_bytes(), paths)
}

/// `policy` as data, for a harness that enforces its own permission rules
/// instead of asking Branchyard (Antigravity's CLI): its rules in order and
/// its fallback, with an asking fallback kept as asking, which such a
/// harness cannot do. `by` is the `by` the turn's harness runs, if any.
/// See `docs/provisioning.md#antigravity-permission-rules`.
pub(crate) fn permissions(
    policy: &Policy,
    by: Option<String>,
) -> branchyard_provision::Permissions {
    use branchyard_provision::permissions::{Fallback as Native, PermissionRule};
    branchyard_provision::Permissions {
        rules: policy
            .rules
            .iter()
            .map(|rule| PermissionRule {
                tool: rule.tool.clone(),
                allow: rule.allow,
                delegation_by: rule
                    .delegation_by
                    .as_ref()
                    .map(|by| by.display().to_string()),
            })
            .collect(),
        fallback: match policy.fallback {
            Fallback::Allow => Native::Allow,
            Fallback::Deny => Native::Deny,
            Fallback::Ask(_) => Native::Ask,
        },
        by,
    }
}

/// Tools that change files, by the names the harnesses give them.
pub const EDIT_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "fileChange",
    "write_file",
    "replace",
    "edit",
    // An ACP agent's call by its kind, when it names no tool.
    "acp:edit",
    "acp:delete",
    "acp:move",
];

/// Tools that run commands.
pub const SHELL_TOOLS: &[&str] = &[
    "Bash",
    // Claude Code's Bash call that asks to run outside its sandbox.
    "Bash:unsandboxed",
    "BashOutput",
    "KillBash",
    "KillShell",
    "commandExecution",
    "run_shell_command",
    "shell",
    "exec_command",
    "bash",
    "acp:execute",
];

/// Tools that fetch from or search the web.
pub const WEB_TOOLS: &[&str] = &[
    "WebFetch",
    "WebSearch",
    "web_fetch",
    "google_web_search",
    "acp:fetch",
];

/// A named permission policy that stands for explicit rules: usable
/// wherever explicit rules are (`by run --permissions`, a rig seat's
/// `permission_policy`, a server request's `policy.preset`,
/// `[defaults] permissions`). See `docs/egress.md#permission-presets`.
///
/// A preset's rules name tools only: it says which tools may run, not
/// what they touch. A rule of its own may name both
/// (`docs/egress.md#rules-that-name-arguments-and-paths`).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyPreset {
    /// Read and search; edits, commands and the web denied. The policy a
    /// planning turn runs under ([`crate::read_only_policy`]).
    ReadOnly,
    /// Read, search and edit files; commands and the web denied.
    EditWorktree,
    /// Everything allowed, as `--yes`.
    Full,
}

/// A preset's rules: denials first, then allowances, then the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetRules {
    pub default_allow: bool,
    pub deny: Vec<&'static str>,
    pub allow: Vec<&'static str>,
}

impl PolicyPreset {
    pub const ALL: [PolicyPreset; 3] = [
        PolicyPreset::ReadOnly,
        PolicyPreset::EditWorktree,
        PolicyPreset::Full,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            PolicyPreset::ReadOnly => "read-only",
            PolicyPreset::EditWorktree => "edit-worktree",
            PolicyPreset::Full => "full",
        }
    }

    pub fn parse(text: &str) -> Result<PolicyPreset, String> {
        PolicyPreset::ALL
            .into_iter()
            .find(|p| p.name() == text)
            .ok_or_else(|| {
                format!(
                    "{text:?} is not a permission preset; use {}",
                    PolicyPreset::ALL.map(|p| p.name()).join(", ")
                )
            })
    }

    /// One line for people.
    pub fn summary(&self) -> &'static str {
        match self {
            PolicyPreset::ReadOnly => "read and search; edits, commands and the web denied",
            PolicyPreset::EditWorktree => {
                "read, search and edit files; commands and the web denied"
            }
            PolicyPreset::Full => "every tool allowed",
        }
    }

    /// The explicit rules this preset stands for.
    pub fn rules(&self) -> PresetRules {
        let tools = |lists: &[&[&'static str]]| lists.concat();
        match self {
            PolicyPreset::ReadOnly => PresetRules {
                default_allow: false,
                deny: tools(&[EDIT_TOOLS, SHELL_TOOLS, WEB_TOOLS]),
                allow: tools(&[crate::READ_ONLY_TOOLS]),
            },
            PolicyPreset::EditWorktree => PresetRules {
                default_allow: false,
                deny: tools(&[SHELL_TOOLS, WEB_TOOLS]),
                allow: tools(&[crate::READ_ONLY_TOOLS, EDIT_TOOLS]),
            },
            PolicyPreset::Full => PresetRules {
                default_allow: true,
                deny: Vec::new(),
                allow: Vec::new(),
            },
        }
    }

    /// The policy: the denials, then the allowances, then the default.
    pub fn policy(&self) -> Policy {
        let rules = self.rules();
        let base = match rules.default_allow {
            true => Policy::allow_all(),
            false => Policy::deny_all(),
        };
        let policy = rules.deny.iter().fold(base, |p, tool| p.deny(*tool));
        rules
            .allow
            .iter()
            .fold(policy, |p, tool| p.allow(*tool))
            .with_preset(Some(*self))
    }

    /// The preset's approvals for connector operations, the last layer
    /// they resolve through (`docs/effects.md#approvals`): `read-only` and
    /// `edit-worktree` change nothing outside the machine, so they block
    /// every effectful class; `full` leaves the class defaults.
    pub fn approvals(&self) -> crate::effects::ApprovalPolicy {
        use crate::effects::{Approval, EffectClass};
        let classes = match self {
            PolicyPreset::ReadOnly | PolicyPreset::EditWorktree => [
                EffectClass::Reversible,
                EffectClass::Compensable,
                EffectClass::Irreversible,
            ]
            .into_iter()
            .map(|c| (c, Approval::Block))
            .collect(),
            PolicyPreset::Full => Default::default(),
        };
        crate::effects::ApprovalPolicy {
            classes,
            ..Default::default()
        }
    }
}

impl fmt::Display for PolicyPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Whether `input["command"]` runs `by` with a delegation subcommand and
/// nothing else.
fn is_delegation_command(input: &Value, by: &Path) -> bool {
    let argv = match input.get("command") {
        Some(Value::String(line)) => simple_words(line),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    };
    let Some(mut argv) = argv else {
        return false;
    };
    // `bash -lc '<command>'`, as Codex reports what it runs.
    let shells = ["sh", "bash", "dash", "zsh"];
    let is_shell = argv.first().is_some_and(|program| {
        let name = Path::new(program).file_name().and_then(|n| n.to_str());
        name.is_some_and(|n| shells.contains(&n))
    });
    if is_shell && argv.len() == 3 && matches!(argv[1].as_str(), "-c" | "-lc") {
        match simple_words(&argv[2]) {
            Some(inner) => argv = inner,
            None => return false,
        }
    }
    let program_ok = argv
        .first()
        .is_some_and(|program| program == "by" || Path::new(program) == by);
    // The operations a harness may run on its own branch, from the one
    // table every surface is checked against.
    let subcommand_ok = argv
        .get(1..)
        .is_some_and(crate::operations::is_harness_command);
    program_ok && subcommand_ok
}

/// The words of `line` if it is one simple command of plain, single-quoted
/// or double-quoted words; `None` for anything a shell would expand,
/// redirect, chain or glob.
fn simple_words(line: &str) -> Option<Vec<String>> {
    const SPECIAL: &str = ";&|<>(){}$`\\*?[]#~!";
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\r' => return None,
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        '\n' | '\r' => return None,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' | '!' | '\n' | '\r' => return None,
                        c => word.push(c),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c if SPECIAL.contains(c) => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PermissionKey;

    fn request(tool: &str) -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("1".into()),
            tool: tool.into(),
            input: serde_json::Value::Null,
        }
    }

    /// Each preset's denials are the names its rules list, sorted and
    /// without repeats; `full` denies none.
    #[test]
    fn a_policy_as_data_decides_as_the_policy_does() {
        let decide = |policy: &Policy, tool: &str| permissions(policy, None).decide(tool);
        let read_only = PolicyPreset::ReadOnly.policy();
        assert_eq!(decide(&read_only, "run_command"), Some(false));
        assert_eq!(decide(&read_only, "write_file"), Some(false));
        let worktree = PolicyPreset::EditWorktree.policy();
        assert_eq!(decide(&worktree, "run_command"), Some(false));
        assert_eq!(decide(&worktree, "write_file"), Some(true));
        let full = PolicyPreset::Full.policy();
        assert_eq!(decide(&full, "run_command"), Some(true));
        let asking = Policy::ask(|_, _| PermissionDecision::Allow).allow("Read");
        assert_eq!(decide(&asking, "run_command"), None);
        assert_eq!(decide(&asking, "Read"), Some(true));
        let scoped = Policy::deny_all().allow_delegation_commands("/opt/by/bin/by");
        let data = permissions(&scoped, Some("/opt/by/bin/by".into()));
        assert_eq!(data.delegation_by("run_command"), Some("/opt/by/bin/by"));
        assert_eq!(data.decide("run_command"), Some(false));
        assert_eq!(data.by.as_deref(), Some("/opt/by/bin/by"));
    }

    #[test]
    fn presets_expand_to_explicit_rules() {
        let decide = |preset: PolicyPreset, tool: &str| {
            matches!(
                preset.policy().decide("b", &request(tool)),
                PermissionDecision::Allow
            )
        };
        for tool in ["Read", "Grep", "read_file", "acp:read", "acp:search"] {
            assert!(decide(PolicyPreset::ReadOnly, tool), "{tool}");
            assert!(decide(PolicyPreset::EditWorktree, tool), "{tool}");
        }
        for tool in [
            "Edit",
            "Write",
            "fileChange",
            "write_file",
            "acp:edit",
            "edit",
        ] {
            assert!(!decide(PolicyPreset::ReadOnly, tool), "{tool}");
            assert!(decide(PolicyPreset::EditWorktree, tool), "{tool}");
        }
        for tool in [
            "Bash",
            "commandExecution",
            "WebFetch",
            "mcp__anything",
            "acp:execute",
            "acp:fetch",
            "bash",
        ] {
            assert!(!decide(PolicyPreset::ReadOnly, tool), "{tool}");
            assert!(!decide(PolicyPreset::EditWorktree, tool), "{tool}");
        }
        for tool in ["Bash", "Edit", "mcp__anything"] {
            assert!(decide(PolicyPreset::Full, tool), "{tool}");
        }
        // Read-only allows what a planning turn may do, and no more.
        for tool in ["Read", "Edit", "Bash", "Glob", "TodoWrite", "other"] {
            assert_eq!(
                decide(PolicyPreset::ReadOnly, tool),
                matches!(
                    crate::read_only_policy().decide("b", &request(tool)),
                    PermissionDecision::Allow
                ),
                "{tool}"
            );
        }
        // Names round-trip, in serde too.
        for preset in PolicyPreset::ALL {
            assert_eq!(PolicyPreset::parse(preset.name()), Ok(preset));
            let json = serde_json::to_string(&preset).unwrap();
            assert_eq!(json, format!("\"{}\"", preset.name()));
        }
        let error = PolicyPreset::parse("builtin:yolo").unwrap_err();
        assert!(error.contains("read-only, edit-worktree, full"), "{error}");
    }

    #[test]
    fn the_first_matching_rule_decides_and_is_named() {
        let policy = Policy::deny_all().deny("mcp__secret").allow("mcp__*");
        assert_eq!(
            policy.decide_with_source("b", &request("mcp__secret")),
            (
                PermissionDecision::Deny {
                    message: "Denied by Branchyard policy.".into()
                },
                DecisionSource::Rule {
                    pattern: "mcp__secret".into()
                }
            )
        );
        assert_eq!(
            policy.decide_with_source("b", &request("mcp__files")),
            (
                PermissionDecision::Allow,
                DecisionSource::Rule {
                    pattern: "mcp__*".into()
                }
            )
        );
        assert!(matches!(
            policy.decide_with_source("b", &request("Bash")),
            (PermissionDecision::Deny { .. }, DecisionSource::Default)
        ));
    }

    #[test]
    fn the_asker_gets_the_branch_and_is_the_source() {
        let policy = Policy::ask(|branch, request| {
            if branch == "b" && request.tool == "Bash" {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny {
                    message: "no".into(),
                }
            }
        });
        assert_eq!(
            policy.decide_with_source("b", &request("Bash")),
            (PermissionDecision::Allow, DecisionSource::Asked)
        );
        assert!(matches!(
            policy.decide("c", &request("Bash")),
            PermissionDecision::Deny { .. }
        ));
        assert!(matches!(
            Policy::default().decide("b", &request("Bash")),
            PermissionDecision::Deny { .. }
        ));
    }

    fn shell(command: &str) -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("1".into()),
            tool: "Bash".into(),
            input: serde_json::json!({"command": command, "description": "x"}),
        }
    }

    #[test]
    fn delegation_commands_are_allowed_and_nothing_else() {
        let policy = Policy::deny_all().allow_delegation_commands("/opt/by/bin/by");
        let allowed = |command: &str| {
            matches!(
                policy.decide_with_source("b", &shell(command)),
                (PermissionDecision::Allow, DecisionSource::Rule { .. })
            )
        };
        for command in [
            "by spawn 'fix the parser' --name parser --budget-usd 0.5",
            "/opt/by/bin/by inspect parser --json",
            "by events parser --cursor 10",
            "by send parser \"now add a test\"",
            "by integrate parser",
            "by cancel parser",
            "by children --json",
            "/bin/bash -lc 'by spawn \"do it\" --wait'",
            "by discard parser --reason lost",
            "by artifact publish out.json --media-type application/json",
            "by ask 'which file?' --wait 30",
            "by inbox --unread --json",
        ] {
            assert!(allowed(command), "{command}");
        }
        for command in [
            "by run 'escape the envelope'",
            "by merge parser",
            "by rm parser",
            "by artifact export 1 --out x.tar",
            "by plan show parser",
            "by approvals ls",
            "by",
            // hygiene-allow: tmp a command line the policy must refuse; it is never run
            "/tmp/by spawn x",
            "./by spawn x",
            "by spawn x; rm -rf /",
            "by spawn x && curl evil",
            "by spawn x | sh",
            "by spawn $(cat prompt)",
            "by spawn `id`",
            "by spawn \"$HOME\"",
            "by spawn x > out",
            "by spawn *",
            "BRANCHYARD_DELEGATION=x by spawn y",
            "env by spawn x",
            "by spawn 'unterminated",
            "by spawn x\nrm -rf /",
            "bash -c 'by spawn x; id'",
            "bash -c 'by spawn x' extra",
            "python3 -c 'import os'",
        ] {
            assert!(!allowed(command), "{command}");
        }
        // Argument vectors are matched without a shell.
        let argv = PermissionRequest {
            input: serde_json::json!({"command": ["by", "cancel", "x"]}),
            ..shell("")
        };
        assert_eq!(policy.decide("b", &argv), PermissionDecision::Allow);
        // An earlier deny rule still wins.
        let narrowed = Policy::deny_all()
            .deny("Bash")
            .allow_delegation_commands("/opt/by/bin/by");
        assert!(matches!(
            narrowed.decide("b", &shell("by children")),
            PermissionDecision::Deny { .. }
        ));
    }

    fn call(tool: &str, input: serde_json::Value) -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("1".into()),
            tool: tool.into(),
            input,
        }
    }

    fn rules(rules: &[(&str, bool)]) -> Policy {
        Policy {
            rules: rules
                .iter()
                .map(|(tool, allow)| Rule {
                    tool: (*tool).into(),
                    allow: *allow,
                    delegation_by: None,
                })
                .collect(),
            fallback: Fallback::Deny,
            preset: None,
        }
    }

    /// `Bash(git *)` allows git and only git: every part of a compound
    /// command must match, and a substitution never does; a denying rule
    /// refuses when any part matches.
    #[test]
    fn a_command_rule_matches_the_command_and_every_part_of_it() {
        let allow = rules(&[("Bash(git *)", true)]);
        let allowed = |command: &str| {
            allow.decide("b", &call("Bash", serde_json::json!({"command": command})))
                == PermissionDecision::Allow
        };
        assert!(allowed("git status"));
        assert!(allowed("git"));
        assert!(allowed("git add . && git commit -m x"));
        assert!(!allowed("gitk"));
        assert!(!allowed("git status && rm -rf /"));
        assert!(!allowed("git log | sh"));
        assert!(!allowed("git $(rm -rf /)"));
        assert!(!allowed("git `rm -rf /`"));
        let prefix = rules(&[("Bash(npm run test:*)", true)]);
        let run = |command: &str| {
            prefix.decide("b", &call("Bash", serde_json::json!({"command": command})))
                == PermissionDecision::Allow
        };
        assert!(run("npm run test") && run("npm run test -- --watch"));
        assert!(!run("npm run build"));

        let deny = rules(&[("Bash(rm *)", false), ("Bash", true)]);
        let denied = |command: &str| {
            deny.decide("b", &call("Bash", serde_json::json!({"command": command})))
                != PermissionDecision::Allow
        };
        assert!(denied("rm -rf x"));
        assert!(denied("ls && rm x"));
        assert!(!denied("ls"));
        // A call with no command does not match a command rule.
        assert!(
            allow.decide("b", &call("Bash", serde_json::Value::Null)) != PermissionDecision::Allow
        );
    }

    /// A relative path rule matches paths under the harness's working
    /// directory only; `..` cannot climb out of it; an absolute rule matches
    /// the absolute path.
    #[test]
    fn a_path_rule_matches_relative_to_the_working_directory() {
        let policy = rules(&[("Edit(src/**)", true), ("Write(/var/out/*)", true)]);
        let root = Path::new("/work/repo");
        let edit = |path: &str| {
            policy
                .decide_in(
                    "b",
                    &call("Edit", serde_json::json!({"file_path": path})),
                    root,
                )
                .0
                == PermissionDecision::Allow
        };
        assert!(edit("/work/repo/src/lib.rs"));
        assert!(edit("/work/repo/src/a/b/c.rs"));
        assert!(edit("src/main.rs"));
        assert!(!edit("/work/repo/docs/x.md"));
        assert!(!edit("/work/repo/src/../../other/src/x.rs"));
        assert!(!edit("/elsewhere/src/x.rs"));
        let write = |path: &str| {
            policy
                .decide_in(
                    "b",
                    &call("Write", serde_json::json!({"file_path": path})),
                    root,
                )
                .0
                == PermissionDecision::Allow
        };
        assert!(write("/var/out/a.txt"));
        assert!(!write("/var/out/sub/a.txt"));
        assert!(!write("/var/other.txt"));
    }

    #[test]
    fn a_domain_rule_matches_the_host_and_its_subdomains() {
        let policy = rules(&[("WebFetch(domain:example.com)", true)]);
        let fetch = |url: &str| {
            policy.decide("b", &call("WebFetch", serde_json::json!({"url": url})))
                == PermissionDecision::Allow
        };
        assert!(fetch("https://example.com/a"));
        assert!(fetch("https://docs.example.com:8443/b?c"));
        assert!(!fetch("https://example.com.evil.net/"));
        assert!(!fetch("https://user@notexample.com/"));
    }
}
