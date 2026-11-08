//! What a tool call acted on, in a form safe to keep in the event log.
//!
//! A [`ToolTarget`] is a bounded, privacy-safe summary of a tool call's
//! input: the path a file tool read or edited, the command a shell tool
//! ran, or the server and tool of an MCP call. It is never a file's
//! content and never the call's full arguments.
//!
//! - A path inside the session's working directory is relative to it. A
//!   path in the mounted tool tree (`/tools/...`, or anything under
//!   `.branchyard/tools/`) is written `tools/...`. Any other absolute path
//!   is reduced to its last component, so the log does not carry a host's
//!   directory layout.
//! - A command is collapsed to one line, has secrets scrubbed (values of
//!   `NAME=secret` assignments and of `--token`-style flags, `-u user:pass`
//!   and `--user` of curl-like tools, `-pPASS` of mysql-like clients and
//!   `-p PASS` of registry logins, the value of an `Authorization`,
//!   `Cookie` or `*-Key`/`*Token`/`*Secret` header, bearer tokens, URL
//!   userinfo, the values of secret-named URL query parameters, AWS-secret
//!   shaped words and well-known token shapes), and is cut at
//!   [`COMMAND_MAX`] characters. A 40 or 64 character hex word in a `git`
//!   command is a commit id and is kept.
//! - A URL given as a path (`uri`) goes through the same URL scrubbing, and
//!   a file path loses a `?query`.
//!
//! The same scrubber works on free text ([`scrub_values`], [`scrub_shapes`]):
//! what memory's observations run at a turn's end (`docs/memory.md`,
//! section 2), before anything derived from a turn is stored. It removes
//! exact values a caller names, then the shapes above plus JWTs, PEM blocks
//! and `Bearer` credentials, cuts URLs at their query, and replaces email
//! addresses that are not a member's, counting each removal by
//! [`ScrubKind`] and never keeping the value.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the text scrubber replaces a secret with ([`REDACTED`] for a
/// value, this for an email address).
pub const EMAIL: &str = "<email>";

/// The shortest exact value [`scrub_values`] removes: a shorter one would
/// shred ordinary words.
pub const VALUE_MIN: usize = 4;

/// What the text scrubber removed, by kind; never the value.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrubKind {
    /// An exact secret value the caller named, or one of its encodings.
    Value,
    /// A well-known token shape: `AKIA…`, `ghp_…`, `sk-…`, `xox…`, an AWS
    /// secret access key.
    Token,
    /// A JSON Web Token.
    Jwt,
    /// A PEM block, from `-----BEGIN` to `-----END`.
    Pem,
    /// The credential after `Bearer` or `Basic`.
    Bearer,
    /// The value of a secret-named assignment or header (`password=…`,
    /// `api_key: …`).
    Assignment,
    /// A 32-plus-character hex or base64 run after a key-like word.
    KeyedRun,
    /// A URL's userinfo, or its query and fragment.
    Url,
    /// An email address that is not a member's.
    Email,
}

/// Text after the scrubber, with what it removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scrubbed {
    /// The text, with each secret replaced.
    pub text: String,
    /// One entry per removal, by kind.
    pub removed: Vec<ScrubKind>,
    /// How many characters of the input were replaced.
    pub chars_removed: usize,
}

impl Scrubbed {
    fn unchanged(text: &str) -> Scrubbed {
        Scrubbed {
            text: text.to_owned(),
            ..Scrubbed::default()
        }
    }
}

/// `text` with every occurrence of each of `values` (at least
/// [`VALUE_MIN`] characters; shorter ones are skipped) replaced by
/// [`REDACTED`]. Longer values go first, so a value that contains another
/// is removed whole. The caller passes the encodings it wants removed too
/// (base64, URL-encoded): this function knows nothing of them.
pub fn scrub_values(text: &str, values: &[String]) -> Scrubbed {
    let mut wanted: Vec<&str> = values
        .iter()
        .map(String::as_str)
        .filter(|v| v.chars().count() >= VALUE_MIN)
        .collect();
    wanted.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    wanted.dedup();
    let mut out = Scrubbed::unchanged(text);
    for value in wanted {
        let mut rest = out.text.as_str();
        let mut rebuilt = String::with_capacity(out.text.len());
        let mut found = 0usize;
        while let Some(at) = rest.find(value) {
            rebuilt.push_str(&rest[..at]);
            rebuilt.push_str(REDACTED);
            rest = &rest[at + value.len()..];
            found += 1;
        }
        if found == 0 {
            continue;
        }
        rebuilt.push_str(rest);
        out.chars_removed += found * value.chars().count();
        out.removed
            .extend(std::iter::repeat_n(ScrubKind::Value, found));
        out.text = rebuilt;
    }
    out
}

