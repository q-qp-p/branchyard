//! The value-and-shape scrubber an observation goes through before it is
//! stored (`docs/memory.md`, section 2). It runs in the engine that ran the
//! turn, the one process that still holds the turn's secret **values**:
//! every exact value, and its base64, URL-encoded, JSON-escaped and hex
//! forms, goes first;
//! then the shapes [`branchyard_harness::target::scrub_shapes`] knows
//! (well-known tokens, JWTs, PEM blocks, `Bearer`, secret-named
//! assignments, keyed hex and base64 runs, URL queries, emails outside the
//! members). An observation that is more than a quarter scrubbed is dropped.

use base64::Engine as _;
use branchyard_harness::target::{self, ScrubKind};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// What RFC 3986 leaves unescaped: letters, digits and `-._~`.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// What `encodeURIComponent` leaves unescaped: `UNRESERVED` and `!*'()`.
const COMPONENT: &AsciiSet = &UNRESERVED
    .remove(b'!')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

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
    /// standard and URL-safe base64, with and without padding,
    /// percent-encoded, JSON-escaped and hex-encoded) and `members`
    /// (lowercased names and emails).
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

/// `text` as the inside of a JSON string; `ascii` also escapes every
/// non-ASCII character as `\uXXXX`, as `ensure_ascii` serializers do, and
/// `slashes` escapes `/` as `\/`.
fn json_inner(text: &str, ascii: bool, slashes: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '/' if slashes => out.push_str("\\/"),
            c if (c as u32) < 0x20 || (ascii && !c.is_ascii()) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `encoded` with its percent escapes in lowercase hex.
fn lower_escapes(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    let mut chars = encoded.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '%' {
            out.extend(chars.by_ref().take(2).map(|h| h.to_ascii_lowercase()));
        }
    }
    out
}

/// The encodings a secret value may appear under in a tool's output:
/// base64 (four alphabets), hex, percent-encoding in each common dialect
/// (every non-alphanumeric escaped; RFC 3986's unreserved `-._~` kept;
/// `encodeURIComponent`'s; form encoding with `+` for a space; each also
/// with lowercase hex escapes), JSON string escapes, and, for a value of
/// several lines, each line on its own, since a log or a cut tail shows a
/// multi-line value a part at a time.
fn encodings(secret: &str) -> Vec<String> {
    let bytes = secret.as_bytes();
    let mut out = vec![
        base64::engine::general_purpose::STANDARD.encode(bytes),
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes),
        base64::engine::general_purpose::URL_SAFE.encode(bytes),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        hex::encode(bytes),
        hex::encode_upper(bytes),
    ];
    for set in [NON_ALPHANUMERIC, UNRESERVED, COMPONENT] {
        let encoded = utf8_percent_encode(secret, set).to_string();
        let form = encoded.replace("%20", "+");
        out.push(lower_escapes(&encoded));
        out.push(lower_escapes(&form));
        out.push(form);
        out.push(encoded);
    }
    for ascii in [false, true] {
        for slashes in [false, true] {
            out.push(json_inner(secret, ascii, slashes));
        }
    }
    if secret.contains('\n') {
        out.extend(
            secret
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
    }
    out.retain(|form| form != secret);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILLER: &str = "The parser's benches want the large fixture, and the small one says \
                          nothing about real inputs, so compare both runs against main before \
                          you report any number, and say which fixture each number came from, \
                          every time. Keep the comparison in the pull request's description, \
                          beside the command that produced it, so a reviewer can run the same \
                          thing and see the same result again, whenever they want to look.";

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

    /// Every spelling of a value that a log can hold is removed, whole.
    #[test]
    fn percent_json_and_hex_forms_of_a_value_are_removed() {
        // (value, spellings besides the value itself)
        let table: [(&str, &[&str]); 4] = [
            // RFC 3986 leaves `-` alone; `!` is escaped.
            ("ab-cd!xyz", &["ab-cd%21xyz", "ab%2Dcd%21xyz", "ab-cd%21xyz"]),
            (
                "p@ss w0rd~x.y_z",
                &[
                    "p%40ss%20w0rd~x.y_z",
                    "p%40ss+w0rd~x.y_z",
                    "p%40ss+w0rd%7Ex%2Ey%5Fz",
                    "p%40ss%20w0rd%7ex%2ey%5fz",
                ],
            ),
            (
                "se\"cr\\et/v\u{e9}\nlinez",
                &[
                    "se\\\"cr\\\\et/v\u{e9}\\nlinez",
                    "se\\\"cr\\\\et\\/v\\u00e9\\nlinez",
                    "se%22cr%5Cet%2Fv%C3%A9%0Alinez",
                ],
            ),
            ("tok3n-value", &["746f6b336e2d76616c7565", "746F6B336E2D76616C7565"]),
        ];
        for (value, forms) in table {
            let scrubber = Scrubber::new(&[value.to_owned()], &[]);
            for form in std::iter::once(&value).chain(forms.iter()) {
                let text = format!("{FILLER} seen as {form} here");
                let Some((out, kinds)) = scrubber.scrub(&text) else {
                    panic!("dropped instead of scrubbed: {form}");
                };
                assert!(!out.contains(form), "{form} kept in {out}");
                assert_eq!(kinds, [ScrubKind::Value], "{form}");
            }
        }
    }

    #[test]
    fn a_multi_line_value_is_removed_a_line_at_a_time() {
        let value = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nKgwggSkAgEAAoIBAQC7VJTUt9Us8cKj\n-----END PRIVATE KEY-----";
        let scrubber = Scrubber::new(&[value.to_owned()], &[]);
        // The middle of the value, with both ends cut away.
        let text = format!("{FILLER}\nKgwggSkAgEAAoIBAQC7VJTUt9Us8cKj\n{FILLER}");
        let (out, _) = scrubber.scrub(&text).unwrap();
        assert!(!out.contains("KgwggSkAgEAAoIBAQC7VJTUt9Us8cKj"), "{out}");
    }
}
