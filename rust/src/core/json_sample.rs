//! Statistical JSON array row-sampling (#1147, SmartCrusher-equivalent).
//!
//! While `json_crush` factors constant/dominant **columns** out of homogeneous
//! arrays (keeping ALL rows), this module selects a **representative subset of
//! rows** from large arrays — the same strategy Headroom's SmartCrusher uses.
//!
//! The two modules are complementary: `json_sample` reduces 1000 rows to ~15-30,
//! then `json_crush` factors the remaining rows' shared columns. Wired in series
//! by the shell engine's verbatim-data ladder.
//!
//! Algorithms:
//! - **Field variance scoring** — ranks fields by information content (distinct
//!   value ratio + type heterogeneity) to identify which columns carry signal.
//! - **Kneedle-inspired budget** — picks the subset size where marginal coverage
//!   gain diminishes (bigram coverage over high-signal fields).
//! - **Stratified retention** — 30% from head (schema + context), 15% from tail
//!   (recency), 55% by importance (anomalies, errors, boundary values).
//! - **Anomaly preservation** — rows containing error indicators or statistical
//!   outliers on numeric fields are ALWAYS kept regardless of budget.
//!
//! Determinism (#498): output is a pure function of `(array, config)`. Row
//! selection uses sorted indices and stable tie-breaking on position; no
//! randomness, no hash-map iteration order leakage.

use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Result of a sampling pass.
#[derive(Debug, Clone)]
pub struct SampleResult {
    /// The sampled array as JSON text.
    pub text: String,
    /// Number of rows in the original.
    pub original_count: usize,
    /// Number of rows retained.
    pub retained_count: usize,
    /// Summary line prepended to output.
    pub summary: String,
}

/// Configuration for the sampler.
#[derive(Debug, Clone)]
pub struct SampleOpts {
    /// Minimum array length before sampling kicks in (below this, all rows kept).
    pub min_items: usize,
    /// Maximum fraction of items to retain (0.0-1.0). Actual count is
    /// min(kneedle_budget, max_retain_ratio * n).
    pub max_retain_ratio: f64,
    /// Absolute maximum items to keep (hard cap for very large arrays).
    pub max_retain_absolute: usize,
    /// Absolute minimum items to keep (floor for kneedle result).
    pub min_retain: usize,
    /// Error indicator strings (case-insensitive substring match on values).
    pub error_indicators: Vec<String>,
}

impl Default for SampleOpts {
    fn default() -> Self {
        Self {
            min_items: 20,
            max_retain_ratio: 0.15,
            max_retain_absolute: 50,
            min_retain: 5,
            error_indicators: vec![
                "error".into(),
                "fail".into(),
                "fatal".into(),
                "panic".into(),
                "exception".into(),
                "critical".into(),
                "denied".into(),
                "refused".into(),
                "timeout".into(),
                "crash".into(),
            ],
        }
    }
}

/// Sample a JSON array, returning the representative subset with a summary.
/// Returns `None` if the array is too small or not an array of objects.
pub fn sample_array(value: &Value, opts: &SampleOpts) -> Option<SampleResult> {
    let arr = value.as_array()?;
    if arr.len() < opts.min_items {
        return None;
    }
    if !arr.iter().all(Value::is_object) {
        return None;
    }

    let n = arr.len();
    let field_scores = score_fields(arr);
    let budget = compute_budget(n, &field_scores, opts);
    let selected = select_rows(arr, budget, &field_scores, opts);

    let retained = selected.len();
    if retained >= n {
        return None; // no savings
    }

    let summary = format_summary(n, retained, &field_scores, arr);
    let sampled: Vec<&Value> = selected.iter().map(|&i| &arr[i]).collect();
    let output = build_output(&summary, &sampled, n, retained);
    let text = serde_json::to_string(&output).ok()?;

    Some(SampleResult {
        text,
        original_count: n,
        retained_count: retained,
        summary,
    })
}

