use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, Context};
use heck::ToSnakeCase;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::{alerting::RPC_POLL_FAILURE_WINDOW_SECONDS, multisig::action_names};

const GRAFANA_SCHEDULER_INTERVAL_SECONDS: u64 = 10;
const MIN_RPC_POLL_INTERVAL_SECONDS: u64 = 1;
const MAX_RPC_POLL_INTERVAL_SECONDS: u64 = 300;
const MIN_RPC_REPLAY_WINDOW_SLOTS: u64 = 1;
const MAX_RPC_REPLAY_WINDOW_SLOTS: u64 = 100_000;
const MAX_EXACT_LOKI_INTEGER: u64 = 1 << 53;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertKind {
    #[default]
    Event,
    Instruction,
    Multisig,
}

impl AlertKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Instruction => "instruction",
            Self::Multisig => "multisig",
        }
    }
}

impl fmt::Display for AlertKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Critical,
    Error,
    Warning,
    Info,
}

impl AlertSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

impl fmt::Display for AlertSeverity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertChannel {
    Slack,
    Telegram,
    Pagerduty,
}

impl AlertChannel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Telegram => "telegram",
            Self::Pagerduty => "pagerduty",
        }
    }
}

impl fmt::Display for AlertChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertMatch {
    #[default]
    All,
    Any,
}

