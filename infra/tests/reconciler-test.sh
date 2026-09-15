#!/usr/bin/env bash
set -Eeuo pipefail

# bash 3.2, still the system bash on macOS, does not apply errexit to a failing
# `[[ ]]`, so every assertion below would pass silently. The suite also needs
# GNU coreutils (mktemp --directory, sha256sum, stat --format, base64 --wrap);
# on macOS run it in Docker or with coreutils on PATH.
if ((BASH_VERSINFO[0] < 4)); then
    echo "this suite needs bash 4 or newer; ${BASH_VERSION} would report a pass without checking anything" >&2
    exit 1
fi

readonly REPOSITORY_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly TEST_ROOT="$(mktemp --directory)"
trap 'rm -rf "$TEST_ROOT"' EXIT

[[ "$(grep -Fc -- './microscope.toml:/etc/microscope/microscope.toml:ro' \
  "$REPOSITORY_ROOT/docker-compose.yml")" == 2 ]]

readonly BIN_DIR="$TEST_ROOT/bin"
readonly BUCKET_DIR="$TEST_ROOT/bucket"
readonly SOURCE_REPOSITORY="$TEST_ROOT/source"
readonly OTHER_REPOSITORY="$TEST_ROOT/other-source"
readonly INSTALL_ROOT="$TEST_ROOT/install"
readonly STATE_DIR="$TEST_ROOT/state"
readonly BOOTSTRAP_ENV="$TEST_ROOT/bootstrap.env"
readonly DOCKER_LOG="$TEST_ROOT/docker.log"
readonly IMAGE_MARKER="$TEST_ROOT/image-built"
readonly BUILD_FAILURE_MARKER="$TEST_ROOT/build-fails"
readonly UP_FAILURE_MARKER="$TEST_ROOT/up-fails"
readonly SECRET_FILE="$TEST_ROOT/secret.json"

mkdir -p "$BIN_DIR" "$BUCKET_DIR/config" "$BUCKET_DIR/idl" "$SOURCE_REPOSITORY"

git -C "$SOURCE_REPOSITORY" init --quiet --initial-branch=main
git -C "$SOURCE_REPOSITORY" config user.email test@example.com
git -C "$SOURCE_REPOSITORY" config user.name Test
git -C "$SOURCE_REPOSITORY" config commit.gpgsign false
git -C "$SOURCE_REPOSITORY" config tag.gpgsign false
printf 'services: {}\n' >"$SOURCE_REPOSITORY/docker-compose.yml"
git -C "$SOURCE_REPOSITORY" add docker-compose.yml
git -C "$SOURCE_REPOSITORY" commit --quiet --message initial
git -C "$SOURCE_REPOSITORY" tag release

mkdir -p "$OTHER_REPOSITORY"
git -C "$OTHER_REPOSITORY" init --quiet --initial-branch=main
git -C "$OTHER_REPOSITORY" config user.email test@example.com
git -C "$OTHER_REPOSITORY" config user.name Test
git -C "$OTHER_REPOSITORY" config commit.gpgsign false
git -C "$OTHER_REPOSITORY" config tag.gpgsign false
printf 'services: {}\n' >"$OTHER_REPOSITORY/docker-compose.yml"
printf 'other\n' >"$OTHER_REPOSITORY/marker"
git -C "$OTHER_REPOSITORY" add docker-compose.yml marker
git -C "$OTHER_REPOSITORY" commit --quiet --message initial

cat >"$SECRET_FILE" <<'EOF'
{
  "geyser_url": "https://example.invalid",
  "geyser_x_token": "test-token",
  "grafana_admin_password": "test-password",
  "slack_webhook_url": "",
  "telegram_bot_token": "",
  "telegram_chat_id": "",
  "pagerduty_integration_key": "",
  "repository_deploy_key": ""
}
EOF

write_config() {
  printf 'program_id = "%s"\nmarker = "%s"\n' "$1" "$2" >"$BUCKET_DIR/config/microscope.toml"
}

write_config program-one first-config
printf '{}\n' >"$BUCKET_DIR/idl/program.json"

write_manifest() {
  local revision="$1"
  jq --null-input \
    --arg revision "$revision" \
    --arg repository_url "${2:-$SOURCE_REPOSITORY}" \
    --arg repository_ref "${3:-main}" \
    --arg config_sha256 "$(sha256sum "$BUCKET_DIR/config/microscope.toml" | cut -d' ' -f1)" \
    --arg idl_sha256 "$(sha256sum "$BUCKET_DIR/idl/program.json" | cut -d' ' -f1)" \
    '{
      schema_version: 1,
      revision: $revision,
      repository_url: $repository_url,
      repository_ref: $repository_ref,
      config_object: "config/microscope.toml",
      config_sha256: $config_sha256,
      idl_object: "idl/program.json",
      idl_sha256: $idl_sha256,
      secret_version: "test-version"
    }' >"$BUCKET_DIR/deployment.json"
}

