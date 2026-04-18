#!/bin/bash
# Phase 6.8.1 — OrioleDB-on-Neon physical replication verification.
#
# Hypothesis. After Phase 6.6, a Neon Replica-mode compute attached to
# the same timeline should see OrioleDB table state as of the
# safekeeper's tip, maintained live through WAL streaming — no
# replication-specific OrioleDB code beyond what rmid=129 already
# provides end-to-end.
#
# Test shape
#   1. Fresh tenant; primary compute writes 1000 rows into an OrioleDB
#      table and commits. CHECKPOINT.
#   2. Start a second endpoint in Replica mode on the same timeline.
#   3. Replica must see the 1000 rows after a short settle.
#   4. Primary inserts 500 more rows (no CHECKPOINT — forces the
#      replica to apply row-level WAL, not checkpoint FPIs).
#   5. Replica must converge to 1500 rows within a timeout.
#
# Exit codes
#   0  — replica converges to primary state via streaming WAL.
#   77 — skipped because neon_local cannot create a Replica endpoint
#        on a writable timeline in this build.
#   other — replica divergence or infrastructure error.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

PRIMARY_ENDPOINT="${PRIMARY_ENDPOINT:-primary}"
REPLICA_ENDPOINT="${REPLICA_ENDPOINT:-replica}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-90}"
REPL_SETTLE_TIMEOUT="${REPL_SETTLE_TIMEOUT:-60}"

section() { printf '\n==> %s\n' "$*"; }
dump_logs() { find .neon -name '*.log' 2>/dev/null | xargs -I {} sh -c 'echo "### {}"; tail -300 {}'; }
cleanup() { local rc=$?; [ "$rc" -ne 0 ] && [ "$rc" -ne 77 ] && dump_logs; cargo neon stop >/dev/null 2>&1 || true; return "$rc"; }
trap cleanup EXIT

wait_for_psql() {
    local port="$1" deadline=$(( SECONDS + READY_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if pg_isready -h 127.0.0.1 -p "$port" -U "$PSQL_USER" -d "$PSQL_DB" >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    echo "FAIL: compute not ready on 127.0.0.1:$port" >&2; return 1
}

# Wait for replica to converge on an expected row count, or fail.
wait_for_replica() {
    local port="$1" expected="$2"
    local deadline=$(( SECONDS + REPL_SETTLE_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        local got
        got=$(psql -p "$port" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" -Atq \
                   -c "SELECT count(*) FROM physrepl_verify" 2>/dev/null || echo "?")
        if [ "$got" = "$expected" ]; then
            echo "  replica converged to $expected rows"
            return 0
        fi
        sleep 1
    done
    echo "FAIL: replica did not converge to $expected rows within ${REPL_SETTLE_TIMEOUT}s" >&2
    return 1
}

if ! cargo neon endpoint create --help 2>/dev/null | grep -qi 'replica'; then
    echo "SKIP: neon_local endpoint create cannot request Replica mode — Phase 6.8.1 scaffold pending" >&2
    exit 77
fi

section "[1/7] Reset .neon state"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon

section "[2/7] Init + tenant + primary"
cargo neon init
cargo neon start
sleep 3
cargo neon tenant create --set-default
cargo neon endpoint create "$PRIMARY_ENDPOINT"
cargo neon endpoint start  "$PRIMARY_ENDPOINT"
PRIMARY_PORT="$(cargo neon endpoint list 2>/dev/null | awk -v n="$PRIMARY_ENDPOINT" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' | head -1)"
PRIMARY_PORT="${PRIMARY_PORT:-55432}"
wait_for_psql "$PRIMARY_PORT"

psql_primary() {
    psql -p "$PRIMARY_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" -v ON_ERROR_STOP=1 -Atq "$@"
}

section "[3/7] Primary: create OrioleDB table + 1000 rows + CHECKPOINT"
psql_primary <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS physrepl_verify;
CREATE TABLE physrepl_verify (id int primary key, payload text) USING orioledb;
INSERT INTO physrepl_verify SELECT g, 'row_' || g FROM generate_series(1, 1000) g;
CHECKPOINT;
SQL

section "[4/7] Start Replica endpoint on the same timeline"
cargo neon endpoint create "$REPLICA_ENDPOINT" --hot-standby true
cargo neon endpoint start  "$REPLICA_ENDPOINT"
REPLICA_PORT="$(cargo neon endpoint list 2>/dev/null | awk -v n="$REPLICA_ENDPOINT" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' | head -1)"
wait_for_psql "$REPLICA_PORT"

section "[5/7] Replica must see 1000 rows (post-CHECKPOINT baseline)"
wait_for_replica "$REPLICA_PORT" 1000

section "[6/7] Primary: add 500 rows WITHOUT checkpoint; replica must converge via streaming"
psql_primary <<SQL
INSERT INTO physrepl_verify SELECT 1000 + g, 'stream_' || g FROM generate_series(1, 500) g;
SQL

wait_for_replica "$REPLICA_PORT" 1500

section "[7/7] PASS — OrioleDB physical replication converges through both FPI and row-level WAL"
echo "  primary rows:  1500"
echo "  replica rows:  1500"
