#!/bin/bash
# Phase 7.3 — OrioleDB-on-Neon CRUD E2E.
#
# Extends the shape of scripts/test_e2e.sh beyond INSERT-only to
# cover UPDATE, DELETE, and large-scale INSERT that forces B-tree
# SPLIT activity across checkpoint boundaries.
#
# Workload (all against a single USING orioledb table):
#   - INSERT 5000 rows (forces multiple SPLITs)
#   - UPDATE half the rows (forces undo log growth)
#   - DELETE every fifth row (forces undo + key-range rebalancing)
#   - CHECKPOINT
#   - stateless restart
#   - Verify:
#       * row count matches expected (5000 - 1000 deleted)
#       * md5 over the UPDATEd payload column matches pre-restart
#
# Exit codes
#   0 — CRUD workload round-trips through stateless restart.
#   other — divergence or infrastructure error.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

ROWS="${ROWS:-5000}"
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

section "[1/7] Reset .neon"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon

section "[2/7] Init + start + tenant + endpoint"
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

section "[3/7] Create table + INSERT $ROWS rows (forces SPLIT activity)"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS crud_verify;
CREATE TABLE crud_verify (
    id int primary key,
    payload text,
    counter int
) USING orioledb;

INSERT INTO crud_verify
SELECT g, 'initial_' || g, 0
FROM generate_series(1, $ROWS) g;
SQL

section "[4/7] UPDATE half the rows + DELETE every fifth"
run_psql <<SQL
UPDATE crud_verify SET payload = 'updated_' || id, counter = 1
 WHERE id % 2 = 0;
DELETE FROM crud_verify WHERE id % 5 = 0;
CHECKPOINT;
SQL

EXPECT_COUNT=$(( ROWS - ROWS / 5 ))
BEFORE_COUNT=$(run_psql -c "SELECT count(*) FROM crud_verify")
BEFORE_SUM=$(run_psql -c "SELECT md5(string_agg(id::text||payload||counter::text, ',' ORDER BY id)) FROM crud_verify")
echo "before:  count=$BEFORE_COUNT sum=$BEFORE_SUM"
if [ "$BEFORE_COUNT" != "$EXPECT_COUNT" ]; then
    echo "FAIL: expected $EXPECT_COUNT rows after CRUD, got $BEFORE_COUNT" >&2
    exit 1
fi

section "[5/7] Stateless restart"
cargo neon endpoint stop "$ENDPOINT_NAME"
sleep 2
cargo neon endpoint start "$ENDPOINT_NAME"
wait_for_psql "$PORT"

section "[6/7] Verify post-restart"
AFTER_COUNT=$(run_psql -c "SELECT count(*) FROM crud_verify")
AFTER_SUM=$(run_psql -c "SELECT md5(string_agg(id::text||payload||counter::text, ',' ORDER BY id)) FROM crud_verify")
echo "after:   count=$AFTER_COUNT sum=$AFTER_SUM"

if [ "$AFTER_COUNT" != "$BEFORE_COUNT" ]; then
    echo "FAIL: row count changed across restart ($BEFORE_COUNT -> $AFTER_COUNT)" >&2
    exit 1
fi
if [ "$AFTER_SUM" != "$BEFORE_SUM" ]; then
    echo "FAIL: md5 of payload+counter diverged across restart" >&2
    echo "      before: $BEFORE_SUM" >&2
    echo "      after:  $AFTER_SUM" >&2
    exit 1
fi

section "[7/7] PASS — CRUD workload round-trips through stateless restart"
echo "  rows:     $EXPECT_COUNT"
echo "  checksum: $BEFORE_SUM"
