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
        label_name, AlertChannel, AlertCondition, AlertConditionOperator, AlertConditionValue,
        AlertKind, AlertMatch, AlertRule, Config, DatasourceMode, SlackMention,
    },
    dashboard, files,
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
const HEALTH_GROUP_INTERVAL: &str = "1m";
const HEALTH_REPEAT_INTERVAL: &str = "4h";

pub fn generate(config: &Config, output_dir: &Path) -> anyhow::Result<PathBuf> {
    let rpc_polling = config.datasource.polls_rpc();
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

    files::replace(
        &output,
        &serde_json::to_vec_pretty(&document)?,
        "alerting config",
    )?;

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
    let health_rules = rules_by_interval
        .entry(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS)
        .or_default();
    health_rules.extend(rpc_recovery_degraded_rules(channels, pending_seconds));
    health_rules.extend(datasource_drop_rules(channels, pending_seconds));
    if config.datasource.mode == DatasourceMode::Yellowstone {
        health_rules.extend(yellowstone_health_rules(channels, pending_seconds));
    }
    health_rules.extend(log_delivery_rules(channels, pending_seconds));
    if !config.multisigs.is_empty() {
        health_rules.extend(multisig_health_rules(
            channels,
            config.alerting.multisig_unmatched_window_seconds,
            pending_seconds,
        ));
    }
    if rpc_polling {
        health_rules.extend(rpc_health_rules(config, channels, pending_seconds));
    }
    if config.alerting.own_policy_tree {
        // The route Grafana derives from a rule's receiver always groups by
        // alert name, so the tree has to route these.
        for rule in health_rules.iter_mut() {
            let grouped = rule["labels"]["signal_kind"] == "datasource"
                && grouped_health_channels(channels)
                    .any(|channel| rule["labels"]["channel"] == channel.as_str());
            if let Some(rule) = rule.as_object_mut().filter(|_| grouped) {
                rule.remove("notification_settings");
            }
        }
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
        let health_mention = match channel {
            AlertChannel::Slack => config.alerting.health_slack_mention.as_ref(),
            _ => None,
        };
        contact_points.push(contact_point(
            config,
            *channel,
            receiver(*channel, false),
            false,
            health_mention,
        ));
        if activity_skips_resolve(*channel) {
            contact_points.push(contact_point(
                config,
                *channel,
                receiver(*channel, true),
                true,
                None,
            ));
        }
    }
    for mention in slack_mentions(config) {
        contact_points.push(contact_point(
            config,
            AlertChannel::Slack,
            slack_mention_receiver(mention),
            true,
            Some(mention),
        ));
    }

    let mut routes = vec![json!({
        "receiver": "microscope-noop",
        "object_matchers": [["channel", "=", "none"]],
        "mute_time_intervals": ["microscope-always"],
        "continue": false,
    })];
    if config.alerting.own_policy_tree {
        routes.extend(grouped_health_channels(channels).map(health_route));
    }

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
/// deployment that shares a stack cannot add channel routes to it. Naming the
/// receiver on the rule reaches the contact point through a route Grafana
/// derives per rule and places ahead of the tree. `channel = none` keeps routing
/// through the tree, where the always-on mute timing lives.
fn notification_settings(channel: &str) -> Option<Value> {
    (channel != "none").then(|| json!({ "receiver": format!("microscope-{channel}") }))
}

const fn activity_skips_resolve(channel: AlertChannel) -> bool {
    match channel {
        AlertChannel::Slack | AlertChannel::Telegram => true,
        AlertChannel::Pagerduty => false,
    }
}

/// A group of mixed severities reaches PagerDuty as one critical incident.
const fn groups_health_alerts(channel: AlertChannel) -> bool {
    match channel {
        AlertChannel::Slack | AlertChannel::Telegram => true,
        AlertChannel::Pagerduty => false,
    }
}

fn grouped_health_channels(
    channels: &BTreeSet<AlertChannel>,
) -> impl Iterator<Item = AlertChannel> + '_ {
    channels
        .iter()
        .copied()
        .filter(|channel| groups_health_alerts(*channel))
}

fn health_route(channel: AlertChannel) -> Value {
    json!({
        "receiver": receiver(channel, false),
        "object_matchers": [
            ["channel", "=", channel.as_str()],
            ["signal_kind", "=", "datasource"],
        ],
        // Only the Grafana Cloud export adds a deployment label. Alertmanager
        // leaves a label the alert lacks out of the group.
        "group_by": ["deployment", "signal_kind"],
        // Set on the route, or a shared tree's root would decide them.
        "group_wait": format!("{RPC_HEALTH_EVALUATION_INTERVAL_SECONDS}s"),
        "group_interval": HEALTH_GROUP_INTERVAL,
        "repeat_interval": HEALTH_REPEAT_INTERVAL,
        "continue": false,
    })
}

fn receiver(channel: AlertChannel, activity: bool) -> String {
    if activity && activity_skips_resolve(channel) {
        format!("microscope-{channel}-activity")
    } else {
        format!("microscope-{channel}")
    }
}

/// A Slack contact point tags the same people on every message, so each
/// distinct mention needs a contact point of its own.
fn slack_mention_receiver(mention: &SlackMention) -> String {
    format!("microscope-slack-{}", mention.name())
}

fn activity_receiver(alert: &AlertRule, channel: AlertChannel) -> String {
    match (channel, &alert.slack_mention) {
        (AlertChannel::Slack, Some(mention)) => slack_mention_receiver(mention),
        _ => receiver(channel, true),
    }
}

fn slack_mentions(config: &Config) -> BTreeSet<&SlackMention> {
    config
        .alert_rules
        .iter()
        .filter(|alert| alert.channels.contains(&AlertChannel::Slack))
        .filter_map(|alert| alert.slack_mention.as_ref())
        .collect()
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
                    &format!("(sum(increase(microscope_yellowstone_disconnects_total[{YELLOWSTONE_GAP_WINDOW_SECONDS}s])) + sum(increase(microscope_yellowstone_failovers_total[{YELLOWSTONE_GAP_WINDOW_SECONDS}s]))) and on() microscope_rpc_recovery_enabled == 0"),
                    "$B > 0",
                    "OK",
                    "error",
                    "yellowstone_gap_unrecovered",
                    channel,
                    "The Yellowstone stream reconnected at the provider's current head after a disconnect, or switched to another endpoint, while RPC recovery is not replaying the gap, either because RPC_URL is unset or because recovery disabled itself (see the RPC recovery disabled alert). The transactions in the gap will never be indexed. microscope_yellowstone_missed_slots_total gives the size of a reconnect gap; a switch leaves a gap from the old endpoint's last message to the new subscription, which no counter sizes. Backfill this interval.",
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
                    "Every probe of every configured Yellowstone endpoint failed, so the datasource cannot establish a stream anywhere. The disconnect and missed-slot counters stay flat throughout, because they only move once an established stream goes silent, so this is the only metric that shows a provider refusing connections. No data also alerts here: the probe gauge is seeded at startup, so its absence means the indexer is gone.",
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-yellowstone-degraded-{channel}"),
                    &format!("Yellowstone endpoint failing its probe [{channel}]"),
                    &format!("max(max_over_time(microscope_yellowstone_probe_healthy[{YELLOWSTONE_PROBE_WINDOW_SECONDS}s])) - min(max_over_time(microscope_yellowstone_probe_healthy[{YELLOWSTONE_PROBE_WINDOW_SECONDS}s]))"),
                    "$B > 0",
                    "OK",
                    "warning",
                    "yellowstone_endpoint_degraded",
                    channel,
                    "One configured Yellowstone endpoint fails every probe while another passes; the endpoint label on microscope_yellowstone_probe_healthy at 0 names it. Indexing continues on a healthy endpoint, but the failover this endpoint exists for has nowhere to go. A fallback that fails from the start usually has a wrong URL or token in GEYSER_FALLBACK_URLS or GEYSER_FALLBACK_X_TOKENS.",
                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-yellowstone-fallback-{channel}"),
                    &format!("Yellowstone running on a fallback endpoint [{channel}]"),
                    "max(microscope_yellowstone_active_endpoint{role=\"fallback\"}) or on() vector(0)",
                    "$B > 0",
                    "OK",
                    "warning",
                    "yellowstone_fallback_active",
                    channel,
                    "The primary Yellowstone endpoint failed its probe for a minute, so the stream moved to a fallback from GEYSER_FALLBACK_URLS. Indexing continues; the switch left a gap that RPC recovery replays when RPC_URL is configured. The microscope_yellowstone_active_endpoint series at 1 names the endpoint in use, and the stream returns to the primary once its probe has been healthy for five minutes, or longer each time the primary fails again right after a return. Check the primary provider's status.",
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
                "Squads instructions decoded but referenced an internal state account that is not configured, so no multisig activity was recorded for them. A configured multisig.state_address or multisig.version is wrong. Without this alert a misconfigured vault is indistinguishable from an idle one: both leave the multisig panels empty.",
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
    let sustained_failure_seconds = config.alerting.rpc_poll_sustained_failure_seconds;
    let poll_interval_seconds = config.datasource.poll_interval_seconds.max(1);
    // Rounding up, and comparing with `>=`, so the rule fires on the poll that
    // completes the configured duration rather than the one after it. A
    // duration below the interval still costs a whole poll, which is the
    // soonest a failure can be observed at all.
    let failure_threshold = sustained_failure_seconds
        .div_ceil(poll_interval_seconds)
        .max(1);
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
                    &format!("$B >= {failure_threshold}"),
                    "OK",
                    "warning",
                    "rpc_poll_failing",
                    channel,
                    &format!(
                        "At least {failure_threshold} RPC polls failed in the last {RPC_POLL_FAILURE_WINDOW_SECONDS} seconds, the count an unbroken outage produces in {sustained_failure_seconds} seconds. Enough polls are still succeeding to keep the freshness and lag rules green, so gap recovery is partially dead rather than stopped; read the poll failure logs for the RPC error."
                    ),
                                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-degraded-{channel}"),
                    &format!("RPC endpoint unusable [{channel}]"),
                    &format!("max(max_over_time(microscope_rpc_endpoint_healthy[{YELLOWSTONE_PROBE_WINDOW_SECONDS}s])) - min(max_over_time(microscope_rpc_endpoint_healthy[{YELLOWSTONE_PROBE_WINDOW_SECONDS}s]))"),
                    "$B > 0",
                    "OK",
                    "warning",
                    "rpc_endpoint_degraded",
                    channel,
                    "One configured RPC endpoint is unusable while another works; the endpoint label on microscope_rpc_endpoint_healthy at 0 names it. It fails its polls or probes, serves another cluster, or was given up on because it cannot serve the checkpoint (the indexer log says which). Polling continues on a working endpoint, but the failover this endpoint exists for has nowhere to go.",
                    pending_seconds,
                ),
                prometheus_health_rule(
                    &format!("ms-rpc-fallback-{channel}"),
                    &format!("RPC polling on a fallback endpoint [{channel}]"),
                    "max(microscope_rpc_active_endpoint{role=\"fallback\"}) or on() vector(0)",
                    "$B > 0",
                    "OK",
                    "warning",
                    "rpc_fallback_active",
                    channel,
                    "RPC_URL kept failing polls for a minute, or cannot serve the checkpoint, so polling moved to an endpoint from RPC_FALLBACK_URLS. Nothing is lost: the cursors carry over. The microscope_rpc_active_endpoint series at 1 names the endpoint in use, and polling returns to RPC_URL once it has answered probes for five minutes, or longer each time it fails again right after a return; an endpoint given up on for its history is not returned to until the indexer restarts. Check the primary provider's status.",
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
        loki_query(query, window_seconds),
        condition,
        pending_seconds.min(window_seconds / 2),
        "OK",
        severity,
        signal_name,
        channel,
        description,
    )
}

