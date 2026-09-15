use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, Context};
use async_trait::async_trait;
use carbon_core::{
    datasource::{Datasource, DatasourceId, TransactionUpdate, Update, UpdateType},
    error::{CarbonResult, Error as CarbonError},
    transformers::transaction_metadata_from_original_meta,
};
use futures::StreamExt;
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    nonblocking::rpc_client::RpcClient,
    rpc_client::GetConfirmedSignaturesForAddress2Config,
    rpc_config::RpcTransactionConfig,
    rpc_custom_error::JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION,
    rpc_request::RpcError,
};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_status::{
    option_serializer::OptionSerializer, EncodedConfirmedTransactionWithStatusMeta,
    UiTransactionEncoding,
};
use tokio::sync::mpsc::UnboundedReceiver;

const SIGNATURE_PAGE_LIMIT: usize = 1_000;
const CONCURRENT_TRANSACTION_FETCHES: usize = 5;
const TRANSACTION_FETCH_ATTEMPTS: usize = 3;
const PUSH_BATCH_LIMIT: usize = 500;
const PUSH_RATE_LIMIT_ATTEMPTS: usize = 10;
const LOKI_STREAM_LABEL: &str = "microscope-indexer";

pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let mut total = Duration::ZERO;
    let mut digits = String::new();
    let mut parsed_any = false;

    for character in value.trim().chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        let amount: u64 = digits
            .parse()
            .map_err(|_| format!("expected a number before '{character}' in {value:?}"))?;
        let unit_seconds = match character {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 24 * 60 * 60,
            'w' => 7 * 24 * 60 * 60,
            other => return Err(format!("unsupported duration unit '{other}' in {value:?}")),
        };
        total += Duration::from_secs(amount * unit_seconds);
        digits.clear();
        parsed_any = true;
    }

    if !digits.is_empty() || !parsed_any {
        return Err(format!(
            "invalid duration {value:?}: use digits with s, m, h, d, or w units, for example 7d or 2w12h"
        ));
    }
    Ok(total)
}

/// Loki rejects samples older than its configured limits, so a backfill
/// deeper than the tightest limit must fail before crawling anything.
pub async fn loki_backfill_limit(
    client: &reqwest::Client,
    loki_url: &str,
) -> anyhow::Result<Duration> {
    let config_url = format!("{}/config", loki_url.trim_end_matches('/'));
    let rendered_config = client
        .get(&config_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("failed to fetch Loki runtime config from {config_url}"))?
        .text()
        .await
        .context("failed to read Loki runtime config body")?;

    let mut limit = config_duration(&rendered_config, "reject_old_samples_max_age")
        .context("reject_old_samples_max_age missing from Loki config")?;
    if config_flag(&rendered_config, "reject_old_samples") == Some(false) {
        limit = Duration::MAX;
    }

    let retention = config_duration(&rendered_config, "retention_period")
        .context("retention_period missing from Loki config")?;
    if !retention.is_zero() {
        limit = limit.min(retention);
    }
    Ok(limit)
}

fn config_duration(rendered_config: &str, key: &str) -> Option<Duration> {
    let value = config_value(rendered_config, key)?;
    if value == "0" || value == "0s" {
        return Some(Duration::ZERO);
    }
    parse_duration(&value).ok()
}

fn config_flag(rendered_config: &str, key: &str) -> Option<bool> {
    config_value(rendered_config, key)?.parse().ok()
}

fn config_value(rendered_config: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    rendered_config
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
}

