.PHONY: help doctor tooling build wasm bench bench-emit-close-path check test test-unit test-e2e test-e2e-exec-claude test-prop lint fmt fmt-check hooks-install hooks-uninstall ci-local sweep run run-pretty run-release run-release-layered stop demo-upstream ca-path ca-trust-macos ca-trust-macos-system ca-untrust-macos ca-untrust-macos-system mitm-smoke tap-path tap-tail side-effects-path side-effects-tail roundtrips-path roundtrips-tail otlp-sink otlp-sink-kill otlp-sink-test embellish ship ship-status ship-rewind demo-attribution demo-attribution-build demo-attribution-env viewer-build viewer viewer-dev clean macos-tooling macos-doctor macos-staticlib macos-build macos-test macos-open-xcode macos-install macos-install-reset macos-list macos-logs macos-logs-recent macos-uninstall macos-clean macos-ca-path macos-env-export

# Use bash for recipes that rely on `set -o pipefail` and similar.
SHELL := /usr/bin/env bash
.SHELLFLAGS := -eu -o pipefail -c

# Default target — print available commands.
help:  ## Show this help.
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?##/ {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ── Build / check ──────────────────────────────────────────────────

build:  ## Build every crate in the workspace.
	cargo build --workspace

wasm:  ## Build noodle-detect for wasm32-unknown-unknown (plugin facade, ADR 039 §8 acceptance signal #1).
	cargo build --release --target wasm32-unknown-unknown -p noodle-detect
	@echo "→ target/wasm32-unknown-unknown/release/libnoodle_detect.rlib"

bench:  ## Legacy-vs-layered codec perf bench (A.7; results in docs/guides/codec-perf-bench.md).
	cargo bench --bench codec_paths -p noodle-adapters

bench-emit-close-path:  ## ADR 049 §9.1 close-path bench (results in docs/guides/emit-close-path-bench.md).
	cargo bench --bench emit_close_path -p noodle-proxy

check:  ## Cheap type-check (no codegen).
	cargo check --workspace

# ── Tests ──────────────────────────────────────────────────────────

test:  ## Run every test (unit + functional + e2e + property).
	cargo test --workspace

test-unit:  ## Library/unit tests only.
	cargo test --workspace --lib

test-e2e:  ## End-to-end tests (proxy + mock upstream).
	cargo test -p noodle-proxy --tests

test-e2e-exec-claude:  ## Real exec-claude e2e (needs claude CLI + Anthropic auth).
	cargo test -p noodle-proxy --test e2e_marking_exec_claude -- --ignored --nocapture

test-prop:  ## Property tests (MarkerScanner FSM, ~1024 random cases).
	cargo test -p noodle-core --test marker_property

# ── Style ──────────────────────────────────────────────────────────

lint:  ## Strict clippy — fails on any warning across the workspace.
	cargo clippy --workspace --all-targets -- -D warnings

fmt:  ## Apply rustfmt.
	cargo fmt --all

# ── Git hooks ──────────────────────────────────────────────────────

hooks-install:  ## Enable the in-repo pre-commit hook (.githooks/).
	@git config core.hooksPath .githooks
	@echo "git hooks enabled — pre-commit will run fmt + clippy + tests"

hooks-uninstall:  ## Revert to git's default hooks dir (.git/hooks/).
	@git config --unset core.hooksPath || true
	@echo "git hooks reverted to default"

ci-local:  ## Run the same gates CI runs (fmt + clippy + tests).
	@$(MAKE) fmt-check
	@$(MAKE) lint
	@$(MAKE) test

fmt-check:  ## Check rustfmt without writing changes.
	cargo fmt --all -- --check

sweep: lint test  ## Lint + full test sweep (pre-commit gate).

# ── Run ────────────────────────────────────────────────────────────

run:  ## Start the proxy (debug build). Stdout = JSON wire log; stderr → ./noodle.err.
	cargo run --quiet --bin noodle 2>noodle.err

