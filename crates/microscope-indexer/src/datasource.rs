use std::{
    collections::HashMap,
    env,
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

/// How long the active endpoint may fail its probe before the stream moves to
/// the next healthy one.
const FAILOVER_AFTER: Duration = Duration::from_secs(60);
/// How long a higher-priority endpoint must stay healthy before the stream
/// moves back to it.
const FAILBACK_AFTER: Duration = Duration::from_secs(300);

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
    let fallback_urls = fallback_urls
        .filter(|urls| !urls.trim().is_empty())
        .map(|urls| urls.split(',').map(str::trim).collect::<Vec<_>>())
        .unwrap_or_default();
    let fallback_x_tokens = match fallback_x_tokens.filter(|tokens| !tokens.is_empty()) {
        None => vec![""; fallback_urls.len()],
        Some(tokens) => tokens.split(',').map(str::trim).collect(),
    };
    if fallback_x_tokens.len() != fallback_urls.len() {
        return Err(format!(
            "GEYSER_FALLBACK_X_TOKENS lists {} token(s) for {} GEYSER_FALLBACK_URLS entries; give one per URL, leaving an entry empty for an endpoint without a token",
            fallback_x_tokens.len(),
            fallback_urls.len()
        ));
    }

    let mut endpoints = Vec::with_capacity(1 + fallback_urls.len());
    for (url, x_token) in std::iter::once((url, x_token.unwrap_or_default()))
        .chain(fallback_urls.into_iter().zip(fallback_x_tokens))
    {
        let label = endpoint_label(url)?;
        if endpoints
            .iter()
            .any(|endpoint: &Endpoint| endpoint.label == label)
        {
            return Err(format!(
                "Yellowstone endpoint {label} is listed twice; each endpoint needs its own host"
            ));
        }
        endpoints.push(Endpoint {
            url: url.to_string(),
            x_token: Some(x_token.to_string()).filter(|token| !token.is_empty()),
            label,
        });
    }
    Ok(endpoints)
}

/// The host and port only: providers put API keys in the path and query, and
/// the label ends up in metrics and alerts.
fn endpoint_label(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url)
        .ok()
        .filter(|parsed| matches!(parsed.scheme(), "http" | "https"))
        .ok_or_else(|| "every Yellowstone endpoint must be an HTTP(S) URL".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "every Yellowstone endpoint must name a host".to_string())?;
    Ok(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
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

/// Streams from one endpoint at a time. Carbon retries a dead endpoint forever
/// and never returns, so leaving one is decided here, from the probes.
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

    async fn probe_all(&self, health: &mut [ProbeHealth]) {
        let results = futures::future::join_all(
            self.endpoints
                .iter()
                .map(|endpoint| probe_get_version(&endpoint.url, endpoint.x_token.clone())),
        )
        .await;
        let now = Instant::now();
        for ((endpoint, state), result) in self.endpoints.iter().zip(health).zip(results) {
            state.record(result.is_ok(), now);
            match result {
                Ok(()) => {
                    health::record_stream_probe_success();
                    telemetry::set_yellowstone_probe_healthy(&endpoint.label, true);
                }
                Err(error) => {
                    log::warn!(
                        "the Yellowstone endpoint {} failed its readiness probe: {error}",
                        endpoint.label
                    );
                    telemetry::record_yellowstone_probe_failure(&endpoint.label);
                }
            }
        }
    }
}

