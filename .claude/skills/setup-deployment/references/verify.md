# Verifying a deployment config

Loop until clean, fixing the config rather than the checks.

## Local

`just generate-alerting` fully parses `microscope.toml`, regenerates the decoder, and renders the Grafana alert rules. The first run builds the Rust workspace and can take several minutes.

It loads `.env` through the Justfile's `dotenv-load`, which has two consequences:

- `RPC_URL` presence there decides whether the RPC health rules and panels are generated.
- A missing credential for a selected channel fails the run with `<VAR> must be set because an alert uses the <channel> channel`.

Confirm `.env` has a non-empty `GEYSER_URL` in Yellowstone mode, `RPC_URL` in RPC mode, `GRAFANA_ADMIN_PASSWORD`, and a credential for every channel any alert selects. `RPC_URL` is also required for backfill; if Yellowstone has no `RPC_URL`, explicitly report automatic gap recovery as disabled.

Then point the user at `just up`.

## Cloud

```bash
terraform -chdir=infra/<cloud> fmt
terraform -chdir=infra/<cloud> init -backend=false && terraform -chdir=infra/<cloud> validate
```

Variable validation rules only execute at plan time, so if cloud credentials are configured also run `terraform -chdir=infra/<cloud> plan`. If not, say so and note the rules run on the user's first `plan`/`apply`.

Check the VM will be able to clone `repository_url` before anything is applied:

```bash
GIT_TERMINAL_PROMPT=0 git -c credential.helper= ls-remote <repository_url>
```

Plain `git ls-remote` lies here: locally stored credentials make a private repository look public, and the VM clones anonymously. If the check fails, switch `repository_url` to the SSH form (`git@github.com:...`) and route the deploy key through `TF_VAR_repository_deploy_key`.

Warn cloud users that an apply which rotates the runtime secret can fail with the google provider bug "Provider produced inconsistent final plan"; rerunning the same apply succeeds.
