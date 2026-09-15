# Solana Microscope

[![Build](https://github.com/solana-foundation/solana-microscope/actions/workflows/build.yml/badge.svg)](https://github.com/solana-foundation/solana-microscope/actions/workflows/build.yml)
[![Test](https://github.com/solana-foundation/solana-microscope/actions/workflows/test.yml/badge.svg)](https://github.com/solana-foundation/solana-microscope/actions/workflows/test.yml)
[![Security](https://github.com/solana-foundation/solana-microscope/actions/workflows/security.yml/badge.svg)](https://github.com/solana-foundation/solana-microscope/actions/workflows/security.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Self-hosted monitoring and alerting for Solana programs.

Microscope consumes confirmed transactions from Yellowstone gRPC or a polling
Solana JSON-RPC endpoint, decodes program activity with
[Carbon](https://github.com/sevenlabs-hq/carbon), and exposes it through
Prometheus, Loki, and Grafana. It also normalizes Squads v3, v4, and Smart
Account/v5 activity. You operate the stack in your own environment; Solana
Foundation does not host it or receive deployment data.

## What it monitors

| Signal | Record | Description |
| --- | --- | --- |
| Instruction | `program_instruction` | IDL-decoded program instruction |
| Event | `program_event` | IDL-decoded log or direct event-CPI |
| Multisig | `multisig_activity` | Squads instruction mapped to a stable action |
| Decode failure | `event_decode_failure` | Event payload the IDL decoder rejected |

Microscope provides:

- Prometheus metrics and a generated Grafana dashboard
- Structured records in Loki and metrics in Prometheus, both retained 30 days in
  the local stack (`PROMETHEUS_RETENTION` overrides the metrics window); hosted
  Grafana Cloud and Kubernetes deployments follow their own retention
- Config-driven Grafana alerts with optional field conditions
- Slack, Telegram, and PagerDuty delivery
- Local Docker Compose and Terraform deployments for AWS and Google Cloud

Raw program-log string matching is not supported. Alerts target decoded
instructions, decoded events, or normalized Squads activity.

```text
Yellowstone gRPC --.
                   +-> Carbon indexer -> Prometheus ------> Grafana
RPC polling -------'                `-> Alloy -> Loki ----> alerts
```

One deployment monitors one program and, optionally, one Squads multisig.

## Quick start

Requirements:

- Docker Engine with Docker Compose v2
- A Yellowstone gRPC endpoint or Solana JSON-RPC endpoint
- The target program ID and IDL
- Optionally, a Squads default vault, internal state account, and version

[`docs/operations.md`](docs/operations.md) covers sourcing those endpoints,
finding the Squads state address, cost, and troubleshooting.

```sh
git clone https://github.com/solana-foundation/solana-microscope.git
cd solana-microscope
cp microscope.toml.example microscope.toml
```

The checked-in example points at the bundled example IDL. Replace its
program ID, IDL, multisig vault, state account and version, dashboard fields,
and alerts for your deployment.

The bundled [`setup-deployment`](.claude/skills/setup-deployment/SKILL.md)
Claude Code skill automates this step: it interviews you for each value,
derives alert rules from your IDL, writes `microscope.toml` and `.env` — or
`terraform.tfvars` for the cloud deployments below — and validates the result.

Create an ignored `.env` file:

```dotenv
GEYSER_URL=https://your-yellowstone-endpoint:443
GEYSER_X_TOKEN=
# Required in RPC mode and by `just backfill`; optional in Yellowstone mode.
RPC_URL=https://your-solana-rpc-endpoint
GRAFANA_ADMIN_PASSWORD=replace-with-a-strong-password
SLACK_WEBHOOK_URL=
TELEGRAM_BOT_TOKEN=
TELEGRAM_CHAT_ID=
PAGERDUTY_INTEGRATION_KEY=
```

Start the stack:

```sh
just up
docker compose ps
```

Grafana reads alert provisioning only at process startup, so `just up`
regenerates the alerting config and recreates Grafana. Running
`docker compose up --build --detach` directly against an already-running stack
leaves Grafana evaluating the previous rules until you add
`docker compose up --detach --no-deps --force-recreate grafana`.

Open Grafana at <http://localhost:3000>. Indexer metrics are available at
<http://localhost:9090/metrics>, and the health probes at
<http://localhost:9091/healthz> and <http://localhost:9091/readyz>.

```sh
docker compose logs --follow indexer
docker compose down
```

`docker compose down` preserves local Prometheus, Loki, Alloy, and Grafana data.
Add `--volumes` to delete those volumes.

## Configuration

`microscope.toml` is the source of truth for local deployments:

```toml
program_id = "<PROGRAM_ID>"
idl_path = "idl/program.json"

[multisig] # optional; all three values are required when present
vault_address = "<SQUADS_DEFAULT_VAULT>"
state_address = "<SQUADS_STATE_ACCOUNT>"
version = "v4" # v3 | v4 | v5

# Optional; Yellowstone is the default.
[datasource]
mode = "yellowstone" # or "rpc"
poll_interval_seconds = 5
replay_window_slots = 300

[dashboard]
event_fields = ["name", "source", "signature", "slot", "failed", "data.amount"]
multisig_fields = ["action", "squads_version", "instruction", "signature", "slot", "failed"]

[alerting]
lookback_window_seconds = 60
evaluation_interval_seconds = 10

[[alerts]]
kind = "event"
name = "payment_settled"
match = "all"
conditions = [
  { field = "data.amount", operator = "gte", value = 1000 },
  { field = "data.currency", operator = "eq", value = "USDC" },
  { field = "failed", operator = "eq", value = false },
]
severity = "warning"
channels = ["slack"]

[[alerts]]
kind = "multisig"
name = "proposal_approved"
severity = "critical"
channels = ["pagerduty"]
```

Alerts support:

- Kinds: `event`, `instruction`, `multisig`
- Names: the snake_cased instruction or event declared by `idl_path`, or an
  action emitted by the configured multisig version; anything else is rejected
  at startup instead of becoming a rule that can never fire. A few Squads
  actions whose instructions reference no state account (for example
  `multisig_created` and the `program_*` actions) validate but cannot fire for
  a configured deployment
- Severities: `critical`, `error`, `warning`, `info`
- Condition groups: `match = "all"` (default) or `match = "any"`
- Operators: `exists`, `contains`, `eq`, `ne`, `gt`, `gte`, `lt`, `lte`
- Typed string, number, and boolean values; `exists` omits `value`
- `exists` matches non-empty values; a missing field, `null`, and `""` do not
  match, because Loki extracts all three as an empty label
- `ne` against a string or boolean matches only records where the field is
  present, so an absent optional field is not treated as "not equal"
- `contains` requires a non-empty string value
- Integer condition values above 2^53 are rejected because Loki compares
  numbers as float64; quote the value to compare it as an exact string
- Global timing defaults with per-alert overrides; evaluation intervals use 10-second multiples
- Dot-separated JSON field paths
- Multisig alerts fire on successful activity only; failed Squads instructions
  stay visible in the dashboard and metrics but never page. Event and
  instruction alerts keep `failed` as an explicit condition

Dashboard fields are also JSON paths. Missing fields render as empty cells;
complete records remain available in Loki. Every record also carries
`instruction_index`, `instruction_path`, and `stack_height`, identifying the
exact instruction occurrence within the transaction; they are usable in
dashboard fields and alert conditions like any other field. An alert with no channels remains
visible in Grafana but does not send notifications.

See [`microscope.toml.example`](microscope.toml.example) for all options.

## Datasource modes

Omit `[datasource]` to keep the default Yellowstone mode. Set `GEYSER_URL` and,
when required by your provider, `GEYSER_X_TOKEN`. Any endpoint speaking the
Yellowstone gRPC protocol works, whoever operates it.

The stream recovers its own disconnects: the client reconnects with an
exponential backoff, resubscribes from the last slot it saw, and discards what
the replay repeats. A disconnect therefore costs nothing as long as the outage
fits inside the retry budget and the provider still serves the slot being
replayed from, which is why the replay depth your provider offers matters.

`RPC_URL` is still worth setting. Recovery inside the stream lives in the
process, so it covers disconnects but never restarts, outages longer than the
retry budget, history past the provider's replay window, or undecodable
transactions. When `RPC_URL` is set, the indexer reconciles confirmed signatures
through RPC to fill exactly those gaps, skipping whatever the stream already
delivered. The primary datasource remains Yellowstone; `RPC_URL` never changes
the selected mode.

Yellowstone exports `microscope_yellowstone_disconnects_total` and
`microscope_yellowstone_missed_slots_total` through Prometheus. Both count only
the disconnects the stream could not replay through, so a non-zero value means
an interval that RPC recovery has to cover.

Use RPC polling as the primary datasource when low latency is less important
than endpoint cost:

```toml
[datasource]
mode = "rpc"
poll_interval_seconds = 5
replay_window_slots = 300
```

Set `RPC_URL` and leave `GEYSER_URL` empty. RPC mode polls confirmed signatures
for the configured program and optional Squads state account, then fetches each
new transaction through the same decoding pipeline.

RPC polling and Yellowstone reconciliation share an atomic checkpoint under
`MICROSCOPE_STATE_DIR`. Every poll rescans `replay_window_slots` behind the
cursor, including the first run and every restart; signatures retained in the
durable checkpoint suppress the duplicates. The checkpoint only records progress
the pipeline has already consumed, so a crash between queueing a transaction
and Carbon processing it replays that transaction instead of skipping it.
Under sustained pipeline traffic the checkpoint may hold an older snapshot
until the queue drains; the cost is duplicate re-emission after a restart,
never loss. The checkpoint retains signatures covered by that replay window,
so transactions already present in the last durable checkpoint are not
re-emitted after a restart. Temporary RPC failures (outages, rate limits, server errors) never
advance a cursor, never discard a transaction, and leave checkpoints unchanged:
polling retries until the endpoint recovers and then catches up from the
durable checkpoint.

In Yellowstone mode the same checkpoint also absorbs the signatures the stream
delivered, recorded only once an update reaches the pipeline. A poll stalled by
a long RPC outage therefore skips them on recovery instead of emitting a second
record for activity Grafana has already alerted on. Deliveries recorded but not
yet absorbed are lost if the process exits, so a restart during an outage can
still re-emit that last window.

Recovery depends on the endpoint retaining `getSignaturesForAddress` and
`getTransaction` history back to the checkpoint. When consecutive polls confirm
that the endpoint's history no longer reaches it, or that a checkpoint cursor
sits implausibly beyond the endpoint's confirmed head (two consecutive
observations are tolerated, since load-balanced providers can briefly answer
from a lagging node), RPC recovery disables itself instead of silently
creating a gap or
stopping the rest of the stack. Yellowstone continues when it is the
primary datasource; RPC-only deployments remain running but ingest no new
records until restarted against usable history: switch `RPC_URL` to an endpoint
that still has the missing slots, or backfill the missed window and delete the
checkpoint to restart at the current confirmed slot. Only a transaction the
endpoint provably cannot decode (an unsupported transaction version, or a
response that repeatedly fails conversion) is quarantined after bounded
confirmations; quarantining advances the affected cursor without a decoded
record and emits an error and metrics. High-volume programs may use more RPC
request units than a filtered gRPC stream.

RPC recovery exports `microscope_rpc_recovery_enabled`,
`microscope_rpc_poll_started_unixtime`,
`microscope_rpc_poll_last_success_unixtime`, `microscope_rpc_poll_head_slot`,
`microscope_rpc_poll_lag_slots`, `microscope_rpc_poll_transactions_total`,
`microscope_rpc_poll_failures_total`,
`microscope_rpc_poll_transaction_failures_total`,
`microscope_rpc_poll_quarantined_transactions_total`,
`microscope_rpc_poll_quarantined_transactions`,
`microscope_rpc_checkpoint_last_success_unixtime`,
`microscope_rpc_checkpoint_slot`, `microscope_rpc_checkpoint_failures_total`,
`microscope_rpc_checkpoint_corrupt_total`,
`microscope_rpc_checkpoint_quarantined_files`,
`microscope_rpc_history_unavailable_total`,
`microscope_rpc_recovery_degraded`, and
`microscope_rpc_recovery_disabled_total` through Prometheus. Whenever RPC
polling runs, whether as the primary datasource or as Yellowstone gap recovery,
the generated dashboard gains polling freshness, lag, throughput, failure, and
quarantine panels, and stale-poll, poll-failure, lag, quarantine, and
checkpoint-stale alert
rules are provisioned. The lag rule warns when cursors trail the confirmed head by more
than twice the replay window, catching stalls where polls still succeed but
transactions stop being decoded. The poll-failure rule warns when more than one
in twenty of the polls expected in a fifteen-minute window failed, catching a
poller that fails a share of its polls indefinitely: enough succeed to keep
freshness, lag, and readiness green while gap recovery is partially dead.
Those rules, plus the checkpoint-corruption, terminal recovery-degradation,
and datasource dropped-update rules that always exist, and the
stream-interruption and gap rules Yellowstone deployments add, are sent to
every contact channel used by the deployment; without a configured channel,
they remain visible in Grafana but muted.

Decoding a record and delivering it are separate failures, so Alloy's own
delivery metrics are scraped and alerted on as well. Every deployment gets a
rule that fires when the indexer decodes transactions while Alloy ships no log
entries, and one that fires when Alloy drops entries or stops reporting
delivery at all. Without them, a shipping failure leaves the dashboard green
and every activity alert evaluating an empty stream.

### Health probes

The indexer serves two probe endpoints on port 9091, for orchestrators that
restart or gate traffic on them:

| Endpoint | 200 when |
| --- | --- |
| `/healthz` | the process is running and its runtime is scheduling work |
| `/readyz` | startup finished, and the configured datasource is reachable |

`/readyz` returns 503 with the reason in the body otherwise. In RPC mode
"reachable" means a poll succeeded within the same `poll_interval_seconds * 6`
threshold, floored at 60 seconds, that the generated RPC poll staleness alert
uses; the window is seeded at startup, so the first poll has that long to
succeed. A poller failing only a share of its polls stays ready, since the
successful ones keep the window fresh; the generated poll-failure alert covers
that case.

In Yellowstone mode the indexer calls `GetVersion` on the geyser endpoint every
15 seconds, using the same `GEYSER_URL` and `GEYSER_X_TOKEN` as the
subscription, and reports unready once that call has been failing for
`MICROSCOPE_STREAM_STALE_AFTER_SECONDS`, 90 seconds by default. A rejected
token, an unresolvable host or a dead endpoint fails the probe; a single blip
does not, because the window is measured from the last success and six attempts
fit inside it. Set 0 to leave readiness blind to the endpoint. Probe failures
are also exported as `microscope_yellowstone_probe_healthy` and
`microscope_yellowstone_probe_failures_total`.

The probe proves the endpoint and the token are healthy, not that our own
subscription is still being served: a stream wedged against a healthy endpoint
still reports ready. The generated `yellowstone_stream_interrupted` alert covers
that case, once the interruptions are sustained: the datasource logs one timeout
per `STREAM_TIMEOUT`, so the rule counts them over 15 minutes and fires above
five, leaving the single reconnect the client replays silent.

## Decoding and Squads

Run `just generate` after changing the program or IDL. Carbon generates the
instruction and event decoder; Microscope gives log events and event-CPI
events the same JSON shape with `source = "log"` or `"cpi"`. An event payload
that matches an event shape but fails to decode emits an
`event_decode_failure` record and increments
`microscope_event_decode_failures_total`.

The decoder is compiled in, so `program_id` and the IDL are build inputs, not
runtime ones. The generated decoder ignores instructions from any other program
and knows only the instructions and events the build's IDL declared. At startup
the indexer therefore refuses to run when its config names a different
`program_id` or an IDL whose contents differ from the one it was built from,
rather than subscribing successfully and decoding nothing.

For Squads, configure the default vault shown in the UI, its internal state
account, and `version` (`v3`, `v4`, or `v5`). The state account is the v3 `Ms`,
v4 `Multisig`, or Smart Account/v5 `Settings` account. At startup, the indexer
locally verifies that the state account derives the configured default vault;
no RPC lookup is needed. Live indexing and backfill monitor only the state
account because record-producing Squads instructions reference it. Smart
Account/v5 instructions scoped to a policy account instead of the `Settings`
account are therefore not monitored; decoded Squads instructions that reference
another state account are counted by
`microscope_multisig_unmatched_state_total`.

A multisig can hold several vaults, and monitoring the state account covers all
of them, not only the configured `vault_address`. Records report that
configured address in `vault_address` whichever vault an instruction acted
on, so use the vault index in the instruction arguments (v4:
`data.data.args.vault_index`) to narrow an alert to one vault.

Version-specific instructions are normalized into actions such as
`proposal_approved`, `transaction_executed`, `member_added`, and
`threshold_changed`.

Governance changes reach those granular actions only when the executing
instruction carries them. The v5 synchronous settings execution does, so it
emits `settings_transaction_executed` plus one action per applied change. The
v4 config transaction and the v5 asynchronous settings transaction apply
changes from a stored account, so their execution emits only
`configuration_transaction_executed` / `settings_transaction_executed`; the
changes themselves are in the `data` of the matching
`configuration_transaction_created` / `settings_transaction_created` record.
Alert on those creations, not on `member_added`, to catch v4 governance
changes.

Pinned Squads IDL sources are documented in
[`idl/squads/README.md`](idl/squads/README.md). Generated decoder crates are
ignored by Git and should not be edited directly.

## Prebuilt container images

The VM deployment builds the indexer image on the VM. To deploy into a runtime
that pulls rather than builds, run the `Publish Docker Image` workflow from your
fork with your `program_id` and IDL path. It pushes to
`ghcr.io/<your-org>/<your-repo>` tagged with the program ID and with
`<program-id>-<short-sha>`.

Publish from your own fork, not from upstream: one image serves one program and
one IDL, and the workflow needs both as inputs.

The image carries no `microscope.toml`. Mount the deployment config at
`/etc/microscope/microscope.toml` and keep secrets in the environment, as
[`docker-compose.yml`](docker-compose.yml) does. The config must name the
program ID and IDL the image was built from, or the indexer refuses to start.

[`docs/kubernetes.md`](docs/kubernetes.md) is a reference for running the
indexer against a Prometheus, Loki, and Grafana you already operate.

## Cloud deployment

Terraform configurations are available for
[`AWS`](infra/aws) and [`Google Cloud`](infra/gcp):

```sh
cd infra/aws # or infra/gcp
cp terraform.tfvars.example terraform.tfvars
terraform init
terraform plan
terraform apply
```

Each deployment creates a dedicated network, an SSH-only Ubuntu VM, a private
versioned deployment bucket, a cloud secret, and a least-privilege workload
identity. Grafana and metrics remain bound to the VM loopback interface and
are accessed through the generated SSH tunnel.

The initial image build can take several minutes. Configuration, secret,
dashboard, and alert changes reuse the existing image; code and IDL changes
rebuild it on the same VM.

For production:

- Pin `repository_ref` to a commit SHA.
- Use an encrypted remote Terraform backend.
- Pass secrets through a secret-aware CI system or `TF_VAR_...` variables.
- SSH runs through IAP (GCP) or SSM Session Manager (AWS) with no public inbound ports; size compute, disk, and retention for the workload.

See the complete [`cloud deployment guide`](infra/README.md).

### Hosted Grafana Cloud stack

To ship into an existing Grafana Cloud stack instead of running Grafana,
Loki, and Prometheus on the VM, set the `GRAFANA_CLOUD_*` and
`MICROSCOPE_DEPLOYMENT` variables from `.env.example` and start only the
indexer and Alloy:

```sh
docker compose -f docker-compose.yml -f docker-compose.cloud.yml up -d
```

`MICROSCOPE_DEPLOYMENT` is a label the deployment attaches to its own data,
not a permission. Dashboards, alert rules, and notification routes filter on
it, but Grafana Cloud does not bind a write token to a label: every deployment
sharing a stack can write under any deployment name. A leaked token or a
compromised VM can therefore forge another deployment's logs and metrics, and
so its alerts, including keeping a victim's series alive through an outage.
Give each deployment its own revocable access policy token so a leak can be
cut off individually, and put deployments from different trust domains in
separate Grafana Cloud stacks.

Switching an already-running local stack over: the overlay only disables
Grafana, Loki, Prometheus, and the alerting-config job, it does not stop
them. Run `docker compose --profile local-stack down` first, otherwise the
old Grafana keeps evaluating and delivering alerts alongside the hosted
stack.

Generate the matching Grafana resources for a deployment name and an
existing dashboard folder UID:

```sh
RPC_URL=<endpoint> ./scripts/export-grafana-cloud.sh <deployment> <folder-uid> [config]
```

The export refuses to run when `[config]` targets a different program than the
checked-out decoder, so run `just generate` for the deployment you are
exporting.

`RPC_URL` is mandatory because it decides whether the RPC polling health
alerts and dashboard panels are generated. Pass the deployment's endpoint
when it runs RPC recovery or the RPC datasource, and `RPC_URL=` when it
runs neither. Only presence matters, so a placeholder endpoint is enough;
the real secret stays on the VM.

The exported files are deployment-specific and stay untracked. Import
`dashboards/<deployment>.json` through the Grafana UI (Dashboards → New →
Import). Alert rules, contact points, and mute timings have no UI import;
create them in the alerting UI using the exported JSON as the reference, or
`PUT` each file to the [Grafana provisioning API](https://grafana.com/docs/grafana/latest/developers/http_api/alerting_provisioning/)
with a token of your own:

```sh
curl -X PUT "$GRAFANA_URL/api/v1/provisioning/alert-rules/$(jq -r .uid rule.json)" \
  -H "Authorization: Bearer $GRAFANA_API_TOKEN" \
  -H "Content-Type: application/json" -H "X-Disable-Provenance: true" \
  -d @rule.json
```

Contact-point JSON keeps `$SLACK_WEBHOOK_URL`-style placeholders; substitute
real secrets only at push time, never in the committed files.

Pushing only creates and updates. Rules and contact points that disappear from
an export stay active in the tenant, and because rule UIDs are content-derived,
editing an alert replaces its UID and leaves the old rule firing. The script
prints the exact `DELETE` calls for everything that vanished since the last
export from this checkout; run them or the stale resources keep evaluating and
delivering to old channels.

## Backfilling history

The live indexer only sees activity from the moment it starts. To load past
activity, run the backfill subcommand against a regular RPC endpoint with the
stack running:

```sh
RPC_URL=https://your-rpc-endpoint just backfill 7d
```

It crawls the program plus the configured multisig state account backwards
through `getSignaturesForAddress`, decodes transactions with the same pipeline as the
live indexer, and pushes the records to Loki backdated to each transaction's
block time. Backdated records appear at their historical position in the
dashboard and never match the alert lookback window, so a backfill does not
trigger notifications.

`--since` accepts `s`, `m`, `h`, `d`, and `w` units and must stay within
Loki's `reject_old_samples_max_age` and `retention_period` (both 720h in the
provided configuration); the command refuses deeper backfills.

Grafana Cloud deployments run no local Loki; push through Alloy instead, which
adds the deployment labels and Cloud credentials the hosted stack expects.
Hosted endpoints do not expose their limits, so state the depth cap explicitly:

```sh
RPC_URL=https://your-rpc-endpoint just backfill 7d --loki-url http://alloy:3100 --loki-max-age 30d
```

Backfill is all-or-nothing: any conversion, processing, missing-block-time, or
interruption failure aborts the run before anything is pushed to Loki.
Prometheus metrics are not backfilled, and a window that overlaps
already-indexed activity can duplicate rows in the event tables.

## Persistence and limitations

Docker volumes preserve Prometheus, Loki, Alloy, Grafana, and RPC recovery
state across container restarts, configuration updates, and `docker compose
down`. Invalid checkpoint JSON is renamed with a `.corrupt-<timestamp>` suffix
and recovery starts from a fresh replay window. The corruption alert stays
asserted while any such file exists: backfill the period the lost checkpoint
covered, then delete the file to resolve it. A checkpoint whose cluster,
program, or monitored addresses do not match the deployment disables recovery
to prevent cross-target reuse. Correct the configuration or endpoint first. If
the target intentionally changed, backfill any uncovered period, then remove the
old checkpoint and restart:

```sh
docker compose stop indexer
docker compose run --rm --entrypoint sh indexer \
  -c 'rm -f /var/lib/solana-microscope/rpc-polling.json'
docker compose up -d indexer
```

VM replacement, `terraform destroy`, or `docker compose down --volumes` removes
local history and checkpoints. Recovery then starts from a fresh replay window;
use `backfill` for any earlier gap.

Microscope is a single-node stack. High availability, external object storage,
automated backups, and history migration are outside its current scope.

## Development

Local development requires Rust 1.97, Node.js 24, Python 3.11 or newer, Just,
Terraform 1.8 or newer for infrastructure work, and `jq` for
`scripts/export-grafana-cloud.sh`.

```sh
just setup
just build
just check
just test
just infra-check
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for contribution guidelines,
[`SECURITY.md`](SECURITY.md) for private vulnerability reporting, and
[`CHANGELOG.md`](CHANGELOG.md) for release history.

## License

Solana Microscope is available under the [MIT License](LICENSE).
