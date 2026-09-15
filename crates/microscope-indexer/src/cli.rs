use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};

use crate::backfill::parse_duration;

#[derive(Debug, Parser)]
#[command(
    name = "microscope-indexer",
    version,
    about = "Decode and monitor activity from a Solana program"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the indexer and monitoring pipeline.
    Run {
        #[arg(default_value = "microscope.toml", value_name = "CONFIG")]
        config_path: PathBuf,
    },
    /// Backfill historical activity from an RPC endpoint into Loki, backdated
    /// to each transaction's block time.
    Backfill {
        #[arg(default_value = "microscope.toml", value_name = "CONFIG")]
        config_path: PathBuf,
        /// How far back to crawl, for example 7d or 2w. Must stay within
        /// Loki's reject_old_samples_max_age and retention_period.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        since: Duration,
        /// Solana RPC endpoint; falls back to the RPC_URL env var.
        #[arg(long, value_name = "URL")]
        rpc_url: Option<String>,
        #[arg(long, default_value = "http://loki:3100", value_name = "URL")]
        loki_url: String,
        /// Maximum backfill depth to enforce instead of probing the Loki
        /// /config endpoint; required when pushing through Alloy or another
        /// endpoint that does not expose its limits.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        loki_max_age: Option<Duration>,
    },
    /// Generate Grafana alerting and dashboard provisioning from the deployment config.
    GenerateAlerting {
        #[arg(default_value = "microscope.toml", value_name = "CONFIG")]
        config_path: PathBuf,
        #[arg(
            default_value = "grafana/provisioning/alerting",
            value_name = "OUTPUT_DIR"
        )]
        alerting_output_dir: PathBuf,
        #[arg(
            default_value = "grafana/dashboards",
            value_name = "DASHBOARD_OUTPUT_DIR"
        )]
        dashboard_output_dir: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn parses_run_with_default_config() {
        let cli = Cli::try_parse_from(["microscope-indexer", "run"]).unwrap();

        let Command::Run { config_path } = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(config_path, Path::new("microscope.toml"));
    }

    #[test]
    fn parses_generate_alerting_paths() {
        let cli = Cli::try_parse_from([
            "microscope-indexer",
            "generate-alerting",
            "deployment.toml",
            "/tmp/alerting",
            "/tmp/dashboards",
        ])
        .unwrap();

        let Command::GenerateAlerting {
            config_path,
            alerting_output_dir,
            dashboard_output_dir,
        } = cli.command
        else {
            panic!("expected generate-alerting command");
        };
        assert_eq!(config_path, Path::new("deployment.toml"));
        assert_eq!(alerting_output_dir, Path::new("/tmp/alerting"));
        assert_eq!(dashboard_output_dir, Path::new("/tmp/dashboards"));
    }

    #[test]
    fn parses_backfill_with_defaults() {
        let cli = Cli::try_parse_from(["microscope-indexer", "backfill", "--since", "7d"]).unwrap();

        let Command::Backfill {
            config_path,
            since,
            rpc_url,
            loki_url,
            loki_max_age,
        } = cli.command
        else {
            panic!("expected backfill command");
        };
        assert_eq!(config_path, Path::new("microscope.toml"));
        assert_eq!(since, std::time::Duration::from_secs(7 * 86_400));
        assert_eq!(rpc_url, None);
        assert_eq!(loki_url, "http://loki:3100");
        assert_eq!(loki_max_age, None);
    }

    #[test]
    fn parses_backfill_with_an_explicit_loki_max_age() {
        let cli = Cli::try_parse_from([
            "microscope-indexer",
            "backfill",
            "--since",
            "7d",
            "--loki-max-age",
            "30d",
        ])
        .unwrap();

        let Command::Backfill { loki_max_age, .. } = cli.command else {
            panic!("expected backfill command");
        };
        assert_eq!(
            loki_max_age,
            Some(std::time::Duration::from_secs(30 * 86_400))
        );
    }

    #[test]
    fn rejects_backfill_without_since() {
        assert!(Cli::try_parse_from(["microscope-indexer", "backfill"]).is_err());
    }

    #[test]
    fn rejects_unknown_subcommands() {
        assert!(Cli::try_parse_from(["microscope-indexer", "generate-alreting"]).is_err());
    }
}
