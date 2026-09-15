mod alerting;
mod backfill;
mod cli;
mod config;
mod dashboard;
mod datasource;
mod delivered;
#[cfg(program_events)]
mod events;
mod health;
mod instructions;
mod logging;
mod multisig;
mod processor;
mod rpc_polling;
mod shipped;
mod telemetry;

use std::{path::Path, sync::Arc, time::Duration};

use carbon_core::{
    datasource::Datasource,
    error::CarbonResult,
    filter::{DeduplicationFilter, Filter},
    pipeline::{Pipeline, PipelineBuilder, ShutdownStrategy},
};
use carbon_log_metrics::LogMetrics;
use carbon_program_decoder::{ProgramDecoder, PROGRAM_ID};
use carbon_squads_smart_account_decoder::SquadsSmartAccountProgramDecoder;
use carbon_squads_v3_decoder::SquadsMplDecoder;
use carbon_squads_v4_decoder::SquadsMultisigProgramDecoder;
use clap::Parser;
use cli::{Cli, Command};
use config::{Config, DatasourceMode, MultisigVersion};
use multisig::{SmartAccountProcessor, SquadsV3Processor, SquadsV4Processor, VerifiedMultisig};
use processor::EventProcessor;
use sha2::{Digest, Sha256};

const BUILD_IDL_SHA256: &str = include_str!("../../program-decoder/.microscope-idl.sha256");

// reqwest's `rustls-no-provider` ships none, and Err means one is already installed.
pub(crate) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn verify_decoder_target(config: &Config, config_path: &Path) {
    if config.program_id != PROGRAM_ID.to_string() {
        panic!(
            "config program_id {} does not match the generated decoder's program id {} \
             (run `just generate` after changing microscope.toml or the IDL)",
            config.program_id, PROGRAM_ID
        );
    }
    let idl_path = config.resolved_idl_path(config_path);
    let idl = std::fs::read(&idl_path)
        .unwrap_or_else(|err| panic!("failed to read IDL {}: {err:?}", idl_path.display()));
    if !idl_matches_decoder(&idl) {
        panic!(
            "IDL {} does not match the IDL the decoder was generated from \
             (run `just generate` after changing microscope.toml or the IDL, or run an image \
             built from this IDL)",
            idl_path.display()
        );
    }
}

fn idl_matches_decoder(idl: &[u8]) -> bool {
    format!("{:x}", Sha256::digest(idl)) == BUILD_IDL_SHA256.trim()
}

fn deduplication_filters() -> Vec<Box<dyn Filter>> {
    // TTL must comfortably exceed the maximum RPC poll interval (300s), or a
    // slow poller re-emits transactions Yellowstone already delivered.
    vec![Box::new(DeduplicationFilter::new(Duration::from_secs(
        15 * 60,
    )))]
}

fn with_instruction_pipes(
    builder: PipelineBuilder,
    multisig: Option<VerifiedMultisig>,
) -> PipelineBuilder {
    let builder = builder.instruction_with_filters(
        ProgramDecoder,
        EventProcessor::default(),
        deduplication_filters(),
    );
    let builder = builder.transaction::<shipped::NoInstructions, _>(shipped::ShippedProcessor);
    let Some(multisig) = multisig else {
        return builder;
    };
    match multisig.version {
        MultisigVersion::V3 => builder.instruction_with_filters(
            SquadsMplDecoder,
            SquadsV3Processor::new(multisig.vault_address, multisig.state_address),
            deduplication_filters(),
        ),
        MultisigVersion::V4 => builder.instruction_with_filters(
            SquadsMultisigProgramDecoder,
            SquadsV4Processor::new(multisig.vault_address, multisig.state_address),
            deduplication_filters(),
        ),
        MultisigVersion::V5 => builder.instruction_with_filters(
            SquadsSmartAccountProgramDecoder,
            SmartAccountProcessor::new(multisig.vault_address, multisig.state_address),
            deduplication_filters(),
        ),
    }
}

fn verify_configured_multisig(config: &Config) -> anyhow::Result<Option<VerifiedMultisig>> {
    let Some(multisig_config) = config.multisig.as_ref() else {
        return Ok(None);
    };
    let verified = multisig::verify(
        config
            .multisig_vault_pubkey()
            .expect("the multisig section is present and validated"),
        config
            .multisig_state_pubkey()
            .expect("the multisig section is present and validated"),
        multisig_config.version,
    )?;
    log::info!(
        "verified configured Squads {} vault {} and state account {}",
        verified.version,
        verified.vault_address,
        verified.state_address
    );
    Ok(Some(verified))
}

