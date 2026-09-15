# Activity alerts

Generated from this deployment's IDL and the `[[alerts]]` entries in `microscope.toml`, so their names differ per operator. Title is `<kind> <name> [<channel>]`, UID is `ms-` plus 32 hex characters, and `signal_kind` is `instruction`, `event`, or `multisig`.

These say the monitored program did something the operator asked to be told about. That is not by itself a fault: half of them are working as intended and the answer is "this is the activity you asked to see, here is what it was". Decide which case it is before proposing a remediation.

## Read the deployment first

An activity alert cannot be interpreted without the config that produced it.

1. `microscope.toml`: the `[[alerts]]` entry whose `kind` and `name` match, for its conditions, `match` mode, severity, and lookback window.
2. The IDL under `idl/`: the field types behind `data.<field>` in those conditions, and what the instruction or event means in the program.
3. `.claude/skills/setup-deployment/references/alerts-from-idl.md`: the naming and operator rules the config had to follow. Useful when the alert looks misconfigured.

## What fired

The `transaction` annotation carries the signature and an explorer link, and `signature` is a query label. Pull the record itself from Loki rather than reasoning from the alert text: the alert only proves a match, the record has the decoded `data`.

`references/investigate.md` has the record shapes and the query transports. For an instruction alert, the record is `kind = "program_instruction"` with the matching `name` and `signature`.

Then judge:

- **Expected activity.** Report what happened, decoded, with the explorer link. Note if the rate is unusual: `increase(microscope_instructions_total{instruction="<name>"}[1h])` against a longer window shows whether this is routine.
- **Unexpected privileged activity.** The escalating class. See below.
- **Failure spike.** `failed = true` on many records of one instruction is a program or client fault, not a Microscope fault. `microscope_errors_total` over `microscope_transactions_total` gives the ratio.
- **Threshold crossed.** A condition on a numeric `data` field fired because a value moved. Report the value and the threshold from the config, and whether the threshold is still the right one.

## Privileged activity

The classes below are the ones worth escalating on a single occurrence, because each is either an irreversible change to who controls the protocol or a bypass of the process that is supposed to gate one.

| The alert that fired | Why it escalates |
|----------------------|------------------|
| `multisig` `threshold_changed`, `member_added`, `member_removed` | Who can move funds just changed. Report the new threshold and signer count from `data`, and whether it still satisfies m-of-n with no single entity holding a majority |
| `multisig` `time_lock_changed` | The delay protecting every future privileged action changed. A reduction to zero is the pre-exploit move |
| `multisig` `configuration_authority_changed`, `settings_authority_changed`, `vault_authority_added` | Control of the multisig itself moved |
| `multisig` `spending_limit_added`, `spending_limit_used` | Funds moved, or can now move, without a proposal. This is the sanctioned bypass path, so a use that no signer expected is indistinguishable from a compromised one until asked |
| `multisig` `transaction_created`, `transaction_executed` | If the upgrade authority is this multisig, a program upgrade goes through here. Microscope does not decode what the inner transaction does, so check the transaction in an explorer before concluding it was routine |
| `instruction` on an authority, admin, or pause instruction | The program's own privileged surface |
| `instruction` on a parameter-setting instruction | Compare the new value in `data` against the bounds the deployment documented |
| `event` on a circuit breaker or invariant violation | The program stopped itself. Treat as an active incident |

For any of these, report three things beyond the decode: whether the transaction succeeded (`failed`), the signature with its explorer link, and whether the change went through the multisig or bypassed it. Then hand off: an escalating change is an incident-response question, not a Microscope one, and the deployment's own playbook and pause authority own it from there.

Two limits to state rather than work around. Microscope cannot see a program upgrade directly, only the multisig transaction carrying it. And it cannot alert on activity that stopped, so "the alert did not fire" is never evidence that a privileged action did not happen through a path Microscope does not watch. Account balances and instructions of other programs (upgrades, SPL transfers, durable nonces) are also out of view.

## The alert should have fired and did not

This is the common report, and almost always one of a fixed set of causes. Rule out `log_delivery_stalled` and a dead indexer first (see `references/health-alerts.md`), then work through the query itself.

The generated LogQL is, in shape:

```
sum by (signature) (count_over_time({service_name="microscope-indexer"}
  | json kind="kind", signal="<name|action>", scope="<program_id|vault_address>", signature="signature"
  | kind = "<record kind>" | signal = "<alert name>" | scope = "<configured address>"
  | (<condition filters>) | __error__ = "" [<lookback>s]))
```

The causes, in the order they are worth checking:

- **`__error__ = ""` dropped the record.** A numeric condition against a field that is a string, null, or absent makes Loki mark the line as a conversion error, and the filter removes it. This silently discards a record that otherwise matched. Query without the condition filters to see whether the record is there at all.
- **The condition does not match the field.** `exists` means present and non-empty: `null` and `""` do not match. Integer values above 2^53 are compared as float64 and must be quoted as strings to compare exactly.
- **`match = "all"`** needs one single record satisfying every condition, not one record per condition. `match = "any"` with more than one condition compiles to independent OR branches precisely so a missing field in one branch cannot hide a match in another.
- **Multisig alerts filter `failed = "false"`.** A failed Squads instruction never alerts, and a `failed` condition on a multisig alert is rejected at config load.
- **The scope filter.** Instruction and event alerts are scoped to `program_id`, multisig alerts to `vault_address`. A record from the right program under a different address is filtered out. For multisig this is what `multisig_unmatched_state` catches.
- **The lookback window** must cover the activity. It defaults from `[alerting]` and is overridable per alert.
- **The record was never produced.** Confirm with `microscope_instructions_total{instruction="<name>"}` or `microscope_program_events_total{event="<name>"}`. If the counter is flat, the problem is upstream of alerting: the datasource, the decoder, or genuinely no activity.

## Events specifically

Programs whose IDL declares no events get no event panel and no event alerts at all; that is expected, not a defect.

`microscope_event_decode_failures_total` and the `kind = "event_decode_failure"` records mean the program emitted something the pinned decoder could not represent. The `source` label separates log-emitted events from CPI-emitted ones. This is a decoder or IDL mismatch, not lost transactions: the instruction record still exists.
