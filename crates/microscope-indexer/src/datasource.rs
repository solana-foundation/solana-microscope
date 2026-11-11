use std::{
    collections::HashMap,
    env,
    future::Future,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use carbon_core::{
    datasource::{Datasource, DatasourceDisconnection, DatasourceId, Update, UpdateType},
    error::CarbonResult,
    pipeline::DEFAULT_CHANNEL_BUFFER_SIZE,
};
use carbon_yellowstone_grpc_datasource::{
    YellowstoneGrpcClientConfig, YellowstoneGrpcGeyserClient,
};
use futures::StreamExt;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yellowstone_grpc_client::{
    Backoff, GeyserGrpcClient, ReconnectConfig, ReconnectionPolicy, DEFAULT_SLOT_RETENTION,
};
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
};

use crate::{delivered::DeliveredSignatures, failover, health, telemetry};

/// The datasource notifies with `try_send` and discards on a full channel.
const DISCONNECT_NOTIFICATION_BUFFER: usize = 16;
/// The datasource forwards with `try_send`, so anything this cannot hold is
/// discarded rather than awaited. Matching the pipeline channel keeps the relay
/// from being the narrower of the two.
const FORWARDING_BUFFER: usize = DEFAULT_CHANNEL_BUFFER_SIZE;
/// Several attempts have to fit inside the grace window, or a single dropped
/// probe decides readiness on its own.
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// How long a probe waits for the endpoint to reject or close a subscription
/// it accepted. Silence past this passes: the probe subscribes to nothing.
const PROBE_REJECTION_GRACE: Duration = Duration::from_secs(2);
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoint {
    url: String,
    x_token: Option<String>,
    label: String,
}

