//! Admission control for heavy index builds (#685).
//!
//! The parallel BM25/graph builds fan the whole corpus across a rayon pool.
//! On very large corpora (the #685 report: 1M+ files across multiple roots)
//! the transient build state grows far past `max_ram_percent` before the
//! memory guardian's 3 s poll can react — the kernel OOM killer fired at
//! 75 GB RSS. Admission control closes that gap *before* the allocation
//! happens: estimate the build's peak memory from the corpus size and only
//! admit the parallel fast path when the estimate fits the remaining headroom
//! below the guardian's Hard threshold. Oversized corpora degrade to the
//! sequential build, which carries fine-grained per-file pressure breaks.
//!
//! This is a heuristic gate, not an allocator: factors are deliberately
//! conservative and the in-build batching (see `bm25_index::build`,
//! `graph_index::process_scan_targets`) remains the second line of defense.

use std::path::Path;

/// Peak-memory expansion factor over raw corpus bytes, per build kind.
///
/// BM25 holds chunk contents, lowercased token vectors and inverted postings
/// simultaneously during the merge; the graph scan retains full file contents
/// for edge-building plus symbol tables.
const BM25_EXPANSION_FACTOR: u64 = 5;
const GRAPH_EXPANSION_FACTOR: u64 = 2;

/// Files above this size are skipped by both builders, so they must not count
/// against the corpus estimate. Mirrors `MAX_FILE_SIZE_BYTES` in both scanners.
const BUILDER_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    Bm25,
    GraphScan,
}

impl BuildKind {
    fn expansion_factor(self) -> u64 {
        match self {
            Self::Bm25 => BM25_EXPANSION_FACTOR,
            Self::GraphScan => GRAPH_EXPANSION_FACTOR,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::GraphScan => "graph",
        }
    }
}

/// Admission decision for a heavy index build.
#[derive(Debug, Clone)]
pub struct Admission {
    /// `true`: the parallel fast path fits the memory budget.
    /// `false`: degrade to the sequential build (fine-grained pressure breaks).
    pub parallel_ok: bool,
    /// Human-readable denial reason for logs (`None` when admitted).
    pub reason: Option<String>,
}

impl Admission {
    fn admitted() -> Self {
        Self {
            parallel_ok: true,
            reason: None,
        }
    }
}

/// Decide whether a parallel build over `corpus_bytes` of source fits the
/// current memory headroom.
///
/// Budget anchor: the guardian escalates to Hard at 2× the configured RSS
/// limit (`max_ram_percent`), where background builds are aborted anyway —
/// so a build whose estimated peak would push RSS past that threshold (1.5× max_ram_percent) is
/// pointless to start in parallel. Everything below stays on the fast path;
/// normal repositories (a few hundred MB of source) are never affected.
#[must_use]
pub fn admit(kind: BuildKind, corpus_bytes: u64) -> Admission {
    let Some(limit) = super::memory_guard::rss_limit_bytes() else {
        // No platform memory introspection — nothing to enforce.
        return Admission::admitted();
    };
    let rss = super::memory_guard::get_rss_bytes().unwrap_or(0);
    admit_with(kind, corpus_bytes, rss, limit)
}

/// Pure decision core, separated for tests.
fn admit_with(kind: BuildKind, corpus_bytes: u64, rss_bytes: u64, limit_bytes: u64) -> Admission {
    let estimated = corpus_bytes.saturating_mul(kind.expansion_factor());
    let hard_threshold = limit_bytes.saturating_mul(3) / 2;
    let available = hard_threshold.saturating_sub(rss_bytes);

    if estimated <= available {
        return Admission::admitted();
    }

    Admission {
        parallel_ok: false,
        reason: Some(format!(
            "{} corpus {:.0} MB × {} ≈ {:.0} MB estimated peak exceeds the {:.0} MB headroom \
             (RSS {:.0} MB, hard limit {:.0} MB = 1.5× max_ram_percent) — degrading to the \
             sequential build with memory-pressure breaks",
            kind.label(),
            corpus_bytes as f64 / 1_048_576.0,
            kind.expansion_factor(),
            estimated as f64 / 1_048_576.0,
            available as f64 / 1_048_576.0,
            rss_bytes as f64 / 1_048_576.0,
            hard_threshold as f64 / 1_048_576.0,
        )),
    }
}

