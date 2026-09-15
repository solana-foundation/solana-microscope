#!/usr/bin/env bash
set -Eeuo pipefail

readonly LOG_FILE="${MICROSCOPE_LOG_FILE:-/var/log/microscope-deploy.log}"
exec >>"$LOG_FILE" 2>&1

export PATH="/snap/bin:$PATH"

readonly BOOTSTRAP_ENV="${MICROSCOPE_BOOTSTRAP_ENV:-/etc/solana-microscope/bootstrap.env}"
readonly INSTALL_ROOT="${MICROSCOPE_INSTALL_ROOT:-/opt/solana-microscope}"
readonly REPOSITORY_DIR="$INSTALL_ROOT/repository"
readonly STATE_DIR="${MICROSCOPE_STATE_DIR:-/var/lib/solana-microscope}"
readonly APPLIED_REVISION_FILE="$STATE_DIR/applied-revision"
readonly BUILT_INPUTS_FILE="$STATE_DIR/built-inputs"

log() {
  printf '%s %s\n' "$(date --utc +%FT%TZ)" "$*"
}

decode() {
  printf '%s' "$1" | base64 --decode
}

retry() {
  local attempts="$1"
  shift

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if "$@"; then
      return 0
    fi
    sleep 2
  done
  return 1
}

gcp_access_token() {
  curl --fail --silent --show-error \
    --header 'Metadata-Flavor: Google' \
    'http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token' |
    jq --raw-output .access_token
}

fetch_object() {
  local object="$1"
  local destination="$2"

  case "$CLOUD_PROVIDER" in
    aws)
      aws s3 cp \
        "s3://$DEPLOYMENT_BUCKET/$object" \
        "$destination" \
        --only-show-errors \
        --region "$AWS_REGION"
      ;;
    gcp)
      local access_token encoded_object
      access_token="$(gcp_access_token)"
      encoded_object="$(jq -nr --arg value "$object" '$value | @uri')"
      curl --fail --silent --show-error \
        --header "Authorization: Bearer $access_token" \
        "https://storage.googleapis.com/storage/v1/b/$DEPLOYMENT_BUCKET/o/$encoded_object?alt=media" \
        --output "$destination"
      ;;
    *)
      log "unsupported cloud provider: $CLOUD_PROVIDER"
      return 1
      ;;
  esac
}

verify_digest() {
  local file="$1"
  local expected="$2"
  local actual

  actual="$(sha256sum "$file" | cut -d' ' -f1)"
  if [[ "$actual" != "$expected" ]]; then
    log "$file does not match the manifest digest $expected"
    return 1
  fi
}

fetch_secret() {
  local version="$1"
  local destination="$2"

  case "$CLOUD_PROVIDER" in
    aws)
      aws secretsmanager get-secret-value \
        --secret-id "$RUNTIME_SECRET" \
        --version-id "$version" \
        --region "$AWS_REGION" \
        --query SecretString \
        --output text >"$destination"
      ;;
    gcp)
      local access_token
      access_token="$(gcp_access_token)"
      curl --fail --silent --show-error \
        --header "Authorization: Bearer $access_token" \
        "https://secretmanager.googleapis.com/v1/projects/$GCP_PROJECT_ID/secrets/$RUNTIME_SECRET/versions/$version:access" |
        jq --raw-output .payload.data |
        base64 --decode >"$destination"
      ;;
    *)
      log "unsupported cloud provider: $CLOUD_PROVIDER"
      return 1
      ;;
  esac
}

if [[ ! -r "$BOOTSTRAP_ENV" ]]; then
  log "missing bootstrap configuration: $BOOTSTRAP_ENV"
  exit 1
fi

# This root-owned file contains only cloud resource identifiers encoded to keep
# shell metacharacters out of the environment file.
# shellcheck disable=SC1090
source "$BOOTSTRAP_ENV"

readonly CLOUD_PROVIDER="$(decode "$CLOUD_PROVIDER_B64")"
readonly DEPLOYMENT_BUCKET="$(decode "$DEPLOYMENT_BUCKET_B64")"
readonly DEPLOYMENT_MANIFEST="$(decode "$DEPLOYMENT_MANIFEST_B64")"
readonly RUNTIME_SECRET="$(decode "$RUNTIME_SECRET_B64")"
readonly AWS_REGION="$(decode "${AWS_REGION_B64:-}")"
readonly GCP_PROJECT_ID="$(decode "${GCP_PROJECT_ID_B64:-}")"

install -d -m 0755 "$INSTALL_ROOT" "$STATE_DIR"
exec 9>"$STATE_DIR/deploy.lock"
if ! flock --nonblock 9; then
  log "another reconciliation is already running"
  exit 0
fi

