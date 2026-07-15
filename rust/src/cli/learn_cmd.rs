use crate::core::gotcha_tracker::{self, GotchaStore, learn};

pub(crate) fn cmd_learn(args: &[String]) {
    // Offline mining mode: `lean-ctx learn --mine <dir>` distills recurring
    // error signatures from a directory of .jsonl transcripts/logs.
    if let Some(pos) = args.iter().position(|a| a == "--mine") {
        let dir = args.get(pos + 1).map(String::as_str);
        cmd_learn_mine(dir);
        return;
    }

    let project_root = super::common::detect_project_root(args);
    let apply = args.iter().any(|a| a == "--apply");

    let mut store = GotchaStore::load(&project_root);
    let universal = gotcha_tracker::load_universal_gotchas();
    for ug in universal {
        store.add_universal(ug);
    }

    let learnings = learn::extract_learnings(&store);

    if learnings.is_empty() {
        println!(
            "No learnings yet. lean-ctx needs to detect and resolve errors across sessions first."
        );
        println!("Tip: Use lean-ctx normally — errors are automatically tracked and correlated.");
        return;
    }

    println!("=== Learned Gotchas ({} total) ===\n", learnings.len());
    for l in &learnings {
        println!("  {l}");
    }

    if apply {
        println!();
        match learn::apply_learnings(&project_root, &learnings) {
            Ok(files) if files.is_empty() => {
                println!("No learnings written (need >=2 occurrences with >=50% confidence).");
            }
            Ok(files) => println!(
                "Wrote {} learnings to {}",
                learnings.len(),
                files.join(" + ")
            ),
            Err(e) => eprintln!("Error: {e}"),
        }
    } else {
        println!(
            "\nUse `lean-ctx learn --apply` to write these to AGENTS.md (and CLAUDE.local.md if present)."
        );
    }
}

/// `lean-ctx learn --mine [dir]`: distill recurring error signatures from a
/// directory of `.jsonl` transcripts/logs. With no `dir`, it auto-discovers the
/// agent-transcripts directory (Claude Code / Cursor), so scanning real subagent
/// transcripts is zero-config. Read-only — it surfaces the project's recurring
/// pain points for review, it never mutates stored state.
fn cmd_learn_mine(dir: Option<&str>) {
    let path = if let Some(d) = dir {
        std::path::PathBuf::from(d)
    } else if let Some(p) = gotcha_tracker::mining::default_transcript_dir() {
        println!("Scanning auto-discovered transcripts: {}\n", p.display());
        p
    } else {
        eprintln!(
            "Usage: lean-ctx learn --mine [dir]  (no agent-transcripts dir found to auto-scan)"
        );
        return;
    };
    if !path.is_dir() {
        eprintln!("Error: '{}' is not a directory", path.display());
        return;
    }
    let mined = gotcha_tracker::mining::mine_jsonl_dir(&path);
    println!(
        "{}",
        gotcha_tracker::mining::format_mining_report(&mined, 2)
    );
}
