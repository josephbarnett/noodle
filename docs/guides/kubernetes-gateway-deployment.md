# Kubernetes gateway deployment runbook

How to deploy `noodle-proxy` as the gateway topology (ADR 039 §1
row 2) into a Kubernetes cluster, with `noodle-embellish` and
`noodle-shipper` colocated as sidecars per ADR 043 §2.1.

The contracts this guide implements are specified in:

- [`docs/adrs/043-kubernetes-gateway-deployment.md`](../adrs/043-kubernetes-gateway-deployment.md) — the pod / service / CA / scaling decisions.
- [`docs/adrs/034-enterprise-ca-and-external-signing.md`](../adrs/034-enterprise-ca-and-external-signing.md) — BYOCA-static + Vault PKI paths.
- [`docs/adrs/022-otel-collector-embellishment-plane.md`](../adrs/022-otel-collector-embellishment-plane.md) — downstream OTLP collector boundary.

---

## 1. Scope

Covered:

- Building the three release binaries into distroless container
  images.
- Reference `Deployment` + `Service` + `Secret` + `ConfigMap`
  manifests.
- Pointing client workloads at the gateway via `HTTPS_PROXY` and
  CA-trust env vars.
- Verifying the pipeline end-to-end inside the cluster.

Not covered:

- Helm / Kustomize packaging (ADR 043 §4 non-goal).
- Service-mesh integration specifics (Istio, Linkerd) — these
  layer cleanly on top; the gateway is mesh-agnostic.
- Multi-cluster routing.

## 2. Prerequisites

| Item | Detail |
|---|---|
| Kubernetes cluster | 1.28+ |
| Container registry | Reachable from the cluster nodes |
| Docker / Podman | For building the images |
| `kubectl` | 1.28+ |
| Enterprise CA material | `ca.pem`, `ca.key`, optional `chain.pem` — see ADR 034 §4 |
| OTel collector endpoint | Already running in the cluster or accessible from it |

## 3. Build the container images

Three images, one base, one Dockerfile each. The build stage is
shared; the runtime stage is distroless `cc-debian12`.

```dockerfile
# Dockerfile.proxy
FROM rust:1.93-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p noodle-proxy

FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=build /src/target/release/noodle /usr/local/bin/noodle
USER nonroot:nonroot
EXPOSE 62100
ENTRYPOINT ["/usr/local/bin/noodle"]
```

Build and push:

```bash
SHA=$(git rev-parse --short HEAD)
REGISTRY=registry.example.com/noodle

docker build -f Dockerfile.proxy     -t $REGISTRY/proxy:$SHA     .
docker build -f Dockerfile.embellish -t $REGISTRY/embellish:$SHA .
docker build -f Dockerfile.shipper   -t $REGISTRY/shipper:$SHA   .

docker push $REGISTRY/proxy:$SHA
docker push $REGISTRY/embellish:$SHA
docker push $REGISTRY/shipper:$SHA
```

`Dockerfile.embellish` and `Dockerfile.shipper` follow the same
two-stage shape, swapping `-p noodle-proxy` for the appropriate
crate name and `EXPOSE 62100` for nothing (neither sidecar
listens on a port).

## 4. Create the namespace and CA Secret

```bash
kubectl create namespace noodle-gateway

kubectl -n noodle-gateway create secret generic noodle-ca \
    --from-file=ca.pem=/path/to/your-org-ca.pem \
    --from-file=ca.key=/path/to/your-org-ca.key
```

If you also have an intermediate chain, add
`--from-file=chain.pem=/path/to/chain.pem`.

The Secret must be readable by the gateway service account only.
Apply a `Role` + `RoleBinding` if your cluster doesn't lock down
namespace secrets by default.

> **The nonroot proxy cannot read the mounted Secret directly.** A
> `Secret` volume mounts root-owned at the `defaultMode` bits, but the
> proxy runs as distroless `nonroot` (uid 65532) and enforces a
> `0400`-or-stricter, **owner-readable** CA key (`load_static`
> rejects group/other permission bits — see ADR 034). A direct mount
> therefore fails with `Permission denied reading ca.pem`. Stage the
> CA into an `emptyDir` owned by 65532 with an initContainer (§6), and
> point `NOODLE_CA_DIR` at the staged copy — not at the Secret mount.
> ADR 044's CA service removes this hand-off entirely: the proxy
> fetches the cert over HTTP (`GET /ca.pem`) instead of mounting a
> Secret.

