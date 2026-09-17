use std::{collections::HashMap, env, time::Duration};

use async_trait::async_trait;
use carbon_core::{
    datasource::{Datasource, DatasourceDisconnection, DatasourceId, Update, UpdateType},
    error::CarbonResult,
    pipeline::DEFAULT_CHANNEL_BUFFER_SIZE,
};
use carbon_yellowstone_grpc_datasource::{
    YellowstoneGrpcClientConfig, YellowstoneGrpcGeyserClient,
};
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yellowstone_grpc_client::{
    Backoff, GeyserGrpcClient, ReconnectConfig, ReconnectionPolicy, DEFAULT_SLOT_RETENTION,
};
use yellowstone_grpc_proto::geyser::{CommitmentLevel, SubscribeRequestFilterTransactions};

use crate::{delivered::DeliveredSignatures, health, telemetry};

/// The datasource notifies with `try_send` and discards on a full channel.
const DISCONNECT_NOTIFICATION_BUFFER: usize = 16;
/// The datasource forwards with `try_send`, so anything this cannot hold is
/// discarded rather than awaited. Matching the pipeline channel keeps the relay
/// from being the narrower of the two.
const FORWARDING_BUFFER: usize = DEFAULT_CHANNEL_BUFFER_SIZE;
/// Several attempts have to fit inside the grace window, or a single dropped
/// probe decides readiness on its own.
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// A reconnect attempt schedule of 1s, 2s, 4s, 8s and 16s. The client replays
/// from the slot it last saw across all of them, so an outage only becomes a
/// gap once the whole budget is spent.
const RECONNECT_INITIAL_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_MULTIPLIER: f64 = 2.0;
const RECONNECT_MAX_RETRIES: u32 = 5;
/// Carbon abandons a stream that has been silent this long and resubscribes at
/// the live head, which throws the replay checkpoint away, so it has to outlast
/// the reconnect budget rather than cut it short.
const STREAM_TIMEOUT: Duration = Duration::from_secs(120);

pub fn yellowstone(
    program_id: &str,
    multisig_state_address: Option<Pubkey>,
) -> YellowstoneGrpcGeyserClient {
    let geyser_url = env::var("GEYSER_URL").unwrap_or_default();
    if geyser_url.is_empty() {
        panic!("GEYSER_URL env var must be set to a Yellowstone gRPC endpoint");
    }

    // docker-compose passes the var through even when unset, so an empty string means "no token".
    let x_token = env::var("GEYSER_X_TOKEN").ok().filter(|t| !t.is_empty());

    health::expect_stream_probe(probe_grace_seconds());
    telemetry::initialize_yellowstone_probe();
    tokio::spawn(probe_endpoint(geyser_url.clone(), x_token.clone()));

    client(geyser_url, x_token, program_id, multisig_state_address)
}

/// How long the endpoint may fail the probe continuously before `/readyz`
/// reports unready.
fn probe_grace_seconds() -> u64 {
    const DEFAULT: u64 = 90;

    let Some(configured) = env::var("MICROSCOPE_STREAM_STALE_AFTER_SECONDS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return DEFAULT;
    };
    configured.parse().unwrap_or_else(|_| {
        panic!("MICROSCOPE_STREAM_STALE_AFTER_SECONDS must be a whole number of seconds, or 0 to disable the check")
    })
}

