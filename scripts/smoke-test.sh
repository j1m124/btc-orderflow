#!/usr/bin/env bash
# End-to-end smoke test for the BTC orderflow data layer.
#
# Phases:
#   0. Configuration banner
#   1. Pre-flight: cargo check + ensure TimescaleDB container is up
#   2. Baseline DB row counts (so the delta after the run is meaningful)
#   3. Schema verification (hypertables + retention policies)
#   4. Start the server (background) + wait for /healthz
#   5. Wait for live ingest to populate `trades` + `book_snapshots`
#   6. WS round-trip tests against the gateway (via Bun script)
#   7. Final DB state + ingest stats
#   8. Summary
#
# Side effects:
#   - Starts a `cargo run -p server` process; kills it on exit (TERM, then KILL)
#   - Does NOT touch the TimescaleDB container; leaves it running so you can
#     inspect with `make db-psql` after the run
#
# Output is teed to scripts/smoke-test.log (overwritten each run). Server
# stdout/stderr → scripts/smoke-test-server.log. Read those after the run.
#
# Usage:
#   ./scripts/smoke-test.sh                  # full run
#   WS_URL=ws://other-host:8787/ws ./scripts/smoke-test.sh
#
# Exit codes:
#   0  all phases passed (or non-fatal warnings only)
#   1  pre-flight failed
#   2  server failed to boot
#   3  WS round-trip tests failed
#   4  unexpected error

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_FILE="$SCRIPT_DIR/smoke-test.log"
SERVER_LOG="$SCRIPT_DIR/smoke-test-server.log"
WS_SCRIPT="$SCRIPT_DIR/smoke-test-ws.ts"

# Defaults; allow override via env.
WS_URL="${WS_URL:-ws://127.0.0.1:8787/ws}"
HEALTHZ_URL="${HEALTHZ_URL:-http://127.0.0.1:8787/healthz}"
SYMBOL="${SYMBOL:-BTCUSDT}"

# How long to wait for live ingest to populate before running WS tests.
INGEST_WAIT_MAX_S="${INGEST_WAIT_MAX_S:-90}"
INGEST_MIN_TRADES="${INGEST_MIN_TRADES:-200}"
INGEST_MIN_BOOK_SNAPS="${INGEST_MIN_BOOK_SNAPS:-3}"

# Redirect everything from here on through tee to the log file.
# Subshell + exec so the trap below can still write to the terminal.
: > "$LOG_FILE"
exec > >(tee "$LOG_FILE") 2>&1

# --- helpers ---------------------------------------------------------------

section() {
    echo
    echo "============================================================"
    echo "==  $*"
    echo "============================================================"
}

note() { echo "[INFO] $*"; }
pass() { echo "[PASS] $*"; }
fail() { echo "[FAIL] $*"; }
warn() { echo "[WARN] $*"; }

run_psql() {
    docker exec btc_orderflow_db psql -U btc -d btc_orderflow "$@"
}

# tAc = tuples-only + aligned + commands; returns just the scalar.
psql_scalar() {
    docker exec btc_orderflow_db psql -U btc -d btc_orderflow -tAc "$1" 2>/dev/null | tr -d '[:space:]'
}

SERVER_PID=""
cleanup() {
    local rc=$?
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        note "cleanup: sending SIGTERM to server pid=$SERVER_PID"
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        for i in 1 2 3 4 5; do
            if ! kill -0 "$SERVER_PID" 2>/dev/null; then break; fi
            sleep 1
        done
        if kill -0 "$SERVER_PID" 2>/dev/null; then
            warn "cleanup: server didn't exit on TERM; SIGKILL"
            kill -KILL "$SERVER_PID" 2>/dev/null || true
        fi
    fi
    echo
    echo "Log file:    $LOG_FILE"
    echo "Server log:  $SERVER_LOG"
    echo "Final RC:    $rc"
    exit "$rc"
}
trap cleanup EXIT INT TERM

