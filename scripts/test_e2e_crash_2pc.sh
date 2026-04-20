#!/bin/bash
# Phase N8.3 — OrioleDB-on-Neon 2PC prepared-transaction crash recovery.
#
# Distinct from test_e2e_crash_mid_ckpt.sh because the commit path for
# `PREPARE TRANSACTION` / `COMMIT PREPARED` goes through a different
# call site than a normal txn's `current_oxid_commit` (undo.c:2680
# rather than undo.c:2219). M1.2 / M1.3 commit-barriers cover both
# sites — N8.3 proves that.
#
# Test shape
#   1. Fresh .neon tenant; CREATE EXTENSION orioledb; populate a
#      table with $ROWS rows; CHECKPOINT.
#   2. `BEGIN; INSERT $ROWS more rows; PREPARE TRANSACTION 'n8_3_gid';`
#      — the prepared txn is now on disk via 2PC state file + WAL.
#   3. Checkpoint is NOT run between step 2 and step 3 — we want the
#      prepared state to live in WAL + compute memory only.
#   4. SIGKILL the compute. The prepared GID is in SafeKeeper WAL; the
#      500 row inserts' visibility hinges on M1.3 having persisted the
#      xidmap CSN and M1.2 having persisted the undo range.
#   5. `cargo neon endpoint start` — stateless restart.
#   6. `SELECT gid FROM pg_prepared_xacts` — the prepared txn should
#      still be listed. `COMMIT PREPARED 'n8_3_gid';` → should succeed.
#   7. `SELECT count(*)` → must equal 2 * $ROWS.
#
# Exit codes
#   0 — prepared-txn survived + committed post-restart, counts match.
#   non-zero — mismatch, infrastructure error, or prepared txn lost.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

ROWS="${ROWS:-100}"
ENDPOINT_NAME="${ENDPOINT_NAME:-main}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-90}"
GID="n8_3_gid"

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

# Enable 2PC by ensuring max_prepared_transactions > 0. neon_local's
# default spec has it at 10 already but we assert here in case of
# future defaults change.
PREPARED_LIMIT="$(run_psql -c "SHOW max_prepared_transactions")"
if [ "$PREPARED_LIMIT" = "0" ]; then
    echo "FAIL: max_prepared_transactions=0, this scenario needs >0" >&2
    exit 1
fi
echo "max_prepared_transactions=$PREPARED_LIMIT"

section "[4/10] Create OrioleDB table + baseline $ROWS rows + CHECKPOINT"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS crash_2pc;
CREATE TABLE crash_2pc (id int primary key, name text) USING orioledb;
INSERT INTO crash_2pc SELECT g, 'clean_' || g FROM generate_series(1, $ROWS) g;
CHECKPOINT;
SQL

BEFORE="$(run_psql -c "SELECT count(*) FROM crash_2pc")"
if [ "$BEFORE" != "$ROWS" ]; then
    echo "FAIL: baseline expected $ROWS, got $BEFORE" >&2
    exit 1
fi
echo "baseline: count=$BEFORE"

section "[5/10] Prepare transaction with additional $ROWS rows"
run_psql <<SQL
BEGIN;
INSERT INTO crash_2pc SELECT $ROWS + g, 'dirty_' || g FROM generate_series(1, $ROWS) g;
PREPARE TRANSACTION '$GID';
SQL

PREPARED_COUNT="$(run_psql -c "SELECT count(*) FROM pg_prepared_xacts WHERE gid = '$GID'")"
if [ "$PREPARED_COUNT" != "1" ]; then
    echo "FAIL: expected 1 prepared txn, got $PREPARED_COUNT" >&2
    exit 1
fi

# The prepared-txn rows are NOT visible yet to other sessions —
# confirm baseline count is still $ROWS (the rows live in WAL +
# compute memory until COMMIT PREPARED).
VIS="$(run_psql -c "SELECT count(*) FROM crash_2pc")"
if [ "$VIS" != "$ROWS" ]; then
    echo "FAIL: prepared-txn rows leaked to visible state; expected $ROWS got $VIS" >&2
    exit 1
fi
echo "prepared-txn present, invisible as expected"

section "[6/10] SIGKILL compute before COMMIT PREPARED"
PID=$(compute_pid)
echo "compute pid: $PID"
kill -9 "$PID" 2>/dev/null || {
    echo "FAIL: compute pid $PID already gone" >&2
    exit 1
}
echo "  compute pid $PID SIGKILLed with prepared-txn '$GID' outstanding"

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

section "[8/10] Verify prepared txn survived"
SURVIVED="$(run_psql -c "SELECT gid FROM pg_prepared_xacts WHERE gid = '$GID'")"
if [ "$SURVIVED" != "$GID" ]; then
    echo "FAIL: prepared txn '$GID' did not survive crash+restart; found: '$SURVIVED'" >&2
    exit 1
fi
echo "prepared txn '$GID' survived"

section "[9/10] COMMIT PREPARED + assert final count"
run_psql -c "COMMIT PREPARED '$GID';"

FINAL="$(run_psql -c "SELECT count(*) FROM crash_2pc")"
EXPECT=$(( 2 * ROWS ))
if [ "$FINAL" != "$EXPECT" ]; then
    echo "FAIL: post-COMMIT-PREPARED expected $EXPECT rows, got $FINAL" >&2
    exit 1
fi

section "[10/10] PASS — 2PC prepared-txn survives compute SIGKILL"
echo "  baseline=$ROWS rows, prepared+committed=$ROWS more rows, total=$FINAL"
