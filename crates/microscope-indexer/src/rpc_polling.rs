mod checkpoint;
mod progress;
mod runtime;
mod signatures;

use std::{
    collections::{btree_map::Entry, BTreeSet, HashMap, HashSet},
    env,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context};
use carbon_core::datasource::{DatasourceId, Update};
use futures::{stream, StreamExt};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;
use tokio::sync::mpsc::Sender;

use self::{
    progress::{
        advance_ready_cursors, signature_is_ready, FailureDisposition, PollingState,
        SignatureContext,
    },
    runtime::{CursorAheadOfHead, HistoryUnavailable},
    signatures::{discover_signatures, AddressCursor},
};
use crate::{
    backfill::{self, FetchError},
    delivered::DeliveredSignatures,
    shipped, telemetry,
};

const CONCURRENT_TRANSACTION_FETCHES: usize = 5;
const MAX_TRANSACTION_FAILURE_ATTEMPTS: u32 = 5;
const HISTORY_BOUNDARY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
const STATE_FILE_NAME: &str = "rpc-polling.json";
const SHIPPED_JOURNAL_FILE_NAME: &str = "shipped.log";

#[derive(Debug)]
pub struct RpcPollingDatasource {
    rpc_url: String,
    program_id: Pubkey,
    multisig_state_address: Option<Pubkey>,
    poll_interval: Duration,
    replay_window_slots: u64,
    checkpoint_path: PathBuf,
    delivered: Option<DeliveredSignatures>,
}

