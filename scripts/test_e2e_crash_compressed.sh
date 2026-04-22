#!/bin/bash
# Phase N8.2 — OrioleDB-on-Neon crash-recovery for COMPRESSED tables (R9).
#
# Compressed OrioleDB pages use `ORIOLEDB_COMP_BLCKSZ` granularity
# instead of 8 KB. Plan E's FPI emit path takes a different branch
# (io.c:1684+ else-clause that first writes an `OrioleDBOndiskPageHeader`
# then the compressed chunks), and `read_page_from_disk` reads the
# header separately at offset 0 before the decompress step. Without
# explicit coverage, this branch isn't exercised by the crash gate.
#
# Test shape matches test_e2e_crash_mid_ckpt.sh (SIGKILL mid-CHECKPOINT)
# but the table is created `WITH (compress = 5)` so dirty pages are
# written compressed and the read-back path goes through the
# compressed branch in io.c.
#
# Exit codes
#   0 — compressed table survives SIGKILL mid-CHECKPOINT with identical
#       md5 pre- and post-restart.
#   non-zero — compressed-page branch has a correctness gap.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

# WSL2 dev env: neon_local's HTTP health probe of compute_ctl on
# 127.0.0.1 can get hijacked by a shell-level HTTP proxy (e.g. Clash)
# even though no_proxy mentions 127.*. Drop proxy vars so all localhost
# HTTP traffic in this test goes direct. No effect in CI.
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy

ROWS="${ROWS:-500}"
ENDPOINT_NAME="${ENDPOINT_NAME:-main}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-90}"
COMPRESS_LEVEL="${COMPRESS_LEVEL:-5}"

section() { printf '\n==> %s\n' "$*"; }

dump_logs() {
    echo ""
    echo "---- .neon/ log dump (last 300 lines per file) ----"
    if [ -d .neon ]; then
        find .neon -name '*.log' -print 2>/dev/null | while read -r f; do
            echo ""; echo "### $f"; tail -300 "$f" 2>/dev/null || true
        done
    fi
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
        if pg_isready -h 127.0.0.1 -p "$port" -U "$PSQL_USER" -d "$PSQL_DB" \
             >/dev/null 2>&1; then
            echo "  compute accepting SQL on port $port"
            return 0
        fi
        sleep 1
    done
    echo "FAIL: compute not ready within ${READY_TIMEOUT}s" >&2
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

section "[2/10] init + start + tenant + endpoint"
cargo neon init
# WSL2 infra workaround: Windows host may squat on 127.0.0.1:7676 (e.g. Clash).
# Only rewrite the safekeeper http_port when the default is actually occupied.
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

COMPUTE_PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$ENDPOINT_NAME" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
COMPUTE_PORT="${COMPUTE_PORT:-55432}"
wait_for_psql "$COMPUTE_PORT"

run_psql() {
    psql -p "$COMPUTE_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -v ON_ERROR_STOP=1 -Atq "$@"
}

section "[3/10] Create COMPRESSED OrioleDB table + baseline $ROWS rows"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS crash_compress;
CREATE TABLE crash_compress (
    id    int primary key,
    name  text,
    value numeric,
    pad   text
) USING orioledb WITH (compress = $COMPRESS_LEVEL);
INSERT INTO crash_compress
SELECT g, 'clean_' || g, (g * 2.3)::numeric, repeat('x', 200)
FROM generate_series(1, $ROWS) g;
CHECKPOINT;
SQL

section "[4/10] Push $ROWS more rows past the baseline checkpoint"
run_psql <<SQL
INSERT INTO crash_compress
SELECT $ROWS + g, 'dirty_' || g, (g * 7.11)::numeric, repeat('y', 200)
FROM generate_series(1, $ROWS) g;
SQL

BEFORE="$(run_psql -c "SELECT count(*) FROM crash_compress")"
BEFORE_SUM="$(run_psql -c \
    "SELECT md5(string_agg(id::text || name || value::text || pad, ',' ORDER BY id))
       FROM crash_compress")"
echo "before-crash: count=$BEFORE sum=$BEFORE_SUM"
EXPECT=$(( 2 * ROWS ))
[ "$BEFORE" = "$EXPECT" ] || { echo "FAIL: expected $EXPECT pre-crash, got $BEFORE" >&2; exit 1; }

section "[5/10] Race CHECKPOINT with SIGKILL on the compute"
PID=$(compute_pid)
echo "compute pid: $PID"
(
    psql -p "$COMPUTE_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -c "CHECKPOINT" >/dev/null 2>&1 || true
) &
CKPT_PID=$!
sleep 0.1
kill -9 "$PID" 2>/dev/null || { echo "FAIL: pid $PID gone" >&2; exit 1; }
wait "$CKPT_PID" 2>/dev/null || true
echo "  compute pid $PID SIGKILLed mid-CHECKPOINT (compressed table)"

cargo neon endpoint stop "$ENDPOINT_NAME" >/dev/null 2>&1 || true
sleep 2

PGDATA_DIR=".neon/endpoints/$ENDPOINT_NAME/pgdata"
if [ -d "$PGDATA_DIR" ]; then
    echo "  wiping $PGDATA_DIR to force fresh basebackup"
    rm -rf "$PGDATA_DIR"
fi

section "[6/10] Stateless restart after crash"
cargo neon endpoint start "$ENDPOINT_NAME" || {
    echo "FAIL: endpoint start returned non-zero after crash" >&2; exit 1; }
wait_for_psql "$COMPUTE_PORT"

section "[7/10] Reconnect + verify compressed data round-tripped"
AFTER="$(run_psql -c "SELECT count(*) FROM crash_compress")"
AFTER_SUM="$(run_psql -c \
    "SELECT md5(string_agg(id::text || name || value::text || pad, ',' ORDER BY id))
       FROM crash_compress")"
echo "after-crash:  count=$AFTER  sum=$AFTER_SUM"

section "[8/10] Invariant: compressed-page branch must reconstruct pre-crash state"
if [ "$AFTER" != "$BEFORE" ]; then
    echo "FAIL: compressed table row count changed across crash+restart ($BEFORE -> $AFTER)" >&2
    echo "      io.c compressed FPI branch (1684+) has a correctness gap." >&2
    exit 1
fi
if [ "$AFTER_SUM" != "$BEFORE_SUM" ]; then
    echo "FAIL: compressed table checksum changed across crash+restart" >&2
    echo "      before: $BEFORE_SUM" >&2
    echo "      after:  $AFTER_SUM" >&2
    exit 1
fi

section "[9/10] PASS — compressed OrioleDB table survives mid-CHECKPOINT crash"
section "[10/10] compress=$COMPRESS_LEVEL, rows=$EXPECT, md5 preserved"