/// `text` with every secret shape removed: PEM blocks, JWTs, well-known
/// token shapes, `Bearer` and `Basic` credentials, the values of
/// secret-named assignments and headers, 32-plus-character hex or base64
/// runs after a key-like word, URL userinfo and query strings, and email
/// addresses that are not one of `members` (compared without case). A 40
/// or 64 character hex word on its own is a commit id and is kept.
pub fn scrub_shapes(text: &str, members: &[String]) -> Scrubbed {
    let mut out = scrub_pem(text);
    let lowered: Vec<String> = members.iter().map(|m| m.to_ascii_lowercase()).collect();
    let mut rebuilt = String::with_capacity(out.text.len());
    let mut previous: Option<String> = None;
    let mut hide_next: Option<ScrubKind> = None;
    for piece in split_keeping_whitespace(&out.text) {
        if piece.chars().all(char::is_whitespace) {
            rebuilt.push_str(piece);
            continue;
        }
        let (lead, core, tail) = trim_punctuation(piece);
        let lower = core.to_ascii_lowercase();
        let replaced: Option<(String, ScrubKind)> = if let Some(kind) = hide_next.take() {
            if matches!(lower.as_str(), "bearer" | "basic") {
                hide_next = Some(ScrubKind::Bearer);
                None
            } else {
                Some((REDACTED.to_owned(), kind))
            }
        } else if matches!(lower.as_str(), "bearer" | "basic") {
            hide_next = Some(ScrubKind::Bearer);
            None
        } else if core.contains("://") {
            cut_url(core).map(|url| (url, ScrubKind::Url))
        } else if let Some(cut) = secret_assignment(core) {
            Some((cut, ScrubKind::Assignment))
        } else if jwt_shaped(core) {
            Some((REDACTED.to_owned(), ScrubKind::Jwt))
        } else if prefixed_token(core) || aws_secret_shaped(core) {
            Some((REDACTED.to_owned(), ScrubKind::Token))
        } else if long_run(core) && previous.as_deref().is_some_and(key_like) {
            Some((REDACTED.to_owned(), ScrubKind::KeyedRun))
        } else if let Some(email) = email_shaped(core) {
            match lowered.iter().any(|m| m == &email.to_ascii_lowercase()) {
                true => None,
                false => Some((EMAIL.to_owned(), ScrubKind::Email)),
            }
        } else {
            None
        };
        // `password:` as its own word hides the next word.
        if replaced.is_none() && hide_next.is_none() {
            if let Some(name) = core.strip_suffix(':').or_else(|| core.strip_suffix('=')) {
                if !name.is_empty() && secret_name(name) && !name.contains("://") {
                    hide_next = Some(ScrubKind::Assignment);
                }
            }
        }
        match replaced {
            Some((value, kind)) => {
                out.removed.push(kind);
                let kept = value
                    .replace(REDACTED, "")
                    .replace(EMAIL, "")
                    .chars()
                    .count();
                out.chars_removed += core.chars().count().saturating_sub(kept);
                rebuilt.push_str(lead);
                rebuilt.push_str(&value);
                rebuilt.push_str(tail);
            }
            None => rebuilt.push_str(piece),
        }
        previous = Some(lower);
    }
    out.text = rebuilt;
    out
}

/// `text` split into runs of whitespace and runs of anything else, in
/// order, so joining the pieces gives `text` back.
fn split_keeping_whitespace(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut in_space: Option<bool> = None;
    for (at, c) in text.char_indices() {
        let space = c.is_whitespace();
        if in_space.is_some_and(|was| was != space) {
            pieces.push(&text[start..at]);
            start = at;
        }
        in_space = Some(space);
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// `piece` as its wrapping punctuation, its core and its trailing
/// punctuation. `password:` and `token=` keep their last character, so the
/// core is judged with it.
fn trim_punctuation(piece: &str) -> (&str, &str, &str) {
    let opening = |c: char| matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '*' | '_');
    let closing = |c: char| {
        matches!(
            c,
            ')' | ']'
                | '}'
                | '>'
                | '"'
                | '\''
                | '`'
                | ','
                | ';'
                | '.'
                | ':'
                | '!'
                | '?'
                | '*'
                | '_'
        )
    };
    let start = piece.len() - piece.trim_start_matches(opening).len();
    let rest = &piece[start..];
    let mut core_len = rest.trim_end_matches(closing).len();
    if core_len + 1 == rest.len() && rest.ends_with([':', '=']) {
        core_len = rest.len();
    }
    if core_len == 0 {
        return ("", piece, "");
    }
    (&piece[..start], &rest[..core_len], &rest[core_len..])
}

/// A PEM block, `-----BEGIN …-----` to `-----END …-----`, replaced whole;
/// one with no end runs to the end of the text.
fn scrub_pem(text: &str) -> Scrubbed {
    const BEGIN: &str = "-----BEGIN";
    const END: &str = "-----END";
    let mut out = Scrubbed::default();
    let mut rest = text;
    while let Some(at) = rest.find(BEGIN) {
        out.text.push_str(&rest[..at]);
        let after = &rest[at..];
        let stop = after
            .find(END)
            .and_then(|e| {
                let tail = &after[e + END.len()..];
                tail.find("-----")
                    .map(|d| e + END.len() + d + "-----".len())
            })
            .unwrap_or(after.len());
        out.text.push_str(REDACTED);
        out.removed.push(ScrubKind::Pem);
        out.chars_removed += after[..stop].chars().count();
        rest = &after[stop..];
    }
    out.text.push_str(rest);
    out
}

/// A URL with its userinfo removed and its query and fragment cut; `None`
/// when nothing had to change.
fn cut_url(word: &str) -> Option<String> {
    let without_credentials = url_credentials(word).unwrap_or_else(|| word.to_owned());
    let cut = match without_credentials.find(['?', '#']) {
        Some(at) => without_credentials[..at].to_owned(),
        None => without_credentials,
    };
    (cut != word).then_some(cut)
}

/// `name=value` or `name:value` whose name is secret-like, with the value
/// replaced; `None` otherwise.
fn secret_assignment(word: &str) -> Option<String> {
    let at = word.find(['=', ':'])?;
    let (name, value) = (&word[..at], &word[at + 1..]);
    let is_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !is_name || value.is_empty() || !secret_name(name) {
        return None;
    }
    Some(format!("{name}{}{REDACTED}", &word[at..=at]))
}

/// A JSON Web Token: three base64url segments, the first a JSON header.
fn jwt_shaped(word: &str) -> bool {
    let mut parts = word.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let base64url = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    };
    header.starts_with("eyJ") && base64url(header) && base64url(payload) && base64url(signature)
}

/// A word that starts with a well-known token prefix and is long enough.
fn prefixed_token(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    TOKEN_PREFIXES
        .iter()
        .any(|prefix| word.starts_with(prefix) && word.len() >= prefix.len() + 8)
}

/// A 32-plus-character hex or base64 run with a digit and a letter.
fn long_run(word: &str) -> bool {
    word.len() >= 32
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// Whether the word before a run makes it a key: it names a secret.
fn key_like(previous: &str) -> bool {
    let previous = previous.trim_end_matches([':', '=']);
    secret_name(previous)
}

/// The email address `word` is, when it is one: a local part, `@`, a
/// domain with a dot and a letters-only last label.
fn email_shaped(word: &str) -> Option<&str> {
    let (local, domain) = word.split_once('@')?;
    let local_ok = !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'));
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
    (local_ok && domain_ok).then_some(word)
}

/// The most characters of a command a target keeps.
pub const COMMAND_MAX: usize = 120;
/// The most characters of a path, skill name or tool name a target keeps.
pub const NAME_MAX: usize = 160;
/// What replaces a secret.
pub const REDACTED: &str = "[redacted]";

/// What kind of thing a [`ToolTarget`] names.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// A file the tool read or changed.
    File,
    /// A shell command.
    Command,
    /// An MCP or dynamic tool, as `server.tool` (or the tool alone).
    Mcp,
    /// A skill the harness's own skill tool opened.
    Skill,
}

