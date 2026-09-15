use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::{extract::Request, http::StatusCode, routing::get, Router};

use crate::telemetry::unix_now;

const PROBE_PORT: u16 = 9091;

static STARTED: AtomicBool = AtomicBool::new(false);
static POLL_STALE_AFTER_SECONDS: AtomicU64 = AtomicU64::new(0);
static LAST_POLL_SUCCESS_UNIX: AtomicU64 = AtomicU64::new(0);
static STREAM_STALE_AFTER_SECONDS: AtomicU64 = AtomicU64::new(0);
static LAST_STREAM_PROBE_SUCCESS_UNIX: AtomicU64 = AtomicU64::new(0);

/// Gates readiness on RPC poll freshness. A threshold of 0 leaves readiness
/// blind to the poller, for deployments that do not run one.
pub fn expect_poll_heartbeat(stale_after_seconds: u64) {
    POLL_STALE_AFTER_SECONDS.store(stale_after_seconds, Ordering::Relaxed);
    LAST_POLL_SUCCESS_UNIX.store(unix_now(), Ordering::Relaxed);
}

pub fn record_poll_success() {
    LAST_POLL_SUCCESS_UNIX.store(unix_now(), Ordering::Relaxed);
}

/// Gates readiness on the Yellowstone endpoint answering an unary probe. Only
/// Yellowstone mode arms it, so RPC-only deployments never wait on it; a
/// threshold of 0 disables the check.
pub fn expect_stream_probe(stale_after_seconds: u64) {
    STREAM_STALE_AFTER_SECONDS.store(stale_after_seconds, Ordering::Relaxed);
    LAST_STREAM_PROBE_SUCCESS_UNIX.store(unix_now(), Ordering::Relaxed);
}

pub fn record_stream_probe_success() {
    LAST_STREAM_PROBE_SUCCESS_UNIX.store(unix_now(), Ordering::Relaxed);
}

pub fn mark_started() {
    STARTED.store(true, Ordering::Relaxed);
}

/// Binds before returning, so a port conflict stops the process instead of
/// leaving an indexer running with probe endpoints that never answer.
pub async fn serve() {
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .fallback(|request: Request| async move {
            (
                StatusCode::NOT_FOUND,
                format!("{} is not a probe endpoint\n", request.uri().path()),
            )
        });

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PROBE_PORT))
        .await
        .unwrap_or_else(|err| panic!("failed to bind the probe port {PROBE_PORT}: {err}"));

    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .unwrap_or_else(|err| panic!("the probe listener stopped: {err}"));
    });
}

async fn readyz() -> (StatusCode, String) {
    match evaluate(
        STARTED.load(Ordering::Relaxed),
        Heartbeat {
            stale_after_seconds: POLL_STALE_AFTER_SECONDS.load(Ordering::Relaxed),
            last_unix: LAST_POLL_SUCCESS_UNIX.load(Ordering::Relaxed),
        },
        Heartbeat {
            stale_after_seconds: STREAM_STALE_AFTER_SECONDS.load(Ordering::Relaxed),
            last_unix: LAST_STREAM_PROBE_SUCCESS_UNIX.load(Ordering::Relaxed),
        },
        unix_now(),
    ) {
        Ok(()) => (StatusCode::OK, "ok\n".to_string()),
        Err(reason) => (StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")),
    }
}

#[derive(Clone, Copy)]
struct Heartbeat {
    stale_after_seconds: u64,
    last_unix: u64,
}

impl Heartbeat {
    fn check(self, now_unix: u64, subject: &str) -> Result<(), String> {
        if self.stale_after_seconds == 0 {
            return Ok(());
        }
        let age = now_unix.saturating_sub(self.last_unix);
        if age > self.stale_after_seconds {
            return Err(format!(
                "{subject} in {age}s, over the {}s threshold",
                self.stale_after_seconds
            ));
        }
        Ok(())
    }
}

fn evaluate(
    started: bool,
    poll: Heartbeat,
    stream: Heartbeat,
    now_unix: u64,
) -> Result<(), String> {
    if !started {
        return Err("the indexer has not finished starting".to_string());
    }
    poll.check(now_unix, "no RPC poll has succeeded")?;
    stream.check(now_unix, "no Yellowstone endpoint probe has succeeded")
}

#[cfg(test)]
mod tests {
    use super::{evaluate, Heartbeat};

    const STALE_AFTER: u64 = 60;

    const UNCHECKED: Heartbeat = Heartbeat {
        stale_after_seconds: 0,
        last_unix: 0,
    };

    const fn heartbeat(last_unix: u64) -> Heartbeat {
        Heartbeat {
            stale_after_seconds: STALE_AFTER,
            last_unix,
        }
    }

    #[test]
    fn is_not_ready_until_startup_finishes() {
        assert!(evaluate(false, UNCHECKED, UNCHECKED, 1_000).is_err());
        assert!(evaluate(true, UNCHECKED, UNCHECKED, 1_000).is_ok());
    }

    #[test]
    fn yellowstone_readiness_does_not_wait_for_a_poll_that_never_runs() {
        assert!(evaluate(true, UNCHECKED, heartbeat(u64::MAX), u64::MAX).is_ok());
    }

    #[test]
    fn a_stalled_poller_is_not_ready() {
        assert!(evaluate(true, heartbeat(1_000), UNCHECKED, 1_000 + STALE_AFTER).is_ok());
        assert!(evaluate(true, heartbeat(1_000), UNCHECKED, 1_001 + STALE_AFTER).is_err());
    }

    /// An endpoint blip, or its own rolling restart, resolves well inside the
    /// grace window and must not take the deployment out of rotation.
    #[test]
    fn a_probe_failing_for_less_than_the_grace_window_stays_ready() {
        assert!(evaluate(true, UNCHECKED, heartbeat(1_000), 1_000 + STALE_AFTER).is_ok());
    }

    /// A rejected geyser token leaves the process alive and consuming nothing,
    /// which readiness has to surface rather than report as healthy.
    #[test]
    fn a_probe_failing_past_the_grace_window_is_not_ready() {
        assert!(evaluate(true, UNCHECKED, heartbeat(1_000), 1_001 + STALE_AFTER).is_err());
    }

    /// The window is measured from the last success, so one probe getting
    /// through has to clear a stall the previous ones built up.
    #[test]
    fn a_probe_that_recovers_is_ready_again() {
        let now = 1_001 + STALE_AFTER;
        assert!(evaluate(true, UNCHECKED, heartbeat(1_000), now).is_err());
        assert!(evaluate(true, UNCHECKED, heartbeat(now), now).is_ok());
    }

    /// Nothing probes a Yellowstone endpoint an RPC-only deployment never
    /// configures, so its readiness must not wait on one.
    #[test]
    fn an_rpc_only_deployment_is_ready_without_any_stream_probe() {
        assert!(evaluate(true, heartbeat(1_000), UNCHECKED, 1_000 + STALE_AFTER).is_ok());
        assert!(evaluate(true, heartbeat(u64::MAX), UNCHECKED, u64::MAX).is_ok());
    }

    #[test]
    fn a_clock_that_moved_backwards_does_not_report_a_stall() {
        assert!(evaluate(true, heartbeat(5_000), heartbeat(5_000), 1_000).is_ok());
    }
}
