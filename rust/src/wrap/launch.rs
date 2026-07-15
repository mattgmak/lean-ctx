//! Agent-specific launch/restart logic after wrap completes.

use std::process::Command;

/// Returns a user-facing hint about what to do next.
pub(super) fn handle_agent_launch(agent_key: &str) -> String {
    match agent_key {
        "cursor" => handle_cursor(),
        "claude" | "claude-code" => handle_claude(),
        "codex" => handle_codex(),
        "vscode" | "vscode-insiders" => handle_vscode(agent_key),
        _ => format!("Restart {agent_key} to activate the MCP server."),
    }
}

fn handle_cursor() -> String {
    let cursor_running = is_process_running("Cursor");

    if cursor_running {
        "Please restart Cursor to activate the MCP server.".to_string()
    } else if which_exists("cursor") {
        match Command::new("cursor").arg(".").spawn() {
            Ok(_) => String::new(),
            Err(_) => "Open Cursor to start using lean-ctx.".to_string(),
        }
    } else {
        "Open Cursor to start using lean-ctx.".to_string()
    }
}

fn handle_claude() -> String {
    if which_exists("claude") {
        "Start a new Claude Code session to use lean-ctx.".to_string()
    } else {
        "Install Claude Code, then run: lean-ctx wrap claude".to_string()
    }
}

fn handle_codex() -> String {
    if which_exists("codex") {
        "Start a new Codex session to use lean-ctx.".to_string()
    } else {
        "Install Codex CLI, then run: lean-ctx wrap codex".to_string()
    }
}

fn handle_vscode(variant: &str) -> String {
    let cmd = if variant == "vscode-insiders" {
        "code-insiders"
    } else {
        "code"
    };

    if !which_exists(cmd) {
        return "Open VS Code to start using lean-ctx.".to_string();
    }

    let running = is_process_running("Code") || is_process_running("Electron");
    if running {
        "Reload VS Code window (Cmd+Shift+P > Reload Window) to activate.".to_string()
    } else {
        "Open VS Code to start using lean-ctx.".to_string()
    }
}

fn which_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn is_process_running(name: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("pgrep")
            .args(["-xq", name])
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("pgrep")
            .args(["-x", name])
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {name}.exe")])
            .stdout(std::process::Stdio::piped())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(name))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = name;
        false
    }
}