/// Streaming datasources recover their own disconnects at best, never their own
/// restarts, so the checkpointed poller stays wired in behind every one of them.
/// The stream is recorded and the poller skips what it already delivered, so
/// every streaming mode gets the same duplicate suppression.
fn with_rpc_recovery(
    builder: PipelineBuilder,
    stream: impl Datasource + 'static,
    config: &Config,
    multisig_state_address: Option<solana_pubkey::Pubkey>,
    stream_name: &str,
) -> (PipelineBuilder, ShutdownStrategy) {
    let delivered = delivered::DeliveredSignatures::new();
    match rpc_polling::RpcPollingDatasource::from_env(
        PROGRAM_ID,
        multisig_state_address,
        Duration::from_secs(config.datasource.poll_interval_seconds),
        config.datasource.replay_window_slots,
        Some(delivered.clone()),
    ) {
        Some(recovery) => {
            log::info!("RPC_URL is configured; enabling {stream_name} gap recovery");
            telemetry::set_rpc_recovery_enabled(true);
            // The poller skips signatures recorded here, so pending updates
            // must still reach the pipeline on shutdown.
            (
                builder
                    .datasource(datasource::RecordingDatasource::new(stream, delivered))
                    .datasource(recovery),
                ShutdownStrategy::ProcessPending,
            )
        }
        None => {
            log::warn!(
                "RPC_URL is not configured; {stream_name} restart and disconnect gaps cannot be recovered automatically"
            );
            telemetry::set_rpc_recovery_enabled(false);
            (builder.datasource(stream), ShutdownStrategy::Immediate)
        }
    }
}

#[tokio::main]
async fn main() -> CarbonResult<()> {
    let config_path = match Cli::parse().command {
        Command::GenerateAlerting {
            config_path,
            alerting_output_dir,
            dashboard_output_dir,
        } => {
            let config = Config::load(&config_path).unwrap_or_else(|err| {
                panic!("failed to load config {}: {err:?}", config_path.display())
            });
            verify_decoder_target(&config, &config_path);
            verify_configured_multisig(&config)
                .unwrap_or_else(|error| panic!("invalid configured multisig: {error:#}"));
            let alerting_output =
                alerting::generate(&config, &alerting_output_dir).unwrap_or_else(|err| {
                    panic!("failed to generate Grafana alerting config: {err:?}")
                });
            let dashboard_output = dashboard::generate(&config, &dashboard_output_dir)
                .unwrap_or_else(|err| panic!("failed to generate Grafana dashboard: {err:?}"));
            println!("generated {}", alerting_output.display());
            println!("generated {}", dashboard_output.display());
            return Ok(());
        }
        Command::Backfill {
            config_path,
            since,
            rpc_url,
            loki_url,
            loki_max_age,
        } => {
            return run_backfill(config_path, since, rpc_url, loki_url, loki_max_age).await;
        }
        Command::Run { config_path } => config_path,
    };

    logging::init();
    install_crypto_provider();

    let config = Config::load(&config_path)
        .unwrap_or_else(|err| panic!("failed to load config {}: {err:?}", config_path.display()));
    verify_decoder_target(&config, &config_path);

    log::info!(
        "microscope-indexer starting: program_id={} idl_path={} multisig_vault={} multisig_version={} datasource={} alert_rules={}",
        config.program_id,
        config.idl_path,
        config
            .multisig
            .as_ref()
            .map(|multisig| multisig.vault_address.as_str())
            .unwrap_or("none"),
        config
            .multisig
            .as_ref()
            .map(|multisig| multisig.version.as_str())
            .unwrap_or("none"),
        config.datasource.mode.as_str(),
        config.alert_rules.len(),
    );

    telemetry::install();
    health::serve().await;
    let multisig = verify_configured_multisig(&config)
        .unwrap_or_else(|error| panic!("invalid configured multisig: {error:#}"));
    let multisig_state_address = multisig.map(|verified| verified.state_address);

    let builder = Pipeline::builder().metrics(Arc::new(LogMetrics::new()));
    let (builder, shutdown_strategy) = match config.datasource.mode {
        DatasourceMode::Yellowstone => with_rpc_recovery(
            builder,
            datasource::yellowstone(&config.program_id, multisig_state_address),
            &config,
            multisig_state_address,
            "Yellowstone",
        ),
        DatasourceMode::Rpc => {
            telemetry::set_rpc_recovery_enabled(true);
            health::expect_poll_heartbeat(config.datasource.rpc_poll_stale_after_seconds());
            (
                builder.datasource(rpc_polling::RpcPollingDatasource::require_from_env(
                    PROGRAM_ID,
                    multisig_state_address,
                    Duration::from_secs(config.datasource.poll_interval_seconds),
                    config.datasource.replay_window_slots,
                )),
                ShutdownStrategy::ProcessPending,
            )
        }
    };
    let mut pipeline = with_instruction_pipes(builder, multisig)
        .shutdown_strategy(shutdown_strategy)
        .build()?;
    health::mark_started();
    pipeline.run().await
}