fn loki_query(query: &str, window_seconds: u64) -> Value {
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
    })
}

fn rule_data(query: Value, condition: &str) -> Value {
    json!([
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
    ])
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
        "data": rule_data(query, condition),
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
    alert_channel: Option<AlertChannel>,
) -> anyhow::Result<Value> {
    let channel = alert_channel.map(AlertChannel::as_str).unwrap_or("none");
    let query = logql_query(config, alert)?;
    let lookback_window_seconds = alert.lookback_window_seconds(&config.alerting);
    let conditions = serde_json::to_string(&alert.conditions)?;
    let program_ids = config.alert_program_ids(alert);
    let names_matched_program = names_matched_program(alert, &program_ids);
    let program_ids = program_ids.join(",");
    let canonical = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}",
        program_ids,
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
        "transaction_url": transaction_url,
    });
    // Grafana rejects a rule carrying one of these two without the other.
    if alert.kind == AlertKind::Multisig {
        annotations["vault_address"] = json!("{{ $labels.scope }}");
    }
    if names_matched_program {
        annotations["program_id"] = json!("{{ $labels.scope }}");
    }
    if !alert.label_fields.is_empty() {
        annotations["fields"] = json!(fields_annotation(&alert.label_fields));
    }
    if let Some(panel_id) = log_panel_id(alert.kind) {
        annotations["__dashboardUid__"] = json!(dashboard::OVERVIEW_UID);
        annotations["__panelId__"] = json!(panel_id.to_string());
    }

    let mut rule = json!({
        "uid": uid,
        "title": title,
        "condition": "C",
        "data": rule_data(loki_query(&query, lookback_window_seconds), "$B > 0"),
        "noDataState": "OK",
        // A match counts for one lookback window, so any pending period long
        // enough to absorb a failed evaluation would mute these for good.
        // Without one, the error itself must not notify.
        "execErrState": "OK",
        "for": "0s",
        "annotations": annotations,
        "labels": {
            "channel": channel,
            "program_id": program_ids,
            "severity": alert.severity.as_str(),
            "signal_kind": alert.kind.as_str(),
            "signal_name": alert.name,
        },
        "isPaused": false,
    });
    if let Some(channel) = alert_channel {
        rule["notification_settings"] = json!({ "receiver": activity_receiver(alert, channel) });
    }

    Ok(rule)
}

