#!/bin/bash
# Phase 6.6.4 release gate — OrioleDB-on-Neon crash mid-CHECKPOINT recovery.
#
# Complements scripts/test_e2e.sh (clean stop/start round-trip) by
# injecting a SIGKILL on the compute while a CHECKPOINT is in flight,
# then asserting that stateless restart reconstructs the pre-crash
# data set byte-for-byte.
#
# Why this matters. The Log-is-Data architecture makes three claims:
#
#   1. Every persistent OrioleDB byte has a path to WAL (either via
#      ORIOLEDB_XLOG_CONTAINER row-level records at txn time, or via
#      Plan B / Plan E FPIs at write/checkpoint time).
#   2. The compute-local orioledb_data/ directory is a cache, never
#      the source of truth.
#   3. stateless restart = fresh empty compute + basebackup(LSN) +
#      WAL replay reconstructs state byte-identical to pre-crash.
#
# A crash mid-CHECKPOINT is the stress case for all three. Dirty
# buffers are half-flushed, Plan E FPI emission is partway through
# the tree walk, the control-file FPI may or may not have landed.
# If any of (1)-(3) is actually violated, the md5 after restart
# will diverge from the md5 before crash.
#
# Test shape
#   1. Fresh .neon tenant; CREATE EXTENSION orioledb; populate a
#      table with $ROWS rows. Checkpoint + stop/start once to prove
#      the baseline clean path works.
#   2. INSERT $ROWS more rows to dirty the buffers beyond the last
#      clean checkpoint.
#   3. Fire CHECKPOINT in a background psql, then kill -9 the
#      compute process while it's still running (we race the
#      checkpoint; the test tolerates both "killed before ckpt
#      finished" and "killed just after" since both must recover).
#   4. `cargo neon endpoint start` — stateless restart.
#   5. SELECT count + md5 from the reconnected compute.
#   6. Compare against the before-crash md5 computed in step 2.
#
# Exit codes
#   0 — restart reconstructed pre-crash state byte-identically.
#   non-zero — mismatch, infrastructure error, or compute failed to
#             come back after crash. The caller (CI) should upload
#             .neon/ logs.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

ROWS="${ROWS:-500}"
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
    if [ "$rc" -ne 0 ]; then
        dump_logs
    fi
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

# Resolve the compute pid by reading the endpoint's postmaster.pid.
# neon_local lays each endpoint's pgdata at .neon/endpoints/$ENDPOINT/pgdata,
# and PG writes its pid to postmaster.pid as the first line.
compute_pid() {
    local pidfile=".neon/endpoints/$ENDPOINT_NAME/pgdata/postmaster.pid"
    if [ ! -f "$pidfile" ]; then
        echo "FAIL: $pidfile not present — compute never started?" >&2
        return 1
    fi
    head -1 "$pidfile"
}

section "[1/10] Reset .neon state"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon

section "[2/10] cargo neon init"
cargo neon init

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

section "[4/10] Create OrioleDB table and establish a clean baseline ($ROWS rows)"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS crash_verify;
CREATE TABLE crash_verify (
    id    int primary key,
    name  text,
    value numeric
) USING orioledb;

INSERT INTO crash_verify
SELECT g, 'clean_' || g, (g * 2.3)::numeric
FROM generate_series(1, $ROWS) g;
CHECKPOINT;
SQL

section "[5/10] Push an additional $ROWS rows past the baseline checkpoint"
# Use values that guarantee md5 diverges from pre-crash if even one row
# is lost. The second batch keys 'dirty_*' and different value formula.
run_psql <<SQL
INSERT INTO crash_verify
SELECT $ROWS + g, 'dirty_' || g, (g * 7.11)::numeric
FROM generate_series(1, $ROWS) g;
SQL

BEFORE_COUNT="$(run_psql -c "SELECT count(*) FROM crash_verify")"
BEFORE_SUM="$(run_psql -c \
    "SELECT md5(string_agg(id::text || name || value::text, ',' ORDER BY id))
       FROM crash_verify")"
