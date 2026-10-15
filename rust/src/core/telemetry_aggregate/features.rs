// SPDX-License-Identifier: Apache-2.0

//! Daily tallies of CLI commands and background features (3.11.1+).
//!
//! Codes come from the closed registry in `core::telemetry_features`; this
//! module only counts them and turns a day's tallies into the wire event.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::telemetry_v2::{
    FeatureCount, FeatureMetrics, MAX_COUNT, MAX_FEATURE_ENTRIES, valid_feature_code,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FeatureTally {
    pub(super) count: u64,
    pub(super) failures: u64,
}

/// Adds to a feature's daily tally; a code beyond the per-day cap is dropped.
pub(super) fn add_feature(
    features: &mut BTreeMap<String, FeatureTally>,
    code: &str,
    extra: FeatureTally,
) {
    if !features.contains_key(code) && features.len() >= MAX_FEATURE_ENTRIES {
        return;
    }
    let tally = features.entry(code.to_string()).or_default();
    tally.count = tally.count.saturating_add(extra.count).min(MAX_COUNT);
    tally.failures = tally
        .failures
        .saturating_add(extra.failures)
        .min(tally.count);
}

/// The wire event for a day's tallies, or `None` when nothing was used.
pub(super) fn feature_metrics(features: &BTreeMap<String, FeatureTally>) -> Option<FeatureMetrics> {
    let features: Vec<FeatureCount> = features
        .iter()
        .filter(|(code, tally)| tally.count > 0 && valid_feature_code(code))
        .take(MAX_FEATURE_ENTRIES)
        .map(|(code, tally)| FeatureCount {
            feature: code.clone(),
            count: tally.count.min(MAX_COUNT),
            failures: tally.failures.min(tally.count),
        })
        .collect();
    (!features.is_empty()).then_some(FeatureMetrics { features })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tallies_are_bounded_and_capped_per_day() {
        let mut features = BTreeMap::new();
        add_feature(
            &mut features,
            "cli.pack.export",
            FeatureTally {
                count: 1,
                failures: 1,
            },
        );
        add_feature(
            &mut features,
            "cli.pack.export",
            FeatureTally {
                count: 1,
                failures: 0,
            },
        );
        assert_eq!(
            features["cli.pack.export"],
            FeatureTally {
                count: 2,
                failures: 1
            }
        );
        add_feature(
            &mut features,
            "index.graph",
            FeatureTally {
                count: 0,
                failures: 5,
            },
        );
        assert_eq!(
            features["index.graph"].failures, 0,
            "failures never exceed uses"
        );
        for index in 0..MAX_FEATURE_ENTRIES * 2 {
            add_feature(
                &mut features,
                &format!("cli.c{index}"),
                FeatureTally {
                    count: 1,
                    failures: 0,
                },
            );
        }
        assert_eq!(features.len(), MAX_FEATURE_ENTRIES);
    }

    #[test]
    fn the_wire_event_is_sorted_valid_and_skips_unused_codes() {
        let mut features = BTreeMap::new();
        add_feature(
            &mut features,
            "index.graph",
            FeatureTally {
                count: 3,
                failures: 1,
            },
        );
        add_feature(
            &mut features,
            "cli.pack.export",
            FeatureTally {
                count: 1,
                failures: 0,
            },
        );
        add_feature(
            &mut features,
            "index.bm25",
            FeatureTally {
                count: 0,
                failures: 0,
            },
        );
        let metrics = feature_metrics(&features).expect("used features");
        let codes: Vec<&str> = metrics
            .features
            .iter()
            .map(|f| f.feature.as_str())
            .collect();
        assert_eq!(codes, ["cli.pack.export", "index.graph"]);
        assert!(feature_metrics(&BTreeMap::new()).is_none());
        let json = serde_json::to_string(&metrics).unwrap();
        assert_eq!(
            json,
            r#"{"features":[{"feature":"cli.pack.export","count":1},{"feature":"index.graph","count":3,"failures":1}]}"#
        );
    }
}
