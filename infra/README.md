# Cloud deployment

The AWS and GCP directories each deploy the same single-VM Docker Compose stack:

- Carbon-based Microscope indexer
- Prometheus metrics
- Loki event logs, collected by Grafana Alloy
- Provisioned Grafana dashboard
- Grafana alert rules and Slack, Telegram, and PagerDuty contact points

In local-stack mode Grafana and Prometheus remain bound to the VM's loopback
interface (in `grafana_cloud` mode neither runs). The VM accepts SSH only
through GCP IAP tunneling or AWS SSM Session Manager, with no public inbound
ports. AWS operators need the [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
installed alongside the AWS CLI; use the Terraform output's SSH
tunnel to reach Grafana.

## Inputs and secrets

Both deployments require:

- a program ID and local IDL file;
- optionally, a Squads default vault, internal state account, and explicit v3/v4/v5 version;
- decoded-event table fields;
- alert timing defaults and program event, instruction, or Squads multisig rules;
- a Yellowstone gRPC URL and optional token, or a Solana JSON-RPC URL for RPC mode;
- a Solana RPC URL in RPC mode, or optionally in Yellowstone mode for automatic gap recovery and backfill;
- a Grafana administrator password;
- credentials for every Slack, Telegram, or PagerDuty channel selected by a rule;
- cloud-account permissions to connect through AWS SSM or Google Cloud IAP.

Setting the optional `grafana_cloud` variable switches the VM to the hosted
Grafana Cloud overlay: only the indexer and Alloy run, logs and metrics ship
to the hosted stack, and the Grafana password and channel credentials are
unused (alert delivery happens through the hosted stack's contact points; see
the README's Grafana Cloud section for exporting dashboards and alert rules).

Private repositories can be cloned with a read-only SSH deploy key. Register
the public half as a deploy key on the repository, set `repository_url` to its
SSH URL, and pass the private half outside `terraform.tfvars`:

```sh
export TF_VAR_repository_deploy_key="$(cat /path/to/deploy-key)"
```

The key is stored with the other runtime credentials, written to a temporary
root-only file for the clone, and deleted immediately afterward. Leave the
variable empty for public repositories.

Both providers accept a branch, tag, or commit through `repository_ref`.
Branches do not need to be the repository's default branch:

```hcl
repository_ref = "feat/my-branch"
```

Terraform renders `microscope.toml` and publishes it with the IDL and a desired
deployment manifest to a private, versioned cloud bucket. Runtime and
contact-point credentials are written to the cloud's secret manager. The VM
receives narrowly scoped access to that bucket and secret. Sensitive values are
still present in Terraform state, so use an encrypted remote backend with
restricted access for shared or production deployments. Never commit
`terraform.tfvars` or state files.

## Datasource configuration

Yellowstone remains the default:

```hcl
datasource = {
  mode                = "yellowstone"
  replay_window_slots = 300
}
geyser_url     = "https://your-yellowstone-endpoint:443"
geyser_x_token = "..."
rpc_url        = "https://your-solana-rpc-endpoint"
```

When `rpc_url` is non-empty, Yellowstone remains primary and RPC automatically
reconciles disconnect and restart gaps. Leave it empty only when automatic
recovery and backfill are not needed.

Use RPC-only polling without a gRPC endpoint:

```hcl
datasource = {
  mode                  = "rpc"
  poll_interval_seconds = 5
  replay_window_slots   = 300
}
rpc_url        = "https://your-solana-rpc-endpoint"
geyser_url     = ""
geyser_x_token = ""
```

Terraform stores endpoint URLs in the cloud runtime secret and in Terraform
state. Use an encrypted, access-controlled remote backend in shared or
production environments. RPC checkpoints live in the indexer's Docker volume,
which survives deployments and VM reboots but not VM replacement or
`terraform destroy`. Terminal RPC history or checkpoint identity failures
disable recovery without stopping Yellowstone or the observability stack and
raise the generated recovery-degradation alert. Corrupt JSON is moved aside
automatically. See the main README's persistence section before manually
removing a mismatched checkpoint.

## Alert configuration

The application owns the default decoded-event and multisig columns.
Optionally set `dashboard.event_fields` or `dashboard.multisig_fields` to
replace them with deployment-specific JSON paths; a path absent from a record
is rendered as an empty cell.

```hcl
dashboard = {
  event_fields    = ["name", "data.amount"]
  multisig_fields = ["action", "squads_version", "signature", "failed"]
}
```

The `alerting` input sets global evaluation defaults. Each object in `alerts`
can override either timing and can select any combination of `slack`,
`telegram`, and `pagerduty`. Set `match` to `all` or `any` to combine typed
conditions. Evaluation intervals must be multiples of Grafana's 10-second
scheduler interval. Terraform rejects a selected channel when its credential is
empty, except in `grafana_cloud` mode, where delivery uses the hosted stack's
contact points.

Alert `name` must be a snake_cased instruction or event from the IDL, or a
Squads action for the configured multisig version. Terraform cannot check
this; a wrong name applies cleanly and then fails the reconcile on the VM.
Check `bootstrap_log` when a revision does not apply.

```hcl
alerting = {
  lookback_window_seconds      = 60
  evaluation_interval_seconds = 10
  # Health-alert timings. Omit any of them to take the indexer's default.
  rpc_poll_sustained_failure_seconds = 45
  health_pending_period_seconds      = 300
  multisig_unmatched_window_seconds  = 3600
}

alerts = [
  {
    kind  = "event"
    name  = "recurring_transfer_event"
    match = "all"
    conditions = [
      { field = "data.amount", operator = "gt", value = 0 },
      { field = "failed", operator = "eq", value = false },
    ]
    severity = "warning"
    channels = ["slack"]
  },
  {
    kind                        = "multisig"
    name                        = "proposal_approved"
    severity                    = "critical"
    channels                    = ["pagerduty"]
    lookback_window_seconds     = 300
    evaluation_interval_seconds = 30
  }
]

slack_webhook_url         = "https://hooks.slack.com/services/..."
pagerduty_integration_key = "..."
```

Rules with an empty `channels` collection are evaluated in Grafana but routed
through an always-muted contact point for local or pre-delivery validation.

## AWS

Authenticate Terraform with the target AWS account, then:

```sh
cd infra/aws
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars.
terraform init
terraform apply
```

This creates a dedicated VPC, one public subnet, an encrypted Ubuntu VM, a
private versioned S3 deployment bucket, and a Secrets Manager secret. The
default VM is `t3.medium`; adjust `instance_type` when required.

## Google Cloud

Authenticate with Application Default Credentials and select a project where
billing is enabled, then:

```sh
gcloud auth application-default login
cd infra/gcp
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars.
terraform init
terraform apply
```

This enables the required project APIs and creates a dedicated VPC, static IP,
shielded Ubuntu VM, private versioned Cloud Storage deployment bucket, and
Secret Manager secret. The default VM is `e2-standard-2`; adjust `machine_type`
when required.

## First boot and access

Cloud-init installs Docker and a `microscope-deploy` systemd service and timer.
On GCP it also installs `microscope-metadata-guard.service`, which inserts a
`DOCKER-USER` rule rejecting container traffic to the instance metadata server
(`169.254.169.254:80`); AWS relies on IMDSv2 with a hop limit of 1 instead.
The reconciler downloads the desired manifest, credentials, IDL, and
`microscope.toml`; verifies the config and IDL against the manifest's SHA-256
digests and aborts on a mismatch; clones and checks out `repository_ref`;
builds the indexer; generates the Grafana dashboard and alerting resources
(local-stack mode only; in `grafana_cloud` mode no provisioning is generated
on the VM); starts the stack; and records the applied deployment revision. The
timer checks for a new desired revision every minute.

The Rust indexer build on first boot can take several minutes. Use the
`bootstrap_log` output to follow initial and subsequent reconciliations. Once
`/opt/solana-microscope/ready` exists, open the tunnel shown by
`grafana_tunnel` and browse to <http://localhost:3000>. The tunnel is unused
in `grafana_cloud` mode, where dashboards live in the hosted stack.

Set `repository_ref` to an immutable commit for reproducible production
deployments. Run `terraform destroy` in the corresponding directory to remove
the cloud resources.

## Remote state

The GCP module ships a partial backend (`backend "gcs" {}`), so `terraform
init` needs the bucket and prefix; without them it prompts. Create a versioned
bucket first — state holds endpoint URLs and tokens in cleartext, so keep it
access-controlled:

```sh
gcloud storage buckets create gs://<state-bucket> \
  --project <project> --location <region> --uniform-bucket-level-access
gcloud storage buckets update gs://<state-bucket> --versioning
```

One module directory can serve several deployments, each with its own var file,
state prefix, and data directory. Sharing one `.terraform` makes each `init`
reconfigure the previous deployment's backend, so keep `TF_DATA_DIR` distinct:

```sh
TF_DATA_DIR=.terraform-<deployment> terraform init \
  -backend-config="bucket=<state-bucket>" \
  -backend-config="prefix=gcp/<deployment>"
```

Add `-migrate-state` to move an existing local state file, then confirm
`terraform plan` reports no changes before applying anything else.

Credentials need not live in `terraform.tfvars`: Terraform reads any variable
from `TF_VAR_<name>`, so a secret manager can inject them per invocation. A
variable left in a var file silently shadows the environment, because
`-var-file` takes precedence. Keep `repository_deploy_key` in mind when
injecting a private-repository deployment this way: omit it and the VM cannot
clone, so the reconcile fails at `git clone` and retries every minute while the
running container keeps serving the previous configuration.

### The manifest object needs -replace

`hashicorp/google` (through 7.39.0) fails an in-place content update of
`google_storage_bucket_object` with `Provider produced inconsistent final
plan`, reporting `crc32c` and `generation` as unknown after reporting them
known. The apply aborts midway, which can leave a new secret version created
and the old one destroyed while the manifest still pins the destroyed version —
a deployment that cannot bootstrap until the apply completes. Force replacement
whenever the manifest content changes:

```sh
terraform apply -replace=google_storage_bucket_object.manifest \
  -var-file=<deployment>.tfvars
```

Recover an aborted apply by rerunning it; the reconciler picks up the rewritten
revision within a minute.

## Updating a deployment

Run `terraform apply` after changing application configuration. Terraform
publishes a new manifest after its config, IDL, and secret version are ready.
Within one minute, the existing VM applies the revision:

- dashboard, alert, and secret changes recreate the indexer and regenerate
  local provisioning without rebuilding the Rust image; in `grafana_cloud`
  mode re-export and push the hosted resources yourself;
- IDL and `program_id` changes rebuild the indexer image so its generated
  decoder matches the uploaded inputs;
- a `repository_ref` change fetches and checks out the selected commit,
  rebuilding only when the resolved commit differs; a `repository_url` change
  discards the old checkout and re-clones;
- adding `grafana_cloud` to an existing deployment stops and removes the VM's
  Grafana, Loki, Prometheus, and alerting-config containers before starting
  the overlay;
- an unchanged revision is a no-op;
- a failed revision clears the applied marker, so the timer retries the full
  reconcile until it succeeds.

Run the reconciler immediately instead of waiting for the timer when needed:

```sh
sudo systemctl start microscope-deploy.service
sudo systemctl status microscope-deploy.service
```

The desired manifest contains the literal `repository_ref`; Terraform cannot
detect a branch moving when that input remains unchanged. Use an immutable
commit SHA for predictable deployments.

Application inputs are no longer embedded in cloud-init. A VM replacement is
only required when the bootstrap template, shared reconciler, or an underlying
infrastructure identifier changes. On AWS that replacement is automatic
(`user_data_replace_on_change`); on GCP a changed template only rewrites
instance metadata and cloud-init never re-runs, so apply it explicitly with
`terraform apply -replace=google_compute_instance.this`. Docker volumes still
live on the VM's root disk in this proof of concept, so a VM replacement loses
its local history.

### Migrating an existing one-shot deployment

Deployments created before the reconciler need one final replacement so the
systemd service and timer are installed:

```sh
# in infra/aws
terraform apply -replace=aws_instance.this
# in infra/gcp
terraform apply -replace=google_compute_instance.this
```

Do not force replacement for later config, IDL, secret, alert, dashboard, or
repository-ref changes.
