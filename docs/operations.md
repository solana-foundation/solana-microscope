# Operations

## Datasource endpoints

**Yellowstone gRPC (`GEYSER_URL`)**, the default: a managed provider that sells
Yellowstone or Geyser gRPC access, usually as an add-on rather than part of an
RPC plan, or your own node running the
[`yellowstone-grpc`](https://github.com/rpcpool/yellowstone-grpc) plugin.
Microscope speaks the upstream protocol only, so any of them work.
`GEYSER_X_TOKEN` is the `x-token` header; leave it empty for providers that
authenticate by URL.

**Solana JSON-RPC (`RPC_URL`)**: required by `just backfill`, adds gap recovery
alongside Yellowstone, and is the datasource in `mode = "rpc"`. It must serve
`getSignaturesForAddress` and `getTransaction` back to the checkpoint, so its
history depth caps the outage recovery can survive. Public endpoints are
rate-limited below what continuous polling needs.

Endpoint cost usually exceeds the machine cost below.

## Finding the Squads state address

`vault_address` is what the Squads UI shows. `state_address` is the account
Squads instructions reference: the v3 `Ms`, v4 `Multisig`, or v5 `Settings`
account, and the source of every record.

The vault is a PDA of the state address, so it cannot be derived backwards.
Open any past Squads transaction of that multisig in an explorer and take the
first account of the Squads instruction. v3 and v4 name it `multisig`; v5 names
it `settings`, or `consensusAccount` on proposal and transaction instructions,
where it is the `Settings` account unless the instruction targets a policy
account, which Microscope does not monitor.

Startup verifies the pair locally, so a candidate costs one restart to test. The
same pair verifies under exactly one of `v3`, `v4`, `v5`, so a mismatch may be a
wrong `version` rather than a wrong address.

## Cost

The full local stack needs 2 dedicated vCPUs, 4 GiB RAM, and 40 GiB disk; the
first-boot Rust build sets that floor and steady state is far below it. On AWS
or GCP that VM, its disk, and a public IPv4 address run around \$50 per month,
\$0 locally; buckets and secrets cost cents. Grafana Cloud mode runs fewer
containers but needs the same build capacity, plus hosted usage beyond the free
tier. Confirm in the provider calculators before budgeting.

## Troubleshooting

### No data in the dashboard

`docker compose ps`, `docker compose logs indexer`, the datasource variables for
the mode in use, recent confirmed program activity, the Grafana time range.
Squads panels stay empty until the multisig produces a supported action.

### Indexer refuses to start

The decoder is compiled in, so the `program_id` and IDL in `microscope.toml`
must be the ones the image was built from. Change both together and
`docker compose up --build --detach`. A `[multisig]` rejection is the vault and
state pair or the `version`, see above.

### A dashboard column is always empty

That JSON path is absent from the record. Read the raw record in Loki and fix
`dashboard.event_fields` or `dashboard.multisig_fields`.

### An alert never fires

Confirm the record is in Loki, then that field path, operator, and value type
match it. `match = "all"` needs one record satisfying every condition, `exists`
does not match `null` or `""`, and multisig alerts ignore failed instructions.
Then check that the rule is provisioned and unpaused, that the lookback window
covers the activity, and that its channel has credentials.

### Clearing a quarantine alert

Two alerts use the word, and neither clears on restart.

`microscope_rpc_poll_quarantined_transactions`: a transaction failed 5
consecutive fetch or conversion attempts, so the cursor advanced past it with no
record. Its signature is in the indexer log at error level, usually a
transaction the pinned decoder cannot represent, in which case only the alert
needs clearing. The list lives in the checkpoint and is restored on startup.
Wait until the confirmed head is more than `replay_window_slots` past that slot,
or the next poll re-quarantines it, then:

```sh
docker compose stop indexer
docker compose cp indexer:/var/lib/solana-microscope/rpc-polling.json .
# Set "quarantined_signatures": [] and change nothing else.
docker compose cp rpc-polling.json indexer:/var/lib/solana-microscope/rpc-polling.json
rm rpc-polling.json
docker compose up -d indexer
```

Keep the JSON valid and every cursor present. A malformed or partial checkpoint
is treated as corrupt and moved aside, which trades the stale alert for a real
gap; deleting the file does the same.

`microscope_rpc_checkpoint_quarantined_files`: counts
`rpc-polling.json.corrupt-<timestamp>` files. Recovery already restarted from a
fresh replay window, so backfill the period the lost checkpoint covered, then
delete the file.

### RPC recovery disabled itself

`microscope_rpc_recovery_degraded` carries a `reason` label: exhausted history,
a cursor beyond the confirmed head, or a checkpoint failure. It stays off until
restart. For exhausted history, point `RPC_URL` at an endpoint holding the
missing slots, or backfill the gap and remove the checkpoint. A checkpoint whose
cluster, program, or addresses do not match the deployment disables recovery on
purpose; fix the config or endpoint, and remove it only if the target genuinely
changed:

```sh
docker compose stop indexer
docker compose run --rm --entrypoint sh indexer \
  -c 'rm -f /var/lib/solana-microscope/rpc-polling.json'
docker compose up -d indexer
```

### Cloud configuration does not update

A failed revision is not marked applied, so the timer retries it.

```sh
sudo systemctl status microscope-deploy.timer
sudo systemctl status microscope-deploy.service
sudo tail -f /var/log/microscope-deploy.log
```

### Grafana still evaluates the old alerts

Grafana reads alert provisioning only at startup. `just up` recreates it, plain
`docker compose up --detach` does not:

```sh
docker compose up --detach --no-deps --force-recreate grafana
```
