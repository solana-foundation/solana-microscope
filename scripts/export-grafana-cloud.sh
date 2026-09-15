#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: scripts/export-grafana-cloud.sh <deployment> <folder-uid> [config]

Generates Grafana provisioning from <config> (default: microscope.toml) and
rewrites it for the shared Grafana Cloud stack into:

  dashboards/<deployment>.json
  alert-rules/<deployment>/*.json           (one rule per file)
  alert-rules/<deployment>/_rule-groups.json (group evaluation intervals;
                                             not a rule, do not PUT it to
                                             the alert-rules endpoint)
  notification-policies/contact-points/<deployment>-*.json
  notification-policies/mute-timings/*.json
  notification-policies/<deployment>-routes.json

Import dashboards through the Grafana UI; push alert rules, contact
points, and mute timings through the Grafana provisioning API. Merge
<deployment>-routes.json into the shared notification-policy tree by
hand; it is not a standalone resource.

Every exported rule names <deployment>-<channel> as its receiver, so
push the contact points before the rules: a rule whose receiver does
not exist is rejected. Rules for a deployment whose contact points are
named differently need that receiver rewritten before the push.

Run `just generate` for <config>'s program first: the export refuses to
run when the config targets a different program than the compiled
decoder.

RPC_URL must be set: to the deployment's endpoint when it has RPC
recovery or backfill, or to the empty string when it has neither.
EOF
  exit 64
}

[[ $# -ge 2 && $# -le 3 ]] || usage
readonly DEPLOYMENT="$1"
readonly FOLDER_UID="$2"
readonly CONFIG="${3:-microscope.toml}"

[[ "$DEPLOYMENT" =~ ^[a-z0-9-]+$ ]] || {
  echo "deployment must be lowercase alphanumeric/hyphens: $DEPLOYMENT" >&2
  exit 64
}

[[ -n "${RPC_URL+set}" ]] || {
  echo "set RPC_URL=<endpoint> if this deployment has RPC recovery, or RPC_URL= if it does not" >&2
  echo "without it the export silently omits the RPC polling health alerts and dashboard panels" >&2
  exit 64
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

export SLACK_WEBHOOK_URL="${SLACK_WEBHOOK_URL:-\$SLACK_WEBHOOK_URL}"
export TELEGRAM_BOT_TOKEN="${TELEGRAM_BOT_TOKEN:-\$TELEGRAM_BOT_TOKEN}"
export TELEGRAM_CHAT_ID="${TELEGRAM_CHAT_ID:-\$TELEGRAM_CHAT_ID}"
export PAGERDUTY_INTEGRATION_KEY="${PAGERDUTY_INTEGRATION_KEY:-\$PAGERDUTY_INTEGRATION_KEY}"

cargo run --quiet -p microscope-indexer -- generate-alerting \
  "$CONFIG" "$work_dir/alerting" "$work_dir/dashboards"

document="$work_dir/alerting/microscope.json"
dashboard="$work_dir/dashboards/overview.json"

readonly CLOUD_REWRITE='
def swap_datasources:
  walk(
    if type == "object" and .uid? == "prometheus" and .type? == "prometheus"
      then .uid = "grafanacloud-prom"
    elif type == "object" and .uid? == "loki" and .type? == "loki"
      then .uid = "grafanacloud-logs"
    elif type == "object" and .datasourceUid? == "prometheus"
      then .datasourceUid = "grafanacloud-prom"
    elif type == "object" and .datasourceUid? == "loki"
      then .datasourceUid = "grafanacloud-logs"
    else . end
  );

def scope_exprs($deployment):
  walk(
    if type == "object" and (.expr? | type) == "string" then
      .expr |= (
        gsub("(?<metric>microscope_[a-zA-Z0-9_]+)";
             "\(.metric){deployment=\"\($deployment)\"}")
        | gsub("(?<metric>loki_write_[a-zA-Z0-9_]+)";
               "\(.metric){deployment=\"\($deployment)\"}")
        | gsub("up\\{job=\"microscope-indexer\"\\}";
               "up{job=\"microscope-indexer\", deployment=\"\($deployment)\"}")
        | gsub("\\{service_name=\"microscope-indexer\"\\}";
               "{service_name=\"microscope-indexer\", deployment=\"\($deployment)\"}")
      )
    else . end
  );
'

mkdir -p dashboards "alert-rules/$DEPLOYMENT" \
  notification-policies/contact-points notification-policies/mute-timings

list_exports() {
  find "$1" -maxdepth 1 -name "$2" ! -name '_rule-groups.json' \
    -exec basename {} .json \; | sort
}

list_exports "alert-rules/$DEPLOYMENT" '*.json' >"$work_dir/previous-rules"
list_exports notification-policies/contact-points "$DEPLOYMENT-*.json" \
  >"$work_dir/previous-contact-points"

rm -f "alert-rules/$DEPLOYMENT"/*.json notification-policies/contact-points/"$DEPLOYMENT"-*.json

jq --arg deployment "$DEPLOYMENT" "$CLOUD_REWRITE"'
  swap_datasources
  | scope_exprs($deployment)
  | .uid = "microscope-" + $deployment
  | .title = .title + " (" + $deployment + ")"
' "$dashboard" >"dashboards/$DEPLOYMENT.json"

rule_uids="$(
  jq --arg deployment "$DEPLOYMENT" --arg folder "$FOLDER_UID" --compact-output \
    "$CLOUD_REWRITE"'
    .groups[] as $group
    | $group.rules[]
    | swap_datasources
    | scope_exprs($deployment)
    | .labels.deployment = $deployment
    | if .notification_settings then
        .notification_settings.receiver |=
          sub("^microscope-"; $deployment + "-")
      else . end
    | if .annotations.__dashboardUid__ then
        .annotations.__dashboardUid__ = "microscope-" + $deployment
      else . end
    | . + {
        orgID: 1,
        folderUID: $folder,
        ruleGroup: ($deployment + "-" + $group.name),
      }
  ' "$document"
)"
while IFS= read -r rule; do
  source_uid="$(jq --raw-output .uid <<<"$rule")"
  digest="$(printf '%s\0%s' "$DEPLOYMENT" "$source_uid" | shasum -a 256 | cut -c1-32)"
  uid="ms-$digest"
  jq --arg uid "$uid" '.uid = $uid' <<<"$rule" >"alert-rules/$DEPLOYMENT/$uid.json"
done <<<"$rule_uids"

jq --arg deployment "$DEPLOYMENT" '
  [.groups[] | { name: ($deployment + "-" + .name),
                 interval_seconds: (.interval | rtrimstr("s") | tonumber) }]
' "$document" >"alert-rules/$DEPLOYMENT/_rule-groups.json"

jq --arg deployment "$DEPLOYMENT" --compact-output '
  .contactPoints[]
  | .name as $name
  | .receivers[]
  | { uid: ($name | sub("^microscope-"; $deployment + "-")),
      name: ($name | sub("^microscope-"; $deployment + "-")),
      type,
      settings,
      disableResolveMessage,
    }
' "$document" | while IFS= read -r point; do
  name="$(jq --raw-output .name <<<"$point")"
  jq . <<<"$point" >"notification-policies/contact-points/$name.json"
done

jq '.muteTimes[] | { name, time_intervals }' "$document" \
  >notification-policies/mute-timings/microscope-always.json

jq --arg deployment "$DEPLOYMENT" '
  [.policies[0].routes[]
   | .receiver |= sub("^microscope-"; $deployment + "-")
   | .object_matchers += [["deployment", "=", $deployment]]]
' "$document" >"notification-policies/$DEPLOYMENT-routes.json"

report_removals() {
  local previous="$1" endpoint="$2" removed
  removed="$(comm -23 "$previous" -)"
  [[ -n "$removed" ]] || return 0
  echo "these $endpoint are no longer exported and stay active until deleted:" >&2
  while IFS= read -r uid; do
    echo "  curl -X DELETE \"\$GRAFANA_URL/api/v1/provisioning/$endpoint/$uid\"" \
      "-H \"Authorization: Bearer \$GRAFANA_API_TOKEN\"" >&2
  done <<<"$removed"
}

list_exports "alert-rules/$DEPLOYMENT" '*.json' |
  report_removals "$work_dir/previous-rules" alert-rules
list_exports notification-policies/contact-points "$DEPLOYMENT-*.json" |
  report_removals "$work_dir/previous-contact-points" contact-points

echo "exported deployment=$DEPLOYMENT folder=$FOLDER_UID" >&2