> **The nonroot proxy cannot read the mounted Secret directly.** A
> `Secret` volume mounts root-owned at the `defaultMode` bits, but the
> proxy runs as distroless `nonroot` (uid 65532) and enforces a
> `0400`-or-stricter, **owner-readable** CA key (`load_static`
> rejects group/other permission bits — see ADR 034). A direct mount
> therefore fails with `Permission denied reading ca.pem`. Stage the
> CA into an `emptyDir` owned by 65532 with an initContainer (§6), and
> point `NOODLE_CA_DIR` at the staged copy — not at the Secret mount.
> ADR 044's CA service removes this hand-off entirely: the proxy
> fetches the cert over HTTP (`GET /ca.pem`) instead of mounting a
> Secret.

## 5. ConfigMap for environment

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: noodle-config
  namespace: noodle-gateway
data:
  NOODLE_LISTEN:            "0.0.0.0:62100"
  NOODLE_OPS_LISTEN:        "0.0.0.0:9091"
  NOODLE_CA_MODE:           "byoca-static"
  NOODLE_CA_DIR:            "/etc/noodle/ca"
  NOODLE_LAYERED_CORE:      "1"
  # Shipper destination
  NOODLE_OTLP_ENDPOINT:     "http://otel-collector.observability:4318"
```

## 6. Deployment manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: noodle-gateway
  namespace: noodle-gateway
  labels: { app: noodle-gateway }
spec:
  replicas: 2
  selector:
    matchLabels: { app: noodle-gateway }
  template:
    metadata:
      labels: { app: noodle-gateway }
      annotations:
        # Pod-discovery Prometheus scrape. Adjust to match the
        # scrape config the cluster's Prometheus uses; the path
        # and port values are what the proxy serves.
        prometheus.io/scrape: "true"
        prometheus.io/port:   "9091"
        prometheus.io/path:   "/metrics"
    spec:
      volumes:
        # Raw Secret — mounts root-owned, unreadable by the nonroot proxy.
        - name: noodle-ca-secret
          secret: { secretName: noodle-ca, defaultMode: 0400 }
        # initContainer stages the CA here, owned by uid 65532 at 0400.
        - name: noodle-ca
          emptyDir: {}
        - name: noodle-data
          emptyDir: {}
      initContainers:
        # The proxy runs nonroot (uid 65532) and requires an
        # owner-readable 0400 CA key (§4). Copy the Secret into an
        # emptyDir owned by 65532 so the proxy can read it.
        - name: ca-prep
          image: busybox:1.37
          command:
            - "sh"
            - "-c"
            - "install -m 0400 -o 65532 -g 65532 /src-ca/ca.pem /ca/ca.pem &&
               install -m 0400 -o 65532 -g 65532 /src-ca/ca.key /ca/ca.key"
          volumeMounts:
            - { name: noodle-ca-secret, mountPath: /src-ca, readOnly: true }
            - { name: noodle-ca, mountPath: /ca }
      containers:

        - name: proxy
          image: registry.example.com/noodle/proxy:SHA      # set via your CD
          envFrom: [{ configMapRef: { name: noodle-config } }]
          ports:
            - { name: proxy, containerPort: 62100 }
            - { name: ops,   containerPort: 9091 }
          volumeMounts:
            - { name: noodle-ca,   mountPath: /etc/noodle/ca, readOnly: true }
            - { name: noodle-data, mountPath: /home/nonroot/.noodle }
          resources:
            requests: { cpu: "100m", memory: "128Mi" }
            limits:   { cpu: "1",    memory: "512Mi" }
          readinessProbe:
            httpGet: { path: /readyz, port: ops }
            periodSeconds: 5
          livenessProbe:
            httpGet: { path: /healthz, port: ops }
            periodSeconds: 30

        - name: embellish
          image: registry.example.com/noodle/embellish:SHA
          # --watch tails tap.jsonl continuously (WireSource::FileTail).
          # Without it the binary does a one-shot read-to-EOF of the
          # empty startup file and exits, crash-looping the sidecar.
          args:
            - "--watch"
            - "--tap"
            - "/home/nonroot/.noodle/tap.jsonl"
            - "--db"
            - "/home/nonroot/.noodle/rollups.db"
          envFrom: [{ configMapRef: { name: noodle-config } }]
          volumeMounts:
            - { name: noodle-data, mountPath: /home/nonroot/.noodle }
          resources:
            requests: { cpu: "50m", memory: "64Mi" }
            limits:   { cpu: "500m", memory: "256Mi" }

        - name: shipper
          image: registry.example.com/noodle/shipper:SHA
          args:
            - "--db"
            - "/home/nonroot/.noodle/rollups.db"
            - "--endpoint"
            - "$(NOODLE_OTLP_ENDPOINT)"
            - "--poll-secs"
            - "5"
          envFrom: [{ configMapRef: { name: noodle-config } }]
          volumeMounts:
            - { name: noodle-data, mountPath: /home/nonroot/.noodle, readOnly: false }
          resources:
            requests: { cpu: "50m", memory: "32Mi" }
            limits:   { cpu: "200m", memory: "128Mi" }
```