run-pretty:  ## Start the proxy with stdout pretty-printed through `jq .`.
	@command -v jq >/dev/null || { echo "jq not installed"; exit 1; }
	cargo run --quiet --bin noodle 2>noodle.err | jq .

run-release:  ## Start the proxy (release build, lower latency for live LLM use).
	cargo run --release --quiet --bin noodle 2>noodle.err

run-release-layered:  ## Same as run-release, but SSE decodes via the layered core (story 031).
	NOODLE_LAYERED_CORE=1 cargo run --release --quiet --bin noodle 2>noodle.err

stop:  ## Send SIGINT to a running noodle proxy.
	@if pkill -INT -f 'target/debug/noodle' 2>/dev/null \
	    || pkill -INT -f 'target/release/noodle' 2>/dev/null; then \
		echo "stopped"; \
	else \
		echo "not running"; \
	fi

demo-upstream:  ## Start a Python HTTP server on :8765 that emits a noodle marker.
	@command -v python3 >/dev/null || { echo "python3 not installed"; exit 1; }
	python3 scripts/demo_upstream.py

# ── TLS MITM CA helpers ────────────────────────────────────────────

ca-path:  ## Print the path to the noodle root CA cert (for NODE_EXTRA_CA_CERTS, --cacert, etc).
	@echo "$$HOME/.config/noodle/ca/ca.pem"

ca-trust-macos:  ## Install the noodle root CA into the macOS user keychain (Chromium-compatible).
	@test -f "$$HOME/.config/noodle/ca/ca.pem" \
	    || { echo "no CA at $$HOME/.config/noodle/ca/ca.pem — run 'make run' once to generate it"; exit 1; }
	@# Flags matter for Chromium/Electron pickup:
	@#   -r trustRoot   mark as root CA (not intermediate)
	@#   -p ssl -p basic   explicit policies — without these the cert is
	@#                     imported but not trusted for anything in particular
	@#   -k login.keychain-db   user keychain; no sudo
	@# Will prompt for your login password.
	security add-trusted-cert \
	    -r trustRoot \
	    -p ssl -p basic \
	    -k "$$HOME/Library/Keychains/login.keychain-db" \
	    "$$HOME/.config/noodle/ca/ca.pem"
	@echo
	@echo "noodle CA trusted in user keychain — Chromium/Electron apps must be"
	@echo "FULLY QUIT (Cmd-Q) and relaunched to pick up the new trust."
	@echo
	@echo "Verify with:  security dump-trust-settings | grep -A 3 noodle"

ca-trust-macos-system:  ## Install into the SYSTEM keychain (admin/sudo; broader blast radius, but Chrome Root Store on macOS honors it).
	@test -f "$$HOME/.config/noodle/ca/ca.pem" \
	    || { echo "no CA at $$HOME/.config/noodle/ca/ca.pem — run 'make run' once to generate it"; exit 1; }
	@echo "This installs the CA system-wide. Every macOS-native client will trust it."
	@echo "Press Ctrl-C now if you don't want that; otherwise enter your sudo password."
	sudo security add-trusted-cert \
	    -d -r trustRoot \
	    -p ssl -p basic \
	    -k /Library/Keychains/System.keychain \
	    "$$HOME/.config/noodle/ca/ca.pem"
	@echo "noodle CA trusted system-wide. To rollback: make ca-untrust-macos-system"

ca-untrust-macos:  ## Remove the noodle root CA from the macOS user keychain.
	@security delete-certificate -c 'noodle MITM root CA' \
	    "$$HOME/Library/Keychains/login.keychain-db" \
	    && echo "removed from login keychain" || echo "not found in login keychain"

ca-untrust-macos-system:  ## Remove the noodle root CA from the System keychain (sudo).
	@sudo security delete-certificate -c 'noodle MITM root CA' \
	    /Library/Keychains/System.keychain \
	    && echo "removed from system keychain" || echo "not found in system keychain"

# ── TAP debugger ───────────────────────────────────────────────────