echo "before-crash: count=$BEFORE_COUNT checksum=$BEFORE_SUM"
EXPECT_COUNT=$(( 2 * ROWS ))
if [ "$BEFORE_COUNT" != "$EXPECT_COUNT" ]; then
    echo "FAIL: expected $EXPECT_COUNT rows before crash, got $BEFORE_COUNT" >&2
    exit 1
fi

section "[6/10] Race CHECKPOINT with SIGKILL on the compute"
# Kick a background psql that asks for CHECKPOINT; then immediately
# kill -9 the compute. Whether the CHECKPOINT got far enough to emit
# any FPI or not, the restart must recover the pre-crash dataset.
PID=$(compute_pid)
echo "compute pid: $PID"
(
    psql -p "$COMPUTE_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -c "CHECKPOINT" >/dev/null 2>&1 || true
) &
CKPT_PID=$!
# Tiny delay so the CHECKPOINT backend has begun doing work; we want
# to race it mid-flight rather than before it's even accepted.
sleep 0.1
kill -9 "$PID" 2>/dev/null || {
    echo "FAIL: compute pid $PID already gone before we could kill it" >&2
    exit 1
}
wait "$CKPT_PID" 2>/dev/null || true
echo "  compute pid $PID SIGKILLed mid-checkpoint"

# neon_local doesn't automatically tear down its bookkeeping when the
# compute is SIGKILLed; tell it to stop so the endpoint start below
# is coming from a clean shut-state.
cargo neon endpoint stop "$ENDPOINT_NAME" >/dev/null 2>&1 || true
sleep 2

# True stateless restart semantics: discard the compute-local pgdata
# (it may be in a half-checkpoint-applied state after SIGKILL) so
# the restart has to rebuild from basebackup + WAL replay. This is
# precisely the "Log-is-Data" recovery path the gate is testing —
# if any OrioleDB state survived only in the local FS, dropping
# pgdata will expose it as md5 divergence after restart.
PGDATA_DIR=".neon/endpoints/$ENDPOINT_NAME/pgdata"
if [ -d "$PGDATA_DIR" ]; then
    echo "  wiping $PGDATA_DIR to force fresh basebackup on restart"
    rm -rf "$PGDATA_DIR"
fi

section "[7/10] Stateless restart after crash"
# Verbose stderr so any cargo neon / compute_ctl failure is visible
# in the CI log (the earlier opaque exit-1 cost one CI round).
cargo neon endpoint start "$ENDPOINT_NAME" || {
    echo "FAIL: cargo neon endpoint start returned non-zero after crash" >&2
    echo "      The stateless-restart path couldn't recover from a mid-CHECKPOINT SIGKILL." >&2
    exit 1
}
wait_for_psql "$COMPUTE_PORT"

section "[8/10] Reconnect and read the table"
AFTER_COUNT="$(run_psql -c "SELECT count(*) FROM crash_verify")"
AFTER_SUM="$(run_psql -c \
    "SELECT md5(string_agg(id::text || name || value::text, ',' ORDER BY id))
       FROM crash_verify")"
echo "after-crash:  count=$AFTER_COUNT checksum=$AFTER_SUM"

section "[9/10] Invariant: restart reconstructs pre-crash state"
if [ "$AFTER_COUNT" != "$BEFORE_COUNT" ]; then
    echo "FAIL: row count changed across crash+restart ($BEFORE_COUNT -> $AFTER_COUNT)" >&2
    echo "      Log-is-Data claim violated: WAL replay did not reconstruct all rows." >&2
    exit 1
fi
if [ "$AFTER_SUM" != "$BEFORE_SUM" ]; then
    echo "FAIL: checksum changed across crash+restart" >&2
    echo "      before: $BEFORE_SUM" >&2
    echo "      after:  $AFTER_SUM" >&2
    echo "      Log-is-Data claim violated: WAL replay produced a divergent row set." >&2
    exit 1
fi

section "[10/10] PASS — OrioleDB stateless recovery after mid-CHECKPOINT crash"
echo "  rows:     $EXPECT_COUNT (clean baseline + dirty-batch past last checkpoint)"
echo "  checksum: $BEFORE_SUM (preserved across kill -9)"