## 7. Service

```yaml
apiVersion: v1
kind: Service
metadata:
  name: noodle-gateway
  namespace: noodle-gateway
spec:
  type: ClusterIP
  selector: { app: noodle-gateway }
  ports:
    - { name: proxy, port: 62100, targetPort: 62100 }
```

For cross-namespace clients the DNS name is
`noodle-gateway.noodle-gateway.svc.cluster.local`.

The ops port (9091, `/healthz` / `/readyz` / `/metrics`) is
intentionally **not** on the Service. The kubelet reaches probes
on the Pod's container port directly; Prometheus reaches `/metrics`
via Pod-IP discovery (annotations on the Pod template, §6). Keeping
ops off the Service prevents accidental ops-surface exposure to
proxy clients.

## 8. NetworkPolicy

Restrict ingress to the gateway from labeled client namespaces
only:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: noodle-gateway-ingress
  namespace: noodle-gateway
spec:
  podSelector: { matchLabels: { app: noodle-gateway } }
  policyTypes: ["Ingress"]
  ingress:
    # Proxy traffic from labeled client namespaces.
    - from:
        - namespaceSelector: { matchLabels: { noodle-client: "true" } }
      ports:
        - { protocol: TCP, port: 62100 }
    # Ops traffic (Prometheus scrape) from the observability namespace.
    # Adjust the selector to match where Prometheus runs in this cluster.
    - from:
        - namespaceSelector: { matchLabels: { name: observability } }
      ports:
        - { protocol: TCP, port: 9091 }
```

The kubelet probes do not transit cluster NetworkPolicy — they
originate on the node and reach the Pod's container port
directly, so `livenessProbe` / `readinessProbe` keep working
without an explicit allow rule.

Label every client namespace that should be allowed to use the
gateway:

```bash
kubectl label namespace team-foo noodle-client=true
```

## 9. Apply

```bash
kubectl apply -f configmap.yaml
kubectl apply -f deployment.yaml
kubectl apply -f service.yaml
kubectl apply -f networkpolicy.yaml

kubectl -n noodle-gateway rollout status deployment/noodle-gateway
```

## 10. Point clients at the gateway

In each client workload's `Deployment`, set:

```yaml
env:
  - name: HTTPS_PROXY
    value: "http://noodle-gateway.noodle-gateway.svc.cluster.local:62100"
  - name: NODE_EXTRA_CA_CERTS
    value: "/etc/noodle/ca.pem"
  - name: REQUESTS_CA_BUNDLE
    value: "/etc/noodle/ca.pem"
  - name: SSL_CERT_FILE
    value: "/etc/noodle/ca.pem"
volumeMounts:
  - { name: noodle-ca, mountPath: /etc/noodle, readOnly: true }
volumes:
  - name: noodle-ca
    secret:
      secretName: noodle-ca         # same Secret as the gateway, or a Secret containing only ca.pem
      items: [{ key: ca.pem, path: ca.pem }]
