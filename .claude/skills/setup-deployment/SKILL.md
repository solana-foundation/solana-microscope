---
name: setup-deployment
description: Use when the user asks to "set up microscope", "configure a deployment", "set up monitoring for my program", "create microscope.toml", "fill in terraform.tfvars", "configure alerts", "deploy to AWS", "deploy to GCP", or "run the stack locally". Interviews the operator, writes the deployment config (microscope.toml + .env for local, infra/aws or infra/gcp terraform.tfvars for cloud), derives alert rules from the program IDL, and verifies the result with the repo's own tooling.
user-invocable: true
---

# Deployment setup

Interview the operator, write the config files for their deployment target, then verify with the repo's own tooling. One deployment monitors one Solana program and, optionally, one Squads multisig.

| Target | Files written | Seed from | Verified by |
|--------|---------------|-----------|-------------|
| local | `microscope.toml`, `.env` | `microscope.toml.example`, `.env.example` | `just generate-alerting` |
| aws | `infra/aws/terraform.tfvars` | `infra/aws/terraform.tfvars.example` | `terraform fmt` + `validate`, then `plan` |
| gcp | `infra/gcp/terraform.tfvars` | `infra/gcp/terraform.tfvars.example` | `terraform fmt` + `validate`, then `plan` |

## Rules

- All three write targets are gitignored. Never place deployment values anywhere else, and never stage or commit them.
- Never echo a secret back into the conversation; confirm which file received it instead.
- A private-repo deploy key never goes in `terraform.tfvars`; instruct `export TF_VAR_repository_deploy_key="$(cat /path/to/key)"`.
- Never run `terraform apply` yourself: it creates billable infrastructure. Show the user the command instead.

## Procedure

1. **Pick the target** — one question: local, AWS, or GCP. Everything downstream branches on this.
2. **Collect the inputs** — validation rules, cloud-only inputs, and the defaults to assume without asking: `references/inputs.md`.
3. **Derive the alerts from the IDL** — signal naming and condition operators: `references/alerts-from-idl.md`. Start from the privileged-activity classes in `.claude/skills/diagnose-incident/references/activity-alerts.md` (authority, parameter, pause, fund-movement instructions; multisig config changes) before asking the operator to invent a list, and say plainly what a deployment cannot see: program upgrades, absent activity, account balances.
4. **Write the files** — copy the example file for the target and edit values in place, preserving its structure and comments. Every channel an alert selects needs a non-empty credential (`slack_webhook_url`, `telegram_bot_token` + `telegram_chat_id`, or `pagerduty_integration_key`); Terraform rejects the mismatch and `just generate-alerting` fails on it. Offer "leave channels empty now, add credentials later" when the user doesn't have them at hand.
5. **Verify** — per-target checks and their gotchas: `references/verify.md`. Finish by showing the next command (`just up`, or `terraform init && terraform apply` in the target directory).