# The paths the proxy writes its four JSONL streams to.
TAP_FILE          := $(HOME)/.noodle/tap.jsonl
SIDE_EFFECTS_FILE := $(HOME)/.noodle/side_effects.jsonl
ROUNDTRIPS_FILE   := $(HOME)/.noodle/roundtrips.jsonl
# events.jsonl + frames.jsonl retired per ADR 023 / story 035 —
# their content lives on tap.jsonl's events[] / frames[] fields.
# Embellish + shipper defaults. Override on the command line:
#   make embellish ROLLUPS_DB=/path/to/rollups.db
#   make ship      OTLP_ENDPOINT=http://collector.internal:4318
ROLLUPS_DB    ?= /tmp/noodle-rollups.db
OTLP_ENDPOINT ?= http://127.0.0.1:4318
# The CA the layered core mints + uses for TLS MITM.
CA_PEM := $(HOME)/.config/noodle/ca/ca.pem
# Where the noodle viewer's React app lives.
VIEWER_WEB := crates/noodle-viewer/web

tap-path:  ## Print the path to the TAP-format JSONL log.
	@echo "$(TAP_FILE)"

tap-tail:  ## Live-tail the TAP log, pretty-printed via jq. Requires `make run-release` running.
	@command -v jq >/dev/null || { echo "jq not installed"; exit 1; }
	@test -f "$(TAP_FILE)" \
	    || { echo "no tap log at $(TAP_FILE) — start the proxy first ('make run-release &')"; exit 1; }
	tail -F "$(TAP_FILE)" | jq -c '{rid: .event_id, dir: .direction, prov: .provider, sess: .session_hash, st: (.body.usage.output_tokens // .status)}'

side-effects-path:  ## Print the path to the side-effect JSONL log (ADR 020 — attribution loop output).
	@echo "$(SIDE_EFFECTS_FILE)"

roundtrips-path:  ## Print the path to the per-round-trip JSONL log (ADR 023 / story 040.b).
	@echo "$(ROUNDTRIPS_FILE)"

roundtrips-tail:  ## Live-tail roundtrips.jsonl: one record per LLM round trip, with TurnUsage + ToolCall. Requires the layered core running.
	@command -v jq >/dev/null || { echo "jq not installed"; exit 1; }
	@test -f "$(ROUNDTRIPS_FILE)" \
	    || { echo "no roundtrips log at $(ROUNDTRIPS_FILE) — start the proxy first ('make run-release-layered &')"; exit 1; }
	tail -F "$(ROUNDTRIPS_FILE)" | jq -c '{event_id, turn_id: .correlation.turn_id, agent_run_id: .correlation.agent_run_id, finish: .response.finish, has_tool_call: ((.response.events // [] | map(select(.ToolCall != null)) | length) > 0), usage: (.response.events // [] | map(select(.TurnEnd != null) | .TurnEnd.usage) | first)}'

side-effects-tail:  ## Live-tail the side-effect log: Hints / Artifacts / Audits / Resolved records. Requires the layered core running.
	@command -v jq >/dev/null || { echo "jq not installed"; exit 1; }
	@test -f "$(SIDE_EFFECTS_FILE)" \
	    || { echo "no side-effects log at $(SIDE_EFFECTS_FILE) — start the layered-core proxy first ('make run-release-layered &')"; exit 1; }
	tail -F "$(SIDE_EFFECTS_FILE)" | jq -c '.'

# ── Attribution demo (Path B: HTTPS_PROXY forward proxy) ──────────
#
# `make demo-attribution` is the one-stop runbook for live-testing
# the full attribution loop end-to-end against api.anthropic.com.
# Path B = the HTTPS_PROXY forward-proxy mode (NOT the macOS sysext
# / transparent proxy). On this development checkout, Path B is the
# only safe path — building Noodle.app activates the sysext and
# corrupts the captures corpus.
#
# After `make demo-attribution` prints the env vars, paste them into
# the shell where you'll run Claude Code (or curl).

