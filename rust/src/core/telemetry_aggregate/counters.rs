// SPDX-License-Identifier: Apache-2.0

//! Arithmetic on cumulative counter checkpoints: the delta a process adds
//! since its last fold, and the sum of deltas per day.

use super::{CounterCheckpoint, ToolCounterCheckpoint};
use crate::core::telemetry_failure::{add_kinds, sub_kinds};

pub(super) fn counter_delta(
    observed: &CounterCheckpoint,
    baseline: &CounterCheckpoint,
) -> CounterCheckpoint {
    CounterCheckpoint {
        tool_calls: observed.tool_calls.saturating_sub(baseline.tool_calls),
        tool_failures: observed
            .tool_failures
            .saturating_sub(baseline.tool_failures),
        tool_latency_buckets: std::array::from_fn(|index| {
            observed.tool_latency_buckets[index]
                .saturating_sub(baseline.tool_latency_buckets[index])
        }),
        session_uptime_secs: observed
            .session_uptime_secs
            .saturating_sub(baseline.session_uptime_secs),
        tools: observed
            .tools
            .iter()
            .filter_map(|(tool, counter)| {
                let base = baseline.tools.get(tool).copied().unwrap_or_default();
                let delta = ToolCounterCheckpoint {
                    calls: counter.calls.saturating_sub(base.calls),
                    failures: counter.failures.saturating_sub(base.failures),
                    latency_us: counter.latency_us.saturating_sub(base.latency_us),
                    failure_kinds: sub_kinds(&counter.failure_kinds, &base.failure_kinds),
                };
                (delta.calls > 0 || delta.failures > 0).then(|| (tool.clone(), delta))
            })
            .collect(),
        tokens_input: observed.tokens_input.saturating_sub(baseline.tokens_input),
        tokens_output: observed
            .tokens_output
            .saturating_sub(baseline.tokens_output),
        failure_messages: observed
            .failure_messages
            .iter()
            .filter_map(|(key, count)| {
                let base = baseline
                    .failure_messages
                    .get(key)
                    .copied()
                    .unwrap_or_default();
                let delta = count.saturating_sub(base);
                (delta > 0).then(|| (key.clone(), delta))
            })
            .collect(),
    }
}

pub(super) fn add_counters(total: &mut CounterCheckpoint, delta: &CounterCheckpoint) {
    total.tool_calls = total.tool_calls.saturating_add(delta.tool_calls);
    total.tool_failures = total.tool_failures.saturating_add(delta.tool_failures);
    for (bucket, added) in total
        .tool_latency_buckets
        .iter_mut()
        .zip(delta.tool_latency_buckets)
    {
        *bucket = bucket.saturating_add(added);
    }
    total.session_uptime_secs = total
        .session_uptime_secs
        .saturating_add(delta.session_uptime_secs);
    total.tokens_input = total.tokens_input.saturating_add(delta.tokens_input);
    total.tokens_output = total.tokens_output.saturating_add(delta.tokens_output);
    for (tool, added) in &delta.tools {
        if !total.tools.contains_key(tool) && total.tools.len() >= MAX_PERSISTED_TOOLS {
            continue;
        }
        let entry = total.tools.entry(tool.clone()).or_default();
        entry.calls = entry.calls.saturating_add(added.calls);
        entry.failures = entry.failures.saturating_add(added.failures);
        entry.latency_us = entry.latency_us.saturating_add(added.latency_us);
        add_kinds(&mut entry.failure_kinds, &added.failure_kinds);
    }
    for (key, added) in &delta.failure_messages {
        if !total.failure_messages.contains_key(key)
            && total.failure_messages.len() >= crate::core::telemetry::MAX_FAILURE_TEMPLATES
        {
            continue;
        }
        let entry = total.failure_messages.entry(key.clone()).or_default();
        *entry = entry.saturating_add(*added);
    }
}

/// Upper bound on distinct tool names kept in the sidecar; the batch itself
/// keeps at most [`super::MAX_TOOL_ENTRIES`] of them.
pub(super) const MAX_PERSISTED_TOOLS: usize = 256;