/// Receives the structured records captured from the processors and ships
/// them to Loki backdated to each transaction's block time. `None` marks the
/// end of the stream.
///
/// Loki only accepts entries within its out-of-order window (max_chunk_age/2,
/// one hour by default) of a stream's newest entry, and the live indexer
/// keeps the shared stream's head at the present, so the whole crawl is
/// buffered and pushed oldest-first into a per-run stream instead.
pub async fn push_records(
    client: reqwest::Client,
    loki_url: String,
    run_label: String,
    mut records: UnboundedReceiver<Option<String>>,
) -> anyhow::Result<u64> {
    let push_url = format!("{}/loki/api/v1/push", loki_url.trim_end_matches('/'));
    let mut buffered: Vec<(i64, String)> = Vec::new();
    let mut skipped_without_block_time: u64 = 0;

    while let Some(record) = records.recv().await {
        let Some(line) = record else { break };
        match block_time_of(&line) {
            Some(block_time) => buffered.push((block_time, line)),
            None => skipped_without_block_time += 1,
        }
    }
    if skipped_without_block_time > 0 {
        bail!(
            "backfill incomplete: {skipped_without_block_time} record(s) lack a block time; \
             nothing was pushed to Loki, rerun against an RPC endpoint with complete block data"
        );
    }
    buffered.sort_by_key(|(block_time, _)| *block_time);

    let mut pushed = 0;
    for batch in buffered.chunks(PUSH_BATCH_LIMIT) {
        pushed += flush(&client, &push_url, &run_label, batch).await?;
    }
    Ok(pushed)
}

fn block_time_of(line: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("block_time")?
        .as_i64()
}

