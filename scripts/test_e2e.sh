#!/bin/bash
# End-to-end test: OrioleDB on Neon Serverless
#
# Drives the full Log-is-Data round-trip:
#   init → start → create orioledb table → insert → CHECKPOINT
#   → endpoint stop → endpoint start (triggers selective WAL replay
#                                      via .orioledb_initialized marker)
#   → reconnect → verify row count + checksum unchanged
#
# Exit codes:
#   0 — all checks pass
#   non-zero — assertion failed or infrastructure error (caller decides)

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

ROWS="${ROWS:-100}"
ENDPOINT_NAME="${ENDPOINT_NAME:-main}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-60}"

section() { printf '\n==> %s\n' "$*"; }

# Dump daemon and compute logs so that failures in CI are self-describing
# — the .neon/ artifact upload is best-effort and was empty on the first
# run, so surface everything through the job log directly.
dump_logs() {
    echo ""
    echo "---- .neon/ log dump (last 200 lines per file) ----"
    if [ -d .neon ]; then
        find .neon -name '*.log' -print 2>/dev/null | while read -r f; do
            echo ""
            echo "### $f"
            tail -200 "$f" 2>/dev/null || true
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

# Wait until a TCP port on 127.0.0.1 accepts connections, or fail.
wait_for_port() {
    local port="$1" name="${2:-port $1}" deadline=$(( SECONDS + READY_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if (exec 3<>/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then
            exec 3>&- 3<&- || true
            echo "  $name ready on 127.0.0.1:$port"
            return 0
        fi
        sleep 1
    done
    echo "FAIL: $name did not accept on 127.0.0.1:$port within ${READY_TIMEOUT}s" >&2
    return 1
}

# Poll pg_isready on the compute port until it returns 0 or we give up.
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

section "[1/9] Reset .neon state"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon

section "[2/9] cargo neon init"
cargo neon init

section "[3/9] cargo neon start"
cargo neon start
# Full storage stack (pageserver 64000, safekeeper default 5454,
# storage_broker) started in the background. Give them a moment
# before creating tenants, but rely on the tenant-create step
# itself to exercise the full readiness.
sleep 3

section "[4/9] Create tenant + endpoint"
cargo neon tenant create --set-default
cargo neon endpoint create "$ENDPOINT_NAME"
cargo neon endpoint start  "$ENDPOINT_NAME"

# Resolve the compute port from neon_local output rather than assuming
# a default — neon_local can change its allocation strategy.
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

section "[5/9] Create OrioleDB table + insert $ROWS rows"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS serverless_verify;
CREATE TABLE serverless_verify (
    id    int primary key,
    name  text,
    value numeric
) USING orioledb;

INSERT INTO serverless_verify
SELECT g, 'row_' || g, (g * 1.7)::numeric
FROM generate_series(1, $ROWS) g;
SQL

# Capture row count and a deterministic checksum. md5(string_agg(...)) over
# an ORDER BY is stable across runs as long as the rows round-trip intact.
BEFORE_COUNT="$(run_psql -c "SELECT count(*) FROM serverless_verify")"
BEFORE_SUM="$(run_psql -c \
    "SELECT md5(string_agg(id::text || name || value::text, ',' ORDER BY id))
       FROM serverless_verify")"
echo "before: count=$BEFORE_COUNT checksum=$BEFORE_SUM"

if [ "$BEFORE_COUNT" != "$ROWS" ]; then
    echo "FAIL: expected $ROWS rows, got $BEFORE_COUNT" >&2
    exit 1
fi

section "[6/9] CHECKPOINT — flush FPIs to PageServer"
run_psql -c "CHECKPOINT"

section "[7/9] Stop + start endpoint (stateless restart)"
cargo neon endpoint stop "$ENDPOINT_NAME"
# Sleep briefly to let SafeKeeper flush and endpoint teardown settle.
sleep 2
cargo neon endpoint start "$ENDPOINT_NAME"
wait_for_psql "$COMPUTE_PORT"

section "[8/9] Reconnect + verify data survived restart"
AFTER_COUNT="$(run_psql -c "SELECT count(*) FROM serverless_verify")"
AFTER_SUM="$(run_psql -c \
    "SELECT md5(string_agg(id::text || name || value::text, ',' ORDER BY id))
       FROM serverless_verify")"
echo "after:  count=$AFTER_COUNT checksum=$AFTER_SUM"

if [ "$AFTER_COUNT" != "$BEFORE_COUNT" ]; then
    echo "FAIL: row count changed across restart ($BEFORE_COUNT → $AFTER_COUNT)" >&2
    exit 1
fi
if [ "$AFTER_SUM" != "$BEFORE_SUM" ]; then
    echo "FAIL: checksum changed across restart" >&2
    exit 1
fi

section "[9/9] PASS — OrioleDB serverless round-trip verified"
echo "  rows:     $ROWS"
echo "  checksum: $BEFORE_SUM"
