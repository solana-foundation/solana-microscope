---
name: diagnose-incident
description: Use when a Microscope alert fires, when the operator asks "what does this alert mean", "why is the dashboard empty", "why did we stop indexing", "did we lose data", "what do I do about this alert", or pastes an alert notification, indexer log line, or Grafana screenshot. Identifies the failing component from live metrics and logs, says whether data was lost, and gives the remediation from the repo's runbook.
user-invocable: true
---

# Incident diagnosis

Read the alert, confirm the cause against live data before explaining it, then give the remediation. Never diagnose from the alert title alone: several alerts share a cause, and one of them makes every other signal go quiet.

Alerts split into two families, distinguished by the `signal_kind` label:

| `signal_kind` | Source | Meaning | Reference |
|---------------|--------|---------|-----------|
| `datasource` | Generated for every deployment | Microscope itself is degraded: the pipeline is broken or lost data | `references/health-alerts.md` |
| `instruction`, `event`, `multisig` | Generated from this deployment's IDL and `microscope.toml` | The monitored program did something the operator asked to be told about | `references/activity-alerts.md` |

## Procedure

1. **Classify the alert.** Take `signal_name` from the notification labels, or map the title through the tables in the two references. An unrecognized title is probably an activity alert, whose title is `<kind> <name> [<channel>]`.
2. **Check whether the alert is masked by a more upstream one.** The health alerts have a fixed precedence, listed in `references/health-alerts.md`. Diagnosing a downstream alert as its own fault is the most common wrong answer.
3. **Confirm against live data.** Query the metrics, logs, and readiness endpoint before stating a cause; transports and the metric catalog are in `references/investigate.md`. If the stack is unreachable, say the diagnosis is unconfirmed rather than presenting it as established.
4. **Answer the data-loss question explicitly.** Every incident report must state whether on-chain activity was permanently missed, recoverable by the poller, or recoverable only by `just backfill`. `references/health-alerts.md` classifies each alert.
5. **Give the remediation.** The runbook in `docs/operations.md` is authoritative for the fix; cite the section rather than paraphrasing its commands. Propose config or code changes only when the runbook has no entry.
6. **Write the report.** End with the fixed block in `references/report.md`, so every incident record answers the same questions in the same order.

For a privileged-activity alert, or an operator asking "are we being attacked", `references/scenarios.md` maps the recurring compromise scenarios to their first Microscope signal and to the point where Microscope stops being able to help.

## Rules

- Silence is not health. `log_delivery_stalled`, `yellowstone_endpoint_unreachable`, and a stopped indexer all leave every activity alert quiet and every dashboard panel empty. Rule out the pipeline before reporting "no activity".
- Never run a remediation that mutates deployment state (editing a checkpoint, deleting `rpc-polling.json`, restarting containers, `just backfill`) without the operator confirming. Show the command from the runbook instead.
- Read-only investigation (metrics, logs, readiness, Loki and Prometheus queries) needs no confirmation.
- Never read a value out of `.env`. Endpoint URLs, tokens, webhooks, and the Grafana password all live there in plaintext, and a value read into the transcript has leaked. When a diagnosis turns on whether a variable is set, read the names only: `grep -oE '^[A-Z_]+=.' .env | cut -d= -f1`.
- Never echo a datasource URL that carries a query string: it may hold an API key. The indexer redacts them in its own logs, `.env` does not.
- A backfill window past Loki's out-of-order limit is silently dropped, so a proposed backfill has to name the window and whether it fits.
