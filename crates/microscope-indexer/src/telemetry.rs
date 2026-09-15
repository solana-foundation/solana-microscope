use std::time::{SystemTime, UNIX_EPOCH};

use metrics_exporter_prometheus::PrometheusBuilder;

const METRICS_PORT: u16 = 9090;

pub fn install() {
    // carbon-prometheus-metrics binds 127.0.0.1 only, which isn't reachable from other
    // docker-compose containers, so the exporter is installed directly here on 0.0.0.0.
    PrometheusBuilder::new()
        .with_http_listener(([0, 0, 0, 0], METRICS_PORT))
        .install()
        .expect("failed to install Prometheus exporter");

    // Seed the liveness gauge at startup so "seconds since last event" is measurable
    // (and alertable) even before the first decoded event arrives.
    record_event();
    initialize_transactions();
}

/// Seeds the counters the transaction panels chart, so a deployment that has
/// decoded or failed nothing charts zero rather than publishing no series.
pub fn initialize_transactions() {
    metrics::counter!("microscope_transactions_total").increment(0);
    metrics::counter!("microscope_errors_total").increment(0);
}

pub fn record_instruction(instruction_name: String) {
    metrics::counter!("microscope_instructions_total", "instruction" => instruction_name)
        .increment(1);
}

#[cfg(program_events)]
pub fn record_program_event(event_name: String, source: &'static str, failed: bool) {
    metrics::counter!(
        "microscope_program_events_total",
        "event" => event_name,
        "source" => source,
        "failed" => if failed { "true" } else { "false" }
    )
    .increment(1);
}

#[cfg(program_events)]
pub fn record_event_decode_failures(source: &'static str, rejected: u64) {
    metrics::counter!("microscope_event_decode_failures_total", "source" => source)
        .increment(rejected);
}

pub fn record_multisig_activity(version: &'static str, action: &'static str, failed: bool) {
    metrics::counter!(
        "microscope_multisig_activity_total",
        "provider" => "squads",
        "version" => version,
        "action" => action,
        "failed" => if failed { "true" } else { "false" }
    )
    .increment(1);
}

pub fn record_multisig_unmatched_state(version: &'static str) {
    metrics::counter!(
        "microscope_multisig_unmatched_state_total",
        "provider" => "squads",
        "version" => version
    )
    .increment(1);
}

pub fn record_transaction(failed: bool) {
    metrics::counter!("microscope_transactions_total").increment(1);
    if failed {
        metrics::counter!("microscope_errors_total").increment(1);
    }
}

pub fn record_event() {
    metrics::gauge!("microscope_last_event_unixtime").set(unix_now() as f64);
}

/// Seeds the counters at zero so the first disconnect registers as an increase
/// rather than as a series appearing, which `increase()` cannot see.
pub fn initialize_yellowstone() {
    record_yellowstone_disconnects(0, 0);
}

/// Counts an established stream going silent, which is narrower than the stream
/// being unusable: a subscribe that never succeeds notifies nothing, so a flat
/// counter is not evidence the datasource was connected.
/// `microscope_yellowstone_probe_healthy` covers that case.
pub fn record_yellowstone_disconnect(missed_slots: u64) {
    record_yellowstone_disconnects(1, missed_slots);
}

fn record_yellowstone_disconnects(disconnects: u64, missed_slots: u64) {
    metrics::counter!("microscope_yellowstone_disconnects_total").increment(disconnects);
    metrics::counter!("microscope_yellowstone_missed_slots_total").increment(missed_slots);
}

/// Seeds the same way readiness seeds its window: healthy until a probe says
/// otherwise, so the first attempt is not read as a failure.
pub fn initialize_yellowstone_probe() {
    metrics::counter!("microscope_yellowstone_probe_failures_total").increment(0);
    set_yellowstone_probe_healthy(true);
}

pub fn record_yellowstone_probe_failure() {
    metrics::counter!("microscope_yellowstone_probe_failures_total").increment(1);
    set_yellowstone_probe_healthy(false);
}

/// A rejected token or an unreachable endpoint leaves the process healthy and
/// consuming nothing, so the endpoint's own reachability has to be a state an
/// alert can read rather than an event in the logs.
pub fn set_yellowstone_probe_healthy(healthy: bool) {
    metrics::gauge!("microscope_yellowstone_probe_healthy").set(if healthy { 1.0 } else { 0.0 });
}