write_manifest revision-one

cat >"$BIN_DIR/aws" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ "$1 $2" == "s3 cp" ]]; then
  object_path="${3#s3://}"
  object_key="${object_path#*/}"
  cp "$FAKE_BUCKET_DIR/$object_key" "$4"
elif [[ "$1 $2" == "secretsmanager get-secret-value" ]]; then
  cat "$FAKE_SECRET_FILE"
else
  exit 1
fi
EOF

cat >"$BIN_DIR/docker" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\n' "$*" >>"$FAKE_DOCKER_LOG"
if [[ "$1 $2" == "image inspect" ]]; then
  [[ -f "$FAKE_IMAGE_MARKER" ]]
elif [[ "$1 $2 $3" == "compose build indexer" ]]; then
  if [[ -f "$FAKE_BUILD_FAILURE_MARKER" ]]; then
    exit 1
  fi
  touch "$FAKE_IMAGE_MARKER"
elif [[ "$1 $2" == "compose up" ]] && [[ -f "$FAKE_UP_FAILURE_MARKER" ]]; then
  exit 1
fi
EOF

cat >"$BIN_DIR/curl" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

chmod +x "$BIN_DIR/aws" "$BIN_DIR/docker" "$BIN_DIR/curl"

encode() {
  printf '%s' "$1" | base64 --wrap=0
}

cat >"$BOOTSTRAP_ENV" <<EOF
CLOUD_PROVIDER_B64=$(encode aws)
DEPLOYMENT_BUCKET_B64=$(encode test-bucket)
DEPLOYMENT_MANIFEST_B64=$(encode deployment.json)
RUNTIME_SECRET_B64=$(encode test-secret)
AWS_REGION_B64=$(encode us-east-1)
EOF

export FAKE_BUCKET_DIR="$BUCKET_DIR"
export FAKE_SECRET_FILE="$SECRET_FILE"
export FAKE_DOCKER_LOG="$DOCKER_LOG"
export FAKE_IMAGE_MARKER="$IMAGE_MARKER"
export FAKE_BUILD_FAILURE_MARKER="$BUILD_FAILURE_MARKER"
export FAKE_UP_FAILURE_MARKER="$UP_FAILURE_MARKER"

reconcile() {
  PATH="$BIN_DIR:$PATH" \
    MICROSCOPE_BOOTSTRAP_ENV="$BOOTSTRAP_ENV" \
    MICROSCOPE_INSTALL_ROOT="$INSTALL_ROOT" \
    MICROSCOPE_STATE_DIR="$STATE_DIR" \
    MICROSCOPE_LOG_FILE="$TEST_ROOT/deploy.log" \
    bash "$(dirname "$0")/../deploy-microscope.sh"
}

reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-one ]]
[[ -f "$INSTALL_ROOT/repository/docker-compose.yml" ]]
grep --quiet '^marker = "first-config"$' "$INSTALL_ROOT/repository/microscope.toml"
[[ "$(stat --format=%a "$INSTALL_ROOT/repository/.env")" == 600 ]]
[[ "$(grep --count '^compose build indexer$' "$DOCKER_LOG")" == 1 ]]
[[ "$(grep --count '^image prune --force$' "$DOCKER_LOG")" == 1 ]]
[[ "$(grep --count '^builder prune --force --filter until=168h$' "$DOCKER_LOG")" == 1 ]]

grep --quiet '^PROMETHEUS_RETENTION="30d"$' "$INSTALL_ROOT/repository/.env"
grep --quiet '^MICROSCOPE_STREAM_STALE_AFTER_SECONDS=""$' "$INSTALL_ROOT/repository/.env"

jq '.prometheus_retention = "90d" | .stream_stale_after_seconds = 120' "$SECRET_FILE" >"$SECRET_FILE.retention"
mv "$SECRET_FILE.retention" "$SECRET_FILE"
write_config program-one second-config
write_manifest revision-two
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-two ]]
grep --quiet '^PROMETHEUS_RETENTION="90d"$' "$INSTALL_ROOT/repository/.env"
grep --quiet '^MICROSCOPE_STREAM_STALE_AFTER_SECONDS="120"$' "$INSTALL_ROOT/repository/.env"
grep --quiet '^marker = "second-config"$' "$INSTALL_ROOT/repository/microscope.toml"
[[ "$(grep --count '^compose build indexer$' "$DOCKER_LOG")" == 1 ]]
[[ "$(grep --count '^compose run --rm --no-deps alerting-config$' "$DOCKER_LOG")" == 2 ]]

