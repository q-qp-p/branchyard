// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: _strip_managed_block,
// _markdown_section and project_instructions of harnesses/scion_harness.py.
// Tests from harnesses/scion_harness_test.py,
// harnesses/codex/provision_test.py and harnesses/hermes/provision_test.py.
// Copyright 2026 Google LLC. Licensed under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to Rust as pure functions
// over the file's content (the caller reads and writes it); Branchyard's
// own markers are written, and Scion's are still recognized and replaced;
// a begin marker without an end marker makes merge_block refuse, where
// Scion would add a second block; skills are not inlined, because no
// translated harness needs it.

//! Standing instructions kept in a harness's instruction file between
//! markers, so what a person wrote outside them stays.

/// The markers Branchyard writes.
pub const BEGIN: &str = "<!-- BEGIN BRANCHYARD MANAGED -->";
pub const END: &str = "<!-- END BRANCHYARD MANAGED -->";

/// Begin markers recognized, in the order they are looked for: Branchyard's,
/// then Scion's.
const BEGIN_MARKERS: &[&str] = &[
    BEGIN,
    "<!-- BEGIN SCION MANAGED CODEX INSTRUCTIONS -->",
    "<!-- BEGIN SCION MANAGED HERMES INSTRUCTIONS -->",
    "<!-- SCION_MANAGED_BEGIN -->",
    "<!-- BEGIN SCION MANAGED -->",
];

const END_MARKERS: &[&str] = &[
    END,
    "<!-- END SCION MANAGED CODEX INSTRUCTIONS -->",
    "<!-- END SCION MANAGED HERMES INSTRUCTIONS -->",
    "<!-- SCION_MANAGED_END -->",
    "<!-- END SCION MANAGED -->",
];

/// The begin marker of a managed block in a file whose comments are `#`
/// lines (YAML, TOML, dotenv), where an HTML comment is not a comment.
pub const COMMENT_BEGIN: &str = "# BEGIN BRANCHYARD MANAGED";
/// The end marker of that block.
pub const COMMENT_END: &str = "# END BRANCHYARD MANAGED";

/// A file's marker dialect: what is written, and what is recognized.
struct Markers {
    begin: &'static str,
    end: &'static str,
    begins: &'static [&'static str],
    ends: &'static [&'static str],
}

/// HTML comments, for Markdown instruction files.
const HTML: Markers = Markers {
    begin: BEGIN,
    end: END,
    begins: BEGIN_MARKERS,
    ends: END_MARKERS,
};

/// `#` comment lines, for YAML and its kin.
const COMMENT: Markers = Markers {
    begin: COMMENT_BEGIN,
    end: COMMENT_END,
    begins: &[COMMENT_BEGIN],
    ends: &[COMMENT_END],
};

/// Where a managed block is.
enum Found {
    None,
    Block(usize, usize),
    /// A begin marker at this offset with no end marker after it.
    Unclosed,
}

fn find_block_with(markers: &Markers, content: &str) -> Found {
    let Some((start, begin)) = markers
        .begins
        .iter()
        .find_map(|m| content.find(m).map(|i| (i, *m)))
    else {
        return Found::None;
    };
    let after = start + begin.len();
    match markers
        .ends
        .iter()
        .find_map(|m| content[after..].find(m).map(|i| after + i + m.len()))
    {
        Some(end) => Found::Block(start, end),
        None => Found::Unclosed,
    }
}

/// `content` without its managed block. Content with a begin marker but no
/// end marker is returned unchanged, so nothing is lost.
pub fn strip_managed_block(content: &str) -> String {
    strip_with(&HTML, content)
}

fn strip_with(markers: &Markers, content: &str) -> String {
    match find_block_with(markers, content) {
        Found::Block(start, end) => {
            format!(
                "{}\n",
                format!("{}{}", &content[..start], &content[end..]).trim()
            )
        }
        Found::None | Found::Unclosed => content.to_owned(),
    }
}

/// `# title`, a blank line and the trimmed body; empty for an empty body.
pub fn section(title: &str, body: &str) -> String {
    match body.trim() {
        "" => String::new(),
        body => format!("# {title}\n\n{body}\n"),
    }
}

