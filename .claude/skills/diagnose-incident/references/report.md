# Incident report

Close every diagnosis with this block, filled in, in this order. Omit nothing; write "unknown" and why.

```
Alert:        <signal_name or "<kind> <name>">, severity, first notification time
Masked by:    <upstream health alert, or "none">
Cause:        <one sentence, confirmed against: metrics | logs | readyz | not confirmed>
Onset:        <from metrics, not the notification time>
Data loss:    none | recoverable by poller | needs backfill <from> to <to> | permanent <slots or interval>
Remediation:  <docs/operations.md section>, or "no runbook entry: <proposed change>"
Hand-off:     <"none" | the escalation the deployment's playbook owns, from references/scenarios.md>
```

Timeline, when the operator asks for one: the Loki records for the signatures involved, ordered by `block_time`, plus the metric that first moved. Keep it to the entries that changed the diagnosis.
