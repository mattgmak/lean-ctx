//! Overflow tests extracted from tests.rs to satisfy LOC gate (#660).
use super::*;

#[test]
fn rg_type_falls_through() {
    assert_eq!(
        rewrite_candidate("rg -t rust pattern src/", "lean-ctx"),
        Some(expect_wrapped("rg -t rust pattern src/", "lean-ctx"))
    );
}
#[test]
fn rg_glob_falls_through() {
    assert_eq!(
        rewrite_candidate("rg --glob=*.rs pattern src/", "lean-ctx"),
        Some(expect_wrapped("rg --glob=*.rs pattern src/", "lean-ctx"))
    );
}
#[test]
fn rg_context_falls_through() {
    assert_eq!(
        rewrite_candidate("rg -A5 pattern file.rs", "lean-ctx"),
        Some(expect_wrapped("rg -A5 pattern file.rs", "lean-ctx"))
    );
}
#[test]
fn rg_json_falls_through() {
    assert_eq!(
        rewrite_candidate("rg --json pattern src/", "lean-ctx"),
        Some(expect_wrapped("rg --json pattern src/", "lean-ctx"))
    );
}
// --- is_shell_tool covers Gemini/Antigravity tool names ---
#[test]
fn is_shell_tool_covers_all_ide_variants() {
    for name in [
        "run_command",
        "run_shell_command",
        "execute_command",
        "exec_command",
        "command_exec",
        "run_terminal",
        "runterminal",
        "run",
        "exec",
        "execute",
        "command",
        "cmd",
        "sh",
    ] {
        assert!(
            is_shell_tool(name),
            "{name} must be recognized as shell tool"
        );
    }
}
// --- Andi's real-world pattern ---
#[test]
fn andis_real_world_grep_rewrites_safely() {
    let cmd = r#"grep -rn "func\|Interval\|Duration" src/"#;
    let result = rewrite_candidate(cmd, "lean-ctx");
    assert!(result.is_some(), "grep -rn must be rewritten");
    let rewritten = result.unwrap();
    assert!(
        rewritten.starts_with("lean-ctx grep"),
        "must route through lean-ctx grep: {rewritten}"
    );
    assert!(
        rewritten.contains("func") && rewritten.contains("Interval"),
        "pattern must be preserved: {rewritten}"
    );
    // Pattern contains backslash → must be quoted
    assert!(
        rewritten.contains('"'),
        "pattern with backslash must be quoted for shell safety: {rewritten}"
    );
}
