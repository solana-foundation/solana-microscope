use std::{
    collections::{HashMap, HashSet},
    fmt,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use carbon_core::{
    datasource::{Datasource, DatasourceId, Update, UpdateType},
    error::CarbonResult,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_signature::Signature;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::{
    checkpoint::{stored_genesis_hash, CheckpointLoadOutcome, CheckpointMismatch, CheckpointStore},
    monitored_addresses, unreachable_below, PollingState, RpcPollingDatasource,
};
use crate::{delivered::DeliveredSignatures, failover, shipped, telemetry};

const CHECKPOINT_LOAD_ATTEMPTS: u32 = 3;
const RECOVERY_DISABLE_AFTER_CONSECUTIVE: u32 = 3;
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
        let rpc_clients = self
            .endpoints
            .iter()
            .map(|endpoint| {
                RpcClient::new_with_commitment(endpoint.url.clone(), CommitmentConfig::confirmed())
            })
            .collect::<Vec<_>>();
        let labels = self
            .endpoints
            .iter()
            .map(|endpoint| endpoint.label.clone())
            .collect::<Vec<_>>();
        let addresses = monitored_addresses(self.program_id, self.multisig_state_address);
        log::info!(
            "RPC polling monitors {}",
            addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        telemetry::initialize_rpc_polling();

        let checkpoint_genesis_hash = stored_genesis_hash(&self.checkpoint_path).await;
        let candidates = startup_candidates(rpc_clients.len(), checkpoint_genesis_hash.is_some());
        let (active, genesis_hash) = 'genesis: loop {
            let mut mismatched = None;
            let mut unreachable = false;
            for (index, rpc_client) in rpc_clients.iter().enumerate().take(candidates) {
                match rpc_client.get_genesis_hash().await {
                    Ok(genesis_hash)
                        if checkpoint_genesis_hash
                            .as_ref()
                            .is_some_and(|expected| *expected != genesis_hash.to_string()) =>
                    {
                        log::error!(
                            "the RPC endpoint {} serves genesis hash {genesis_hash}, not the checkpoint's {}; skipping it",
                            labels[index],
                            checkpoint_genesis_hash.as_deref().unwrap_or_default()
                        );
                        mismatched.get_or_insert((index, genesis_hash.to_string()));
                    }
                    Ok(genesis_hash) => break 'genesis (index, genesis_hash.to_string()),
                    Err(error) => {
                        unreachable = true;
                        telemetry::record_rpc_poll_failure();
                        log::error!(
                            "failed to read the RPC genesis hash from {}: {error}",
                            labels[index]
                        );
                    }
                }
            }
            if let Some(mismatched) = mismatched.filter(|_| !unreachable) {
                break 'genesis mismatched;
            }
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
        };
        let mut failover = failover::Failover::new(rpc_clients.len(), active);
        let mut failed_on = HashMap::new();
        telemetry::initialize_rpc_failovers();
        telemetry::set_rpc_active_endpoint(&labels, active);
        for label in &labels {
            telemetry::set_rpc_endpoint_healthy(label, true);
        }
        let mut last_probe: Option<Instant> = None;
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
            genesis_hash.clone(),
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

            let active = failover.active();
            let polled = self
                .poll(&rpc_clients[active], &sender, &id, &mut state, &addresses)
                .await;
            let mut untried = None;
            match &polled {
                Ok(outcome) => {
                    untried = untried_endpoint(
                        &outcome.transient_failures,
                        &mut failed_on,
                        active,
                        (0..rpc_clients.len()).filter(|&index| failover.is_usable(index)),
                    );
                    failover.record(active, untried.is_none(), Instant::now());
                }
                Err(error)
                    if !error.is::<CursorAheadOfHead>() && !error.is::<HistoryUnavailable>() =>
                {
                    failover.record(active, false, Instant::now());
                }
                Err(_) => {}
            }
            match polled {
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
                Err(error)
                    if error.is::<CursorAheadOfHead>() || error.is::<HistoryUnavailable>() =>
                {
                    let (tracker, reason) = if error.is::<CursorAheadOfHead>() {
                        (&mut cursors_ahead, "cursor_ahead_of_head")
                    } else {
                        telemetry::record_rpc_history_unavailable();
                        (&mut history_gaps, "history_unavailable")
                    };
                    if !tracker.record_gap() {
                        log::warn!(
                            "{error:#}; {} is given up on after {RECOVERY_DISABLE_AFTER_CONSECUTIVE} consecutive observations in case this response came from a lagging RPC node",
                            labels[active]
                        );
                    } else if failover.exclude_active() {
                        if failover.all_excluded() {
                            return wait_with_recovery_disabled(
                                &cancellation_token,
                                self.delivered.as_ref(),
                                reason,
                                &format!("RPC recovery cannot continue: {error:#}"),
                            )
                            .await;
                        }
                        log::error!(
                            "{error:#}; RPC polling will not use {} again until the indexer restarts, and moves to another endpoint as soon as one is healthy",
                            labels[active]
                        );
                    }
                }
                Err(error) => {
                    telemetry::record_rpc_poll_failure();
                    log::error!("RPC poll failed on {}: {error:#}", labels[active]);
                }
            }

            let probe_due = rpc_clients.len() > 1
                && last_probe.is_none_or(|probed| probed.elapsed() >= PROBE_INTERVAL);
            if probe_due {
                last_probe = Some(Instant::now());
            }
            let delay = self.poll_interval.saturating_sub(started.elapsed());
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = async {
                    tokio::join!(
                        async {
                            if probe_due {
                                probe_standby(&rpc_clients, &labels, &genesis_hash, &mut failover)
                                    .await;
                            }
                        },
                        tokio::time::sleep(delay),
                    )
                } => {}
            }
            for (index, label) in labels.iter().enumerate() {
                telemetry::set_rpc_endpoint_healthy(label, failover.is_usable(index));
            }
            let untried = untried.filter(|&index| failover.is_usable(index));
            if let Some(next) = failover
                .next(Instant::now())
                .map(|next| untried.unwrap_or(next))
            {
                log::warn!(
                    "switching RPC polling from {} to {}",
                    labels[active],
                    labels[next]
                );
                failover.switch_to(next, Instant::now());
                history_gaps = HistoryGapTracker::default();
                cursors_ahead = HistoryGapTracker::default();
                telemetry::record_rpc_failover();
                telemetry::set_rpc_active_endpoint(&labels, next);
            }
        }
    }

    fn update_types(&self) -> Vec<UpdateType> {
        vec![UpdateType::Transaction]
    }
}

