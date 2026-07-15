//! Per-turn effort routing (#1148, opt-in dynamic thinking budget).
//!
//! Unlike the static `effort.rs` which applies a constant reasoning level
//! across all turns (for cache stability), this module classifies each turn
//! and adjusts thinking effort dynamically.
//!
//! **Opt-in only** (`proxy.effort_routing = true`). When disabled, the static
//! `effort.rs` path remains the sole controller. When enabled, this module
//! overrides the static level with a per-turn classification.
//!
//! ## Cache stability tradeoff
//!
//! Provider prompt caches (Anthropic `cache_control`, OpenAI prefix caching)
//! break when reasoning parameters change. This module accepts that tradeoff
//! because:
//! 1. Output tokens on Opus-class models cost **5x** input tokens — savings
//!    from reduced thinking often exceed the cache-miss penalty.
//! 2. Routine turns (file reads, passing tests) generate disproportionate
//!    thinking waste for trivial tool-result acknowledgements.
//! 3. The module uses a **two-level** strategy (not N levels) to minimize cache
//!    key diversity: `routine` or `full` — only two cache prefixes to warm.
//!
//! ## Classification
//!
//! A turn is classified as **routine** when the last assistant message was a
//! tool call and the tool result indicates success on a non-complex operation:
//! - File read (tool_use with `ctx_read`, `read_file`, `Read`)
//! - Successful shell command (exit_code == 0, no error indicators)
//! - Search results (grep/glob/find)
//! - Status checks (git status, test passing)
//!
//! A turn is classified as **full** (keep maximum thinking) when:
//! - The user sent a new message (requires understanding intent)
//! - The tool result contains errors/failures
//! - Multiple tool results arrived (complex multi-step)
//! - The content is architecturally complex (refactoring, debugging)

use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::config::Effort;

/// Turn classification result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnClass {
    /// Routine tool-result acknowledgement — minimize thinking.
    Routine,
    /// Full complexity — keep maximum thinking effort.
    Full,
}

/// Statistics for monitoring effort routing effectiveness.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RoutingStats {
    pub routine_count: u64,
    pub full_count: u64,
}

static ROUTINE_COUNT: AtomicU64 = AtomicU64::new(0);
static FULL_COUNT: AtomicU64 = AtomicU64::new(0);

/// Classify the current turn based on the message array.
/// Returns `Routine` if the latest context is a simple tool-result
/// acknowledgement, `Full` otherwise.
pub fn classify_turn(messages: &Value) -> TurnClass {
    let Some(arr) = messages.as_array() else {
        return TurnClass::Full;
    };

    if arr.is_empty() {
        return TurnClass::Full;
    }

    let last = &arr[arr.len() - 1];
    let role = last.get("role").and_then(Value::as_str).unwrap_or("");

    if role == "tool" {
        classify_tool_result(last, arr)
    } else {
        TurnClass::Full
    }
}

/// Classify based on OpenAI Responses API `input` array (different structure).
pub fn classify_turn_responses(input: &Value) -> TurnClass {
    let Some(arr) = input.as_array() else {
        return TurnClass::Full;
    };
    if arr.is_empty() {
        return TurnClass::Full;
    }

    // In Responses API, look for the last item's type.
    let last = &arr[arr.len() - 1];
    let item_type = last.get("type").and_then(Value::as_str).unwrap_or("");

    if item_type == "function_call_output" {
        let output = last.get("output").and_then(Value::as_str).unwrap_or("");
        if is_routine_tool_output(output) {
            return TurnClass::Routine;
        }
    }

    TurnClass::Full
}

/// Classify based on Anthropic messages structure.
pub fn classify_turn_anthropic(messages: &Value) -> TurnClass {
    let Some(arr) = messages.as_array() else {
        return TurnClass::Full;
    };
    if arr.is_empty() {
        return TurnClass::Full;
    }

    let last = &arr[arr.len() - 1];
    let role = last.get("role").and_then(Value::as_str).unwrap_or("");

    if role != "user" {
        return TurnClass::Full;
    }

    // Anthropic puts tool_results in user messages with content array.
    let content = last.get("content");
    if let Some(Value::Array(blocks)) = content {
        let all_tool_results = !blocks.is_empty()
            && blocks
                .iter()
                .all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));

        if all_tool_results {
            // Check if any tool result has errors.
            let has_errors = blocks.iter().any(|b| {
                b.get("is_error") == Some(&Value::Bool(true))
                    || b.get("content")
                        .and_then(|c| c.as_str().or_else(|| extract_text_from_content(c)))
                        .is_some_and(contains_error_indicators)
            });

            if has_errors {
                return TurnClass::Full;
            }

            // Multiple tool results → likely complex multi-step.
            if blocks.len() > 3 {
                return TurnClass::Full;
            }

            // Check individual results.
            let all_routine = blocks.iter().all(|b| {
                let text = b
                    .get("content")
                    .and_then(|c| c.as_str().or_else(|| extract_text_from_content(c)))
                    .unwrap_or("");
                is_routine_tool_output(text)
            });

            if all_routine {
                return TurnClass::Routine;
            }
        }
    }

    TurnClass::Full
}