# --- 0. configuration banner ----------------------------------------------

section "0. Configuration"
echo "Repo root:          $REPO_ROOT"
echo "Log file:           $LOG_FILE"
echo "Server log:         $SERVER_LOG"
echo "WS URL:             $WS_URL"
echo "Healthz URL:        $HEALTHZ_URL"
echo "Symbol:             $SYMBOL"
echo "Ingest wait max:    ${INGEST_WAIT_MAX_S}s"
echo "Ingest min trades:  $INGEST_MIN_TRADES"
echo "Ingest min books:   $INGEST_MIN_BOOK_SNAPS"
echo "Start time:         $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "Host:               $(uname -srm)"
echo "Git HEAD:           $(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "Git status:         $(cd "$REPO_ROOT" && (git status --porcelain | head -5 | tr '\n' ' ') || echo n/a)"

cd "$REPO_ROOT"

# --- 1. pre-flight --------------------------------------------------------

section "1. Pre-flight"

note "running make check (cargo check per crate)"
if make check 2>&1 | tail -8; then
    pass "cargo check"
else
    fail "cargo check failed"
    exit 1
fi

note "checking TimescaleDB container"
if ! docker compose ps db 2>/dev/null | grep -q "healthy"; then
    note "DB not healthy; bringing it up"
    docker compose up -d db
    for i in 1 2 3 4 5 6 7 8 9 10; do
        if docker compose ps db | grep -q "healthy"; then break; fi
        sleep 2
    done
fi
if docker compose ps db | grep -q "healthy"; then
    pass "DB healthy"
else
    fail "DB still not healthy after wait"
    docker compose ps db
    exit 1
fi

# --- 2. baseline DB state -------------------------------------------------

section "2. Baseline DB state"

BASELINE_CANDLES=$(psql_scalar "SELECT COUNT(*) FROM candles;")
BASELINE_TRADES=$(psql_scalar "SELECT COUNT(*) FROM trades;")
BASELINE_BOOK=$(psql_scalar "SELECT COUNT(*) FROM book_snapshots;")
echo "candles:        $BASELINE_CANDLES"
echo "trades:         $BASELINE_TRADES"
echo "book_snapshots: $BASELINE_BOOK"

# --- 3. schema verification -----------------------------------------------

section "3. Schema verification"

note "hypertables"
run_psql -c "SELECT hypertable_name, num_chunks
             FROM timescaledb_information.hypertables
             ORDER BY hypertable_name;"

note "retention policies"
run_psql -c "SELECT hypertable_name, config->>'drop_after' AS drop_after
             FROM timescaledb_information.jobs
             WHERE proc_name = 'policy_retention'
             ORDER BY hypertable_name;"

note "primary keys / chunk intervals"
run_psql -c "SELECT hypertable_name, time_interval
             FROM timescaledb_information.dimensions
             ORDER BY hypertable_name;"

# --- 4. start server ------------------------------------------------------

section "4. Start server"

# Diagnostic: turn on debug for the binance ws/parse path so we can confirm
# whether kline/aggTrade events are arriving (the info-level baseline only
# shows depth-maintainer state). Keep the rest at info.
RUST_LOG_LEVEL="${RUST_LOG_LEVEL:-server=info,server::binance=debug}"
: > "$SERVER_LOG"
note "launching: RUST_LOG='$RUST_LOG_LEVEL' cargo run -p server > $SERVER_LOG 2>&1 &"
( cd "$REPO_ROOT" && RUST_LOG="$RUST_LOG_LEVEL" cargo run -p server > "$SERVER_LOG" 2>&1 ) &
SERVER_PID=$!
note "server pid: $SERVER_PID"

note "waiting for /healthz (up to 60s — includes cold cargo compile)"
HEALTHZ_OK=0
for i in $(seq 1 60); do
    if curl -sf "$HEALTHZ_URL" > /dev/null 2>&1; then
        pass "healthz responding after ${i}s"
        HEALTHZ_OK=1
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        fail "server died during boot"
        echo "--- server log tail ---"
        tail -40 "$SERVER_LOG"
        exit 2
    fi
    sleep 1