impl RpcPollingDatasource {
    pub fn from_env(
        program_id: Pubkey,
        multisig_state_address: Option<Pubkey>,
        poll_interval: Duration,
        replay_window_slots: u64,
        delivered: Option<DeliveredSignatures>,
    ) -> Option<Self> {
        let rpc_url = env::var("RPC_URL").ok().filter(|url| !url.is_empty())?;
        if !valid_rpc_url(&rpc_url) {
            panic!("RPC_URL must be a valid HTTP(S) URL");
        }
        let state_dir = env::var_os("MICROSCOPE_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".microscope-state"));
        shipped::install(state_dir.join(SHIPPED_JOURNAL_FILE_NAME));

        Some(Self {
            rpc_url,
            program_id,
            multisig_state_address,
            poll_interval,
            replay_window_slots,
            checkpoint_path: state_dir.join(STATE_FILE_NAME),
            delivered,
        })
    }

    pub fn require_from_env(
        program_id: Pubkey,
        multisig_state_address: Option<Pubkey>,
        poll_interval: Duration,
        replay_window_slots: u64,
    ) -> Self {
        Self::from_env(
            program_id,
            multisig_state_address,
            poll_interval,
            replay_window_slots,
            None,
        )
        .unwrap_or_else(|| panic!("RPC_URL env var must be set when datasource.mode is rpc"))
    }

    fn monitored_addresses(&self) -> Vec<Pubkey> {
        let mut addresses = vec![self.program_id];
        if let Some(state_address) = self.multisig_state_address {
            addresses.push(state_address);
        }
        addresses.sort_unstable();
        addresses.dedup();
        addresses
    }

    async fn absorb_delivered_signatures(&self, state: &mut PollingState) {
        let Some(delivered) = &self.delivered else {
            return;
        };
        let deliveries = delivered.drain().await;
        if deliveries.is_empty() {
            return;
        }
        for (signature, slot) in &deliveries {
            state.record_recent_signature(*signature, *slot);
        }
        telemetry::record_rpc_yellowstone_deliveries(deliveries.len() as u64);
        log::debug!(
            "recorded {} transaction(s) Yellowstone already delivered; the poller will not re-emit them",
            deliveries.len()
        );
    }

    async fn poll(
        &self,
        rpc_client: &RpcClient,
        sender: &Sender<(Update, DatasourceId)>,
        id: &DatasourceId,
        state: &mut PollingState,
        addresses: &[Pubkey],
    ) -> anyhow::Result<PollOutcome> {
        // Before the first fallible call: a poll that cannot reach the endpoint
        // must still absorb what Yellowstone delivered while it was failing.
        self.absorb_delivered_signatures(state).await;

        let head_slot = rpc_client
            .get_slot_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("failed to read the confirmed RPC slot")?;

        if let Some(ahead) =
            cursor_ahead_of_head(&state.cursors, head_slot, self.replay_window_slots)
        {
            return Err(ahead.into());
        }

        let mut initialized = false;
        for address in addresses {
            if let Entry::Vacant(entry) = state.cursors.entry(address.to_string()) {
                let replay_slot = head_slot.saturating_sub(self.replay_window_slots);
                entry.insert(AddressCursor {
                    scanned_slot: replay_slot,
                });
                initialized = true;
                log::info!(
                    "RPC polling for {address} starts at slot {replay_slot}, replaying up to confirmed slot {head_slot}"
                );
            }
        }

        let mut history_boundary_verified = false;
        let boundary_checked_at = Instant::now();
        if state.history_boundary_check_due(boundary_checked_at, HISTORY_BOUNDARY_CHECK_INTERVAL) {
            if let Some(minimum_scanned_slot) = state
                .cursors
                .values()
                .map(|cursor| cursor.scanned_slot)
                .min()
            {
                let first_available_slot = rpc_client
                    .get_first_available_block()
                    .await
                    .context("failed to read the RPC history boundary")?;
                state.record_history_boundary_check(boundary_checked_at);
                if first_available_slot > minimum_scanned_slot.saturating_add(1) {
                    return Err(HistoryUnavailable {
                        first_available_slot,
                        checkpoint_slot: minimum_scanned_slot,
                    }
                    .into());
                }
                history_boundary_verified = true;
            }
        }

        let mut batches = Vec::with_capacity(addresses.len());
        let mut signature_contexts = HashMap::<Signature, SignatureContext>::new();
        let mut first_available_slot = None;
        for address in addresses {
            let cursor = state
                .cursors
                .get(&address.to_string())
                .expect("all monitored addresses have cursors")
                .clone();
            let batch = discover_signatures(
                rpc_client,
                *address,
                &cursor,
                head_slot,
                self.replay_window_slots,
            )
            .await?;
            if !batch.reached_replay_floor {
                let boundary = match first_available_slot {
                    Some(boundary) => boundary,
                    None => {
                        let boundary = rpc_client
                            .get_first_available_block()
                            .await
                            .context("failed to read the RPC history boundary")?;
                        first_available_slot = Some(boundary);
                        boundary
                    }
                };
                if boundary > cursor.scanned_slot.saturating_add(1) {
                    return Err(HistoryUnavailable {
                        first_available_slot: boundary,
                        checkpoint_slot: cursor.scanned_slot,
                    }
                    .into());
                }
                history_boundary_verified = true;
            }
            for discovered in &batch.signatures {
                if let Some(context) = signature_contexts.get_mut(&discovered.signature) {
                    if context.slot != discovered.slot {
                        bail!(
                            "RPC returned signature {} at conflicting slots {} and {}",
                            discovered.signature,
                            context.slot,
                            discovered.slot
                        );
                    }
                    context.addresses.insert(address.to_string());
                } else {
                    signature_contexts.insert(
                        discovered.signature,
                        SignatureContext {
                            slot: discovered.slot,
                            addresses: BTreeSet::from([address.to_string()]),
                        },
                    );
                }
            }
            batches.push(batch);
        }

        let replayed_signatures = signature_contexts
            .keys()
            .filter(|signature| state.has_recent_signature(signature))
            .count();
        if replayed_signatures > 0 {
            log::debug!(
                "RPC replay skipped {replayed_signatures} transaction(s) already recorded in the durable checkpoint"
            );
        }
        let fetched = stream::iter(signatures_to_fetch(&signature_contexts, state))
            .map(|signature| async move {
                (
                    signature,
                    backfill::fetch_transaction(rpc_client, signature).await,
                )
            })
            .buffer_unordered(CONCURRENT_TRANSACTION_FETCHES)
            .collect::<Vec<_>>()
            .await;

        let mut successful_updates = Vec::with_capacity(fetched.len());
        let mut retrying_signatures = HashSet::new();
        for (signature, fetched_transaction) in fetched {
            match process_fetched_transaction(state, signature, fetched_transaction) {
                TransactionPollOutcome::Ready { slot, update } => {
                    successful_updates.push((signature, slot, update));
                }
                TransactionPollOutcome::TransientFailure { error } => {
                    let context = &signature_contexts[&signature];
                    telemetry::record_rpc_transaction_failure();
                    retrying_signatures.insert(signature);
                    log::warn!(
                        "RPC transaction {signature} at slot {} failed a transient fetch; cursors for {} remain unchanged until it succeeds: {error:#}",
                        context.slot,
                        context.addresses.iter().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
                TransactionPollOutcome::Poisoned { error, disposition } => {
                    let context = &signature_contexts[&signature];
                    let addresses = context.addresses.iter().cloned().collect::<Vec<_>>();
                    let error = format!("{error:#}");
                    telemetry::record_rpc_transaction_failure();
                    if disposition.quarantined {
                        state.record_recent_signature(signature, context.slot);
                        if disposition.newly_quarantined {
                            telemetry::record_rpc_transaction_quarantine();
                            log::error!(
                                "quarantining undecodable RPC transaction {signature} at slot {} after {} confirmations; cursors for {} may now advance without a decoded record: {error}",
                                context.slot,
                                disposition.attempts,
                                addresses.join(", ")
                            );
                        }
                    } else {
                        retrying_signatures.insert(signature);
                        log::warn!(
                            "RPC transaction {signature} at slot {} cannot be decoded ({}/{} confirmations before quarantine); cursors for {} remain unchanged: {error}",
                            context.slot,
                            disposition.attempts,
                            MAX_TRANSACTION_FAILURE_ATTEMPTS,
                            addresses.join(", ")
                        );
                    }
                }
            }
        }

        let blocked_addresses = retrying_signatures
            .iter()
            .flat_map(|signature| &signature_contexts[signature].addresses)
            .cloned()
            .collect::<BTreeSet<_>>();
        let discovered_successes = successful_updates.len();
        successful_updates.retain(|(signature, _, _)| {
            signature_is_ready(*signature, &signature_contexts, &blocked_addresses)
        });
        let deferred_transactions = discovered_successes - successful_updates.len();
        if deferred_transactions > 0 {
            log::warn!(
                "deferring {deferred_transactions} RPC transaction(s) while cursors for {} retry failed transactions",
                blocked_addresses.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }

        successful_updates.sort_by_key(|(_, slot, _)| *slot);
        let queued_transactions = successful_updates.len();
        for (signature, slot, update) in successful_updates {
            sender
                .send((update, id.clone()))
                .await
                .map_err(|_| anyhow!("pipeline update channel closed"))?;
            state.record_recent_signature(signature, slot);
        }

        let advanced_with_activity = advance_ready_cursors(state, &batches, &retrying_signatures);
        telemetry::set_rpc_quarantined_transactions(state.quarantined_transaction_count() as u64);

        let minimum_scanned_slot = state
            .cursors
            .values()
            .map(|cursor| cursor.scanned_slot)
            .min()
            .unwrap_or(head_slot);
        state.retain_signatures_after(unreachable_below(
            minimum_scanned_slot,
            self.replay_window_slots,
        ));
        telemetry::set_rpc_recent_signatures(state.recent_signature_count() as u64);
        telemetry::record_rpc_poll_success(
            head_slot,
            minimum_scanned_slot,
            queued_transactions as u64,
        );
        Ok(PollOutcome {
            queued_transactions,
            checkpoint_urgent: initialized || advanced_with_activity,
            history_boundary_verified,
        })
    }
}

/// The cursor tracks the head observed by a previous poll, and backends
/// behind one load-balanced endpoint report heads a few slots apart, so a
/// lagging backend can legitimately answer below the cursor. One replay
/// window is the tolerance for that skew; only a cursor beyond it is
/// implausible rather than skew.
fn cursor_ahead_of_head(
    cursors: &std::collections::BTreeMap<String, AddressCursor>,
    head_slot: u64,
    replay_window_slots: u64,
) -> Option<CursorAheadOfHead> {
    let implausible_beyond = head_slot.saturating_add(replay_window_slots);
    cursors
        .iter()
        .find(|(_, cursor)| cursor.scanned_slot > implausible_beyond)
        .map(|(address, cursor)| CursorAheadOfHead {
            address: address.clone(),
            cursor_slot: cursor.scanned_slot,
            head_slot,
        })
}

/// A restart rewinds twice: checkpoint load rewinds the stored cursor by one
/// replay window, discovery by another. Suppression dropped between those
/// floors covers transactions still rediscoverable as duplicates.
pub(super) fn unreachable_below(slot: u64, replay_window_slots: u64) -> u64 {
    slot.saturating_sub(2 * replay_window_slots)
}

fn signatures_to_fetch(
    contexts: &HashMap<Signature, SignatureContext>,
    state: &PollingState,
) -> Vec<Signature> {
    contexts
        .keys()
        .copied()
        .filter(|signature| !state.has_recent_signature(signature))
        .collect()
}

#[derive(Debug)]
struct PollOutcome {
    queued_transactions: usize,
    checkpoint_urgent: bool,
    history_boundary_verified: bool,
}

fn valid_rpc_url(value: &str) -> bool {
    reqwest::Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
}

enum TransactionPollOutcome {
    Ready {
        slot: u64,
        update: Update,
    },
    TransientFailure {
        error: anyhow::Error,
    },
    Poisoned {
        error: anyhow::Error,
        disposition: FailureDisposition,
    },
}

fn process_fetched_transaction(
    state: &mut PollingState,
    signature: Signature,
    fetched_transaction: Result<(Signature, EncodedConfirmedTransactionWithStatusMeta), FetchError>,
) -> TransactionPollOutcome {
    let poisoned = |state: &mut PollingState, error| TransactionPollOutcome::Poisoned {
        error,
        disposition: state.record_poison_failure(signature, MAX_TRANSACTION_FAILURE_ATTEMPTS),
    };
    match fetched_transaction {
        Ok((_, transaction)) => {
            let slot = transaction.slot;
            match convert_transaction(signature, transaction) {
                Ok(update) => {
                    state.clear_poison_failure(&signature);
                    TransactionPollOutcome::Ready { slot, update }
                }
                Err(error) => poisoned(state, error),
            }
        }
        Err(failure) if failure.permanent => poisoned(state, anyhow::Error::new(failure.error)),
        Err(failure) => TransactionPollOutcome::TransientFailure {
            error: anyhow::Error::new(failure.error),
        },
    }
}

fn convert_transaction(
    signature: Signature,
    transaction: EncodedConfirmedTransactionWithStatusMeta,
) -> anyhow::Result<Update> {
    backfill::transaction_update(signature, transaction)
        .ok_or_else(|| anyhow!("transaction {signature} could not be converted"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap, HashSet},
        time::Duration,
    };

    use solana_client::{
        client_error::{ClientError, ClientErrorKind},
        rpc_custom_error::JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION,
        rpc_request::RpcError,
    };
    use solana_pubkey::Pubkey;
    use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;

    use carbon_core::datasource::DatasourceId;
    use solana_commitment_config::CommitmentConfig;

    use super::{
        advance_ready_cursors, convert_transaction, cursor_ahead_of_head,
        process_fetched_transaction, signatures, signatures_to_fetch, valid_rpc_url, AddressCursor,
        DeliveredSignatures, PollingState, RpcClient, RpcPollingDatasource, TransactionPollOutcome,
    };
    use crate::{backfill::FetchError, rpc_polling::progress::SignatureContext};

    #[test]
    fn accepts_only_http_rpc_urls() {
        assert!(valid_rpc_url("https://rpc.example.com"));
        assert!(valid_rpc_url("http://127.0.0.1:8899"));
        assert!(!valid_rpc_url("rpc.example.com"));
        assert!(!valid_rpc_url("ftp://rpc.example.com"));
    }

    #[test]
    fn monitors_the_program_and_multisig_state_account() {
        let program_id = Pubkey::new_unique();
        let state_address = Pubkey::new_unique();
        let datasource = RpcPollingDatasource {
            rpc_url: "https://rpc.example.com".to_string(),
            program_id,
            multisig_state_address: Some(state_address),
            poll_interval: Duration::from_secs(5),
            replay_window_slots: 300,
            checkpoint_path: ".microscope-state/rpc-polling.json".into(),
            delivered: None,
        };
        let mut expected = vec![program_id, state_address];
        expected.sort_unstable();

        assert_eq!(datasource.monitored_addresses(), expected);
    }

    fn signature(value: u64) -> solana_signature::Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        solana_signature::Signature::from(bytes)
    }

    fn client_error(kind: ClientErrorKind) -> ClientError {
        ClientError {
            request: None,
            kind: Box::new(kind),
        }
    }

    #[test]
    fn does_not_refetch_signatures_restored_from_the_checkpoint() {
        let already_seen = signature(1);
        let unseen = signature(2);
        let address = Pubkey::new_unique().to_string();
        let contexts = HashMap::from([
            (
                already_seen,
                SignatureContext {
                    slot: 100,
                    addresses: BTreeSet::from([address.clone()]),
                },
            ),
            (
                unseen,
                SignatureContext {
                    slot: 101,
                    addresses: BTreeSet::from([address]),
                },
            ),
        ]);
        let mut state = PollingState::default();
        state.record_recent_signature(already_seen, 100);

        assert_eq!(signatures_to_fetch(&contexts, &state), vec![unseen]);
    }

    fn polling_datasource(delivered: Option<DeliveredSignatures>) -> RpcPollingDatasource {
        RpcPollingDatasource {
            rpc_url: "https://rpc.example.com".to_string(),
            program_id: Pubkey::new_unique(),
            multisig_state_address: None,
            poll_interval: Duration::from_secs(5),
            replay_window_slots: 300,
            checkpoint_path: ".microscope-state/rpc-polling.json".into(),
            delivered,
        }
    }

    /// The transaction was already logged once from the Yellowstone stream, so
    /// fetching it again would duplicate the record and re-fire its alerts.
    #[tokio::test]
    async fn does_not_refetch_what_yellowstone_already_delivered() {
        let delivered_by_yellowstone = signature(1);
        let unseen = signature(2);
        let address = Pubkey::new_unique().to_string();
        let contexts = HashMap::from([
            (
                delivered_by_yellowstone,
                SignatureContext {
                    slot: 100,
                    addresses: BTreeSet::from([address.clone()]),
                },
            ),
            (
                unseen,
                SignatureContext {
                    slot: 101,
                    addresses: BTreeSet::from([address.clone()]),
                },
            ),
        ]);
        let delivered = DeliveredSignatures::new();
        delivered.record(delivered_by_yellowstone, 100);
        let datasource = polling_datasource(Some(delivered));
        let mut state = PollingState::default();
        state
            .cursors
            .insert(address.clone(), AddressCursor { scanned_slot: 99 });

        datasource.absorb_delivered_signatures(&mut state).await;

        assert_eq!(signatures_to_fetch(&contexts, &state), vec![unseen]);
        // A suppressed signature must not hold the cursor behind it.
        let batches = vec![signatures::AddressBatch {
            address: Pubkey::from_str_const(&address),
            signatures: vec![],
            next_cursor: AddressCursor { scanned_slot: 101 },
            reached_replay_floor: true,
        }];
        advance_ready_cursors(&mut state, &batches, &HashSet::new());

        assert_eq!(state.cursors[&address].scanned_slot, 101);
    }

    #[tokio::test]
    async fn absorbs_deliveries_without_a_reachable_endpoint() {
        let delivered_by_yellowstone = signature(3);
        let delivered = DeliveredSignatures::new();
        delivered.record(delivered_by_yellowstone, 100);
        let datasource = polling_datasource(Some(delivered));
        let mut state = PollingState::default();

        let poll = datasource
            .poll(
                &RpcClient::new_with_commitment(
                    "http://127.0.0.1:1".to_string(),
                    CommitmentConfig::confirmed(),
                ),
                &tokio::sync::mpsc::channel(1).0,
                &DatasourceId::new_unique(),
                &mut state,
                &[Pubkey::new_unique()],
            )
            .await;

        assert!(poll.is_err(), "an unreachable endpoint must fail the poll");
        assert!(state.has_recent_signature(&delivered_by_yellowstone));
    }

    #[test]
    fn transient_fetch_failures_never_quarantine() {
        let transaction = signature(1);
        let mut state = PollingState::default();

        for _ in 0..(2 * super::MAX_TRANSACTION_FAILURE_ATTEMPTS) {
            let outcome = process_fetched_transaction(
                &mut state,
                transaction,
                Err(FetchError {
                    permanent: false,
                    error: client_error(ClientErrorKind::Custom("connection reset".to_string())),
                }),
            );
            let TransactionPollOutcome::TransientFailure { error } = outcome else {
                panic!("a transient fetch failure must stay retryable");
            };
            assert!(error.to_string().contains("connection reset"));
        }
        assert_eq!(state.quarantined_transaction_count(), 0);
    }

    #[test]
    fn permanently_unfetchable_transactions_reach_bounded_quarantine() {
        let transaction = signature(1);
        let mut state = PollingState::default();

        for expected_attempt in 1..=5 {
            let outcome = process_fetched_transaction(
                &mut state,
                transaction,
                Err(FetchError {
                    permanent: true,
                    error: client_error(ClientErrorKind::RpcError(RpcError::RpcResponseError {
                        code: JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION,
                        message: "unsupported transaction version".to_string(),
                        data: solana_client::rpc_request::RpcResponseErrorData::Empty,
                    })),
                }),
            );
            let TransactionPollOutcome::Poisoned { error, disposition } = outcome else {
                panic!("a permanent fetch failure must count toward quarantine");
            };
            assert!(error
                .to_string()
                .contains("unsupported transaction version"));
            assert_eq!(disposition.attempts, expected_attempt);
            assert_eq!(disposition.quarantined, expected_attempt == 5);
            assert_eq!(disposition.newly_quarantined, expected_attempt == 5);
        }
        assert_eq!(state.quarantined_transaction_count(), 1);
    }

    #[test]
    fn undecodable_transactions_count_toward_quarantine() {
        let mut transaction: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .unwrap();
        transaction.transaction.meta = None;
        let mut state = PollingState::default();

        let outcome =
            process_fetched_transaction(&mut state, signature(1), Ok((signature(1), transaction)));

        let TransactionPollOutcome::Poisoned { error, disposition } = outcome else {
            panic!("an undecodable transaction must count toward quarantine");
        };
        assert!(error.to_string().contains("could not be converted"));
        assert_eq!(disposition.attempts, 1);
        assert!(!disposition.quarantined);
    }

    #[test]
    fn rejects_transactions_that_cannot_be_converted() {
        let mut transaction: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_str(
            include_str!("../tests/fixtures/squads_v4_vault_execute.json"),
        )
        .unwrap();
        transaction.transaction.meta = None;

        let result = convert_transaction(signature(1), transaction);

        let Err(error) = result else {
            panic!("malformed transaction should be rejected");
        };
        assert!(error.to_string().contains("could not be converted"));
    }

    fn cursors_at(scanned_slot: u64) -> std::collections::BTreeMap<String, AddressCursor> {
        std::collections::BTreeMap::from([(
            Pubkey::new_unique().to_string(),
            AddressCursor { scanned_slot },
        )])
    }

    #[test]
    fn reports_a_cursor_implausibly_past_the_confirmed_head() {
        let address = Pubkey::new_unique().to_string();
        let cursors = std::collections::BTreeMap::from([(
            address.clone(),
            AddressCursor {
                scanned_slot: 4_000_000_000,
            },
        )]);

        let ahead = cursor_ahead_of_head(&cursors, 300_000_000, 300).expect("cursor is ahead");

        assert_eq!(ahead.address, address);
        assert_eq!(ahead.cursor_slot, 4_000_000_000);
        assert_eq!(ahead.head_slot, 300_000_000);
    }

    /// Ties the retention floor to the rewinds a restart actually performs, so
    /// a change to either one fails here rather than as silent duplicates.
    #[test]
    fn keeps_suppression_for_everything_a_restart_can_rediscover() {
        let replay_window_slots = 300;
        let checkpoint_slot = 300_000_000;
        let restored = AddressCursor {
            scanned_slot: checkpoint_slot - replay_window_slots,
        };

        let discovery_floor = restored.scanned_slot - replay_window_slots;

        assert_eq!(
            super::unreachable_below(checkpoint_slot, replay_window_slots),
            discovery_floor
        );
    }

    #[test]
    fn accepts_a_cursor_at_the_confirmed_head() {
        assert!(cursor_ahead_of_head(&cursors_at(300_000_000), 300_000_000, 300).is_none());
    }

    #[test]
    fn accepts_a_head_behind_the_cursor_by_less_than_the_replay_window() {
        let cursors = cursors_at(300_000_000);

        assert!(cursor_ahead_of_head(&cursors, 300_000_000 - 40, 300).is_none());
        assert!(cursor_ahead_of_head(&cursors, 300_000_000 - 300, 300).is_none());
        assert!(cursor_ahead_of_head(&cursors, 300_000_000 - 301, 300).is_some());
    }
}
