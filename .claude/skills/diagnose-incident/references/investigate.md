# Investigating

Confirm the diagnosis against live data before stating it. All of the following are read-only and need no operator confirmation.

## Reaching the stack

Establish the deployment shape first, because half the commands below exist in only one of them.

| Shape | How to tell | What is running |
|-------|-------------|-----------------|
| Local stack | `docker compose ps` lists `prometheus`, `loki`, `grafana` | Everything, ports 9090, 9091 and 3000 on loopback |
| Grafana Cloud | Those three are absent; `alloy` runs with `GRAFANA_CLOUD_*` set (Terraform `grafana_cloud`, or `-f docker-compose.cloud.yml`) | Indexer and Alloy only. Ports 9090 and 9091, nothing on 3000 |
| Kubernetes | No compose project at all | A reference, not a supported target (`docs/kubernetes.md`). Reach 9090 and 9091 with `kubectl port-forward`; the rest is the cluster's own tooling |

Nothing is ever published beyond loopback. On a cloud VM, tunnel: `aws ssm start-session ... AWS-StartPortForwardingSession` on AWS, `gcloud compute ssh --tunnel-through-iap -- -L 3000:127.0.0.1:3000 -N` on GCP. Both Terraform targets print the exact command as the `grafana_tunnel` output. Never the public IP.

| What | How | Shape |
|------|-----|-------|
| Indexer metrics | `curl -s localhost:9090/metrics` | all |
| Readiness, with the reason it is not ready | `curl -s localhost:9091/readyz` | all |
| Liveness | `curl -s localhost:9091/healthz` | all |
| Container state and restarts | `docker compose ps` | compose |
| Indexer logs | `docker compose logs --tail 200 indexer` | compose |
| Prometheus query | `docker compose exec prometheus promtool query instant http://localhost:9090 '<expr>'` | local stack |
| Loki query | `docker compose exec grafana wget -qO- '<url>'` against `http://loki:3100/loki/api/v1/query_range?query=...&start=...&end=...&limit=...` | local stack |
| Grafana | `http://localhost:3000` | local stack |

The two `exec` rows work only in the local stack: Prometheus and Loki listen on the compose network, not the host, so a query has to run inside a container, and in Grafana Cloud mode those containers do not exist. `promtool` is the only HTTP client in the Prometheus image, the Loki image is distroless and has none, and the indexer image carries no `curl` or `wget`. The Grafana container is the one to borrow a client from.

In Grafana Cloud mode, query the hosted stack for anything historical, and use the indexer's own metrics and probe endpoints on the VM for the rest. Alloy's own metrics answer whether records are being shipped.

If the stack is unreachable, say the diagnosis is unconfirmed. Do not present a plausible cause as an established one.

## `/readyz`

Returns 503 with the reason. It gates on three things: startup finished, an RPC poll succeeded within the stale threshold (skipped when no poller is configured), and a Yellowstone endpoint probe succeeded within its threshold (armed only in Yellowstone mode). A 503 naming the stream probe and a firing `yellowstone_endpoint_unreachable` are the same fact.

## Metrics

Read `crates/microscope-indexer/src/telemetry.rs` for the authoritative list. The ones that answer diagnostic questions:

| Question | Metric |
|----------|--------|
| Is the provider answering at all | `microscope_yellowstone_probe_healthy`, `microscope_yellowstone_probe_failures_total` |
| Did an established stream drop, and how much did it miss | `microscope_yellowstone_disconnects_total`, `microscope_yellowstone_missed_slots_total` |
| Is gap recovery on, and why not | `microscope_rpc_recovery_enabled`, `microscope_rpc_recovery_degraded` (the `reason` label) |
| Is the poller working | `microscope_rpc_poll_last_success_unixtime`, `microscope_rpc_poll_failures_total`, `microscope_rpc_poll_lag_slots`, `microscope_rpc_poll_head_slot` |
| Is the checkpoint advancing | `microscope_rpc_checkpoint_last_success_unixtime`, `microscope_rpc_checkpoint_slot`, `microscope_rpc_checkpoint_quarantined_files` |
| What was skipped | `microscope_rpc_poll_quarantined_transactions`, `microscope_rpc_history_unavailable_total` |
| Is anything being decoded | `microscope_transactions_total`, `microscope_errors_total`, `microscope_last_event_unixtime` |
| Which signals, at what rate | `microscope_instructions_total{instruction}`, `microscope_program_events_total{event,source,failed}`, `microscope_multisig_activity_total{provider,version,action,failed}` |
| Are records reaching Loki | `loki_write_sent_entries_total` (from Alloy, not the indexer) |

Counters are seeded at zero on startup, so a series present at zero means running and idle, while an absent series means the indexer is gone. That distinction is the point of the seeding; do not read them as equivalent.

## Records in Loki

Everything the indexer decodes is a single-line JSON log on the stream `{service_name="microscope-indexer"}`, discriminated by `kind`. Deployments sharing one Loki (Grafana Cloud) all use that stream name; add `deployment="<MICROSCOPE_DEPLOYMENT>"` to the selector or another deployment's records are counted as this one's. The generated health rules already do.

| `kind` | Fields |
|--------|--------|
| `program_instruction` | `name`, `data`, `program_id`, `instruction_index`, `instruction_path`, `stack_height`, `signature`, `slot`, `block_time`, `failed` |
| `program_event` | `name`, `data`, `source`, `program_id`, `instruction`, plus the same position and transaction fields |
| `event_decode_failure` | `rejected`, `source`, `program_id`, `instruction`, `signature`, `slot`, `block_time` |
| `multisig_activity` | `action`, `provider`, `squads_version`, `idl_version`, `instruction`, `data`, `vault_address`, `multisig_address`, `configured_address_kind`, plus the same position and transaction fields |

Everything else on that stream is an ordinary human-readable indexer log line. To read raw records for one signature:

```
{service_name="microscope-indexer"} | json | signature = "<signature>"
```

Drop the condition filters from a generated alert query before concluding the record is missing: `__error__ = ""` hides records a numeric condition could not convert.

## Backfill

Re-indexing a lost window is `just backfill <since> [args]`, which needs `RPC_URL` and is capped by the endpoint's history depth. It is a state-changing action: propose the exact command and let the operator run it.

Loki rejects entries older than its out-of-order window, so a backfill reaching past that is accepted and silently dropped. Grafana Cloud stacks must push through Alloy (`--loki-url http://alloy:3100`), not Loki directly. Name the window and confirm it fits before proposing the command.