/// Map a turn classification to the effort level to apply.
/// `base` is the operator's configured static effort level.
pub fn effort_for_turn(class: TurnClass, base: Effort) -> Effort {
    match class {
        TurnClass::Routine => {
            ROUTINE_COUNT.fetch_add(1, Ordering::Relaxed);
            // Routine turns get minimal thinking regardless of base.
            Effort::Minimal
        }
        TurnClass::Full => {
            FULL_COUNT.fetch_add(1, Ordering::Relaxed);
            base
        }
    }
}

/// Snapshot routing statistics.
pub fn stats() -> RoutingStats {
    RoutingStats {
        routine_count: ROUTINE_COUNT.load(Ordering::Relaxed),
        full_count: FULL_COUNT.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Internal classification helpers
// ---------------------------------------------------------------------------

fn classify_tool_result(msg: &Value, _all_messages: &[Value]) -> TurnClass {
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");

    if contains_error_indicators(content) {
        return TurnClass::Full;
    }

    if is_routine_tool_output(content) {
        return TurnClass::Routine;
    }

    TurnClass::Full
}

/// Heuristic: does this tool output look like a routine, successful result?
fn is_routine_tool_output(content: &str) -> bool {
    if content.is_empty() || content.len() < 10 {
        return false;
    }

    // Error indicators → not routine.
    if contains_error_indicators(content) {
        return false;
    }

    // Very large outputs (>8000 chars) likely need careful processing.
    if content.len() > 8000 {
        return false;
    }

    // Positive signals for routine:
    let routine_signals = [
        // File read results (lean-ctx or native).
        "deps ",      // lean-ctx read header
        "[unchanged", // cached re-read
        "[lean-ctx]", // lean-ctx footer
        "lines:",     // line count indicators
        // Shell success patterns.
        "exit_code: 0",
        "Command completed",
        "0 errors",
        "All tests passed",
        "no changes",
        "nothing to commit",
        "Already up to date",
        "Build succeeded",
        // Search results.
        "matches in",
        "0 matches",
    ];

    routine_signals.iter().any(|sig| content.contains(sig))
}

/// Check if content contains error/failure indicators that need full thinking.
fn contains_error_indicators(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    let indicators = [
        "error",
        "failed",
        "failure",
        "fatal",
        "panic",
        "exception",
        "traceback",
        "stack trace",
        "segfault",
        "abort",
        "denied",
        "permission",
        "not found",
        "timed out",
        "exit_code: 1",
        "exit code 1",
        "compilation error",
        "syntax error",
        "type error",
    ];

    indicators.iter().any(|ind| lower.contains(ind))
}

/// Extract text from Anthropic content blocks.
fn extract_text_from_content(content: &Value) -> Option<&str> {
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                return block.get("text").and_then(Value::as_str);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_is_always_full() {
        let messages = json!([
            {"role": "user", "content": "What does this function do?"}
        ]);
        assert_eq!(classify_turn(&messages), TurnClass::Full);
    }

    #[test]
    fn successful_file_read_is_routine() {
        let messages = json!([
            {"role": "assistant", "content": "Let me read that file."},
            {"role": "tool", "content": "main.rs 50L\n  deps serde\n[lean-ctx] full source: ..."}
        ]);
        assert_eq!(classify_turn(&messages), TurnClass::Routine);
    }

    #[test]
    fn error_tool_result_is_full() {
        let messages = json!([
            {"role": "tool", "content": "error[E0308]: mismatched types\n  --> src/main.rs:5:12"}
        ]);
        assert_eq!(classify_turn(&messages), TurnClass::Full);
    }

    #[test]
    fn successful_shell_is_routine() {
        let messages = json!([
            {"role": "tool", "content": "Command completed in 150ms\nexit_code: 0\nAll tests passed"}
        ]);
        assert_eq!(classify_turn(&messages), TurnClass::Routine);
    }

    #[test]
    fn anthropic_tool_result_routine() {
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "abc", "content": "[unchanged 5L]\n[lean-ctx] cached"}
            ]}
        ]);
        assert_eq!(classify_turn_anthropic(&messages), TurnClass::Routine);
    }

    #[test]
    fn anthropic_tool_result_with_error() {
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "abc", "is_error": true, "content": "Tool failed"}
            ]}
        ]);
        assert_eq!(classify_turn_anthropic(&messages), TurnClass::Full);
    }

    #[test]
    fn effort_mapping() {
        assert_eq!(
            effort_for_turn(TurnClass::Routine, Effort::High),
            Effort::Minimal
        );
        assert_eq!(effort_for_turn(TurnClass::Full, Effort::High), Effort::High);
        assert_eq!(
            effort_for_turn(TurnClass::Full, Effort::Medium),
            Effort::Medium
        );
    }

    #[test]
    fn empty_messages_is_full() {
        assert_eq!(classify_turn(&json!([])), TurnClass::Full);
        assert_eq!(classify_turn(&json!(null)), TurnClass::Full);
    }

    #[test]
    fn deterministic_classification() {
        let messages = json!([
            {"role": "tool", "content": "Build succeeded\nexit_code: 0\nCommand completed in 2s"}
        ]);
        let c1 = classify_turn(&messages);
        let c2 = classify_turn(&messages);
        assert_eq!(c1, c2, "classification must be deterministic");
    }
}