/// Sum the on-disk sizes of `files` (relative to `root`), skipping entries the
/// builders would skip (missing or above the 2 MB per-file cap). Bails out
/// early once the running total exceeds `cap` — the caller only needs to know
/// "fits / does not fit", so a 1M-file corpus never pays a full stat walk when
/// the first thousands of files already blow the budget.
#[must_use]
pub fn corpus_bytes_capped(root: &Path, files: &[String], cap: u64) -> u64 {
    let mut total: u64 = 0;
    for rel in files {
        if let Ok(meta) = std::fs::metadata(root.join(rel)) {
            let len = meta.len();
            if len > BUILDER_MAX_FILE_BYTES {
                continue;
            }
            total = total.saturating_add(len);
            if total > cap {
                return total;
            }
        }
    }
    total
}

/// Convenience: full admission check for a file list — stat-walk with early
/// bail, then the headroom decision. Logs the denial reason once.
#[must_use]
pub fn admit_files(kind: BuildKind, root: &Path, files: &[String]) -> Admission {
    let Some(limit) = super::memory_guard::rss_limit_bytes() else {
        return Admission::admitted();
    };
    let rss = super::memory_guard::get_rss_bytes().unwrap_or(0);
    let available = (limit.saturating_mul(3) / 2).saturating_sub(rss);
    // The stat walk can stop as soon as the corpus alone proves the estimate
    // exceeds the headroom (factor ≥ 1 ⇒ corpus > available/factor suffices).
    let bail_cap = available / kind.expansion_factor().max(1);
    let corpus = corpus_bytes_capped(root, files, bail_cap);
    let admission = admit_with(kind, corpus, rss, limit);
    if let Some(ref reason) = admission.reason {
        tracing::warn!("[index_admission] {reason}");
    }
    admission
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_048_576;

    #[test]
    fn small_corpus_is_admitted() {
        // 50 MB source × 5 = 250 MB estimate, 4 GB headroom → parallel.
        let a = admit_with(BuildKind::Bm25, 50 * MB, 500 * MB, 2_048 * MB);
        assert!(a.parallel_ok);
        assert!(a.reason.is_none());
    }

    #[test]
    fn oversized_corpus_degrades_to_sequential() {
        // 8 GB source × 5 = 40 GB estimate vs (2×4.8 GB − 1 GB) headroom → deny.
        let a = admit_with(BuildKind::Bm25, 8 * 1024 * MB, 1024 * MB, 4_810 * MB);
        assert!(!a.parallel_ok);
        let reason = a.reason.expect("denial carries a reason");
        assert!(reason.contains("sequential build"), "reason: {reason}");
    }

    #[test]
    fn graph_factor_is_smaller_than_bm25() {
        // Same corpus/headroom: BM25 (×5) denied, graph (×2) admitted.
        let corpus = 3 * 1024 * MB;
        let rss = 1024 * MB;
        let limit = 4_810 * MB;
        assert!(!admit_with(BuildKind::Bm25, corpus, rss, limit).parallel_ok);
        assert!(admit_with(BuildKind::GraphScan, corpus, rss, limit).parallel_ok);
    }

    #[test]
    fn high_rss_shrinks_headroom() {
        // Identical corpus: fits with low RSS, denied when RSS already near hard cap.
        let corpus = 500 * MB;
        let limit = 2_048 * MB;
        assert!(admit_with(BuildKind::Bm25, corpus, 100 * MB, limit).parallel_ok);
        assert!(!admit_with(BuildKind::Bm25, corpus, 3_900 * MB, limit).parallel_ok);
    }

    #[test]
    fn corpus_walk_skips_oversized_and_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("small.rs"), vec![b'x'; 1000]).unwrap();
        std::fs::write(
            tmp.path().join("big.bin"),
            vec![b'x'; (BUILDER_MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        let files = vec![
            "small.rs".to_string(),
            "big.bin".to_string(),
            "missing.rs".to_string(),
        ];
        assert_eq!(corpus_bytes_capped(tmp.path(), &files, u64::MAX), 1000);
    }

    #[test]
    fn corpus_walk_bails_early_at_cap() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(tmp.path().join(format!("f{i}.rs")), vec![b'x'; 1000]).unwrap();
        }
        let files: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
        // Cap of 2500 → stops after the third file (3000 > 2500), not 10 000.
        let total = corpus_bytes_capped(tmp.path(), &files, 2_500);
        assert!(total > 2_500 && total < 10_000, "total: {total}");
    }
}