/// Stream silence proves nothing (it only carries the monitored program), so
/// probe with `GetVersion`: the cheapest Geyser call, behind the same
/// `x-token` interceptor as `Subscribe`, so a bad token fails identically.
async fn probe_endpoint(geyser_url: String, x_token: Option<String>) {
    loop {
        match probe_get_version(&geyser_url, x_token.clone()).await {
            Ok(()) => {
                health::record_stream_probe_success();
                telemetry::set_yellowstone_probe_healthy(true);
            }
            Err(error) => {
                log::warn!("the Yellowstone endpoint failed its readiness probe: {error}");
                telemetry::record_yellowstone_probe_failure();
            }
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Dials from scratch each probe, covering DNS, TLS and the token the way a
/// fresh subscription would.
async fn probe_get_version(geyser_url: &str, x_token: Option<String>) -> Result<(), String> {
    let builder = GeyserGrpcClient::build_from_shared(geyser_url.to_string())
        .and_then(|builder| builder.x_token(x_token))
        .and_then(|builder| YellowstoneGrpcClientConfig::default().geyser_config_builder(builder))
        .map_err(|error| error.to_string())?;
    let mut client = builder.connect().await.map_err(|error| error.to_string())?;
    client
        .get_version()
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn client(
    geyser_url: String,
    x_token: Option<String>,
    program_id: &str,
    multisig_state_address: Option<Pubkey>,
) -> YellowstoneGrpcGeyserClient {
    let (disconnections, notifications) = mpsc::channel(DISCONNECT_NOTIFICATION_BUFFER);
    telemetry::initialize_yellowstone();
    tokio::spawn(record_disconnections(
        notifications,
        telemetry::record_yellowstone_disconnect,
    ));

    YellowstoneGrpcGeyserClient::new(
        geyser_url,
        x_token,
        Some(CommitmentLevel::Confirmed),
        HashMap::new(),
        transaction_filters(program_id, multisig_state_address),
        Default::default(),
        YellowstoneGrpcClientConfig::default().with_reconnect(reconnect()),
        Some(disconnections),
        Some(STREAM_TIMEOUT),
    )
}

/// Reconnects inside the stream and replays from the last slot the client saw,
/// deduplicating what the replay repeats. Only what this fails to recover
/// reaches the RPC poller.
fn reconnect() -> ReconnectConfig {
    ReconnectConfig {
        backoff: Backoff::new(
            RECONNECT_INITIAL_INTERVAL,
            RECONNECT_MULTIPLIER,
            RECONNECT_MAX_RETRIES,
        ),
        policy: ReconnectionPolicy::RecoverMissedData {
            slot_retention: DEFAULT_SLOT_RETENTION,
        },
    }
}

/// Records the signatures a datasource delivered, so the RPC poller can skip
/// what has already reached the pipeline instead of re-emitting it.
pub struct RecordingDatasource<D> {
    inner: D,
    delivered: DeliveredSignatures,
}

impl<D> RecordingDatasource<D> {
    pub fn new(inner: D, delivered: DeliveredSignatures) -> Self {
        Self { inner, delivered }
    }
}

#[async_trait]
impl<D: Datasource> Datasource for RecordingDatasource<D> {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: mpsc::Sender<(Update, DatasourceId)>,
        cancellation_token: CancellationToken,
    ) -> CarbonResult<()> {
        let (forwarding_sender, mut forwarding_receiver) = mpsc::channel(FORWARDING_BUFFER);
        let delivered = self.delivered.clone();
        let forwarding = tokio::spawn(async move {
            while let Some((update, source)) = forwarding_receiver.recv().await {
                let delivery = match &update {
                    Update::Transaction(transaction) => {
                        Some((transaction.signature, transaction.slot))
                    }
                    _ => None,
                };
                if sender.send((update, source)).await.is_err() {
                    return;
                }
                // Only after the send: an unqueued update must stay
                // rediscoverable by the poller.
                if let Some((signature, slot)) = delivery {
                    delivered.record(signature, slot);
                }
            }
        });

        let consumed = self
            .inner
            .consume(id, forwarding_sender, cancellation_token)
            .await;
        if let Err(error) = forwarding.await {
            log::error!("the datasource forwarding task failed: {error}");
        }
        consumed
    }

    fn update_types(&self) -> Vec<UpdateType> {
        self.inner.update_types()
    }
}

async fn record_disconnections(
    mut notifications: mpsc::Receiver<DatasourceDisconnection>,
    record: impl Fn(u64),
) {
    while let Some(disconnection) = notifications.recv().await {
        record(disconnection.missed_slots);
    }
}

fn transaction_filters(
    program_id: &str,
    multisig_state_address: Option<Pubkey>,
) -> HashMap<String, SubscribeRequestFilterTransactions> {
    let mut transaction_filters = HashMap::new();
    transaction_filters.insert(
        "program".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: None,
            signature: None,
            account_include: vec![],
            account_exclude: vec![],
            account_required: vec![program_id.to_string()],
            cuckoo_account_include: None,
            token_accounts: None,
        },
    );
    let Some(state_address) = multisig_state_address else {
        return transaction_filters;
    };
    transaction_filters.insert(
        "multisig_state".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: None,
            signature: None,
            account_include: vec![],
            account_exclude: vec![],
            account_required: vec![state_address.to_string()],
            cuckoo_account_include: None,
            token_accounts: None,
        },
    );

    transaction_filters
}

#[cfg(test)]
mod tests {
    use carbon_core::datasource::DatasourceDisconnection;
    use solana_pubkey::Pubkey;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    use async_trait::async_trait;
    use carbon_core::{
        datasource::{Datasource, DatasourceId, TransactionUpdate, Update, UpdateType},
        error::CarbonResult,
    };
    use solana_signature::Signature;
    use solana_transaction_status::TransactionStatusMeta;
    use tokio_util::sync::CancellationToken;

    use super::{
        client, reconnect, record_disconnections, transaction_filters, RecordingDatasource,
        DEFAULT_CHANNEL_BUFFER_SIZE, STREAM_TIMEOUT,
    };
    use crate::delivered::DeliveredSignatures;
    use std::time::Duration;
    use yellowstone_grpc_client::ReconnectionPolicy;

    struct StubDatasource {
        updates: Vec<(Signature, u64)>,
    }

    #[async_trait]
    impl Datasource for StubDatasource {
        async fn consume(
            &self,
            id: DatasourceId,
            sender: mpsc::Sender<(Update, DatasourceId)>,
            _cancellation_token: CancellationToken,
        ) -> CarbonResult<()> {
            for (signature, slot) in &self.updates {
                let update = Update::Transaction(Box::new(TransactionUpdate {
                    signature: *signature,
                    transaction: Default::default(),
                    meta: TransactionStatusMeta::default(),
                    is_vote: false,
                    slot: *slot,
                    index: None,
                    block_time: None,
                    block_hash: None,
                }));
                let _ = sender.send((update, id.clone())).await;
            }
            Ok(())
        }

        fn update_types(&self) -> Vec<UpdateType> {
            vec![UpdateType::Transaction]
        }
    }

    /// Mirrors carbon's yellowstone datasource, which never awaits a full
    /// channel.
    struct TrySendDatasource {
        updates: Vec<(Signature, u64)>,
        rejected: Arc<Mutex<Vec<Signature>>>,
    }

    #[async_trait]
    impl Datasource for TrySendDatasource {
        async fn consume(
            &self,
            id: DatasourceId,
            sender: mpsc::Sender<(Update, DatasourceId)>,
            _cancellation_token: CancellationToken,
        ) -> CarbonResult<()> {
            for (signature, slot) in &self.updates {
                let update = Update::Transaction(Box::new(TransactionUpdate {
                    signature: *signature,
                    transaction: Default::default(),
                    meta: TransactionStatusMeta::default(),
                    is_vote: false,
                    slot: *slot,
                    index: None,
                    block_time: None,
                    block_hash: None,
                }));
                if sender.try_send((update, id.clone())).is_err() {
                    self.rejected.lock().unwrap().push(*signature);
                }
            }
            Ok(())
        }

        fn update_types(&self) -> Vec<UpdateType> {
            vec![UpdateType::Transaction]
        }
    }

    fn signature(value: u64) -> Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        Signature::from(bytes)
    }

    fn disconnection(missed_slots: u64) -> DatasourceDisconnection {
        DatasourceDisconnection {
            source: "yellowstone-grpc".to_string(),
            disconnect_time: chrono::Utc::now(),
            last_slot_before_disconnect: 100,
            first_slot_after_reconnect: 100 + missed_slots,
            missed_slots,
        }
    }

    #[tokio::test]
    async fn subscribes_with_a_notifier_so_gaps_are_not_silent() {
        let client = client("http://localhost:10000".to_string(), None, "program", None);

        assert!(client.disconnect_notifier.is_some());
    }

    #[tokio::test]
    async fn recovers_a_disconnect_by_replaying_it_rather_than_resuming_at_the_head() {
        let client = client("http://localhost:10000".to_string(), None, "program", None);

        let configured = client
            .geyser_config
            .reconnect
            .expect("the stream recovers its own disconnects");
        assert!(matches!(
            configured.policy,
            ReconnectionPolicy::RecoverMissedData { .. }
        ));
    }

    /// Carbon abandons a silent stream and resubscribes at the live head, which
    /// discards the replay checkpoint. A timeout inside the reconnect budget
    /// would cut every recovery short and hand the gap to the RPC poller.
    #[test]
    fn keeps_the_stream_alive_for_longer_than_the_reconnect_budget() {
        let backoff = reconnect().backoff;
        let mut interval = backoff.initial_interval;
        let mut budget = Duration::ZERO;
        for _ in 0..backoff.max_retries {
            budget += interval;
            interval = interval.mul_f64(backoff.multiplier);
        }

        assert!(
            STREAM_TIMEOUT > budget,
            "a {STREAM_TIMEOUT:?} stream timeout cuts the {budget:?} reconnect budget short"
        );
    }

    #[tokio::test]
    async fn records_every_reconnect_with_the_slots_it_missed() {
        let (notifier, notifications) = mpsc::channel(4);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        for missed_slots in [7, 0, 250] {
            notifier.send(disconnection(missed_slots)).await.unwrap();
        }
        drop(notifier);

        record_disconnections(notifications, move |missed_slots| {
            sink.lock().unwrap().push(missed_slots)
        })
        .await;

        assert_eq!(*recorded.lock().unwrap(), [7, 0, 250]);
    }

    #[tokio::test]
    async fn forwards_every_update_and_records_what_reached_the_pipeline() {
        let delivered = DeliveredSignatures::new();
        let datasource = RecordingDatasource::new(
            StubDatasource {
                updates: vec![(signature(1), 100), (signature(2), 101)],
            },
            delivered.clone(),
        );
        let (sender, mut receiver) = mpsc::channel(4);

        datasource
            .consume(DatasourceId::new_unique(), sender, CancellationToken::new())
            .await
            .unwrap();

        let mut forwarded = Vec::new();
        while let Ok((update, _)) = receiver.try_recv() {
            let Update::Transaction(transaction) = update else {
                panic!("the stub only emits transactions");
            };
            forwarded.push((transaction.signature, transaction.slot));
        }
        assert_eq!(forwarded, [(signature(1), 100), (signature(2), 101)]);
        assert_eq!(
            delivered.drain().await,
            [(signature(1), 100), (signature(2), 101)]
        );
    }

    /// Carbon's datasource sends with `try_send`, so a relay too small to hold
    /// a slot's worth of updates discards them instead of applying
    /// backpressure. Two transactions in one slot is the common case that
    /// exposes it.
    #[tokio::test]
    async fn keeps_every_update_a_datasource_offers_without_awaiting() {
        let rejected = Arc::new(Mutex::new(Vec::new()));
        let delivered = DeliveredSignatures::new();
        let datasource = RecordingDatasource::new(
            TrySendDatasource {
                updates: vec![(signature(1), 100), (signature(2), 100)],
                rejected: rejected.clone(),
            },
            delivered.clone(),
        );
        let (sender, mut receiver) = mpsc::channel(DEFAULT_CHANNEL_BUFFER_SIZE);

        datasource
            .consume(DatasourceId::new_unique(), sender, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            *rejected.lock().unwrap(),
            Vec::<Signature>::new(),
            "the relay discarded updates the datasource could not await"
        );
        let mut forwarded = Vec::new();
        while let Ok((update, _)) = receiver.try_recv() {
            let Update::Transaction(transaction) = update else {
                panic!("the stub only emits transactions");
            };
            forwarded.push((transaction.signature, transaction.slot));
        }
        assert_eq!(forwarded, [(signature(1), 100), (signature(2), 100)]);
    }

    /// An update the pipeline never received must stay rediscoverable by the
    /// RPC poller, or the transaction is lost instead of duplicated.
    #[tokio::test]
    async fn records_nothing_once_the_pipeline_channel_closes() {
        let delivered = DeliveredSignatures::new();
        let datasource = RecordingDatasource::new(
            StubDatasource {
                updates: vec![(signature(1), 100)],
            },
            delivered.clone(),
        );
        let (sender, receiver) = mpsc::channel(4);
        drop(receiver);

        datasource
            .consume(DatasourceId::new_unique(), sender, CancellationToken::new())
            .await
            .unwrap();

        assert!(delivered.drain().await.is_empty());
    }

    #[test]
    fn subscribes_to_the_program_only_without_a_multisig() {
        let filters = transaction_filters("program", None);

        assert_eq!(filters.len(), 1);
        assert_eq!(filters["program"].account_required, ["program"]);
    }

    #[test]
    fn subscribes_to_the_multisig_state_account_and_not_the_vault() {
        let state = Pubkey::new_unique();
        let filters = transaction_filters("program", Some(state));

        assert_eq!(filters.len(), 2);
        assert_eq!(filters["program"].account_required, ["program"]);
        assert_eq!(
            filters["multisig_state"].account_required,
            [state.to_string()]
        );
        assert!(!filters.contains_key("multisig_vault"));
        assert!(filters.keys().all(|name| !name.starts_with("squads_")));
    }
}
