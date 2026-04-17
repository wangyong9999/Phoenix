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

section() { printf '\n==> %s\n' "$*"; }

cleanup() {
    cargo neon stop 2>/dev/null || true
}
trap cleanup EXIT

section "[1/9] Reset .neon state"
cargo neon stop 2>/dev/null || true
rm -rf .neon

section "[2/9] cargo neon init"
cargo neon init 2>&1 | tail -3

section "[3/9] cargo neon start"
cargo neon start 2>&1 | tail -5

section "[4/9] Create tenant + endpoint"
cargo neon tenant create --set-default 2>&1 | tail -3
cargo neon endpoint create "$ENDPOINT_NAME" 2>&1 | tail -3
cargo neon endpoint start  "$ENDPOINT_NAME" 2>&1 | tail -5

# Resolve the compute port from neon_local output rather than assuming
# a default — neon_local can change its allocation strategy.
COMPUTE_PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$ENDPOINT_NAME" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
COMPUTE_PORT="${COMPUTE_PORT:-55432}"
echo "compute port: $COMPUTE_PORT"

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
cargo neon endpoint stop "$ENDPOINT_NAME" 2>&1 | tail -3
# Sleep briefly to let SafeKeeper flush and endpoint teardown settle.
sleep 2
cargo neon endpoint start "$ENDPOINT_NAME" 2>&1 | tail -5

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
