# Changelog

All notable changes to Solana Microscope will be documented in this file.

The project follows [Semantic Versioning](https://semver.org/). Releases are git
tags of the form `vX.Y.Z`; the tag must match the `microscope-indexer` crate
version and the section heading below.

## [Unreleased]

### Added

- One deployment can monitor several Squads multisigs, of mixed versions. Write
  one `[[multisig]]` table per multisig in `microscope.toml`, or set the new
  `multisigs` Terraform list instead of `multisig`. Existing single-multisig
  configs load unchanged and keep their alert rule UIDs. Multisig alerts cover
  every configured vault and name the one that matched in a `vault_address`
  annotation; a `vault_address` condition narrows one to a single multisig.
  Adding a multisig to a running deployment changes the monitored addresses,
  so RPC recovery stays disabled until the RPC checkpoint is removed (see
  "RPC recovery disabled itself" in `docs/operations.md`).

### Fixed

- Event, instruction and multisig alerts on Slack and Telegram no longer send
  a second "resolved" notification after every match. They go to a new
  `microscope-<channel>-activity` contact point with resolved messages off;
  health alerts keep `microscope-<channel>` and still notify on recovery.
  PagerDuty is unchanged.
- The notification-policy tree no longer declares per-channel routes. Rules
  with a channel name their contact point, and Grafana routes them before the
  tree, so those routes were never used. `<deployment>-routes.json` now holds
  only the route that mutes alerts without a channel, so Grafana Cloud users
  have nothing per channel to merge.
- `scripts/export-grafana-cloud.sh` fails before writing anything when the
  dashboard or a contact point UID would exceed Grafana's 40-character limit.
  The dashboard UID alone caps deployment names at 29 characters; with
  `-telegram-activity`, names longer than 22 characters hit it.

## [0.1.0] — 2026-09-14

Initial public release.

### Added

- Rust indexer that decodes one Solana program, and optionally the Squads
  account controlling it, into Prometheus metrics and Loki logs. Decoder crates
  are generated from the program IDL by `scripts/generate-decoder.sh`, so no
  program-specific code is checked in.
- Two datasources: a Yellowstone gRPC stream with reconnect, `from_slot` replay
  and replay dedup, and an RPC poller with a persisted cursor that covers
  restarts, poison transactions and RPC-only deployments. Either can run alone.
- Squads v3, v4 and smart-account decoding, so multisig proposals, approvals and
  executions on the controlling account are indexed alongside program activity.
- Datasource liveness checks, a from-scratch Yellowstone probe, and a health
  endpoint, because a returned datasource leaves the process alive with nothing
  flowing.
- Grafana dashboards, alert rules, contact points and notification policies
  generated from `microscope.toml` at stack start, covering indexer health, the
  datasources, and the program's own events.
- `just backfill` for historical ingest through Loki or, for Grafana Cloud
  stacks, through Alloy.
- Docker Compose stack (indexer, Prometheus, Loki, Grafana, Alloy) for local and
  single-VM deployments, a `Publish Docker Image` workflow for registry-pull
  runtimes, and a Kubernetes reference in [`docs/kubernetes.md`](docs/kubernetes.md).
- Terraform configurations for [AWS](infra/aws) and [Google Cloud](infra/gcp),
  each deploying the stack into the operator's own account.
- Operations guide in [`docs/operations.md`](docs/operations.md).

[Unreleased]: https://github.com/solana-foundation/solana-microscope/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/solana-foundation/solana-microscope/releases/tag/v0.1.0
