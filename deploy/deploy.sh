#!/usr/bin/env bash
# ADR 043 proof — build, push, and deploy the noodle gateway to the
# trading-platform RPi5 cluster (arm64). Run from the repo root.
#
# Prereqs you provide:
#   - docker logged in to ghcr.io   (echo "$GHCR_PAT" | docker login ghcr.io -u <user> --password-stdin)
#   - GHCR_USER + GHCR_PAT exported  (PAT needs write:packages; cluster pulls with read:packages)
#   - kubectl context `trading-platform` reachable
#   - a local noodle root CA at ~/.config/noodle/ca/{ca.pem,ca.key}
#     (this script generates one via the release proxy if absent)
#
# This script is idempotent: re-running re-pushes images and re-applies.
set -euo pipefail

REGISTRY="${REGISTRY:-ghcr.io/josephbarnett}"
TAG="${TAG:-dev}"
# Defaults to the local rancher-desktop cluster (test-local-first).
# For the RPi cluster: CTX=trading-platform ./deploy/deploy.sh
CTX="${CTX:-rancher-desktop}"
NS="${NS:-noodle}"
CA_DIR="${CA_DIR:-$HOME/.config/noodle/ca}"

# Accept either GHCR_* or the shorter GH_* names.
GHCR_USER="${GHCR_USER:-${GH_USER:-}}"
GHCR_PAT="${GHCR_PAT:-${GH_PAT:-}}"

say() { printf '\n=== %s ===\n' "$*"; }

# 1. Build + push the four arm64 images. `viewer` embeds its React UI
# via rust-embed; build the frontend first so `web/dist/` carries the
# assets into the Docker build context.
say "build viewer web UI (vite)"
( cd crates/noodle-viewer/web && npm ci --silent && npm run build --silent )

say "build + push images (${REGISTRY}, tag ${TAG}, linux/arm64)"
for t in proxy embellish shipper viewer; do
  docker build --platform linux/arm64 --target "$t" -t "${REGISTRY}/noodle-${t}:${TAG}" .
  docker push "${REGISTRY}/noodle-${t}:${TAG}"
done

# 2. Ensure a local noodle root CA exists (BYOCA-static source of truth).
if [[ ! -f "${CA_DIR}/ca.pem" || ! -f "${CA_DIR}/ca.key" ]]; then
  say "no CA at ${CA_DIR} — generating one via the release proxy"
  cargo build --release --bin noodle
  ./target/release/noodle >/dev/null 2>&1 &
  PROXY_PID=$!
  sleep 2
  kill "${PROXY_PID}" 2>/dev/null || true
fi
test -f "${CA_DIR}/ca.pem" || { echo "CA generation failed — ${CA_DIR}/ca.pem missing"; exit 1; }

# 3. Namespace.
say "namespace ${NS} on ${CTX}"
kubectl --context "$CTX" create namespace "$NS" --dry-run=client -o yaml | kubectl --context "$CTX" apply -f -

# 4. Secrets: ghcr pull secret + the BYOCA CA.
say "ghcr image-pull secret"
: "${GHCR_USER:?set GHCR_USER}"; : "${GHCR_PAT:?set GHCR_PAT}"
kubectl --context "$CTX" -n "$NS" create secret docker-registry ghcr \
  --docker-server=ghcr.io --docker-username="$GHCR_USER" --docker-password="$GHCR_PAT" \
  --dry-run=client -o yaml | kubectl --context "$CTX" apply -f -

say "noodle-ca secret (from ${CA_DIR})"
kubectl --context "$CTX" -n "$NS" create secret generic noodle-ca \
  --from-file=ca.pem="${CA_DIR}/ca.pem" --from-file=ca.key="${CA_DIR}/ca.key" \
  --dry-run=client -o yaml | kubectl --context "$CTX" apply -f -

# 5. OTLP sink script ConfigMap (from the tested demos/otlp_sink.py).
say "otlp-sink script ConfigMap"
kubectl --context "$CTX" -n "$NS" create configmap noodle-otlp-sink-script \
  --from-file=otlp_sink.py=demos/otlp_sink.py \
  --dry-run=client -o yaml | kubectl --context "$CTX" apply -f -

# 5b. Proxy config ConfigMap (the editable tag/enhancer language). The
# proxy reads it via NOODLE_CONFIG=/etc/noodle/noodle.toml. Edit the
# ConfigMap directly (kubectl edit configmap noodle-config) or change
# crates/noodle-proxy/default-noodle.toml and re-run this script; either
# way roll the deployment to apply (config is read once at startup).
say "proxy config ConfigMap (from crates/noodle-proxy/default-noodle.toml)"
kubectl --context "$CTX" -n "$NS" create configmap noodle-config \
  --from-file=noodle.toml=crates/noodle-proxy/default-noodle.toml \
  --dry-run=client -o yaml | kubectl --context "$CTX" apply -f -

# 6. Apply manifests.
say "apply manifests"
kubectl --context "$CTX" -n "$NS" apply -f deploy/k8s/

# 7. Roll the pods. `apply` is a no-op when the manifest is unchanged (the
# image tag is a fixed `:dev`), so without an explicit restart the cluster
# keeps the OLD pods even though a fresh `:dev` was just pushed — the deploy
# silently ships nothing. `rollout restart` forces new pods; pullPolicy:Always
# then re-pulls the freshly-pushed image.
say "rollout restart (pull the freshly-pushed :dev)"
kubectl --context "$CTX" -n "$NS" rollout restart deploy/noodle-gateway

say "rollout status"
kubectl --context "$CTX" -n "$NS" rollout status deploy/noodle-otlp-sink --timeout=120s
kubectl --context "$CTX" -n "$NS" rollout status deploy/noodle-gateway   --timeout=180s

say "deployed — now run: scripts/watch-gateway.sh"
