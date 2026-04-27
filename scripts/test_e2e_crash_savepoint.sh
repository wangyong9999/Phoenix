#!/bin/bash
# Phase N8.4 — OrioleDB-on-Neon SAVEPOINT + ROLLBACK TO + crash recovery.
#
# OrioleDB subtransactions use autonomous nesting (undoStackLocations
# per nesting level) and apply rollback via undo chains rather than
# distinct subxact IDs. Before this test, we had zero regression
# coverage for:
#   (a) a subxact's rows being correctly rolled back before COMMIT,
#   (b) the parent-txn's remaining rows surviving a crash after
#       the rollback, and
#   (c) post-crash visibility consistency — no rolled-back rows leak,
#       no committed rows lost.
#
# Test shape
#   1. Fresh .neon tenant; CREATE EXTENSION orioledb; populate a
#      table with $ROWS rows; CHECKPOINT.
#   2. BEGIN; SAVEPOINT s1; INSERT $ROWS "dirty" rows; ROLLBACK TO s1;
#      INSERT 1 sentinel row; COMMIT;
#   3. SIGKILL during a subsequent CHECKPOINT (or after; the scenario
#      under test is "ROLLBACK + COMMIT then crash").
#   4. Restart; count must be $ROWS + 1 and no "dirty_*" names visible.
#
# Exit codes
#   0 — rollback-then-crash preserved parent-txn commit and dropped
#       subxact inserts.
#   non-zero — mismatch.

set -euo pipefail

# macOS: drop orphan PG SHM segments from previous SIGKILLed runs.
# Source the helper if present (no-op on Linux).
if [ -f "$(dirname "$0")/_macos_shm_cleanup.sh" ]; then
    . "$(dirname "$0")/_macos_shm_cleanup.sh"
    shm_cleanup_macos
fi

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

# WSL2 dev env: neon_local's HTTP health probe of compute_ctl on
# 127.0.0.1 can get hijacked by a shell-level HTTP proxy (e.g. Clash)
# even though no_proxy mentions 127.*. Drop proxy vars so all localhost
# HTTP traffic in this test goes direct. No effect in CI.
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy

ROWS="${ROWS:-100}"
ENDPOINT_NAME="${ENDPOINT_NAME:-main}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-90}"

section() { printf '\n==> %s\n' "$*"; }

dump_logs() {
    echo ""
    echo "---- .neon/ log dump (last 300 lines per file) ----"
    if [ -d .neon ]; then
        find .neon -name '*.log' -print 2>/dev/null | while read -r f; do
            echo ""
            echo "### $f"
            tail -300 "$f" 2>/dev/null || true
        done
    else
        echo "(.neon/ not present)"
    fi
}

cleanup() {
    local rc=$?
    if [ "$rc" -ne 0 ]; then dump_logs; fi
    cargo neon stop >/dev/null 2>&1 || true
    return "$rc"
}
trap cleanup EXIT