pub fn yellowstone(program_id: &str, multisig_state_address: Option<Pubkey>) -> FailoverDatasource {
    let endpoints = endpoints(
        &env::var("GEYSER_URL").unwrap_or_default(),
        env::var("GEYSER_X_TOKEN").ok().as_deref(),
        env::var("GEYSER_FALLBACK_URLS").ok().as_deref(),
        env::var("GEYSER_FALLBACK_X_TOKENS").ok().as_deref(),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    log::info!(
        "Yellowstone endpoints in priority order: {}",
        endpoints
            .iter()
            .map(|endpoint| endpoint.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    health::expect_stream_probe(probe_grace_seconds());
    telemetry::initialize_yellowstone_failovers();
    for endpoint in &endpoints {
        telemetry::initialize_yellowstone_probe(&endpoint.label);
    }
    let clients = endpoints
        .iter()
        .map(|endpoint| client(endpoint, program_id, multisig_state_address))
        .collect();
    let datasource = FailoverDatasource { endpoints, clients };
    telemetry::set_yellowstone_active_endpoint(&datasource.labels(), 0);
    datasource
}

/// docker-compose passes every variable through even when unset, so an empty
/// value means "not configured", for the lists as for the single token.
fn endpoints(
    url: &str,
    x_token: Option<&str>,
    fallback_urls: Option<&str>,
    fallback_x_tokens: Option<&str>,
) -> Result<Vec<Endpoint>, String> {
    if url.is_empty() {
        return Err("GEYSER_URL env var must be set to a Yellowstone gRPC endpoint".to_string());
    }
    let urls = failover::endpoints("Yellowstone", url, fallback_urls)?;
    let fallbacks = urls.len() - 1;
    let fallback_x_tokens = match fallback_x_tokens.filter(|tokens| !tokens.is_empty()) {
        None => vec![""; fallbacks],
        Some(tokens) => tokens.split(',').map(str::trim).collect(),
    };
    if fallback_x_tokens.len() != fallbacks {
        return Err(format!(
            "GEYSER_FALLBACK_X_TOKENS lists {} token(s) for {fallbacks} GEYSER_FALLBACK_URLS entries; give one per URL, leaving an entry empty for an endpoint without a token",
            fallback_x_tokens.len()
        ));
    }
    Ok(urls
        .into_iter()
        .zip(std::iter::once(x_token.unwrap_or_default()).chain(fallback_x_tokens))
        .map(|((url, label), x_token)| Endpoint {
            url: url.to_string(),
            x_token: Some(x_token.to_string()).filter(|token| !token.is_empty()),
            label,
        })
        .collect())
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

/// Streams from one endpoint, and from two only while a switch hands over.
/// Carbon retries a dead endpoint forever and never returns, so leaving one is
/// decided here, from the probes.
pub struct FailoverDatasource {
    endpoints: Vec<Endpoint>,
    clients: Vec<YellowstoneGrpcGeyserClient>,
}

impl FailoverDatasource {
    fn labels(&self) -> Vec<String> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.label.clone())
            .collect()
    }

    /// Carbon's `consume` dials, spawns the stream and returns, so the token
    /// is the only handle on a running stream.
    async fn open(
        &self,
        index: usize,
        id: &DatasourceId,
        sender: &mpsc::Sender<(Update, DatasourceId)>,
        cancellation_token: &CancellationToken,
    ) -> Option<CancellationToken> {
        let stream = cancellation_token.child_token();
        match self.clients[index]
            .consume(id.clone(), sender.clone(), stream.clone())
            .await
        {
            Ok(()) => Some(stream),
            Err(error) => {
                log::error!(
                    "failed to open the Yellowstone stream to {}: {error}",
                    self.endpoints[index].label
                );
                None
            }
        }
    }

    async fn probe_due(&self, failover: &mut failover::Failover) {
        let started = Instant::now();
        let results = futures::future::join_all(
            self.endpoints
                .iter()
                .enumerate()
                .filter(|(index, _)| failover.probe_due(*index, started))
                .map(|(index, endpoint)| async move { (index, probe_subscribe(endpoint).await) }),
        )
        .await;
        let now = Instant::now();
        for (index, result) in results {
            let endpoint = &self.endpoints[index];
            let changed = failover.record(index, result.is_ok(), now);
            match result {
                Ok(()) => {
                    health::record_stream_probe_success();
                    telemetry::set_yellowstone_probe_healthy(&endpoint.label, true);
                    if changed {
                        log::info!(
                            "the Yellowstone endpoint {} passes its readiness probe",
                            endpoint.label
                        );
                    }
                }
                Err(error) => {
                    telemetry::record_yellowstone_probe_failure(&endpoint.label);
                    if changed {
                        log::warn!(
                            "the Yellowstone endpoint {} failed its readiness probe: {error}",
                            endpoint.label
                        );
                    }
                }
            }
        }
    }
}

/// Opens the stream the policy asks for and only then lets go of the running
/// one, which is returned for the caller to cancel once the new subscription
/// has had time to start: the pipeline deduplicates the overlap, while a
/// cancel-first switch drops whatever lands during the dial. A dial that fails
/// leaves the running stream and the active endpoint as they were.
async fn move_stream<Opening>(
    failover: &mut failover::Failover,
    stream: &mut Option<CancellationToken>,
    now: Instant,
    open: impl FnOnce(usize) -> Opening,
) -> Option<CancellationToken>
where
    Opening: Future<Output = Option<CancellationToken>>,
{
    let target = failover
        .next(now)
        .or_else(|| stream.is_none().then(|| failover.active()))?;
    let opened = open(target).await?;
    if target != failover.active() {
        failover.switch_to(target, now);
    }
    stream.replace(opened)
}

#[async_trait]
impl Datasource for FailoverDatasource {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: mpsc::Sender<(Update, DatasourceId)>,
        cancellation_token: CancellationToken,
    ) -> CarbonResult<()> {
        let mut failover = failover::Failover::new(self.endpoints.len(), 0);
        let mut stream = self.open(0, &id, &sender, &cancellation_token).await;
        let mut retiring: Option<CancellationToken> = None;
        let mut probes = tokio::time::interval(PROBE_INTERVAL);
        probes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = probes.tick() => {}
            }
            if let Some(retired) = retiring.take() {
                retired.cancel();
            }
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = self.probe_due(&mut failover) => {}
            }
            let left = failover.active();
            retiring = move_stream(&mut failover, &mut stream, Instant::now(), |index| {
                self.open(index, &id, &sender, &cancellation_token)
            })
            .await;
            if failover.active() != left {
                log::warn!(
                    "switching the Yellowstone stream from {} to {}",
                    self.endpoints[left].label,
                    self.endpoints[failover.active()].label
                );
                telemetry::record_yellowstone_failover();
                telemetry::set_yellowstone_active_endpoint(&self.labels(), failover.active());
            }
        }
    }

    fn update_types(&self) -> Vec<UpdateType> {
        self.clients[0].update_types()
    }
}