done
if [[ "$HEALTHZ_OK" -ne 1 ]]; then
    fail "healthz never responded within 60s"
    echo "--- server log tail ---"
    tail -40 "$SERVER_LOG"
    exit 2
fi

note "checking expected boot log markers"
for marker in \
    "db writer task started" \
    "trade writer task started" \
    "subsec aggregator task started" \
    "book maintainer task started" \
    "binance ingest task started" \
    "gateway listening"; do
    if grep -q "$marker" "$SERVER_LOG"; then
        pass "log marker: $marker"
    else
        warn "log marker missing: $marker (may appear later)"
    fi
done

# --- 5. wait for live ingest ---------------------------------------------

section "5. Wait for live ingest"

# Two readiness gates, both must trip before phase 6:
#   (a) Both Binance WS endpoints connected (`/public` for depth and `/market`
#       for kline+aggTrade). `/market` is gated on the trade REST gap-heal,
#       which can take 30+ s after a long outage — so this is usually the
#       slow gate.
#   (b) DB row counts past the configured thresholds.
# Gate (a) avoids the previous smoke-test failure mode where (b) was already
# satisfied by stale baseline data and tests started before live kline /
# aggTrade events could flow.
note "polling every 3s for /public + /market WS connect AND trades > $INGEST_MIN_TRADES AND book_snapshots > $INGEST_MIN_BOOK_SNAPS"
WAIT_START=$(date +%s)
INGEST_OK=0
while true; do
    trades_now=$(psql_scalar "SELECT COUNT(*) FROM trades;")
    book_now=$(psql_scalar "SELECT COUNT(*) FROM book_snapshots;")
    # `tracing-subscriber` interleaves ANSI escapes between `label` and
    # `="public"` (italic-on / italic-off / dim-on …), so a strict
    # `label="public"` pattern fails. Match on `connected` … `"public"`
    # instead — robust to the escape codes and still uniquely identifies
    # the connect line per label.
    public_up=$(grep -ac 'connected.*"public"' "$SERVER_LOG" 2>/dev/null || true)
    market_up=$(grep -ac 'connected.*"market"' "$SERVER_LOG" 2>/dev/null || true)
    public_up=${public_up:-0}
    market_up=${market_up:-0}
    elapsed=$(( $(date +%s) - WAIT_START ))
    echo "[t+${elapsed}s] trades=$trades_now book_snapshots=$book_now public_ws=$public_up market_ws=$market_up"
    if (( public_up > 0 && market_up > 0 )) \
       && [[ "${trades_now:-0}" -gt "$INGEST_MIN_TRADES" && "${book_now:-0}" -gt "$INGEST_MIN_BOOK_SNAPS" ]]; then
        pass "live ingest fully up after ${elapsed}s (both WS connected, DB counts above threshold)"
        INGEST_OK=1
        break
    fi
    if [[ "$elapsed" -ge "$INGEST_WAIT_MAX_S" ]]; then
        warn "readiness gates not met within ${INGEST_WAIT_MAX_S}s — proceeding anyway"
        echo "--- recent server log ---"
        tail -50 "$SERVER_LOG"
        break
    fi
    sleep 3
done

# --- 6. WS round-trip tests ----------------------------------------------

section "6. WS round-trip tests"

if ! command -v bun >/dev/null 2>&1; then
    fail "bun not on PATH — install from https://bun.sh, then re-run"
    WS_RC=127
else
    note "running: bun $WS_SCRIPT"
    WS_URL="$WS_URL" bun "$WS_SCRIPT"
    WS_RC=$?
    echo "WS test exit code: $WS_RC"
fi

# --- 7. final DB state ---------------------------------------------------

section "7. Final DB state"

