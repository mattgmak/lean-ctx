//! Evidence chain for context delivery lifecycles.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::accounting_fix::PostDeliveryAccounting;
use super::token_envelope::TokenEnvelope;
use super::types::ReceiptOutcome;

/// A single entry in the receipt chain — one context delivery lifecycle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChainEntry {
    /// Unique chain ID (monotonic counter).
    pub chain_id: u64,
    /// Tool or proxy path that initiated the request.
    pub source: String,
    /// Token envelope for the request.
    pub envelope: TokenEnvelope,
    /// Honest accounting for this delivery.
    pub accounting: PostDeliveryAccounting,
    /// Final outcome.
    pub outcome: ReceiptOutcome,
    /// Whether context was supplemented by kernel.
    pub kernel_supplemented: bool,
    /// Kernel budget used (tokens).
    pub kernel_budget_used: usize,
    /// Epoch seconds when recorded.
    pub recorded_at: u64,
}

#[derive(Default)]
struct ReceiptChain {
    entries: Vec<ChainEntry>,
    next_id: u64,
}

static CHAIN: OnceLock<Mutex<ReceiptChain>> = OnceLock::new();

/// Aggregate statistics for the receipt chain.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ChainSummary {
    /// Number of recorded delivery lifecycles.
    pub total_entries: usize,
    /// Number of accepted deliveries.
    pub accepted: usize,
    /// Number of rejected deliveries.
    pub rejected: usize,
    /// Number of kernel-supplemented deliveries.
    pub kernel_supplemented: usize,
    /// Average kernel budget among supplemented deliveries.
    pub avg_kernel_budget: f64,
    /// Total tokens delivered across the chain.
    pub total_tokens_delivered: usize,
    /// Sum of phantom-savings percentages across the chain.
    pub total_phantom_savings_pct: f64,
}

fn chain() -> MutexGuard<'static, ReceiptChain> {
    let mutex = CHAIN.get_or_init(|| Mutex::new(ReceiptChain::default()));
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Records a context delivery lifecycle and returns its monotonic chain ID.
pub fn record_chain_entry(
    source: &str,
    envelope: TokenEnvelope,
    accounting: PostDeliveryAccounting,
    outcome: ReceiptOutcome,
    kernel_supplemented: bool,
    kernel_budget_used: usize,
) -> u64 {
    let mut chain = chain();
    let chain_id = chain.next_id;
    chain.next_id = chain.next_id.saturating_add(1);
    let recorded_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    chain.entries.push(ChainEntry {
        chain_id,
        source: source.to_owned(),
        envelope,
        accounting,
        outcome,
        kernel_supplemented,
        kernel_budget_used,
        recorded_at,
    });
    chain_id
}

/// Returns the number of entries in the chain.
pub fn chain_length() -> usize {
    chain().entries.len()
}

/// Returns an owned snapshot of the full chain.
pub fn chain_entries() -> Vec<ChainEntry> {
    chain().entries.clone()
}

/// Returns aggregate statistics for all recorded entries.
pub fn chain_summary() -> ChainSummary {
    let chain = chain();
    let mut summary = ChainSummary {
        total_entries: chain.entries.len(),
        ..ChainSummary::default()
    };
    for entry in &chain.entries {
        match entry.outcome {
            ReceiptOutcome::Accepted => summary.accepted += 1,
            ReceiptOutcome::Rejected => summary.rejected += 1,
            ReceiptOutcome::Partial | ReceiptOutcome::Unknown => {}
        }
        if entry.kernel_supplemented {
            summary.kernel_supplemented += 1;
            summary.avg_kernel_budget += entry.kernel_budget_used as f64;
        }
        summary.total_tokens_delivered = summary
            .total_tokens_delivered
            .saturating_add(entry.accounting.delivered_tokens);
        summary.total_phantom_savings_pct += entry.accounting.phantom_savings_pct;
    }
    if summary.kernel_supplemented > 0 {
        summary.avg_kernel_budget /= summary.kernel_supplemented as f64;
    }
    summary
}