async fn run_backfill(
    config_path: std::path::PathBuf,
    since: Duration,
    rpc_url: Option<String>,
    loki_url: String,
    loki_max_age: Option<Duration>,
) -> CarbonResult<()> {
    let (record_sink, record_receiver) = tokio::sync::mpsc::unbounded_channel();
    logging::init_with_record_sink(record_sink.clone());
    install_crypto_provider();

    let config = Config::load(&config_path)
        .unwrap_or_else(|err| panic!("failed to load config {}: {err:?}", config_path.display()));
    verify_decoder_target(&config, &config_path);
    let rpc_url = rpc_url
        .or_else(|| std::env::var("RPC_URL").ok().filter(|url| !url.is_empty()))
        .unwrap_or_else(|| panic!("pass --rpc-url or set the RPC_URL env var"));

    let http_client = reqwest::Client::new();
    let loki_limit = match loki_max_age {
        Some(limit) => limit,
        None => backfill::loki_backfill_limit(&http_client, &loki_url)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to read Loki limits from {loki_url}: {err:?} \
                     (endpoints without a /config endpoint, such as Alloy or hosted Loki, \
                     require --loki-max-age)"
                )
            }),
    };
    if since > loki_limit {
        panic!(
            "--since {}h exceeds what Loki keeps ({}h): raise reject_old_samples_max_age and \
             retention_period in loki/loki-config.yml, or pass a matching --loki-max-age",
            since.as_secs() / 3_600,
            loki_limit.as_secs() / 3_600,
        );
    }

    let started_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock predates the unix epoch")
        .as_secs();
    let cutoff_unix = started_unix as i64 - since.as_secs() as i64;
    let multisig = verify_configured_multisig(&config)
        .unwrap_or_else(|error| panic!("invalid configured multisig: {error:#}"));
    log::info!(
        "backfilling {} and {} since unix time {cutoff_unix} into {loki_url}",
        config.program_id,
        multisig
            .map(|verified| verified.vault_address.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );

    let pusher = tokio::spawn(backfill::push_records(
        http_client,
        loki_url,
        started_unix.to_string(),
        record_receiver,
    ));
    let crawl_failures = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut crawl_addresses = vec![PROGRAM_ID];
    if let Some(verified) = multisig {
        crawl_addresses.push(verified.state_address);
    }
    crawl_addresses.sort_unstable();
    crawl_addresses.dedup();

    let mut builder = Pipeline::builder();
    for address in crawl_addresses {
        builder = builder.datasource(backfill::BackfillDatasource::new(
            rpc_url.clone(),
            address,
            cutoff_unix,
            crawl_failures.clone(),
        ));
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    let pipeline_result = with_instruction_pipes(builder, multisig)
        .datasource_cancellation_token(cancellation.clone())
        .shutdown_strategy(ShutdownStrategy::ProcessPending)
        .build()?
        .run()
        .await;

    if cancellation.is_cancelled() {
        pusher.abort();
        panic!(
            "backfill interrupted before reaching the cutoff; nothing was pushed to Loki, \
             rerun to completion"
        );
    }
    let failed_crawls = crawl_failures.load(std::sync::atomic::Ordering::Relaxed);
    let failed_updates =
        carbon_failed_updates(&carbon_core::metrics::MetricsRegistry::global().snapshot());
    if pipeline_result.is_err() || failed_crawls > 0 || failed_updates > 0 {
        pusher.abort();
        pipeline_result?;
        panic!(
            "backfill incomplete: {failed_crawls} transaction(s) failed to fetch or convert \
             and {failed_updates} update(s) failed during processing; nothing was pushed to \
             Loki, rerun once the RPC endpoint recovers or check the logged errors if \
             failures persist"
        );
    }

    let _ = record_sink.send(None);
    let pushed = pusher
        .await
        .expect("record pusher panicked")
        .unwrap_or_else(|err| panic!("failed to push records to Loki: {err:?}"));
    log::info!("backfill complete: pushed {pushed} records");
    Ok(())
}

fn carbon_failed_updates(snapshot: &carbon_core::metrics::MetricsSnapshot) -> u64 {
    snapshot
        .counters
        .iter()
        .find(|(name, _, _)| *name == "carbon_updates_failed_total")
        .map(|(_, _, value)| *value)
        .expect("carbon pipeline registers the failed-updates counter")
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use carbon_core::{
        datasource::{DatasourceId, Update},
        filter::{FilterContext, FilterResult},
        instruction::{NestedInstruction, NestedInstructions},
        pipeline::Pipeline,
        transaction::TransactionMetadata,
        transformers::extract_instructions_with_metadata,
    };
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;
    use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;

    use super::{deduplication_filters, with_instruction_pipes, MultisigVersion, VerifiedMultisig};

    fn redelivered_instruction() -> NestedInstruction {
        let fetched: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .expect("fixture deserializes");
        let signature = Signature::from_str(
            "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
        )
        .unwrap();
        let Update::Transaction(update) = crate::backfill::transaction_update(signature, fetched)
            .expect("fixture converts to an update")
        else {
            panic!("expected a transaction update");
        };
        let metadata: Arc<TransactionMetadata> =
            Arc::new((*update.clone()).try_into().expect("metadata converts"));
        let (metadata, instruction) = extract_instructions_with_metadata(&metadata, &update)
            .expect("instructions extract")
            .into_iter()
            .next()
            .expect("the fixture carries instructions");

        NestedInstruction {
            metadata,
            instruction,
            inner_instructions: NestedInstructions::default(),
        }
    }

    #[test]
    fn a_transaction_redelivered_within_the_dedup_ttl_is_emitted_once() {
        let filters = deduplication_filters();
        let datasource_id = DatasourceId::new_named("test");
        let context = FilterContext {
            datasource_id: &datasource_id,
        };
        let instruction = redelivered_instruction();

        assert_eq!(
            filters[0].filter_instruction(&context, &instruction),
            FilterResult::Accept
        );
        assert_eq!(
            filters[0].filter_instruction(&context, &instruction),
            FilterResult::Reject,
            "RPC reconciliation rediscovers what Yellowstone already delivered"
        );
    }

    #[test]
    fn rejects_an_idl_the_decoder_was_not_generated_from() {
        let config_path = std::path::Path::new("../../microscope.toml");
        let config = super::Config::load(config_path).expect("the repository is configured");
        let built_idl = std::fs::read(config.resolved_idl_path(config_path))
            .expect("the configured IDL is readable");

        assert!(super::idl_matches_decoder(&built_idl));
        assert!(!super::idl_matches_decoder(b"{}"));
    }

    #[test]
    fn reads_failed_updates_from_the_metrics_snapshot() {
        let snapshot = carbon_core::metrics::MetricsSnapshot {
            counters: vec![("carbon_updates_failed_total", "help", 3)],
            gauges: vec![],
            histograms: vec![],
        };

        assert_eq!(super::carbon_failed_updates(&snapshot), 3);
    }

    #[test]
    fn registers_only_the_configured_squads_decoder() {
        for version in [
            MultisigVersion::V3,
            MultisigVersion::V4,
            MultisigVersion::V5,
        ] {
            let builder = with_instruction_pipes(
                Pipeline::builder(),
                Some(VerifiedMultisig {
                    vault_address: Pubkey::new_unique(),
                    state_address: Pubkey::new_unique(),
                    version,
                }),
            );

            assert_eq!(builder.instruction_pipes.len(), 2);
        }
    }

    /// Carbon runs transaction pipes after every instruction pipe, making the
    /// journal a completion boundary rather than a first-instruction one.
    #[test]
    fn journals_shipped_transactions_from_a_transaction_pipe() {
        let builder = with_instruction_pipes(Pipeline::builder(), None);

        assert_eq!(builder.transaction_pipes.len(), 1);
    }

    struct CancellingDatasource;

    #[async_trait::async_trait]
    impl carbon_core::datasource::Datasource for CancellingDatasource {
        async fn consume(
            &self,
            _id: carbon_core::datasource::DatasourceId,
            _sender: tokio::sync::mpsc::Sender<(
                carbon_core::datasource::Update,
                carbon_core::datasource::DatasourceId,
            )>,
            cancellation_token: tokio_util::sync::CancellationToken,
        ) -> carbon_core::error::CarbonResult<()> {
            cancellation_token.cancel();
            Ok(())
        }

        fn update_types(&self) -> Vec<carbon_core::datasource::UpdateType> {
            vec![carbon_core::datasource::UpdateType::Transaction]
        }
    }

    #[tokio::test]
    async fn a_cancelled_run_still_returns_ok_so_the_gate_must_check_the_token() {
        let token = tokio_util::sync::CancellationToken::new();

        let result = Pipeline::builder()
            .datasource(CancellingDatasource)
            .datasource_cancellation_token(token.clone())
            .shutdown_strategy(carbon_core::pipeline::ShutdownStrategy::ProcessPending)
            .build()
            .expect("pipeline builds")
            .run()
            .await;

        assert!(result.is_ok());
        assert!(token.is_cancelled());
    }
}
