//! The value-and-shape scrubber an observation goes through before it is
//! stored (`docs/memory.md`, section 2). It runs in the engine that ran the
//! turn, the one process that still holds the turn's secret **values**:
//! every exact value, and its base64 and URL-encoded forms, goes first;
//! then the shapes [`branchyard_harness::target::scrub_shapes`] knows
//! (well-known tokens, JWTs, PEM blocks, `Bearer`, secret-named
//! assignments, keyed hex and base64 runs, URL queries, emails outside the
//! members). An observation that is more than a quarter scrubbed is dropped.

use base64::Engine as _;
use branchyard_harness::target::{self, ScrubKind};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

/// The share of an observation's characters the scrubber may remove before
/// the observation is dropped instead: a quarter.
pub const DROP_OVER: f64 = 0.25;

/// A turn's scrubber: the exact values to remove, in every encoding, and
/// the members whose email addresses stay.
#[derive(Clone, Debug, Default)]
pub struct Scrubber {
    values: Vec<String>,
    members: Vec<String>,
}

impl Scrubber {
    /// A scrubber for `secrets` (exact values; each is also removed as
    /// standard and URL-safe base64, with and without padding, and
    /// percent-encoded) and `members` (lowercased names and emails).
    pub fn new(secrets: &[String], members: &[String]) -> Scrubber {
        let mut values = Vec::new();
        for secret in secrets {
            if secret.is_empty() {
                continue;
            }
            values.push(secret.clone());
            values.extend(encodings(secret));
        }
        values.sort();
        values.dedup();
        Scrubber {
            values,
            members: members.to_vec(),
        }
    }

    /// `text` scrubbed, with what was removed by kind; `None` when more
    /// than [`DROP_OVER`] of its characters were secrets.
    pub fn scrub(&self, text: &str) -> Option<(String, Vec<ScrubKind>)> {
        let total = text.chars().count();
        let values = target::scrub_values(text, &self.values);
        let shapes = target::scrub_shapes(&values.text, &self.members);
        let removed = values.chars_removed + shapes.chars_removed;
        if total > 0 && (removed as f64) / (total as f64) > DROP_OVER {
            return None;
        }
        let mut kinds = values.removed;
        kinds.extend(shapes.removed);
        Some((shapes.text, kinds))
    }
}

/// The encodings a secret value may appear under in a tool's output.
fn encodings(secret: &str) -> Vec<String> {
    let bytes = secret.as_bytes();
    let mut out = vec![
        base64::engine::general_purpose::STANDARD.encode(bytes),
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes),
        base64::engine::general_purpose::URL_SAFE.encode(bytes),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
    ];
    let encoded = utf8_percent_encode(secret, NON_ALPHANUMERIC).to_string();
    if encoded != secret {
        out.push(encoded);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_values_go_in_every_encoding_then_shapes() {
        let scrubber = Scrubber::new(&["hunter2!".into()], &["ann@example.com".into()]);
        // Enough ordinary words around the secrets that less than a quarter
        // of the text goes; the drop rule is the next test's.
        let filler = "The parser's benches want the large fixture, and the small one says \
                      nothing about real inputs, so compare both runs against main before you \
                      report any number, and say which fixture each number came from, every time. \
                      Keep the comparison in the pull request's description, beside the command \
                      that produced it, so a reviewer can run the same thing and see the same.";
        let text = format!(
            "{filler} pw hunter2! b64 aHVudGVyMiE= nopad aHVudGVyMiE url hunter2%21 then \
             ghp_abcdefghijklmnopqrstuvwxyz0123 for bob@example.com and ann@example.com"
        );
        let (out, kinds) = scrubber.scrub(&text).unwrap();
        assert_eq!(
            out,
            format!(
                "{filler} pw [redacted] b64 [redacted] nopad [redacted] url [redacted] then \
                 [redacted] for <email> and ann@example.com"
            )
        );
        assert_eq!(
            kinds,
            [
                ScrubKind::Value,
                ScrubKind::Value,
                ScrubKind::Value,
                ScrubKind::Value,
                ScrubKind::Token,
                ScrubKind::Email
            ]
        );
    }

    #[test]
    fn a_text_that_is_mostly_secret_is_dropped() {
        let scrubber = Scrubber::new(&["s3cretvalue".into()], &[]);
        assert!(scrubber.scrub("the key is s3cretvalue").is_none());
        let (kept, kinds) = scrubber
            .scrub("a long sentence about the parser and its fixtures, then s3cretvalue once")
            .unwrap();
        assert!(kept.ends_with("then [redacted] once"));
        assert_eq!(kinds, [ScrubKind::Value]);
        assert_eq!(
            Scrubber::default().scrub("").unwrap(),
            (String::new(), Vec::new())
        );
    }
}
