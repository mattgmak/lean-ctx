//! `lean-ctx unwrap <agent>` — restore pre-wrap state from snapshot.

use super::snapshot::WrapSnapshot;
use std::path::Path;

pub fn run_unwrap(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: lean-ctx unwrap <agent>");
        println!();
        println!("Restore the agent's configuration to its pre-wrap state.");
        println!("Removes lean-ctx MCP registration, hooks, and shell integration");
        println!("that were installed by `lean-ctx wrap <agent>`.");
        return;
    }

    let agent_key = match args.first() {
        Some(a) if !a.starts_with('-') => a.as_str(),
        _ => {
            eprintln!("Usage: lean-ctx unwrap <agent>");
            eprintln!("Example: lean-ctx unwrap cursor");
            std::process::exit(1);
        }
    };

    let snap = match WrapSnapshot::load(agent_key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "Restoring {agent_key} to pre-wrap state (snapshot from {})...",
        snap.timestamp
    );

    let mut restored = 0u32;
    let mut errors = 0u32;

    for (path_str, record) in &snap.files {
        let path = Path::new(path_str);

        if !record.existed {
            if path.exists() {
                match std::fs::remove_file(path) {
                    Ok(()) => {
                        eprintln!("  Removed: {path_str}");
                        restored += 1;
                    }
                    Err(e) => {
                        eprintln!("  Error removing {path_str}: {e}");
                        errors += 1;
                    }
                }
            }
            continue;
        }

        if record.backup_path.is_empty() {
            eprintln!("  Skipped: {path_str} (no backup available)");
            continue;
        }

        let backup = Path::new(&record.backup_path);
        if !backup.exists() {
            eprintln!("  Skipped: {path_str} (backup file missing)");
            continue;
        }

        match std::fs::copy(backup, path) {
            Ok(_) => {
                eprintln!("  Restored: {path_str}");
                restored += 1;
            }
            Err(e) => {
                eprintln!("  Error restoring {path_str}: {e}");
                errors += 1;
            }
        }
    }

    // Remove MCP registration for the agent
    eprintln!("  Removing MCP registration...");
    if let Some(home) = dirs::home_dir() {
        let targets = crate::core::editor_registry::build_targets(&home);
        for target in targets.iter().filter(|t| t.agent_key == agent_key) {
            let _ = crate::core::editor_registry::remove_lean_ctx_mcp_server(
                &target.config_path,
                crate::core::editor_registry::WriteOptions {
                    overwrite_invalid: true,
                },
            );
        }
    }

    // Clean up snapshot dir
    if let Ok(state) = crate::core::paths::state_dir() {
        let snap_dir = state.join("snapshots").join(agent_key);
        let _ = std::fs::remove_dir_all(&snap_dir);
    }

    eprintln!();
    if errors == 0 {
        eprintln!(
            "\x1b[1;32mlean-ctx unwrapped {agent_key} successfully.\x1b[0m ({restored} files restored)"
        );
    } else {
        eprintln!(
            "\x1b[1;33mlean-ctx unwrap completed with {errors} error(s).\x1b[0m ({restored} files restored)"
        );
    }
    eprintln!();
    eprintln!("  Restart {agent_key} to complete the removal.");
}