impl AlertMatch {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertConditionOperator {
    Exists,
    Contains,
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl AlertConditionOperator {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exists => "exists",
            Self::Contains => "contains",
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AlertConditionValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertCondition {
    pub field: String,
    pub operator: AlertConditionOperator,
    #[serde(default)]
    pub value: Option<AlertConditionValue>,
}

pub(crate) fn validate_field_path(field: &str) -> anyhow::Result<()> {
    if field.is_empty()
        || field.split('.').any(|segment| {
            let mut characters = segment.chars();
            !matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
                || characters
                    .any(|character| character != '_' && !character.is_ascii_alphanumeric())
        })
    {
        bail!(
            "field {field:?} must be a dot-separated JSON path containing letters, numbers, and underscores"
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRule {
    #[serde(default)]
    pub kind: AlertKind,
    pub name: String,
    #[serde(default, rename = "match")]
    pub match_mode: AlertMatch,
    #[serde(default)]
    pub conditions: Vec<AlertCondition>,
    pub severity: AlertSeverity,
    #[serde(default)]
    pub channels: Vec<AlertChannel>,
    #[serde(default)]
    pub lookback_window_seconds: Option<u64>,
    #[serde(default)]
    pub evaluation_interval_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AlertingConfig {
    pub lookback_window_seconds: u64,
    pub evaluation_interval_seconds: u64,
    /// `{signature}` is replaced with the matched transaction's signature.
    pub explorer_transaction_url: String,
    /// A poller failing a share of its polls indefinitely leaves every
    /// freshness and lag rule green, so the failure count itself has to be
    /// alertable. How long a provider stays broken before that count pages
    /// depends on the provider, so the duration is the knob rather than the
    /// count: the rule divides it by the poll interval.
    pub rpc_poll_sustained_failure_seconds: u64,
    /// Grafana honours the pending period for the Error state too, so a
    /// datasource that stays unreachable pages while a single failed
    /// evaluation does not. A rule pends only while its condition holds, so
    /// log-backed rules cap this at half their window.
    pub health_pending_period_seconds: u64,
    /// Multisig proposals are approved over hours, so a misconfigured vault has
    /// to stay alerting long enough to be noticed rather than resolving between
    /// instructions.
    pub multisig_unmatched_window_seconds: u64,
}

impl Default for AlertingConfig {
    fn default() -> Self {
        Self {
            lookback_window_seconds: 60,
            evaluation_interval_seconds: 10,
            explorer_transaction_url: "https://explorer.solana.com/tx/{signature}".to_string(),
            rpc_poll_sustained_failure_seconds: 45,
            health_pending_period_seconds: 300,
            multisig_unmatched_window_seconds: 3600,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub event_fields: Vec<String>,
    pub multisig_fields: Vec<String>,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            event_fields: ["name", "source", "signature", "slot", "failed"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            multisig_fields: [
                "action",
                "squads_version",
                "instruction",
                "signature",
                "slot",
                "failed",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MultisigVersion {
    V3,
    V4,
    V5,
}

impl MultisigVersion {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V3 => "v3",
            Self::V4 => "v4",
            Self::V5 => "v5",
        }
    }
}

impl fmt::Display for MultisigVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultisigConfig {
    pub vault_address: String,
    pub state_address: String,
    pub version: MultisigVersion,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatasourceMode {
    #[default]
    Yellowstone,
    Rpc,
}

impl DatasourceMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yellowstone => "yellowstone",
            Self::Rpc => "rpc",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatasourceConfig {
    pub mode: DatasourceMode,
    pub poll_interval_seconds: u64,
    pub replay_window_slots: u64,
}

impl Default for DatasourceConfig {
    fn default() -> Self {
        Self {
            mode: DatasourceMode::Yellowstone,
            poll_interval_seconds: 5,
            replay_window_slots: 300,
        }
    }
}

impl DatasourceConfig {
    pub const fn rpc_poll_stale_after_seconds(&self) -> u64 {
        let threshold = self.poll_interval_seconds.saturating_mul(6);
        if threshold < 60 {
            60
        } else {
            threshold
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub program_id: String,
    pub idl_path: String,
    pub multisig: Option<MultisigConfig>,
    #[serde(default)]
    pub datasource: DatasourceConfig,
    #[serde(default)]
    pub alerting: AlertingConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default, rename = "alerts")]
    pub alert_rules: Vec<AlertRule>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        config.validate(path)?;
        Ok(config)
    }

    pub fn resolved_idl_path(&self, config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&self.idl_path)
    }

    fn validate(&self, config_path: &Path) -> anyhow::Result<()> {
        Pubkey::from_str(&self.program_id)
            .with_context(|| format!("invalid program_id {}", self.program_id))?;
        if let Some(multisig) = &self.multisig {
            Pubkey::from_str(&multisig.vault_address).with_context(|| {
                format!("invalid multisig.vault_address {}", multisig.vault_address)
            })?;
            Pubkey::from_str(&multisig.state_address).with_context(|| {
                format!("invalid multisig.state_address {}", multisig.state_address)
            })?;
        }

        let idl_path = self.resolved_idl_path(config_path);
        if !idl_path.is_file() {
            bail!("IDL file {} does not exist", idl_path.display());
        }

        if !(MIN_RPC_POLL_INTERVAL_SECONDS..=MAX_RPC_POLL_INTERVAL_SECONDS)
            .contains(&self.datasource.poll_interval_seconds)
        {
            bail!(
                "datasource.poll_interval_seconds must be between {MIN_RPC_POLL_INTERVAL_SECONDS} and {MAX_RPC_POLL_INTERVAL_SECONDS}"
            );
        }
        if !(MIN_RPC_REPLAY_WINDOW_SLOTS..=MAX_RPC_REPLAY_WINDOW_SLOTS)
            .contains(&self.datasource.replay_window_slots)
        {
            bail!(
                "datasource.replay_window_slots must be between {MIN_RPC_REPLAY_WINDOW_SLOTS} and {MAX_RPC_REPLAY_WINDOW_SLOTS}"
            );
        }

        validate_timing(
            self.alerting.lookback_window_seconds,
            self.alerting.evaluation_interval_seconds,
        )
        .context("invalid alerting defaults")?;

        if !(1..=RPC_POLL_FAILURE_WINDOW_SECONDS)
            .contains(&self.alerting.rpc_poll_sustained_failure_seconds)
        {
            bail!(
                "alerting.rpc_poll_sustained_failure_seconds must be between 1 and {RPC_POLL_FAILURE_WINDOW_SECONDS}, the window the rule counts failures over"
            );
        }
        if self.alerting.health_pending_period_seconds == 0 {
            bail!("alerting.health_pending_period_seconds must be greater than zero");
        }
        if self.alerting.multisig_unmatched_window_seconds == 0 {
            bail!("alerting.multisig_unmatched_window_seconds must be greater than zero");
        }

        validate_dashboard_fields("event_fields", &self.dashboard.event_fields)?;
        validate_dashboard_fields("multisig_fields", &self.dashboard.multisig_fields)?;

        let mut idl_signals = None;
        for (index, alert) in self.alert_rules.iter().enumerate() {
            if matches!(alert.kind, AlertKind::Multisig) && self.multisig.is_none() {
                bail!(
                    "alerts entry at index {index} has kind multisig \
                     but no [multisig] section is configured"
                );
            }
            alert
                .validate(&self.alerting)
                .with_context(|| format!("invalid alerts entry at index {index}"))?;

            let known_names: Vec<&str> = match alert.kind {
                AlertKind::Event | AlertKind::Instruction => {
                    let signals = match idl_signals.as_ref() {
                        Some(signals) => signals,
                        None => idl_signals.insert(IdlSignalNames::read(&idl_path)?),
                    };
                    let names = match alert.kind {
                        AlertKind::Event => &signals.events,
                        _ => &signals.instructions,
                    };
                    names.iter().map(String::as_str).collect()
                }
                AlertKind::Multisig => action_names(
                    self.multisig
                        .as_ref()
                        .expect("multisig alerts are rejected above without a [multisig] section")
                        .version,
                )
                .to_vec(),
            };
            if !known_names.contains(&alert.name.as_str()) {
                bail!(
                    "alerts entry at index {index} names an unknown {} signal {:?}; \
                     this deployment emits {}",
                    alert.kind,
                    alert.name,
                    known_names.join(", ")
                );
            }
        }

        Ok(())
    }

    pub fn multisig_vault_pubkey(&self) -> Option<Pubkey> {
        self.multisig.as_ref().map(|multisig| {
            Pubkey::from_str(&multisig.vault_address)
                .expect("multisig.vault_address is validated while loading config")
        })
    }

    pub fn multisig_state_pubkey(&self) -> Option<Pubkey> {
        self.multisig.as_ref().map(|multisig| {
            Pubkey::from_str(&multisig.state_address)
                .expect("multisig.state_address is validated while loading config")
        })
    }
}

struct IdlSignalNames {
    instructions: BTreeSet<String>,
    events: BTreeSet<String>,
}

impl IdlSignalNames {
    fn read(idl_path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read(idl_path)
            .with_context(|| format!("reading IDL file {}", idl_path.display()))?;
        let idl: serde_json::Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing IDL file {}", idl_path.display()))?;
        let program = idl.get("program").unwrap_or(&idl);
        Ok(Self {
            instructions: declared_names(program, "instructions"),
            events: declared_names(program, "events"),
        })
    }
}

fn declared_names(program: &serde_json::Value, section: &str) -> BTreeSet<String> {
    program[section]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["name"].as_str())
        .map(str::to_snake_case)
        .collect()
}

fn validate_dashboard_fields(name: &str, fields: &[String]) -> anyhow::Result<()> {
    if fields.is_empty() {
        bail!("dashboard.{name} must contain at least one field");
    }
    let mut unique_fields = BTreeSet::new();
    for field in fields {
        validate_field_path(field).with_context(|| format!("invalid dashboard {name} field"))?;
        if !unique_fields.insert(field) {
            bail!("duplicate dashboard {name} field {field}");
        }
    }
    Ok(())
}

impl AlertRule {
    fn validate(&self, defaults: &AlertingConfig) -> anyhow::Result<()> {
        if self.name.trim().is_empty() {
            bail!("name must not be empty");
        }
        let mut unique_channels = BTreeSet::new();
        for channel in &self.channels {
            if !unique_channels.insert(channel) {
                bail!("duplicate channel {channel}");
            }
        }
        for (index, condition) in self.conditions.iter().enumerate() {
            condition
                .validate()
                .with_context(|| format!("invalid condition at index {index}"))?;
            if self.kind == AlertKind::Multisig && condition.field == "failed" {
                bail!(
                    "condition at index {index} is redundant or unsatisfiable: multisig alerts \
                     already match successful activity only"
                );
            }
        }
        validate_timing(
            self.lookback_window_seconds(defaults),
            self.evaluation_interval_seconds(defaults),
        )?;

        Ok(())
    }

    pub fn lookback_window_seconds(&self, defaults: &AlertingConfig) -> u64 {
        self.lookback_window_seconds
            .unwrap_or(defaults.lookback_window_seconds)
    }

    pub fn evaluation_interval_seconds(&self, defaults: &AlertingConfig) -> u64 {
        self.evaluation_interval_seconds
            .unwrap_or(defaults.evaluation_interval_seconds)
    }
}

impl AlertCondition {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        validate_field_path(&self.field)?;

        if let Some(AlertConditionValue::Integer(value)) = self.value {
            if value.unsigned_abs() > MAX_EXACT_LOKI_INTEGER {
                bail!(
                    "integer condition value {value} exceeds {MAX_EXACT_LOKI_INTEGER}; Loki \
                     compares numbers as float64, so larger values match neighbouring integers. \
                     Quote the value to compare it as an exact string instead"
                );
            }
        }

        match (self.operator, &self.value) {
            (AlertConditionOperator::Exists, None) => {}
            (AlertConditionOperator::Exists, Some(_)) => {
                bail!("operator exists does not accept a value");
            }
            (_, None) => bail!("operator {} requires a value", self.operator.as_str()),
            (AlertConditionOperator::Contains, Some(AlertConditionValue::String(value))) => {
                if value.is_empty() {
                    bail!("operator contains requires a non-empty string value");
                }
            }
            (AlertConditionOperator::Contains, Some(_)) => {
                bail!("operator contains requires a string value");
            }
            (
                AlertConditionOperator::Gt
                | AlertConditionOperator::Gte
                | AlertConditionOperator::Lt
                | AlertConditionOperator::Lte,
                Some(AlertConditionValue::Integer(_)),
            ) => {}
            (
                AlertConditionOperator::Gt
                | AlertConditionOperator::Gte
                | AlertConditionOperator::Lt
                | AlertConditionOperator::Lte,
                Some(AlertConditionValue::Float(value)),
            ) if value.is_finite() => {}
            (
                AlertConditionOperator::Gt
                | AlertConditionOperator::Gte
                | AlertConditionOperator::Lt
                | AlertConditionOperator::Lte,
                Some(_),
            ) => bail!(
                "operator {} requires a finite numeric value",
                self.operator.as_str()
            ),
            (
                AlertConditionOperator::Eq | AlertConditionOperator::Ne,
                Some(AlertConditionValue::Float(value)),
            ) if !value.is_finite() => bail!(
                "operator {} requires a finite numeric value",
                self.operator.as_str()
            ),
            (AlertConditionOperator::Eq | AlertConditionOperator::Ne, Some(_)) => {}
        }

        Ok(())
    }
}

fn validate_timing(
    lookback_window_seconds: u64,
    evaluation_interval_seconds: u64,
) -> anyhow::Result<()> {
    if lookback_window_seconds == 0 {
        bail!("lookback_window_seconds must be greater than zero");
    }
    if evaluation_interval_seconds == 0 {
        bail!("evaluation_interval_seconds must be greater than zero");
    }
    if !evaluation_interval_seconds.is_multiple_of(GRAFANA_SCHEDULER_INTERVAL_SECONDS) {
        bail!(
            "evaluation_interval_seconds must be a multiple of the Grafana scheduler interval \
             ({GRAFANA_SCHEDULER_INTERVAL_SECONDS} seconds)"
        );
    }
    if lookback_window_seconds < evaluation_interval_seconds {
        bail!(
            "lookback_window_seconds must be greater than or equal to evaluation_interval_seconds"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde::Deserialize;

    use super::{
        AlertChannel, AlertCondition, AlertConditionOperator, AlertConditionValue, AlertKind,
        AlertMatch, AlertRule, AlertSeverity, AlertingConfig, Config, DashboardConfig,
        DatasourceConfig, DatasourceMode, MultisigConfig, MultisigVersion,
    };

    const IDL: &str = "tests/fixtures/idl.json";

    fn config(program_id: &str, vault_address: &str, idl_path: &str) -> Config {
        Config {
            program_id: program_id.to_string(),
            idl_path: idl_path.to_string(),
            multisig: Some(MultisigConfig {
                vault_address: vault_address.to_string(),
                state_address: vault_address.to_string(),
                version: MultisigVersion::V4,
            }),
            datasource: DatasourceConfig::default(),
            alerting: AlertingConfig::default(),
            dashboard: DashboardConfig::default(),
            alert_rules: vec![],
        }
    }

    #[test]
    fn validates_program_multisig_and_idl() {
        let valid = "11111111111111111111111111111111";
        let config = config(valid, valid, "Cargo.toml");

        config
            .validate(Path::new("microscope.toml"))
            .expect("valid deployment config");
    }

    #[test]
    fn rejects_invalid_program_id() {
        let config = config(
            "not-a-pubkey",
            "11111111111111111111111111111111",
            "Cargo.toml",
        );

        assert!(config.validate(Path::new("microscope.toml")).is_err());
    }

    #[test]
    fn rejects_invalid_multisig_vault_address() {
        let config = config(
            "11111111111111111111111111111111",
            "not-a-pubkey",
            "Cargo.toml",
        );

        assert!(config.validate(Path::new("microscope.toml")).is_err());
    }

    #[test]
    fn rejects_invalid_multisig_state_address() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.multisig.as_mut().unwrap().state_address = "not-a-pubkey".to_string();

        assert!(config.validate(Path::new("microscope.toml")).is_err());
    }

    #[test]
    fn accepts_missing_multisig_config() {
        let mut config = config("11111111111111111111111111111111", "unused", "Cargo.toml");
        config.multisig = None;

        config
            .validate(Path::new("microscope.toml"))
            .expect("config without a multisig is valid");
    }

    #[test]
    fn deserializes_explicit_multisig_config() {
        for version in ["v3", "v4", "v5"] {
            let raw = format!(
                r#"program_id = "11111111111111111111111111111111"
idl_path = "Cargo.toml"

[multisig]
vault_address = "11111111111111111111111111111111"
state_address = "11111111111111111111111111111111"
version = "{version}""#
            );
            let config: Config = toml::from_str(&raw).unwrap();
            let multisig = config.multisig.unwrap();
            assert_eq!(multisig.version.as_str(), version);
            assert_eq!(multisig.state_address, "11111111111111111111111111111111");
        }

        assert!(toml::from_str::<Config>(
            r#"program_id = "11111111111111111111111111111111"
idl_path = "Cargo.toml"
multisig_address = "11111111111111111111111111111111""#
        )
        .is_err());
        assert!(toml::from_str::<Config>(
            r#"program_id = "11111111111111111111111111111111"
idl_path = "Cargo.toml"

[multisig]
vault_address = "11111111111111111111111111111111"
version = "v4""#
        )
        .is_err());
        assert!(toml::from_str::<Config>(
            r#"program_id = "11111111111111111111111111111111"
idl_path = "Cargo.toml"

[multisig]
vault_address = "11111111111111111111111111111111"
state_address = "11111111111111111111111111111111""#
        )
        .is_err());
    }

    #[test]
    fn rejects_multisig_alert_without_multisig_config() {
        let mut config = config("11111111111111111111111111111111", "unused", "Cargo.toml");
        config.multisig = None;
        config.alert_rules.push(AlertRule {
            kind: AlertKind::Multisig,
            name: "proposal_created".to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![],
            severity: AlertSeverity::Warning,
            channels: vec![],
            lookback_window_seconds: None,
            evaluation_interval_seconds: None,
        });

        assert!(config.validate(Path::new("microscope.toml")).is_err());
    }

    #[test]
    fn rejects_missing_idl() {
        let valid = "11111111111111111111111111111111";
        let config = config(valid, valid, "missing.json");

        assert!(config.validate(Path::new("microscope.toml")).is_err());
    }

    #[test]
    fn validates_structured_alert_rules() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, IDL);
        config.alert_rules.push(AlertRule {
            kind: AlertKind::Instruction,
            name: "create_record".to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![
                AlertCondition {
                    field: "data.data.transfer_data.amount".to_string(),
                    operator: AlertConditionOperator::Gt,
                    value: Some(AlertConditionValue::Integer(0)),
                },
                AlertCondition {
                    field: "failed".to_string(),
                    operator: AlertConditionOperator::Eq,
                    value: Some(AlertConditionValue::Boolean(false)),
                },
            ],
            severity: AlertSeverity::Warning,
            channels: vec![AlertChannel::Slack],
            lookback_window_seconds: None,
            evaluation_interval_seconds: None,
        });

        config
            .validate(Path::new("microscope.toml"))
            .expect("structured alert rule is valid");
    }

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

    #[test]
    fn accepts_alert_names_matching_the_snake_cased_idl_declarations() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, IDL);
        config.alert_rules = vec![
            alert(AlertKind::Event, "record_created_event"),
            alert(AlertKind::Instruction, "close_record"),
            alert(AlertKind::Multisig, "proposal_approved"),
        ];

        config
            .validate(Path::new("microscope.toml"))
            .expect("names declared by the IDL and the multisig version are alertable");
    }

    #[test]
    fn rejects_alert_names_the_deployment_can_never_emit() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, IDL);
        config.alert_rules = vec![alert(AlertKind::Event, "record_archived_event")];

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("an event the IDL does not declare would stay silently green forever");
        assert!(format!("{error:#}").contains("unknown event signal \"record_archived_event\""));
        assert!(format!("{error:#}").contains("record_created_event"));

        config.alert_rules = vec![alert(AlertKind::Instruction, "record_created_event")];
        assert!(config
            .validate(Path::new("microscope.toml"))
            .is_err_and(|error| format!("{error:#}").contains("unknown instruction signal")));
    }

    #[test]
    fn rejects_multisig_actions_the_configured_version_never_emits() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, IDL);
        config.alert_rules = vec![alert(AlertKind::Multisig, "spending_limit_used")];

