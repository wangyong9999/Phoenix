#!/bin/bash
# Phase N3-follow-up — OrioleDB-on-Neon DDL + crash recovery.
#
# Distinct from N8.1-4 because the state under test isn't user data,
# it's sys-tree entries (O_TABLES, O_INDICES, SHARED_ROOT_INFO).
# Sys-tree writes go via apply_btree_modify_record / o_btree_modify
# which emits ORIOLEDB_XLOG_CONTAINER records (recovery/wal.c:980).
# On post-crash restart, those records must be re-dispatched by
# orioledb_redo to rebuild the sys-tree state.
#
# If the 6.6.4c-3 CONTAINER-replay reliability question also affects
# this path, this scenario will surface it: the second table created
# after the checkpoint won't be visible post-crash.
#
# Test shape
#   1. CREATE TABLE t1 + rows + CHECKPOINT
#   2. CREATE TABLE t2 + rows (no CHECKPOINT after)
#   3. SIGKILL, wipe pgdata, restart
#   4. Post-restart: both t1 and t2 exist; both have their rows.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

# WSL2 dev env: neon_local's HTTP health probe of compute_ctl on
# 127.0.0.1 can get hijacked by a shell-level HTTP proxy (e.g. Clash)
# even though no_proxy mentions 127.*. Drop proxy vars so all localhost
# HTTP traffic in this test goes direct. No effect in CI.
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy

ROWS="${ROWS:-50}"
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
    echo "FAIL: compute not ready within ${READY_TIMEOUT}s" >&2
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

PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$ENDPOINT_NAME" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
PORT="${PORT:-55432}"
wait_for_psql "$PORT"

run_psql() {
    psql -p "$PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -v ON_ERROR_STOP=1 -Atq "$@"
}

section "[2/10] CREATE TABLE t1 + $ROWS rows + CHECKPOINT"
run_psql <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS ddl_crash_t1;
DROP TABLE IF EXISTS ddl_crash_t2;
CREATE TABLE ddl_crash_t1 (id int primary key, payload text) USING orioledb;
INSERT INTO ddl_crash_t1 SELECT g, 't1_' || g FROM generate_series(1, $ROWS) g;
CHECKPOINT;
SQL

BEFORE_T1="$(run_psql -c "SELECT count(*) FROM ddl_crash_t1")"
[ "$BEFORE_T1" = "$ROWS" ] || { echo "FAIL: baseline t1 count $BEFORE_T1 != $ROWS" >&2; exit 1; }

section "[3/10] CREATE TABLE t2 + $ROWS rows (no CHECKPOINT after)"
run_psql <<SQL
CREATE TABLE ddl_crash_t2 (id int primary key, payload text) USING orioledb;
INSERT INTO ddl_crash_t2 SELECT g, 't2_' || g FROM generate_series(1, $ROWS) g;
SQL

BEFORE_T2="$(run_psql -c "SELECT count(*) FROM ddl_crash_t2")"
[ "$BEFORE_T2" = "$ROWS" ] || { echo "FAIL: baseline t2 count $BEFORE_T2 != $ROWS" >&2; exit 1; }
echo "pre-crash: t1=$BEFORE_T1 rows, t2=$BEFORE_T2 rows"

section "[4/10] SIGKILL compute (post-chkp t2 creation still only in WAL + shmem)"
PID=$(compute_pid)
echo "compute pid: $PID"
kill -9 "$PID" 2>/dev/null || { echo "FAIL: pid $PID gone" >&2; exit 1; }

cargo neon endpoint stop "$ENDPOINT_NAME" >/dev/null 2>&1 || true
sleep 2

PGDATA_DIR=".neon/endpoints/$ENDPOINT_NAME/pgdata"
if [ -d "$PGDATA_DIR" ]; then
    echo "  wiping $PGDATA_DIR (stateless restart)"
    rm -rf "$PGDATA_DIR"
fi

section "[5/10] Stateless restart"
cargo neon endpoint start "$ENDPOINT_NAME" || {
    echo "FAIL: endpoint start returned non-zero after crash" >&2; exit 1; }
wait_for_psql "$PORT"

section "[6/10] Verify t1 (pre-chkp) survived"
T1_EXISTS="$(run_psql -c "SELECT count(*) FROM pg_class WHERE relname='ddl_crash_t1'")"
if [ "$T1_EXISTS" != "1" ]; then
    echo "FAIL: ddl_crash_t1 missing after restart (pre-CHECKPOINT DDL lost!)" >&2
    exit 1
fi
AFTER_T1="$(run_psql -c "SELECT count(*) FROM ddl_crash_t1")"
if [ "$AFTER_T1" != "$BEFORE_T1" ]; then
    echo "FAIL: t1 row count changed $BEFORE_T1 -> $AFTER_T1" >&2
    exit 1
fi
echo "t1 survived: $AFTER_T1 rows"

section "[7/10] Verify t2 (post-chkp) survived — the acid test"
T2_EXISTS="$(run_psql -c "SELECT count(*) FROM pg_class WHERE relname='ddl_crash_t2'")"
if [ "$T2_EXISTS" != "1" ]; then
    echo "FAIL: ddl_crash_t2 missing after restart (post-CHECKPOINT DDL lost)" >&2
    echo "      CONTAINER-replay of O_TABLES / O_INDICES didn't run." >&2
    exit 1
fi
AFTER_T2="$(run_psql -c "SELECT count(*) FROM ddl_crash_t2")"
if [ "$AFTER_T2" != "$BEFORE_T2" ]; then
    echo "FAIL: t2 row count changed $BEFORE_T2 -> $AFTER_T2" >&2
    exit 1
fi
echo "t2 survived: $AFTER_T2 rows"

section "[8/10] Sanity: can still INSERT and query post-restart"
run_psql -c "INSERT INTO ddl_crash_t2 VALUES (10000, 'post_restart');" >/dev/null
POST="$(run_psql -c "SELECT count(*) FROM ddl_crash_t2")"
[ "$POST" = "$(( ROWS + 1 ))" ] || { echo "FAIL: post-restart INSERT didn't take, got $POST" >&2; exit 1; }

section "[9/10] PASS — DDL across CHECKPOINT survives SIGKILL"
section "[10/10] t1 ($ROWS), t2 ($ROWS + 1 post-restart insert); sys-tree CONTAINER replay works"
