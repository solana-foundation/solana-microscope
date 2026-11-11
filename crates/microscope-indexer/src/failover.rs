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
/// An endpoint that fails again soon after the datasource returned to it waits
/// twice as long each time, up to this, so a probe that passes while real use
/// fails cannot make the datasource flap every few minutes.
const MAX_FAILBACK_AFTER: Duration = Duration::from_secs(3600);
/// How often a healthy standby is probed while nothing depends on it: often
/// enough to alert on a fallback that died, without paying the full probe rate
/// for an endpoint that may go unused for months.
const IDLE_PROBE_INTERVAL: Duration = Duration::from_secs(300);

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

/// The primary and its comma-separated fallbacks in priority order, each with
/// its label. docker-compose passes a variable through even when unset, so an
/// empty list means no fallbacks. The error never echoes a URL, which may hold
/// a key.
pub(crate) fn endpoints<'a>(
    kind: &str,
    primary: &'a str,
    fallbacks: Option<&'a str>,
) -> Result<Vec<(&'a str, String)>, String> {
    let fallbacks = fallbacks
        .filter(|urls| !urls.trim().is_empty())
        .into_iter()
        .flat_map(|urls| urls.split(',').map(str::trim));
    let mut endpoints: Vec<(&str, String)> = Vec::new();
    for url in std::iter::once(primary).chain(fallbacks) {
        let label = endpoint_label(url)
            .ok_or_else(|| format!("every {kind} endpoint must be an HTTP(S) URL naming a host"))?;
        if endpoints.iter().any(|(_, listed)| *listed == label) {
            return Err(format!(
                "{kind} endpoint {label} is listed twice; each endpoint needs its own host"
            ));
        }
        endpoints.push((url, label));
    }
    Ok(endpoints)
}

#[derive(Clone, Copy, Debug, Default)]
struct ProbeHealth {
    failing_since: Option<Instant>,
    healthy_since: Option<Instant>,
}

impl ProbeHealth {
    /// Returns whether this observation changed the endpoint's state.
    fn record(&mut self, healthy: bool, now: Instant) -> bool {
        let (ended, started) = if healthy {
            (&mut self.failing_since, &mut self.healthy_since)
        } else {
            (&mut self.healthy_since, &mut self.failing_since)
        };
        *ended = None;
        let changed = started.is_none();
        started.get_or_insert(now);
        changed
    }
}

/// Which endpoint a datasource uses, given how long each has been failing or
/// healthy.
#[derive(Debug)]
pub(crate) struct Failover {
    active: usize,
    returned_at: Option<Instant>,
    health: Vec<ProbeHealth>,
    failback_after: Vec<Duration>,
    excluded: Vec<bool>,
    probed_at: Vec<Option<Instant>>,
}