demo-attribution-build:  ## Build the layered-core proxy in release mode.
	cargo build --release --bin noodle

demo-attribution: demo-attribution-build  ## Print the env vars + commands to live-test the attribution loop with Claude Code.
	@echo ""
	@echo "── noodle attribution demo (Path B: HTTPS_PROXY) ─────────────────"
	@echo ""
	@echo "1. Start noodle (this terminal):"
	@echo ""
	@echo "     NOODLE_LAYERED_CORE=1 ./target/release/noodle"
	@echo ""
	@echo "   (or in the background: 'make run-release-layered &')"
	@echo ""
	@echo "   On first run the CA is generated at $(CA_PEM)."
	@echo ""
	@echo "2. In a second terminal, point your client at noodle. For"
	@echo "   the Claude Code CLI (Node-based, honors NODE_EXTRA_CA_CERTS):"
	@echo ""
	@echo "     export HTTPS_PROXY=http://127.0.0.1:62100"
	@echo "     export NODE_EXTRA_CA_CERTS=$(CA_PEM)"
	@echo "     claude    # then ask it to do anything"
	@echo ""
	@echo "   For curl smoke tests:"
	@echo ""
	@echo "     ANTHROPIC_API_KEY=sk-ant-... make mitm-smoke"
	@echo ""
	@echo "3. In a third terminal, watch the attribution records land:"
	@echo ""
	@echo "     make side-effects-tail"
	@echo ""
	@echo "   What to look for:"
	@echo "     • {\"kind\":\"hint\",\"category\":\"tool\",\"source\":\"user_agent\",\"value\":\"Claude Code\"}"
	@echo "         → noodle detected the client by its User-Agent"
	@echo "     • {\"kind\":\"audit\",\"kind_inner\":\"injected\",...}"
	@echo "         → the attribution directive reached the upstream system prompt"
	@echo "     • {\"kind\":\"artifact\",\"name\":\"work_type\",\"value\":\"...\"}"
	@echo "         → the model emitted a marker and noodle stripped it"
	@echo "     • {\"kind\":\"hint\",\"source\":\"marker\",\"category\":\"work_type\",\"value\":\"...\"}"
	@echo "         → the stripped marker fed the Resolver"
	@echo "     • {\"kind\":\"resolved\",\"resolved\":{\"tool\":\"Claude Code\",\"work_type\":\"...\"}}"
	@echo "         → the attribution-product loop closed end-to-end"
	@echo ""
	@echo "──────────────────────────────────────────────────────────────────"

demo-attribution-env: demo-attribution-build  ## Print just the export lines to paste into a client shell.
	@test -f "$(CA_PEM)" \
	    || { echo "no CA at $(CA_PEM) — start the proxy first ('make run-release-layered &')"; exit 1; }
	@echo "export HTTPS_PROXY=http://127.0.0.1:62100"
	@echo "export NODE_EXTRA_CA_CERTS=$(CA_PEM)"
	@echo "export REQUESTS_CA_BUNDLE=$(CA_PEM)"
	@echo "export SSL_CERT_FILE=$(CA_PEM)"
	@echo "export CURL_CA_BUNDLE=$(CA_PEM)"

# ── Embellish + ship pipeline (story 042 + 043) ────────────────────

otlp-sink:  ## Run the demo's OTLP/HTTP receiver (writes bodies to /tmp/noodle-demo/otlp-bodies; logs to stderr). See demos/otlp_sink.py.
	@mkdir -p /tmp/noodle-demo
	python3 $(CURDIR)/demos/otlp_sink.py