/// Without a checkpoint nothing vouches for a fallback's cluster, and the
/// checkpoint it would write then rejects the primary across restarts, so only
/// the primary may name the cluster.
fn startup_candidates(endpoints: usize, checkpoint_names_cluster: bool) -> usize {
    if checkpoint_names_cluster {
        endpoints
    } else {
        1
    }
}

/// Notes the active endpoint against every transaction it failed to fetch,
/// forgets the ones fetched since, and returns the first usable endpoint that
/// has not failed all of them yet. A failed fetch only counts against the
/// active endpoint while such an endpoint is left to try: once every usable
/// endpoint has failed a transaction, no switch can help, and counting it
/// would bounce polling between endpoints forever.
fn untried_endpoint(
    failures: &HashSet<Signature>,
    failed_on: &mut HashMap<Signature, HashSet<usize>>,
    active: usize,
    mut usable: impl Iterator<Item = usize>,
) -> Option<usize> {
    failed_on.retain(|signature, _| failures.contains(signature));
    for signature in failures {
        failed_on.entry(*signature).or_default().insert(active);
    }
    usable.find(|index| {
        failed_on
            .values()
            .any(|endpoints| !endpoints.contains(index))
    })
}

/// The active endpoint's health comes from its polls; the others answer
/// `getGenesisHash`, which also keeps an endpoint on another cluster from ever
/// looking healthy enough to switch to. Logs a standby only when its state
/// changes.
async fn probe_standby(
    rpc_clients: &[RpcClient],
    labels: &[String],
    genesis_hash: &str,
    failover: &mut failover::Failover,
) {
    let active = failover.active();
    let started = Instant::now();
    let results = futures::future::join_all(
        rpc_clients
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != active && failover.probe_due(*index, started))
            .map(|(index, rpc_client)| async move {
                let result = tokio::time::timeout(PROBE_TIMEOUT, rpc_client.get_genesis_hash())
                    .await
                    .map_err(|_| format!("no answer within {PROBE_TIMEOUT:?}"))
                    .and_then(|result| result.map_err(|error| error.to_string()));
                (index, result)
            }),
    )
    .await;
    let now = Instant::now();
    for (index, result) in results {
        let problem = match result {
            Ok(hash) if hash.to_string() == genesis_hash => None,
            Ok(hash) => Some((
                log::Level::Error,
                format!("serves genesis hash {hash}, not {genesis_hash}; it is on another cluster and will not be used"),
            )),
            Err(error) => Some((log::Level::Warn, format!("failed its probe: {error}"))),
        };
        if !failover.record(index, problem.is_none(), now) {
            continue;
        }
        match problem {
            None => log::info!(
                "the standby RPC endpoint {} answers its probe",
                labels[index]
            ),
            Some((level, problem)) => {
                log::log!(
                    level,
                    "the standby RPC endpoint {} {problem}",
                    labels[index]
                )
            }
        }
    }
}

