use std::io::Read;

use super::HOOK_STDIN_TIMEOUT;

const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg", "pdf", "zip", "tar", "gz", "bz2",
    "xz", "7z", "rar", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "wav", "avi", "mov",
    "mkv", "so", "dylib", "dll", "exe", "bin", "o", "a", "class", "pyc", "wasm",
];

/// Handle the `lean-ctx hook deny` subcommand.
///
/// Called by PreToolUse hooks in Replace mode. Denies native Read/Grep/Glob/Shell
/// calls unless an exception applies (binary files, MCP down, etc.).
///
/// Output format matches both Claude Code and Cursor hook protocols.
pub fn handle_deny() {
    let stdin_payload = read_stdin_with_timeout();
    let tool_name = extract_tool_name(&stdin_payload);
    let file_path = extract_file_path(&stdin_payload);

    // #805: deny Write/Edit payloads that contain compression markers.
    // These indicate the agent is writing compressed ctx_read output to disk.
    if is_write_tool(&tool_name) {
        if !is_compression_guard_disabled()
            && let Some(content) = extract_write_content(&stdin_payload)
            && has_compression_markers(&content)
        {
            print_deny_compression_markers(&tool_name);
        }
        print_allow();
        return;
    }

    if should_allow(&tool_name, file_path.as_deref()) {
        print_allow();
    } else {
        print_deny(&tool_name);
    }
}

fn should_allow(tool_name: &str, file_path: Option<&str>) -> bool {
    if super::is_disabled() {
        return true;
    }

    if !is_mcp_server_reachable() {
        return true;
    }

    if file_path.is_some_and(is_binary_file) {
        return true;
    }

    if is_replace_mode_disabled() {
        return true;
    }

    let _ = tool_name;
    false
}

fn is_mcp_server_reachable() -> bool {
    let path = crate::daemon::daemon_pid_path();
    if !path.exists() {
        return true;
    }
    if let Ok(pid_str) = std::fs::read_to_string(&path)
        && let Ok(pid) = pid_str.trim().parse::<u32>()
        && !crate::ipc::process::is_alive(pid)
    {
        return false;
    }
    true
}

fn is_replace_mode_disabled() -> bool {
    matches!(
        std::env::var("LEAN_CTX_REPLACE_MODE"),
        Ok(v) if v.trim() == "0" || v.trim().eq_ignore_ascii_case("off")
    )
}

fn is_binary_file(path: &str) -> bool {
    if let Some(ext) = path.rsplit('.').next() {
        return BINARY_EXTENSIONS.contains(&ext.to_lowercase().as_str());
    }
    false
}

fn extract_tool_name(payload: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Some(name) = json.get("tool_name").and_then(|v| v.as_str()) {
            return name.to_string();
        }
        if let Some(name) = json
            .get("hookSpecificInput")
            .and_then(|h| h.get("toolName"))
            .and_then(|v| v.as_str())
        {
            return name.to_string();
        }
    }
    "unknown".to_string()
}