otlp-sink-kill:  ## Kill whatever is currently bound to 127.0.0.1:4318 (e.g. a stale sink from an earlier demo run).
	@PID=$$(lsof -nP -iTCP:4318 -sTCP:LISTEN -t 2>/dev/null | sort -u | head -1); \
	if [ -z "$$PID" ]; then \
		echo "nothing listening on 4318"; \
	else \
		echo "killing pid=$$PID"; kill "$$PID"; \
		for i in 1 2 3 4 5; do \
			lsof -nP -iTCP:4318 -sTCP:LISTEN >/dev/null 2>&1 || { echo "port freed"; exit 0; }; \
			sleep 0.5; \
		done; \
		echo "still bound; sending SIGKILL"; kill -9 "$$PID" 2>/dev/null || true; \
	fi

otlp-sink-test:  ## Self-test the OTLP sink: 4 checks (POST, malformed, SIGINT, AddrInUse).
	bash $(CURDIR)/demos/test_otlp_sink.sh

embellish:  ## Map tap.jsonl → ai-telemetry v0.0.2 SQLite rollups at $(ROLLUPS_DB) (story 042).
	@test -f "$(TAP_FILE)" \
	    || { echo "no tap log at $(TAP_FILE) — drive traffic through the proxy first"; exit 1; }
	cargo build --release --quiet -p noodle-embellish
	./target/release/noodle-embellish --tap "$(TAP_FILE)" --db "$(ROLLUPS_DB)"
	@echo "→ rollups db at $(ROLLUPS_DB)"

ship:  ## Run noodle-shipper against $(ROLLUPS_DB), OTLP/HTTP to $(OTLP_ENDPOINT) (story 043).
	@test -f "$(ROLLUPS_DB)" \
	    || { echo "no rollups db at $(ROLLUPS_DB) — run 'make embellish' first"; exit 1; }
	cargo build --release --quiet -p noodle-shipper
	./target/release/noodle-shipper --db "$(ROLLUPS_DB)" --endpoint "$(OTLP_ENDPOINT)"

ship-rewind:  ## Reset every rollup row in $(ROLLUPS_DB) back to delivery_status='pending' (for demo re-runs).
	@test -f "$(ROLLUPS_DB)" || { echo "no rollups db at $(ROLLUPS_DB)"; exit 1; }
	sqlite3 "$(ROLLUPS_DB)" "UPDATE ai_telemetry_v_0_0_2 SET delivery_status='pending', shipped_at=NULL, retry_count=0;"
	@echo "→ all rollup rows reset to pending; 'make ship' will re-deliver"

ship-status:  ## Print shipper cursor state (pending / in_flight / delivered / retry / poison) and exit.
	@test -f "$(ROLLUPS_DB)" \
	    || { echo "no rollups db at $(ROLLUPS_DB) — run 'make embellish' first"; exit 1; }
	cargo build --release --quiet -p noodle-shipper
	./target/release/noodle-shipper --db "$(ROLLUPS_DB)" --endpoint "$(OTLP_ENDPOINT)" --status

# ── Demo prerequisites (host tooling) ──────────────────────────────

doctor:  ## Check every prerequisite the demos/ scripts assume. Exits non-zero on any missing tool.
	@ok=1; \
	check_cmd() { \
	    if command -v "$$1" >/dev/null 2>&1; then \
	        printf "  \033[32m✓\033[0m %-12s %s\n" "$$1" "$$(eval "$$2" 2>/dev/null | head -1)"; \
	    else \
	        printf "  \033[31m✗\033[0m %-12s %s\n" "$$1" "$$3"; ok=0; \
	    fi; \
	}; \
	echo "Host tools:"; \
	check_cmd cargo   "cargo --version"   "rust toolchain — install via https://rustup.rs"; \
	check_cmd rustup  "rustup --version"  "rustup — install via https://rustup.rs"; \
	check_cmd claude  "claude --version"  "Claude Code CLI — install via 'npm i -g @anthropic-ai/claude-code'"; \
	check_cmd jq      "jq --version"      "jq — 'brew install jq'"; \
	check_cmd sqlite3 "sqlite3 --version" "sqlite3 — 'brew install sqlite' (macOS ships it but old)"; \
	check_cmd python3 "python3 --version" "python3 — 'brew install python'"; \
	echo "Rust targets:"; \
	if rustup target list --installed | grep -q '^wasm32-unknown-unknown$$'; then \
	    printf "  \033[32m✓\033[0m %-22s %s\n" "wasm32-unknown-unknown" "installed"; \
	else \
	    printf "  \033[31m✗\033[0m %-22s %s\n" "wasm32-unknown-unknown" "missing — 'make tooling' or 'rustup target add wasm32-unknown-unknown'"; ok=0; \
	fi; \
	[ $$ok -eq 1 ] || { echo; echo "Some tools are missing — run 'make tooling' to install what's installable."; exit 1; }
	@echo "Claude auth: 'claude' uses its own credential store (Keychain / enterprise login). If you can run 'claude -p hi' in a fresh shell, you're set."