fn fields_annotation(fields: &[String]) -> String {
    fields
        .iter()
        .map(|field| format!("{field}={{{{ $labels.{} }}}}", label_name(field)))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A rule covering one program has nothing to name, and grouping by scope
/// would only change its query.
fn names_matched_program(alert: &AlertRule, program_ids: &[&str]) -> bool {
    alert.kind != AlertKind::Multisig && program_ids.len() > 1
}

fn log_panel_id(kind: AlertKind) -> Option<u64> {
    match kind {
        AlertKind::Event => Some(dashboard::EVENT_PANEL_ID),
        AlertKind::Multisig => Some(dashboard::MULTISIG_PANEL_ID),
        AlertKind::Instruction => None,
    }
}

fn logql_query(config: &Config, alert: &AlertRule) -> anyhow::Result<String> {
    let (record_kind, signal_path, scope_path, scope_values) = match alert.kind {
        AlertKind::Event => (
            "program_event",
            "name",
            "program_id",
            config.alert_program_ids(alert),
        ),
        AlertKind::Instruction => (
            "program_instruction",
            "name",
            "program_id",
            config.alert_program_ids(alert),
        ),
        AlertKind::Multisig => {
            if config.multisigs.is_empty() {
                bail!(
                    "multisig alert {} requires a [multisig] section in the config",
                    alert.name
                );
            }
            (
                "multisig_activity",
                "action",
                "vault_address",
                config
                    .multisigs
                    .iter()
                    .map(|multisig| multisig.vault_address.as_str())
                    .collect(),
            )
        }
    };
    let quoted_name = serde_json::to_string(&alert.name)?;
    let scope_filter = dashboard::scope_filter(&scope_values);

    let mut grouping = if alert.kind == AlertKind::Multisig
        || names_matched_program(alert, &config.alert_program_ids(alert))
    {
        "signature, scope".to_string()
    } else {
        "signature".to_string()
    };
    let mut parser = format!(
        "| json kind=\"kind\", signal=\"{signal_path}\", scope=\"{scope_path}\", signature=\"signature\""
    );
    let mut filters =
        format!("| kind = \"{record_kind}\" | signal = {quoted_name} | scope {scope_filter}");
    if alert.kind == AlertKind::Multisig {
        parser.push_str(", failed=\"failed\"");
        filters.push_str(" | failed = \"false\"");
    }
    for field in &alert.label_fields {
        let label = label_name(field);
        parser.push_str(&format!(", {label}=\"{field}\""));
        grouping.push_str(&format!(", {label}"));
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
            "sum by ({grouping}) (count_over_time({{service_name=\"microscope-indexer\"}} {parser} {filters} | __error__ = \"\" [{lookback_window_seconds}s]))"
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

const TELEGRAM_SINGLE_DESCRIPTION_LIMIT: usize = 3000;

const TELEGRAM_GROUPED_ACTIVITY_DESCRIPTION_LIMIT: usize = 100;

/// Health alerts print one link and no transaction or matched line, so five fit in 4096.
const TELEGRAM_GROUPED_HEALTH_DESCRIPTION_LIMIT: usize = 350;

/// An alert's first line, and all that a health alert prints when more than five fire.
const TELEGRAM_ALERT_TITLE: &str = r#"{{ if eq .Labels.severity "critical" }}🚨{{ else if eq .Labels.severity "error" }}🔴{{ else if eq .Labels.severity "warning" }}🟠{{ else }}🔵{{ end }} <b>{{ if eq .Labels.alertname "DatasourceError" }}DatasourceError: {{ end }}{{ or .Labels.signal_name .Labels.alertname | html | printf "%.64s" }}</b>{{ with .Labels.severity }} · {{ . | html | printf "%.16s" }}{{ end }}"#;

const TELEGRAM_SINGLE_FIELDS_LIMIT: usize = 300;

const TELEGRAM_ALERT_DETAILS: &str = r#"{description}
{fields}{matched}{{ if .Annotations.transaction_url }}<a href="{{ .Annotations.transaction_url | html }}">Transaction</a> · {{ end }}{{ if .PanelURL }}<a href="{{ .PanelURL | html }}">Dashboard</a>{{ else if .DashboardURL }}<a href="{{ .DashboardURL | html }}">Dashboard</a>{{ else if .GeneratorURL }}<a href="{{ .GeneratorURL | html }}">Rule</a>{{ end }}

"#;

const TELEGRAM_RESOLVED: &str =
    "✅ resolved: {{ or .Labels.signal_name .Labels.alertname | html | printf \"%.64s\" }}\n";

fn telegram_matched_line(config: &Config) -> String {
    let named = [
        ("Program", "program_id", config.programs.len() > 1),
        ("Vault", "vault_address", config.multisigs.len() > 1),
    ];
    let mut line = String::new();
    for (label, annotation, _) in named.into_iter().filter(|(_, _, several)| *several) {
        let keyword = if line.is_empty() { "if" } else { "else if" };
        line += &format!(
            "{{{{ {keyword} .Annotations.{annotation} }}}}{label} {{{{ .Annotations.{annotation} | html | printf \"%.44s\" }}}}\n"
        );
    }
    if !line.is_empty() {
        line += "{{ end }}";
    }
    line
}

#[derive(Clone, Copy)]
enum TelegramFields {
    Hidden,
    OwnLine,
    InPlaceOfDescription,
}

fn telegram_alert(description_limit: usize, matched_line: &str, fields: TelegramFields) -> String {
    let description = format!(
        "{{{{ or .Annotations.Error .Annotations.description .Annotations.summary | html | printf \"%.{description_limit}s\" }}}}"
    );
    // Telegram turns text such as `data.group` into a link, but adds no link inside `code`.
    let (description, fields_line) = match fields {
        TelegramFields::Hidden => (description, String::new()),
        TelegramFields::OwnLine => (
            description,
            format!(
                "{{{{ with .Annotations.fields }}}}<code>{{{{ . | html | printf \"%.{TELEGRAM_SINGLE_FIELDS_LIMIT}s\" }}}}</code>\n{{{{ end }}}}"
            ),
        ),
        TelegramFields::InPlaceOfDescription => (
            format!(
                "{{{{ if .Annotations.fields }}}}<code>{{{{ .Annotations.fields | html | printf \"%.{description_limit}s\" }}}}</code>{{{{ else }}}}{description}{{{{ end }}}}"
            ),
            String::new(),
        ),
    };
    let details = TELEGRAM_ALERT_DETAILS
        .replace("{description}", &description)
        .replace("{fields}", &fields_line)
        .replace("{matched}", matched_line);
    format!("{TELEGRAM_ALERT_TITLE}\n{details}")
}

fn telegram_shows_fields(config: &Config, activity: bool) -> bool {
    activity
        && config.alert_rules.iter().any(|alert| {
            !alert.label_fields.is_empty() && alert.channels.contains(&AlertChannel::Telegram)
        })
}

fn telegram_message(config: &Config, activity: bool) -> String {
    let matched_line = telegram_matched_line(config);
    let shows_fields = telegram_shows_fields(config, activity);
    let single = telegram_alert(
        TELEGRAM_SINGLE_DESCRIPTION_LIMIT,
        &matched_line,
        if shows_fields {
            TelegramFields::OwnLine
        } else {
            TelegramFields::Hidden
        },
    );
    let grouped = telegram_alert(
        if activity {
            TELEGRAM_GROUPED_ACTIVITY_DESCRIPTION_LIMIT
        } else {
            TELEGRAM_GROUPED_HEALTH_DESCRIPTION_LIMIT
        },
        &matched_line,
        if shows_fields {
            TelegramFields::InPlaceOfDescription
        } else {
            TelegramFields::Hidden
        },
    );
    // A health alert is never hidden: above five, each one gets its title line.
    let above_five = if activity {
        [
            "{{ range slice .Alerts.Firing 0 5 }}",
            &grouped,
            "{{ end }}… {{ len .Alerts.Firing }} alerts firing, first 5 shown\n",
        ]
        .concat()
    } else {
        [
            "{{ range .Alerts.Firing }}",
            TELEGRAM_ALERT_TITLE,
            "\n{{ end }}",
        ]
        .concat()
    };
    [
        "{{ if eq (len .Alerts.Firing) 1 }}{{ range .Alerts.Firing }}",
        &single,
        "{{ end }}",
        "{{ else if gt (len .Alerts.Firing) 5 }}",
        &above_five,
        "{{ else }}{{ range .Alerts.Firing }}",
        &grouped,
        "{{ end }}{{ end }}",
        "{{ if gt (len .Alerts.Resolved) 5 }}{{ range slice .Alerts.Resolved 0 5 }}",
        TELEGRAM_RESOLVED,
        "{{ end }}… {{ len .Alerts.Resolved }} alerts resolved, first 5 shown\n",
        "{{ else }}{{ range .Alerts.Resolved }}",
        TELEGRAM_RESOLVED,
        "{{ end }}{{ end }}",
    ]
    .concat()
}

fn contact_point(
    config: &Config,
    channel: AlertChannel,
    name: String,
    activity: bool,
    slack_mention: Option<&SlackMention>,
) -> Value {
    let settings = match channel {
        AlertChannel::Slack => {
            let mut settings = json!({ "url": "$SLACK_WEBHOOK_URL" });
            match slack_mention {
                Some(SlackMention::Here) => settings["mentionChannel"] = json!("here"),
                Some(SlackMention::Channel) => settings["mentionChannel"] = json!("channel"),
                Some(SlackMention::User(id)) => settings["mentionUsers"] = json!(id),
                None => {}
            }
            settings
        }
        AlertChannel::Telegram => json!({
            "bottoken": "$TELEGRAM_BOT_TOKEN",
            "chatid": "$TELEGRAM_CHAT_ID",
            "message": telegram_message(config, activity),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        }),
        AlertChannel::Pagerduty => json!({
            "integrationKey": "$PAGERDUTY_INTEGRATION_KEY",
            "severity": "{{ .CommonLabels.severity }}",
        }),
    };

    json!({
        "orgId": 1,
        "name": name,
        "receivers": [{
            "uid": name,
            "type": channel.as_str(),
            "settings": settings,
            "disableResolveMessage": activity && activity_skips_resolve(channel),
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
    use std::collections::{BTreeMap, BTreeSet};

    use crate::config::{
        AlertChannel, AlertCondition, AlertConditionOperator, AlertConditionValue, AlertKind,
        AlertMatch, AlertRule, AlertSeverity, AlertingConfig, Config, DatasourceMode,
        ProgramConfig, SlackMention,
    };

    use super::{
        add_resource_deletions, contact_point_uids, deletion_uids, grafana_rule,
        log_stream_selector_for, logql_query, provisioning_document, rule_uids, telegram_message,
        RPC_HEALTH_EVALUATION_INTERVAL_SECONDS, TELEGRAM_ALERT_TITLE,
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
            program_id: None,
            slack_mention: None,
            label_fields: vec![],
        }
    }

    fn alert_with_channel(kind: AlertKind, name: &str, channel: AlertChannel) -> AlertRule {
        let mut alert = alert(kind, name);
        alert.channels = vec![channel];
        alert
    }

    fn config(alert_rules: Vec<AlertRule>) -> Config {
        let mut config = crate::test_support::config();
        config.alert_rules = alert_rules;
        config
    }

    #[test]
    fn generates_recovery_degradation_alerts_in_yellowstone_mode() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let degraded = health_rule(&document, "ms-rpc-recovery-degraded-slack").unwrap();

        assert_eq!(
            degraded["data"][0]["model"]["expr"],
            "max(microscope_rpc_recovery_degraded)"
        );
        assert_eq!(degraded["noDataState"], "OK");
        assert_eq!(degraded["labels"]["severity"], "critical");
        assert!(health_rule(&document, "ms-rpc-checkpoint-corrupt-slack").is_some());
    }

    #[test]
    fn routes_dropped_datasource_updates_to_a_contact_point_in_every_mode() {
        let mut config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let drop_rule = |document: &serde_json::Value| {
            health_rule(document, "ms-datasource-drop-slack").cloned()
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
        let gap = health_rule(&document, "ms-yellowstone-gap-slack").unwrap();

        assert_eq!(gap["data"][0]["datasourceUid"], "prometheus");
        assert_eq!(
            gap["data"][0]["model"]["expr"],
            "(sum(increase(microscope_yellowstone_disconnects_total[600s])) + sum(increase(microscope_yellowstone_failovers_total[600s]))) and on() microscope_rpc_recovery_enabled == 0"
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
        config.multisigs.clear();
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
        assert!(health_rule(&document, "ms-yellowstone-fallback-slack").is_none());
    }

    /// The primary is the provider the deployment chose and pays for, so
    /// streaming from a fallback is worth a warning even while nothing is lost.
    /// Without fallbacks the role series never exists, which must read as
    /// healthy rather than as missing data.
    #[test]
    fn warns_while_rpc_polling_runs_on_a_fallback_endpoint() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let without_rpc = provisioning_document(&config, &channels, false).unwrap();
        assert!(health_rule(&without_rpc, "ms-rpc-fallback-slack").is_none());

        let document = provisioning_document(&config, &channels, true).unwrap();
        let rule = health_rule(&document, "ms-rpc-fallback-slack").unwrap();
        assert_eq!(
            rule["data"][0]["model"]["expr"],
            "max(microscope_rpc_active_endpoint{role=\"fallback\"}) or on() vector(0)"
        );
        assert_eq!(rule["data"][2]["model"]["expression"], "$B > 0");
        assert_eq!(rule["noDataState"], "OK");
        assert_eq!(rule["labels"]["severity"], "warning");
        assert_eq!(rule["labels"]["signal_name"], "rpc_fallback_active");
    }

    /// The unreachable alert takes the best endpoint, so a fallback with a
    /// wrong token stays invisible until the primary dies and the failover
    /// finds nowhere to go. All endpoints failing is the unreachable alert's.
    #[test]
    fn warns_when_one_endpoint_fails_while_another_works() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let document = provisioning_document(&config, &channels, true).unwrap();

        for (uid, metric, signal) in [
            (
                "ms-yellowstone-degraded-slack",
                "microscope_yellowstone_probe_healthy",
                "yellowstone_endpoint_degraded",
            ),
            (
                "ms-rpc-degraded-slack",
                "microscope_rpc_endpoint_healthy",
                "rpc_endpoint_degraded",
            ),
        ] {
            let rule = health_rule(&document, uid).unwrap();
            assert_eq!(
                rule["data"][0]["model"]["expr"],
                format!("max(max_over_time({metric}[120s])) - min(max_over_time({metric}[120s]))")
            );
            assert_eq!(rule["data"][2]["model"]["expression"], "$B > 0");
            assert_eq!(rule["noDataState"], "OK");
            assert_eq!(rule["labels"]["severity"], "warning");
            assert_eq!(rule["labels"]["signal_name"], signal);
        }
    }

    #[test]
    fn warns_while_the_stream_runs_on_a_fallback_endpoint() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);

        let document = provisioning_document(&config, &channels, false).unwrap();
        let rule = health_rule(&document, "ms-yellowstone-fallback-slack").unwrap();

        assert_eq!(
            rule["data"][0]["model"]["expr"],
            "max(microscope_yellowstone_active_endpoint{role=\"fallback\"}) or on() vector(0)"
        );
        assert_eq!(rule["data"][2]["model"]["expression"], "$B > 0");
        assert_eq!(rule["noDataState"], "OK");
        assert_eq!(rule["labels"]["severity"], "warning");
        assert_eq!(rule["labels"]["signal_name"], "yellowstone_fallback_active");
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

    fn rules(document: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
        document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["rules"].as_array().unwrap())
    }

    fn health_rule<'a>(
        document: &'a serde_json::Value,
        uid: &str,
    ) -> Option<&'a serde_json::Value> {
        rules(document).find(|rule| rule["uid"] == uid)
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
        let event = rules(&document)
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
            health_rule(document, "ms-yellowstone-stream-slack").cloned()
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
        let stalled = health_rule(&document, "ms-log-delivery-stalled-slack").unwrap();
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
        let stale = health_rule(&document, "ms-rpc-poll-stale-slack").unwrap();
        let lag = health_rule(&document, "ms-rpc-poll-lag-slack").unwrap();
        let quarantine = health_rule(&document, "ms-rpc-quarantine-slack").unwrap();

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
        assert_eq!(lag["data"][2]["model"]["expression"], "$B > 1200");
        assert_eq!(lag["labels"]["severity"], "warning");
        assert_eq!(
            quarantine["data"][0]["model"]["expr"],
            "microscope_rpc_poll_quarantined_transactions"
        );

        let checkpoint_stale = health_rule(&document, "ms-rpc-checkpoint-stale-slack").unwrap();
        assert!(checkpoint_stale["data"][0]["model"]["expr"]
            .as_str()
            .unwrap()
            .contains("microscope_rpc_checkpoint_last_success_unixtime"));
        assert_eq!(
            checkpoint_stale["data"][2]["model"]["expression"],
            "$B > 300"
        );
        assert_eq!(checkpoint_stale["noDataState"], "Alerting");

        let corrupt = health_rule(&document, "ms-rpc-checkpoint-corrupt-slack").unwrap();
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
            health_rule(&document, "ms-rpc-poll-failing-slack")
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
            default_interval["data"][2]["model"]["expression"], "$B >= 9",
            "the polls forty-five seconds of unbroken failure costs at a five-second interval"
        );
        assert_eq!(default_interval["labels"]["severity"], "warning");
        assert_eq!(default_interval["noDataState"], "OK");
        assert_eq!(
            failing(300)["data"][2]["model"]["expression"],
            "$B >= 1",
            "a poll interval longer than the sustained-failure duration alerts on the first \
             failure, the soonest one can be observed, rather than a poll later"
        );
        assert_eq!(
            failing_for(5, 600)["data"][2]["model"]["expression"],
            "$B >= 120",
            "a provider given ten minutes to recover pages only after ten minutes of failure"
        );
        assert_eq!(
            failing_for(10, 45)["data"][2]["model"]["expression"],
            "$B >= 5",
            "a duration the interval does not divide rounds up, so the rule never fires early"
        );
    }

    #[test]
    fn gates_rpc_health_alerts_on_polling_not_on_the_primary_mode() {
        let config = config(vec![]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let stale_uids = |document: &serde_json::Value| {
            rules(document)
                .filter(|rule| rule["uid"] == "ms-rpc-poll-stale-slack")
                .count()
        };

        let with_recovery = provisioning_document(
            &config,
            &channels,
            config.datasource.polls_rpc_with(Some("http://rpc.test")),
        )
        .unwrap();
        let without_recovery =
            provisioning_document(&config, &channels, config.datasource.polls_rpc_with(None))
                .unwrap();

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
    fn multisig_alerts_match_activity_on_every_configured_vault() {
        let single = config(vec![]);
        let mut several = config(vec![]);
        let mut second = several.multisigs[0].clone();
        second.vault_address = "SysvarC1ock11111111111111111111111111111111".to_string();
        several.multisigs.push(second);
        let rule = alert_with_channel(
            AlertKind::Multisig,
            "proposal_approved",
            AlertChannel::Slack,
        );

        let query = logql_query(&several, &rule).unwrap();
        assert!(query.contains(
            "| scope =~ \"11111111111111111111111111111111|SysvarC1ock11111111111111111111111111111111\""
        ));
        assert!(!query.contains("| scope = "));
        assert!(query.contains("sum by (signature, scope) (count_over_time("));
        assert_eq!(
            grafana_rule(&several, &rule, Some(AlertChannel::Slack)).unwrap()["annotations"]
                ["vault_address"],
            "{{ $labels.scope }}"
        );
        assert_eq!(
            grafana_rule(&several, &rule, Some(AlertChannel::Slack)).unwrap()["uid"],
            grafana_rule(&single, &rule, Some(AlertChannel::Slack)).unwrap()["uid"],
            "adding a multisig must not replace the rules that already exist"
        );
    }

    #[test]
    fn program_alerts_cover_every_program_unless_one_is_named() {
        let single = config(vec![]);
        let mut several = config(vec![]);
        several.programs.push(ProgramConfig {
            program_id: "SysvarC1ock11111111111111111111111111111111".to_string(),
            idl_path: "Cargo.toml".to_string(),
        });
        let mut rule = alert_with_channel(AlertKind::Event, "transfer", AlertChannel::Slack);

        let query = logql_query(&several, &rule).unwrap();
        assert!(query.contains(
            "| scope =~ \"11111111111111111111111111111111|SysvarC1ock11111111111111111111111111111111\""
        ));
        assert!(query.contains("sum by (signature, scope) (count_over_time("));
        let covering = grafana_rule(&several, &rule, Some(AlertChannel::Slack)).unwrap();
        assert_eq!(covering["annotations"]["program_id"], "{{ $labels.scope }}");

        let unnamed = grafana_rule(&single, &rule, Some(AlertChannel::Slack)).unwrap();
        rule.program_id = Some("11111111111111111111111111111111".to_string());
        let query = logql_query(&several, &rule).unwrap();
        assert!(query.contains("| scope = \"11111111111111111111111111111111\""));
        assert!(query.contains("sum by (signature) (count_over_time("));
        assert_eq!(
            grafana_rule(&several, &rule, Some(AlertChannel::Slack)).unwrap(),
            unnamed,
            "naming the program keeps the rule a single-program deployment had"
        );
    }

    #[test]
    fn a_vault_address_condition_narrows_a_multisig_alert_to_one_vault() {
        let mut config = config(vec![]);
        let mut second = config.multisigs[0].clone();
        second.vault_address = "SysvarC1ock11111111111111111111111111111111".to_string();
        config.multisigs.push(second);
        let mut rule = alert(AlertKind::Multisig, "proposal_approved");
        rule.conditions = vec![AlertCondition {
            field: "vault_address".to_string(),
            operator: AlertConditionOperator::Eq,
            value: Some(AlertConditionValue::String(
                "SysvarC1ock11111111111111111111111111111111".to_string(),
            )),
        }];

        let query = logql_query(&config, &rule).unwrap();
        assert!(query.contains(", condition_0=\"vault_address\""));
        assert!(query.contains("| (condition_0 = \"SysvarC1ock11111111111111111111111111111111\")"));
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
    fn activity_alerts_carry_the_explorer_link_on_its_own() {
        let mut config = config(vec![]);
        let transaction_url = |config: &Config| {
            let alert = alert(AlertKind::Event, "created");
            grafana_rule(config, &alert, Some(AlertChannel::Telegram)).unwrap()["annotations"]
                ["transaction_url"]
                .clone()
        };
        assert_eq!(
            transaction_url(&config),
            "https://explorer.solana.com/tx/{{ $labels.signature }}"
        );

        config.alerting.explorer_transaction_url =
            "https://explorer.example/tx/{signature}?cluster=devnet".to_string();
        assert_eq!(
            transaction_url(&config),
            "https://explorer.example/tx/{{ $labels.signature }}?cluster=devnet"
        );
    }

    /// Grafana updates a rule in place only while its UID holds, and a Grafana
    /// Cloud export leaves the rule behind the old UID active.
    #[test]
    fn rule_uids_stay_pinned_to_the_alert_identity() {
        let alert = alert(AlertKind::Event, "created");
        let rule = grafana_rule(&config(vec![]), &alert, Some(AlertChannel::Telegram)).unwrap();
        assert_eq!(rule["uid"], "ms-b0c36b31784b130c356feb6976226009");
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

    fn with_label_fields(mut alert: AlertRule, fields: &[&str]) -> AlertRule {
        alert.label_fields = fields.iter().map(|field| field.to_string()).collect();
        alert
    }

    fn exists(field: &str) -> AlertCondition {
        AlertCondition {
            field: field.to_string(),
            operator: AlertConditionOperator::Exists,
            value: None,
        }
    }

    #[test]
    fn label_fields_become_labels_of_the_matched_series() {
        let rule = with_label_fields(
            alert(AlertKind::Event, "created"),
            &["data.group", "data.refunded_total"],
        );

        assert_eq!(
            logql_query(&config(vec![]), &rule).unwrap(),
            "(sum by (signature, data_group, data_refunded_total) (count_over_time(\
             {service_name=\"microscope-indexer\"} | json kind=\"kind\", signal=\"name\", \
             scope=\"program_id\", signature=\"signature\", data_group=\"data.group\", \
             data_refunded_total=\"data.refunded_total\" | kind = \"program_event\" | \
             signal = \"created\" | scope = \"11111111111111111111111111111111\" | \
             __error__ = \"\" [60s])) or on() vector(0))"
        );
    }

    #[test]
    fn label_fields_follow_the_scope_of_multisig_and_multi_program_alerts() {
        let multisig = with_label_fields(
            alert(AlertKind::Multisig, "proposal_approved"),
            &["instruction"],
        );
        let query = logql_query(&config(vec![]), &multisig).unwrap();
        assert!(query.contains("sum by (signature, scope, instruction) (count_over_time("));
        assert!(query.contains(", failed=\"failed\", instruction=\"instruction\" |"));

        let event = with_label_fields(alert(AlertKind::Event, "created"), &["data.group"]);
        let query = logql_query(&several_programs(), &event).unwrap();
        assert!(query.contains("sum by (signature, scope, data_group) (count_over_time("));
    }

    /// `or` drops a series only when its labels equal one on its left, so a
    /// branch without the fields would send a second alert for one record.
    #[test]
    fn every_any_branch_carries_the_same_label_fields() {
        let mut rule = with_label_fields(
            alert(AlertKind::Event, "created"),
            &["data.group", "data.refunded_total"],
        );
        rule.match_mode = AlertMatch::Any;
        rule.conditions = vec![
            exists("data.memo"),
            exists("data.authority"),
            exists("data.reason"),
        ];

        let query = logql_query(&config(vec![]), &rule).unwrap();
        let branches = query
            .strip_prefix('(')
            .unwrap()
            .strip_suffix(" or on() vector(0))")
            .unwrap()
            .split(" or ")
            .collect::<Vec<_>>();

        assert_eq!(branches.len(), 3);
        for (index, branch) in branches.iter().enumerate() {
            assert!(
                branch.starts_with(
                    "sum by (signature, data_group, data_refunded_total) (count_over_time("
                ),
                "{branch}"
            );
            assert!(
                branch.contains(&format!(
                    "signature=\"signature\", data_group=\"data.group\", \
                     data_refunded_total=\"data.refunded_total\", condition_{index}=\""
                )),
                "{branch}"
            );
        }
    }

    #[test]
    fn an_alert_without_label_fields_keeps_its_query() {
        let mut rule = alert(AlertKind::Event, "created");
        rule.match_mode = AlertMatch::Any;
        rule.conditions = vec![exists("data.memo"), exists("data.authority")];

        let query = logql_query(&config(vec![]), &rule).unwrap();
        assert_eq!(
            query
                .matches("sum by (signature) (count_over_time(")
                .count(),
            2
        );
        assert!(query.contains("signature=\"signature\", condition_0=\"data.memo\" |"));
    }

    #[test]
    fn label_fields_show_in_a_fields_annotation_and_keep_the_rule_uid() {
        let plain = alert(AlertKind::Event, "created");
        let labelled = with_label_fields(
            alert(AlertKind::Event, "created"),
            &["data.group", "data.refunded_total"],
        );
        let rule =
            |alert| grafana_rule(&config(vec![]), alert, Some(AlertChannel::Telegram)).unwrap();

        let plain = rule(&plain);
        let labelled = rule(&labelled);
        assert_eq!(labelled["uid"], plain["uid"]);
        assert_eq!(
            labelled["annotations"]["fields"],
            "data.group={{ $labels.data_group }} · data.refunded_total={{ $labels.data_refunded_total }}"
        );
        assert!(plain["annotations"].get("fields").is_none());
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

        let rules = rules(&document).collect::<Vec<_>>();
        assert!(rules.len() > 1);

        for rule in rules {
            let receiver = if rule["labels"]["signal_kind"] == "datasource" {
                "microscope-slack"
            } else {
                "microscope-slack-activity"
            };
            assert_eq!(
                rule["notification_settings"]["receiver"], receiver,
                "{}",
                rule["title"]
            );
        }
    }

    /// An activity rule matches for one lookback window, then its series goes
    /// away and Grafana resolves it, so a resolved message would follow every
    /// match. Health alerts still say when they recover.
    #[test]
    fn gives_activity_alerts_a_contact_point_without_resolved_messages() {
        let channels = BTreeSet::from([
            AlertChannel::Slack,
            AlertChannel::Telegram,
            AlertChannel::Pagerduty,
        ]);
        let document = provisioning_document(&config(vec![]), &channels, false).unwrap();
        let contact_points = document["contactPoints"].as_array().unwrap();

        let disable_resolve = contact_points
            .iter()
            .map(|contact_point| {
                (
                    contact_point["name"].as_str().unwrap(),
                    contact_point["receivers"][0]["disableResolveMessage"]
                        .as_bool()
                        .unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            disable_resolve,
            BTreeMap::from([
                ("microscope-noop", true),
                ("microscope-pagerduty", false),
                ("microscope-slack", false),
                ("microscope-slack-activity", true),
                ("microscope-telegram", false),
                ("microscope-telegram-activity", true),
            ])
        );

        let receiver = |name: &str| {
            contact_points
                .iter()
                .find(|contact_point| contact_point["name"] == name)
                .unwrap()["receivers"][0]
                .clone()
        };
        let telegram = receiver("microscope-telegram");
        let activity = receiver("microscope-telegram-activity");
        assert_eq!(activity["uid"], "microscope-telegram-activity");
        assert_eq!(activity["type"], "telegram");
        let mut health_settings = telegram["settings"].clone();
        let health_message = health_settings["message"].as_str().unwrap();
        let listing = [
            "{{ range .Alerts.Firing }}",
            TELEGRAM_ALERT_TITLE,
            "\n{{ end }}",
        ]
        .concat();
        health_settings["message"] = health_message
            .replace(&listing, &above_five_firing("microscope-telegram-activity"))
            .replace("printf \"%.350s\"", "printf \"%.100s\"")
            .into();
        assert_eq!(
            activity["settings"], health_settings,
            "only the grouped description limit and the list above five differ"
        );
    }

    fn mentioning(mut alert: AlertRule, mention: &str) -> AlertRule {
        alert.slack_mention = Some(SlackMention::try_from(mention.to_string()).unwrap());
        alert
    }

    fn contact_point<'a>(document: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        document["contactPoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|contact_point| contact_point["name"] == name)
            .unwrap_or_else(|| panic!("no contact point {name}"))
    }

    /// A Slack contact point tags the same people on every message, so an
    /// alert tagging its owner needs a contact point no other owner shares.
    #[test]
    fn routes_slack_alerts_to_a_contact_point_that_tags_their_mention() {
        let owner = mentioning(
            alert_with_channel(AlertKind::Instruction, "set_authority", AlertChannel::Slack),
            "U0123ABCDEF",
        );
        let same_owner = mentioning(
            alert_with_channel(AlertKind::Instruction, "delete_plan", AlertChannel::Slack),
            "U0123ABCDEF",
        );
        let everyone = mentioning(
            alert_with_channel(AlertKind::Multisig, "proposal_created", AlertChannel::Slack),
            "@here",
        );
        let untagged = alert_with_channel(AlertKind::Event, "created", AlertChannel::Slack);
        let config = config(vec![owner, same_owner, everyone, untagged]);
        let channels = BTreeSet::from([AlertChannel::Slack]);
        let document = provisioning_document(&config, &channels, false).unwrap();

        let receivers = rules(&document)
            .filter(|rule| rule["labels"]["signal_kind"] != "datasource")
            .map(|rule| {
                (
                    rule["labels"]["signal_name"].as_str().unwrap(),
                    rule["notification_settings"]["receiver"].as_str().unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            receivers,
            BTreeMap::from([
                ("created", "microscope-slack-activity"),
                ("delete_plan", "microscope-slack-u0123abcdef"),
                ("proposal_created", "microscope-slack-here"),
                ("set_authority", "microscope-slack-u0123abcdef"),
            ])
        );

        let owner_point = contact_point(&document, "microscope-slack-u0123abcdef");
        assert_eq!(
            owner_point["receivers"][0]["settings"],
            serde_json::json!({ "url": "$SLACK_WEBHOOK_URL", "mentionUsers": "U0123ABCDEF" })
        );
        assert_eq!(owner_point["receivers"][0]["disableResolveMessage"], true);
        assert_eq!(
            contact_point(&document, "microscope-slack-here")["receivers"][0]["settings"]
                ["mentionChannel"],
            "here"
        );
        assert_eq!(
            contact_point(&document, "microscope-slack-activity")["receivers"][0]["settings"],
            serde_json::json!({ "url": "$SLACK_WEBHOOK_URL" })
        );
        let mention_points = document["contactPoints"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|contact_point| {
                contact_point["receivers"][0]["settings"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|key| key.starts_with("mention"))
            })
            .count();
        assert_eq!(mention_points, 2);
    }

    #[test]
    fn a_slack_mention_leaves_the_alerts_other_channels_untagged() {
        let mut alert = mentioning(
            alert_with_channel(AlertKind::Event, "created", AlertChannel::Slack),
            "U0123ABCDEF",
        );
        alert.channels.push(AlertChannel::Telegram);
        let config = config(vec![alert]);

        let telegram = grafana_rule(
            &config,
            &config.alert_rules[0],
            Some(AlertChannel::Telegram),
        )
        .unwrap();
        assert_eq!(
            telegram["notification_settings"]["receiver"],
            "microscope-telegram-activity"
        );
    }

    /// Moving an alert to another contact point must not replace its UID, or a
    /// Grafana Cloud export leaves the old rule firing.
    #[test]
    fn a_slack_mention_keeps_the_rule_uid() {
        let untagged = alert_with_channel(AlertKind::Event, "created", AlertChannel::Slack);
        let tagged = mentioning(
            alert_with_channel(AlertKind::Event, "created", AlertChannel::Slack),
            "@channel",
        );
        let uid = |alert: &AlertRule| {
            grafana_rule(&config(vec![]), alert, Some(AlertChannel::Slack)).unwrap()["uid"].clone()
        };
        assert_eq!(uid(&untagged), uid(&tagged));
    }

    #[test]
    fn health_alerts_tag_the_health_mention_on_slack_only() {
        let mut config = config(vec![alert_with_channel(
            AlertKind::Event,
            "created",
            AlertChannel::Slack,
        )]);
        config.alerting.health_slack_mention = Some(SlackMention::Here);
        let channels = BTreeSet::from([AlertChannel::Slack, AlertChannel::Telegram]);
        let document = provisioning_document(&config, &channels, false).unwrap();

        assert_eq!(
            contact_point(&document, "microscope-slack")["receivers"][0]["settings"],
            serde_json::json!({ "url": "$SLACK_WEBHOOK_URL", "mentionChannel": "here" })
        );
        assert!(
            contact_point(&document, "microscope-slack-activity")["receivers"][0]["settings"]
                .get("mentionChannel")
                .is_none()
        );
        assert!(
            contact_point(&document, "microscope-telegram")["receivers"][0]["settings"]
                .get("mentionChannel")
                .is_none()
        );
        assert!(rules(&document)
            .filter(|rule| rule["labels"]["signal_kind"] == "datasource"
                && rule["labels"]["channel"] == "slack")
            .all(|rule| rule["notification_settings"]["receiver"] == "microscope-slack"));
    }

    fn contact_point_settings(channel: AlertChannel, name: &str) -> serde_json::Value {
        let channels = BTreeSet::from([channel]);
        let document = provisioning_document(&config(vec![]), &channels, false).unwrap();
        document["contactPoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|contact_point| contact_point["name"] == name)
            .unwrap()["receivers"][0]["settings"]
            .clone()
    }

    fn several_programs() -> Config {
        let mut config = config(vec![]);
        config.programs.push(ProgramConfig {
            program_id: "SysvarC1ock11111111111111111111111111111111".to_string(),
            idl_path: "Cargo.toml".to_string(),
        });
        config
    }

    fn several_multisigs() -> Config {
        let mut config = config(vec![]);
        let mut second = config.multisigs[0].clone();
        second.vault_address = "SysvarC1ock11111111111111111111111111111111".to_string();
        config.multisigs.push(second);
        config
    }

    fn several_programs_and_multisigs() -> Config {
        let mut config = several_programs();
        config.multisigs = several_multisigs().multisigs;
        config
    }

    fn telegram_messages() -> Vec<String> {
        let configs = [
            config(vec![]),
            several_programs_and_multisigs(),
            config(vec![with_label_fields(
                alert_with_channel(AlertKind::Event, "created", AlertChannel::Telegram),
                &["data.group", "data.refunded_total"],
            )]),
        ];
        configs
            .iter()
            .flat_map(|config| [false, true].map(|activity| telegram_message(config, activity)))
            .collect()
    }

    fn template_actions(message: &str) -> impl Iterator<Item = &str> {
        message.split("{{").skip(1).map(|rest| {
            let action = rest.split("}}").next().unwrap();
            action.trim_matches(|c: char| c == '-' || c.is_whitespace())
        })
    }

    fn template_keyword(action: &str) -> &str {
        action.split_whitespace().next().unwrap()
    }

    #[test]
    fn telegram_contact_points_send_the_compact_message_as_html() {
        for name in ["microscope-telegram", "microscope-telegram-activity"] {
            let settings = contact_point_settings(AlertChannel::Telegram, name);
            assert_eq!(settings["bottoken"], "$TELEGRAM_BOT_TOKEN");
            assert_eq!(settings["chatid"], "$TELEGRAM_CHAT_ID");
            assert_eq!(settings["parse_mode"], "HTML");
            assert_eq!(settings["disable_web_page_preview"], true);

            let message = settings["message"].as_str().unwrap();
            assert!(
                message.contains("{{ range .Alerts.Firing }}"),
                "grouped alerts share no annotation, so each one is read on its own"
            );
            assert!(message.contains(".Annotations.transaction_url"));
            assert!(message.contains("{{ range .Alerts.Resolved }}✅ resolved: "));
            assert!(message.contains(
                "{{ if gt (len .Alerts.Resolved) 5 }}{{ range slice .Alerts.Resolved 0 5 }}✅ resolved: "
            ));
        }
    }

    #[test]
    fn slack_and_pagerduty_contact_points_keep_their_default_message() {
        assert_eq!(
            contact_point_settings(AlertChannel::Slack, "microscope-slack"),
            serde_json::json!({ "url": "$SLACK_WEBHOOK_URL" })
        );
        assert_eq!(
            contact_point_settings(AlertChannel::Pagerduty, "microscope-pagerduty"),
            serde_json::json!({
                "integrationKey": "$PAGERDUTY_INTEGRATION_KEY",
                "severity": "{{ .CommonLabels.severity }}",
            })
        );
    }

    /// Grafana expands `$NAME` in contact point settings.
    #[test]
    fn the_telegram_message_survives_grafana_env_expansion() {
        for message in telegram_messages() {
            assert!(!message.contains('$'));
        }
    }

    fn printed_actions(message: &str) -> Vec<&str> {
        template_actions(message)
            .filter(|action| {
                !["if", "else", "end", "range", "with", "len"].contains(&template_keyword(action))
            })
            .collect()
    }

    #[test]
    fn the_telegram_message_escapes_every_alert_value() {
        for message in telegram_messages() {
            let printed = printed_actions(&message);

            assert!(!printed.is_empty());
            for action in printed {
                let mut stages = action.split('|').map(str::trim).skip(1);
                assert_eq!(
                    stages.next(),
                    Some("html"),
                    "`{action}` reaches Telegram unescaped"
                );
                assert!(
                    stages.all(|stage| stage.starts_with("printf \"%.")),
                    "`{action}` changes the text after it is escaped"
                );
            }
        }
    }

    /// Grafana cuts the message at 4096 characters wherever that falls, and
    /// Telegram rejects a message cut inside a tag.
    #[test]
    fn the_telegram_message_bounds_every_text_it_prints() {
        for message in telegram_messages() {
            let unbounded = printed_actions(&message)
                .into_iter()
                .filter(|action| {
                    let last_stage = action.rsplit('|').next().unwrap().trim();
                    !last_stage.starts_with("printf \"%.")
                })
                .collect::<BTreeSet<_>>();

            assert_eq!(
                unbounded,
                BTreeSet::from([
                    ".Annotations.transaction_url | html",
                    ".DashboardURL | html",
                    ".GeneratorURL | html",
                    ".PanelURL | html",
                ]),
                "only a link address may be printed whole"
            );
        }
    }

    #[test]
    fn grouped_health_alerts_keep_a_longer_description_than_grouped_activity_alerts() {
        let description =
            |limit: usize| format!(".Annotations.summary | html | printf \"%.{limit}s\" }}}}");

        for (name, grouped_limit, grouped_branches) in [
            ("microscope-telegram", 350, 1),
            ("microscope-telegram-activity", 100, 2),
        ] {
            let settings = contact_point_settings(AlertChannel::Telegram, name);
            let message = settings["message"].as_str().unwrap();
            let (single, grouped) = message
                .split_once("{{ else if gt (len .Alerts.Firing) 5 }}")
                .unwrap();

            assert!(single.starts_with("{{ if eq (len .Alerts.Firing) 1 }}"));
            assert_eq!(single.matches(&description(3000)).count(), 1, "{name}");
            assert_eq!(
                grouped.matches(&description(grouped_limit)).count(),
                grouped_branches,
                "{name}"
            );
            assert_eq!(
                message.matches(".Annotations.summary").count(),
                1 + grouped_branches
            );
        }
    }

    fn above_five_firing(name: &str) -> String {
        let settings = contact_point_settings(AlertChannel::Telegram, name);
        let message = settings["message"].as_str().unwrap();
        let (_, rest) = message
            .split_once("{{ else if gt (len .Alerts.Firing) 5 }}")
            .unwrap();
        let (above_five, _) = rest
            .split_once("{{ else }}{{ range .Alerts.Firing }}")
            .unwrap();
        above_five.to_owned()
    }

    #[test]
    fn the_health_message_lists_every_firing_alert_above_five() {
        let above_five = above_five_firing("microscope-telegram");

        assert_eq!(
            above_five,
            format!("{{{{ range .Alerts.Firing }}}}{TELEGRAM_ALERT_TITLE}\n{{{{ end }}}}")
        );
        assert!(!above_five.contains("first 5 shown"));
        assert!(!above_five.contains(".Annotations"));
        assert!(!above_five.contains("URL"));
    }

    #[test]
    fn the_activity_message_still_shows_the_first_five_firing_alerts() {
        let above_five = above_five_firing("microscope-telegram-activity");

        assert!(above_five.starts_with("{{ range slice .Alerts.Firing 0 5 }}"));
        assert!(above_five
            .ends_with("{{ end }}… {{ len .Alerts.Firing }} alerts firing, first 5 shown\n"));
        assert!(above_five.contains(".Annotations.description"));
    }

    #[test]
    fn telegram_shows_the_fields_on_their_own_line_or_in_place_of_a_grouped_description() {
        let telegram = with_label_fields(
            alert_with_channel(AlertKind::Event, "created", AlertChannel::Telegram),
            &["data.group"],
        );
        let message = telegram_message(&config(vec![telegram]), true);
        let (single, grouped) = message
            .split_once("{{ else if gt (len .Alerts.Firing) 5 }}")
            .unwrap();

        assert_eq!(
            single
                .matches(
                    "{{ with .Annotations.fields }}<code>{{ . | html | printf \"%.300s\" }}</code>\n{{ end }}"
                )
                .count(),
            1
        );
        assert!(single.contains(
            "{{ or .Annotations.Error .Annotations.description .Annotations.summary | html | printf \"%.3000s\" }}"
        ));
        assert_eq!(
            grouped
                .matches(
                    "{{ if .Annotations.fields }}<code>{{ .Annotations.fields | html | printf \"%.100s\" }}</code>\
                     {{ else }}{{ or .Annotations.Error .Annotations.description .Annotations.summary \
                     | html | printf \"%.100s\" }}{{ end }}"
                )
                .count(),
            2
        );
        assert!(!grouped.contains("with .Annotations.fields"));
    }

    /// Telegram links text such as `data.group` unless it is inside `code`.
    #[test]
    fn telegram_prints_the_fields_as_code() {
        let mut printed_anywhere = 0;
        for message in telegram_messages() {
            let printed = message.matches(".Annotations.fields | html").count()
                + message.matches("{{ with .Annotations.fields }}").count();
            let as_code = message
                .matches("<code>{{ .Annotations.fields | html")
                .count()
                + message
                    .matches("{{ with .Annotations.fields }}<code>{{ . | html")
                    .count();
            assert_eq!(printed, as_code);
            assert_eq!(
                message.matches("<code>").count(),
                message.matches("</code>").count()
            );
            printed_anywhere += printed;
        }
        assert!(printed_anywhere > 0);
    }

    #[test]
    fn telegram_messages_change_only_for_activity_alerts_sent_there_with_fields() {
        let unchanged = |config: &Config| {
            for activity in [false, true] {
                assert_eq!(
                    telegram_message(config, activity),
                    telegram_message(&config_without_label_fields(), activity)
                );
            }
        };
        unchanged(&config(vec![with_label_fields(
            alert_with_channel(AlertKind::Event, "created", AlertChannel::Slack),
            &["data.group"],
        )]));

        let telegram = config(vec![with_label_fields(
            alert_with_channel(AlertKind::Event, "created", AlertChannel::Telegram),
            &["data.group"],
        )]);
        assert_eq!(
            telegram_message(&telegram, false),
            telegram_message(&config_without_label_fields(), false)
        );
        assert_ne!(
            telegram_message(&telegram, true),
            telegram_message(&config_without_label_fields(), true)
        );
    }

    fn config_without_label_fields() -> Config {
        config(vec![alert_with_channel(
            AlertKind::Event,
            "created",
            AlertChannel::Telegram,
        )])
    }

    /// The health contact point's longer grouped description fits in 4096
    /// characters only while health alerts print one link and nothing else.
    #[test]
    fn health_alerts_carry_no_annotation_the_message_prints_beyond_the_description() {
        let document = provisioning_document(
            &several_programs_and_multisigs(),
            &BTreeSet::from([AlertChannel::Telegram]),
            true,
        )
        .unwrap();

        let health = rules(&document)
            .filter(|rule| rule["labels"]["signal_kind"] == "datasource")
            .collect::<Vec<_>>();
        assert!(!health.is_empty());
        for rule in health {
            let annotations = rule["annotations"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                annotations,
                BTreeSet::from(["description", "summary"]),
                "{}",
                rule["uid"]
            );
        }
    }

    #[test]
    fn one_program_and_one_multisig_keep_the_three_line_message() {
        let message = telegram_message(&config(vec![]), true);

        assert!(!message.contains(".Annotations.program_id"));
        assert!(!message.contains(".Annotations.vault_address"));
    }

    #[test]
    fn several_programs_or_multisigs_name_the_one_that_matched() {
        let program = "{{ if .Annotations.program_id }}Program {{ .Annotations.program_id | html | printf \"%.44s\" }}\n";
        let vault = |keyword: &str| {
            format!(
                "{{{{ {keyword} .Annotations.vault_address }}}}Vault {{{{ .Annotations.vault_address | html | printf \"%.44s\" }}}}\n"
            )
        };
        let end = "{{ end }}{{ if .Annotations.transaction_url }}";

        let message = telegram_message(&several_programs(), true);
        assert_eq!(message.matches(&format!("{program}{end}")).count(), 3);
        assert!(!message.contains(".Annotations.vault_address"));

        let message = telegram_message(&several_multisigs(), true);
        assert_eq!(message.matches(&format!("{}{end}", vault("if"))).count(), 3);
        assert!(!message.contains(".Annotations.program_id"));

        let message = telegram_message(&several_programs_and_multisigs(), true);
        let both = format!("{program}{}{end}", vault("else if"));
        assert_eq!(message.matches(&both).count(), 3);
    }

    #[test]
    fn the_telegram_message_closes_every_block() {
        for message in telegram_messages() {
            let count = |keyword: &str| {
                template_actions(&message)
                    .filter(|action| template_keyword(action) == keyword)
                    .count()
            };

            assert_eq!(count("if") + count("range") + count("with"), count("end"));
        }
    }

    #[test]
    fn routes_every_activity_kind_on_telegram_to_its_activity_contact_point() {
        let channels = BTreeSet::from([AlertChannel::Telegram]);
        let document = provisioning_document(
            &config(vec![
                alert_with_channel(AlertKind::Event, "created", AlertChannel::Telegram),
                alert_with_channel(AlertKind::Instruction, "transfer", AlertChannel::Telegram),
                alert_with_channel(
                    AlertKind::Multisig,
                    "proposal_approved",
                    AlertChannel::Telegram,
                ),
            ]),
            &channels,
            false,
        )
        .unwrap();

        let receivers = rules(&document)
            .map(|rule| {
                (
                    rule["labels"]["signal_kind"].as_str().unwrap(),
                    rule["notification_settings"]["receiver"].as_str().unwrap(),
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            receivers,
            BTreeSet::from([
                ("datasource", "microscope-telegram"),
                ("event", "microscope-telegram-activity"),
                ("instruction", "microscope-telegram-activity"),
                ("multisig", "microscope-telegram-activity"),
            ])
        );
    }

    /// PagerDuty closes an incident only on the resolve event. Without one, the
    /// incident stays open and later matches join it instead of opening a new one.
    #[test]
    fn keeps_pagerduty_activity_alerts_on_the_resolving_contact_point() {
        let channels = BTreeSet::from([AlertChannel::Pagerduty]);
        let document = provisioning_document(
            &config(vec![alert_with_channel(
                AlertKind::Event,
                "created",
                AlertChannel::Pagerduty,
            )]),
            &channels,
            false,
        )
        .unwrap();

        let event = rules(&document)
            .find(|rule| rule["labels"]["signal_kind"] == "event")
            .unwrap();
        assert_eq!(
            event["notification_settings"]["receiver"],
            "microscope-pagerduty"
        );
    }

    /// Grafana places the route it derives from a rule's receiver ahead of the
    /// tree and stops there, so only rules without a channel reach the tree.
    #[test]
    fn keeps_only_the_muted_noop_route_in_the_policy_tree() {
        let channels = BTreeSet::from([
            AlertChannel::Slack,
            AlertChannel::Telegram,
            AlertChannel::Pagerduty,
        ]);
        let document = provisioning_document(&config(vec![]), &channels, false).unwrap();

        assert_eq!(
            document["policies"][0]["routes"],
            serde_json::json!([{
                "receiver": "microscope-noop",
                "object_matchers": [["channel", "=", "none"]],
                "mute_time_intervals": ["microscope-always"],
                "continue": false,
            }])
        );
    }

    fn every_channel() -> BTreeSet<AlertChannel> {
        BTreeSet::from([
            AlertChannel::Slack,
            AlertChannel::Telegram,
            AlertChannel::Pagerduty,
        ])
    }

    fn alerts_on_every_channel() -> Vec<AlertRule> {
        every_channel()
            .into_iter()
            .map(|channel| alert_with_channel(AlertKind::Event, "created", channel))
            .collect()
    }

    fn own_policy_tree(alert_rules: Vec<AlertRule>) -> Config {
        let mut config = config(alert_rules);
        config.alerting.own_policy_tree = true;
        config
    }

    #[test]
    fn own_policy_tree_adds_a_grouped_health_route_for_slack_and_telegram() {
        let document =
            provisioning_document(&own_policy_tree(vec![]), &every_channel(), false).unwrap();

        let health_route = |channel: &str| {
            serde_json::json!({
                "receiver": format!("microscope-{channel}"),
                "object_matchers": [["channel", "=", channel], ["signal_kind", "=", "datasource"]],
                "group_by": ["deployment", "signal_kind"],
                "group_wait": "30s",
                "group_interval": "1m",
                "repeat_interval": "4h",
                "continue": false,
            })
        };
        assert_eq!(
            document["policies"][0]["routes"],
            serde_json::json!([
                {
                    "receiver": "microscope-noop",
                    "object_matchers": [["channel", "=", "none"]],
                    "mute_time_intervals": ["microscope-always"],
                    "continue": false,
                },
                health_route("slack"),
                health_route("telegram"),
            ])
        );
    }

    #[test]
    fn own_policy_tree_routes_name_contact_points_the_document_declares() {
        let document =
            provisioning_document(&own_policy_tree(vec![]), &every_channel(), false).unwrap();

        let declared = contact_point_uids(&document);
        for route in document["policies"][0]["routes"].as_array().unwrap() {
            let receiver = route["receiver"].as_str().unwrap();
            assert!(declared.contains(receiver), "{receiver}");
        }
    }

    #[test]
    fn own_policy_tree_takes_the_receiver_off_slack_and_telegram_health_rules_only() {
        let document = provisioning_document(
            &own_policy_tree(alerts_on_every_channel()),
            &every_channel(),
            true,
        )
        .unwrap();

        let mut grouped = 0;
        for rule in rules(&document) {
            let channel = rule["labels"]["channel"].as_str().unwrap();
            let health = rule["labels"]["signal_kind"] == "datasource";
            let expected = match (health, channel) {
                (true, "slack" | "telegram") => None,
                (false, "slack" | "telegram") => Some(format!("microscope-{channel}-activity")),
                _ => Some(format!("microscope-{channel}")),
            };
            grouped += usize::from(expected.is_none());
            assert_eq!(
                rule.get("notification_settings")
                    .map(|settings| settings["receiver"].as_str().unwrap().to_string()),
                expected,
                "{}",
                rule["title"]
            );
        }
        assert!(grouped > 0);
    }

    #[test]
    fn own_policy_tree_keeps_the_activity_receiver_of_an_alert_in_the_health_interval() {
        let alerts = alerts_on_every_channel()
            .into_iter()
            .map(|mut alert| {
                alert.evaluation_interval_seconds = Some(RPC_HEALTH_EVALUATION_INTERVAL_SECONDS);
                alert
            })
            .collect();
        let document =
            provisioning_document(&own_policy_tree(alerts), &every_channel(), true).unwrap();

        let mut checked = 0;
        for rule in rules(&document).filter(|rule| rule["labels"]["signal_kind"] == "event") {
            let channel = rule["labels"]["channel"].as_str().unwrap();
            if ["slack", "telegram"].contains(&channel) {
                assert_eq!(
                    rule["notification_settings"]["receiver"],
                    format!("microscope-{channel}-activity"),
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 2);
    }

    #[test]
    fn own_policy_tree_changes_nothing_but_the_routing() {
        let without_routing = |config: &Config| {
            let mut document = provisioning_document(config, &every_channel(), true).unwrap();
            document["policies"][0]
                .as_object_mut()
                .unwrap()
                .remove("routes");
            for group in document["groups"].as_array_mut().unwrap() {
                for rule in group["rules"].as_array_mut().unwrap() {
                    rule.as_object_mut()
                        .unwrap()
                        .remove("notification_settings");
                }
            }
            document
        };
        let default = without_routing(&config(alerts_on_every_channel()));
        let opted_in = without_routing(&own_policy_tree(alerts_on_every_channel()));

        assert_eq!(default, opted_in);
        assert_eq!(rule_uids(&default), rule_uids(&opted_in));
        assert_eq!(contact_point_uids(&default), contact_point_uids(&opted_in));
    }

    #[test]
    fn own_policy_tree_without_a_channel_keeps_only_the_muted_route() {
        let document = provisioning_document(
            &own_policy_tree(vec![alert(AlertKind::Event, "created")]),
            &Default::default(),
            true,
        )
        .unwrap();

        assert_eq!(
            document["policies"][0]["routes"].as_array().unwrap().len(),
            1
        );
        assert!(rules(&document).all(|rule| rule.get("notification_settings").is_none()));
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
            serde_json::json!([
                { "orgId": 1, "uid": "microscope-slack" },
                { "orgId": 1, "uid": "microscope-slack-activity" },
            ])
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
