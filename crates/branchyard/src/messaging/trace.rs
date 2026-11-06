//! The W3C `traceparent` a message carries: checked for form, never made
//! here (a turn's comes from `TRACEPARENT`, which a server sets from its
//! operation's span).

use crate::Error;

/// Check that `text` is a W3C `traceparent`:
/// `VV-TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT-SSSSSSSSSSSSSSSS-FF`, lower-case
/// hex, version not `ff`, trace and parent ids not all zeros.
pub fn check(text: &str) -> Result<(), Error> {
    let refuse = |why: &str| {
        Err(Error::Denied(format!(
            "trace {text:?} is not a W3C traceparent (version-trace_id-parent_id-flags in \
             lower-case hex): {why}"
        )))
    };
    let parts: Vec<&str> = text.split('-').collect();
    let [version, trace_id, parent_id, flags] = parts.as_slice() else {
        return refuse("it does not have four parts");
    };
    let hex = |part: &str, len: usize| {
        part.len() == len
            && part
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    };
    if !hex(version, 2) || !hex(trace_id, 32) || !hex(parent_id, 16) || !hex(flags, 2) {
        return refuse("a part has the wrong length or is not lower-case hex");
    }
    if *version == "ff" {
        return refuse("version ff is invalid");
    }
    if trace_id.chars().all(|c| c == '0') || parent_id.chars().all(|c| c == '0') {
        return refuse("an all-zero id is invalid");
    }
    Ok(())
}

#[allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    #[test]
    fn a_traceparent_is_checked_for_form() {
        check(GOOD).unwrap();
        for (bad, why) in [
            (
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331",
                "four parts",
            ),
            (
                "00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01",
                "lower-case hex",
            ),
            (
                "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                "version ff",
            ),
            (
                "00-00000000000000000000000000000000-b7ad6b7169203331-01",
                "all-zero",
            ),
            (
                "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
                "all-zero",
            ),
            ("", "four parts"),
        ] {
            match check(bad) {
                Err(Error::Denied(message)) => assert!(message.contains(why), "{bad}: {message}"),
                other => panic!("{bad}: {other:?}"),
            }
        }
    }
}