FINAL_CANDLES=$(psql_scalar "SELECT COUNT(*) FROM candles;")
FINAL_TRADES=$(psql_scalar "SELECT COUNT(*) FROM trades;")
FINAL_BOOK=$(psql_scalar "SELECT COUNT(*) FROM book_snapshots;")

echo "candles:        $FINAL_CANDLES   (delta: $((FINAL_CANDLES - BASELINE_CANDLES)))"
echo "trades:         $FINAL_TRADES   (delta: $((FINAL_TRADES - BASELINE_TRADES)))"
echo "book_snapshots: $FINAL_BOOK   (delta: $((FINAL_BOOK - BASELINE_BOOK)))"

note "trade time range"
run_psql -c "SELECT MIN(ts) AS first_trade, MAX(ts) AS last_trade FROM trades;"

note "book_snapshots time range + avg depth"
run_psql -c "SELECT MIN(ts) AS first_snap, MAX(ts) AS last_snap,
                    AVG(cardinality(bid_prices))::numeric(5,1) AS avg_bid_levels,
                    AVG(cardinality(ask_prices))::numeric(5,1) AS avg_ask_levels
             FROM book_snapshots;"

note "candles per TF (last hour)"
run_psql -c "SELECT tf, COUNT(*) AS rows
             FROM candles
             WHERE open_time > NOW() - INTERVAL '1 hour'
             GROUP BY tf
             ORDER BY tf;"

note "sample latest aggTrade"
run_psql -c "SELECT ts, agg_id, price, qty, is_buyer_maker
             FROM trades ORDER BY ts DESC LIMIT 3;"

# --- 8. summary ----------------------------------------------------------

section "8. Summary"

echo "Ingest OK:        $INGEST_OK"
echo "WS test RC:       $WS_RC"
echo "Trades delta:     $((FINAL_TRADES - BASELINE_TRADES))"
echo "Books delta:      $((FINAL_BOOK - BASELINE_BOOK))"
echo "Candles delta:    $((FINAL_CANDLES - BASELINE_CANDLES))"

# Diagnostic: with RUST_LOG=server::binance=debug, ws.rs emits one debug line
# per inbound event. Counting them tells us whether Binance actually sent us
# kline/aggTrade events on this connection — the smoke run's main open
# question.
echo
echo "Inbound Binance events (after WS connect):"
# grep -c returns 1 (with stdout="0") when there are no matches; the `|| echo 0`
# would then double-print 0. Use a `|| true` and force-print the captured count.
KLINE_HITS=$(grep -c 'kline tick' "$SERVER_LOG" || true)
AGGTRADE_HITS=$(grep -c 'agg trade' "$SERVER_LOG" || true)
DEPTH_HITS=$(grep -c 'depth diff' "$SERVER_LOG" || true)
KLINE_HITS=${KLINE_HITS:-0}
AGGTRADE_HITS=${AGGTRADE_HITS:-0}
DEPTH_HITS=${DEPTH_HITS:-0}
echo "  kline tick events:   $KLINE_HITS"
echo "  agg trade events:    $AGGTRADE_HITS"
echo "  depth diff events:   $DEPTH_HITS"
if (( DEPTH_HITS > 0 && KLINE_HITS == 0 )); then
    warn "depth events flowing but zero kline events — Binance combined-stream URL is not delivering klines (or parse_event silently drops them)"
fi
if (( DEPTH_HITS > 0 && AGGTRADE_HITS == 0 )); then
    warn "depth events flowing but zero aggTrade events — same problem on the aggTrade stream"
fi
echo
echo "If something looks off, read in this order:"
echo "  1) $SERVER_LOG  — actual server logs (Binance connect, errors, etc.)"
echo "  2) $LOG_FILE    — this script's structured output"
echo

# WS test failure is the only thing that makes this script return non-zero.
if [[ "$WS_RC" -ne 0 ]]; then
    fail "smoke test failed — WS round-trip tests returned $WS_RC"
    exit 3
fi
pass "smoke test complete"
exit 0