/// Returns an owned snapshot containing only accepted entries.
pub fn accepted_entries() -> Vec<ChainEntry> {
    outcome_entries(ReceiptOutcome::Accepted)
}

/// Returns an owned snapshot containing only rejected entries.
pub fn rejected_entries() -> Vec<ChainEntry> {
    outcome_entries(ReceiptOutcome::Rejected)
}

fn outcome_entries(outcome: ReceiptOutcome) -> Vec<ChainEntry> {
    chain()
        .entries
        .iter()
        .filter(|entry| entry.outcome == outcome)
        .cloned()
        .collect()
}

/// Returns the fraction of entries supplemented by the kernel.
pub fn kernel_hit_rate() -> f64 {
    let summary = chain_summary();
    if summary.total_entries == 0 {
        0.0
    } else {
        summary.kernel_supplemented as f64 / summary.total_entries as f64
    }
}

/// Clears all entries and resets the monotonic counter for testing.
pub fn reset_chain() {
    *chain() = ReceiptChain::default();
}

#[cfg(test)]
pub mod tests {
    use std::sync::MutexGuard;

    use super::*;
    use crate::core::context_kernel::accounting_fix::compute_honest_accounting;

    // The chain is process-wide and the kernel e2e suites record into it too:
    // a module-private lock let them reset and refill it mid-test once enough
    // tests ran in parallel. Every kernel test serializes on the shared lock.
    fn isolated() -> MutexGuard<'static, ()> {
        let guard = crate::core::context_kernel::kernel_config::KERNEL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_chain();
        guard
    }

    fn record(outcome: ReceiptOutcome, supplemented: bool) -> u64 {
        record_chain_entry(
            "test",
            TokenEnvelope::default(),
            compute_honest_accounting(100, 50, 10, 5),
            outcome,
            supplemented,
            if supplemented { 10 } else { 0 },
        )
    }

    fn record_many(outcome: ReceiptOutcome, count: usize) {
        for _ in 0..count {
            record(outcome, false);
        }
    }

    macro_rules! isolated_test {
        ($name:ident, $body:block) => {
            #[test]
            fn $name() {
                let _guard = isolated();
                $body
            }
        };
    }

    isolated_test!(record_and_retrieve, {
        record_many(ReceiptOutcome::Unknown, 3);
        assert_eq!(chain_length(), 3);
    });

    isolated_test!(chain_ids_monotonic, {
        assert_eq!(record(ReceiptOutcome::Unknown, false), 0);
        assert_eq!(record(ReceiptOutcome::Unknown, false), 1);
        assert_eq!(record(ReceiptOutcome::Unknown, false), 2);
    });

    isolated_test!(summary_counts, {
        record_many(ReceiptOutcome::Accepted, 5);
        record_many(ReceiptOutcome::Rejected, 2);
        let summary = chain_summary();
        assert_eq!(summary.accepted, 5);
        assert_eq!(summary.rejected, 2);
    });

    isolated_test!(kernel_hit_rate_tracks_supplements, {
        for supplemented in [true, true, true, false, false] {
            record(ReceiptOutcome::Unknown, supplemented);
        }
        assert!((kernel_hit_rate() - 0.6).abs() < f64::EPSILON);
    });

    isolated_test!(accepted_filter, {
        record(ReceiptOutcome::Accepted, false);
        record(ReceiptOutcome::Rejected, false);
        record(ReceiptOutcome::Accepted, false);
        assert_eq!(accepted_entries().len(), 2);
    });

    isolated_test!(reset_clears, {
        record(ReceiptOutcome::Accepted, true);
        reset_chain();
        assert_eq!(chain_length(), 0);
        assert_eq!(record(ReceiptOutcome::Unknown, false), 0);
    });
}
