//! Formatter routing for `ctx_refactor action=reformat`: pick a formatter by
//! file extension, using built-in routing per extension.

/// The formatter selected for a file: either the IDE HTTP backend or an external
/// shell command (template with a `{file}` placeholder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Formatter {
    Jetbrains,
    Command(String),
}

/// Pick the formatter for `abs_path` using built-in defaults per extension.
/// Extension match is case-insensitive; no extension or an unknown extension → `Jetbrains`.
pub fn resolve_formatter(abs_path: &str) -> Formatter {
    let ext = std::path::Path::new(abs_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    builtin_default(&ext)
}

/// Built-in routing when the config has no entry for this extension.
fn builtin_default(ext: &str) -> Formatter {
    match ext {
        "rs" => Formatter::Command("rustfmt {file}".to_string()),
        _ => Formatter::Jetbrains,
    }
}

/// The binary name of a command template, for the `via <name>` output label.
pub fn command_label(template: &str) -> &str {
    template.split_whitespace().next().unwrap_or("formatter")
}

/// Split a command template into argv, substituting the `{file}` placeholder with
/// `abs_path`. `{file}` may be a standalone token or embedded in a token. If no
/// placeholder is present, `abs_path` is appended as the final argument. The path
/// is always a single argv element (spaces in the path are preserved).
pub fn build_argv(template: &str, abs_path: &str) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    let mut saw_placeholder = false;
    for tok in template.split_whitespace() {
        if tok == "{file}" {
            argv.push(abs_path.to_string());
            saw_placeholder = true;
        } else if tok.contains("{file}") {
            argv.push(tok.replace("{file}", abs_path));
            saw_placeholder = true;
        } else {
            argv.push(tok.to_string());
        }
    }
    if !saw_placeholder {
        argv.push(abs_path.to_string());
    }
    argv
}

/// Run an external formatter command on `abs_path` with cwd `project_root` (so
/// tool config like `rustfmt.toml` is discovered). Returns `Err` with a clear
/// message if the binary is missing or the command exits non-zero.
pub fn run_command_formatter(
    template: &str,
    abs_path: &str,
    project_root: &str,
) -> Result<(), String> {
    let argv = build_argv(template, abs_path);
    let (bin, rest) = argv
        .split_first()
        .ok_or_else(|| "INVALID_TARGET: empty formatter template".to_string())?;
    let output = std::process::Command::new(bin)
        .args(rest)
        .current_dir(project_root)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("formatter '{bin}' not found in PATH")
            } else {
                format!("failed to run '{bin}': {e}")
            }
        })?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{bin} exited {code}: {}", stderr.trim()));
    }
    Ok(())
}

/// Hex BLAKE3 of the file content, for honest before/after change detection.
pub fn blake3_of(abs_path: &str) -> Result<String, String> {
    let bytes = std::fs::read(abs_path).map_err(|e| format!("FILE_NOT_FOUND: {abs_path}: {e}"))?;
    Ok(crate::core::hasher::hash_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rs_defaults_to_rustfmt() {
        let f = resolve_formatter("/x/a.rs");
        assert!(matches!(f, Formatter::Command(ref t) if t == "rustfmt {file}"));
    }

    #[test]
    fn md_and_unknown_and_no_ext_default_to_jetbrains() {
        assert!(matches!(resolve_formatter("/x/a.md"), Formatter::Jetbrains));
        assert!(matches!(
            resolve_formatter("/x/a.txt"),
            Formatter::Jetbrains
        ));
        assert!(matches!(
            resolve_formatter("/x/README"),
            Formatter::Jetbrains
        ));
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert!(matches!(
            resolve_formatter("/x/A.RS"),
            Formatter::Command(_)
        ));
    }

    #[test]
    fn command_label_is_first_token() {
        assert_eq!(command_label("rustfmt {file}"), "rustfmt");
        assert_eq!(command_label("ruff format {file}"), "ruff");
        assert_eq!(command_label(""), "formatter");
    }

    #[test]
    fn argv_substitutes_placeholder() {
        assert_eq!(
            build_argv("rustfmt {file}", "/x/a.rs"),
            vec!["rustfmt".to_string(), "/x/a.rs".to_string()]
        );
        assert_eq!(
            build_argv("ruff format {file}", "/x/a.py"),
            vec![
                "ruff".to_string(),
                "format".to_string(),
                "/x/a.py".to_string()
            ]
        );
    }

    #[test]
    fn argv_appends_path_when_no_placeholder() {
        assert_eq!(
            build_argv("gofmt -w", "/x/a.go"),
            vec!["gofmt".to_string(), "-w".to_string(), "/x/a.go".to_string()]
        );
    }

    #[test]
    fn argv_path_with_spaces_stays_one_arg() {
        let argv = build_argv("rustfmt {file}", "/x/my dir/a.rs");
        assert_eq!(
            argv,
            vec!["rustfmt".to_string(), "/x/my dir/a.rs".to_string()]
        );
    }

    #[test]
    fn blake3_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "one").unwrap();
        let p = f.to_str().unwrap();
        let h1 = blake3_of(p).unwrap();
        let h2 = blake3_of(p).unwrap();
        assert_eq!(h1, h2, "same content → same hash");
        std::fs::write(&f, "two").unwrap();
        assert_ne!(
            h1,
            blake3_of(p).unwrap(),
            "changed content → different hash"
        );
    }

    #[test]
    fn blake3_missing_file_errors() {
        assert!(blake3_of("/no/such/file.xyz").is_err());
    }

    #[test]
    fn run_command_missing_binary_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn x(){}\n").unwrap();
        let err = run_command_formatter(
            "definitely-not-a-formatter-binary {file}",
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn run_command_nonzero_exit_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn x(){}\n").unwrap();
        // `false` exits 1 and ignores its args.
        let err = run_command_formatter(
            "false {file}",
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("exited"), "got: {err}");
    }

    #[test]
    fn run_rustfmt_formats_and_reports_change() {
        // Gated: only runs when rustfmt is installed.
        if std::process::Command::new("rustfmt")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: rustfmt not in PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn   x( ){let y=1;}\n").unwrap(); // deliberate drift
        let p = f.to_str().unwrap();
        let before = blake3_of(p).unwrap();
        run_command_formatter("rustfmt {file}", p, dir.path().to_str().unwrap()).unwrap();
        let after = blake3_of(p).unwrap();
        assert_ne!(
            before, after,
            "rustfmt should have changed the drifted file"
        );

        // A second run is a no-op (already conformant).
        let before2 = blake3_of(p).unwrap();
        run_command_formatter("rustfmt {file}", p, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(
            before2,
            blake3_of(p).unwrap(),
            "second run should be unchanged"
        );
    }
}
