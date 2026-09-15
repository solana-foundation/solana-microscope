# Running the indexer on Kubernetes

This is a reference, not a supported deployment target. The repository ships
Docker Compose and Terraform; the manifests below are a starting point for
operators who already run Kubernetes and want the indexer next to their
existing Prometheus, Loki, and Grafana rather than on a dedicated VM.

Kubernetes replaces only the indexer's runtime. Everything the Compose stack
provides around it, log shipping with the right labels, metrics scraping, and
Grafana alert provisioning, becomes yours to wire up. The three sections after
the manifests cover what that means in practice.

## Prerequisites

- A published image. The VM deployment builds on the VM; Kubernetes pulls, so
  run the `Publish Docker Image` workflow from your fork first. See
  [Prebuilt container images](../README.md#prebuilt-container-images). One image
  serves one program and one IDL.
- Prometheus, Loki, and Grafana you already operate, or a Grafana Cloud stack.
  These manifests deploy none of them.

## Configuration

The image bakes in the IDL at `/etc/microscope/idl` but carries no
`microscope.toml`. Mount the config with `subPath`, or the volume shadows the
baked IDL directory and the indexer refuses to start.

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: microscope-config
data:
  microscope.toml: |
    program_id = "<PROGRAM_ID>"
    idl_path = "idl/<YOUR_IDL>.json"

    [alerting]
    lookback_window_seconds = 60
    evaluation_interval_seconds = 10

    [[alerts]]
    kind = "event"
    name = "<EVENT_NAME>"
    severity = "warning"
    channels = ["slack"]
---
apiVersion: v1
kind: Secret
metadata:
  name: microscope-endpoints
stringData:
  GEYSER_URL: https://your-yellowstone-endpoint:443
  GEYSER_X_TOKEN: ""
  RPC_URL: https://your-solana-rpc-endpoint
```

`idl_path` resolves relative to the config file, so `idl/<YOUR_IDL>.json` finds
the copy baked into the image. It must be the same IDL the image was built from;
the indexer compares digests at startup.

## StatefulSet

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: microscope-indexer
spec:
  serviceName: microscope-indexer
  replicas: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: microscope-indexer
  template:
    metadata:
      labels:
        app.kubernetes.io/name: microscope-indexer
    spec:
      containers:
        - name: indexer
          image: ghcr.io/<your-org>/<your-repo>:<program-id>
          args: ["run", "/etc/microscope/microscope.toml"]
          env:
            - name: MICROSCOPE_STATE_DIR
              value: /var/lib/solana-microscope
            - name: RUST_LOG
              value: info
          envFrom:
            - secretRef:
                name: microscope-endpoints
          ports:
            - name: metrics
              containerPort: 9090
            - name: probes
              containerPort: 9091
          livenessProbe:
            httpGet:
              path: /healthz
              port: probes
          readinessProbe:
            httpGet:
              path: /readyz
              port: probes
            periodSeconds: 30
            failureThreshold: 3
          volumeMounts:
            - name: config
              mountPath: /etc/microscope/microscope.toml
              subPath: microscope.toml
              readOnly: true
            - name: state
              mountPath: /var/lib/solana-microscope
      volumes:
        - name: config
          configMap:
            name: microscope-config
  volumeClaimTemplates:
    - metadata:
        name: state
      spec:
        accessModes: ["ReadWriteOnce"]
        resources:
          requests:
            storage: 1Gi
```

`replicas: 1` is a requirement, not a default. Deduplication is per process, so
a second replica indexes the same transactions again: duplicated records in
Loki, which double-counts every alert, and a second set of counter series.

The volume claim holds the RPC polling cursor. Without it a restart resumes
from the stream head and the interval in between is never indexed. It matters
whenever `RPC_URL` is set, in RPC mode and for Yellowstone gap recovery.
`MICROSCOPE_STATE_DIR` must be set explicitly: the default is
`.microscope-state` relative to the working directory.

In Yellowstone mode `/readyz` reports unready once the geyser endpoint has
failed an unary `GetVersion` probe continuously for
`MICROSCOPE_STREAM_STALE_AFTER_SECONDS` (90 by default), which catches a
rejected token or an endpoint that is gone. It does not catch a subscription
wedged against a healthy endpoint, so a probe-only setup misses that case; the
generated `yellowstone_stream_interrupted` alert is the signal there, once more
than five interruptions land in 15 minutes. See
[Health probes](../README.md#health-probes).

## Log shipping

Records reach Grafana as logs, not metrics, and every generated Loki query
selects `{service_name="microscope-indexer"}`. A cluster log collector that
labels pods its own way produces alerts that match nothing and a dashboard that
stays empty while the indexer is healthy.

The bundled [`alloy/config.alloy`](../alloy/config.alloy) cannot be reused: it
discovers containers over the Docker socket and filters on Compose labels.
Reproduce its one meaningful output with Kubernetes discovery instead, setting
`service_name` to `microscope-indexer` on this pod's logs, and keep the
indexer's JSON lines unwrapped so Loki's `json` parser still sees the record
fields.

Verify with a query before trusting any alert:

```logql
{service_name="microscope-indexer"} | json | line_format "{{.kind}}"
```

## Metrics, alerts, and the dashboard

Scrape port 9090 the way your cluster already does, a `ServiceMonitor`,
`PodMonitor`, or scrape annotations. The generated alert rules query Prometheus
and Loki by metric and label name only, so any scrape path works.

Alert rules and the dashboard are generated from `microscope.toml` by the
indexer binary, not by a controller:

```sh
docker run --rm \
  -e RPC_URL="$RPC_URL" \
  -e SLACK_WEBHOOK_URL="$SLACK_WEBHOOK_URL" \
  -v "$PWD/microscope.toml:/etc/microscope/microscope.toml:ro" \
  -v "$PWD/out:/out" \
  ghcr.io/<your-org>/<your-repo>:<program-id> \
  generate-alerting /etc/microscope/microscope.toml /out/alerting /out/dashboards
```

Pass the same `RPC_URL` the pod gets. Outside RPC mode the RPC rules are
emitted only when that variable is set, so generating without it silently drops
stale-poll, lag, checkpoint, quarantine, and recovery monitoring from a
deployment that is actively using RPC recovery. The channel credentials are
required too, for the reason below.

The Compose stack runs this as a one-shot container and hands the output to
Grafana through shared volumes. There is no equivalent handoff here, so run it
in CI whenever `microscope.toml` changes and provision the output into your
Grafana: a `ConfigMap` the Grafana sidecar picks up, a file provisioning mount,
or the Grafana API. Grafana reads alert provisioning only at startup, so a
file-based route needs Grafana restarted after the files change.

The emitted contact points hold placeholders such as `$SLACK_WEBHOOK_URL`, not
values. Grafana expands them from its own environment when it loads the
provisioning files, so the credentials the config selects
(`SLACK_WEBHOOK_URL`, `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`,
`PAGERDUTY_INTEGRATION_KEY`) belong in the Grafana pod. They also have to be
set wherever `generate-alerting` runs, which refuses to emit a rule whose
channel has no credentials rather than shipping one that can never deliver.

This route therefore needs a Grafana whose environment you control. A hosted
Grafana Cloud stack never expands the placeholders; provision its rules and
contact points through the Grafana Cloud API or Terraform instead, as the
Compose Grafana Cloud overlay does by disabling the generator entirely.

## Not covered

Backfill (`microscope-indexer backfill`, a `Job` rather than a long-running
pod), network policy, resource requests and limits, and pod security context.
No Helm chart is planned until there is demand to justify maintaining a second
deployment topology.
