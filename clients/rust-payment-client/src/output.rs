//! Canonical: docs/technical/rmpc-read-output-contract.md
//!
//! One owner for "serialise a command's output document to stdout".
//!
//! Every `rmpc` subcommand ends by printing a single JSON document on
//! stdout, compact by default and pretty-printed under `--pretty`. That
//! four-line body was copy-pasted into 19 command modules (issue #1285),
//! each with its own `.expect(...)` message and two of them silently
//! degrading to `unwrap_or_default()` — i.e. printing an empty string
//! instead of failing — on a serialisation error.
//!
//! Serialisation of these output structs is infallible in practice: they
//! are plain `#[derive(Serialize)]` records of strings, integers, bools
//! and vectors, with no maps keyed on non-strings and no custom impls
//! that can fail. A failure here is a programming error in the output
//! type, so it panics loudly rather than emitting a document that
//! downstream tooling would parse as empty.

use serde::Serialize;

/// Print `value` as a JSON document on stdout, followed by a newline.
///
/// `pretty` selects `serde_json::to_string_pretty`; otherwise the
/// compact form is used. This is the only place any command writes its
/// output document.
pub fn emit<T: Serialize>(value: &T, pretty: bool) {
    println!("{}", render(value, pretty));
}

/// The exact string [`emit`] prints. Split out so the rendering rule is
/// testable without capturing stdout.
fn render<T: Serialize>(value: &T, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("rmpc command output serialises")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Doc {
        status: &'static str,
        amount: String,
    }

    fn doc() -> Doc {
        Doc {
            status: "success",
            amount: "12".to_string(),
        }
    }

    #[test]
    fn compact_render_is_single_line_json() {
        let out = render(&doc(), false);
        assert!(
            !out.contains('\n'),
            "compact output must be one line: {out:?}"
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["amount"], "12");
    }

    #[test]
    fn pretty_render_carries_the_same_fields() {
        let out = render(&doc(), true);
        assert!(
            out.contains('\n'),
            "pretty output must be multi-line: {out:?}"
        );
        let pretty: serde_json::Value = serde_json::from_str(&out).unwrap();
        let compact: serde_json::Value = serde_json::from_str(&render(&doc(), false)).unwrap();
        assert_eq!(pretty, compact);
    }
}