/// The managed block's body from sections, as Scion composes them.
pub fn compose(sections: &[String]) -> Option<String> {
    let parts: Vec<&str> = sections
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Replace the managed block of `current` with `body` (none: remove it).
/// The block comes first and user content after it. Returns `None` when
/// nothing is left, so the file is removed.
pub fn merge_block(current: Option<&str>, body: Option<&str>) -> Result<Option<String>, String> {
    merge_with(&HTML, current, body)
}

/// [`merge_block`] for a file whose comments are `#` lines, between
/// [`COMMENT_BEGIN`] and [`COMMENT_END`]. The body is the file's own
/// syntax (a YAML mapping, say); the markers are comments to it.
pub fn merge_comment_block(
    current: Option<&str>,
    body: Option<&str>,
) -> Result<Option<String>, String> {
    merge_with(&COMMENT, current, body)
}

fn merge_with(
    markers: &Markers,
    current: Option<&str>,
    body: Option<&str>,
) -> Result<Option<String>, String> {
    let current = current.unwrap_or("");
    if matches!(find_block_with(markers, current), Found::Unclosed) {
        return Err(
            "has a managed-block begin marker without an end marker; not editing it".into(),
        );
    }
    let existing = strip_with(markers, current);
    let (begin, end) = (markers.begin, markers.end);
    let managed = body
        .filter(|b| !b.trim().is_empty())
        .map(|b| format!("{begin}\n\n{}\n\n{end}\n", b.trim()));
    let unmanaged = match existing.trim() {
        "" => None,
        text => Some(format!("{text}\n")),
    };
    let content = match (managed, unmanaged) {
        (None, None) => return Ok(None),
        (Some(managed), None) => managed,
        (None, Some(unmanaged)) => unmanaged,
        (Some(managed), Some(unmanaged)) => format!("{managed}\n{unmanaged}"),
    };
    // Unchanged content is returned exactly.
    Ok(Some(if content == current {
        current.to_owned()
    } else {
        content
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_BEGIN: &str = "<!-- BEGIN SCION MANAGED CODEX INSTRUCTIONS -->";
    const LEGACY_END: &str = "<!-- END SCION MANAGED CODEX INSTRUCTIONS -->";

    // From Scion's scion_harness_test.py, TestStripManagedBlock.

    #[test]
    fn strip_standard_markers() {
        let content =
            "before\n<!-- BEGIN SCION MANAGED -->\nmanaged\n<!-- END SCION MANAGED -->\nafter";
        let result = strip_managed_block(content);
        assert!(result.contains("before") && result.contains("after"));
        assert!(!result.contains("managed"));
    }

    #[test]
    fn strip_codex_legacy_markers() {
        let content = format!("user\n{LEGACY_BEGIN}\nstuff\n{LEGACY_END}\nrest");
        let result = strip_managed_block(&content);
        assert!(result.contains("user") && result.contains("rest"));
        assert!(!result.contains("stuff"));
    }

    #[test]
    fn strip_hermes_and_copilot_legacy_markers() {
        for (begin, end) in [
            (
                "<!-- BEGIN SCION MANAGED HERMES INSTRUCTIONS -->",
                "<!-- END SCION MANAGED HERMES INSTRUCTIONS -->",
            ),
            ("<!-- SCION_MANAGED_BEGIN -->", "<!-- SCION_MANAGED_END -->"),
            (BEGIN, END),
        ] {
            let content = format!("user\n{begin}\nstuff\n{end}\nrest");
            assert!(!strip_managed_block(&content).contains("stuff"), "{begin}");
        }
    }

    #[test]
    fn unclosed_marker_preserved() {
        let content = "before\n<!-- BEGIN SCION MANAGED -->\nunclosed content";
        assert_eq!(strip_managed_block(content), content);
        // Scion would prepend a second block; Branchyard refuses.
        assert!(merge_block(Some(content), Some("new")).is_err());
    }

    #[test]
    fn no_markers() {
        assert!(strip_managed_block("just plain text\n").contains("just plain text"));
    }

    // From Scion's TestProjectInstructions and codex/provision_test.py.

    #[test]
    fn composes_sections_once_however_often_it_runs() {
        let body = compose(&[
            section("System Instruction", "System rules"),
            section("Agent Instructions", "Agent rules"),
        ])
        .unwrap();
        let once = merge_block(None, Some(&body)).unwrap().unwrap();
        let twice = merge_block(Some(&once), Some(&body)).unwrap().unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert!(twice.contains(
            "# System Instruction\n\nSystem rules\n\n# Agent Instructions\n\nAgent rules"
        ));
        assert!(twice.contains(END));
    }

    #[test]
    fn strips_existing_managed_block() {
        let current =
            "<!-- BEGIN SCION MANAGED -->\nold\n<!-- END SCION MANAGED -->\nuser content\n";
        let body = compose(&[section("Agent Instructions", "New instructions.")]).unwrap();
        let out = merge_block(Some(current), Some(&body)).unwrap().unwrap();
        assert!(!out.contains("old"));
        assert!(out.contains("New instructions.") && out.contains("user content"));
        assert!(out.starts_with(BEGIN));
    }

    #[test]
    fn cleans_stale_managed_block_when_inputs_empty() {
        let current = format!(
            "{LEGACY_BEGIN}\n\n# Agent Instructions\n\nOld managed content\n\n{LEGACY_END}\n\n\
             # User Notes\n\nKeep this.\n"
        );
        let out = merge_block(Some(&current), None).unwrap().unwrap();
        assert_eq!(out, "# User Notes\n\nKeep this.\n");
    }

    #[test]
    fn removes_file_when_only_stale_managed_block_remains() {
        let current = format!(
            "{LEGACY_BEGIN}\n\n# Agent Instructions\n\nOld managed content\n\n{LEGACY_END}\n"
        );
        assert_eq!(merge_block(Some(&current), None).unwrap(), None);
        assert_eq!(merge_block(None, None).unwrap(), None);
    }

    /// `#` markers for a YAML file: the block is merged, replaced and
    /// removed the same way, and an HTML-comment block in the same file is
    /// not touched by it.
    #[test]
    fn comment_blocks_merge_in_yaml() {
        let first = merge_comment_block(
            Some("model: glm\n"),
            Some("approvals:\n  deny:\n    - \"*\""),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            first,
            "# BEGIN BRANCHYARD MANAGED\n\napprovals:\n  deny:\n    - \"*\"\n\n# END BRANCHYARD MANAGED\n\nmodel: glm\n"
        );
        let again = merge_comment_block(Some(&first), Some("approvals:\n  deny:\n    - \"*\""))
            .unwrap()
            .unwrap();
        assert_eq!(again, first);
        let replaced = merge_comment_block(Some(&first), Some("approvals: {}"))
            .unwrap()
            .unwrap();
        assert!(replaced.contains("approvals: {}") && !replaced.contains("deny"));
        assert_eq!(
            merge_comment_block(Some(&first), None).unwrap().unwrap(),
            "model: glm\n"
        );
        assert!(merge_comment_block(Some("# BEGIN BRANCHYARD MANAGED\nx"), Some("y")).is_err());
        assert_eq!(merge_comment_block(None, None).unwrap(), None);
        // The HTML dialect leaves a comment block alone and vice versa.
        assert_eq!(strip_managed_block(&first), first);
    }
}
