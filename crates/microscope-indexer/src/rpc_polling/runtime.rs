use std::{fmt, time::Instant};

use async_trait::async_trait;
use carbon_core::{
    datasource::{Datasource, DatasourceId, Update, UpdateType},
    error::CarbonResult,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::{
    checkpoint::{CheckpointLoadOutcome, CheckpointMismatch, CheckpointStore},
    unreachable_below, PollingState, RpcPollingDatasource,
};
use crate::{delivered::DeliveredSignatures, shipped, telemetry};

const CHECKPOINT_LOAD_ATTEMPTS: u32 = 3;
const RECOVERY_DISABLE_AFTER_CONSECUTIVE: u32 = 3;

#[derive(Debug)]
pub(super) struct HistoryUnavailable {
    pub(super) first_available_slot: u64,
    pub(super) checkpoint_slot: u64,
}

impl fmt::Display for HistoryUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "RPC history starts at slot {}, after the durable polling checkpoint at slot {}; recovery requires an archival endpoint or manual backfill",
            self.first_available_slot, self.checkpoint_slot
        )
    }
}

impl std::error::Error for HistoryUnavailable {}

#[derive(Debug)]
pub(super) struct CursorAheadOfHead {
    pub(super) address: String,
    pub(super) cursor_slot: u64,
    pub(super) head_slot: u64,
}

impl fmt::Display for CursorAheadOfHead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the polling cursor for {} is at slot {}, ahead of the confirmed head at slot {}; every poll would report success while scanning nothing",
            self.address, self.cursor_slot, self.head_slot
        )
    }
}

impl std::error::Error for CursorAheadOfHead {}