#[async_trait]
impl Datasource for FailoverDatasource {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: mpsc::Sender<(Update, DatasourceId)>,
        cancellation_token: CancellationToken,
    ) -> CarbonResult<()> {
        let mut health = vec![ProbeHealth::default(); self.endpoints.len()];
        let mut active = 0;
        let mut stream = self.open(active, &id, &sender, &cancellation_token).await;
        let mut probes = tokio::time::interval(PROBE_INTERVAL);
        probes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = probes.tick() => {}
            }
            tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(()),
                _ = self.probe_all(&mut health) => {}
            }
            if let Some(next) = next_endpoint(active, &health, Instant::now()) {
                log::warn!(
                    "switching the Yellowstone stream from {} to {}",
                    self.endpoints[active].label,
                    self.endpoints[next].label
                );
                if let Some(stream) = stream.take() {
                    stream.cancel();
                }
                active = next;
                telemetry::record_yellowstone_failover();
                telemetry::set_yellowstone_active_endpoint(&self.labels(), active);
            }
            if stream.is_none() {
                stream = self.open(active, &id, &sender, &cancellation_token).await;
            }
        }
    }

    fn update_types(&self) -> Vec<UpdateType> {
        self.clients[0].update_types()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ProbeHealth {
    failing_since: Option<Instant>,
    healthy_since: Option<Instant>,
}

impl ProbeHealth {
    fn record(&mut self, healthy: bool, now: Instant) {
        if healthy {
            self.failing_since = None;
            self.healthy_since.get_or_insert(now);
        } else {
            self.healthy_since = None;
            self.failing_since.get_or_insert(now);
        }
    }
}

/// Leaves a failing endpoint for the first healthy one in priority order, and
/// otherwise returns to a higher-priority endpoint once it has stayed healthy.
fn next_endpoint(active: usize, health: &[ProbeHealth], now: Instant) -> Option<usize> {
    let lasted = |since: Option<Instant>, period: Duration| {
        since.is_some_and(|since| now.duration_since(since) >= period)
    };
    if lasted(health[active].failing_since, FAILOVER_AFTER) {
        return (0..health.len())
            .find(|&index| index != active && health[index].healthy_since.is_some());
    }
    (0..active).find(|&index| lasted(health[index].healthy_since, FAILBACK_AFTER))
}

/// Stream silence proves nothing (it only carries the monitored program), so
/// probe with `GetVersion`: the cheapest Geyser call, behind the same
/// `x-token` interceptor as `Subscribe`, so a bad token fails identically.
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
        client, endpoints, next_endpoint, reconnect, record_disconnections, transaction_filters,
        Endpoint, ProbeHealth, RecordingDatasource, DEFAULT_CHANNEL_BUFFER_SIZE, FAILBACK_AFTER,
        FAILOVER_AFTER, STREAM_TIMEOUT,
    };
    use crate::{delivered::DeliveredSignatures, test_support::signature};
    use std::time::Duration;
    use std::time::Instant;
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
    fn labels_endpoints_without_the_keys_in_their_path_or_query() {
        let configured = endpoints(
            "https://mainnet.example.com/secret-key?api-key=secret",
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(configured[0].label, "mainnet.example.com");
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

    fn healthy_since(now: Instant, ago: Duration) -> ProbeHealth {
        ProbeHealth {
            failing_since: None,
            healthy_since: Some(now - ago),
        }
    }

    fn failing_since(now: Instant, ago: Duration) -> ProbeHealth {
        ProbeHealth {
            failing_since: Some(now - ago),
            healthy_since: None,
        }
    }

    #[test]
    fn stays_on_an_endpoint_that_failed_for_less_than_the_failover_period() {
        let now = Instant::now();
        let health = [
            failing_since(now, FAILOVER_AFTER - Duration::from_secs(1)),
            healthy_since(now, Duration::from_secs(600)),
        ];

        assert_eq!(next_endpoint(0, &health, now), None);
    }

    #[test]
    fn fails_over_to_the_first_healthy_endpoint_in_priority_order() {
        let now = Instant::now();
        let health = [
            failing_since(now, FAILOVER_AFTER),
            failing_since(now, Duration::from_secs(30)),
            healthy_since(now, Duration::from_secs(15)),
            healthy_since(now, Duration::from_secs(600)),
        ];

        assert_eq!(next_endpoint(0, &health, now), Some(2));
    }

    /// Switching to an endpoint that is also failing only adds a resubscribe at
    /// the head; staying keeps the replay the stream can still do on recovery.
    #[test]
    fn stays_on_a_failing_endpoint_when_no_other_is_healthy() {
        let now = Instant::now();
        let health = [
            failing_since(now, FAILOVER_AFTER * 10),
            failing_since(now, FAILOVER_AFTER * 10),
            ProbeHealth::default(),
        ];

        assert_eq!(next_endpoint(0, &health, now), None);
    }

    #[test]
    fn returns_to_a_higher_priority_endpoint_only_after_it_stays_healthy() {
        let now = Instant::now();
        let recovering = [
            healthy_since(now, FAILBACK_AFTER - Duration::from_secs(1)),
            healthy_since(now, Duration::from_secs(600)),
        ];
        let recovered = [
            healthy_since(now, FAILBACK_AFTER),
            healthy_since(now, Duration::from_secs(600)),
        ];

        assert_eq!(next_endpoint(1, &recovering, now), None);
        assert_eq!(next_endpoint(1, &recovered, now), Some(0));
        assert_eq!(next_endpoint(0, &recovered, now), None);
    }

    #[test]
    fn a_single_failure_resets_the_healthy_period_and_a_success_the_failing_one() {
        let start = Instant::now();
        let mut health = ProbeHealth::default();

        health.record(true, start);
        health.record(false, start + Duration::from_secs(15));
        health.record(false, start + Duration::from_secs(30));

        assert_eq!(health.healthy_since, None);
        assert_eq!(health.failing_since, Some(start + Duration::from_secs(15)));

        health.record(true, start + Duration::from_secs(45));
        assert_eq!(health.failing_since, None);
        assert_eq!(health.healthy_since, Some(start + Duration::from_secs(45)));
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
