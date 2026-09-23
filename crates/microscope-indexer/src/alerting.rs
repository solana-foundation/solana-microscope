use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    config::{
        AlertChannel, AlertCondition, AlertConditionOperator, AlertConditionValue, AlertKind,
        AlertMatch, AlertRule, Config, DatasourceMode,
    },
    dashboard,
};

const RPC_HEALTH_EVALUATION_INTERVAL_SECONDS: u64 = 30;
/// Five times the idle checkpoint write interval, so only a sustained failure
/// pages.
const CHECKPOINT_STALE_AFTER_SECONDS: u64 = 300;
/// A dropped update is logged once, so the window only has to outlast the
/// evaluation interval.
const HEALTH_LOG_WINDOW_SECONDS: u64 = 120;
/// The datasource logs one timeout per `STREAM_TIMEOUT`, so a window that
/// short only ever holds one line. Seven fit here: a wedged stream crosses in
/// ~12 minutes, a replayed reconnect stays silent.
const STREAM_INTERRUPTION_WINDOW_SECONDS: u64 = 900;
const STREAM_INTERRUPTION_THRESHOLD: u64 = 5;
/// Long enough that `increase` sees the counter step across several scrapes,
/// so a single disconnect still resolves once the gap stops growing.
const YELLOWSTONE_GAP_WINDOW_SECONDS: u64 = 600;
/// Eight probes at the datasource's 15s interval, so a dropped dial does not
/// page on its own but a provider that refuses every connection does.
const YELLOWSTONE_PROBE_WINDOW_SECONDS: u64 = 120;
pub(crate) const RPC_POLL_FAILURE_WINDOW_SECONDS: u64 = 900;

pub fn generate(config: &Config, output_dir: &Path) -> anyhow::Result<PathBuf> {
    let rpc_polling = config.datasource.mode == DatasourceMode::Rpc
        || env::var("RPC_URL").is_ok_and(|url| !url.trim().is_empty());
    let channels = configured_channels(config);
    validate_contact_point_credentials(&channels)?;
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "creating alerting output directory {}",
            output_dir.display()
        )
    })?;

    let output = output_dir.join("microscope.json");
    let previous = read_existing_document(&output)?;
    let mut document = provisioning_document(config, &channels, rpc_polling)?;
    if let Some(previous) = previous.as_ref() {
        add_resource_deletions(&mut document, previous);
    }

    let temporary = output_dir.join(".microscope.json.tmp");
    let contents = serde_json::to_vec_pretty(&document)?;
    fs::write(&temporary, contents)
        .with_context(|| format!("writing temporary alerting config {}", temporary.display()))?;
    fs::rename(&temporary, &output)
        .with_context(|| format!("installing alerting config {}", output.display()))?;

    Ok(output)
}

fn read_existing_document(path: &Path) -> anyhow::Result<Option<Value>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("parsing existing alerting config {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("reading existing alerting config {}", path.display())),
    }
}

fn add_resource_deletions(document: &mut Value, previous: &Value) {
    let desired_rules = rule_uids(document);
    let mut previous_rules = rule_uids(previous);
    previous_rules.extend(deletion_uids(previous, "deleteRules"));
    let delete_rules = previous_rules
        .difference(&desired_rules)
        .filter(|uid| uid.starts_with("ms-"))
        .map(|uid| json!({ "orgId": 1, "uid": uid }))
        .collect::<Vec<_>>();
    if !delete_rules.is_empty() {
        document["deleteRules"] = json!(delete_rules);
    }

    let desired_contact_points = contact_point_uids(document);
    let mut previous_contact_points = contact_point_uids(previous);
    previous_contact_points.extend(deletion_uids(previous, "deleteContactPoints"));
    let delete_contact_points = previous_contact_points
        .difference(&desired_contact_points)
        .filter(|uid| uid.starts_with("microscope-"))
        .map(|uid| json!({ "orgId": 1, "uid": uid }))
        .collect::<Vec<_>>();
    if !delete_contact_points.is_empty() {
        document["deleteContactPoints"] = json!(delete_contact_points);
    }
}