/// Stream silence proves nothing (it only carries the monitored program), and
/// an endpoint can answer unary calls while refusing streams (a quota, a plan
/// without streaming), which Carbon only logs. So the probe subscribes the way
/// the stream does: a fresh dial covering DNS, TLS and the token, then a
/// subscription. It asks for no filters, so the endpoint sends pings and no
/// transactions: every endpoint is probed for as long as the indexer runs, and
/// replaying the stream into each probe would bill that data many times over.
async fn probe_subscribe(endpoint: &Endpoint) -> Result<(), String> {
    let builder = GeyserGrpcClient::build_from_shared(endpoint.url.clone())
        .and_then(|builder| builder.x_token(endpoint.x_token.clone()))
        .and_then(|builder| YellowstoneGrpcClientConfig::default().geyser_config_builder(builder))
        .map_err(|error| error.to_string())?;
    let mut connection = builder.connect().await.map_err(|error| error.to_string())?;
    let (_requests, mut updates) = connection
        .subscribe_with_request(Some(SubscribeRequest::default()))
        .await
        .map_err(|error| error.to_string())?;
    match tokio::time::timeout(PROBE_REJECTION_GRACE, updates.next()).await {
        Ok(Some(Err(status))) => Err(status.to_string()),
        Ok(None) => Err("the endpoint closed the subscription as soon as it opened".to_string()),
        Ok(Some(Ok(_))) | Err(_) => Ok(()),
    }
}

