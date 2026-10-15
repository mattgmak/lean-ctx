// SPDX-License-Identifier: Apache-2.0

//! Turns a tool's error message into a template that is safe to report.
//!
//! The approach follows crash-reporting scrubbers (Sentry's PII scrubbing,
//! VS Code's telemetry path cleaning), but is stricter: instead of removing
//! what looks sensitive, only what looks like LeanCTX's own static wording
//! survives. Everything variable becomes a placeholder:
//!
//! - quoted text → `‹str›`, URLs → `‹url›`, e-mail addresses → `‹email›`
//! - paths and file names → `‹path›`
//! - numbers and anything containing a digit → `‹n›`
//! - identifiers (camelCase, snake_case, `::`, long tokens) → `‹sym›`,
//!   except LeanCTX's own tool and parameter names
//! - non-ASCII words → `‹text›`
//!
//! A template without plain words, or with characters outside a small safe
//! set, is dropped entirely; only the failure class is reported then.

use std::sync::OnceLock;

use regex::Regex;

/// Longest template that is reported.
pub const MAX_TEMPLATE_CHARS: usize = 160;
/// Input beyond this is not considered (the first line is what matters).
const MAX_INPUT_CHARS: usize = 400;

/// LeanCTX's own parameter names: kept verbatim because they explain errors.
const KNOWN_IDENTIFIERS: &[&str] = &[
    "old_string",
    "new_string",
    "replace_all",
    "file_path",
    "start_line",
    "end_line",
    "max_results",
    "dry_run",
    "old_text",
    "new_text",
    "replace_unique",
    "set_line",
    "replace_lines",
    "insert_after",
    "replace_symbol",
    "timeout_ms",
    "run_in_background",
    "job_id",
    "doc_id",
    "if_version",
];

fn quoted() -> &'static Regex {
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    QUOTED.get_or_init(|| {
        Regex::new(r#""[^"\n]{0,300}"|`[^`\n]{0,300}`|'[^'\s][^'\n]{0,300}'|«[^»\n]{0,300}»"#)
            .expect("static regex")
    })
}

fn is_known_identifier(token: &str) -> bool {
    token.starts_with("ctx_")
        && token[4..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_')
        || KNOWN_IDENTIFIERS.contains(&token)
}

fn is_path_like(token: &str) -> bool {
    if token.contains('/') || token.contains('\\') || token.starts_with('~') {
        return true;
    }
    // file.ext with a short alphanumeric extension: main.rs, Cargo.toml
    match token.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
                && ext.bytes().any(|b| b.is_ascii_alphabetic())
        }
        None => false,
    }
}

fn is_identifier_like(token: &str) -> bool {
    let has_inner_upper = token.chars().skip(1).any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    token.contains("::")
        || token.contains('_')
        || token.contains('$')
        || token.contains('#')
        || token.contains('=')
        || (has_inner_upper && has_lower)
        || token.len() > 24
}

/// Classify one whitespace-separated token, keeping its outer punctuation.
fn scrub_token(token: &str) -> String {
    const EDGE: &[char] = &[
        '(', ')', '[', ']', '{', '}', ',', ';', ':', '.', '!', '?', '<', '>',
    ];
    let core = token.trim_matches(|c| EDGE.contains(&c));
    if core.is_empty() {
        return token.to_string();
    }
    let start = token.find(core).unwrap_or(0);
    let (lead, trail) = (&token[..start], &token[start + core.len()..]);
    let replaced = if core.starts_with('‹') && core.ends_with('›') {
        core.to_string()
    } else if !core.is_ascii() {
        "‹text›".to_string()
    } else if core.contains("://") || core.starts_with("www.") {
        "‹url›".to_string()
    } else if core.contains('@') && core.contains('.') {
        "‹email›".to_string()
    } else if is_known_identifier(core) {
        core.to_string()
    } else if is_path_like(core) {
        "‹path›".to_string()
    } else if core.bytes().any(|b| b.is_ascii_digit()) {
        "‹n›".to_string()
    } else if is_identifier_like(core) {
        "‹sym›".to_string()
    } else if core
        .bytes()
        .all(|b| b.is_ascii_alphabetic() || b == b'-' || b == b'\'')
    {
        core.to_string()
    } else {
        "‹sym›".to_string()
    };
    format!("{lead}{replaced}{trail}")
}