/// What a tool call acted on.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTarget {
    /// What `value` names.
    pub kind: TargetKind,
    /// The path, command or tool name, bounded and scrubbed as the module
    /// documents.
    pub value: String,
}

impl ToolTarget {
    /// A file, with `raw` made relative to `cwd` or reduced as documented.
    pub fn file(raw: &str, cwd: &str) -> Option<ToolTarget> {
        let value = path(raw, cwd)?;
        Some(ToolTarget {
            kind: TargetKind::File,
            value,
        })
    }

    /// A shell command, scrubbed and bounded; its absolute paths are
    /// reduced as [`path`] does with no working directory.
    pub fn command(raw: &str) -> Option<ToolTarget> {
        ToolTarget::command_in(raw, "")
    }

    /// A shell command run in `cwd`, scrubbed and bounded; a word that is an
    /// absolute path is reduced as [`path`] does, so the command's first
    /// words fit in the bound and the log carries no host layout.
    pub fn command_in(raw: &str, cwd: &str) -> Option<ToolTarget> {
        let value = command(raw, cwd)?;
        Some(ToolTarget {
            kind: TargetKind::Command,
            value,
        })
    }

    /// An MCP or dynamic tool call.
    pub fn mcp(server: Option<&str>, tool: &str) -> Option<ToolTarget> {
        let tool = tool.trim();
        if tool.is_empty() {
            return None;
        }
        let value = match server.map(str::trim).filter(|s| !s.is_empty()) {
            Some(server) => format!("{server}.{tool}"),
            None => tool.to_owned(),
        };
        Some(ToolTarget {
            kind: TargetKind::Mcp,
            value: cut(&value, NAME_MAX),
        })
    }

    /// A skill opened by name.
    pub fn skill(name: &str) -> Option<ToolTarget> {
        let name = name.trim();
        (!name.is_empty()).then(|| ToolTarget {
            kind: TargetKind::Skill,
            value: cut(name, NAME_MAX),
        })
    }
}

/// Keys whose string value is a shell command.
const COMMAND_KEYS: [&str; 6] = [
    "command",
    "cmd",
    "commandline",
    "command_line",
    "script",
    "shell",
];
/// Keys whose string value is a path.
const PATH_KEYS: [&str; 7] = [
    "file_path",
    "filepath",
    "path",
    "notebook_path",
    "file",
    "filename",
    "uri",
];

/// The target of a tool call from its name and input object, for harnesses
/// that report both: a `mcp__server__tool` name is an MCP call; the native
/// skill tool names the skill; an input with a command is a command; one
/// with a path is a file. `None` when the input shows none of these.
pub fn from_input(tool: &str, input: &Value, cwd: &str) -> Option<ToolTarget> {
    if let Some(rest) = tool.strip_prefix("mcp__") {
        return match rest.split_once("__") {
            Some((server, name)) => ToolTarget::mcp(Some(server), name),
            None => ToolTarget::mcp(None, rest),
        };
    }
    let object = input.as_object()?;
    if tool.eq_ignore_ascii_case("skill") {
        let name = ["skill", "name"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str));
        if let Some(target) = name.and_then(ToolTarget::skill) {
            return Some(target);
        }
    }
    let string_at = |keys: &[&str]| {
        object.iter().find_map(|(key, value)| {
            let key = key.to_ascii_lowercase();
            match keys.contains(&key.as_str()) {
                true => value.as_str(),
                false => None,
            }
        })
    };
    if let Some(command) = string_at(&COMMAND_KEYS).and_then(|raw| ToolTarget::command_in(raw, cwd))
    {
        return Some(command);
    }
    let path = string_at(&PATH_KEYS).or_else(|| {
        // `paths` and `files` lists: the first is the target.
        ["paths", "files"]
            .iter()
            .find_map(|key| object.get(*key)?.as_array()?.first()?.as_str())
    })?;
    ToolTarget::file(path, cwd)
}

/// `raw` as a log-safe path: relative to `cwd`, `tools/...` for the tool
/// tree, otherwise the last component of an absolute path.
pub fn path(raw: &str, cwd: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return None;
    }
    // A URL that is not a file URL keeps its host and path; userinfo and
    // secret query values go.
    if raw.contains("://") && !raw.starts_with("file://") {
        return Some(cut(&redact_url(raw), NAME_MAX));
    }
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    // A file has no query string: `a.txt?token=x` is `a.txt`.
    let raw = raw.split('?').next().unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }
    let value = if let Some(rest) = after_marker(raw) {
        format!("tools/{rest}")
    } else if raw.starts_with('/') {
        let cwd = cwd.trim_end_matches('/');
        match raw
            .strip_prefix(cwd)
            .filter(|rest| !cwd.is_empty() && (rest.is_empty() || rest.starts_with('/')))
        {
            Some(rest) => match rest.trim_start_matches('/') {
                "" => ".".to_owned(),
                inside => inside.to_owned(),
            },
            None => raw
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(raw)
                .to_owned(),
        }
    } else {
        raw.trim_start_matches("./").to_owned()
    };
    Some(cut(&value, NAME_MAX))
}

/// What follows the tool tree's directory in `raw`, when it names one.
fn after_marker(raw: &str) -> Option<&str> {
    if let Some(rest) = raw.strip_prefix("/tools/") {
        return Some(rest);
    }
    raw.split_once(".branchyard/tools/").map(|(_, rest)| rest)
}

/// `raw` as a one-line, scrubbed command of at most [`COMMAND_MAX`]
/// characters, with a plain shell wrapper (`bash -lc "..."`) removed and
/// each word that is an absolute path reduced as [`path`] reduces it.
pub fn command(raw: &str, cwd: &str) -> Option<String> {
    let unwrapped = unwrap_shell(raw.trim());
    let words = command_words(unwrapped);
    if words.is_empty() {
        return None;
    }
    let reduced: Vec<String> = scrub_words(&words)
        .iter()
        .map(|word| reduce_path_word(word, cwd))
        .collect();
    let rendered: Vec<String> = reduced.iter().map(|word| render_word(word)).collect();
    Some(cut(&rendered.join(" "), COMMAND_MAX))
}

