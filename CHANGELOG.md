# Changelog

All notable changes to Solana Microscope will be documented in this file.

The project follows [Semantic Versioning](https://semver.org/). Releases are git
tags of the form `vX.Y.Z`; the tag must match the `microscope-indexer` crate
version and the section heading below.

## [Unreleased]

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