fn client(
    endpoint: &Endpoint,
    program_id: &str,
    multisig_state_address: Option<Pubkey>,
) -> YellowstoneGrpcGeyserClient {
    let (disconnections, notifications) = mpsc::channel(DISCONNECT_NOTIFICATION_BUFFER);
    telemetry::initialize_yellowstone(&endpoint.label);
    let label = endpoint.label.clone();
    tokio::spawn(record_disconnections(notifications, move |missed_slots| {
        telemetry::record_yellowstone_disconnect(&label, missed_slots)
    }));

    YellowstoneGrpcGeyserClient::new(
        endpoint.url.clone(),
        endpoint.x_token.clone(),
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
    let requiring = |address: String| SubscribeRequestFilterTransactions {
        vote: Some(false),
        failed: None,
        signature: None,
        account_include: vec![],
        account_exclude: vec![],
        account_required: vec![address],
        cuckoo_account_include: None,
        token_accounts: None,
    };
    let mut transaction_filters =
        HashMap::from([("program".to_string(), requiring(program_id.to_string()))]);
    if let Some(state_address) = multisig_state_address {
        transaction_filters.insert(
            "multisig_state".to_string(),
            requiring(state_address.to_string()),
        );
    }
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
        client, endpoints, move_stream, reconnect, record_disconnections, transaction_filters,
        Endpoint, RecordingDatasource, DEFAULT_CHANNEL_BUFFER_SIZE, STREAM_TIMEOUT,
    };
    use crate::{
        delivered::DeliveredSignatures,
        failover::{Failover, FAILOVER_AFTER},
        test_support::signature,
    };
    use std::time::{Duration, Instant};
    use yellowstone_grpc_client::ReconnectionPolicy;

    fn transaction_update(signature: Signature, slot: u64) -> Update {
        Update::Transaction(Box::new(TransactionUpdate {
            signature,
            transaction: Default::default(),
            meta: TransactionStatusMeta::default(),
            is_vote: false,
            slot,
            index: None,
            block_time: None,
            block_hash: None,
        }))
    }

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
                let update = transaction_update(*signature, *slot);
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
                let update = transaction_update(*signature, *slot);
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
        let client = client(&endpoint("http://localhost:10000"), "program", None);

        assert!(client.disconnect_notifier.is_some());
    }

    #[tokio::test]
    async fn recovers_a_disconnect_by_replaying_it_rather_than_resuming_at_the_head() {
        let client = client(&endpoint("http://localhost:10000"), "program", None);

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

    /// The primary has failed long enough to leave, and the fallback is
    /// healthy.
    fn failover_due(now: Instant) -> Failover {
        let mut failover = Failover::new(2, 0);
        failover.record(0, false, now - FAILOVER_AFTER);
        failover.record(1, true, now - FAILOVER_AFTER);
        failover
    }

    /// Cancelling first leaves no stream at all when the dial then fails, and
    /// drops what lands during the dial when it succeeds.
    #[tokio::test]
    async fn opens_the_next_endpoint_before_letting_go_of_the_running_stream() {
        let now = Instant::now();
        let mut failover = failover_due(now);
        let running = CancellationToken::new();
        let opened = CancellationToken::new();
        let mut stream = Some(running.clone());

        let retiring = move_stream(&mut failover, &mut stream, now, |index| {
            assert_eq!(index, 1);
            std::future::ready(Some(opened.clone()))
        })
        .await
        .expect("the stream that was running is handed back to retire");

        assert_eq!(failover.active(), 1);
        assert!(!running.is_cancelled());
        retiring.cancel();
        assert!(running.is_cancelled());
        stream.unwrap().cancel();
        assert!(opened.is_cancelled());
    }

    #[tokio::test]
    async fn keeps_the_running_stream_and_endpoint_when_the_next_one_cannot_be_opened() {
        let now = Instant::now();
        let mut failover = failover_due(now);
        let running = CancellationToken::new();
        let mut stream = Some(running.clone());

        let retiring = move_stream(&mut failover, &mut stream, now, |_| {
            std::future::ready(None)
        })
        .await;

        assert!(retiring.is_none());
        assert_eq!(failover.active(), 0);
        assert!(stream.is_some());
        assert!(!running.is_cancelled());
    }

    #[tokio::test]
    async fn reopens_the_active_endpoint_when_no_stream_is_running_and_leaves_a_running_one() {
        let now = Instant::now();
        let mut failover = Failover::new(2, 0);
        let mut stream = None;

        let retiring = move_stream(&mut failover, &mut stream, now, |index| {
            assert_eq!(index, 0);
            std::future::ready(Some(CancellationToken::new()))
        })
        .await;
        assert!(retiring.is_none());
        assert!(stream.is_some());

        move_stream(&mut failover, &mut stream, now, |_| async {
            panic!("a running stream on a healthy endpoint is left alone")
        })
        .await;
    }

    fn endpoint(url: &str) -> Endpoint {
        endpoints(url, None, None, None).unwrap().remove(0)
    }

    #[test]
    fn streams_from_the_main_endpoint_alone_without_fallbacks() {
        let configured = endpoints(
            "https://grpc.main.example:443",
            Some("main"),
            Some(""),
            Some(""),
        )
        .unwrap();

        assert_eq!(
            configured,
            [Endpoint {
                url: "https://grpc.main.example:443".to_string(),
                x_token: Some("main".to_string()),
                label: "grpc.main.example".to_string(),
            }]
        );
    }

    #[test]
    fn pairs_each_fallback_with_its_token_by_position() {
        let configured = endpoints(
            "https://grpc.main.example",
            None,
            Some("https://grpc.second.example, https://grpc.third.example:10000"),
            Some(",third"),
        )
        .unwrap();

        let pairs = configured
            .iter()
            .map(|endpoint| (endpoint.label.as_str(), endpoint.x_token.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            [
                ("grpc.main.example", None),
                ("grpc.second.example", None),
                ("grpc.third.example:10000", Some("third")),
            ]
        );
    }

    /// A token list off by one would pair every later fallback with the wrong
    /// provider's token, which only shows when the failover is needed.
    #[test]
    fn rejects_a_token_list_that_does_not_match_the_fallbacks() {
        let error = endpoints(
            "https://grpc.main.example",
            None,
            Some("https://grpc.second.example,https://grpc.third.example"),
            Some("second"),
        )
        .unwrap_err();

        assert!(error.contains("1 token(s) for 2"), "{error}");
    }

    #[test]
    fn rejects_a_missing_main_endpoint_a_duplicate_and_a_non_http_fallback() {
        assert!(endpoints("", None, None, None).is_err());
        assert!(endpoints(
            "https://grpc.main.example",
            None,
            Some("https://grpc.main.example/other"),
            None
        )
        .is_err());
        assert!(endpoints(
            "https://grpc.main.example",
            None,
            Some("grpc.second.example"),
            None
        )
        .is_err());
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
