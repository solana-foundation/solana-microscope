//! The ordered failover policy shared by the Yellowstone stream and the RPC
//! poller: which endpoint to use, given how long each has been failing or
//! healthy.

use std::time::{Duration, Instant};

/// How long the active endpoint may keep failing before the datasource moves
/// to the next healthy one.
pub(crate) const FAILOVER_AFTER: Duration = Duration::from_secs(60);
/// How long a higher-priority endpoint must stay healthy before the
/// datasource moves back to it.
pub(crate) const FAILBACK_AFTER: Duration = Duration::from_secs(300);

/// The host and port only: providers put API keys in the path and query, and
/// the label ends up in metrics and alerts.
pub(crate) fn endpoint_label(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url)
        .ok()
        .filter(|parsed| matches!(parsed.scheme(), "http" | "https"))?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProbeHealth {
    failing_since: Option<Instant>,
    healthy_since: Option<Instant>,
}

impl ProbeHealth {
    pub(crate) fn record(&mut self, healthy: bool, now: Instant) {
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
pub(crate) fn next_endpoint(active: usize, health: &[ProbeHealth], now: Instant) -> Option<usize> {
    let lasted = |since: Option<Instant>, period: Duration| {
        since.is_some_and(|since| now.duration_since(since) >= period)
    };
    if lasted(health[active].failing_since, FAILOVER_AFTER) {
        return (0..health.len())
            .find(|&index| index != active && health[index].healthy_since.is_some());
    }
    (0..active).find(|&index| lasted(health[index].healthy_since, FAILBACK_AFTER))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{endpoint_label, next_endpoint, ProbeHealth, FAILBACK_AFTER, FAILOVER_AFTER};

    #[test]
    fn labels_endpoints_without_the_keys_in_their_path_or_query() {
        assert_eq!(
            endpoint_label("https://mainnet.example.com/secret-key?api-key=secret").as_deref(),
            Some("mainnet.example.com")
        );
        assert_eq!(
            endpoint_label("http://127.0.0.1:10000").as_deref(),
            Some("127.0.0.1:10000")
        );
        assert_eq!(endpoint_label("mainnet.example.com"), None);
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
}
