use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context};
use heck::ToTitleCase;
use serde_json::{json, Value};

use crate::config::{Config, DatasourceMode};

const OVERVIEW_TEMPLATE: &str = include_str!("../../../grafana/dashboard-templates/overview.json");
pub const OVERVIEW_UID: &str = "microscope-overview";
pub const EVENT_PANEL_ID: u64 = 6;
const MULTISIG_ACTIVITY_PANEL_ID: u64 = 7;
pub const MULTISIG_PANEL_ID: u64 = 8;
const RPC_POLLING_ROW_ID: u64 = 9;
const RPC_POLL_AGE_PANEL_ID: u64 = 10;
const RPC_POLL_LAG_PANEL_ID: u64 = 11;
const RPC_QUARANTINE_PANEL_ID: u64 = 12;
const RPC_HEAD_SLOT_PANEL_ID: u64 = 13;
const RPC_ACTIVITY_PANEL_ID: u64 = 14;

pub fn generate(config: &Config, output_dir: &Path) -> anyhow::Result<PathBuf> {
    let rpc_polling = config.datasource.mode == DatasourceMode::Rpc
        || env::var("RPC_URL").is_ok_and(|url| !url.trim().is_empty());
    let document = overview_document(config, rpc_polling)?;
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "creating dashboard output directory {}",
            output_dir.display()
        )
    })?;

    let output = output_dir.join("overview.json");
    let temporary = output_dir.join(".overview.json.tmp");
    let contents = serde_json::to_vec_pretty(&document)?;
    fs::write(&temporary, contents)
        .with_context(|| format!("writing temporary dashboard {}", temporary.display()))?;
    fs::rename(&temporary, &output)
        .with_context(|| format!("installing dashboard {}", output.display()))?;

    Ok(output)
}

fn overview_document(config: &Config, rpc_polling: bool) -> anyhow::Result<Value> {
    let mut dashboard: Value = serde_json::from_str(OVERVIEW_TEMPLATE)?;
    let panels = dashboard["panels"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("overview dashboard template is missing panels"))?;
    #[cfg(program_events)]
    {
        let event_panel = panels
            .iter_mut()
            .find(|panel| panel["id"].as_u64() == Some(EVENT_PANEL_ID))
            .ok_or_else(|| anyhow!("overview dashboard template is missing event panel"))?;
        configure_table_panel(
            event_panel,
            &config.dashboard.event_fields,
            "event_field",
            "program_event",
            Some(("program_id", &config.program_id)),
            "Decoded program events for the configured program. Missing configured fields display as a placeholder.",
        );
    }

    if let Some(multisig) = config.multisig.as_ref() {
        let multisig_panel = panels
            .iter_mut()
            .find(|panel| panel["id"].as_u64() == Some(MULTISIG_PANEL_ID))
            .ok_or_else(|| anyhow!("overview dashboard template is missing multisig panel"))?;
        configure_table_panel(
            multisig_panel,
            &config.dashboard.multisig_fields,
            "multisig_field",
            "multisig_activity",
            Some(("vault_address", multisig.vault_address.as_str())),
            "Squads multisig activity for the configured vault. Missing configured fields display as a placeholder.",
        );
    }

    if rpc_polling {
        panels.extend(rpc_polling_panels(
            config.datasource.rpc_poll_stale_after_seconds(),
        ));
    }

    if config.multisig.is_none() {
        remove_panels(panels, &[MULTISIG_ACTIVITY_PANEL_ID, MULTISIG_PANEL_ID]);
    }

    // A program whose IDL declares no events emits none, so the panel would sit
    // permanently empty, which reads the same as broken decoding.
    #[cfg(not(program_events))]
    remove_panels(panels, &[EVENT_PANEL_ID]);

    Ok(dashboard)
}

