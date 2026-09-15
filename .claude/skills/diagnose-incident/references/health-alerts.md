# Health alerts

Generated for every deployment, independent of the monitored program, so these are the failure modes shared across all operators. All carry `signal_kind = "datasource"`.

## What a deployment actually has

| Condition | Alerts |
|-----------|--------|
| Always | `rpc_recovery_disabled`, `rpc_checkpoint_corrupt`, `datasource_updates_dropped`, `log_delivery_stalled` |
| `datasource.mode = "yellowstone"` | `yellowstone_endpoint_unreachable`, `yellowstone_stream_interrupted`, `yellowstone_gap_unrecovered` |
| `RPC_URL` set, or `mode = "rpc"` | `rpc_poll_stale`, `rpc_poll_lag`, `rpc_poll_failing`, `rpc_checkpoint_stale`, `rpc_transaction_quarantined` |
| `[multisig]` configured | `multisig_unmatched_state` |

Only a deployment meeting every condition at once has all of them, and Grafana shows more rules than signals, because each signal generates one rule per notification channel. An operator asking why they never see one of these has probably not met its condition; check the deployment before treating absence as a bug. `microscope.toml` is safe to read. `.env` is not: every variable in it except `MICROSCOPE_DEPLOYMENT` and `MICROSCOPE_ENV` is a credential. The conditions only need to know which variables are set, so read the names alone:

```
grep -oE '^[A-Z_]+=.' .env | cut -d= -f1
```

Every health rule pends before it pages, so a brief fault never notifies at all and a notification timestamp is the end of the pending period, not the start of the fault. Read the onset from the metrics, and read the pending period off the rule in Grafana if the delay matters.

The definitions and their thresholds live in `crates/microscope-indexer/src/alerting.rs`. Each rule carries a `description` annotation that already explains its cause; quote it rather than reinventing an explanation, and use this file for the relationships between alerts, which the annotations cannot express.

## Precedence

Diagnose downward. An alert lower in this list is usually a symptom of one higher up, and reporting it as an independent fault is the most common wrong answer.

1. **The indexer is gone.** Three rules alert on missing data (`yellowstone_endpoint_unreachable`, `rpc_poll_stale`, `rpc_checkpoint_stale`); every other rule treats no data as OK. So a stopped or crash-looping indexer looks like exactly those alerts firing, everything else green, and total silence from the activity alerts. Confirm with `/readyz` and `docker compose ps` before blaming the provider.
2. **`yellowstone_endpoint_unreachable`.** The provider refuses every connection, so no stream is ever established. `microscope_yellowstone_disconnects_total` and `microscope_yellowstone_missed_slots_total` stay flat throughout, because they only move when an established stream goes silent. A flat disconnect counter is not evidence of health here. This alert also explains any concurrent `yellowstone_stream_interrupted` (the subscribe failures) and the silence of every activity alert.
3. **`log_delivery_stalled`.** Decoded transactions are not reaching Loki. This masks more than the activity alerts: `datasource_updates_dropped` and `yellowstone_stream_interrupted` are themselves Loki queries, so they cannot fire while it is true. When this alert is up, treat every Loki-backed signal as unknown, not as negative.
4. **`rpc_recovery_disabled`.** The `reason` label on `microscope_rpc_recovery_degraded` says which of exhausted provider history, a cursor past the confirmed head, or a checkpoint failure caused it. It stays off until the indexer restarts. In Yellowstone mode this converts every future disconnect into permanent loss; in RPC mode nothing is indexed at all. It is the direct cause of `yellowstone_gap_unrecovered` and often of `rpc_checkpoint_stale`.
5. **`rpc_poll_failing`, `rpc_poll_stale`, `rpc_poll_lag`.** The poller is the exception handler for stream gaps, restarts, poison transactions, and RPC-only deployments. Broken, it explains a gap that never closes. `rpc_poll_failing` is the subtle one: enough polls still succeed to keep freshness and lag green, so recovery is partially dead rather than stopped. Read the poll failure logs for the provider error.
6. **Everything else is independent.** `multisig_unmatched_state`, `rpc_transaction_quarantined`, and `rpc_checkpoint_corrupt` are local faults with their own causes.

## Data loss

Step 4 of the procedure requires an explicit answer. Classify by alert:

| Alert | Was activity lost |
|-------|-------------------|
| `yellowstone_endpoint_unreachable` | Everything for the duration, unless `RPC_URL` is set and the poller is healthy |
| `yellowstone_stream_interrupted` | The intervals between interruptions, unless `RPC_URL` is set. Fires only on repeated interruptions in one window: a wedged stream, not a single reconnect, which the fork replays silently |
| `yellowstone_gap_unrecovered` | Yes, permanently. `microscope_yellowstone_missed_slots_total` gives the size. Backfill the interval |
| `datasource_updates_dropped` | The dropped transaction, permanently, unless `RPC_URL` re-delivers it inside the replay window |
| `rpc_recovery_disabled` | Nothing yet in Yellowstone mode, but the next disconnect is unrecoverable. Everything, in RPC mode |
| `rpc_checkpoint_corrupt` | The window the lost checkpoint covered. Recovery restarted from a fresh replay window; backfill that period |
| `rpc_transaction_quarantined` | One transaction per quarantined signature, with no record. The signatures are in the indexer log at error level |
| `log_delivery_stalled` | The records exist in the container log. Alloy retries from its own WAL, so a short Loki outage self-heals and a long one loses the window; re-index it with backfill |
| `rpc_poll_stale`, `rpc_poll_failing`, `rpc_poll_lag` | Not directly. Gap recovery is degraded, so an outage overlapping this becomes loss |
| `rpc_checkpoint_stale` | Not while running. A restart resumes from a stale cursor and re-fetches or misses the uncheckpointed window |
| `multisig_unmatched_state` | No. This is a configuration fault: nothing was recorded for the configured vault because the config names the wrong account |

## Remediation

`docs/operations.md` is authoritative. Its sections map as follows:

| Alert | Section |
|-------|---------|
| `rpc_transaction_quarantined`, `rpc_checkpoint_corrupt` | Clearing a quarantine alert |
| `rpc_recovery_disabled` | RPC recovery disabled itself |
| `multisig_unmatched_state` | Finding the Squads state address |
| `yellowstone_endpoint_unreachable`, `yellowstone_stream_interrupted` | Datasource endpoints |
| Any alert the operator says never fires | An alert never fires |
| Any alert Grafana is still evaluating after a config change | Grafana still evaluates the old alerts |

Neither quarantine alert clears on restart; both need the operator to remove state by hand.
