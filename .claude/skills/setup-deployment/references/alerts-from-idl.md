# Deriving alerts from the IDL

Signal names and condition field paths must match what the indexer emits. Every alert `name` is validated at config load against the IDL or the multisig action list, so a wrong name fails startup rather than silently never firing.

## Signal names

Read the configured IDL and present the available signals:

- **event** alert `name` = IDL event name in snake_case (`RecurringTransferEvent` -> `recurring_transfer_event`)
- **instruction** alert `name` = IDL instruction name in snake_case
- **multisig** alert `name` = a normalized action string. Common ones: `proposal_created`, `proposal_approved`, `proposal_rejected`, `proposal_cancelled`, `transaction_created`, `transaction_executed`, `member_added`, `member_removed`, `threshold_changed`. The canonical per-version lists are the `ACTIONS` consts in `crates/microscope-indexer/src/multisig/{v3,v4,smart_account}.rs` (resolved by `multisig::action_names`); check there before inventing a name.

## Per-alert questions

For each signal the user wants: severity (`critical`/`error`/`warning`/`info`), channels (`slack`/`telegram`/`pagerduty`, or none), and optional conditions.

## Conditions

Build them from the IDL's field types.

- Field paths for events/instructions: `name`, `program_id`, `failed`, and `data.<idl field>` (nested fields dot-separated). For multisig: `action`, `squads_version`, and `data.*`. A `failed` condition on a multisig alert is rejected, multisig alerts already match successful activity only.
- Operators: `exists` (no value), `contains` (non-empty string value), `eq`/`ne` (string, number, bool), `gt`/`gte`/`lt`/`lte` (number). Pick the operator matching the IDL field type.
- `exists` means present and non-empty: `null` and `""` do not match. Use it only for Option-typed fields where a non-empty value is the signal.
- Integer condition values above 2^53 are rejected (Loki compares numbers as float64); quote large thresholds as strings to compare exactly.
- `match` is `all` or `any`.
- Timing overrides: `evaluation_interval_seconds` must be a multiple of 10, and `lookback_window_seconds` at least the interval.

## Dashboard fields

Offer `dashboard.event_fields` (and `multisig_fields`) overrides built from the most informative IDL fields; skip if the defaults suffice.
