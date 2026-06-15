#!/usr/bin/env bash
# Point your quad at the in-cluster noodle gateway and WATCH it consume
# data live. Run AFTER deploy/deploy.sh has the gateway green.
#
# What it does:
#   1. port-forwards the gateway Service to localhost:62100
#   2. prints the exact exports to point your quad's `claude` at it
#   3. (optional) drives one smoke request if ANTHROPIC_API_KEY is set
#   4. tails the embellish + otlp-sink logs so you watch rows get mapped
#      and shipped in real time
set -euo pipefail

# Defaults to the local rancher-desktop cluster. For the RPi cluster:
#   CTX=trading-platform ./scripts/watch-gateway.sh
CTX="${CTX:-rancher-desktop}"
NS="${NS:-noodle}"
CA="${CA:-$HOME/.config/noodle/ca/ca.pem}"
PORT="${PORT:-62100}"

test -f "$CA" || { echo "CA not found at $CA — run deploy/deploy.sh first"; exit 1; }

# Fail loudly if the gateway isn't where we're looking (wrong context is
# the easy mistake — local vs RPi).
if ! kubectl --context "$CTX" -n "$NS" get deploy/noodle-gateway >/dev/null 2>&1; then
  echo "No noodle-gateway in namespace '$NS' on context '$CTX'."
  echo "Deployed to a different cluster? Re-run with CTX=<context>, e.g.:"
  echo "  CTX=rancher-desktop $0      (local)"
  echo "  CTX=trading-platform $0     (RPi)"
  exit 1
fi

echo "=== port-forwarding svc/noodle-gateway (${CTX}) → localhost:${PORT} ==="
kubectl --context "$CTX" -n "$NS" port-forward svc/noodle-gateway "${PORT}:62100" \
  >/tmp/noodle-portforward.log 2>&1 &
PF=$!
trap 'kill $PF 2>/dev/null || true' EXIT
sleep 2
if ! kill -0 "$PF" 2>/dev/null; then
  echo "port-forward failed:"; cat /tmp/noodle-portforward.log; exit 1
fi

cat <<EOF

=== point your quad's Claude at the gateway ===
Paste these into the shell that runs claude, then use it normally:

  export HTTPS_PROXY=http://127.0.0.1:${PORT}
  export NODE_EXTRA_CA_CERTS=${CA}
  claude -p "say hello"

EOF

if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "=== ANTHROPIC_API_KEY set — driving one smoke request through the gateway ==="
  curl -sS -x "http://127.0.0.1:${PORT}" --cacert "$CA" \
    -X POST https://api.anthropic.com/v1/messages \
    -H "x-api-key: ${ANTHROPIC_API_KEY}" \
    -H 'anthropic-version: 2023-06-01' \
    -H 'content-type: application/json' \
    -d '{"model":"claude-haiku-4-5","max_tokens":16,"messages":[{"role":"user","content":"say hi"}]}' \
    -o /dev/null -w "  gateway returned HTTP %{http_code}\n" || echo "  (request failed — check the gateway)"
fi

echo
echo "=== watching the gateway consume data (Ctrl-C to stop) ==="
echo "embellish logs 'rows_written=N' as it maps pairs; the sink logs each OTLP batch it receives."
echo
# embellish: rows_written=N per mapped pair; sink: one line per OTLP POST.
kubectl --context "$CTX" -n "$NS" logs -f -l app=noodle-gateway   -c embellish --prefix --tail=20 &
kubectl --context "$CTX" -n "$NS" logs -f -l app=noodle-otlp-sink              --prefix --tail=20 &
wait