/// Seeds every counter the polling panels chart. Unseeded, a metric that never
/// fired, one that was renamed, and a target that was never scraped all render
/// identically.
pub fn initialize_rpc_polling() {
    metrics::gauge!("microscope_rpc_poll_started_unixtime").set(unix_now() as f64);
    set_rpc_quarantined_transactions(0);
    metrics::counter!("microscope_rpc_poll_transactions_total").increment(0);
    metrics::counter!("microscope_rpc_poll_failures_total").increment(0);
    metrics::counter!("microscope_rpc_poll_transaction_failures_total").increment(0);
    metrics::counter!("microscope_rpc_poll_quarantined_transactions_total").increment(0);
}

pub fn record_rpc_poll_success(head_slot: u64, scanned_slot: u64, transactions: u64) {
    crate::health::record_poll_success();
    metrics::gauge!("microscope_rpc_poll_last_success_unixtime").set(unix_now() as f64);
    metrics::gauge!("microscope_rpc_poll_head_slot").set(head_slot as f64);
    metrics::gauge!("microscope_rpc_poll_lag_slots")
        .set(head_slot.saturating_sub(scanned_slot) as f64);
    metrics::counter!("microscope_rpc_poll_transactions_total").increment(transactions);
}

pub fn record_rpc_poll_failure() {
    metrics::counter!("microscope_rpc_poll_failures_total").increment(1);
}

pub fn set_rpc_recovery_enabled(enabled: bool) {
    metrics::gauge!("microscope_rpc_recovery_enabled").set(if enabled { 1.0 } else { 0.0 });
}

pub fn record_rpc_recovery_disabled(reason: &'static str) {
    set_rpc_recovery_enabled(false);
    metrics::gauge!("microscope_rpc_recovery_degraded", "reason" => reason).set(1.0);
    metrics::counter!("microscope_rpc_recovery_disabled_total", "reason" => reason).increment(1);
}

pub fn record_rpc_checkpoint_success(scanned_slot: u64) {
    metrics::gauge!("microscope_rpc_checkpoint_last_success_unixtime").set(unix_now() as f64);
    metrics::gauge!("microscope_rpc_checkpoint_slot").set(scanned_slot as f64);
}

pub fn record_rpc_checkpoint_failure() {
    metrics::counter!("microscope_rpc_checkpoint_failures_total").increment(1);
}

pub fn set_rpc_checkpoint_quarantined_files(files: u64) {
    metrics::gauge!("microscope_rpc_checkpoint_quarantined_files").set(files as f64);
}

pub fn record_rpc_checkpoint_corrupt() {
    metrics::counter!("microscope_rpc_checkpoint_corrupt_total").increment(1);
}

pub fn record_rpc_history_unavailable() {
    metrics::counter!("microscope_rpc_history_unavailable_total").increment(1);
}

pub fn record_rpc_transaction_failure() {
    metrics::counter!("microscope_rpc_poll_transaction_failures_total").increment(1);
}

pub fn record_rpc_transaction_quarantine() {
    metrics::counter!("microscope_rpc_poll_quarantined_transactions_total").increment(1);
}

pub fn set_rpc_quarantined_transactions(transactions: u64) {
    metrics::gauge!("microscope_rpc_poll_quarantined_transactions").set(transactions as f64);
}

pub fn record_rpc_yellowstone_deliveries(transactions: u64) {
    metrics::counter!("microscope_rpc_poll_yellowstone_deliveries_total").increment(transactions);
}

pub fn set_rpc_recent_signatures(signatures: u64) {
    metrics::gauge!("microscope_rpc_poll_recent_signatures").set(signatures as f64);
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    /// The panels chart these directly, with no `vector(0)` fallback left to
    /// cover an unseeded counter.
    #[test]
    fn seeds_every_counter_the_panels_chart() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            super::initialize_transactions();
            super::initialize_rpc_polling();
        });

        let seeded = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(_, _, _, value)| *value == DebugValue::Counter(0))
            .map(|(key, _, _, _)| key.key().name().to_string())
            .collect::<Vec<_>>();

        for counter in [
            "microscope_transactions_total",
            "microscope_errors_total",
            "microscope_rpc_poll_transactions_total",
            "microscope_rpc_poll_failures_total",
            "microscope_rpc_poll_transaction_failures_total",
            "microscope_rpc_poll_quarantined_transactions_total",
        ] {
            assert!(seeded.iter().any(|name| name == counter), "{counter}");
        }
    }
}