fn remove_panels(panels: &mut Vec<Value>, ids: &[u64]) {
    let is_removed = |panel: &Value| panel["id"].as_u64().is_some_and(|id| ids.contains(&id));
    let removed = panels
        .iter()
        .filter(|panel| is_removed(panel))
        .map(|panel| {
            (
                panel["gridPos"]["y"].as_u64().unwrap_or_default(),
                panel["gridPos"]["h"].as_u64().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    panels.retain(|panel| !is_removed(panel));

    for panel in panels.iter_mut() {
        let y = panel["gridPos"]["y"].as_u64().unwrap_or_default();
        let shift = removed
            .iter()
            .filter(|(removed_y, _)| *removed_y < y)
            .map(|(_, height)| height)
            .sum::<u64>();
        panel["gridPos"]["y"] = json!(y.saturating_sub(shift));
    }
}

fn rpc_polling_panels(stale_after_seconds: u64) -> Vec<Value> {
    let prometheus = json!({ "type": "prometheus", "uid": "prometheus" });
    vec![
        json!({
            "id": RPC_POLLING_ROW_ID,
            "title": "RPC polling health",
            "type": "row",
            "collapsed": false,
            "gridPos": { "h": 1, "w": 24, "x": 0, "y": 56 },
            "panels": [],
        }),
        json!({
            "id": RPC_POLL_AGE_PANEL_ID,
            "title": "Seconds since successful RPC poll",
            "description": "Alerts when no RPC poll succeeds within the configured freshness window.",
            "type": "stat",
            "datasource": prometheus,
            "gridPos": { "h": 6, "w": 6, "x": 0, "y": 57 },
            "fieldConfig": { "defaults": {
                "unit": "s",
                "thresholds": { "mode": "absolute", "steps": [
                    { "color": "green", "value": null },
                    { "color": "orange", "value": stale_after_seconds / 2 },
                    { "color": "red", "value": stale_after_seconds }
                ] }
            }, "overrides": [] },
            "targets": [{
                "expr": "time() - (microscope_rpc_poll_last_success_unixtime or microscope_rpc_poll_started_unixtime)",
                "legendFormat": "poll age",
                "refId": "A"
            }]
        }),
        json!({
            "id": RPC_POLL_LAG_PANEL_ID,
            "title": "RPC polling lag",
            "description": "Confirmed slots between the RPC head and the oldest address cursor.",
            "type": "stat",
            "datasource": prometheus,
            "gridPos": { "h": 6, "w": 6, "x": 6, "y": 57 },
            "fieldConfig": { "defaults": { "unit": "short" }, "overrides": [] },
            "targets": [{
                "expr": "microscope_rpc_poll_lag_slots",
                "legendFormat": "slots",
                "refId": "A"
            }]
        }),
        json!({
            "id": RPC_QUARANTINE_PANEL_ID,
            "title": "Quarantined RPC transactions",
            "description": "Transactions skipped after bounded fetch or conversion failures.",
            "type": "stat",
            "datasource": prometheus,
            "gridPos": { "h": 6, "w": 6, "x": 12, "y": 57 },
            "fieldConfig": { "defaults": {
                "unit": "short",
                "thresholds": { "mode": "absolute", "steps": [
                    { "color": "green", "value": null },
                    { "color": "red", "value": 1 }
                ] }
            }, "overrides": [] },
            "targets": [{
                "expr": "microscope_rpc_poll_quarantined_transactions",
                "legendFormat": "quarantined",
                "refId": "A"
            }]
        }),
        json!({
            "id": RPC_HEAD_SLOT_PANEL_ID,
            "title": "RPC confirmed head slot",
            "type": "stat",
            "datasource": prometheus,
            "gridPos": { "h": 6, "w": 6, "x": 18, "y": 57 },
            "fieldConfig": { "defaults": { "unit": "short" }, "overrides": [] },
            "targets": [{
                "expr": "microscope_rpc_poll_head_slot",
                "legendFormat": "slot",
                "refId": "A"
            }]
        }),
        json!({
            "id": RPC_ACTIVITY_PANEL_ID,
            "title": "RPC polling activity",
            "description": "Transaction throughput and polling, fetch, conversion, and quarantine failures.",
            "type": "timeseries",
            "datasource": prometheus,
            "gridPos": { "h": 8, "w": 24, "x": 0, "y": 63 },
            "fieldConfig": { "defaults": { "unit": "ops" }, "overrides": [] },
            "targets": [
                {
                    "expr": "sum(rate(microscope_rpc_poll_transactions_total[5m]))",
                    "legendFormat": "transactions/s",
                    "refId": "A"
                },
                {
                    "expr": "sum(rate(microscope_rpc_poll_failures_total[5m]))",
                    "legendFormat": "poll failures/s",
                    "refId": "B"
                },
                {
                    "expr": "sum(rate(microscope_rpc_poll_transaction_failures_total[5m]))",
                    "legendFormat": "transaction failures/s",
                    "refId": "C"
                },
                {
                    "expr": "sum(rate(microscope_rpc_poll_quarantined_transactions_total[5m]))",
                    "legendFormat": "quarantined/s",
                    "refId": "D"
                }
            ]
        }),
    ]
}

fn configure_table_panel(
    panel: &mut Value,
    fields: &[String],
    field_prefix: &str,
    record_kind: &str,
    scope: Option<(&str, &str)>,
    description: &str,
) {
    let json_paths = fields
        .iter()
        .enumerate()
        .map(|(index, path)| {
            json!({
                "alias": field_key(field_prefix, index),
                "path": path,
            })
        })
        .collect::<Vec<_>>();
    let mut field_order = serde_json::Map::from_iter([("Time".to_string(), json!(0))]);
    let mut field_names = serde_json::Map::new();
    let display_names = collision_safe_display_names(fields);
    for (index, display_name) in display_names.iter().enumerate() {
        let key = field_key(field_prefix, index);
        field_order.insert(key.clone(), json!(index + 1));
        field_names.insert(key, json!(display_name));
    }

    panel["type"] = json!("table");
    panel["description"] = json!(description);
    panel["fieldConfig"] = json!({
        "defaults": {
            "custom": {
                "align": "auto",
                "cellOptions": { "type": "auto" },
                "filterable": true,
                "inspect": false,
            },
            "mappings": [],
            "noValue": "—",
            "thresholds": {
                "mode": "absolute",
                "steps": [{ "color": "green", "value": null }],
            },
        },
        "overrides": [],
    });
    panel["options"] = json!({
        "cellHeight": "sm",
        "footer": { "show": false },
        "showHeader": true,
        "sortBy": [{ "desc": true, "displayName": "Time" }],
    });
    let (scope_label, scope_filter) = match scope {
        Some((path, value)) => (
            format!(", scope=\"{path}\""),
            format!(" | scope = {}", json!(value)),
        ),
        None => (String::new(), String::new()),
    };
    panel["targets"] = json!([{
        "datasource": { "type": "loki", "uid": "loki" },
        "editorMode": "code",
        "expr": format!("{{service_name=\"microscope-indexer\"}} | json kind=\"kind\"{scope_label} | kind = \"{record_kind}\"{scope_filter} | __error__ = \"\""),
        "queryType": "range",
        "refId": "A",
    }]);
    panel["transformations"] = json!([
        {
            "id": "extractFields",
            "options": {
                "format": "json",
                "jsonPaths": json_paths,
                "keepTime": true,
                "replace": true,
                "source": "Line",
            },
        },
        {
            "id": "organize",
            "options": {
                "excludeByName": {},
                "includeByName": {},
                "indexByName": field_order,
                "renameByName": field_names,
            },
        },
    ]);
}

fn field_key(prefix: &str, index: usize) -> String {
    format!("{prefix}_{index}")
}

fn display_name(path: &str) -> String {
    match path {
        "name" => "Event".to_string(),
        "program_id" => "Program ID".to_string(),
        other => other.rsplit('.').next().unwrap_or(other).to_title_case(),
    }
}

fn collision_safe_display_names(paths: &[String]) -> Vec<String> {
    let short_names = paths
        .iter()
        .map(|path| display_name(path))
        .collect::<Vec<_>>();
    let counts = short_names.iter().fold(HashMap::new(), |mut counts, name| {
        *counts.entry(name.clone()).or_insert(0) += 1;
        counts
    });

    paths
        .iter()
        .zip(&short_names)
        .map(|(path, short_name)| {
            if counts[short_name] > 1 {
                path.clone()
            } else {
                short_name.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::config::{
        AlertingConfig, Config, DashboardConfig, DatasourceConfig, DatasourceMode, MultisigConfig,
        MultisigVersion,
    };
    use serde_json::json;

    use super::{
        overview_document, EVENT_PANEL_ID, MULTISIG_ACTIVITY_PANEL_ID, MULTISIG_PANEL_ID,
        RPC_ACTIVITY_PANEL_ID, RPC_POLLING_ROW_ID, RPC_POLL_AGE_PANEL_ID,
    };

    fn config(event_fields: &[&str]) -> Config {
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
            dashboard: DashboardConfig {
                event_fields: event_fields.iter().map(|field| field.to_string()).collect(),
                ..DashboardConfig::default()
            },
            alert_rules: vec![],
        }
    }

    #[test]
    #[cfg(program_events)]
    fn generates_table_columns_from_configured_json_paths() {
        let dashboard =
            overview_document(&config(&["name", "data.amount", "data.receiver"]), false)
                .expect("dashboard generates");
        let panel = dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|panel| panel["id"] == 6)
            .unwrap();

        assert_eq!(panel["type"], "table");
        assert_eq!(
            panel["transformations"][0]["options"]["jsonPaths"],
            json!([
                { "alias": "event_field_0", "path": "name" },
                { "alias": "event_field_1", "path": "data.amount" },
                { "alias": "event_field_2", "path": "data.receiver" },
            ])
        );
        assert_eq!(
            panel["transformations"][1]["options"]["renameByName"]["event_field_1"],
            "Amount"
        );
        assert_eq!(panel["fieldConfig"]["defaults"]["noValue"], "—");
        assert_eq!(
            panel["fieldConfig"]["defaults"]["custom"]["filterable"],
            true
        );
        assert_eq!(
            panel["targets"][0]["expr"],
            "{service_name=\"microscope-indexer\"} | json kind=\"kind\", scope=\"program_id\" \
             | kind = \"program_event\" | scope = \"11111111111111111111111111111111\" \
             | __error__ = \"\""
        );
    }

    #[test]
    fn table_panels_hide_records_from_a_previously_monitored_target() {
        let mut config = config(&["name"]);
        config.program_id = "So11111111111111111111111111111111111111112".to_string();
        config.multisig.as_mut().unwrap().vault_address =
            "SysvarC1ock11111111111111111111111111111111".to_string();

        let dashboard = overview_document(&config, false).expect("dashboard generates");
        let panels = dashboard["panels"].as_array().unwrap();
        let expr = |id: u64| {
            panels.iter().find(|panel| panel["id"] == id).unwrap()["targets"][0]["expr"]
                .as_str()
                .unwrap()
                .to_string()
        };

        #[cfg(program_events)]
        assert!(expr(EVENT_PANEL_ID)
            .contains("| scope = \"So11111111111111111111111111111111111111112\""));
        assert!(expr(MULTISIG_PANEL_ID)
            .contains("| scope = \"SysvarC1ock11111111111111111111111111111111\""));
    }

    /// An unscoped multisig panel charts every multisig on the cluster, and a
    /// scoped one with no vault to scope to is permanently empty, which reads
    /// the same as broken decoding.
    #[test]
    fn leaves_out_the_multisig_panels_when_no_vault_is_configured() {
        let mut config = config(&["name"]);
        config.multisig = None;

        let dashboard = overview_document(&config, true).expect("dashboard generates");
        let panels = dashboard["panels"].as_array().unwrap();
        let ids = panels
            .iter()
            .map(|panel| panel["id"].as_u64().unwrap())
            .collect::<Vec<_>>();

        assert!(!ids.contains(&MULTISIG_ACTIVITY_PANEL_ID), "{ids:?}");
        assert!(!ids.contains(&MULTISIG_PANEL_ID), "{ids:?}");
    }

    /// Which panels the events-less build drops is a compile-time decision, so
    /// only one side of it is reachable from a single test binary. This covers
    /// the removal the other side performs.
    #[test]
    #[cfg(program_events)]
    fn removing_the_event_panel_closes_the_gap_it_leaves() {
        use serde_json::Value;

        use super::remove_panels;

        let dashboard = overview_document(&config(&["name"]), true).expect("dashboard generates");
        let mut panels = dashboard["panels"].as_array().unwrap().clone();
        let y = |panels: &[Value], id: u64| {
            panels
                .iter()
                .find(|panel| panel["id"].as_u64() == Some(id))
                .unwrap()["gridPos"]["y"]
                .as_u64()
                .unwrap()
        };
        let before = y(&panels, RPC_POLLING_ROW_ID);

        remove_panels(&mut panels, &[EVENT_PANEL_ID]);

        assert!(!panels
            .iter()
            .any(|panel| panel["id"].as_u64() == Some(EVENT_PANEL_ID)));
        assert_eq!(y(&panels, RPC_POLLING_ROW_ID), before - 12);
        assert_eq!(y(&panels, MULTISIG_ACTIVITY_PANEL_ID), 24);
    }

    /// Removing a panel from the middle of the grid leaves a hole that pushes
    /// everything below it off the bottom of the viewport.
    #[test]
    fn closes_the_vertical_gap_the_removed_multisig_panels_leave() {
        let mut config = config(&["name"]);
        let with_multisig = overview_document(&config, true).expect("dashboard generates");
        config.multisig = None;
        let without_multisig = overview_document(&config, true).expect("dashboard generates");

        let present = |id: u64| {
            with_multisig["panels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|panel| panel["id"].as_u64() == Some(id))
        };

        for (id, shift) in [
            (EVENT_PANEL_ID, 8),
            (RPC_POLLING_ROW_ID, 8 + 12),
            (RPC_ACTIVITY_PANEL_ID, 8 + 12),
        ]
        .into_iter()
        .filter(|(id, _)| present(*id))
        {
            let y = |dashboard: &serde_json::Value| {
                dashboard["panels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|panel| panel["id"].as_u64() == Some(id))
                    .unwrap()["gridPos"]["y"]
                    .as_u64()
                    .unwrap()
            };
            assert_eq!(
                y(&without_multisig),
                y(&with_multisig) - shift,
                "panel {id} did not move up"
            );
        }
    }

    /// The counters are seeded at startup, so a surviving `vector(0)` fallback
    /// would only chart a confident zero for a renamed or unscraped metric.
    #[test]
    fn charts_counters_without_masking_a_missing_series() {
        let dashboard = overview_document(&config(&["name"]), true).expect("dashboard generates");
        let expressions = dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|panel| panel["targets"].as_array().into_iter().flatten())
            .filter_map(|target| target["expr"].as_str())
            .collect::<Vec<_>>();

        assert!(expressions.contains(&"sum(rate(microscope_errors_total[5m]))"));
        assert!(
            expressions.iter().all(|expr| !expr.contains("vector(0)")),
            "{expressions:?}"
        );
    }

    #[test]
    #[cfg(program_events)]
    fn uses_full_paths_only_for_colliding_column_names() {
        let dashboard = overview_document(
            &config(&["data.amount", "fees.amount", "data.receiver"]),
            false,
        )
        .expect("dashboard generates");
        let panel = dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|panel| panel["id"] == 6)
            .unwrap();
        let names = &panel["transformations"][1]["options"]["renameByName"];

        assert_eq!(names["event_field_0"], "data.amount");
        assert_eq!(names["event_field_1"], "fees.amount");
        assert_eq!(names["event_field_2"], "Receiver");
    }

    #[test]
    fn adds_rpc_polling_health_panels_in_rpc_mode() {
        let mut config = config(&["name"]);
        config.datasource.mode = DatasourceMode::Rpc;
        config.datasource.poll_interval_seconds = 20;

        let dashboard = overview_document(&config, true).expect("dashboard generates");
        let panels = dashboard["panels"].as_array().unwrap();
        let poll_age = panels
            .iter()
            .find(|panel| panel["id"] == RPC_POLL_AGE_PANEL_ID)
            .unwrap();
        let activity = panels
            .iter()
            .find(|panel| panel["id"] == RPC_ACTIVITY_PANEL_ID)
            .unwrap();

        assert_eq!(
            poll_age["targets"][0]["expr"],
            "time() - (microscope_rpc_poll_last_success_unixtime or microscope_rpc_poll_started_unixtime)"
        );
        assert_eq!(
            poll_age["fieldConfig"]["defaults"]["thresholds"]["steps"][2]["value"],
            120
        );
        assert!(activity["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|target| target["expr"]
                .as_str()
                .unwrap()
                .contains("microscope_rpc_poll_transaction_failures_total")));
    }

    #[test]
    fn omits_rpc_polling_health_panels_without_rpc_polling() {
        let dashboard = overview_document(&config(&["name"]), false).expect("dashboard generates");
        assert!(!dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|panel| panel["id"] == RPC_POLLING_ROW_ID));
    }

    #[test]
    fn adds_rpc_polling_health_panels_alongside_yellowstone_recovery() {
        let config = config(&["name"]);

        let dashboard = overview_document(&config, true).expect("dashboard generates");

        assert_eq!(config.datasource.mode, DatasourceMode::Yellowstone);
        assert!(dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|panel| panel["id"] == RPC_POLLING_ROW_ID));
    }

    #[test]
    fn generates_configurable_multisig_table_columns() {
        let mut config = config(&["name"]);
        config.dashboard.multisig_fields =
            ["action", "squads_version", "data.data.member", "failed"]
                .into_iter()
                .map(str::to_string)
                .collect();

        let dashboard = overview_document(&config, false).expect("dashboard generates");
        let panel = dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|panel| panel["id"] == 8)
            .unwrap();

        assert_eq!(panel["type"], "table");
        assert_eq!(
            panel["transformations"][0]["options"]["jsonPaths"],
            json!([
                { "alias": "multisig_field_0", "path": "action" },
                { "alias": "multisig_field_1", "path": "squads_version" },
                { "alias": "multisig_field_2", "path": "data.data.member" },
                { "alias": "multisig_field_3", "path": "failed" },
            ])
        );
        assert_eq!(
            panel["transformations"][1]["options"]["renameByName"]["multisig_field_1"],
            "Squads Version"
        );
        assert!(panel["targets"][0]["expr"].as_str().unwrap().contains(
            "kind = \"multisig_activity\" | scope = \"11111111111111111111111111111111\""
        ));
        assert_eq!(panel["fieldConfig"]["defaults"]["noValue"], "—");
    }
}
