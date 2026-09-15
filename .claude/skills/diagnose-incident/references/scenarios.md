# Incident scenarios

The compromise scenarios an incident playbook has to cover, and what Microscope contributes to each. Every row ends where Microscope's view ends; from there the deployment's own playbook and pause authority own the incident. Alert names are the `multisig` actions from `crates/microscope-indexer/src/multisig/*.rs`; instruction names are the deployment's own.

| Scenario | First Microscope signal | Confirm | Microscope cannot tell you |
|----------|------------------------|---------|----------------------------|
| Upgrade authority compromise | `transaction_created` then `transaction_executed` on the multisig that holds the authority. Nothing at all if the authority is a single key or another multisig | Open the transaction in the explorer: a BPF Loader `Upgrade` or `SetAuthority` inside. Check `proposal_approved` count against the threshold, and whether `time_lock_changed` preceded it | What the new program does, whether the deployed hash matches a verified build |
| Multisig takeover, signer key compromise | `member_added`, `member_removed`, `threshold_changed`, `time_lock_changed`, `configuration_authority_changed` or `settings_authority_changed`, in any order within hours | `data` on each record for the new signer, threshold, delay. A threshold lowered before a member is added is the takeover order. `spending_limit_added` followed by `spending_limit_used` is the drain without a proposal | Which signer's key leaked. Whether the change was planned; ask the signers out of band |
| Exploit in progress | Failure spike or burst on one instruction: `microscope_instructions_total{instruction}` rate versus the previous day, `failed = true` ratio from `microscope_errors_total`. Any `event` alert on the program's own breaker or invariant | Pull the records for the signature cluster. Same signer, same instruction, escalating `data` amounts is the pattern. Report whether a pause instruction has fired since | Balances and TVL. Whether the program is still exploitable after a pause |
| Oracle failure | Only if the program emits a rejection event and an `event` alert exists on it. Otherwise nothing: a stale feed is an absence, and activity rules fire on presence | Rate of the consuming instruction dropping to zero while the rest of the program stays busy is the indirect sign; compare per-instruction counters | Feed freshness, price deviation |
| Frontend or key compromise off-chain | Nothing directly. Downstream on-chain effects surface as the exploit or takeover rows above | Same as those rows | Anything about the frontend, DNS, or signer devices |

Two rules across all rows:

- **Silence is not exoneration.** A health alert (`references/health-alerts.md`, precedence list) or a path Microscope does not watch both look like "nothing happened". State which one you ruled out.
- **Never conclude "routine" from the alert alone.** A `transaction_executed` is the same record for a treasury payout and a malicious upgrade. The explorer link in the `transaction` annotation is the minimum check.