/// `text` split into words as a shell would (`shlex`): quotes and
/// backslash escapes are resolved, so `--password 'first second'` is two
/// words, the second one `first second`. An unterminated quote is closed;
/// text the splitter still refuses is split at whitespace. Whitespace inside
/// a word is one space and other control characters are dropped.
fn command_words(text: &str) -> Vec<String> {
    let words = shlex::split(text)
        .or_else(|| shlex::split(&format!("{text}'")))
        .or_else(|| shlex::split(&format!("{text}\"")))
        .unwrap_or_else(|| text.split_whitespace().map(str::to_owned).collect());
    words
        .into_iter()
        .map(|word| {
            word.chars()
                .filter(|c| c.is_whitespace() || !c.is_control())
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect()
        })
        .collect()
}

/// `word` as a shell word again: quoted when it holds a space, a quote, a
/// backslash or nothing, as it was typed otherwise.
fn render_word(word: &str) -> String {
    let plain = !word.is_empty() && !word.contains([' ', '\'', '"', '\\']);
    match plain {
        true => word.to_owned(),
        false => {
            shlex::try_quote(word).map_or_else(|_| word.to_owned(), |quoted| quoted.into_owned())
        }
    }
}

/// `word` with its path, when it is one (`/a/b`, `'/a/b'`), reduced; a word
/// that holds spaces (a quoted argument) is reduced piece by piece.
fn reduce_path_word(word: &str, cwd: &str) -> String {
    if word.contains(' ') {
        return word
            .split(' ')
            .map(|piece| reduce_path_word(piece, cwd))
            .collect::<Vec<_>>()
            .join(" ");
    }
    let quotes = |c: char| matches!(c, '\'' | '"' | ';' | '(' | ')');
    let core = word.trim_matches(quotes);
    if !core.starts_with('/') || core.len() < 2 {
        return word.to_owned();
    }
    let Some(reduced) = path(core, cwd) else {
        return word.to_owned();
    };
    let lead = &word[..word.len() - word.trim_start_matches(quotes).len()];
    let tail = &word[word.trim_end_matches(quotes).len()..];
    format!("{lead}{reduced}{tail}")
}

/// The script of `sh -c SCRIPT` and `bash -lc SCRIPT`, without the quotes
/// around it; anything else unchanged.
fn unwrap_shell(raw: &str) -> &str {
    let mut words = raw.splitn(3, char::is_whitespace);
    let (Some(shell), Some(flag), Some(rest)) = (words.next(), words.next(), words.next()) else {
        return raw;
    };
    let name = shell.rsplit('/').next().unwrap_or(shell);
    let shell_like = matches!(name, "sh" | "bash" | "zsh" | "dash");
    let runs_script = flag.starts_with('-') && flag.ends_with('c') && !flag.starts_with("--");
    if !(shell_like && runs_script) {
        return raw;
    }
    let rest = rest.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = rest
            .strip_prefix(quote)
            .and_then(|inner| inner.strip_suffix(quote))
        {
            return inner;
        }
    }
    rest
}

/// Name fragments that make a variable or flag hold a secret.
const SECRET_WORDS: [&str; 11] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "credential",
    "auth",
    "cookie",
    "session",
    "bearer",
];
/// Prefixes of well-known token shapes.
const TOKEN_PREFIXES: [&str; 12] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xox",
    "glpat-",
    "AKIA",
    "AIza",
    "npm_",
];

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|word| name.contains(word))
}

fn token_shaped(word: &str) -> bool {
    if aws_secret_shaped(word) {
        return true;
    }
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    if TOKEN_PREFIXES
        .iter()
        .any(|prefix| word.starts_with(prefix) && word.len() >= prefix.len() + 8)
    {
        return true;
    }
    // A long run of token characters with a digit and a letter in it is a
    // credential far more often than a word.
    word.len() >= 32
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
        && !word.contains('/')
}

/// An AWS secret access key: 40 base64 characters, here required to hold a
/// `/` or `+` (the ones the plain rule skips), both cases and a digit, and
/// not to look like a path (no leading `/` or `.`, no `.` at all).
fn aws_secret_shaped(word: &str) -> bool {
    let word = word.trim_matches(|c: char| matches!(c, '\'' | '"' | ',' | ';' | '(' | ')'));
    word.len() == 40
        && word.contains(['/', '+'])
        && !word.starts_with(['/', '.'])
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '+'))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_uppercase())
        && word.chars().any(|c| c.is_ascii_lowercase())
}

/// A commit id: 40 or 64 hex characters.
fn commit_id(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    matches!(word.len(), 40 | 64) && word.chars().all(|c| c.is_ascii_hexdigit())
}

/// `word` with the userinfo of a URL's credentials removed.
fn url_credentials(word: &str) -> Option<String> {
    let (scheme, rest) = word.split_once("://")?;
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let (_, host) = authority.rsplit_once('@')?;
    Some(format!("{scheme}://{REDACTED}@{host}{tail}"))
}

/// Query and fragment parameters whose name is exactly one of these hold a
/// secret (a name containing a [`SECRET_WORDS`] fragment does too).
const SECRET_PARAMS: [&str; 6] = ["sig", "signature", "code", "sas", "jwt", "otp"];

fn secret_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_PARAMS.contains(&lower.as_str())
        || lower.ends_with("signature")
        || lower.ends_with("-sig")
        || lower.ends_with("_sig")
        || secret_name(&lower)
}

/// `word` with the values of secret-named `?query` and `#fragment`
/// parameters replaced.
fn redact_query(word: &str) -> String {
    let Some(start) = word.find(['?', '#']) else {
        return word.to_owned();
    };
    let (head, params) = word.split_at(start + 1);
    let cleaned: Vec<String> = params
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((name, value)) if !value.is_empty() && secret_param(name) => {
                let tail: String = value
                    .chars()
                    .rev()
                    .take_while(|c| matches!(c, '\'' | '"' | ')' | ';'))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("{name}={REDACTED}{tail}")
            }
            _ => param.to_owned(),
        })
        .collect();
    format!("{head}{}", cleaned.join("&"))
}

/// A URL as the log may keep it: no userinfo, no secret query values.
fn redact_url(url: &str) -> String {
    let url = url_credentials(url).unwrap_or_else(|| url.to_owned());
    redact_query(&url)
}

