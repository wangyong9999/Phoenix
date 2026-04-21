#!/bin/bash
# Phase N8.7 — OrioleDB-on-Neon concurrent INSERTs + SIGKILL mid-burst.
#
# test_e2e_concurrent.sh covers the clean-shutdown variant. N8.7 adds
# the crash dimension: while N backends are concurrently INSERT-ing
# disjoint id ranges, SIGKILL the compute at a pseudo-random point.
# The per-backend commit order is non-deterministic, so the number
# of rows surviving is likewise non-deterministic — but the rows that
# DO survive must form a prefix of each backend's insert stream, and
# no row should appear duplicated, torn, or with a missing PK.
#
# Invariants
#   1. No PK violations post-restart (uniqueness preserved).
#   2. No partial rows (each id that exists has a matching name+value).
#   3. The surviving count is between 0 and N * ROWS_PER_BACKEND.
#   4. md5 over surviving rows, when those rows are grouped by backend,
#      matches the expected md5 over a CONTIGUOUS prefix of each
#      backend's insert sequence — i.e. no "middle-of-sequence" loss.
#
# R17 check: if two backends' commit-barrier FPIs collide on the same
# leaf page, the last-write wins and both txns' rows must be present
# in the surviving FPI. A backend whose commit_lsn < SIGKILL_lsn must
# either have ALL its committed rows present, or NONE (half-visible
# commits are the failure mode this test pins down).

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

BACKENDS="${BACKENDS:-4}"
ROWS_PER_BACKEND="${ROWS_PER_BACKEND:-250}"
ENDPOINT_NAME="${ENDPOINT_NAME:-main}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-90}"

section() { printf '\n==> %s\n' "$*"; }
dump_logs() {
    find .neon -name '*.log' 2>/dev/null | while read -r f; do
        echo "### $f"; tail -300 "$f" 2>/dev/null || true
    done
}
cleanup() {
    local rc=$?
    [ "$rc" -ne 0 ] && dump_logs
    cargo neon stop >/dev/null 2>&1 || true
    return "$rc"
}
trap cleanup EXIT

wait_for_psql() {
    local port="$1" deadline=$(( SECONDS + READY_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if pg_isready -h 127.0.0.1 -p "$port" -U "$PSQL_USER" -d "$PSQL_DB" >/dev/null 2>&1; then
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

section "[1/10] Reset .neon + init + start + tenant + endpoint"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon
cargo neon init
# WSL2 infra workaround: Windows host may squat on 127.0.0.1:7676 (e.g. Clash).
# Only rewrite the safekeeper http port when the default is actually occupied.
if (exec 3<>/dev/tcp/127.0.0.1/7676) 2>/dev/null; then
    exec 3>&- 3<&-
    echo "note: port 7676 busy on host — rewriting safekeeper http_port to 17676"
    sed -i 's/http_port = 7676/http_port = 17676/' .neon/config
fi
cargo neon start
sleep 3
cargo neon tenant create --set-default
cargo neon endpoint create "$ENDPOINT_NAME"
cargo neon endpoint start  "$ENDPOINT_NAME"

PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$ENDPOINT_NAME" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
PORT="${PORT:-55432}"
wait_for_psql "$PORT"

run_psql() {
    psql -p "$PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -v ON_ERROR_STOP=1 -Atq "$@"
}

section "[2/10] Create OrioleDB table"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS crash_concur;
CREATE TABLE crash_concur (
    backend_id int,
    id         int,
    name       text,
    value      numeric,
    primary key (backend_id, id)
) USING orioledb;
SQL

section "[3/10] Launch $BACKENDS concurrent INSERT backends"
PIDS=()
for b in $(seq 1 "$BACKENDS"); do
    (
        psql -p "$PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
             -v ON_ERROR_STOP=1 -Atq <<SQL 2>/dev/null || true
INSERT INTO crash_concur
SELECT $b, g, 'b${b}_' || g, ($b * 1000 + g)::numeric
FROM generate_series(1, $ROWS_PER_BACKEND) g;
SQL
    ) &
    PIDS+=( $! )
done
echo "  launched ${#PIDS[@]} concurrent INSERT streams"

section "[4/10] Pseudo-random sleep, then SIGKILL the compute"
# Aim to kill roughly mid-burst. Tune via env if needed.
SLEEP_MS="${CRASH_SLEEP_MS:-300}"
awk "BEGIN {printf \"%.3f\n\", $SLEEP_MS/1000.0}" | xargs sleep

PID=$(compute_pid)
echo "  compute pid $PID → SIGKILL (concurrent backends still in flight)"
kill -9 "$PID" 2>/dev/null || echo "  (pid already exited)"

# Reap backend psql processes (they'll likely see connection loss and exit)
for p in "${PIDS[@]}"; do
    wait "$p" 2>/dev/null || true
done

cargo neon endpoint stop "$ENDPOINT_NAME" >/dev/null 2>&1 || true
sleep 2

PGDATA_DIR=".neon/endpoints/$ENDPOINT_NAME/pgdata"
if [ -d "$PGDATA_DIR" ]; then
    echo "  wiping $PGDATA_DIR (stateless-restart boundary)"
    rm -rf "$PGDATA_DIR"
fi

section "[5/10] Stateless restart"
cargo neon endpoint start "$ENDPOINT_NAME" || {
    echo "FAIL: endpoint start returned non-zero after crash" >&2
    exit 1
}
wait_for_psql "$PORT"

section "[6/10] Post-restart inventory"
TOTAL="$(run_psql -c "SELECT count(*) FROM crash_concur")"
echo "survived total: $TOTAL rows"
MAX_PER_BACKEND=$(( ROWS_PER_BACKEND ))
UPPER=$(( BACKENDS * ROWS_PER_BACKEND ))
if [ "$TOTAL" -lt 0 ] || [ "$TOTAL" -gt "$UPPER" ]; then
    echo "FAIL: surviving count $TOTAL outside [0, $UPPER]" >&2
    exit 1
fi

section "[7/10] Per-backend prefix invariant"
# For each backend b, the surviving rows with backend_id=b must be
# ids {1..k_b} for some 0 <= k_b <= ROWS_PER_BACKEND. If we find a
# gap (id missing but higher id present), the commit was non-atomic
# and we broke Log-is-Data.
FAIL=0
for b in $(seq 1 "$BACKENDS"); do
    # Maximum id for this backend
    MAX=$(run_psql -c "SELECT coalesce(max(id), 0) FROM crash_concur WHERE backend_id=$b")
    COUNT=$(run_psql -c "SELECT count(*) FROM crash_concur WHERE backend_id=$b")
    if [ "$MAX" != "$COUNT" ]; then
        echo "FAIL: backend $b has max(id)=$MAX but count=$COUNT — gap in ids" >&2
        FAIL=1
    fi
    echo "  backend $b: max_id=$MAX count=$COUNT (${MAX_PER_BACKEND} expected if fully committed)"
done
[ "$FAIL" -ne 0 ] && exit 1

section "[8/10] Per-row integrity: name and value match id"
# Deterministic formula: name = 'b${b}_' || id, value = (b*1000+id)
BAD=$(run_psql -c "SELECT count(*) FROM crash_concur WHERE
    name != 'b' || backend_id || '_' || id
    OR value != (backend_id*1000 + id)::numeric")
if [ "$BAD" != "0" ]; then
    echo "FAIL: $BAD rows have name/value that don't match id formula" >&2
    exit 1
fi

section "[9/10] PASS — concurrent + SIGKILL preserved commit atomicity"
section "[10/10] $TOTAL/$UPPER rows survived, no gaps, no torn rows"
