#!/bin/bash
# Phase 7.3 — OrioleDB-on-Neon concurrent-backend E2E.
#
# Two backends concurrently INSERT disjoint id ranges into the same
# OrioleDB table. After both finish, CHECKPOINT + stateless restart,
# then assert both ranges survived with matching md5s.
#
# This pins a concurrency invariant that neither test_e2e.sh nor
# test_e2e_crud.sh exercises: the OBuffers-backed undo log must
# serialise writes from multiple backends and emit a coherent Plan B
# WAL stream even when ordering between the two streams is
# non-deterministic from either backend's perspective.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

ROWS_PER_BACKEND="${ROWS_PER_BACKEND:-1500}"
ENDPOINT_NAME="${ENDPOINT_NAME:-main}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-120}"

section() { printf '\n==> %s\n' "$*"; }
dump_logs() { find .neon -name '*.log' 2>/dev/null | xargs -I {} sh -c 'echo "### {}"; tail -300 {}'; }
cleanup() { local rc=$?; [ "$rc" -ne 0 ] && dump_logs; cargo neon stop >/dev/null 2>&1 || true; return "$rc"; }
trap cleanup EXIT

wait_for_psql() {
    local port="$1" deadline=$(( SECONDS + READY_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if pg_isready -h 127.0.0.1 -p "$port" -U "$PSQL_USER" -d "$PSQL_DB" >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    echo "FAIL: compute not ready on 127.0.0.1:$port" >&2; return 1
}

section "[1/7] Reset .neon + init"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon
cargo neon init
cargo neon start
sleep 3
cargo neon tenant create --set-default
cargo neon endpoint create "$ENDPOINT_NAME"
cargo neon endpoint start  "$ENDPOINT_NAME"

PORT="$(cargo neon endpoint list 2>/dev/null | awk -v n="$ENDPOINT_NAME" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' | head -1)"
PORT="${PORT:-55432}"
wait_for_psql "$PORT"

run_psql() { psql -p "$PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" -v ON_ERROR_STOP=1 -Atq "$@"; }

section "[2/7] Create table"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS concurrent_verify;
CREATE TABLE concurrent_verify (
    id int primary key,
    origin text
) USING orioledb;
SQL

section "[3/7] Launch two backends writing disjoint id ranges in parallel"
# Backend A: ids [1 .. ROWS_PER_BACKEND]
# Backend B: ids [10_000_000 .. 10_000_000 + ROWS_PER_BACKEND)
(
    run_psql <<SQL &
INSERT INTO concurrent_verify
SELECT g, 'A_' || g FROM generate_series(1, $ROWS_PER_BACKEND) g;
SQL
    A_PID=$!
    run_psql <<SQL &
INSERT INTO concurrent_verify
SELECT 10000000 + g, 'B_' || g FROM generate_series(1, $ROWS_PER_BACKEND) g;
SQL
    B_PID=$!
    wait "$A_PID" "$B_PID"
)

section "[4/7] CHECKPOINT + verify pre-restart count"
run_psql -c "CHECKPOINT"
EXPECT_COUNT=$(( 2 * ROWS_PER_BACKEND ))
BEFORE_COUNT=$(run_psql -c "SELECT count(*) FROM concurrent_verify")
BEFORE_SUM_A=$(run_psql -c "SELECT md5(string_agg(id::text||origin, ',' ORDER BY id)) FROM concurrent_verify WHERE origin LIKE 'A\\_%' ESCAPE '\\'")
BEFORE_SUM_B=$(run_psql -c "SELECT md5(string_agg(id::text||origin, ',' ORDER BY id)) FROM concurrent_verify WHERE origin LIKE 'B\\_%' ESCAPE '\\'")
echo "before: count=$BEFORE_COUNT  A=$BEFORE_SUM_A  B=$BEFORE_SUM_B"
if [ "$BEFORE_COUNT" != "$EXPECT_COUNT" ]; then
    echo "FAIL: expected $EXPECT_COUNT rows, got $BEFORE_COUNT" >&2
    exit 1
fi

section "[5/7] Stateless restart"
cargo neon endpoint stop "$ENDPOINT_NAME"
sleep 2
cargo neon endpoint start "$ENDPOINT_NAME"
wait_for_psql "$PORT"

section "[6/7] Verify both backends' writes survived independently"
AFTER_COUNT=$(run_psql -c "SELECT count(*) FROM concurrent_verify")
AFTER_SUM_A=$(run_psql -c "SELECT md5(string_agg(id::text||origin, ',' ORDER BY id)) FROM concurrent_verify WHERE origin LIKE 'A\\_%' ESCAPE '\\'")
AFTER_SUM_B=$(run_psql -c "SELECT md5(string_agg(id::text||origin, ',' ORDER BY id)) FROM concurrent_verify WHERE origin LIKE 'B\\_%' ESCAPE '\\'")
echo "after:  count=$AFTER_COUNT  A=$AFTER_SUM_A  B=$AFTER_SUM_B"

if [ "$AFTER_COUNT" != "$BEFORE_COUNT" ]; then
    echo "FAIL: row count changed across restart" >&2; exit 1
fi
if [ "$AFTER_SUM_A" != "$BEFORE_SUM_A" ]; then
    echo "FAIL: backend A's write range diverged" >&2; exit 1
fi
if [ "$AFTER_SUM_B" != "$BEFORE_SUM_B" ]; then
    echo "FAIL: backend B's write range diverged" >&2; exit 1
fi

section "[7/7] PASS — concurrent-backend OrioleDB writes round-trip"
echo "  total rows:    $AFTER_COUNT"
echo "  A checksum:    $AFTER_SUM_A"
echo "  B checksum:    $AFTER_SUM_B"