#[async_trait]
impl Datasource for RpcPollingDatasource {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: Sender<(Update, DatasourceId)>,
        cancellation_token: CancellationToken,
    ) -> CarbonResult<()> {
        let rpc_client =
            RpcClient::new_with_commitment(self.rpc_url.clone(), CommitmentConfig::confirmed());
        let addresses = self.monitored_addresses();
        log::info!(
            "RPC polling monitors {}",
            addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        telemetry::initialize_rpc_polling();

        let genesis_hash = loop {
            match rpc_client.get_genesis_hash().await {
                Ok(genesis_hash) => break genesis_hash.to_string(),
                Err(error) => {
                    telemetry::record_rpc_poll_failure();
                    log::error!("failed to read RPC genesis hash: {error}");
                    tokio::select! {
                        _ = cancellation_token.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(self.poll_interval) => {}
                    }
                }
            }
        };
        let journal_identity = {
            let mut addresses = addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            addresses.sort();
            format!("{genesis_hash} {} {}", self.program_id, addresses.join(","))
        };
        let mut checkpoint = CheckpointStore::new(
            self.checkpoint_path.clone(),
            genesis_hash,
            self.program_id.to_string(),
            addresses.iter().map(ToString::to_string).collect(),
            self.replay_window_slots,
        );
        let loaded = {
            let mut attempts = 0;
            loop {
                match checkpoint.load().await {
                    Ok(loaded) => break loaded,
                    Err(error) if error.is::<CheckpointMismatch>() => {
                        shipped::discard(journal_identity.clone());
                        telemetry::record_rpc_checkpoint_failure();
                        return wait_with_recovery_disabled(
                            &cancellation_token,
                            self.delivered.as_ref(),
                            "checkpoint_load",
                            &format!("failed to load the RPC recovery checkpoint: {error:#}"),
                        )
                        .await;
                    }
                    Err(error) => {
                        attempts += 1;
                        telemetry::record_rpc_checkpoint_failure();
                        if attempts >= CHECKPOINT_LOAD_ATTEMPTS {
                            return wait_with_recovery_disabled(
                                &cancellation_token,
                                self.delivered.as_ref(),
                                "checkpoint_load",
                                &format!(
                                    "failed to load the RPC recovery checkpoint after {CHECKPOINT_LOAD_ATTEMPTS} attempts: {error:#}"
                                ),
                            )
                            .await;
                        }
                        log::warn!(
                            "failed to load the RPC recovery checkpoint (attempt {attempts}/{CHECKPOINT_LOAD_ATTEMPTS}): {error:#}"
                        );
                        tokio::select! {
                            _ = cancellation_token.cancelled() => return Ok(()),
                            _ = tokio::time::sleep(self.poll_interval) => {}
                        }
                    }
                }
            }
        };
        let mut state = PollingState::default();
        match loaded {
            CheckpointLoadOutcome::Missing => shipped::discard(journal_identity),
            CheckpointLoadOutcome::Corrupt {
                backup_path,
                reason,
            } => {
                telemetry::record_rpc_checkpoint_corrupt();
                log::error!(
                    "RPC recovery checkpoint was corrupt ({reason}); moved it to {} and starting from a fresh replay window",
                    backup_path.display()
                );
                shipped::discard(journal_identity);
            }
            CheckpointLoadOutcome::Loaded(snapshot) => {
                let resume_slot = snapshot
                    .cursors
                    .values()
                    .map(|cursor| cursor.scanned_slot)
                    .min()
                    .unwrap_or_default();
                log::info!(
                    "RPC polling resumes from durable slot {resume_slot} after applying a {}-slot replay window with {} recently seen transaction(s)",
                    self.replay_window_slots,
                    snapshot.recent_signatures.len()
                );
                state.cursors = snapshot.cursors;
                state.recent_signatures = snapshot.recent_signatures;
                state.restore_quarantined(snapshot.quarantined_signatures);
                telemetry::set_rpc_quarantined_transactions(
                    state.quarantined_transaction_count() as u64
                );

                // The journal holds exactly what shipped since that checkpoint:
                // the transactions its replay window would otherwise re-emit.
                let shipped = shipped::restore(journal_identity);
                if !shipped.is_empty() {
                    log::info!(
                        "{} transaction(s) shipped since the last checkpoint will not be re-emitted",
                        shipped.len()
                    );
                }
                for (signature, slot) in shipped {
                    state.record_recent_signature(signature, slot);
                }
            }
        }

        let mut checkpoint_urgent = false;
        let mut history_gaps = HistoryGapTracker::default();
        let mut cursors_ahead = HistoryGapTracker::default();
        let mut processed_state = (
            state.cursors.clone(),
            state.recent_signatures.clone(),
            state.quarantined_signatures().copied().collect(),
        );
        loop {
            if cancellation_token.is_cancelled() {
                return Ok(());
            }
            let started = Instant::now();

            match checkpoint.quarantined_file_count().await {
                Ok(files) => telemetry::set_rpc_checkpoint_quarantined_files(files),
                Err(error) => {
                    log::warn!("failed to count quarantined RPC checkpoints: {error:#}")
                }
            }

            if pipeline_drained(
                sender.capacity() == sender.max_capacity(),
                &carbon_core::metrics::MetricsRegistry::global().snapshot(),
            ) {
                processed_state = (
                    state.cursors.clone(),
                    state.recent_signatures.clone(),
                    state.quarantined_signatures().copied().collect(),
                );
            } else {
                log::debug!(
                    "holding the RPC recovery checkpoint at the last processed state while queued updates await processing"
                );
            }
            match checkpoint
                .save_if_due(
                    &processed_state.0,
                    &processed_state.1,
                    &processed_state.2,
                    checkpoint_urgent,
                )
                .await
            {
                Ok(true) => {
                    let checkpoint_slot = processed_state
                        .0
                        .values()
                        .map(|cursor| cursor.scanned_slot)
                        .min()
                        .unwrap_or_default();
                    telemetry::record_rpc_checkpoint_success(checkpoint_slot);
                    // Against the durable slot, never the in-memory cursor: a
                    // restart resumes from this checkpoint, so entries dropped
                    // ahead of it replay with nothing left to suppress them.
                    shipped::compact(unreachable_below(checkpoint_slot, self.replay_window_slots));
                }
                Ok(false) => {}
                Err(error) => {
                    telemetry::record_rpc_checkpoint_failure();
                    log::error!("failed to save RPC recovery checkpoint: {error:#}");
                }
            }

            match self
                .poll(&rpc_client, &sender, &id, &mut state, &addresses)
                .await
            {
                Ok(outcome) => {
                    checkpoint_urgent = outcome.checkpoint_urgent;
                    cursors_ahead.record_recovered();
                    if outcome.history_boundary_verified {
                        history_gaps.record_recovered();
                    }
                    if outcome.queued_transactions > 0 {
                        log::info!(
                            "RPC poll queued {} new transaction(s)",
                            outcome.queued_transactions
                        );
                    }
                }
                Err(error) if error.is::<CursorAheadOfHead>() => {
                    if cursors_ahead.record_gap() {
                        return wait_with_recovery_disabled(
                            &cancellation_token,
                            self.delivered.as_ref(),
                            "cursor_ahead_of_head",
                            &format!("RPC recovery cannot continue: {error:#}"),
                        )
                        .await;
                    }
                    log::warn!(
                        "{error:#}; recovery disables itself after {RECOVERY_DISABLE_AFTER_CONSECUTIVE} consecutive observations in case this response came from a lagging RPC node"
                    );
                }
                Err(error) if error.is::<HistoryUnavailable>() => {
                    telemetry::record_rpc_history_unavailable();
                    if history_gaps.record_gap() {
                        return wait_with_recovery_disabled(
                            &cancellation_token,
                            self.delivered.as_ref(),
                            "history_unavailable",
                            &format!("RPC recovery cannot continue: {error:#}"),
                        )
                        .await;
                    }
                    log::warn!(
                        "{error:#}; recovery disables itself after {RECOVERY_DISABLE_AFTER_CONSECUTIVE} consecutive observations in case this response came from a lagging RPC node"
                    );
                }
                Err(error) => {
                    telemetry::record_rpc_poll_failure();
                    log::error!("RPC poll failed: {error:#}");
                }
            }

            let delay = self.poll_interval.saturating_sub(started.elapsed());
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    fn update_types(&self) -> Vec<UpdateType> {
        vec![UpdateType::Transaction]
    }
}

/// Carbon dequeues an update before processing it, so an empty channel does
/// not prove the work finished.
fn pipeline_drained(channel_empty: bool, snapshot: &carbon_core::metrics::MetricsSnapshot) -> bool {
    if !channel_empty {
        return false;
    }
    let counter = |name: &str| {
        snapshot
            .counters
            .iter()
            .find(|(counter_name, _, _)| *counter_name == name)
            .map(|(_, _, value)| *value)
    };
    match (
        counter("carbon_updates_received_total"),
        counter("carbon_updates_processed_total"),
    ) {
        (Some(received), Some(processed)) => received == processed,
        _ => true,
    }
}

#[derive(Debug, Default)]
struct HistoryGapTracker {
    consecutive: u32,
}

impl HistoryGapTracker {
    fn record_gap(&mut self) -> bool {
        self.consecutive += 1;
        self.consecutive >= RECOVERY_DISABLE_AFTER_CONSECUTIVE
    }

    fn record_recovered(&mut self) {
        self.consecutive = 0;
    }
}

async fn wait_with_recovery_disabled(
    cancellation_token: &CancellationToken,
    delivered: Option<&DeliveredSignatures>,
    reason: &'static str,
    message: &str,
) -> CarbonResult<()> {
    // A parked poller emits nothing, so recorded deliveries can only pile up.
    if let Some(delivered) = delivered {
        delivered.stop().await;
    }
    telemetry::record_rpc_recovery_disabled(reason);
    log::error!(
        "{message}; RPC recovery is disabled until the indexer restarts, while any other datasource and the monitoring stack continue running"
    );
    cancellation_token.cancelled().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::{pipeline_drained, wait_with_recovery_disabled, HistoryGapTracker};

    #[test]
    fn one_history_gap_response_does_not_disable_recovery() {
        let mut tracker = HistoryGapTracker::default();

        assert!(!tracker.record_gap());
        assert!(!tracker.record_gap());
        tracker.record_recovered();
        assert!(!tracker.record_gap());
        assert!(!tracker.record_gap());
        assert!(tracker.record_gap());
    }

    #[tokio::test]
    async fn disabling_recovery_does_not_cancel_the_pipeline() {
        let pipeline_token = CancellationToken::new();
        let datasource_token = pipeline_token.clone();
        let disabled = tokio::spawn(async move {
            wait_with_recovery_disabled(&datasource_token, None, "test", "recovery failed").await
        });
        tokio::task::yield_now().await;

        assert!(!pipeline_token.is_cancelled());
        assert!(!disabled.is_finished());

        pipeline_token.cancel();
        disabled.await.unwrap().unwrap();
    }

    fn snapshot(received: u64, processed: u64) -> carbon_core::metrics::MetricsSnapshot {
        carbon_core::metrics::MetricsSnapshot {
            counters: vec![
                ("carbon_updates_received_total", "help", received),
                ("carbon_updates_processed_total", "help", processed),
            ],
            gauges: vec![],
            histograms: vec![],
        }
    }

    #[test]
    fn a_drained_channel_with_updates_still_processing_holds_the_checkpoint() {
        assert!(!pipeline_drained(true, &snapshot(5, 4)));
    }

    #[test]
    fn a_drained_channel_with_every_update_processed_commits_the_checkpoint() {
        assert!(pipeline_drained(true, &snapshot(5, 5)));
    }

    #[test]
    fn a_queued_channel_holds_the_checkpoint() {
        assert!(!pipeline_drained(false, &snapshot(5, 5)));
    }
}