fn extract_file_path(payload: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(payload).ok()?;

    let input = json
        .get("input")
        .or_else(|| json.get("hookSpecificInput").and_then(|h| h.get("input")));

    if let Some(input) = input {
        for key in ["file_path", "path", "filePath"] {
            if let Some(path) = input.get(key).and_then(|v| v.as_str()) {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn is_write_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Write"
            | "write"
            | "WriteFile"
            | "Edit"
            | "edit"
            | "MultiEdit"
            | "StrReplace"
            | "str_replace"
            | "EditNotebook"
    )
}

fn is_compression_guard_disabled() -> bool {
    std::env::var("LEAN_CTX_ALLOW_COMPRESSED_WRITE")
        .is_ok_and(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
}

fn has_compression_markers(content: &str) -> bool {
    content.contains("[lean-ctx:")
}

fn extract_write_content(payload: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(payload).ok()?;
    let input = json
        .get("input")
        .or_else(|| json.get("hookSpecificInput").and_then(|h| h.get("input")))?;

    // Check all common content field names across tool variants
    for key in [
        "content",
        "contents",
        "file_text",
        "text",
        "new_string",
        "new_text",
    ] {
        if let Some(text) = input.get(key).and_then(|v| v.as_str()) {
            return Some(text.to_string());
        }
    }
    // MultiEdit: check edits array for old_text/new_text
    if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
        let mut combined = String::new();
        for edit in edits {
            if let Some(t) = edit.get("new_text").and_then(|v| v.as_str()) {
                combined.push_str(t);
            }
            if let Some(t) = edit.get("newText").and_then(|v| v.as_str()) {
                combined.push_str(t);
            }
        }
        if !combined.is_empty() {
            return Some(combined);
        }
    }
    None
}

fn print_deny_compression_markers(tool_name: &str) {
    let msg = format!(
        "Blocked {tool_name}: payload contains lean-ctx compression markers \
         ([lean-ctx: omitted ...] or similar). Writing compressed ctx_read \
         output to disk corrupts files. Use ctx_read(raw=true) or ctx_expand \
         to recover full content before editing. \
         Set LEAN_CTX_ALLOW_COMPRESSED_WRITE=1 to override."
    );
    let output = serde_json::json!({
        "permission": "deny",
        "user_message": msg
    });
    println!("{output}");
    std::process::exit(2);
}

fn print_deny(tool_name: &str) {
    let msg = match tool_name {
        "Read" | "read" | "ReadFile" => {
            "Use ctx_read instead — lean-ctx replace mode is active. Native Read is disabled."
        }
        "Grep" | "grep" | "Search" => {
            "Use ctx_search instead — lean-ctx replace mode is active. Native Grep is disabled."
        }
        "Glob" | "glob" => {
            "Use ctx_glob instead — lean-ctx replace mode is active. Native Glob is disabled."
        }
        "Shell" | "Bash" | "bash" => {
            "Use ctx_shell instead — lean-ctx replace mode is active. Native Shell is disabled."
        }
        _ => "Use the equivalent ctx_* tool — lean-ctx replace mode is active.",
    };

    let output = serde_json::json!({
        "permission": "deny",
        "user_message": msg
    });
    println!("{output}");
    std::process::exit(2);
}

fn print_allow() {
    println!("{{}}");
}

fn read_stdin_with_timeout() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(HOOK_STDIN_TIMEOUT).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_write_tool_recognizes_all_variants() {
        assert!(is_write_tool("Write"));
        assert!(is_write_tool("write"));
        assert!(is_write_tool("Edit"));
        assert!(is_write_tool("StrReplace"));
        assert!(is_write_tool("MultiEdit"));
        assert!(is_write_tool("EditNotebook"));
        assert!(!is_write_tool("Read"));
        assert!(!is_write_tool("Grep"));
        assert!(!is_write_tool("Shell"));
    }

    #[test]
    fn has_compression_markers_detects_lean_ctx_patterns() {
        assert!(has_compression_markers(
            "some text [lean-ctx: omitted 42 lines] more"
        ));
        assert!(has_compression_markers("... [lean-ctx: archived] ..."));
        assert!(!has_compression_markers("[lean-ctx compressed] tail"));
        assert!(!has_compression_markers(
            "[lean-ctx docs](https://example.com)"
        ));
        assert!(!has_compression_markers(
            "normal file content without markers"
        ));
        assert!(!has_compression_markers("lean-ctx is great"));
        assert!(!has_compression_markers(""));
    }

    #[test]
    fn extract_write_content_from_cursor_write() {
        let payload = r#"{"hookSpecificInput":{"toolName":"Write","input":{"path":"test.md","contents":"hello [lean-ctx: omitted 5 lines]"}}}"#;
        let content = extract_write_content(payload).unwrap();
        assert!(content.contains("[lean-ctx:"));
    }

    #[test]
    fn extract_write_content_from_claude_code_edit() {
        let payload = r#"{"tool_name":"Edit","input":{"path":"test.rs","new_text":"fn foo() { [lean-ctx: omitted 10 lines] }"}}"#;
        let content = extract_write_content(payload).unwrap();
        assert!(content.contains("[lean-ctx:"));
    }

    #[test]
    fn extract_write_content_from_multi_edit() {
        let payload = r#"{"tool_name":"MultiEdit","input":{"path":"x.rs","edits":[{"new_text":"[lean-ctx: omitted 3 lines]"}]}}"#;
        let content = extract_write_content(payload).unwrap();
        assert!(content.contains("[lean-ctx:"));
    }

    #[test]
    fn extract_write_content_clean_payload_returns_none_for_markers() {
        let payload =
            r#"{"tool_name":"Write","input":{"path":"test.md","contents":"normal content"}}"#;
        let content = extract_write_content(payload).unwrap();
        assert!(!has_compression_markers(&content));
    }

    #[test]
    fn extract_write_content_no_content_returns_none() {
        let payload = r#"{"tool_name":"Write","input":{"path":"test.md"}}"#;
        assert!(extract_write_content(payload).is_none());
    }
}
