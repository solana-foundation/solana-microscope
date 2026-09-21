# solana-microscope

Rust indexer + Prometheus/Loki/Grafana docker-compose stack, templated with Terraform.
One deployment monitors one Solana program (optionally one Squads-controlled account).
Users clone and deploy in their own cloud; we never host.

## Gotchas

### Carbon comes from two places, at the same version

- Rust crates: plain `carbon-* = "2.0.0"` from crates.io. Never go below that:
  1.0.0 predates the v1 transaction migration (SIMD-0385) and the opt-in
  yellowstone reconnect, so it does not work here.
- npm generator (`@sevenlabs-hq/carbon-cli`, `@sevenlabs-hq/carbon-codama-renderer`):
  exact `2.0.0` from the npm registry, not a caret range, because the generator
  version decides the generated decoder source.

Keep both at the same carbon version; bump them together, in their own pull request.

Carbon pins several deps with `=`, so a carbon bump can force microscope's floors
up (`solana-transaction-status`, `solana-client`, `yellowstone-grpc-{client,proto}`).
A `cargo check` failing on "all possible versions conflict" is that, not a real break.

### Decoder crates are generated, never committed

`crates/{program,squads-v3,squads-v4,squads-smart-account}-decoder/` are gitignored
output of `scripts/generate-decoder.sh` (`just generate`), which needs
`microscope.toml` to exist. Each has its own `[workspace]` and is in the root
`exclude` list.

- Every Justfile target that compiles depends on `generate` first. Bare `cargo build`
  on a fresh clone fails.
- `fmt`/`clippy` use `-p microscope-indexer` and `--no-deps` deliberately. `--all`
  reaches into generated code and gates CI on codegen lints.
- The script rewrites the IDL's program name to `program` so the generated Rust API
  is stable across IDLs. It no longer patches the generated `Cargo.toml`: the
  generator emits the matching `carbon-core` and `solana-*` versions itself.
- Regeneration is gated by `.microscope-source.sha256` over IDL + script +
  `package-lock.json`. Editing the generated crates directly is silently discarded.

### Stream reconnect is opt-in, and we opt in

`yellowstone-grpc-client` reconnects, replays from `from_slot` and dedups the
replay, but only when a `ReconnectConfig` is set. Carbon defaults to none, so
`datasource.rs` sets one through `YellowstoneGrpcClientConfig::with_reconnect`.
Two coupled invariants:

- `STREAM_TIMEOUT` must outlast the reconnect backoff budget. Carbon abandons a
  silent stream and resubscribes at the live head, throwing the replay
  checkpoint away, so a shorter timeout silently defeats the replay. There is a
  test for this.
- The keepalive ping carries the current filters. The client's sink remembers
  the last request it sent and a reconnect resubscribes with it, so a filterless
  ping would leave reconnected streams subscribed to nothing.

`microscope_yellowstone_missed_slots_total` therefore counts only what the
stream could *not* replay through. The RPC poller is the exception handler for
that, plus restarts, poison transactions and RPC-only deployments.

Both that counter and `microscope_yellowstone_disconnects_total` move only when
an *established* stream goes silent past `STREAM_TIMEOUT`. A subscribe that
never succeeds notifies nothing, so an endpoint refusing every connection
leaves both flat while nothing is indexed. `microscope_yellowstone_probe_healthy`
is the signal there, because the probe dials from scratch instead of watching
the stream; do not read a flat disconnect counter as a healthy datasource.

### Grafana artifacts are generated at stack start, not checked in

`alerting-config` (a one-shot run of the indexer image) turns `microscope.toml`
into dashboards, alert rules, contact points, and notification policies on shared
volumes. Grafana provisions only at boot, so `just up` runs `alerting-config`
first and then force-recreates Grafana. Changing alerts means re-running both,
not restarting Grafana alone.
Root-level `dashboards/`, `alert-rules/`, `notification-policies/` are gitignored
exporter output from a live Grafana Cloud stack, not sources.

### Datasource liveness must be checked by us

- Carbon only *logs* a datasource that returns; the process stays alive and healthy
  with nothing flowing. Liveness lives in `health.rs` / `telemetry.rs` / generated
  alerts, not in Carbon.
- `carbon-prometheus-metrics` binds 127.0.0.1 only, unreachable from other containers,
  so the indexer serves its own metrics endpoint.
- Alert-generation tests assert on Carbon's log wording (`alerting.rs`); a crate
  upgrade that rewords it fails there by design.

### Backfill vs Loki

Loki rejects entries outside its out-of-order window (`max_chunk_age/2`), so
`just backfill --since` past that window silently drops. Grafana Cloud stacks must
push through Alloy (`--loki-url http://alloy:3100`), not Loki directly.

### Never commit

`.env`, `microscope.toml`, `*.tfvars`, `tfstate`, `tfplan`, or anything under a
`.terraform/`. Avoid `git add -A` anywhere near `infra/` - it has leaked before.