async fn flush(
    client: &reqwest::Client,
    push_url: &str,
    run_label: &str,
    batch: &[(i64, String)],
) -> anyhow::Result<u64> {
    if batch.is_empty() {
        return Ok(0);
    }
    let values: Vec<[String; 2]> = batch
        .iter()
        .map(|(block_time, line)| {
            [
                (*block_time as i128 * 1_000_000_000).to_string(),
                line.clone(),
            ]
        })
        .collect();
    let payload = serde_json::json!({
        "streams": [{
            "stream": {
                "service_name": LOKI_STREAM_LABEL,
                "backfill_run": run_label,
            },
            "values": values,
        }]
    });

    for attempt in 1..=PUSH_RATE_LIMIT_ATTEMPTS {
        let response = client
            .post(push_url)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("failed to push {} records to {push_url}", batch.len()))?;
        if response.status().is_success() {
            return Ok(batch.len() as u64);
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == PUSH_RATE_LIMIT_ATTEMPTS {
            bail!(
                "Loki rejected a push of {} records: {status} {body}",
                batch.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
    }
    unreachable!("push retry loop always returns or bails");
}

/// Crawls an account's signatures backwards through RPC until the block time
/// crosses the cutoff, feeding each transaction through the same decoding
/// pipeline as the live indexer. The sender drops when the crawl completes,
/// which lets the pipeline shut down on its own.
pub struct BackfillDatasource {
    rpc_url: String,
    account: Pubkey,
    cutoff_unix: i64,
    crawl_failures: Arc<AtomicU32>,
}

impl BackfillDatasource {
    pub fn new(
        rpc_url: String,
        account: Pubkey,
        cutoff_unix: i64,
        crawl_failures: Arc<AtomicU32>,
    ) -> Self {
        Self {
            rpc_url,
            account,
            cutoff_unix,
            crawl_failures,
        }
    }
}

#[async_trait]
impl Datasource for BackfillDatasource {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: tokio::sync::mpsc::Sender<(Update, DatasourceId)>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> CarbonResult<()> {
        let rpc_client =
            RpcClient::new_with_commitment(self.rpc_url.clone(), CommitmentConfig::confirmed());
        let mut before: Option<Signature> = None;
        let fetched_count = AtomicU64::new(0);

        loop {
            if cancellation_token.is_cancelled() {
                return Ok(());
            }

            let page = rpc_client
                .get_signatures_for_address_with_config(
                    &self.account,
                    GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: None,
                        limit: Some(SIGNATURE_PAGE_LIMIT),
                        commitment: Some(CommitmentConfig::confirmed()),
                    },
                )
                .await
                .map_err(|error| {
                    self.crawl_failures.fetch_add(1, Ordering::Relaxed);
                    CarbonError::FailedToConsumeDatasource(format!(
                        "failed to fetch signatures for {}: {error}",
                        self.account
                    ))
                })?;
            if page.is_empty() {
                break;
            }
            let (signatures, reached_cutoff) = select_signatures(
                page.iter()
                    .map(|info| (info.block_time, info.signature.as_str())),
                self.cutoff_unix,
            )
            .map_err(|error| {
                self.crawl_failures.fetch_add(1, Ordering::Relaxed);
                CarbonError::FailedToConsumeDatasource(error)
            })?;
            let page_end = signatures.last().copied();
            futures::stream::iter(signatures)
                .map(|signature| fetch_transaction(&rpc_client, signature))
                .buffer_unordered(CONCURRENT_TRANSACTION_FETCHES)
                .for_each(|fetched| async {
                    let (signature, transaction) = match fetched {
                        Ok(fetched) => fetched,
                        Err(failure) => {
                            log::warn!("failed to fetch a transaction: {}", failure.error);
                            self.crawl_failures.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    };
                    let fetched_so_far = fetched_count.fetch_add(1, Ordering::Relaxed) + 1;
                    if fetched_so_far.is_multiple_of(100) {
                        log::info!(
                            "backfill {}: fetched {fetched_so_far} transactions so far",
                            self.account
                        );
                    }
                    let Some(update) =
                        counted_transaction_update(signature, transaction, &self.crawl_failures)
                    else {
                        return;
                    };
                    if let Err(error) = sender.send((update, id.clone())).await {
                        log::warn!("failed to queue backfill update: {error}");
                    }
                })
                .await;

            if reached_cutoff || page.len() < SIGNATURE_PAGE_LIMIT || page_end.is_none() {
                break;
            }
            before = page_end;
        }

        log::info!(
            "backfill {}: finished after {} transactions",
            self.account,
            fetched_count.load(Ordering::Relaxed)
        );
        Ok(())
    }

    fn update_types(&self) -> Vec<UpdateType> {
        vec![UpdateType::Transaction]
    }
}

/// A signature the RPC returns unparsed must abort the crawl: the page's last
/// signature is the pagination cursor, so skipping it would restart the crawl
/// from the newest transaction forever.
fn select_signatures<'a>(
    page: impl Iterator<Item = (Option<i64>, &'a str)>,
    cutoff_unix: i64,
) -> Result<(Vec<Signature>, bool), String> {
    let mut signatures = Vec::new();
    for (block_time, encoded_signature) in page {
        if block_time.is_some_and(|time| time < cutoff_unix) {
            return Ok((signatures, true));
        }
        let signature = Signature::from_str(encoded_signature).map_err(|error| {
            format!("RPC returned invalid signature {encoded_signature}: {error}")
        })?;
        signatures.push(signature);
    }
    Ok((signatures, false))
}

#[derive(Debug)]
pub(crate) struct FetchError {
    pub(crate) permanent: bool,
    pub(crate) error: ClientError,
}

pub(crate) async fn fetch_transaction(
    rpc_client: &RpcClient,
    signature: Signature,
) -> Result<(Signature, EncodedConfirmedTransactionWithStatusMeta), FetchError> {
    for attempt in 1..=TRANSACTION_FETCH_ATTEMPTS {
        match rpc_client
            .get_transaction_with_config(
                &signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(1),
                },
            )
            .await
        {
            Ok(transaction) => return Ok((signature, transaction)),
            Err(error) => {
                if fetch_error_is_permanent(&error) {
                    return Err(FetchError {
                        permanent: true,
                        error,
                    });
                }
                if attempt == TRANSACTION_FETCH_ATTEMPTS {
                    return Err(FetchError {
                        permanent: false,
                        error,
                    });
                }
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
            }
        }
    }
    unreachable!("the fetch retry loop always returns");
}

fn fetch_error_is_permanent(error: &ClientError) -> bool {
    matches!(
        &*error.kind,
        ClientErrorKind::RpcError(RpcError::RpcResponseError {
            code: JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION,
            ..
        })
    )
}

pub(crate) fn counted_transaction_update(
    signature: Signature,
    fetched: EncodedConfirmedTransactionWithStatusMeta,
    conversion_failures: &AtomicU32,
) -> Option<Update> {
    let update = transaction_update(signature, fetched);
    if update.is_none() {
        conversion_failures.fetch_add(1, Ordering::Relaxed);
    }
    update
}

pub(crate) fn transaction_update(
    signature: Signature,
    fetched: EncodedConfirmedTransactionWithStatusMeta,
) -> Option<Update> {
    let transaction = fetched.transaction;
    let Some(meta) = transaction.meta else {
        log::warn!("skipping transaction {signature}: missing meta");
        return None;
    };
    let Some(decoded_transaction) = transaction.transaction.decode() else {
        log::warn!("skipping transaction {signature}: failed to decode");
        return None;
    };
    if !matches!(meta.inner_instructions, OptionSerializer::Some(_)) {
        log::warn!(
            "skipping transaction {signature}: inner instructions not recorded by the RPC endpoint"
        );
        return None;
    }
    if !matches!(meta.log_messages, OptionSerializer::Some(_)) {
        log::warn!(
            "skipping transaction {signature}: log messages not recorded by the RPC endpoint"
        );
        return None;
    }
    let Ok(meta) = transaction_metadata_from_original_meta(meta) else {
        log::warn!("skipping transaction {signature}: malformed meta");
        return None;
    };

    Some(Update::Transaction(Box::new(TransactionUpdate {
        signature,
        transaction: decoded_transaction,
        meta,
        is_vote: false,
        slot: fetched.slot,
        index: None,
        block_time: fetched.block_time,
        block_hash: None,
    })))
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc, time::Duration};

    use base64::Engine as _;
    use carbon_core::{
        datasource::Update, instruction::InstructionDecoder, transaction::TransactionMetadata,
        transformers::extract_instructions_with_metadata,
    };
    use carbon_squads_v4_decoder::{
        instructions::SquadsMultisigProgramInstruction, SquadsMultisigProgramDecoder,
        PROGRAM_ID as SQUADS_V4_PROGRAM_ID,
    };
    use solana_client::{
        client_error::{ClientError, ClientErrorKind},
        rpc_custom_error::{
            JSON_RPC_SERVER_ERROR_NODE_UNHEALTHY,
            JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION,
        },
        rpc_request::{RpcError, RpcResponseErrorData},
    };
    use solana_instruction::AccountMeta;
    use solana_message::{v1, VersionedMessage};
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;
    use solana_transaction_status::{
        EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction,
        EncodedTransactionWithStatusMeta, TransactionBinaryEncoding, TransactionStatusMeta,
        UiTransactionStatusMeta,
    };

    use super::{
        block_time_of, config_duration, config_flag, fetch_error_is_permanent, parse_duration,
        select_signatures, transaction_update,
    };
    use crate::multisig::common::AddressResolver;

    const FIXTURES: &[(&str, &str)] = &[
        (
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
            "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
        ),
        (
            include_str!("../tests/fixtures/squads_v4_vault_execute_jul9.json"),
            "2KMkE4KfmvHRyd975JLqhrnGyhGVKLRUpR1HVPsran6XkCmhQ5XST99p5coFkCwNrPRrodLD1CGtL7XT6yjJQEX6",
        ),
    ];

    struct FixtureDatasource;

    #[async_trait::async_trait]
    impl carbon_core::datasource::Datasource for FixtureDatasource {
        async fn consume(
            &self,
            id: carbon_core::datasource::DatasourceId,
            sender: tokio::sync::mpsc::Sender<(Update, carbon_core::datasource::DatasourceId)>,
            _cancellation_token: tokio_util::sync::CancellationToken,
        ) -> carbon_core::error::CarbonResult<()> {
            for (fixture, signature) in FIXTURES {
                let fetched: EncodedConfirmedTransactionWithStatusMeta =
                    serde_json::from_str(fixture).expect("fixture deserializes");
                let signature = Signature::from_str(signature).unwrap();
                let update = transaction_update(signature, fetched).expect("fixture converts");
                sender
                    .send((update, id.clone()))
                    .await
                    .expect("update queues");
            }
            Ok(())
        }

        fn update_types(&self) -> Vec<carbon_core::datasource::UpdateType> {
            vec![carbon_core::datasource::UpdateType::Transaction]
        }
    }

    struct CaptureLogger(std::sync::Mutex<Vec<String>>);

    static CAPTURED: CaptureLogger = CaptureLogger(std::sync::Mutex::new(Vec::new()));

    impl log::Log for CaptureLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            if record.target().starts_with("microscope::") {
                self.0.lock().unwrap().push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    #[tokio::test]
    async fn ignores_a_backfilled_vault_execute_from_an_unrelated_multisig() {
        let _ = log::set_logger(&CAPTURED);
        log::set_max_level(log::LevelFilter::Info);
        let known_vault = Pubkey::from_str("DXtFpbPjcn2hxPnw79x1Pfoj35vXh5AsWBkS37YnXMVv").unwrap();
        let known_state = Pubkey::from_str("4CxQs26DewQ1KaCHfyyjktkYjndNdUqCJvVdygtJFwcJ").unwrap();

        carbon_core::pipeline::Pipeline::builder()
            .datasource(FixtureDatasource)
            .instruction_with_filters(
                SquadsMultisigProgramDecoder,
                crate::multisig::SquadsV4Processor::new(known_vault, known_state),
                vec![Box::new(carbon_core::filter::DeduplicationFilter::new(
                    Duration::from_secs(5 * 60),
                ))],
            )
            .shutdown_strategy(carbon_core::pipeline::ShutdownStrategy::ProcessPending)
            .build()
            .expect("pipeline builds")
            .run()
            .await
            .expect("pipeline runs");

        let records = CAPTURED.0.lock().unwrap();
        let multisig_records: Vec<_> = records
            .iter()
            .filter(|line| line.contains("\"multisig_activity\""))
            .collect();
        assert_eq!(
            multisig_records.len(),
            1,
            "only the configured multisig should produce a record: {multisig_records:?}"
        );
        assert!(multisig_records[0].contains("transaction_executed"));
        assert!(multisig_records[0].contains(FIXTURES[0].1));
        assert!(!multisig_records[0].contains(FIXTURES[1].1));

        let record: serde_json::Value =
            serde_json::from_str(multisig_records[0]).expect("record is structured json");
        assert!(record["instruction_path"].is_string());
        assert!(record["instruction_index"].is_u64());
        assert!(record["stack_height"].is_u64());
    }

    #[test]
    fn decodes_a_mainnet_squads_v4_vault_execute_through_the_backfill_path() {
        let fetched: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .expect("fixture deserializes");
        let signature = Signature::from_str(
            "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
        )
        .unwrap();

        let update = transaction_update(signature, fetched).expect("fixture converts to an update");
        let Update::Transaction(update) = update else {
            panic!("expected a transaction update");
        };
        let metadata: Arc<TransactionMetadata> =
            Arc::new((*update.clone()).try_into().expect("metadata converts"));
        let instructions =
            extract_instructions_with_metadata(&metadata, &update).expect("instructions extract");

        let squads_instruction = instructions
            .iter()
            .map(|(_, instruction)| instruction)
            .find(|instruction| instruction.program_id == SQUADS_V4_PROGRAM_ID)
            .expect("transaction contains a squads v4 instruction");

        let decoded = SquadsMultisigProgramDecoder
            .decode_instruction(squads_instruction)
            .expect("squads v4 instruction decodes");
        let state_address = match decoded {
            SquadsMultisigProgramInstruction::VaultTransactionExecute { accounts, .. } => {
                accounts.multisig
            }
            _ => panic!("expected a vault transaction execute"),
        };

        let known_vault = Pubkey::from_str("DXtFpbPjcn2hxPnw79x1Pfoj35vXh5AsWBkS37YnXMVv").unwrap();
        let matched = AddressResolver::new(known_vault, state_address, "v4")
            .match_state(state_address)
            .expect("decoded instruction uses the resolved multisig state");
        assert_eq!(matched.state_address, state_address);
        assert_eq!(matched.configured_address_kind, "default_vault");
    }

    #[test]
    fn decodes_a_v1_transaction_through_the_backfill_path() {
        let payer = Pubkey::new_unique();
        let program_id = Pubkey::new_unique();
        let account = Pubkey::new_unique();

        let message = v1::Message::try_compile_with_config(
            &payer,
            &[solana_instruction::Instruction {
                program_id,
                accounts: vec![AccountMeta::new(account, false)],
                data: vec![7, 7, 7],
            }],
            solana_hash::Hash::new_unique(),
            v1::TransactionConfig {
                compute_unit_limit: Some(30_000),
                loaded_accounts_data_size_limit: Some(200_000),
                priority_fee: Some(5_000),
                heap_size: None,
            },
        )
        .expect("v1 message compiles");

        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); message.header.num_required_signatures as usize],
            message: VersionedMessage::V1(message),
        };
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(wincode::serialize(&transaction).expect("v1 transaction serializes"));

        let fetched = EncodedConfirmedTransactionWithStatusMeta {
            slot: 42,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Binary(encoded, TransactionBinaryEncoding::Base64),
                meta: Some(UiTransactionStatusMeta::from(TransactionStatusMeta {
                    inner_instructions: Some(vec![]),
                    log_messages: Some(vec![]),
                    ..Default::default()
                })),
                version: None,
            },
            block_time: Some(1),
            transaction_index: None,
        };

        let update = transaction_update(Signature::default(), fetched)
            .expect("a v1 transaction converts to an update");
        let Update::Transaction(update) = update else {
            panic!("expected a transaction update");
        };

        let VersionedMessage::V1(message) = &update.transaction.message else {
            panic!("expected the v1 message to survive decoding");
        };
        assert_eq!(message.config.compute_unit_limit, Some(30_000));
        assert_eq!(message.config.priority_fee, Some(5_000));

        let metadata: Arc<TransactionMetadata> =
            Arc::new((*update.clone()).try_into().expect("metadata converts"));
        let instructions =
            extract_instructions_with_metadata(&metadata, &update).expect("instructions extract");
        let instruction = instructions
            .iter()
            .map(|(_, instruction)| instruction)
            .find(|instruction| instruction.program_id == program_id)
            .expect("the v1 transaction exposes its program instruction");
        assert_eq!(instruction.data, vec![7, 7, 7]);
        assert_eq!(instruction.accounts[0].pubkey, account);
    }

    #[test]
    fn parses_single_and_compound_durations() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            parse_duration("2w12h").unwrap(),
            Duration::from_secs(14 * 86_400 + 12 * 3_600)
        );
    }

    #[test]
    fn rejects_malformed_durations() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("d7").is_err());
        assert!(parse_duration("7y").is_err());
    }

    #[test]
    fn aborts_the_crawl_when_a_page_holds_an_unparsable_signature() {
        let valid = Signature::from([7; 64]).to_string();

        let error = select_signatures(
            [(Some(10), valid.as_str()), (Some(9), "not-a-signature")].into_iter(),
            0,
        )
        .expect_err("an unparsable signature must not be skipped");

        assert!(error.contains("not-a-signature"), "{error}");
    }

    #[test]
    fn stops_a_page_at_the_cutoff() {
        let newer = Signature::from([1; 64]);
        let newer_encoded = newer.to_string();
        let older_encoded = Signature::from([2; 64]).to_string();

        let (signatures, reached_cutoff) = select_signatures(
            [
                (Some(100), newer_encoded.as_str()),
                (Some(50), older_encoded.as_str()),
            ]
            .into_iter(),
            80,
        )
        .unwrap();

        assert!(reached_cutoff);
        assert_eq!(signatures, vec![newer]);
    }

    #[test]
    fn reads_limits_from_rendered_loki_config() {
        let rendered = "limits_config:\n  reject_old_samples: true\n  reject_old_samples_max_age: 2w\n  retention_period: 336h\n";
        assert_eq!(
            config_duration(rendered, "reject_old_samples_max_age"),
            Some(Duration::from_secs(14 * 86_400))
        );
        assert_eq!(
            config_duration(rendered, "retention_period"),
            Some(Duration::from_secs(336 * 3_600))
        );
        assert_eq!(config_flag(rendered, "reject_old_samples"), Some(true));
    }

    #[test]
    fn extracts_block_time_from_structured_records() {
        assert_eq!(
            block_time_of(r#"{"kind":"program_event","block_time":1722300000}"#),
            Some(1_722_300_000)
        );
        assert_eq!(
            block_time_of(r#"{"kind":"program_event","block_time":null}"#),
            None
        );
        assert_eq!(block_time_of("not json"), None);
    }

    #[test]
    fn counts_a_conversion_failure_when_meta_is_missing() {
        let mut fetched: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .expect("fixture deserializes");
        fetched.transaction.meta = None;
        let signature = Signature::from_str(
            "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
        )
        .unwrap();
        let conversion_failures = std::sync::atomic::AtomicU32::new(0);

        let update = super::counted_transaction_update(signature, fetched, &conversion_failures);

        assert!(update.is_none());
        assert_eq!(
            conversion_failures.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn counts_a_conversion_failure_when_inner_instructions_are_not_recorded() {
        let mut fetched: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .expect("fixture deserializes");
        fetched
            .transaction
            .meta
            .as_mut()
            .unwrap()
            .inner_instructions =
            solana_transaction_status::option_serializer::OptionSerializer::None;
        let signature = Signature::from_str(
            "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
        )
        .unwrap();
        let conversion_failures = std::sync::atomic::AtomicU32::new(0);

        let update = super::counted_transaction_update(signature, fetched, &conversion_failures);

        assert!(update.is_none());
        assert_eq!(
            conversion_failures.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn counts_a_conversion_failure_when_log_messages_are_not_recorded() {
        let mut fetched: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .expect("fixture deserializes");
        fetched.transaction.meta.as_mut().unwrap().log_messages =
            solana_transaction_status::option_serializer::OptionSerializer::None;
        let signature = Signature::from_str(
            "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
        )
        .unwrap();
        let conversion_failures = std::sync::atomic::AtomicU32::new(0);

        let update = super::counted_transaction_update(signature, fetched, &conversion_failures);

        assert!(update.is_none());
        assert_eq!(
            conversion_failures.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    fn client_error(kind: ClientErrorKind) -> ClientError {
        ClientError {
            request: None,
            kind: Box::new(kind),
        }
    }

    fn rpc_response_error(code: i64) -> ClientErrorKind {
        ClientErrorKind::RpcError(RpcError::RpcResponseError {
            code,
            message: "server error".to_string(),
            data: RpcResponseErrorData::Empty,
        })
    }

    #[test]
    fn retries_refusals_timeouts_rate_limits_and_garbage_bodies() {
        let retryable = [
            ClientErrorKind::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            ClientErrorKind::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            ClientErrorKind::RpcError(RpcError::RpcRequestError(
                "429 Too Many Requests".to_string(),
            )),
            rpc_response_error(JSON_RPC_SERVER_ERROR_NODE_UNHEALTHY),
            ClientErrorKind::SerdeJson(
                serde_json::from_str::<serde_json::Value>("<html>502 Bad Gateway</html>")
                    .unwrap_err(),
            ),
        ];

        for kind in retryable {
            let error = client_error(kind);
            assert!(
                !fetch_error_is_permanent(&error),
                "{error} must stay retryable so the cursor waits instead of advancing past the transaction"
            );
        }
    }

    #[test]
    fn gives_up_only_on_transaction_versions_this_client_can_never_decode() {
        let error = client_error(rpc_response_error(
            JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION,
        ));

        assert!(fetch_error_is_permanent(&error));
    }

    #[tokio::test]
    async fn fails_the_push_when_a_record_lacks_a_block_time() {
        let (sink, receiver) = tokio::sync::mpsc::unbounded_channel();
        sink.send(Some(
            r#"{"kind":"program_event","block_time":null}"#.to_string(),
        ))
        .unwrap();
        sink.send(None).unwrap();

        crate::install_crypto_provider();
        let result = super::push_records(
            reqwest::Client::new(),
            "http://127.0.0.1:9".to_string(),
            "test-run".to_string(),
            receiver,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn counts_a_crawl_failure_when_the_rpc_endpoint_is_unreachable() {
        use carbon_core::datasource::Datasource;

        let crawl_failures = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let datasource = super::BackfillDatasource::new(
            "http://127.0.0.1:9".to_string(),
            Pubkey::new_unique(),
            0,
            crawl_failures.clone(),
        );
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);

        let result = datasource
            .consume(
                carbon_core::datasource::DatasourceId::new_named("test"),
                sender,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(crawl_failures.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