tooling:  ## Install/upgrade the demo prerequisites that have a one-line install path.
	@command -v brew >/dev/null || { echo "brew is required to install jq/sqlite3/python3 on macOS"; exit 1; }
	@for pkg in jq sqlite python3; do \
	    if brew list "$$pkg" >/dev/null 2>&1; then \
	        echo "  ✓ $$pkg (already installed)"; \
	    else \
	        echo "  → brew install $$pkg"; brew install "$$pkg"; \
	    fi; \
	done
	@if rustup target list --installed | grep -q '^wasm32-unknown-unknown$$'; then \
	    echo "  ✓ rustup target wasm32-unknown-unknown (already installed)"; \
	else \
	    echo "  → rustup target add wasm32-unknown-unknown"; rustup target add wasm32-unknown-unknown; \
	fi
	@if command -v claude >/dev/null 2>&1; then \
	    echo "  ✓ claude (already installed)"; \
	else \
	    echo "  → claude not installed. Install with: npm i -g @anthropic-ai/claude-code"; \
	fi
	@echo "Done. Re-run 'make doctor' to verify."

viewer-build:  ## Build the noodle-native viewer's React app (npm install + vite build).
	@command -v npm >/dev/null || { echo "npm not installed"; exit 1; }
	cd $(VIEWER_WEB) && npm install --no-audit --no-fund && npm run build
	cargo build --release -p noodle-viewer

viewer:  ## Run the noodle-native viewer (browser at http://localhost:9092).
	@test -f "$(TAP_FILE)" \
	    || { echo "no tap log at $(TAP_FILE) — start the proxy first ('make run-release &')"; exit 1; }
	@if [ ! -f "$(VIEWER_WEB)/dist/index.html" ]; then \
	    echo "(UI not built — placeholder page will load. Run 'make viewer-build' for the real UI.)"; \
	  fi
	./target/release/noodle-viewer

viewer-dev:  ## Run the viewer's vite dev server (hot reload). Backend must be running separately.
	@command -v npm >/dev/null || { echo "npm not installed"; exit 1; }
	cd $(VIEWER_WEB) && npm install --no-audit --no-fund && npm run dev

mitm-smoke:  ## Smoke-test the MITM path: curl api.anthropic.com via the proxy. Requires ANTHROPIC_API_KEY env.
	@test -n "$$ANTHROPIC_API_KEY" \
	    || { echo "ANTHROPIC_API_KEY not set; export it before running this target"; exit 1; }
	@test -f "$$HOME/.config/noodle/ca/ca.pem" \
	    || { echo "no CA at $$HOME/.config/noodle/ca/ca.pem — start the proxy first ('make run-release &')"; exit 1; }
	curl -sS -i -x http://127.0.0.1:62100 --cacert "$$HOME/.config/noodle/ca/ca.pem" \
	    -X POST https://api.anthropic.com/v1/messages \
	    -H "x-api-key: $$ANTHROPIC_API_KEY" \
	    -H 'anthropic-version: 2023-06-01' \
	    -H 'content-type: application/json' \
	    -d '{"model":"claude-haiku-4-5","max_tokens":40,"messages":[{"role":"user","content":"say hi in one short sentence"}]}' \
	    --max-time 30

