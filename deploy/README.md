# ADR 043 proof — deploy the noodle gateway and watch it consume data

This is the **proof that ADR 043 actually deploys** — not the design
doc, the running thing. It builds the three arm64 images, deploys the
sidecar gateway Pod to a Kubernetes cluster, and gives you a local
script to point your quad's Claude traffic at it and watch data flow
through in real time.

**Both scripts default to the local `rancher-desktop` cluster**
(test-local-first). Target the RPi cluster with `CTX=trading-platform`
in front of either command.

It is also the **stepping stone to ADR 044** (the scalable, CA-service,
Parquet-data-plane version): the images, manifests, and the
`WireSource`-tail embellish here are exactly what 044 extends.

## What's here

| Path | What |
|---|---|
| `../Dockerfile` | Multi-stage build → `proxy` / `embellish` / `shipper` images (arm64, distroless) |
| `k8s/deployment.yaml` | The sidecar Pod: proxy + **embellish `--watch`** + shipper, one `emptyDir` |
| `k8s/service.yaml` | `ClusterIP` for the proxy port |
| `k8s/otlp-sink.yaml` | Mock OTLP collector (runs `demos/otlp_sink.py`) so the shipper has a target |
| `../crates/noodle-proxy/default-noodle.toml` | The proxy config (tag/enhancer language); `deploy.sh` loads it into the `noodle-config` ConfigMap |
| `deploy.sh` | Build → push → secrets → apply (you run it) |
| `../scripts/watch-gateway.sh` | Point your quad at the gateway and watch it consume data live |

## The two defects this proof already caught

ADR 043 was design-only; building it surfaced two things that would have
failed in production:

1. **Missing `libz.so.1`** in the distroless runtime — the proxy binary
   wouldn't start. Fixed in the Dockerfile (proxy stage copies the lib).
2. **`embellish` was one-shot**, but ADR 043 runs it as a tailing
   sidecar. A one-shot container processes the empty startup file and
   exits, never mapping live traffic. Fixed properly by landing the
   S12/S13 `WireSource::FileTail` and running embellish with `--watch`.

## Run it

### 1. Prerequisites you provide

```sh
# ghcr login (PAT needs write:packages; the cluster pulls with read:packages)
export GHCR_USER=josephbarnett
export GHCR_PAT=ghp_xxx
echo "$GHCR_PAT" | docker login ghcr.io -u "$GHCR_USER" --password-stdin

# a reachable cluster — local rancher-desktop by default
kubectl --context rancher-desktop get nodes
```

### 2. Build, push, deploy

```sh
./deploy/deploy.sh
```

This builds + pushes the three images, generates a local noodle root CA
if you don't have one (`~/.config/noodle/ca/`), creates the `ghcr` pull
secret + the `noodle-ca` BYOCA secret + the OTLP-sink script ConfigMap +
the `noodle-config` proxy-config ConfigMap (from
`crates/noodle-proxy/default-noodle.toml`), applies the manifests, and
waits for rollout.

### 3. Point your quad at it and watch

```sh
./scripts/watch-gateway.sh
```

It port-forwards the gateway to `localhost:62100`, prints the exports to
point `claude` at it (`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS`), optionally
drives one smoke request if `ANTHROPIC_API_KEY` is set, and then tails
the **embellish** and **otlp-sink** logs so you watch it happen:

```
embellish  noodle-embellish: rows_written=1 requests=1 responses=1
otlp-sink  OTLP <- /v1/logs bytes=2914 log_records=1 -> /tmp/otlp-bodies/001.json
```

Each `claude` turn → the proxy MITMs it → `tap.jsonl` grows → embellish
tails it and maps a row → the shipper ships it → the sink logs the OTLP
batch. That is the gateway consuming data end to end, in your cluster.

## Configuration — editing the proxy's tag/enhancer language

The proxy's config (the `noodle.toml` tag set + enhancers) is no longer
baked-only into the binary. It still embeds `default-noodle.toml` as the
fallback, but the deployed proxy reads
`NOODLE_CONFIG=/etc/noodle/noodle.toml`, mounted from the `noodle-config`
ConfigMap. Config is read **once at startup**, so any edit needs a pod
roll to take effect.

**In-cluster — edit the live ConfigMap:**

```sh
kubectl -n noodle edit configmap noodle-config        # edit the noodle.toml key
kubectl -n noodle rollout restart deploy/noodle-gateway
```

Or edit `crates/noodle-proxy/default-noodle.toml` and re-run
`./deploy/deploy.sh` (it re-creates the ConfigMap and rolls the pods).

**Locally with Docker — mount a file from your drive:**

```sh
docker run --rm \
  -v "$PWD/my-noodle.toml:/etc/noodle/noodle.toml:ro" \
  -e NOODLE_CONFIG=/etc/noodle/noodle.toml \
  -e NOODLE_LISTEN=0.0.0.0:62100 \
  -p 62100:62100 ghcr.io/josephbarnett/noodle-proxy:dev
```

If `NOODLE_CONFIG` points at a missing or invalid file, the proxy
**fails loud** (exits non-zero with a `NOODLE_CONFIG load failed`
message) rather than silently degrading — so a typo in your mounted
file can't quietly no-op. With `NOODLE_CONFIG` unset it falls back to
`~/.noodle/noodle.toml`, then the embedded default.

## Notes

- **CA:** the proxy runs BYOCA-static from the `noodle-ca` Secret, whose
  source is your local `~/.config/noodle/ca/ca.pem`. The same file is
  what your quad trusts (`NODE_EXTRA_CA_CERTS`), so leaves minted
  in-cluster verify locally. (ADR 044 replaces this hand-off with the
  `noodle-ca` service's `GET /ca.pem`.)
- **Image tag** is `dev` while this is unmerged. On commit, retag to the
  git SHA (`TAG=$(git rev-parse --short HEAD) ./deploy/deploy.sh`).
- **Scaling / CA service / Parquet** are ADR 044, not this proof.