/// Carbon dequeues an update before processing it, so an empty channel does
/// not prove the work finished.
fn pipeline_drained(channel_empty: bool, snapshot: &carbon_core::metrics::MetricsSnapshot) -> bool {
    if !channel_empty {
        return false;
    }
    match (
        telemetry::carbon_counter(snapshot, "carbon_updates_received_total"),
        telemetry::carbon_counter(snapshot, "carbon_updates_processed_total"),
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

    use std::collections::{HashMap, HashSet};

    use super::{
        pipeline_drained, startup_candidates, untried_endpoint, wait_with_recovery_disabled,
        HistoryGapTracker,
    };
    use crate::test_support::signature;

    /// A fallback pointed at another cluster would otherwise write a
    /// checkpoint that rejects the primary from then on.
    #[test]
    fn only_the_primary_names_the_cluster_until_a_checkpoint_does() {
        assert_eq!(startup_candidates(3, false), 1);
        assert_eq!(startup_candidates(3, true), 3);
    }

    /// With only the previous endpoint remembered, two endpoints that cannot
    /// fetch a transaction would hide a third that can.
    #[test]
    fn a_failing_transaction_is_tried_on_every_usable_endpoint_and_then_stops_counting() {
        let poison = HashSet::from([signature(1)]);
        let mut failed_on = HashMap::new();

        assert_eq!(untried_endpoint(&poison, &mut failed_on, 0, 0..3), Some(1));
        assert_eq!(untried_endpoint(&poison, &mut failed_on, 1, 0..3), Some(2));
        assert_eq!(untried_endpoint(&poison, &mut failed_on, 2, 0..3), None);
        assert_eq!(untried_endpoint(&poison, &mut failed_on, 0, 0..3), None);

        let another = HashSet::from([signature(1), signature(2)]);
        assert_eq!(untried_endpoint(&another, &mut failed_on, 0, 0..3), Some(1));
    }

    #[test]
    fn a_failing_transaction_does_not_count_while_no_other_endpoint_is_usable() {
        let poison = HashSet::from([signature(1)]);
        let mut failed_on = HashMap::new();

        assert_eq!(
            untried_endpoint(&poison, &mut failed_on, 0, [0].into_iter()),
            None
        );
        assert_eq!(untried_endpoint(&poison, &mut failed_on, 0, 0..2), Some(1));
    }

    #[test]
    fn a_transaction_fetched_since_is_forgotten() {
        let poison = HashSet::from([signature(1)]);
        let mut failed_on = HashMap::new();
        untried_endpoint(&poison, &mut failed_on, 0, 0..2);
        untried_endpoint(&poison, &mut failed_on, 1, 0..2);

        assert_eq!(
            untried_endpoint(&HashSet::new(), &mut failed_on, 1, 0..2),
            None
        );
        assert!(failed_on.is_empty());
        assert_eq!(untried_endpoint(&poison, &mut failed_on, 1, 0..2), Some(0));
    }

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