wait_for_psql() {
    local port="$1" deadline=$(( SECONDS + READY_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if pg_isready -h 127.0.0.1 -p "$port" -U "$PSQL_USER" -d "$PSQL_DB" \
             >/dev/null 2>&1; then
            echo "  compute accepting SQL on port $port"
            return 0
        fi
        sleep 1
    done
    echo "FAIL: compute not ready on port $port within ${READY_TIMEOUT}s" >&2
    return 1
}

compute_pid() {
    local pidfile=".neon/endpoints/$ENDPOINT_NAME/pgdata/postmaster.pid"
    [ -f "$pidfile" ] || { echo "FAIL: $pidfile missing" >&2; return 1; }
    head -1 "$pidfile"
}

section "[1/10] Reset .neon state"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon

section "[2/10] cargo neon init"
cargo neon init
# WSL2 infra workaround: Windows host may squat on 127.0.0.1:7676 (e.g. Clash).
# Only rewrite the safekeeper http_port when the default is actually occupied.
if (exec 3<>/dev/tcp/127.0.0.1/7676) 2>/dev/null; then
    exec 3>&- 3<&-
    echo "note: port 7676 busy on host — rewriting safekeeper http_port to 17676"
    sed -i 's/http_port = 7676/http_port = 17676/' .neon/config
fi

section "[3/10] cargo neon start + create tenant + endpoint"
cargo neon start
sleep 3
cargo neon tenant create --set-default
cargo neon endpoint create "$ENDPOINT_NAME"
cargo neon endpoint start  "$ENDPOINT_NAME"

COMPUTE_PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$ENDPOINT_NAME" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
COMPUTE_PORT="${COMPUTE_PORT:-55432}"
echo "compute port: $COMPUTE_PORT"
wait_for_psql "$COMPUTE_PORT"

run_psql() {
    psql -p "$COMPUTE_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -v ON_ERROR_STOP=1 -Atq "$@"
}

section "[4/10] Create OrioleDB table + baseline $ROWS rows + CHECKPOINT"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS crash_sp;
CREATE TABLE crash_sp (id int primary key, name text) USING orioledb;
INSERT INTO crash_sp SELECT g, 'clean_' || g FROM generate_series(1, $ROWS) g;
CHECKPOINT;
SQL

BEFORE="$(run_psql -c "SELECT count(*) FROM crash_sp")"
if [ "$BEFORE" != "$ROWS" ]; then
    echo "FAIL: baseline expected $ROWS, got $BEFORE" >&2
    exit 1
fi

section "[5/10] Subtxn: INSERT then ROLLBACK TO; then sentinel + COMMIT"
run_psql <<SQL
BEGIN;
SAVEPOINT s1;
INSERT INTO crash_sp SELECT $ROWS + g, 'dirty_' || g FROM generate_series(1, $ROWS) g;
ROLLBACK TO SAVEPOINT s1;
INSERT INTO crash_sp VALUES ($(( ROWS * 2 + 1 )), 'sentinel');
COMMIT;
SQL

EXPECT=$(( ROWS + 1 ))
CONFIRM="$(run_psql -c "SELECT count(*) FROM crash_sp")"
if [ "$CONFIRM" != "$EXPECT" ]; then
    echo "FAIL: pre-crash expected $EXPECT, got $CONFIRM" >&2
    exit 1
fi
DIRTY_LEAK="$(run_psql -c "SELECT count(*) FROM crash_sp WHERE name LIKE 'dirty_%'")"
if [ "$DIRTY_LEAK" != "0" ]; then
    echo "FAIL: $DIRTY_LEAK rolled-back rows leaked pre-crash" >&2
    exit 1
fi
echo "pre-crash: count=$CONFIRM, no dirty rows visible"

section "[6/10] Race CHECKPOINT with SIGKILL on the compute"
PID=$(compute_pid)
echo "compute pid: $PID"
(
    psql -p "$COMPUTE_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -c "CHECKPOINT" >/dev/null 2>&1 || true
) &
CKPT_PID=$!
sleep 0.1
kill -9 "$PID" 2>/dev/null || {
    echo "FAIL: compute pid $PID already gone" >&2
    exit 1
}
wait "$CKPT_PID" 2>/dev/null || true
echo "  compute pid $PID SIGKILLed mid-checkpoint"

cargo neon endpoint stop "$ENDPOINT_NAME" >/dev/null 2>&1 || true
sleep 2

PGDATA_DIR=".neon/endpoints/$ENDPOINT_NAME/pgdata"
if [ -d "$PGDATA_DIR" ]; then
    echo "  wiping $PGDATA_DIR to force stateless restart"
    rm -rf "$PGDATA_DIR"
fi

section "[7/10] Stateless restart after crash"
cargo neon endpoint start "$ENDPOINT_NAME" || {
    echo "FAIL: cargo neon endpoint start returned non-zero after crash" >&2
    exit 1
}
wait_for_psql "$COMPUTE_PORT"

section "[8/10] Verify rolled-back rows are gone, sentinel present"
AFTER_COUNT="$(run_psql -c "SELECT count(*) FROM crash_sp")"
AFTER_DIRTY="$(run_psql -c "SELECT count(*) FROM crash_sp WHERE name LIKE 'dirty_%'")"
AFTER_SENTINEL="$(run_psql -c "SELECT count(*) FROM crash_sp WHERE name = 'sentinel'")"
echo "after-crash: count=$AFTER_COUNT dirty=$AFTER_DIRTY sentinel=$AFTER_SENTINEL"

if [ "$AFTER_COUNT" != "$EXPECT" ]; then
    echo "FAIL: post-crash expected $EXPECT rows, got $AFTER_COUNT" >&2
    exit 1
fi
if [ "$AFTER_DIRTY" != "0" ]; then
    echo "FAIL: $AFTER_DIRTY rolled-back rows leaked across crash+restart" >&2
    echo "      Log-is-Data violation: subtxn undo chain did not survive" >&2
    exit 1
fi
if [ "$AFTER_SENTINEL" != "1" ]; then
    echo "FAIL: sentinel row (parent-txn COMMIT) missing post-crash" >&2
    exit 1
fi

section "[9/10] Invariant: rollback-then-crash preserves parent-txn commit"
section "[10/10] PASS — SAVEPOINT + ROLLBACK TO survives compute SIGKILL"
echo "  baseline=$ROWS rows, rolled-back=$ROWS ignored, sentinel kept, total=$AFTER_COUNT"