impl Failover {
    pub(crate) fn new(endpoints: usize, active: usize) -> Self {
        Self {
            active,
            returned_at: None,
            health: vec![ProbeHealth::default(); endpoints],
            failback_after: vec![FAILBACK_AFTER; endpoints],
            excluded: vec![false; endpoints],
            probed_at: vec![None; endpoints],
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.active
    }

    /// Returns whether this observation changed the endpoint's state.
    pub(crate) fn record(&mut self, index: usize, healthy: bool, now: Instant) -> bool {
        self.probed_at[index] = Some(now);
        self.health[index].record(healthy, now)
    }

    /// Whether to probe this endpoint on this round. A healthy fallback below
    /// the active endpoint is only needed once the active one fails, and the
    /// failover period then leaves several rounds to check it again, so until
    /// then it is probed rarely. An excluded endpoint is never used again.
    pub(crate) fn probe_due(&self, index: usize, now: Instant) -> bool {
        if self.excluded[index] && index != self.active {
            return false;
        }
        let idle = index > self.active
            && self.health[self.active].failing_since.is_none()
            && self.health[index].healthy_since.is_some();
        !idle
            || self.probed_at[index]
                .is_none_or(|probed| now.duration_since(probed) >= IDLE_PROBE_INTERVAL)
    }

    pub(crate) fn is_usable(&self, index: usize) -> bool {
        !self.excluded[index] && self.health[index].healthy_since.is_some()
    }

    /// Gives up on the active endpoint for good. Returns whether it was still
    /// in use.
    pub(crate) fn exclude_active(&mut self) -> bool {
        !std::mem::replace(&mut self.excluded[self.active], true)
    }

    pub(crate) fn all_excluded(&self) -> bool {
        self.excluded.iter().all(|excluded| *excluded)
    }

    /// Leaves a failing or excluded endpoint for the first usable one in
    /// priority order, and otherwise returns to a higher-priority endpoint
    /// once it has stayed healthy.
    pub(crate) fn next(&self, now: Instant) -> Option<usize> {
        let lasted = |since: Option<Instant>, period: Duration| {
            since.is_some_and(|since| now.duration_since(since) >= period)
        };
        if self.excluded[self.active]
            || lasted(self.health[self.active].failing_since, FAILOVER_AFTER)
        {
            return (0..self.health.len())
                .find(|&index| index != self.active && self.is_usable(index));
        }
        (0..self.active).find(|&index| {
            !self.excluded[index]
                && lasted(self.health[index].healthy_since, self.failback_after[index])
        })
    }

    pub(crate) fn switch_to(&mut self, next: usize, now: Instant) {
        let left = self.active;
        if self.health[left].failing_since.is_some() {
            let waited = self.failback_after[left];
            let failed_early = self
                .returned_at
                .is_some_and(|returned| now.duration_since(returned) < waited);
            self.failback_after[left] = if failed_early {
                (waited * 2).min(MAX_FAILBACK_AFTER)
            } else {
                FAILBACK_AFTER
            };
        }
        self.active = next;
        self.returned_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        endpoint_label, endpoints, Failover, ProbeHealth, FAILBACK_AFTER, FAILOVER_AFTER,
        IDLE_PROBE_INTERVAL, MAX_FAILBACK_AFTER,
    };

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

    #[test]
    fn rejects_an_endpoint_list_without_echoing_the_key_in_a_url() {
        let error = endpoints(
            "RPC",
            "https://rpc.example.com",
            Some("rpc.other.com/secret"),
        )
        .unwrap_err();
        assert!(!error.contains("secret"), "{error}");

        assert!(endpoints(
            "RPC",
            "https://rpc.example.com",
            Some("https://rpc.other.com,")
        )
        .is_err());
    }

    /// Each endpoint starts `ago` in the given state, as seen from `now`.
    fn failover(active: usize, now: Instant, states: &[Option<(bool, Duration)>]) -> Failover {
        let mut failover = Failover::new(states.len(), active);
        for (index, state) in states.iter().enumerate() {
            if let Some((healthy, ago)) = state {
                failover.record(index, *healthy, now - *ago);
            }
        }
        failover
    }

    const fn healthy(ago: Duration) -> Option<(bool, Duration)> {
        Some((true, ago))
    }

    const fn failing(ago: Duration) -> Option<(bool, Duration)> {
        Some((false, ago))
    }

    #[test]
    fn stays_on_an_endpoint_that_failed_for_less_than_the_failover_period() {
        let now = Instant::now();
        let failover = failover(
            0,
            now,
            &[
                failing(FAILOVER_AFTER - Duration::from_secs(1)),
                healthy(Duration::from_secs(600)),
            ],
        );

        assert_eq!(failover.next(now), None);
    }

    #[test]
    fn fails_over_to_the_first_healthy_endpoint_in_priority_order() {
        let now = Instant::now();
        let failover = failover(
            0,
            now,
            &[
                failing(FAILOVER_AFTER),
                failing(Duration::from_secs(30)),
                healthy(Duration::from_secs(15)),
                healthy(Duration::from_secs(600)),
            ],
        );

        assert_eq!(failover.next(now), Some(2));
    }

    /// Switching to an endpoint that is also failing only adds a resubscribe at
    /// the head; staying keeps the replay the stream can still do on recovery.
    #[test]
    fn stays_on_a_failing_endpoint_when_no_other_is_healthy() {
        let now = Instant::now();
        let failover = failover(
            0,
            now,
            &[
                failing(FAILOVER_AFTER * 10),
                failing(FAILOVER_AFTER * 10),
                None,
            ],
        );

        assert_eq!(failover.next(now), None);
    }

    #[test]
    fn returns_to_a_higher_priority_endpoint_only_after_it_stays_healthy() {
        let now = Instant::now();
        let recovering = failover(
            1,
            now,
            &[
                healthy(FAILBACK_AFTER - Duration::from_secs(1)),
                healthy(Duration::from_secs(600)),
            ],
        );
        let recovered = |active| {
            failover(
                active,
                now,
                &[healthy(FAILBACK_AFTER), healthy(Duration::from_secs(600))],
            )
        };

        assert_eq!(recovering.next(now), None);
        assert_eq!(recovered(1).next(now), Some(0));
        assert_eq!(recovered(0).next(now), None);
    }

    /// A probe can pass while real use fails (a rate limit, a broken method),
    /// and that endpoint would otherwise be returned to every five minutes
    /// forever, each return stalling the datasource for the failover period.
    #[test]
    fn an_endpoint_that_fails_again_right_after_a_return_waits_longer_each_time() {
        let start = Instant::now();
        let mut failover = Failover::new(2, 1);
        failover.record(1, true, start);
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..6 {
            failover.record(0, true, now);
            let recovering_since = now;
            while failover.next(now) != Some(0) {
                now += Duration::from_secs(15);
            }
            waits.push(now - recovering_since);
            failover.switch_to(0, now);

            failover.record(0, false, now);
            now += FAILOVER_AFTER;
            assert_eq!(failover.next(now), Some(1));
            failover.switch_to(1, now);
        }

        assert_eq!(
            waits,
            [
                FAILBACK_AFTER,
                FAILBACK_AFTER * 2,
                FAILBACK_AFTER * 4,
                FAILBACK_AFTER * 8,
                MAX_FAILBACK_AFTER,
                MAX_FAILBACK_AFTER,
            ]
        );
    }

    #[test]
    fn an_endpoint_that_held_after_its_return_waits_the_usual_period_again() {
        let start = Instant::now();
        let mut failover = Failover::new(2, 0);
        failover.record(1, true, start);
        failover.record(0, false, start);
        failover.switch_to(1, start + FAILOVER_AFTER);

        let returned = start + Duration::from_secs(3600);
        failover.switch_to(0, returned);
        let failed = returned + FAILBACK_AFTER * 3;
        failover.record(0, false, failed);
        failover.switch_to(1, failed + FAILOVER_AFTER);

        failover.record(0, true, failed + FAILOVER_AFTER);
        assert_eq!(
            failover.next(failed + FAILOVER_AFTER + FAILBACK_AFTER),
            Some(0)
        );
    }

    /// The endpoint the process starts on was never returned to, so its first
    /// failure says nothing about a probe that passes while real use fails.
    #[test]
    fn a_failure_right_after_startup_does_not_lengthen_the_wait() {
        let start = Instant::now();
        let mut failover = Failover::new(2, 0);
        failover.record(1, true, start);
        failover.record(0, false, start);
        let left = start + FAILOVER_AFTER;
        failover.switch_to(1, left);

        failover.record(0, true, left);
        assert_eq!(failover.next(left + FAILBACK_AFTER), Some(0));
    }

    /// An endpoint that cannot serve the checkpoint is left at once, never
    /// waited on and never returned to, however healthy its probe looks.
    #[test]
    fn leaves_an_excluded_endpoint_for_good_as_soon_as_another_is_healthy() {
        let now = Instant::now();
        let mut failover = failover(
            1,
            now,
            &[failing(Duration::from_secs(30)), healthy(Duration::ZERO)],
        );

        assert!(failover.exclude_active());
        assert!(!failover.exclude_active());
        assert!(!failover.all_excluded());
        assert_eq!(failover.next(now), None);

        failover.record(0, true, now);
        assert_eq!(failover.next(now), Some(0));
        failover.switch_to(0, now);

        failover.record(1, true, now);
        let later = now + MAX_FAILBACK_AFTER;
        assert_eq!(failover.next(later), None);
        failover.record(0, false, later);
        assert_eq!(failover.next(later + FAILOVER_AFTER), None);
    }

    /// The rare probe must not slow a failover or a failback down, nor hide a
    /// fallback that is failing.
    #[test]
    fn probes_a_healthy_fallback_rarely_until_something_depends_on_it() {
        let start = Instant::now();
        let round = Duration::from_secs(15);
        let mut failover = Failover::new(3, 1);
        assert!(failover.probe_due(2, start));

        failover.record(0, false, start);
        failover.record(1, true, start);
        failover.record(2, true, start);
        assert!(failover.probe_due(0, start + round));
        assert!(failover.probe_due(1, start + round));
        assert!(!failover.probe_due(2, start + round));
        assert!(failover.probe_due(2, start + IDLE_PROBE_INTERVAL));

        failover.record(2, false, start + round);
        assert!(failover.probe_due(2, start + round * 2));

        failover.record(2, true, start + round * 2);
        assert!(!failover.probe_due(2, start + round * 3));
        failover.record(1, false, start + round * 3);
        assert!(failover.probe_due(2, start + round * 4));
    }

    #[test]
    fn a_single_failure_resets_the_healthy_period_and_a_success_the_failing_one() {
        let start = Instant::now();
        let mut health = ProbeHealth::default();

        assert!(health.record(true, start));
        assert!(health.record(false, start + Duration::from_secs(15)));
        assert!(!health.record(false, start + Duration::from_secs(30)));

        assert_eq!(health.healthy_since, None);
        assert_eq!(health.failing_since, Some(start + Duration::from_secs(15)));

        assert!(health.record(true, start + Duration::from_secs(45)));
        assert_eq!(health.failing_since, None);
        assert_eq!(health.healthy_since, Some(start + Duration::from_secs(45)));
    }
}