/// The program a word names: its last path component.
fn program_of(word: &str) -> &str {
    let word = word.trim_matches(|c: char| matches!(c, '\'' | '"' | '(' | '`'));
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether `word` ends a simple command (`|`, `&&`, `;`).
fn separator(word: &str) -> bool {
    matches!(word, "|" | "||" | "&&" | "&" | ";") || (word.len() > 1 && word.ends_with(';'))
}

/// Programs whose `-u`/`--user` (and `-a` for the HTTP clients) carry
/// `user:password`.
const USER_PASS_TOOLS: [&str; 6] = ["curl", "wget", "http", "https", "xh", "xhs"];
/// Programs whose `-pPASSWORD` (attached) is a password.
const ATTACHED_P_TOOLS: [&str; 4] = ["mysql", "mysqldump", "mysqladmin", "mariadb"];
/// Programs whose `-p PASSWORD` is a password.
const SPACED_P_TOOLS: [&str; 6] = ["docker", "podman", "helm", "oras", "skopeo", "crane"];

/// Whether `word` names a header that holds a secret: `Authorization`,
/// `Cookie`, or a name ending in key, token, secret, password or
/// signature. A name without a hyphen counts only after `-H`.
fn secret_header(name: &str, after_header_flag: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if !lower
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        || lower.is_empty()
    {
        return false;
    }
    if matches!(
        lower.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) {
        return true;
    }
    let ends = ["key", "token", "secret", "password", "signature", "auth"]
        .iter()
        .any(|end| lower.ends_with(end));
    ends && (after_header_flag || lower.contains('-'))
}

/// `-H'Name: value'` and `--header='Name: value'` as the flag and its
/// value, so the header is judged as one after `-H`.
fn detach_headers(words: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    for word in words {
        let rest = match word.strip_prefix("--header=") {
            Some(rest) => Some(("--header", rest)),
            None => word
                .strip_prefix("-H")
                .filter(|_| !word.starts_with("--"))
                .map(|rest| ("-H", rest)),
        };
        match rest {
            Some((flag, rest)) if rest.contains(':') => {
                out.push(flag.to_owned());
                out.push(rest.to_owned());
            }
            _ => out.push(word.clone()),
        }
    }
    out
}

/// A word that holds spaces (a quoted argument, decoded), scrubbed piece by
/// piece: `echo "a token=abc"` keeps its shape and loses the value.
fn scrub_spaced(word: &str) -> String {
    let pieces: Vec<String> = word.split_whitespace().map(str::to_owned).collect();
    scrub_words(&pieces).join(" ")
}

fn scrub_words(words: &[String]) -> Vec<String> {
    let words = detach_headers(words);
    let mut out = Vec::with_capacity(words.len());
    let mut hide_next = false;
    let mut program: Option<String> = None;
    let mut previous = String::new();
    let mut login = false;
    for word in &words {
        let lower = word.to_ascii_lowercase();
        if separator(word) {
            program = None;
            login = false;
        } else if program.is_none()
            && !word.contains('=')
            && !matches!(program_of(word), "sudo" | "env" | "time" | "exec" | "nohup")
        {
            program = Some(program_of(word).to_owned());
        }
        let tool = program.as_deref().unwrap_or("");
        if lower == "login" {
            login = true;
        }
        let after_header_flag = matches!(previous.as_str(), "-H" | "--header");
        previous.clone_from(word);
        if hide_next {
            hide_next = false;
            // `Authorization: Bearer TOKEN`: the scheme word stays.
            if lower == "bearer" || lower == "basic" {
                hide_next = true;
                out.push(word.clone());
            } else {
                out.push(REDACTED.to_owned());
            }
            continue;
        }
        if lower == "bearer" || lower == "basic" {
            out.push(word.clone());
            hide_next = true;
            continue;
        }
        // A header, quoted or not: `Name: value`, `"Name: value"`,
        // `"Name: value`, `Name:value`.
        let core = word.trim_start_matches(['\'', '"']);
        if let Some((name, value)) = core.split_once(':') {
            if !core.contains("://") && secret_header(name, after_header_flag) {
                let lead = &word[..word.len() - core.len()];
                let close = lead
                    .chars()
                    .last()
                    .filter(|q| core.len() > 1 && word.ends_with(*q))
                    .map(String::from)
                    .unwrap_or_default();
                let space = if value.starts_with(' ') { " " } else { "" };
                let value = value.trim().trim_matches(['\'', '"']);
                let (first, rest) = value.split_once(' ').unwrap_or((value, ""));
                let scheme = matches!(first.to_ascii_lowercase().as_str(), "bearer" | "basic");
                if value.is_empty() {
                    out.push(word.clone());
                    hide_next = true;
                } else if scheme && rest.is_empty() {
                    out.push(format!("{lead}{name}:{REDACTED}"));
                    hide_next = true;
                } else if scheme {
                    out.push(format!("{lead}{name}:{space}{first} {REDACTED}{close}"));
                } else {
                    out.push(format!("{lead}{name}:{space}{REDACTED}{close}"));
                }
                continue;
            }
        }
        if let Some(flag) = word.strip_prefix('-') {
            let flag = flag.trim_start_matches('-');
            let user_pass_tool = USER_PASS_TOOLS.contains(&tool);
            if user_pass_tool
                && (matches!(word.as_str(), "-u" | "-U" | "--user" | "--proxy-user")
                    || (matches!(tool, "http" | "https" | "xh" | "xhs") && word == "-a"))
            {
                out.push(word.clone());
                hide_next = true;
                continue;
            }
            if user_pass_tool {
                // `-uuser:pass`, `--user=user:pass`.
                if word.len() > 2 && word.starts_with("-u") && !word.starts_with("--") {
                    out.push(format!("-u{REDACTED}"));
                    continue;
                }
                if let Some(long) = ["--user=", "--proxy-user="]
                    .iter()
                    .find(|long| word.starts_with(**long))
                {
                    out.push(format!("{long}{REDACTED}"));
                    continue;
                }
            }
            if word == "-p" && login && SPACED_P_TOOLS.contains(&tool) {
                out.push(word.clone());
                hide_next = true;
                continue;
            }
            if ATTACHED_P_TOOLS.contains(&tool)
                && word.len() > 2
                && word.starts_with("-p")
                && !word.starts_with("--")
            {
                out.push(format!("-p{REDACTED}"));
                continue;
            }
            if let Some((name, _)) = flag.split_once('=') {
                if secret_name(name) {
                    out.push(format!(
                        "{}={REDACTED}",
                        word.split('=').next().unwrap_or_default()
                    ));
                    continue;
                }
            } else if secret_name(flag) {
                out.push(word.clone());
                hide_next = true;
                continue;
            }
        }
        if let Some((name, value)) = word.split_once('=') {
            let is_assignment = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if is_assignment && !value.is_empty() && secret_name(name) {
                out.push(format!("{name}={REDACTED}"));
                continue;
            }
        }
        if word.contains(' ') {
            out.push(scrub_spaced(word));
        } else if word.contains("://") {
            out.push(redact_url(word));
        } else if tool == "git" && commit_id(word) {
            out.push(word.clone());
        } else if token_shaped(word) {
            out.push(REDACTED.to_owned());
        } else if word.contains('?') {
            out.push(redact_query(word));
        } else {
            out.push(word.clone());
        }
    }
    out
}

/// `text` cut to `max` characters, with `...` marking a cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{}...", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_path_inside_the_working_directory_is_relative() {
        assert_eq!(
            path("/workspace/src/a.rs", "/workspace").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            path("./notes.txt", "/workspace").as_deref(),
            Some("notes.txt")
        );
        assert_eq!(path("/workspace", "/workspace").as_deref(), Some("."));
        assert_eq!(
            path("/workspace2/x.txt", "/workspace").as_deref(),
            Some("x.txt")
        );
    }

    #[test]
    fn a_tool_tree_path_keeps_its_place_in_the_tree() {
        assert_eq!(
            path("/tools/skills/docx/SKILL.md", "/workspace").as_deref(),
            Some("tools/skills/docx/SKILL.md")
        );
        assert_eq!(
            path("/home/me/.branchyard/tools/INDEX.md", "/workspace").as_deref(),
            Some("tools/INDEX.md")
        );
    }

    #[test]
    fn an_absolute_path_elsewhere_is_reduced_to_its_basename() {
        assert_eq!(
            path("/etc/ssh/id_rsa", "/workspace").as_deref(),
            Some("id_rsa")
        );
        assert_eq!(
            path("file:///Users/me/secret.txt", "/workspace").as_deref(),
            Some("secret.txt")
        );
        assert_eq!(path("", "/workspace"), None);
    }

    #[test]
    fn a_command_is_one_line_scrubbed_and_bounded() {
        assert_eq!(
            command("API_TOKEN=abc123 curl -H 'Authorization: Bearer abc.def' https://u:pw@host/x --password hunter2", "").as_deref(),
            Some("API_TOKEN=[redacted] curl -H 'Authorization: Bearer [redacted]' https://[redacted]@host/x --password [redacted]")
        );
        assert_eq!(
            command("git push\n  --token=zzz origin ghp_abcdefghijklmnop", "").as_deref(),
            Some("git push --token=[redacted] origin [redacted]")
        );
        let long = format!("echo {}", "word ".repeat(60));
        let cut = command(&long, "").unwrap_or_default();
        assert!(cut.starts_with("echo word"));
        assert!(cut.ends_with("..."));
        assert!(cut.chars().count() <= COMMAND_MAX + 3);
        assert_eq!(command("   ", ""), None);
    }

    #[test]
    fn absolute_paths_in_a_command_are_reduced_so_its_first_words_survive_the_bound() {
        let deep = "/srv/run-1/repo/.branchyard/capabilities/ab12/.branchyard/tools/bin/tools";
        assert_eq!(
            command(
                &format!("{deep} search word document | head -3"),
                "/srv/run-1/repo"
            )
            .as_deref(),
            Some("tools/bin/tools search word document | head -3")
        );
        assert_eq!(
            command(
                "python3 /workspace/scripts/run.py --out '/home/me/out.txt'",
                "/workspace"
            )
            .as_deref(),
            Some("python3 scripts/run.py --out out.txt")
        );
        assert_eq!(
            command("cat /tools/skills/docx/SKILL.md;", "/workspace").as_deref(),
            Some("cat tools/skills/docx/SKILL.md;")
        );
        assert_eq!(
            command("cd / && ls", "/workspace").as_deref(),
            Some("cd / && ls")
        );
    }

    #[test]
    fn a_shell_wrapper_is_removed_before_the_command_is_cut() {
        assert_eq!(
            command(r#"/bin/bash -lc "cat rec.txt && wc -c < rec.txt""#, "").as_deref(),
            Some("cat rec.txt && wc -c < rec.txt")
        );
        assert_eq!(
            command("bash script.sh", "").as_deref(),
            Some("bash script.sh")
        );
    }

    #[test]
    fn input_objects_become_the_right_kind_of_target() {
        let cwd = "/workspace";
        assert_eq!(
            from_input(
                "Bash",
                &json!({"command": "ls -la", "description": "x"}),
                cwd
            ),
            ToolTarget::command("ls -la")
        );
        assert_eq!(
            from_input("Read", &json!({"file_path": "/workspace/a.txt"}), cwd),
            ToolTarget::file("a.txt", cwd)
        );
        assert_eq!(
            from_input("Skill", &json!({"skill": "docx"}), cwd),
            ToolTarget::skill("docx")
        );
        assert_eq!(
            from_input("mcp__github__list_issues", &json!({"repo": "a/b"}), cwd),
            ToolTarget::mcp(Some("github"), "list_issues")
        );
        assert_eq!(
            from_input("run_command", &json!({"CommandLine": "pwd"}), cwd),
            ToolTarget::command("pwd")
        );
        assert_eq!(from_input("Task", &json!({"prompt": "do it"}), cwd), None);
        assert_eq!(from_input("Read", &Value::Null, cwd), None);
    }

    #[test]
    fn a_file_content_key_is_never_a_target() {
        let input = json!({"content": "secret body", "text": "more"});
        assert_eq!(from_input("Write", &input, "/workspace"), None);
    }

    const AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn secrets_in_commands_are_scrubbed_in_each_shape_the_review_found() {
        let cases: Vec<(String, &str)> = vec![
            (
                "curl -u admin:hunter2 https://h/x".into(),
                "curl -u [redacted] https://h/x",
            ),
            (
                "curl --user admin:hunter2 https://h/x".into(),
                "curl --user [redacted] https://h/x",
            ),
            (
                "curl --user=admin:hunter2 https://h/x".into(),
                "curl --user=[redacted] https://h/x",
            ),
            (
                "curl -uadmin:hunter2 https://h/x".into(),
                "curl -u[redacted] https://h/x",
            ),
            (
                "mysql -u root -phunter2 db".into(),
                "mysql -u root -p[redacted] db",
            ),
            (
                "docker login -u me -p hunter2".into(),
                "docker login -u me -p [redacted]",
            ),
            (
                r#"curl -H "X-Api-Key: abc123" https://h/x"#.into(),
                r#"curl -H 'X-Api-Key: [redacted]' https://h/x"#,
            ),
            (
                "curl -H 'X-Api-Key:abc123' https://h/x".into(),
                "curl -H X-Api-Key:[redacted] https://h/x",
            ),
            (
                r#"curl -H "X-Auth-Token: abc123" https://h"#.into(),
                r#"curl -H 'X-Auth-Token: [redacted]' https://h"#,
            ),
            (
                r#"curl -H "X-Client-Secret: abc123" https://h"#.into(),
                r#"curl -H 'X-Client-Secret: [redacted]' https://h"#,
            ),
            (
                r#"curl -H "Authorization: Bearer abc.def" https://h"#.into(),
                r#"curl -H 'Authorization: Bearer [redacted]' https://h"#,
            ),
            (
                r#"curl -H "Authorization: Basic dTpw" https://h"#.into(),
                r#"curl -H 'Authorization: Basic [redacted]' https://h"#,
            ),
            (
                "curl -H 'Cookie: sid=1234' https://h".into(),
                "curl -H 'Cookie: [redacted]' https://h",
            ),
            (
                format!("aws configure set aws_secret_access_key {AWS_SECRET}"),
                "aws configure set aws_secret_access_key [redacted]",
            ),
            (format!("echo {AWS_SECRET}"), "echo [redacted]"),
            (
                "git clone https://user:pass@github.com/o/r.git".into(),
                "git clone https://[redacted]@github.com/o/r.git",
            ),
            (
                "curl 'https://h/x?access_token=abc&page=2'".into(),
                "curl https://h/x?access_token=[redacted]&page=2",
            ),
            (
                "curl https://h/x?a=1&signature=zzz&token=t".into(),
                "curl https://h/x?a=1&signature=[redacted]&token=[redacted]",
            ),
            (
                "curl https://h/x#access_token=abc".into(),
                "curl https://h/x#access_token=[redacted]",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(command(&raw, "").as_deref(), Some(want), "{raw}");
        }
    }

    #[test]
    fn quoted_secrets_with_spaces_are_redacted_whole() {
        // (command, what the logged command must be). No part of the secret
        // may remain, whatever the quoting.
        let cases: [(&str, &str); 14] = [
            (
                "client --password 'first second'",
                "client --password [redacted]",
            ),
            (
                r#"client --password "first second" next"#,
                "client --password [redacted] next",
            ),
            (
                r#"client --password "say \"hi\" there" next"#,
                "client --password [redacted] next",
            ),
            (
                "client --password='first second' next",
                "client --password=[redacted] next",
            ),
            (
                r#"client --token="first second" next"#,
                "client --token=[redacted] next",
            ),
            (
                "client --password first\\ second next",
                "client --password [redacted] next",
            ),
            (
                r#"curl -H "X-Api-Key: a b" https://h"#,
                r#"curl -H 'X-Api-Key: [redacted]' https://h"#,
            ),
            (
                "curl -H 'X-Api-Key:a b c' https://h",
                "curl -H X-Api-Key:[redacted] https://h",
            ),
            (
                "curl -H'X-Api-Key: a b' https://h",
                "curl -H 'X-Api-Key: [redacted]' https://h",
            ),
            (
                "curl --header='Cookie: sid=a b' https://h",
                "curl --header 'Cookie: [redacted]' https://h",
            ),
            (
                r#"curl -H "Authorization: Bearer a b" https://h"#,
                r#"curl -H 'Authorization: Bearer [redacted]' https://h"#,
            ),
            (
                "curl -u 'user:pa ss' https://h",
                "curl -u [redacted] https://h",
            ),
            (
                "curl -u'user:pa ss' https://h",
                "curl -u[redacted] https://h",
            ),
            (
                r#"curl --user="user:pa ss" https://h"#,
                "curl --user=[redacted] https://h",
            ),
        ];
        for (raw, want) in cases {
            let got = command(raw, "").unwrap_or_default();
            assert_eq!(got, want, "{raw}");
            for part in ["first", "second", "pa ss", "pa", "ss'", "a b", "hi"] {
                if raw.contains(part) && !want.contains(part) {
                    assert!(!got.contains(part), "{raw} -> {got} keeps {part}");
                }
            }
        }
    }

    #[test]
    fn quoted_words_with_spaces_are_scrubbed_inside_and_stay_bounded() {
        assert_eq!(
            command(r#"echo "hello token=abc world" done"#, "").as_deref(),
            Some("echo 'hello token=[redacted] world' done")
        );
        assert_eq!(
            command("echo 'see https://u:pw@h/x now'", "").as_deref(),
            Some("echo 'see https://[redacted]@h/x now'")
        );
        assert_eq!(
            command("grep 'two words' src/lib.rs", "").as_deref(),
            Some("grep 'two words' src/lib.rs")
        );
        let long = format!("client --password 'a b' {}", "word ".repeat(60));
        let cut = command(&long, "").unwrap_or_default();
        assert!(cut.starts_with("client --password [redacted] word"));
        assert!(cut.chars().count() <= COMMAND_MAX + 3);
    }

    #[test]
    fn ordinary_commands_are_not_scrubbed() {
        let kept = [
            "git push -u origin main".to_owned(),
            format!("git checkout {SHA}"),
            format!("git log --format=%H {SHA}..HEAD"),
            "ls -u src".to_owned(),
            "psql -p 5432 -U app db".to_owned(),
            "docker run -p 8080:80 image".to_owned(),
            "docker run --name monkey:latest image".to_owned(),
            "cat src/components/deeply/nested/Directory2/file.tsx".to_owned(),
            "curl https://h/search?q=rust&page=2".to_owned(),
            "curl -H 'Accept: application/json' https://h".to_owned(),
        ];
        for raw in kept {
            assert_eq!(command(&raw, "").as_deref(), Some(raw.as_str()), "{raw}");
        }
        // Outside git a commit-id-shaped word is still treated as a token.
        assert_eq!(
            command(&format!("echo {SHA}"), "").as_deref(),
            Some("echo [redacted]")
        );
    }

    #[test]
    fn uri_and_path_targets_lose_credentials_and_query_strings() {
        assert_eq!(
            path("https://u:p@h/x?access_token=abc&page=2", "/w").as_deref(),
            Some("https://[redacted]@h/x?access_token=[redacted]&page=2")
        );
        assert_eq!(
            from_input("Fetch", &json!({"uri": "https://h/x?key=abc"}), "/w"),
            ToolTarget::file("https://h/x?key=abc", "/w")
        );
        assert_eq!(
            from_input("Fetch", &json!({"uri": "https://h/x?key=abc"}), "/w")
                .map(|t| t.value)
                .as_deref(),
            Some("https://h/x?key=[redacted]")
        );
        assert_eq!(
            path("/workspace/a.txt?token=abc", "/workspace").as_deref(),
            Some("a.txt")
        );
        assert_eq!(
            path("/srv/export/report.csv?sig=zzz", "/workspace").as_deref(),
            Some("report.csv")
        );
        assert_eq!(path("?token=abc", "/w"), None);
    }

    #[test]
    fn exact_values_are_removed_whole_longest_first_and_short_ones_skipped() {
        let values = vec!["hunter2".to_owned(), "hunter22".to_owned(), "ab".to_owned()];
        let out = scrub_values("pw hunter22 then hunter2, ab stays", &values);
        assert_eq!(out.text, "pw [redacted] then [redacted], ab stays");
        assert_eq!(out.removed, [ScrubKind::Value, ScrubKind::Value]);
        assert_eq!(out.chars_removed, 15);
        let none = scrub_values("nothing here", &values);
        assert_eq!(none.text, "nothing here");
        assert!(none.removed.is_empty() && none.chars_removed == 0);
    }

    #[test]
    fn each_text_shape_is_scrubbed_and_counted_by_kind() {
        let members = vec!["ann@example.com".to_owned()];
        let cases: Vec<(String, &str, ScrubKind)> = vec![
            (
                "set AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE first".into(),
                "set AWS_ACCESS_KEY_ID=[redacted] first",
                ScrubKind::Assignment,
            ),
            (
                "the key (AKIAIOSFODNN7EXAMPLE) leaked".into(),
                "the key ([redacted]) leaked",
                ScrubKind::Token,
            ),
            (
                "use ghp_abcdefghijklmnopqrstuvwxyz012345 here".into(),
                "use [redacted] here",
                ScrubKind::Token,
            ),
            (
                "sk-abcdefghijklmnop please".into(),
                "[redacted] please",
                ScrubKind::Token,
            ),
            (
                "slack xoxb-123456789012-abcdefghij".into(),
                "slack [redacted]",
                ScrubKind::Token,
            ),
            (
                format!("secret {AWS_SECRET} ok"),
                "secret [redacted] ok",
                ScrubKind::Token,
            ),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl end".into(),
                "jwt [redacted] end",
                ScrubKind::Jwt,
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIE\nAAA\n-----END RSA PRIVATE KEY-----\nafter"
                    .into(),
                "[redacted]\nafter",
                ScrubKind::Pem,
            ),
            (
                "Authorization: Bearer abc.def-ghi then".into(),
                "Authorization: Bearer [redacted] then",
                ScrubKind::Bearer,
            ),
            (
                "curl -H 'Authorization: Basic dTpw' x".into(),
                "curl -H 'Authorization: Basic [redacted]' x",
                ScrubKind::Bearer,
            ),
            (
                "login with password=hunter2, then".into(),
                "login with password=[redacted], then",
                ScrubKind::Assignment,
            ),
            (
                "api_key: abc123 and more".into(),
                "api_key: [redacted] and more",
                ScrubKind::Assignment,
            ),
            (
                "token=0123456789abcdef0123456789abcdef0123".into(),
                "token=[redacted]",
                ScrubKind::Assignment,
            ),
            (
                "the secret 0123456789abcdef0123456789abcdef01 was".into(),
                "the secret [redacted] was",
                ScrubKind::KeyedRun,
            ),
            (
                "see https://u:p@h/x?token=abc#frag now".into(),
                "see https://[redacted]@h/x now",
                ScrubKind::Url,
            ),
            (
                "see https://h/x?page=2 now".into(),
                "see https://h/x now",
                ScrubKind::Url,
            ),
            (
                "ask bob@example.com about it".into(),
                "ask <email> about it",
                ScrubKind::Email,
            ),
        ];
        for (raw, want, kind) in cases {
            let out = scrub_shapes(&raw, &members);
            assert_eq!(out.text, want, "{raw}");
            assert_eq!(out.removed, [kind], "{raw}");
            assert!(out.chars_removed > 0, "{raw}");
        }
    }

    #[test]
    fn ordinary_text_members_and_commit_ids_are_kept() {
        let members = vec!["Ann@Example.com".to_owned()];
        let kept = [
            "Run cargo fmt before you finish.".to_owned(),
            format!("see commit {SHA} on main"),
            "ask ann@example.com, who wrote it".to_owned(),
            "the parser's tests live next to it (crates/parser/tests)".to_owned(),
            "https://github.com/o/r/pull/4 has the review".to_owned(),
            "not an email: foo@bar".to_owned(),
            "a long word abcdefghijklmnopqrstuvwxyz0123456789 stays without a key before it"
                .to_owned(),
        ];
        for text in kept {
            let out = scrub_shapes(&text, &members);
            assert_eq!(out.text, text);
            assert!(out.removed.is_empty(), "{text}: {:?}", out.removed);
            assert_eq!(out.chars_removed, 0);
        }
        let pem = scrub_shapes("-----BEGIN CERTIFICATE-----\nAAA", &[]);
        assert_eq!(pem.text, "[redacted]");
        assert_eq!(pem.removed, [ScrubKind::Pem]);
    }
}