/// The reportable template of an error message, or `None` when nothing safe
/// and meaningful is left.
pub fn message_template(message: &str) -> Option<String> {
    let line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line: String = line.chars().take(MAX_INPUT_CHARS).collect();
    let unquoted = quoted().replace_all(&line, " ‹str› ");
    let mut words: Vec<String> = Vec::new();
    for token in unquoted.split_whitespace() {
        let scrubbed = scrub_token(token);
        // Collapse runs of the same placeholder ("‹n› ‹n›" → "‹n›").
        if scrubbed.starts_with('‹') && words.last() == Some(&scrubbed) {
            continue;
        }
        words.push(scrubbed);
    }
    let mut template = words.join(" ");
    if template.chars().count() > MAX_TEMPLATE_CHARS {
        let cut: String = template.chars().take(MAX_TEMPLATE_CHARS - 1).collect();
        template = match cut.rfind(' ') {
            Some(space) if space > MAX_TEMPLATE_CHARS / 2 => format!("{}…", &cut[..space]),
            _ => format!("{cut}…"),
        };
    }
    let safe = template
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " .,:;()[]{}<>!?'-_/‹›…".contains(c));
    let words = template
        .split_whitespace()
        .filter(|word| {
            word.chars().filter(char::is_ascii_alphabetic).count() >= 2 && !word.contains('‹')
        })
        .count();
    (safe && words >= 2).then_some(template)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_parts_become_placeholders_and_static_wording_stays() {
        for (message, expected) in [
            (
                "old_string not found in /Users/anna/acme/src/payroll.rs",
                "old_string not found in ‹path›",
            ),
            (
                "Permission denied (os error 13): C:\\Users\\bob\\secret.txt",
                "Permission denied (os error ‹n›): ‹path›",
            ),
            (
                "command timed out after 120s",
                "command timed out after ‹n›",
            ),
            (
                "unknown parameter 'customerName' for ctx_read",
                "unknown parameter ‹str› for ctx_read",
            ),
            (
                "no symbol PayrollService::compute_net in index",
                "no symbol ‹sym› in index",
            ),
            (
                "failed to fetch https://intranet.acme.example/api?id=42 connection refused",
                "failed to fetch ‹url› connection refused",
            ),
            (
                "invite for anna.meier@acme.ch rejected",
                "invite for ‹email› rejected",
            ),
            (
                "CONFLICT: line 12 hash 9f3a2b1c changed since read",
                "CONFLICT: line ‹n› hash ‹n› changed since read",
            ),
            (
                "Datei \"Gehaltsliste März.xlsx\" wurde nicht gefunden",
                "Datei ‹str› wurde nicht gefunden",
            ),
        ] {
            assert_eq!(
                message_template(message).as_deref(),
                Some(expected),
                "{message}"
            );
        }
    }

    #[test]
    fn only_the_first_line_and_a_bounded_length_are_reported() {
        let template =
            message_template("first line fails here\nsecret second line /etc/passwd").unwrap();
        assert_eq!(template, "first line fails here");
        let long = format!("error {}", "word ".repeat(100));
        assert!(message_template(&long).unwrap().chars().count() <= MAX_TEMPLATE_CHARS);
    }

    #[test]
    fn messages_without_safe_wording_are_dropped() {
        for message in [
            "",
            "   ",
            "/Users/anna/acme/src/payroll.rs",
            "42",
            "Ünïcödé ñämes ønly",
            "x",
        ] {
            assert_eq!(message_template(message), None, "{message:?}");
        }
    }

    #[test]
    fn templates_never_contain_digits_paths_or_quotes() {
        for message in [
            "read failed for ~/notes/2026-q3.md after 3 retries",
            "token abc123def rejected by provider",
            "edit at offset 1234 of `fn main() { println!(\"hi\") }` failed",
            "user \"Anna Meier\" lacks role admin",
        ] {
            let template = message_template(message).unwrap_or_default();
            assert!(!template.chars().any(|c| c.is_ascii_digit()), "{template}");
            for leak in [
                "anna", "Anna", "Meier", "notes", "abc123", "println", "main()",
            ] {
                assert!(!template.contains(leak), "{template} leaks {leak}");
            }
        }
    }
}