        config
            .validate(Path::new("microscope.toml"))
            .expect("v4 emits spending_limit_used");

        config.multisig.as_mut().unwrap().version = MultisigVersion::V3;
        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("v3 has no spending limits, so the alert could never fire");
        assert!(format!("{error:#}").contains("unknown multisig signal \"spending_limit_used\""));
    }

    #[test]
    fn rejects_multisig_conditions_that_fight_the_built_in_success_filter() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, IDL);
        let mut rule = alert(AlertKind::Multisig, "proposal_approved");
        rule.conditions = vec![AlertCondition {
            field: "failed".to_string(),
            operator: AlertConditionOperator::Eq,
            value: Some(AlertConditionValue::Boolean(true)),
        }];
        config.alert_rules = vec![rule];

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("the generated query already pins failed to false");
        assert!(format!("{error:#}").contains("multisig alerts already match successful activity"));
    }

    #[test]
    fn defaults_to_yellowstone_and_deserializes_rpc_polling() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            datasource: DatasourceConfig,
        }

        let default: Wrapper = toml::from_str("").unwrap();
        assert_eq!(default.datasource.mode, DatasourceMode::Yellowstone);
        assert_eq!(default.datasource.poll_interval_seconds, 5);
        assert_eq!(default.datasource.rpc_poll_stale_after_seconds(), 60);
        assert_eq!(default.datasource.replay_window_slots, 300);

        let rpc: Wrapper = toml::from_str(
            r#"[datasource]
mode = "rpc"
poll_interval_seconds = 15
replay_window_slots = 900"#,
        )
        .unwrap();
        assert_eq!(rpc.datasource.mode, DatasourceMode::Rpc);
        assert_eq!(rpc.datasource.poll_interval_seconds, 15);
        assert_eq!(rpc.datasource.rpc_poll_stale_after_seconds(), 90);
        assert_eq!(rpc.datasource.replay_window_slots, 900);
    }

    #[test]
    fn rejects_invalid_rpc_poll_interval() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.datasource.poll_interval_seconds = 0;

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("zero-second polling must be rejected");
        assert!(format!("{error:#}")
            .contains("datasource.poll_interval_seconds must be between 1 and 300"));
    }

    #[test]
    fn rejects_invalid_rpc_replay_window() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.datasource.replay_window_slots = 0;

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("an empty replay window must be rejected");
        assert!(format!("{error:#}")
            .contains("datasource.replay_window_slots must be between 1 and 100000"));
    }

    #[test]
    fn applies_dashboard_defaults_when_section_is_absent() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            dashboard: DashboardConfig,
        }

        let wrapper: Wrapper = toml::from_str("").unwrap();

        assert_eq!(
            wrapper.dashboard.event_fields,
            DashboardConfig::default().event_fields
        );
        assert_eq!(
            wrapper.dashboard.multisig_fields,
            DashboardConfig::default().multisig_fields
        );
    }

    #[test]
    fn rejects_invalid_or_duplicate_dashboard_fields() {
        let valid = "11111111111111111111111111111111";
        let mut event_config = config(valid, valid, "Cargo.toml");
        event_config.dashboard.event_fields =
            vec!["data.amount".to_string(), "data.amount".to_string()];
        assert!(event_config.validate(Path::new("microscope.toml")).is_err());

        event_config.dashboard.event_fields = vec!["data..amount".to_string()];
        assert!(event_config.validate(Path::new("microscope.toml")).is_err());

        let mut multisig_config = config(valid, valid, "Cargo.toml");
        multisig_config.dashboard.multisig_fields =
            vec!["action".to_string(), "action".to_string()];
        assert!(multisig_config
            .validate(Path::new("microscope.toml"))
            .is_err());

        multisig_config.dashboard.multisig_fields = vec!["data..member".to_string()];
        assert!(multisig_config
            .validate(Path::new("microscope.toml"))
            .is_err());
    }

    #[test]
    fn rejects_alert_rules_with_invalid_field_paths() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.alert_rules.push(AlertRule {
            kind: AlertKind::Event,
            name: "record_created_event".to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![AlertCondition {
                field: "data..amount".to_string(),
                operator: AlertConditionOperator::Gt,
                value: Some(AlertConditionValue::Integer(0)),
            }],
            severity: AlertSeverity::Critical,
            channels: vec![],
            lookback_window_seconds: None,
            evaluation_interval_seconds: None,
        });

        assert!(config.validate(Path::new("microscope.toml")).is_err());
    }

    #[test]
    fn rejects_alert_rules_with_duplicate_channels() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.alert_rules.push(AlertRule {
            kind: AlertKind::Event,
            name: "record_created_event".to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![],
            severity: AlertSeverity::Warning,
            channels: vec![AlertChannel::Slack, AlertChannel::Slack],
            lookback_window_seconds: None,
            evaluation_interval_seconds: None,
        });

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("duplicate channels must be rejected");
        assert!(format!("{error:#}").contains("duplicate channel slack"));
    }

    #[test]
    fn rejects_unknown_alert_enum_values_during_deserialization() {
        for invalid_rule in [
            r#"kind = "log"
name = "created"
severity = "warning"
channels = []"#,
            r#"kind = "event"
name = "created"
severity = "urgent"
channels = []"#,
            r#"kind = "event"
name = "created"
severity = "warning"
channels = ["email"]"#,
            r#"kind = "event"
name = "created"
match = "some"
severity = "warning"
channels = []"#,
            r#"kind = "event"
name = "created"
severity = "warning"
conditions = [{ field = "data.amount", operator = "changed", value = 1 }]
channels = []"#,
        ] {
            assert!(toml::from_str::<AlertRule>(invalid_rule).is_err());
        }
    }

    #[test]
    fn rejects_legacy_alert_fields() {
        assert!(toml::from_str::<AlertRule>(
            r#"name = "created"
field = "data.amount"
condition = "> 0"
severity = "warning"
channels = []"#,
        )
        .is_err());
    }

    #[test]
    fn validates_condition_operator_values() {
        let invalid_rules = [
            r#"name = "created"
severity = "warning"
conditions = [{ field = "data.amount", operator = "exists", value = 1 }]"#,
            r#"name = "created"
severity = "warning"
conditions = [{ field = "data.amount", operator = "contains", value = 1 }]"#,
            r#"name = "created"
severity = "warning"
conditions = [{ field = "data.amount", operator = "gt", value = "1" }]"#,
            r#"name = "created"
severity = "warning"
conditions = [{ field = "data.amount", operator = "eq" }]"#,
        ];

        for invalid_rule in invalid_rules {
            let rule: AlertRule = toml::from_str(invalid_rule).unwrap();
            assert!(rule.validate(&AlertingConfig::default()).is_err());
        }
    }

    #[test]
    fn rejects_integer_condition_values_loki_cannot_compare_exactly() {
        let condition = |value: i64| AlertCondition {
            field: "data.amount".to_string(),
            operator: AlertConditionOperator::Gt,
            value: Some(AlertConditionValue::Integer(value)),
        };

        condition(9_007_199_254_740_992)
            .validate()
            .expect("the largest threshold float64 represents exactly stays usable");

        let error = condition(9_007_199_254_740_993)
            .validate()
            .expect_err("a threshold Loki rounds would silently shift the alert boundary");
        assert!(format!("{error:#}").contains("Loki compares numbers as float64"));

        assert!(condition(-9_007_199_254_740_993).validate().is_err());
    }

    #[test]
    fn deserializes_typed_any_condition_groups() {
        let rule: AlertRule = toml::from_str(
            r#"name = "created"
match = "any"
severity = "warning"
conditions = [
  { field = "data.amount", operator = "gte", value = 1000 },
  { field = "data.currency", operator = "eq", value = "USDC" },
  { field = "failed", operator = "eq", value = false },
  { field = "data.memo", operator = "exists" },
]"#,
        )
        .unwrap();

        assert_eq!(rule.match_mode, AlertMatch::Any);
        assert_eq!(
            rule.conditions[0].value,
            Some(AlertConditionValue::Integer(1000))
        );
        assert_eq!(
            rule.conditions[1].value,
            Some(AlertConditionValue::String("USDC".to_string()))
        );
        assert_eq!(
            rule.conditions[2].value,
            Some(AlertConditionValue::Boolean(false))
        );
        assert_eq!(rule.conditions[3].value, None);
        rule.validate(&AlertingConfig::default()).unwrap();
    }

    #[test]
    fn applies_alerting_defaults_and_per_rule_overrides() {
        let defaults: AlertingConfig = toml::from_str("").unwrap();
        assert_eq!(defaults.lookback_window_seconds, 60);
        assert_eq!(defaults.evaluation_interval_seconds, 10);

        let rule: AlertRule = toml::from_str(
            r#"name = "created"
severity = "warning"
channels = []
lookback_window_seconds = 300
evaluation_interval_seconds = 30"#,
        )
        .unwrap();
        assert_eq!(rule.lookback_window_seconds(&defaults), 300);
        assert_eq!(rule.evaluation_interval_seconds(&defaults), 30);
    }

    #[test]
    fn rejects_invalid_alert_timing() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.alert_rules.push(AlertRule {
            kind: AlertKind::Event,
            name: "created".to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![],
            severity: AlertSeverity::Warning,
            channels: vec![],
            lookback_window_seconds: Some(5),
            evaluation_interval_seconds: Some(10),
        });

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("the lookback must cover the interval between evaluations");
        assert!(format!("{error:#}").contains(
            "lookback_window_seconds must be greater than or equal to evaluation_interval_seconds"
        ));
    }

    #[test]
    fn rejects_default_evaluation_interval_outside_grafana_scheduler() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.alerting.evaluation_interval_seconds = 5;

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("Grafana cannot schedule five-second evaluations");
        assert!(format!("{error:#}").contains(
            "evaluation_interval_seconds must be a multiple of the Grafana scheduler interval \
             (10 seconds)"
        ));
    }

    #[test]
    fn rejects_alert_evaluation_interval_outside_grafana_scheduler() {
        let valid = "11111111111111111111111111111111";
        let mut config = config(valid, valid, "Cargo.toml");
        config.alert_rules.push(AlertRule {
            kind: AlertKind::Event,
            name: "created".to_string(),
            match_mode: AlertMatch::All,
            conditions: vec![],
            severity: AlertSeverity::Warning,
            channels: vec![],
            lookback_window_seconds: Some(60),
            evaluation_interval_seconds: Some(15),
        });

        let error = config
            .validate(Path::new("microscope.toml"))
            .expect_err("Grafana cannot schedule fifteen-second evaluations");
        assert!(format!("{error:#}").contains(
            "evaluation_interval_seconds must be a multiple of the Grafana scheduler interval \
             (10 seconds)"
        ));
    }
}