work_dir="$(mktemp --directory "$INSTALL_ROOT/.deploy.XXXXXX")"
deploy_key_file=""
cleanup() {
  if [[ -n "$deploy_key_file" ]]; then
    rm -f "$deploy_key_file"
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

manifest_file="$work_dir/deployment.json"
secret_file="$work_dir/runtime-secret.json"
config_file="$work_dir/microscope.toml"
idl_file="$work_dir/program.json"

fetch_object "$DEPLOYMENT_MANIFEST" "$manifest_file"
jq --exit-status '
  .schema_version == 1 and
  (.revision | type == "string" and length > 0) and
  (.repository_url | type == "string" and length > 0) and
  (.repository_ref | type == "string" and length > 0) and
  (.config_object | type == "string" and length > 0) and
  (.config_sha256 | type == "string" and length > 0) and
  (.idl_object | type == "string" and length > 0) and
  (.idl_sha256 | type == "string" and length > 0) and
  (.secret_version | type == "string" and length > 0)
' "$manifest_file" >/dev/null

desired_revision="$(jq --raw-output .revision "$manifest_file")"
if [[ -f "$APPLIED_REVISION_FILE" ]] && [[ "$(<"$APPLIED_REVISION_FILE")" == "$desired_revision" ]]; then
  exit 0
fi

repository_url="$(jq --raw-output .repository_url "$manifest_file")"
repository_ref="$(jq --raw-output .repository_ref "$manifest_file")"
config_object="$(jq --raw-output .config_object "$manifest_file")"
config_sha256="$(jq --raw-output .config_sha256 "$manifest_file")"
idl_object="$(jq --raw-output .idl_object "$manifest_file")"
idl_sha256="$(jq --raw-output .idl_sha256 "$manifest_file")"
secret_version="$(jq --raw-output .secret_version "$manifest_file")"

log "reconciling deployment revision $desired_revision"
rm -f "$APPLIED_REVISION_FILE"
fetch_secret "$secret_version" "$secret_file"
fetch_object "$config_object" "$config_file"
fetch_object "$idl_object" "$idl_file"
verify_digest "$config_file" "$config_sha256"
verify_digest "$idl_file" "$idl_sha256"

repository_deploy_key="$(jq --raw-output '.repository_deploy_key // empty' "$secret_file")"
if [[ -n "$repository_deploy_key" ]]; then
  install -d -m 0700 /root/.ssh
  deploy_key_file="$(mktemp /root/.microscope-deploy-key.XXXXXX)"
  chmod 0600 "$deploy_key_file"
  printf '%s\n' "$repository_deploy_key" >"$deploy_key_file"
  export GIT_SSH_COMMAND="ssh -i $deploy_key_file -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
fi
unset repository_deploy_key

if [[ -d "$REPOSITORY_DIR/.git" ]] &&
  [[ "$(git -C "$REPOSITORY_DIR" remote get-url origin)" != "$repository_url" ]]; then
  log "discarding the checkout of the previous repository"
  rm -rf "$REPOSITORY_DIR"
fi

fresh_clone=false
if [[ ! -d "$REPOSITORY_DIR/.git" ]]; then
  if [[ -e "$REPOSITORY_DIR" ]]; then
    log "$REPOSITORY_DIR exists but is not a Git repository"
    exit 1
  fi
  git clone --no-checkout "$repository_url" "$REPOSITORY_DIR"
  fresh_clone=true
else
  git -C "$REPOSITORY_DIR" fetch --force --prune --prune-tags --tags origin
fi

if git -C "$REPOSITORY_DIR" rev-parse --verify --quiet "refs/remotes/origin/$repository_ref^{commit}" >/dev/null; then
  resolved_ref="refs/remotes/origin/$repository_ref"
elif git -C "$REPOSITORY_DIR" rev-parse --verify --quiet "refs/tags/$repository_ref^{commit}" >/dev/null; then
  resolved_ref="refs/tags/$repository_ref"
elif git -C "$REPOSITORY_DIR" rev-parse --verify --quiet "$repository_ref^{commit}" >/dev/null; then
  resolved_ref="$repository_ref"
else
  log "repository_ref does not resolve to a commit: $repository_ref"
  exit 1
fi

resolved_commit="$(git -C "$REPOSITORY_DIR" rev-parse "$resolved_ref^{commit}")"
current_commit="$(git -C "$REPOSITORY_DIR" rev-parse HEAD 2>/dev/null || true)"
if [[ "$fresh_clone" == true || "$current_commit" != "$resolved_commit" ]]; then
  git -C "$REPOSITORY_DIR" checkout --detach --force "$resolved_commit"
fi

if [[ -n "$deploy_key_file" ]]; then
  rm -f "$deploy_key_file"
  deploy_key_file=""
  unset GIT_SSH_COMMAND
fi

install -d -m 0755 "$REPOSITORY_DIR/idl"
program_id="$(sed -nE 's/^[[:space:]]*"?program_id"?[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' \
  "$config_file" | head -n 1)"
if [[ -z "$program_id" ]]; then
  echo "microscope.toml declares no program_id, so a decoder rebuild cannot be detected" >&2
  exit 1
fi
build_inputs="$resolved_commit $(sha256sum "$idl_file" | cut -d' ' -f1) $program_id"
install -m 0644 "$config_file" "$REPOSITORY_DIR/microscope.toml"
install -m 0644 "$idl_file" "$REPOSITORY_DIR/idl/program.json"

grafana_cloud_enabled="$(jq --raw-output '(.grafana_cloud_token // "") != ""' "$secret_file")"

env_file="$work_dir/.env"
jq --raw-output '
  def dotenv: @json | gsub("\\$"; "$$");
  "GEYSER_URL=" + ((.geyser_url // "") | dotenv),
  "GEYSER_X_TOKEN=" + ((.geyser_x_token // "") | dotenv),
  "RPC_URL=" + ((.rpc_url // "") | dotenv),
  "MICROSCOPE_STREAM_STALE_AFTER_SECONDS=" + ((.stream_stale_after_seconds // "") | tostring | dotenv),
  "GRAFANA_ADMIN_PASSWORD=" + (((.grafana_admin_password // "") | if . == "" then "unused-in-grafana-cloud-mode" else . end) | dotenv),
  "SLACK_WEBHOOK_URL=" + ((.slack_webhook_url // "") | dotenv),
  "TELEGRAM_BOT_TOKEN=" + ((.telegram_bot_token // "") | dotenv),
  "TELEGRAM_CHAT_ID=" + ((.telegram_chat_id // "") | dotenv),
  "PAGERDUTY_INTEGRATION_KEY=" + ((.pagerduty_integration_key // "") | dotenv),
  "GRAFANA_CLOUD_LOKI_URL=" + ((.grafana_cloud_loki_url // "") | dotenv),
  "GRAFANA_CLOUD_LOKI_USER=" + ((.grafana_cloud_loki_user // "") | dotenv),
  "GRAFANA_CLOUD_PROM_URL=" + ((.grafana_cloud_prom_url // "") | dotenv),
  "GRAFANA_CLOUD_PROM_USER=" + ((.grafana_cloud_prom_user // "") | dotenv),
  "GRAFANA_CLOUD_TOKEN=" + ((.grafana_cloud_token // "") | dotenv),
  "MICROSCOPE_DEPLOYMENT=" + ((.microscope_deployment // "") | dotenv),
  "MICROSCOPE_ENV=" + ((.microscope_env // "") | dotenv),
  "PROMETHEUS_RETENTION=" + ((.prometheus_retention // "30d") | dotenv)
' "$secret_file" >"$env_file"
install -m 0600 "$env_file" "$REPOSITORY_DIR/.env"
rm -f "$secret_file" "$env_file"

compose=(docker compose)
if [[ "$grafana_cloud_enabled" == true ]]; then
  compose=(docker compose --file docker-compose.yml --file docker-compose.cloud.yml)
fi

cd "$REPOSITORY_DIR"
"${compose[@]}" config --quiet

if [[ ! -f "$BUILT_INPUTS_FILE" ]] || [[ "$(<"$BUILT_INPUTS_FILE")" != "$build_inputs" ]] ||
  ! docker image inspect solana-microscope-indexer:local >/dev/null 2>&1; then
  log "building indexer at commit $resolved_commit"
  "${compose[@]}" build indexer
  built_inputs_file="$work_dir/built-inputs"
  printf '%s\n' "$build_inputs" >"$built_inputs_file"
  install -m 0644 "$built_inputs_file" "$BUILT_INPUTS_FILE"
fi

if [[ "$grafana_cloud_enabled" == true ]]; then
  "${compose[@]}" --profile local-stack stop alerting-config prometheus loki grafana
  "${compose[@]}" --profile local-stack rm --force alerting-config prometheus loki grafana
  "${compose[@]}" up --detach --no-build --remove-orphans alloy
  "${compose[@]}" up --detach --no-build --no-deps --force-recreate indexer
else
  docker compose run --rm --no-deps alerting-config
  docker compose up --detach --no-build --remove-orphans prometheus loki alloy
  docker compose up --detach --no-build --no-deps --force-recreate indexer
  docker compose up --detach --no-build --no-deps --force-recreate grafana
fi

retry 60 curl --fail --silent --show-error http://127.0.0.1:9090/metrics >/dev/null
if [[ "$grafana_cloud_enabled" != true ]]; then
  retry 60 curl --fail --silent --show-error http://127.0.0.1:3000/api/health >/dev/null
fi

docker image prune --force >/dev/null || log "pruning dangling images failed"
docker builder prune --force --filter until=168h >/dev/null || log "pruning the build cache failed"

revision_file="$work_dir/applied-revision"
printf '%s\n' "$desired_revision" >"$revision_file"
install -m 0644 "$revision_file" "$APPLIED_REVISION_FILE"
touch "$INSTALL_ROOT/ready"
log "applied deployment revision $desired_revision"