# ── macOS transparent mode (Story 011) ─────────────────────────────

# Apple Developer team ID used by Xcode codesigning. Override on the
# command line if you're not Joe. Documented in
# docs/guides/macos-transparent-mode.md.
NOODLE_TPROXY_DEVELOPMENT_TEAM ?= KRU5V3NCWA
export NOODLE_TPROXY_DEVELOPMENT_TEAM

MACOS_APP_DIR := apps/noodle-macos

macos-tooling:  ## Install xcodegen + just via Homebrew (one-time).
	@command -v brew >/dev/null || { echo "Homebrew not installed — see https://brew.sh"; exit 1; }
	brew install xcodegen just

macos-doctor:  ## Verify prerequisites for macOS transparent mode.
	@printf "Tooling:\n"
	@command -v xcodegen >/dev/null && echo "  ✓ xcodegen" || { echo "  ✗ xcodegen — run: make macos-tooling"; exit 1; }
	@command -v just >/dev/null && echo "  ✓ just" || { echo "  ✗ just — run: make macos-tooling"; exit 1; }
	@command -v xcodebuild >/dev/null && echo "  ✓ xcodebuild" || { echo "  ✗ xcodebuild — install Xcode from the App Store"; exit 1; }
	@printf "\nTeam ID: %s\n" "$$NOODLE_TPROXY_DEVELOPMENT_TEAM"
	@printf "\nCodesigning identities on this machine:\n"
	@security find-identity -p codesigning -v 2>/dev/null | grep -E 'Apple Development|Developer ID Application' || \
	    { echo "  ✗ no Apple Development cert found — sign into Xcode → Settings → Accounts"; exit 1; }

macos-staticlib:  ## Build the Rust staticlib the sysext links against.
	cargo build --release -p noodle-macos-tproxy

macos-build: macos-staticlib  ## Build the Xcode app + sysext (dev mode).
	cd $(MACOS_APP_DIR) && ./scripts/build_noodle_app_with_signing.sh

macos-test:  ## Run Swift unit tests (UninstallService) + Rust tests for noodle-macos-tproxy.
	cd $(MACOS_APP_DIR) && xcodegen generate --spec Project.yml >/dev/null
	@echo "── Swift XCTest (NoodleTests) ─────────────────────────────────────"
	@cd $(MACOS_APP_DIR) && xcodebuild test \
	    -project Noodle.xcodeproj \
	    -scheme NoodleTests \
	    -destination 'platform=macOS,arch=arm64' \
	    -derivedDataPath .xcode-derived/noodle-tests \
	    -allowProvisioningUpdates 2>&1 \
	    | grep -E "Test Case|Test Suite|TEST SUCCEEDED|TEST FAILED|Executed [0-9]+ test" \
	    || { echo "xcodebuild test failed — re-run without filter:  cd $(MACOS_APP_DIR) && xcodebuild test -scheme NoodleTests -destination 'platform=macOS,arch=arm64'"; exit 1; }
	@echo
	@echo "── Rust cargo test (noodle-macos-tproxy) ──────────────────────────"
	cargo test -p noodle-macos-tproxy

macos-open-xcode:  ## Generate + open the Xcode project (first-time setup or sign-in fixes).
	cd $(MACOS_APP_DIR) && xcodegen generate --spec Project.yml
	open $(MACOS_APP_DIR)/Noodle.xcodeproj

macos-install: macos-build  ## Build, install, and launch the dev-mode app + sysext.
	cd $(MACOS_APP_DIR) && ./scripts/install_noodle_app_bundle.sh dev \
	    ./.xcode-derived/noodle-app-dev/Build/Products/Debug/Noodle.app 0
	@echo
	@echo "Installed at /Applications/Noodle.app"
	@echo "Approve the system extension in:"
	@echo "  System Settings → General → Login Items & Extensions → Network Extensions"
	@echo
	@echo "Then: make macos-list   (expect [activated enabled])"