fn rule_uids(document: &Value) -> BTreeSet<String> {
    document["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group["rules"].as_array())
        .flatten()
        .filter_map(|rule| rule["uid"].as_str())
        .map(str::to_string)
        .collect()
}

fn deletion_uids(document: &Value, field: &str) -> BTreeSet<String> {
    document[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|resource| resource["uid"].as_str())
        .map(str::to_string)
        .collect()
}

fn contact_point_uids(document: &Value) -> BTreeSet<String> {
    document["contactPoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|contact_point| contact_point["receivers"].as_array())
        .flatten()
        .filter_map(|receiver| receiver["uid"].as_str())
        .map(str::to_string)
        .collect()
}

fn provisioning_document(
    config: &Config,
    channels: &BTreeSet<AlertChannel>,
    rpc_polling: bool,
) -> anyhow::Result<Value> {
    let pending_seconds = config.alerting.health_pending_period_seconds;
    let mut rules_by_interval = BTreeMap::<u64, Vec<Value>>::new();
    rules_by_interval
        .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
        .or_default()
        .extend(rpc_recovery_degraded_rules(channels, pending_seconds));
    rules_by_interval
        .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
        .or_default()
        .extend(datasource_drop_rules(channels, pending_seconds));
    if config.datasource.mode == DatasourceMode::Yellowstone {
        rules_by_interval
            .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
            .or_default()
            .extend(yellowstone_health_rules(channels, pending_seconds));
    }
    rules_by_interval
        .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
        .or_default()
        .extend(log_delivery_rules(channels, pending_seconds));
    if config.multisig.is_some() {
        rules_by_interval
            .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
            .or_default()
            .extend(multisig_health_rules(
                channels,
                config.alerting.multisig_unmatched_window_seconds,
                pending_seconds,
            ));
    }
    if rpc_polling {
        rules_by_interval
            .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
            .or_default()
            .extend(rpc_health_rules(config, channels, pending_seconds));
    }
    for alert in &config.alert_rules {
        let rules = rules_by_interval
            .entry(alert.evaluation_interval_seconds(&config.alerting))
            .or_default();
        if alert.channels.is_empty() {
            rules.push(grafana_rule(config, alert, None)?);
        } else {
            for channel in &alert.channels {
                rules.push(grafana_rule(config, alert, Some(*channel))?);
            }
        }
    }
    let groups = rules_by_interval
        .into_iter()
        .map(|(interval_seconds, rules)| {
            json!({
                "orgId": 1,
                "name": format!("microscope-{interval_seconds}s"),
                "folder": "Microscope",
                "interval": format!("{interval_seconds}s"),
                "rules": rules,
            })
        })
        .collect::<Vec<_>>();

    let mut contact_points = vec![json!({
        "orgId": 1,
        "name": "microscope-noop",
        "receivers": [{
            "uid": "microscope-noop",
            "type": "webhook",
            // This receiver is required by Grafana's policy schema but is never called:
            // every channel=none rule is routed through the always-muted child policy.
            "settings": { "url": "http://127.0.0.1:9" },
            "disableResolveMessage": true,
        }],
    })];
    for channel in channels {
        contact_points.push(contact_point(*channel));
    }

    let mut routes = vec![json!({
        "receiver": "microscope-noop",
        "object_matchers": [["channel", "=", "none"]],
        "mute_time_intervals": ["microscope-always"],
        "continue": false,
    })];
    routes.extend(channels.iter().map(|channel| {
        json!({
            "receiver": format!("microscope-{channel}"),
            "object_matchers": [["channel", "=", channel.as_str()]],
            "continue": false,
        })
    }));

    Ok(json!({
        "apiVersion": 1,
        "groups": groups,
        "contactPoints": contact_points,
        "muteTimes": [{
            "orgId": 1,
            "name": "microscope-always",
            "time_intervals": [{
                "location": "UTC",
                "times": [{ "start_time": "00:00", "end_time": "24:00" }],
            }],
        }],
        "policies": [{
            "orgId": 1,
            "receiver": "microscope-noop",
            "group_by": ["alertname", "channel"],
            "group_wait": "5s",
            "group_interval": "1m",
            "repeat_interval": "4h",
            "routes": routes,
        }],
    }))
}

/// Deployments sharing a stack all push `service_name="microscope-indexer"`,
/// so an unscoped grep counts their interruptions as this one's. Alloy labels
/// each stream with the same variable.
fn log_stream_selector() -> String {
    log_stream_selector_for(env::var("MICROSCOPE_DEPLOYMENT").ok().as_deref())
}

fn log_stream_selector_for(deployment: Option<&str>) -> String {
    match deployment.map(str::trim) {
        Some(deployment) if !deployment.is_empty() => {
            format!("{{service_name=\"microscope-indexer\", deployment=\"{deployment}\"}}")
        }
        _ => "{service_name=\"microscope-indexer\"}".to_string(),
    }
}

fn health_channel_names(channels: &BTreeSet<AlertChannel>) -> Vec<&str> {
    if channels.is_empty() {
        vec!["none"]
    } else {
        channels.iter().map(|channel| channel.as_str()).collect()
    }
}

/// Grafana Cloud's notification-policy tree belongs to the whole org, so a
/// deployment that shares a stack cannot add the channel routes this document
/// declares. Naming the receiver on the rule reaches the same contact point
/// through a route Grafana derives per rule. `channel = none` keeps routing
/// through the tree, where the always-on mute timing lives.
fn notification_settings(channel: &str) -> Option<Value> {
    (channel != "none").then(|| json!({ "receiver": format!("microscope-{channel}") }))
}

fn rpc_recovery_degraded_rules(
    channels: &BTreeSet<AlertChannel>,
    pending_seconds: u64,
) -> Vec<Value> {
    health_channel_names(channels)
        .into_iter()
        .flat_map(|channel| {
            [
                prometheus_health_rule(
                    &format!("ms-rpc-recovery-degraded-{channel}"),
                    &format!("RPC recovery disabled [{channel}]"),
                    "max(microscope_rpc_recovery_degraded)",
                    "$B > 0",
                    "OK",
                    "critical",
                    "rpc_recovery_disabled",
                    channel,
                    "RPC recovery disabled itself and stays off until the indexer restarts; the reason label on microscope_rpc_recovery_degraded says whether the cause was a checkpoint failure, exhausted provider history, or a cursor past the confirmed head. In Yellowstone mode the stack continues but gaps are no longer recovered; in RPC mode nothing is indexed at all.",
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-checkpoint-corrupt-{channel}"),
                    &format!("RPC checkpoint recovered from corruption [{channel}]"),
                    "microscope_rpc_checkpoint_quarantined_files",
                    "$B > 0",
                    "OK",
                    "critical",
                    "rpc_checkpoint_corrupt",
                    channel,
                    "A corrupt RPC checkpoint was moved aside. Recovery restarted from the configured replay window; backfill may be required for older activity. This resolves once the quarantined rpc-polling.json.corrupt-* file is deleted.",
                                    pending_seconds,
                ),
            ]
        })
        .collect()
}

fn datasource_drop_rules(channels: &BTreeSet<AlertChannel>, pending_seconds: u64) -> Vec<Value> {
    health_channel_names(channels)
        .into_iter()
        .map(|channel| {
            loki_health_rule(
                &format!("ms-datasource-drop-{channel}"),
                &format!("Datasource updates dropped [{channel}]"),
                &format!(
                    "(sum(count_over_time({} |~ \"Failed to send (account|transaction)\" [{HEALTH_LOG_WINDOW_SECONDS}s])) or on() vector(0))",
                    log_stream_selector()
                ),
                HEALTH_LOG_WINDOW_SECONDS,
                "$B > 0",
                "critical",
                "datasource_updates_dropped",
                channel,
                "The Yellowstone datasource discarded a monitored update because the channel it forwards into was full; the signature is in the log line. With RPC_URL configured the poller re-delivers it within the replay window, so confirm the transaction was indexed rather than treating it as lost. Without RPC_URL the transaction is lost and that slot needs a backfill.",
                            pending_seconds,
            )
        })
        .collect()
}

fn yellowstone_health_rules(channels: &BTreeSet<AlertChannel>, pending_seconds: u64) -> Vec<Value> {
    health_channel_names(channels)
        .into_iter()
        .flat_map(|channel| {
            [
                loki_health_rule(
                    &format!("ms-yellowstone-stream-{channel}"),
                    &format!("Yellowstone stream interrupted [{channel}]"),
                    &format!(
                        "(sum(count_over_time({} |~ \"Stream timeout - no messages|Failed to subscribe|Geyser stream error|Stream closed\" [{STREAM_INTERRUPTION_WINDOW_SECONDS}s])) or on() vector(0))",
                        log_stream_selector()
                    ),
                    STREAM_INTERRUPTION_WINDOW_SECONDS,
                    &format!("$B > {STREAM_INTERRUPTION_THRESHOLD}"),
                    "error",
                    "yellowstone_stream_interrupted",
                    channel,
                    "The Yellowstone stream closed, timed out, or could not resubscribe more than five times in fifteen minutes. No on-chain activity is observed while this fires, and the indexer resumes at the stream head, so the interrupted interval is lost unless RPC_URL is configured.",
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-yellowstone-gap-{channel}"),
                    &format!("Yellowstone gap unrecovered [{channel}]"),
                    &format!("max(increase(microscope_yellowstone_disconnects_total[{YELLOWSTONE_GAP_WINDOW_SECONDS}s]) and on() microscope_rpc_recovery_enabled == 0)"),
                    "$B > 0",
                    "OK",
                    "error",
                    "yellowstone_gap_unrecovered",
                    channel,
                    "The Yellowstone stream reconnected at the provider's current head after a disconnect while RPC recovery is not replaying the gap, either because RPC_URL is unset or because recovery disabled itself (see the RPC recovery disabled alert). The transactions in the gap will never be indexed. microscope_yellowstone_missed_slots_total gives the size of the gap; backfill this interval.",
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-yellowstone-unreachable-{channel}"),
                    &format!("Yellowstone endpoint unreachable [{channel}]"),
                    &format!("max(max_over_time(microscope_yellowstone_probe_healthy[{YELLOWSTONE_PROBE_WINDOW_SECONDS}s]))"),
                    "$B < 1",
                    "Alerting",
                    "error",
                    "yellowstone_endpoint_unreachable",
                    channel,
                    "Every probe of the Yellowstone endpoint failed, so the datasource cannot establish a stream. The disconnect and missed-slot counters stay flat throughout, because they only move once an established stream goes silent, so this is the only metric that shows a provider refusing connections. No data also alerts here: the probe gauge is seeded at startup, so its absence means the indexer is gone.",
                                    pending_seconds,
                ),
            ]
        })
        .collect()
}

fn log_delivery_rules(channels: &BTreeSet<AlertChannel>, pending_seconds: u64) -> Vec<Value> {
    health_channel_names(channels)
        .into_iter()
        .map(|channel| {
            prometheus_health_rule(
                &format!("ms-log-delivery-stalled-{channel}"),
                &format!("Log delivery stalled [{channel}]"),
                "sum(increase(microscope_transactions_total[10m])) > 0 unless sum(increase(loki_write_sent_entries_total[10m])) > 0",
                "$B > 0",
                "OK",
                "error",
                "log_delivery_stalled",
                channel,
                "The indexer decoded transactions but Alloy shipped no log entries. Decoded records are not reaching Loki, so every activity alert evaluates against an empty stream and stays silent.",
                            pending_seconds,
            )
        })
        .collect()
}

fn multisig_health_rules(
    channels: &BTreeSet<AlertChannel>,
    window_seconds: u64,
    pending_seconds: u64,
) -> Vec<Value> {
    health_channel_names(channels)
        .into_iter()
        .map(|channel| {
            prometheus_health_rule(
                &format!("ms-multisig-unmatched-{channel}"),
                &format!("Squads activity for another multisig [{channel}]"),
                &format!(
                    "max(increase(microscope_multisig_unmatched_state_total[{window_seconds}s]))"
                ),
                "$B > 0",
                "OK",
                "error",
                "multisig_unmatched_state",
                channel,
                "Squads instructions decoded but referenced a different internal state account than the configured one, so no multisig activity was recorded for the configured vault. The multisig.state_address or multisig.version is wrong for this vault. Without this alert a misconfigured vault is indistinguishable from an idle one: both leave the multisig panels empty.",
                            pending_seconds,
            )
        })
        .collect()
}

fn rpc_health_rules(
    config: &Config,
    channels: &BTreeSet<AlertChannel>,
    pending_seconds: u64,
) -> Vec<Value> {
    let channel_names = health_channel_names(channels);
    let stale_after_seconds = config.datasource.rpc_poll_stale_after_seconds();
    let lag_threshold_slots = config.datasource.replay_window_slots.saturating_mul(2);
    let failure_threshold = (config.alerting.rpc_poll_sustained_failure_seconds
        / config.datasource.poll_interval_seconds.max(1))
    .max(1);
    let sustained_failure_seconds = config.alerting.rpc_poll_sustained_failure_seconds;
    channel_names
        .into_iter()
        .flat_map(|channel| {
            [
                prometheus_health_rule(
                    &format!("ms-rpc-poll-stale-{channel}"),
                    &format!("RPC polling stale [{channel}]"),
                    "time() - (microscope_rpc_poll_last_success_unixtime or microscope_rpc_poll_started_unixtime)",
                    &format!("$B > {stale_after_seconds}"),
                    "Alerting",
                    "error",
                    "rpc_poll_stale",
                    channel,
                    &format!(
                        "No RPC poll has completed successfully within {stale_after_seconds} seconds."
                    ),
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-poll-lag-{channel}"),
                    &format!("RPC polling lagging [{channel}]"),
                    "microscope_rpc_poll_lag_slots",
                    &format!("$B > {lag_threshold_slots}"),
                    "OK",
                    "warning",
                    "rpc_poll_lag",
                    channel,
                    &format!(
                        "RPC polling cursors trail the confirmed head by more than {lag_threshold_slots} slots (twice the replay window). Polls are succeeding but transactions are not being decoded; check for unfetchable transactions or provider history gaps."
                    ),
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-poll-failing-{channel}"),
                    &format!("RPC polling failing [{channel}]"),
                    &format!(
                        "increase(microscope_rpc_poll_failures_total[{RPC_POLL_FAILURE_WINDOW_SECONDS}s])"
                    ),
                    &format!("$B > {failure_threshold}"),
                    "OK",
                    "warning",
                    "rpc_poll_failing",
                    channel,
                    &format!(
                        "More than {failure_threshold} RPC polls failed in the last {RPC_POLL_FAILURE_WINDOW_SECONDS} seconds, the count an unbroken outage produces in {sustained_failure_seconds} seconds. Enough polls are still succeeding to keep the freshness and lag rules green, so gap recovery is partially dead rather than stopped; read the poll failure logs for the RPC error."
                    ),
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-checkpoint-stale-{channel}"),
                    &format!("RPC checkpoint stale [{channel}]"),
                    "time() - (microscope_rpc_checkpoint_last_success_unixtime or microscope_rpc_poll_started_unixtime)",
                    &format!("$B > {CHECKPOINT_STALE_AFTER_SECONDS}"),
                    "Alerting",
                    "error",
                    "rpc_checkpoint_stale",
                    channel,
                    &format!(
                        "No RPC recovery checkpoint has been written within {CHECKPOINT_STALE_AFTER_SECONDS} seconds. Polling continues, but a restart would resume from a stale cursor and re-fetch or miss the uncheckpointed window."
                    ),
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-quarantine-{channel}"),
                    &format!("RPC transactions quarantined [{channel}]"),
                    "microscope_rpc_poll_quarantined_transactions",
                    "$B > 0",
                    "OK",
                    "error",
                    "rpc_transaction_quarantined",
                    channel,
                    "RPC transactions were skipped after bounded fetch or conversion failures.",
                                    pending_seconds,
                ),
            ]
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn prometheus_health_rule(
    uid: &str,
    title: &str,
    query: &str,
    condition: &str,
    no_data_state: &str,
    severity: &str,
    signal_name: &str,
    channel: &str,
    description: &str,
    pending_seconds: u64,
) -> Value {
    health_rule(
        uid,
        title,
        json!({
            "refId": "A",
            "queryType": "",
            "relativeTimeRange": { "from": 600, "to": 0 },
            "datasourceUid": "prometheus",
            "model": {
                "datasource": { "type": "prometheus", "uid": "prometheus" },
                "editorMode": "code",
                "expr": query,
                "instant": true,
                "intervalMs": 15000,
                "legendFormat": "__auto",
                "maxDataPoints": 43200,
                "range": false,
                "refId": "A",
            },
        }),
        condition,
        pending_seconds,
        no_data_state,
        severity,
        signal_name,
        channel,
        description,
    )
}

#[allow(clippy::too_many_arguments)]
fn loki_health_rule(
    uid: &str,
    title: &str,
    query: &str,
    window_seconds: u64,
    condition: &str,
    severity: &str,
    signal_name: &str,
    channel: &str,
    description: &str,
    pending_seconds: u64,
) -> Value {
    health_rule(
        uid,
        title,
        json!({
            "refId": "A",
            "queryType": "range",
            "relativeTimeRange": { "from": window_seconds, "to": 0 },
            "datasourceUid": "loki",
            "model": {
                "datasource": { "type": "loki", "uid": "loki" },
                "editorMode": "code",
                "expr": query,
                "intervalMs": 1000,
                "maxDataPoints": 43200,
                "queryType": "range",
                "refId": "A",
            },
        }),
        condition,
        pending_seconds.min(window_seconds / 2),
        "OK",
        severity,
        signal_name,
        channel,
        description,
    )
}

#[allow(clippy::too_many_arguments)]
fn health_rule(
    uid: &str,
    title: &str,
    query: Value,
    condition: &str,
    pending_seconds: u64,
    no_data_state: &str,
    severity: &str,
    signal_name: &str,
    channel: &str,
    description: &str,
) -> Value {
    let mut rule = json!({
        "uid": uid,
        "title": title,
        "condition": "C",
        "data": [
            query,
            {
                "refId": "B",
                "queryType": "",
                "relativeTimeRange": { "from": 0, "to": 0 },
                "datasourceUid": "__expr__",
                "model": {
                    "conditions": [],
                    "datasource": { "type": "__expr__", "uid": "__expr__" },
                    "expression": "A",
                    "reducer": "last",
                    "refId": "B",
                    "settings": { "mode": "dropNN" },
                    "type": "reduce",
                },
            },
            {
                "refId": "C",
                "queryType": "",
                "relativeTimeRange": { "from": 0, "to": 0 },
                "datasourceUid": "__expr__",
                "model": {
                    "conditions": [],
                    "datasource": { "type": "__expr__", "uid": "__expr__" },
                    "expression": condition,
                    "refId": "C",
                    "type": "math",
                },
            },
        ],
        "noDataState": no_data_state,
        "execErrState": "Error",
        "for": format!("{pending_seconds}s"),
        "annotations": {
            "description": description,
            "summary": title,
        },
        "labels": {
            "channel": channel,
            "severity": severity,
            "signal_kind": "datasource",
            "signal_name": signal_name,
        },
        "isPaused": false,
    });
    if let Some(settings) = notification_settings(channel) {
        rule["notification_settings"] = settings;
    }
    rule
}

fn grafana_rule(
    config: &Config,
    alert: &AlertRule,
    channel: Option<AlertChannel>,
) -> anyhow::Result<Value> {
    let channel = channel.map(AlertChannel::as_str).unwrap_or("none");
    let query = logql_query(config, alert)?;
    let lookback_window_seconds = alert.lookback_window_seconds(&config.alerting);
    let conditions = serde_json::to_string(&alert.conditions)?;
    let canonical = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}",
        config.program_id,
        alert.kind,
        alert.name,
        alert.match_mode.as_str(),
        conditions,
        alert.severity,
        channel,
    );
    let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
    // Grafana UIDs are limited to 40 characters. A 128-bit SHA-256 prefix keeps
    // the ID deterministic with ample collision resistance and fits comfortably.
    let uid = format!("ms-{}", &digest[..32]);
    let title = format!("{} {} [{}]", alert.kind, alert.name, channel);
    let description = if alert.conditions.is_empty() {
        format!("Matched {} {}", alert.kind, alert.name)
    } else {
        let conditions = alert
            .conditions
            .iter()
            .map(condition_description)
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(&format!(" {} ", alert.match_mode.as_str()));
        format!("Matched {} {} where {conditions}", alert.kind, alert.name)
    };

    let transaction_url = config
        .alerting
        .explorer_transaction_url
        .replace("{signature}", "{{ $labels.signature }}");
    let mut annotations = json!({
        "description": description,
        "summary": format!("Microscope matched {} {}", alert.kind, alert.name),
        "transaction": format!("{{{{ $labels.signature }}}} {transaction_url}"),
    });
    // Grafana rejects a rule carrying one of these two without the other.
    if let Some(panel_id) = log_panel_id(alert.kind) {
        annotations["__dashboardUid__"] = json!(dashboard::OVERVIEW_UID);
        annotations["__panelId__"] = json!(panel_id.to_string());
    }

    let mut rule = json!({
        "uid": uid,
        "title": title,
        "condition": "C",
        "data": [
            {
                "refId": "A",
                "queryType": "range",
                "relativeTimeRange": { "from": lookback_window_seconds, "to": 0 },
                "datasourceUid": "loki",
                "model": {
                    "datasource": { "type": "loki", "uid": "loki" },
                    "editorMode": "code",
                    "expr": query,
                    "intervalMs": 1000,
                    "maxDataPoints": 43200,
                    "queryType": "range",
                    "refId": "A",
                },
            },
            {
                "refId": "B",
                "queryType": "",
                "relativeTimeRange": { "from": 0, "to": 0 },
                "datasourceUid": "__expr__",
                "model": {
                    "conditions": [],
                    "datasource": { "type": "__expr__", "uid": "__expr__" },
                    "expression": "A",
                    "reducer": "last",
                    "refId": "B",
                    "settings": { "mode": "dropNN" },
                    "type": "reduce",
                },
            },
            {
                "refId": "C",
                "queryType": "",
                "relativeTimeRange": { "from": 0, "to": 0 },
                "datasourceUid": "__expr__",
                "model": {
                    "conditions": [],
                    "datasource": { "type": "__expr__", "uid": "__expr__" },
                    "expression": "$B > 0",
                    "refId": "C",
                    "type": "math",
                },
            },
        ],
        "noDataState": "OK",
        // A match counts for one lookback window, so any pending period long
        // enough to absorb a failed evaluation would mute these for good.
        // Without one, the error itself must not notify.
        "execErrState": "OK",
        "for": "0s",
        "annotations": annotations,
        "labels": {
            "channel": channel,
            "program_id": config.program_id,
            "severity": alert.severity.as_str(),
            "signal_kind": alert.kind.as_str(),
            "signal_name": alert.name,
        },
        "isPaused": false,
    });
    if let Some(settings) = notification_settings(channel) {
        rule["notification_settings"] = settings;
    }

    Ok(rule)
}

fn log_panel_id(kind: AlertKind) -> Option<u64> {
    match kind {
        AlertKind::Event => Some(dashboard::EVENT_PANEL_ID),
        AlertKind::Multisig => Some(dashboard::MULTISIG_PANEL_ID),
        AlertKind::Instruction => None,
    }
}

fn logql_query(config: &Config, alert: &AlertRule) -> anyhow::Result<String> {
    let (record_kind, signal_path, scope_path, scope_value) = match alert.kind {
        AlertKind::Event => ("program_event", "name", "program_id", &config.program_id),
        AlertKind::Instruction => (
            "program_instruction",
            "name",
            "program_id",
            &config.program_id,
        ),
        AlertKind::Multisig => {
            let Some(multisig) = config.multisig.as_ref() else {
                bail!(
                    "multisig alert {} requires a [multisig] section in the config",
                    alert.name
                );
            };
            (
                "multisig_activity",
                "action",
                "vault_address",
                &multisig.vault_address,
            )
        }
    };
    let quoted_name = serde_json::to_string(&alert.name)?;
    let quoted_scope = serde_json::to_string(scope_value)?;

    let mut parser = format!(
        "| json kind=\"kind\", signal=\"{signal_path}\", scope=\"{scope_path}\", signature=\"signature\""
    );
    let mut filters =
        format!("| kind = \"{record_kind}\" | signal = {quoted_name} | scope = {quoted_scope}");
    if alert.kind == AlertKind::Multisig {
        parser.push_str(", failed=\"failed\"");
        filters.push_str(" | failed = \"false\"");
    }
    let lookback_window_seconds = alert.lookback_window_seconds(&config.alerting);
    let indexed_conditions = alert.conditions.iter().enumerate().collect::<Vec<_>>();
    let count_query = |conditions: &[(usize, &AlertCondition)]| -> anyhow::Result<String> {
        let mut parser = parser.clone();
        let mut condition_filters = Vec::with_capacity(conditions.len());
        for (index, condition) in conditions {
            condition.validate()?;
            let label = format!("condition_{index}");
            parser.push_str(&format!(", {label}=\"{}\"", condition.field));
            condition_filters.push(condition_filter(condition, &label)?);
        }

        let mut filters = filters.clone();
        if !condition_filters.is_empty() {
            filters.push_str(" | (");
            filters.push_str(&condition_filters.join(" and "));
            filters.push(')');
        }

        Ok(format!(
            "sum by (signature) (count_over_time({{service_name=\"microscope-indexer\"}} {parser} {filters} | __error__ = \"\" [{lookback_window_seconds}s]))"
        ))
    };

    // Loki marks failed numeric conversions with __error__. Independent OR
    // branches keep one missing field from hiding a match in another branch.
    let matched = if alert.match_mode == AlertMatch::Any && indexed_conditions.len() > 1 {
        indexed_conditions
            .iter()
            .map(|condition| count_query(std::slice::from_ref(condition)))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(" or ")
    } else {
        count_query(&indexed_conditions)?
    };

    Ok(format!("({matched} or on() vector(0))"))
}

fn condition_filter(condition: &AlertCondition, label: &str) -> anyhow::Result<String> {
    match (condition.operator, &condition.value) {
        (AlertConditionOperator::Exists, None) => Ok(format!("{label} != \"\"")),
        (AlertConditionOperator::Contains, Some(AlertConditionValue::String(value))) => {
            Ok(format!(
                "{label} =~ {}",
                serde_json::to_string(&format!(".*{}.*", regex::escape(value)))?
            ))
        }
        (
            AlertConditionOperator::Ne,
            Some(value @ (AlertConditionValue::String(_) | AlertConditionValue::Boolean(_))),
        ) => Ok(format!(
            "{label} != \"\" and {label} != {}",
            logql_value(value)?
        )),
        (operator, Some(value)) => Ok(format!(
            "{label} {} {}",
            logql_operator(operator),
            logql_value(value)?
        )),
        _ => bail!("invalid condition for field {}", condition.field),
    }
}

fn logql_operator(operator: AlertConditionOperator) -> &'static str {
    match operator {
        AlertConditionOperator::Eq => "=",
        AlertConditionOperator::Ne => "!=",
        AlertConditionOperator::Gt => ">",
        AlertConditionOperator::Gte => ">=",
        AlertConditionOperator::Lt => "<",
        AlertConditionOperator::Lte => "<=",
        AlertConditionOperator::Exists | AlertConditionOperator::Contains => {
            unreachable!("special condition operators are rendered separately")
        }
    }
}

fn logql_value(value: &AlertConditionValue) -> anyhow::Result<String> {
    match value {
        AlertConditionValue::Integer(value) => Ok(value.to_string()),
        AlertConditionValue::Float(value) => Ok(serde_json::to_string(value)?),
        AlertConditionValue::Boolean(value) => Ok(serde_json::to_string(&value.to_string())?),
        AlertConditionValue::String(value) => Ok(serde_json::to_string(value)?),
    }
}

fn condition_description(condition: &AlertCondition) -> anyhow::Result<String> {
    if condition.operator == AlertConditionOperator::Exists {
        return Ok(format!("{} is present and non-empty", condition.field));
    }

    let value = condition
        .value
        .as_ref()
        .context("validated conditions have a value")?;
    Ok(format!(
        "{} {} {}",
        condition.field,
        condition.operator.as_str(),
        serde_json::to_string(value)?
    ))
}

fn contact_point(channel: AlertChannel) -> Value {
    let settings = match channel {
        AlertChannel::Slack => json!({ "url": "$SLACK_WEBHOOK_URL" }),
        AlertChannel::Telegram => json!({
            "bottoken": "$TELEGRAM_BOT_TOKEN",
            "chatid": "$TELEGRAM_CHAT_ID",
        }),
        AlertChannel::Pagerduty => json!({
            "integrationKey": "$PAGERDUTY_INTEGRATION_KEY",
            "severity": "{{ .CommonLabels.severity }}",
        }),
    };

    json!({
        "orgId": 1,
        "name": format!("microscope-{channel}"),
        "receivers": [{
            "uid": format!("microscope-{channel}"),
            "type": channel.as_str(),
            "settings": settings,
            "disableResolveMessage": false,
        }],
    })
}

fn configured_channels(config: &Config) -> BTreeSet<AlertChannel> {
    config
        .alert_rules
        .iter()
        .flat_map(|alert| alert.channels.iter().copied())
        .collect()
}

fn validate_contact_point_credentials(channels: &BTreeSet<AlertChannel>) -> anyhow::Result<()> {
    for channel in channels {
        let required = match channel {
            AlertChannel::Slack => &["SLACK_WEBHOOK_URL"][..],
            AlertChannel::Telegram => &["TELEGRAM_BOT_TOKEN", "TELEGRAM_CHAT_ID"][..],
            AlertChannel::Pagerduty => &["PAGERDUTY_INTEGRATION_KEY"][..],
        };
        for variable in required {
            if env::var(variable).map_or(true, |value| value.trim().is_empty()) {
                bail!("{variable} must be set because an alert uses the {channel} channel");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::config::{
        AlertChannel, AlertCondition, AlertConditionOperator, AlertConditionValue, AlertKind,
        AlertMatch, AlertRule, AlertSeverity, AlertingConfig, Config, DashboardConfig,
        DatasourceConfig, DatasourceMode, MultisigConfig, MultisigVersion,
    };

    use super::{
        add_resource_deletions, deletion_uids, grafana_rule, log_stream_selector_for, logql_query,
        provisioning_document, rule_uids,
    };

    fn alert(kind: AlertKind, name: &str) -> AlertRule {
        AlertRule {
            kind,
            name: name.to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![],
            severity: AlertSeverity::Warning,
            channels: vec![],
            lookback_window_seconds: None,
            evaluation_interval_seconds: None,
        }
    }

    fn alert_with_channel(kind: AlertKind, name: &str, channel: AlertChannel) -> AlertRule {
        let mut alert = alert(kind, name);
        alert.channels = vec![channel];
        alert
    }

    fn config(alert_rules: Vec<AlertRule>) -> Config {
        Config {
            program_id: "11111111111111111111111111111111".to_string(),
            idl_path: "Cargo.toml".to_string(),
            multisig: Some(MultisigConfig {
                vault_address: "11111111111111111111111111111111".to_string(),
                state_address: "11111111111111111111111111111111".to_string(),
                version: MultisigVersion::V4,
            }),
            datasource: DatasourceConfig::default(),
            alerting: AlertingConfig::default(),
            dashboard: DashboardConfig::default(),
            alert_rules,
        }
    }

    #[test]
    fn generates_recovery_degradation_alerts_in_yellowstone_mode() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let degraded = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .find(|rule| rule["uid"] == "ms-rpc-recovery-degraded-slack")
            .unwrap();

        assert_eq!(
            degraded["data"][0]["model"]["expr"],
            "max(microscope_rpc_recovery_degraded)"
        );
        assert_eq!(degraded["noDataState"], "OK");
        assert_eq!(degraded["labels"]["severity"], "critical");
        assert!(document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .any(|rule| rule["uid"] == "ms-rpc-checkpoint-corrupt-slack"));
    }

    #[test]
    fn routes_dropped_datasource_updates_to_a_contact_point_in_every_mode() {
        let mut config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let drop_rule = |document: &serde_json::Value| {
            document["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["rules"].as_array().unwrap())
                .find(|rule| rule["uid"] == "ms-datasource-drop-slack")
                .cloned()
        };

        let rule = drop_rule(&provisioning_document(&config, &channels, false).unwrap()).unwrap();

        assert_eq!(rule["data"][0]["datasourceUid"], "loki");
        // The substring is the pinned datasource crate's own wording for a
        // discarded update; a crate upgrade that reworded it must fail here.
        assert_eq!(
            rule["data"][0]["model"]["expr"],
            "(sum(count_over_time({service_name=\"microscope-indexer\"} |~ \"Failed to send (account|transaction)\" [120s])) or on() vector(0))"
        );
        assert_eq!(rule["data"][2]["model"]["expression"], "$B > 0");
        assert_eq!(rule["labels"]["severity"], "critical");
        assert_eq!(rule["labels"]["signal_name"], "datasource_updates_dropped");
        assert_eq!(rule["labels"]["channel"], "slack");

        config.datasource.mode = DatasourceMode::Rpc;
        assert!(drop_rule(&provisioning_document(&config, &channels, true).unwrap()).is_some());
    }

    #[test]
    fn alerts_on_yellowstone_gaps_only_while_nothing_replays_them() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let gap = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .find(|rule| rule["uid"] == "ms-yellowstone-gap-slack")
            .unwrap();

        assert_eq!(gap["data"][0]["datasourceUid"], "prometheus");
        assert_eq!(
            gap["data"][0]["model"]["expr"],
            "max(increase(microscope_yellowstone_disconnects_total[600s]) and on() microscope_rpc_recovery_enabled == 0)"
        );
        assert_eq!(gap["data"][2]["model"]["expression"], "$B > 0");
        assert_eq!(gap["noDataState"], "OK");
        assert_eq!(gap["labels"]["severity"], "error");
        assert_eq!(gap["labels"]["channel"], "slack");
    }

    /// An idle vault and a vault whose state address or version is wrong both
    /// leave the multisig panels empty, so the unmatched counter is the only
    /// signal that separates them.
    #[test]
    fn alerts_when_squads_activity_matches_another_multisig() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let rule = health_rule(&document, "ms-multisig-unmatched-slack").unwrap();

        assert_eq!(
            rule["data"][0]["model"]["expr"],
            "max(increase(microscope_multisig_unmatched_state_total[3600s]))"
        );
        assert_eq!(rule["data"][2]["model"]["expression"], "$B > 0");
        assert_eq!(rule["labels"]["severity"], "error");
        assert_eq!(rule["labels"]["signal_name"], "multisig_unmatched_state");
    }

    /// Without a configured vault the counter can never move, so the rule would
    /// only ever be a permanently silent entry in the rule list.
    #[test]
    fn leaves_the_multisig_rule_out_without_a_configured_vault() {
        let mut config = config(vec![]);
        config.multisig = None;
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();

        assert!(health_rule(&document, "ms-multisig-unmatched-slack").is_none());
    }

    /// Polling reports its own staleness, so the Yellowstone rules would only
    /// alert on a stream this deployment does not have.
    #[test]
    fn leaves_the_yellowstone_rules_out_of_an_rpc_deployment() {
        let mut config = config(vec![]);
        config.datasource.mode = DatasourceMode::Rpc;
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, true).unwrap();

        assert!(health_rule(&document, "ms-yellowstone-gap-slack").is_none());
        assert!(health_rule(&document, "ms-yellowstone-stream-slack").is_none());
        assert!(health_rule(&document, "ms-yellowstone-unreachable-slack").is_none());
    }

    /// The disconnect counter only moves once an established stream goes
    /// silent, so an endpoint refusing every subscribe leaves it flat. The
    /// probe gauge has to alert on its own, and only once a whole window of
    /// dials has failed, or one dropped probe pages.
    #[test]
    fn alerts_when_every_probe_of_the_yellowstone_endpoint_fails() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let rule = health_rule(&document, "ms-yellowstone-unreachable-slack").unwrap();

        assert_eq!(rule["data"][0]["datasourceUid"], "prometheus");
        assert_eq!(
            rule["data"][0]["model"]["expr"],
            "max(max_over_time(microscope_yellowstone_probe_healthy[120s]))"
        );
        assert_eq!(rule["data"][2]["model"]["expression"], "$B < 1");
        assert_eq!(rule["noDataState"], "Alerting");
        assert_eq!(rule["labels"]["severity"], "error");
        assert_eq!(
            rule["labels"]["signal_name"],
            "yellowstone_endpoint_unreachable"
        );
        assert_eq!(rule["labels"]["channel"], "slack");
    }

    fn health_rule<'a>(
        document: &'a serde_json::Value,
        uid: &str,
    ) -> Option<&'a serde_json::Value> {
        document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .find(|rule| rule["uid"] == uid)
    }

    #[test]
    fn pends_health_rules_instead_of_notifying_on_a_single_failed_evaluation() {
        let config = config(vec![alert(AlertKind::Event, "transfer")]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let document = provisioning_document(&config, &channels, true).unwrap();
        let rule = |uid: &str| health_rule(&document, uid).expect("rule is generated");

        // A gauge holds while the fault lasts; a log window pends for half.
        assert_eq!(rule("ms-rpc-poll-stale-slack")["for"], "300s");
        assert_eq!(rule("ms-rpc-poll-stale-slack")["execErrState"], "Error");
        assert_eq!(rule("ms-yellowstone-stream-slack")["for"], "300s");
        assert_eq!(rule("ms-datasource-drop-slack")["for"], "60s");
        assert_eq!(rule("ms-datasource-drop-slack")["execErrState"], "Error");

        // Event rules match for one lookback window, so they cannot pend at all.
        let event = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .find(|rule| rule["labels"]["signal_name"] == "transfer")
            .expect("event rule is generated");
        assert_eq!(event["for"], "0s");
        assert_eq!(event["execErrState"], "OK");
    }

    #[test]
    fn scopes_log_greps_to_the_deployment_sharing_the_stack() {
        assert_eq!(
            log_stream_selector_for(Some("subscriptions")),
            "{service_name=\"microscope-indexer\", deployment=\"subscriptions\"}"
        );
        assert_eq!(
            log_stream_selector_for(Some("  ")),
            "{service_name=\"microscope-indexer\"}"
        );
        assert_eq!(
            log_stream_selector_for(None),
            "{service_name=\"microscope-indexer\"}"
        );
    }

    #[test]
    fn routes_yellowstone_stream_interruptions_to_a_contact_point() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let stream_rule = |document: &serde_json::Value| {
            document["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["rules"].as_array().unwrap())
                .find(|rule| rule["uid"] == "ms-yellowstone-stream-slack")
                .cloned()
        };

        assert_eq!(config.datasource.mode, DatasourceMode::Yellowstone);
        let yellowstone = provisioning_document(&config, &channels, false).unwrap();
        let rule = stream_rule(&yellowstone).unwrap();

        assert_eq!(rule["data"][0]["datasourceUid"], "loki");
        assert_eq!(
            rule["data"][0]["model"]["expr"],
            "(sum(count_over_time({service_name=\"microscope-indexer\"} |~ \"Stream timeout - no messages|Failed to subscribe|Geyser stream error|Stream closed\" [900s])) or on() vector(0))"
        );
        assert_eq!(rule["data"][2]["model"]["expression"], "$B > 5");
        assert_eq!(rule["data"][0]["relativeTimeRange"]["from"], 900);
        assert_eq!(rule["labels"]["channel"], "slack");
        assert_eq!(rule["labels"]["severity"], "error");
        assert_eq!(rule["labels"]["signal_kind"], "datasource");

        let mut rpc = config;
        rpc.datasource.mode = DatasourceMode::Rpc;
        assert!(stream_rule(&provisioning_document(&rpc, &channels, true).unwrap()).is_none());
    }

    #[test]
    fn alerts_when_decoded_records_stop_reaching_loki() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let rules = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .collect::<Vec<_>>();
        let stalled = rules
            .iter()
            .find(|rule| rule["uid"] == "ms-log-delivery-stalled-slack")
            .unwrap();
        assert_eq!(
            stalled["data"][0]["model"]["expr"],
            "sum(increase(microscope_transactions_total[10m])) > 0 unless sum(increase(loki_write_sent_entries_total[10m])) > 0"
        );
        assert_eq!(stalled["data"][2]["model"]["expression"], "$B > 0");
        assert_eq!(stalled["noDataState"], "OK");
        assert_eq!(stalled["labels"]["severity"], "error");
        assert_eq!(stalled["labels"]["channel"], "slack");
    }

    #[test]
    fn generates_rpc_health_alerts_for_configured_channels() {
        let mut config = config(vec![]);
        config.datasource.mode = DatasourceMode::Rpc;
        config.datasource.poll_interval_seconds = 20;
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, true).unwrap();
        let rules = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .collect::<Vec<_>>();
        let stale = rules
            .iter()
            .find(|rule| rule["uid"] == "ms-rpc-poll-stale-slack")
            .unwrap();
        let lag = rules
            .iter()
            .find(|rule| rule["uid"] == "ms-rpc-poll-lag-slack")
            .unwrap();
        let quarantine = rules
            .iter()
            .find(|rule| rule["uid"] == "ms-rpc-quarantine-slack")
            .unwrap();

        assert_eq!(stale["data"][0]["datasourceUid"], "prometheus");
        assert!(stale["data"][0]["model"]["expr"]
            .as_str()
            .unwrap()
            .contains("microscope_rpc_poll_started_unixtime"));
        assert_eq!(stale["data"][2]["model"]["expression"], "$B > 120");
        assert_eq!(stale["noDataState"], "Alerting");
        assert_eq!(stale["labels"]["channel"], "slack");
        assert_eq!(
            lag["data"][0]["model"]["expr"],
            "microscope_rpc_poll_lag_slots"
        );
        assert_eq!(lag["data"][2]["model"]["expression"], "$B > 600");
        assert_eq!(lag["labels"]["severity"], "warning");
        assert_eq!(
            quarantine["data"][0]["model"]["expr"],
            "microscope_rpc_poll_quarantined_transactions"
        );

        let checkpoint_stale = rules
            .iter()
            .find(|rule| rule["uid"] == "ms-rpc-checkpoint-stale-slack")
            .unwrap();
        assert!(checkpoint_stale["data"][0]["model"]["expr"]
            .as_str()
            .unwrap()
            .contains("microscope_rpc_checkpoint_last_success_unixtime"));
        assert_eq!(
            checkpoint_stale["data"][2]["model"]["expression"],
            "$B > 300"
        );
        assert_eq!(checkpoint_stale["noDataState"], "Alerting");

        let corrupt = rules
            .iter()
            .find(|rule| rule["uid"] == "ms-rpc-checkpoint-corrupt-slack")
            .unwrap();
        assert_eq!(
            corrupt["data"][0]["model"]["expr"],
            "microscope_rpc_checkpoint_quarantined_files"
        );
    }

    #[test]
    fn alerting_timings_follow_the_configured_overrides() {
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let mut config = config(vec![]);
        config.alerting.health_pending_period_seconds = 600;
        config.alerting.multisig_unmatched_window_seconds = 7200;
        let document = provisioning_document(&config, &channels, false).unwrap();

        let multisig = health_rule(&document, "ms-multisig-unmatched-slack").unwrap();
        assert_eq!(
            multisig["data"][0]["model"]["expr"],
            "max(increase(microscope_multisig_unmatched_state_total[7200s]))"
        );
        assert_eq!(multisig["for"], "600s");
        assert_eq!(
            health_rule(&document, "ms-datasource-drop-slack").unwrap()["for"],
            "60s",
            "a log-backed rule still caps the pending period at half its window"
        );
    }

    #[test]
    fn alerts_on_a_share_of_failing_polls_the_freshness_rules_cannot_see() {
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let failing_for = |poll_interval_seconds, sustained_failure_seconds| {
            let mut config = config(vec![]);
            config.datasource.mode = DatasourceMode::Rpc;
            config.datasource.poll_interval_seconds = poll_interval_seconds;
            config.alerting.rpc_poll_sustained_failure_seconds = sustained_failure_seconds;
            let document = provisioning_document(&config, &channels, true).unwrap();
            document["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["rules"].as_array().unwrap())
                .find(|rule| rule["uid"] == "ms-rpc-poll-failing-slack")
                .cloned()
                .expect("a failing poller is alertable")
        };
        let failing = |poll_interval_seconds| {
            failing_for(
                poll_interval_seconds,
                AlertingConfig::default().rpc_poll_sustained_failure_seconds,
            )
        };

        let default_interval = failing(5);
        assert_eq!(
            default_interval["data"][0]["model"]["expr"],
            "increase(microscope_rpc_poll_failures_total[900s])"
        );
        assert_eq!(
            default_interval["data"][2]["model"]["expression"], "$B > 9",
            "the polls forty-five seconds of unbroken failure costs at a five-second interval"
        );
        assert_eq!(default_interval["labels"]["severity"], "warning");
        assert_eq!(default_interval["noDataState"], "OK");
        assert_eq!(
            failing(300)["data"][2]["model"]["expression"],
            "$B > 1",
            "a poll interval longer than the sustained-failure duration still alerts"
        );
        assert_eq!(
            failing_for(5, 600)["data"][2]["model"]["expression"],
            "$B > 120",
            "a provider given ten minutes to recover pages only after ten minutes of failure"
        );
    }

    #[test]
    fn gates_rpc_health_alerts_on_polling_not_on_the_primary_mode() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let stale_uids = |document: &serde_json::Value| {
            document["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["rules"].as_array().unwrap())
                .filter(|rule| rule["uid"] == "ms-rpc-poll-stale-slack")
                .count()
        };

        let with_recovery = provisioning_document(&config, &channels, true).unwrap();
        let without_recovery = provisioning_document(&config, &channels, false).unwrap();

        assert_eq!(config.datasource.mode, DatasourceMode::Yellowstone);
        assert_eq!(stale_uids(&with_recovery), 1);
        assert_eq!(stale_uids(&without_recovery), 0);
    }

    #[test]
    fn generates_event_instruction_and_multisig_queries() {
        let config = config(vec![]);
        assert!(logql_query(&config, &alert(AlertKind::Event, "created"))
            .unwrap()
            .contains("kind = \"program_event\" | signal = \"created\" | scope = \"11111111111111111111111111111111\""));
        assert!(
            logql_query(&config, &alert(AlertKind::Instruction, "execute"))
                .unwrap()
                .contains("kind = \"program_instruction\" | signal = \"execute\"")
        );
        assert!(logql_query(&config, &alert(AlertKind::Multisig, "proposal_approved"))
            .unwrap()
            .contains("kind = \"multisig_activity\" | signal = \"proposal_approved\" | scope = \"11111111111111111111111111111111\""));
    }

    #[test]
    fn alerts_carry_the_matched_transaction_signature() {
        let config = config(vec![]);
        let query = logql_query(&config, &alert(AlertKind::Event, "created")).unwrap();
        assert!(query.contains(", signature=\"signature\""));
        assert!(query.contains("sum by (signature) (count_over_time("));

        let rule = grafana_rule(
            &config,
            &alert(AlertKind::Event, "created"),
            Some(AlertChannel::Slack),
        )
        .unwrap();
        assert_eq!(
            rule["annotations"]["transaction"],
            "{{ $labels.signature }} https://explorer.solana.com/tx/{{ $labels.signature }}"
        );
    }

    #[test]
    fn log_backed_alerts_link_to_the_matching_dashboard_panel() {
        let config = config(vec![]);
        let rule = |kind| {
            grafana_rule(&config, &alert(kind, "created"), Some(AlertChannel::Slack)).unwrap()
        };

        let event = rule(AlertKind::Event);
        assert_eq!(
            event["annotations"]["__dashboardUid__"],
            "microscope-overview"
        );
        assert_eq!(event["annotations"]["__panelId__"], "6");
        assert_eq!(rule(AlertKind::Multisig)["annotations"]["__panelId__"], "8");

        let instruction = rule(AlertKind::Instruction);
        assert!(instruction["annotations"].get("__panelId__").is_none());
        assert!(
            instruction["annotations"].get("__dashboardUid__").is_none(),
            "Grafana rejects a rule that names a dashboard without a panel"
        );
    }

    #[test]
    fn multisig_alerts_ignore_failed_squads_instructions() {
        let config = config(vec![]);

        let multisig =
            logql_query(&config, &alert(AlertKind::Multisig, "proposal_approved")).unwrap();
        assert!(multisig.contains(", failed=\"failed\""));
        assert!(multisig.contains("| failed = \"false\""));

        for kind in [AlertKind::Event, AlertKind::Instruction] {
            let query = logql_query(&config, &alert(kind, "created")).unwrap();
            assert!(!query.contains("failed"));
        }
    }

    #[test]
    fn generates_all_condition_filters() {
        let mut rule = alert(AlertKind::Event, "record_updated_event");
        rule.conditions = vec![
            AlertCondition {
                field: "data.amount".to_string(),
                operator: AlertConditionOperator::Gt,
                value: Some(AlertConditionValue::Integer(100)),
            },
            AlertCondition {
                field: "failed".to_string(),
                operator: AlertConditionOperator::Eq,
                value: Some(AlertConditionValue::Boolean(false)),
            },
        ];

        let query = logql_query(&config(vec![]), &rule).unwrap();
        assert!(query.contains("condition_0=\"data.amount\", condition_1=\"failed\""));
        assert!(query.contains("| (condition_0 > 100 and condition_1 = \"false\")"));
    }

    #[test]
    fn absent_fields_do_not_satisfy_a_not_equal_condition() {
        let string_ne = AlertCondition {
            field: "data.currency".to_string(),
            operator: AlertConditionOperator::Ne,
            value: Some(AlertConditionValue::String("USDC".to_string())),
        };
        let boolean_ne = AlertCondition {
            field: "data.recurring".to_string(),
            operator: AlertConditionOperator::Ne,
            value: Some(AlertConditionValue::Boolean(true)),
        };
        let numeric_ne = AlertCondition {
            field: "data.amount".to_string(),
            operator: AlertConditionOperator::Ne,
            value: Some(AlertConditionValue::Integer(0)),
        };

        let mut all = alert(AlertKind::Event, "created");
        all.conditions = vec![string_ne.clone(), boolean_ne.clone(), numeric_ne];
        let query = logql_query(&config(vec![]), &all).unwrap();
        assert!(query.contains("condition_0 != \"\" and condition_0 != \"USDC\""));
        assert!(query.contains("condition_1 != \"\" and condition_1 != \"true\""));
        assert!(query.contains("condition_2 != 0"));
        assert!(!query.contains("condition_2 != \"\""));

        let mut any = alert(AlertKind::Event, "created");
        any.match_mode = AlertMatch::Any;
        any.conditions = vec![string_ne, boolean_ne];
        let query = logql_query(&config(vec![]), &any).unwrap();
        assert!(query.contains("| (condition_0 != \"\" and condition_0 != \"USDC\")"));
        assert!(query.contains("| (condition_1 != \"\" and condition_1 != \"true\")"));
    }

    #[test]
    fn generates_any_condition_filters() {
        let mut rule = alert(AlertKind::Event, "record_updated_event");
        rule.match_mode = AlertMatch::Any;
        rule.conditions = vec![
            AlertCondition {
                field: "data.memo".to_string(),
                operator: AlertConditionOperator::Contains,
                value: Some(AlertConditionValue::String("urgent.*".to_string())),
            },
            AlertCondition {
                field: "data.authority".to_string(),
                operator: AlertConditionOperator::Exists,
                value: None,
            },
        ];

        let query = logql_query(&config(vec![]), &rule).unwrap();
        assert!(query.contains("| (condition_0 =~ \".*urgent\\\\.\\\\*.*\")"));
        assert!(query.contains("| (condition_1 != \"\")"));
        assert_eq!(
            query.matches("sum by (signature) (count_over_time").count(),
            2
        );
        assert!(query.contains(")) or sum by (signature) (count_over_time("));
    }

    #[test]
    fn describes_exists_conditions_the_way_the_query_evaluates_them() {
        let mut rule = alert(AlertKind::Event, "created");
        rule.conditions = vec![AlertCondition {
            field: "data.memo".to_string(),
            operator: AlertConditionOperator::Exists,
            value: None,
        }];

        let document =
            provisioning_document(&config(vec![rule]), &Default::default(), false).unwrap();
        let description = document["groups"][0]["rules"][0]["annotations"]["description"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(description.contains("data.memo is present and non-empty"));
    }

    #[test]
    fn applies_per_rule_timing_to_queries_and_groups() {
        let default_rule = alert(AlertKind::Event, "created");
        let mut slower_rule = alert(AlertKind::Instruction, "execute");
        slower_rule.lookback_window_seconds = Some(300);
        slower_rule.evaluation_interval_seconds = Some(30);
        let mut config = config(vec![default_rule, slower_rule]);
        config.alerting.lookback_window_seconds = 120;
        config.alerting.evaluation_interval_seconds = 20;

        let document = provisioning_document(&config, &Default::default(), false).unwrap();
        assert_eq!(document["groups"][0]["interval"], "20s");
        assert_eq!(
            document["groups"][0]["rules"][0]["data"][0]["relativeTimeRange"]["from"],
            120
        );
        assert!(
            document["groups"][0]["rules"][0]["data"][0]["model"]["expr"]
                .as_str()
                .unwrap()
                .contains("[120s]")
        );
        assert_eq!(document["groups"][1]["interval"], "30s");
        let slower = document["groups"][1]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["labels"]["signal_kind"] == "instruction")
            .unwrap();
        assert_eq!(slower["data"][0]["relativeTimeRange"]["from"], 300);
        assert!(slower["data"][0]["model"]["expr"]
            .as_str()
            .unwrap()
            .contains("[300s]"));
    }

    #[test]
    fn generates_a_noop_policy_for_rules_without_channels() {
        let document = provisioning_document(
            &config(vec![alert(AlertKind::Event, "created")]),
            &Default::default(),
            false,
        )
        .unwrap();

        assert_eq!(document["groups"][0]["rules"].as_array().unwrap().len(), 1);
        assert_eq!(
            document["groups"][0]["rules"][0]["labels"]["channel"],
            "none"
        );
        assert_eq!(document["policies"][0]["receiver"], "microscope-noop");
        assert_eq!(
            document["policies"][0]["routes"][0]["mute_time_intervals"][0],
            "microscope-always"
        );
        assert_eq!(
            document["contactPoints"][0]["receivers"][0]["type"],
            "webhook"
        );
        assert!(document["groups"][0]["rules"][0]
            .get("notification_settings")
            .is_none());
    }

    /// A shared Grafana stack rejects writes to the org's policy tree, so a rule
    /// that only carries `channel` falls through to whatever the org routes by
    /// default and never reaches the deployment's contact point.
    #[test]
    fn names_the_contact_point_on_every_rule_that_has_a_channel() {
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let document = provisioning_document(
            &config(vec![alert_with_channel(
                AlertKind::Event,
                "created",
                AlertChannel::Slack,
            )]),
            &channels,
            true,
        )
        .unwrap();

        let rules = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
            .collect::<Vec<_>>();
        assert!(rules.len() > 1);

        for rule in rules {
            assert_eq!(
                rule["notification_settings"]["receiver"], "microscope-slack",
                "{}",
                rule["title"]
            );
        }
    }

    #[test]
    fn deletes_rules_removed_from_the_previous_document() {
        let previous = provisioning_document(
            &config(vec![alert(AlertKind::Multisig, "proposal_approved")]),
            &Default::default(),
            false,
        )
        .unwrap();
        let previous_uids = rule_uids(&previous);

        let channels = [AlertChannel::Slack].into_iter().collect();
        let mut current = provisioning_document(
            &config(vec![alert_with_channel(
                AlertKind::Multisig,
                "proposal_approved",
                AlertChannel::Slack,
            )]),
            &channels,
            false,
        )
        .unwrap();

        add_resource_deletions(&mut current, &previous);

        assert_eq!(
            deletion_uids(&current, "deleteRules"),
            previous_uids
                .difference(&rule_uids(&current))
                .cloned()
                .collect()
        );
        assert!(current.get("deleteContactPoints").is_none());

        let mut regenerated = provisioning_document(
            &config(vec![alert_with_channel(
                AlertKind::Multisig,
                "proposal_approved",
                AlertChannel::Slack,
            )]),
            &channels,
            false,
        )
        .unwrap();
        add_resource_deletions(&mut regenerated, &current);
        assert_eq!(regenerated["deleteRules"], current["deleteRules"]);
    }

    #[test]
    fn deletes_contact_points_removed_from_the_previous_document() {
        let channels = [AlertChannel::Slack].into_iter().collect();
        let previous = provisioning_document(
            &config(vec![alert_with_channel(
                AlertKind::Event,
                "created",
                AlertChannel::Slack,
            )]),
            &channels,
            false,
        )
        .unwrap();
        let previous_rule_uids = rule_uids(&previous);
        let mut current =
            provisioning_document(&config(vec![]), &Default::default(), false).unwrap();

        add_resource_deletions(&mut current, &previous);

        assert_eq!(
            current["deleteContactPoints"],
            serde_json::json!([{ "orgId": 1, "uid": "microscope-slack" }])
        );
        assert_eq!(
            deletion_uids(&current, "deleteRules"),
            previous_rule_uids
                .difference(&rule_uids(&current))
                .cloned()
                .collect()
        );

        let mut regenerated =
            provisioning_document(&config(vec![]), &Default::default(), false).unwrap();
        add_resource_deletions(&mut regenerated, &current);

        assert_eq!(
            regenerated["deleteContactPoints"],
            current["deleteContactPoints"]
        );
        assert_eq!(regenerated["deleteRules"], current["deleteRules"]);
    }
}