```

## 11. Verify the pipeline inside the cluster

```bash
# Probes — the kubelet runs these continuously, but you can hit them
# directly from inside the proxy container to confirm the ops listener
# is bound and serving:
kubectl -n noodle-gateway exec deploy/noodle-gateway -c proxy -- \
    sh -c 'curl -sS http://127.0.0.1:9091/healthz && \
           curl -sS http://127.0.0.1:9091/readyz && \
           curl -sS http://127.0.0.1:9091/metrics | head -20'
# Expect: "ok", "ready", and a Prometheus exposition starting with
# "# HELP noodle_proxy_uptime_seconds ...".

# Drive a synthetic Anthropic request from a client pod (or any pod with curl + the CA mounted):
kubectl -n team-foo exec -it some-pod -- \
    curl -sS -x http://noodle-gateway.noodle-gateway.svc.cluster.local:62100 \
         --cacert /etc/noodle/ca.pem \
         -X POST https://api.anthropic.com/v1/messages \
         -H 'content-type: application/json' \
         -H "x-api-key: $ANTHROPIC_API_KEY" \
         -H 'anthropic-version: 2023-06-01' \
         -d '{"model":"claude-3-5-sonnet-20240620","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'

# Tap output landed in the proxy container:
kubectl -n noodle-gateway exec deploy/noodle-gateway -c proxy -- \
    wc -l /home/nonroot/.noodle/tap.jsonl

# Rollups landed in the embellish container's SQLite:
kubectl -n noodle-gateway exec deploy/noodle-gateway -c embellish -- \
    sqlite3 /home/nonroot/.noodle/rollups.db \
        "SELECT delivery_status, COUNT(*) FROM ai_telemetry_v_0_0_2 GROUP BY delivery_status;"

# Shipper drained the cursor:
kubectl -n noodle-gateway logs deploy/noodle-gateway -c shipper | grep "tick complete"
```

Expected:

- `/healthz` returns `ok`, `/readyz` returns `ready`, `/metrics` emits
  Prometheus text exposition.
- `tap.jsonl` line count > 0.
- `rollups.db` shows rows in `delivery_status='delivered'`.
- Shipper log emits one `tick complete claimed=N delivered=N` line per poll cycle.

## 12. Scaling

```bash
kubectl -n noodle-gateway autoscale deployment noodle-gateway \
    --cpu-percent=70 --min=2 --max=10
```

ADR 043 §2.8 covers the in-memory `MarkingStore` per-Pod
assumption — `turn_id` may discontinue across pod boundaries
within a single session. Acceptable for v1; if your downstream
needs continuity, deploy as `StatefulSet` with session-sticky
selectors.

## 13. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Client `curl: (60) SSL certificate problem: unable to get local issuer certificate` | Client doesn't trust the gateway's CA | Confirm `NODE_EXTRA_CA_CERTS` / `--cacert` points at the same `ca.pem` the Secret holds |
| Proxy pod crashlooping with `noodle: CA config invalid: SPKI mismatch` | `ca.pem` and `ca.key` in the Secret don't agree | Rotate the Secret with matching keypair; ADR 034 §4 |
| `tap.jsonl` line count stays 0 | Traffic isn't reaching the proxy | Check NetworkPolicy labels; check client `HTTPS_PROXY` value with `kubectl -n team-foo exec ... -- env \| grep PROXY` |
| Shipper log shows `transient error: connection refused` to the OTLP endpoint | Collector unreachable from gateway namespace | Check `NetworkPolicy` egress; check `NOODLE_OTLP_ENDPOINT` value |
| `delivery_status='pending'` count grows without bound | Shipper isn't running, or collector is rejecting | Inspect shipper logs; ADR 042 §2 — non-2xx responses move rows to `retry`, then `poison` after `--max-retries` |
| Pod restarts lose rollup rows | `emptyDir` is ephemeral by design | Upgrade to PVC per ADR 043 §2.6 if needed |

## 14. Where to go next

- [`docs/adrs/043-...`](../adrs/043-kubernetes-gateway-deployment.md) — the design contract this runbook implements.
- [`shipper-runbook.md`](shipper-runbook.md) — downstream OTLP delivery troubleshooting.
- [`codec-perf-bench.md`](codec-perf-bench.md) — perf budgets for sizing the proxy resource limits.
