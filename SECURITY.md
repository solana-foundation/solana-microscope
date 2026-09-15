# Security Policy

## Reporting security problems

**Do not create a public GitHub issue to report a security problem.**

Use GitHub's private
[Report a Vulnerability](https://github.com/solana-foundation/solana-microscope/security/advisories/new)
workflow instead. Include a helpful title, affected version or commit,
reproduction steps, impact, and any proposed mitigation.

Expect an initial response in the advisory as soon as possible, typically
within 72 hours.

## Sensitive deployment data

Do not attach real Yellowstone tokens, Grafana passwords, webhook URLs,
Telegram bot tokens, PagerDuty integration keys, repository deploy keys,
Terraform state, `.env` files, or `terraform.tfvars` to an issue or pull
request.

If a credential is exposed, revoke or rotate it before sharing sanitized
diagnostic information.

## Supported versions

Security fixes are applied to the `main` branch and carried into the next
`vX.Y.Z` tag. Only the latest release is supported; older tags and downstream
forks may need to apply fixes independently.
