# Deployment inputs

Validate each answer as it arrives using the same rules Terraform enforces at plan time (`infra/*/variables.tf`), so the first `terraform apply` or `docker compose up` succeeds.

## Core inputs (all targets)

| Input | Rule |
|-------|------|
| `program_id` | base58 pubkey, 32-44 chars |
| `multisig.vault_address` | optional; base58 pubkey for the Squads default vault shown in the UI. Omit the entire section when the deployment has no multisig, multisig alerts then become unavailable |
| `multisig.state_address` | required with `vault_address`; the corresponding v3 `Ms`, v4 `Multisig`, or Smart Account/v5 `Settings` account. The indexer verifies locally that it derives the default vault |
| `multisig.version` | required with `vault_address`; one of `v3`, `v4`, `v5`. Explicit, never inferred from the vault |
| IDL | path to a JSON IDL; it must declare events, or decoder generation fails. If the file lives outside the repo, copy it into `idl/` and reference that path |
| datasource | `yellowstone` (default) or `rpc` |
| `geyser_url` | required in Yellowstone mode; `http(s)://` URL |
| `geyser_x_token` | optional in Yellowstone mode; empty is fine |
| `rpc_url` | required in RPC mode and by the backfill command; optional in Yellowstone mode, where setting it enables disconnect and restart recovery and is recommended |
| `poll_interval_seconds` | RPC polling or reconciliation interval; 1-300 seconds, default 5 |
| `replay_window_slots` | crash-safety overlap replayed on first start and restart; 1-100,000 slots, default 300 |
| `grafana_admin_password` | >= 12 chars; offer to generate one with `openssl rand -base64 18` |

`idl_path` is relative to the repo root for local, and to `infra/<cloud>/` for cloud (e.g. `../../idl/my-program.json`).

## Cloud-only inputs

| Input | Notes |
|-------|-------|
| `project_id` | GCP only, required |
| `operators` | GCP only; IAM members granted SSH through IAP + OS Login, e.g. `user:$(gcloud config get-value account)`. Needs project IAM-admin rights; use `[]` on shared projects where an administrator grants `compute.osAdminLogin`, `iap.tunnelResourceAccessor`, and `compute.viewer` outside Terraform |
| `grafana_cloud` | optional; ask whether the deployment ships into a hosted Grafana Cloud stack. When yes, collect the object (`loki_url`, `loki_user`, `prom_url`, `prom_user`, `deployment_name`, with the token through `TF_VAR_...`): the VM then runs only the indexer and Alloy, and `grafana_admin_password` plus channel credentials are unused |

AWS needs no access inputs: the VM has no inbound ports and operators connect with `aws ssm start-session` using their own AWS credentials (`ssm:StartSession` permission) plus the [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) for the AWS CLI (`brew install --cask session-manager-plugin` on macOS).

## Defaults to use without asking

Mention that each can be changed later:

- `name`, `instance_type`/`machine_type`, `disk_size_gb`, `repository_url`, `repository_ref`: keep the variable defaults.
- AWS `region` defaults to `us-east-2`; GCP `region`/`zone` default to `us-central1`/`us-central1-a`. Ask only if the user cares where it runs.
- Yellowstone is the default datasource; keep it when the operator has no preference.
- RPC polling and Yellowstone reconciliation default to 5 seconds with a 300-slot crash replay window.
- Alert timing: keep the global defaults (`lookback_window_seconds = 60`, `evaluation_interval_seconds = 10`).
- An alert with empty `channels` is valid: it evaluates in Grafana through a muted contact point, useful before credentials exist.