printf '{"version":2}\n' >"$BUCKET_DIR/idl/program.json"
write_manifest revision-three
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-three ]]
[[ "$(<"$INSTALL_ROOT/repository/idl/program.json")" == '{"version":2}' ]]
[[ "$(grep --count '^compose build indexer$' "$DOCKER_LOG")" == 2 ]]
[[ "$(grep --count '^compose run --rm --no-deps alerting-config$' "$DOCKER_LOG")" == 3 ]]

touch "$BUILD_FAILURE_MARKER"
printf '{"version":3}\n' >"$BUCKET_DIR/idl/program.json"
write_manifest revision-four
if reconcile; then
  printf 'a failed indexer build must abort the reconcile\n' >&2
  exit 1
fi

[[ ! -e "$STATE_DIR/applied-revision" ]]
[[ "$(grep --count '^compose build indexer$' "$DOCKER_LOG")" == 3 ]]

rm -f "$BUILD_FAILURE_MARKER"
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-four ]]
[[ "$(grep --count '^compose build indexer$' "$DOCKER_LOG")" == 4 ]]

touch "$UP_FAILURE_MARKER"
write_config program-one rolled-back-config
write_manifest revision-rolled-back
if reconcile; then
  printf 'a failed compose up must abort the reconcile\n' >&2
  exit 1
fi

[[ ! -e "$STATE_DIR/applied-revision" ]]

rm -f "$UP_FAILURE_MARKER"
write_config program-one second-config
write_manifest revision-four
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-four ]]
grep --quiet '^marker = "second-config"$' "$INSTALL_ROOT/repository/microscope.toml"

write_config program-two second-config
write_manifest revision-five
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-five ]]
[[ "$(grep --count '^compose build indexer$' "$DOCKER_LOG")" == 5 ]]

jq '.grafana_admin_password = ""
  | .grafana_cloud_loki_url = "https://loki.example.invalid/push"
  | .grafana_cloud_loki_user = "111"
  | .grafana_cloud_prom_url = "https://prom.example.invalid/push"
  | .grafana_cloud_prom_user = "222"
  | .grafana_cloud_token = "cloud-token"
  | .microscope_deployment = "test-deployment"
  | .microscope_env = "prd"' "$SECRET_FILE" >"$SECRET_FILE.cloud"
mv "$SECRET_FILE.cloud" "$SECRET_FILE"
write_manifest revision-six
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-six ]]
grep --quiet '^GRAFANA_CLOUD_TOKEN="cloud-token"$' "$INSTALL_ROOT/repository/.env"
grep --quiet '^MICROSCOPE_DEPLOYMENT="test-deployment"$' "$INSTALL_ROOT/repository/.env"
! grep --quiet '^GRAFANA_ADMIN_PASSWORD=""$' "$INSTALL_ROOT/repository/.env"
[[ "$(grep --count '^compose --file docker-compose.yml --file docker-compose.cloud.yml up --detach --no-build --remove-orphans alloy$' "$DOCKER_LOG")" == 1 ]]
[[ "$(grep --count -- '--profile local-stack stop alerting-config prometheus loki grafana$' "$DOCKER_LOG")" == 1 ]]
[[ "$(grep --count -- '--profile local-stack rm --force alerting-config prometheus loki grafana$' "$DOCKER_LOG")" == 1 ]]
[[ "$(grep --count '^compose run --rm --no-deps alerting-config$' "$DOCKER_LOG")" == 7 ]]
[[ "$(grep --count 'force-recreate grafana$' "$DOCKER_LOG")" == 6 ]]
[[ "$(grep --count 'force-recreate indexer$' "$DOCKER_LOG")" == 7 ]]

write_manifest revision-seven "$OTHER_REPOSITORY"
reconcile

[[ "$(<"$STATE_DIR/applied-revision")" == revision-seven ]]
[[ "$(<"$INSTALL_ROOT/repository/marker")" == other ]]

write_manifest revision-eight "$OTHER_REPOSITORY" release
if reconcile; then
  printf 'a ref that only exists in the previous repository must not resolve\n' >&2
  exit 1
fi

[[ ! -e "$STATE_DIR/applied-revision" ]]
grep --quiet 'repository_ref does not resolve to a commit: release' "$TEST_ROOT/deploy.log"

write_config program-one raced-config
write_manifest revision-nine "$OTHER_REPOSITORY"
write_config program-one overwritten-after-the-manifest
if reconcile; then
  printf 'objects that do not match the manifest digests must abort the reconcile\n' >&2
  exit 1
fi

[[ ! -e "$STATE_DIR/applied-revision" ]]
grep --quiet '^marker = "second-config"$' "$INSTALL_ROOT/repository/microscope.toml"

printf 'reconciler test passed\n'