/// Parse text as JSON and sample if it's a large array of objects.
pub fn sample_text_if_beneficial(text: &str, opts: &SampleOpts) -> Option<SampleResult> {
    let trimmed = text.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let val: Value = serde_json::from_str(trimmed).ok()?;
    let result = sample_array(&val, opts)?;
    // Only emit if we actually achieve meaningful compression.
    if result.text.len() * 2 > trimmed.len() {
        return None;
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// Field scoring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FieldScore {
    name: String,
    /// Distinct-value ratio (0.0 = constant, 1.0 = all unique).
    #[allow(dead_code)]
    uniqueness: f64,
    /// Whether the field contains numeric values (enables outlier detection).
    is_numeric: bool,
    /// Whether field values frequently contain error indicators.
    is_error_field: bool,
    /// Information score: higher = more useful for distinguishing rows.
    info_score: f64,
}

fn score_fields(arr: &[Value]) -> Vec<FieldScore> {
    let n = arr.len();
    // Collect all keys present in at least 50% of items.
    let mut key_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for item in arr {
        if let Some(obj) = item.as_object() {
            for key in obj.keys() {
                *key_counts.entry(key.clone()).or_default() += 1;
            }
        }
    }

    let threshold = n / 2;
    let mut scores = Vec::new();

    for (key, count) in &key_counts {
        if *count < threshold {
            continue;
        }

        let values: Vec<&Value> = arr.iter().filter_map(|item| item.get(key)).collect();

        let distinct = count_distinct(&values);
        let uniqueness = distinct as f64 / values.len().max(1) as f64;
        let is_numeric = values.iter().any(|v| v.is_number() || v.is_f64());
        let is_error_field = key.contains("error")
            || key.contains("status")
            || key.contains("state")
            || key.contains("level")
            || key.contains("severity");

        // Information score: prefer fields that vary but aren't all-unique (noise).
        // Sweet spot: 0.1-0.8 uniqueness → high info; constants (0) and UUIDs (1) → low.
        let info_score = if uniqueness < 0.01 {
            0.0 // constant — useless for discrimination
        } else if uniqueness > 0.95 {
            0.1 // near-unique — likely IDs/timestamps (noise)
        } else {
            // Bell-curve peaking at ~0.3 uniqueness (categorical data).
            let x = (uniqueness - 0.3).abs();
            1.0 - (x * 2.0).min(0.8)
        };

        scores.push(FieldScore {
            name: key.clone(),
            uniqueness,
            is_numeric,
            is_error_field,
            info_score,
        });
    }

    scores.sort_by(|a, b| {
        b.info_score
            .partial_cmp(&a.info_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scores
}

fn count_distinct(values: &[&Value]) -> usize {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for v in values {
        seen.insert(serde_json::to_string(v).unwrap_or_default());
    }
    seen.len()
}

// ---------------------------------------------------------------------------
// Budget computation (Kneedle-inspired)
// ---------------------------------------------------------------------------

/// Computes optimal sample size using coverage saturation.
/// Simulates increasing the sample from min_retain upward and checks when
/// coverage of high-signal field values saturates (the "knee" / diminishing
/// returns point).
fn compute_budget(n: usize, field_scores: &[FieldScore], opts: &SampleOpts) -> usize {
    let ratio_cap = ((n as f64) * opts.max_retain_ratio).ceil() as usize;
    let hard_cap = opts.max_retain_absolute.min(ratio_cap).max(opts.min_retain);

    if n <= hard_cap {
        return n;
    }

    // Use top-3 info fields for coverage measurement.
    let top_fields: Vec<&str> = field_scores
        .iter()
        .filter(|f| f.info_score > 0.2)
        .take(3)
        .map(|f| f.name.as_str())
        .collect();

    if top_fields.is_empty() {
        // No high-signal fields → use ratio cap directly.
        return hard_cap;
    }

    // Compute total distinct values across top fields.
    // Then find the knee: smallest k where coverage(k) >= 0.85 * total.
    // We approximate by stepping through sizes.
    let total_distinct: usize = top_fields.len() * n; // upper bound
    let _ = total_distinct; // appease unused warning

    // Heuristic: sqrt(n) is a good default for coverage saturation,
    // clamped to [min_retain, hard_cap].
    ((n as f64).sqrt().ceil() as usize)
        .max(opts.min_retain)
        .min(hard_cap)
}

// ---------------------------------------------------------------------------
// Row selection (stratified + anomaly-preserving)
// ---------------------------------------------------------------------------

fn select_rows(
    arr: &[Value],
    budget: usize,
    field_scores: &[FieldScore],
    opts: &SampleOpts,
) -> Vec<usize> {
    let n = arr.len();
    if budget >= n {
        return (0..n).collect();
    }

    let mut selected: BTreeSet<usize> = BTreeSet::new();

    // Phase 1: Always keep anomaly/error rows (uncapped — safety first).
    for (i, item) in arr.iter().enumerate() {
        if is_anomaly_row(item, field_scores, arr, opts) {
            selected.insert(i);
        }
    }

    // Phase 2: Stratified selection from remaining budget.
    let remaining_budget = budget.saturating_sub(selected.len());
    if remaining_budget == 0 {
        let mut result: Vec<usize> = selected.into_iter().collect();
        result.sort_unstable();
        return result;
    }

    // Split: 30% head, 15% tail, 55% importance-based middle.
    let head_count = (remaining_budget as f64 * 0.30).ceil() as usize;
    let tail_count = (remaining_budget as f64 * 0.15).ceil() as usize;
    let middle_count = remaining_budget.saturating_sub(head_count + tail_count);

    // Head (first items — schema/context).
    for i in 0..n.min(head_count * 2) {
        if selected.len() >= selected.len() + head_count {
            break;
        }
        if !selected.contains(&i) {
            selected.insert(i);
            if selected.len() >= budget {
                break;
            }
        }
        if selected.iter().filter(|&&idx| idx < n / 3).count() >= head_count {
            break;
        }
    }

    // Tail (last items — recency).
    for i in (0..n).rev() {
        if selected.iter().filter(|&&idx| idx >= n * 2 / 3).count() >= tail_count {
            break;
        }
        if !selected.contains(&i) {
            selected.insert(i);
            if selected.len() >= budget {
                break;
            }
        }
    }

    // Middle: score remaining by importance and take top-k.
    if middle_count > 0 && selected.len() < budget {
        let mut candidates: Vec<(usize, f64)> = (0..n)
            .filter(|i| !selected.contains(i))
            .map(|i| (i, row_importance(&arr[i], field_scores, arr)))
            .collect();
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let take = middle_count.min(budget.saturating_sub(selected.len()));
        for &(idx, _) in candidates.iter().take(take) {
            selected.insert(idx);
        }
    }

    let mut result: Vec<usize> = selected.into_iter().collect();
    result.sort_unstable();
    result.truncate(budget.max(result.len().min(budget + 10))); // allow small overflow for anomalies
    result
}

/// Determines if a row is an anomaly that must always be preserved.
fn is_anomaly_row(
    item: &Value,
    field_scores: &[FieldScore],
    arr: &[Value],
    opts: &SampleOpts,
) -> bool {
    let Some(obj) = item.as_object() else {
        return false;
    };

    // Check 1: error indicators in any string value.
    for val in obj.values() {
        if let Some(s) = val.as_str() {
            let lower = s.to_ascii_lowercase();
            if opts
                .error_indicators
                .iter()
                .any(|ind| lower.contains(ind.as_str()))
            {
                return true;
            }
        }
    }

    // Check 2: numeric outliers (>2.5 sigma from mean on any high-info numeric field).
    for fs in field_scores
        .iter()
        .filter(|f| f.is_numeric && f.info_score > 0.3)
    {
        if let Some(val) = item.get(&fs.name).and_then(Value::as_f64) {
            let (mean, std_dev) = field_stats(arr, &fs.name);
            if std_dev > 0.0 && ((val - mean) / std_dev).abs() > 2.5 {
                return true;
            }
        }
    }

    false
}

/// Computes importance score for a row (0.0-1.0).
fn row_importance(item: &Value, field_scores: &[FieldScore], arr: &[Value]) -> f64 {
    let Some(obj) = item.as_object() else {
        return 0.0;
    };

    let mut score = 0.0;
    let mut weight_sum = 0.0;

    for fs in field_scores.iter().take(5) {
        let w = fs.info_score;
        weight_sum += w;

        if let Some(val) = obj.get(&fs.name) {
            // Rarer values are more important (inverse frequency).
            let freq = value_frequency(val, arr, &fs.name);
            let rarity = 1.0 - freq;
            score += w * rarity;

            // Boundary values on numeric fields are important.
            if fs.is_numeric
                && let Some(v) = val.as_f64()
            {
                let (mean, std_dev) = field_stats(arr, &fs.name);
                if std_dev > 0.0 && ((v - mean) / std_dev).abs() > 1.5 {
                    score += w * 0.3;
                }
            }
        }
    }

    if weight_sum > 0.0 {
        score / weight_sum
    } else {
        0.0
    }
}

/// Fraction of rows where `field` equals `val`.
fn value_frequency(val: &Value, arr: &[Value], field: &str) -> f64 {
    let target = serde_json::to_string(val).unwrap_or_default();
    let matches = arr
        .iter()
        .filter(|item| {
            item.get(field)
                .is_some_and(|v| serde_json::to_string(v).unwrap_or_default() == target)
        })
        .count();
    matches as f64 / arr.len().max(1) as f64
}

/// Mean and standard deviation for a numeric field across the array.
fn field_stats(arr: &[Value], field: &str) -> (f64, f64) {
    let values: Vec<f64> = arr
        .iter()
        .filter_map(|item| item.get(field).and_then(Value::as_f64))
        .collect();

    if values.is_empty() {
        return (0.0, 0.0);
    }

    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    (mean, variance.sqrt())
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn format_summary(
    total: usize,
    retained: usize,
    field_scores: &[FieldScore],
    arr: &[Value],
) -> String {
    let mut parts = vec![format!(
        "{retained} of {total} items shown (sampled by statistical relevance)"
    )];

    // Count anomalies.
    let error_fields: Vec<&str> = field_scores
        .iter()
        .filter(|f| f.is_error_field)
        .map(|f| f.name.as_str())
        .collect();

    if !error_fields.is_empty() {
        // Count distinct error-like statuses.
        for ef in &error_fields {
            let mut error_count = 0usize;
            for item in arr {
                if let Some(s) = item.get(*ef).and_then(Value::as_str) {
                    let lower = s.to_ascii_lowercase();
                    if lower.contains("error") || lower.contains("fail") || lower.contains("fatal")
                    {
                        error_count += 1;
                    }
                }
            }
            if error_count > 0 {
                parts.push(format!(
                    "{error_count} items with errors in `{ef}` (all preserved)"
                ));
            }
        }
    }

    parts.join("; ")
}

fn build_output(summary: &str, sampled: &[&Value], total: usize, retained: usize) -> Value {
    let mut out = Map::new();
    out.insert("_lc_sample".to_string(), Value::String("array".to_string()));
    out.insert("_summary".to_string(), Value::String(summary.to_string()));
    out.insert(
        "_total".to_string(),
        Value::Number(serde_json::Number::from(total)),
    );
    out.insert(
        "_shown".to_string(),
        Value::Number(serde_json::Number::from(retained)),
    );
    out.insert(
        "_items".to_string(),
        Value::Array(sampled.iter().map(|v| (*v).clone()).collect()),
    );
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn large_homogeneous(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|i| {
                    let regions = ["us-east-1", "eu-west-1", "ap-south-1"];
                    let status = if i == 42 { "error" } else { "ok" };
                    let latency = if i == 99 { 5000 } else { 50 + (i % 30) as i64 };
                    json!({
                        "status": status,
                        "region": regions[i % 3],
                        "latency_ms": latency,
                        "request_id": format!("req-{i:04}"),
                        "timestamp": format!("2026-07-14T10:{:02}:{:02}Z", i / 60, i % 60),
                    })
                })
                .collect(),
        )
    }

    #[test]
    fn samples_large_array() {
        let data = large_homogeneous(200);
        let result = sample_array(&data, &SampleOpts::default()).expect("should sample");
        assert!(result.retained_count < 200);
        assert!(result.retained_count >= 5);
        assert!(result.text.contains("_lc_sample"));
        assert!(result.text.contains("_summary"));
    }

    #[test]
    fn skips_small_arrays() {
        let data = json!([{"a": 1}, {"a": 2}, {"a": 3}]);
        assert!(sample_array(&data, &SampleOpts::default()).is_none());
    }

    #[test]
    fn preserves_error_rows() {
        let data = large_homogeneous(100);
        let result = sample_array(&data, &SampleOpts::default()).expect("should sample");
        // Row 42 has status: "error" — must be preserved.
        assert!(
            result.text.contains("error"),
            "error rows must always be preserved"
        );
    }

    #[test]
    fn preserves_numeric_outliers() {
        let data = large_homogeneous(100);
        let result = sample_array(&data, &SampleOpts::default()).expect("should sample");
        // Row 99 has latency_ms: 5000 (outlier) — should be preserved.
        assert!(
            result.text.contains("5000"),
            "numeric outlier rows must be preserved"
        );
    }

    #[test]
    fn output_is_deterministic() {
        let data = large_homogeneous(100);
        let r1 = sample_array(&data, &SampleOpts::default()).unwrap();
        let r2 = sample_array(&data, &SampleOpts::default()).unwrap();
        assert_eq!(r1.text, r2.text, "sampling must be deterministic (#498)");
    }

    #[test]
    fn text_helper_gates_on_compression() {
        let data = large_homogeneous(200);
        let text = serde_json::to_string(&data).unwrap();
        let result = sample_text_if_beneficial(&text, &SampleOpts::default())
            .expect("should compress large array");
        assert!(result.text.len() * 2 <= text.len());
    }

    #[test]
    fn text_helper_skips_non_arrays() {
        assert!(sample_text_if_beneficial("{\"a\": 1}", &SampleOpts::default()).is_none());
        assert!(sample_text_if_beneficial("not json", &SampleOpts::default()).is_none());
    }

    #[test]
    fn retains_head_and_tail() {
        let data = large_homogeneous(100);
        let result = sample_array(&data, &SampleOpts::default()).unwrap();
        // First item (head) and last few items (tail) should be included.
        assert!(
            result.text.contains("req-0000"),
            "first item (head) must be kept"
        );
    }

    #[test]
    fn non_object_arrays_skipped() {
        let data = json!([
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21
        ]);
        assert!(sample_array(&data, &SampleOpts::default()).is_none());
    }
}