macos-install-reset: macos-build  ## Install + recreate the saved NETransparentProxyManager profile.
	cd $(MACOS_APP_DIR) && ./scripts/install_noodle_app_bundle.sh dev \
	    ./.xcode-derived/noodle-app-dev/Build/Products/Debug/Noodle.app 1

macos-list:  ## Show the live system-extension state for noodle.
	@systemextensionsctl list | grep 'com\.noodleproxy\.macos' || echo '(no noodle sysext registered)'

# Path the sysext writes its root CA cert PEM to — matches
# crates/noodle-macos-tproxy/src/tls.rs::CA_PEM_PATH. World-
# readable; not under $HOME because the sysext runs as root and
# would write to /var/root/ (mode 0700, unreadable by Joe).
NOODLE_MACOS_CA_PATH := /Library/Application Support/noodle/macos-tproxy-ca.pem

macos-ca-path:  ## Print the path to the sysext's root CA cert PEM.
	@echo "$(NOODLE_MACOS_CA_PATH)"

macos-env-export:  ## Run launchctl setenv for the standard CA-bundle env vars (Node, Python, OpenSSL, curl, AWS).
	@test -f "$(NOODLE_MACOS_CA_PATH)" \
	    || { echo "CA not found at $(NOODLE_MACOS_CA_PATH) — install + run the sysext first"; exit 1; }
	@for v in NODE_EXTRA_CA_CERTS REQUESTS_CA_BUNDLE SSL_CERT_FILE CURL_CA_BUNDLE AWS_CA_BUNDLE; do \
	    launchctl setenv $$v "$(NOODLE_MACOS_CA_PATH)" && echo "  ✓ $$v"; \
	done
	@echo
	@echo "Set for this launchd user session. Already-running processes"
	@echo "do not inherit — relaunch Claude Code, Cursor, your terminal,"
	@echo "etc. to pick up the new trust. Lasts until logout."

macos-logs:  ## Stream live logs from the noodle sysext + macOS NE daemons.
	log stream --info --debug \
	    --predicate 'process == "com.noodleproxy.macos.dev.provider" OR process == "neagent" OR process == "nesessionmanager" OR process == "sysextd"'

macos-logs-recent:  ## Last 5 minutes of logs from the noodle sysext + NE daemons.
	log show --last 5m --style compact --info --debug \
	    --predicate 'process == "com.noodleproxy.macos.dev.provider" OR process == "neagent" OR process == "nesessionmanager" OR process == "sysextd"'

macos-uninstall:  ## Show how to uninstall — uninstall is in-app only (Noodle menu → "Uninstall Noodle…").
	@echo
	@echo "Uninstall is in-app. From the menu bar (🦙 tproxy demo):"
	@echo "    Noodle → Uninstall Noodle…"
	@echo
	@echo "That flow:"
	@echo "  1. Stops the proxy."
	@echo "  2. Removes the saved NETransparentProxyManager profile"
	@echo "     (the row in System Settings → Network → VPN & Filters)."
	@echo "  3. Clears the MITM root CA from the keychain."
	@echo "  4. Clears the app's UserDefaults."
	@echo "  5. Submits OSSystemExtensionRequest.deactivationRequest —"
	@echo "     the only API path that can deactivate a sysext while"
	@echo "     SIP is enabled (it must be called by the container app"
	@echo "     that activated the sysext)."
	@echo "  6. Moves /Applications/Noodle.app to the Trash and quits."
	@echo
	@echo "If the app won't launch and you need a hard-reset, the only"
	@echo "supported path is: reboot, then 'rm -rf /Applications/Noodle.app'."

macos-clean:  ## Remove the Xcode build cache + generated .xcodeproj for the macOS app.
	rm -rf $(MACOS_APP_DIR)/.xcode-derived $(MACOS_APP_DIR)/Noodle.xcodeproj

# ── House-keeping ──────────────────────────────────────────────────

clean:  ## cargo clean.
	cargo clean
