#!/bin/bash
# Phase 6.7.1 — OrioleDB-on-Neon branching verification.
#
# Hypothesis under test. After Phase 6.6 (Log-is-Data completeness),
# Neon's existing timeline-branching machinery (COW of timeline +
# LSN-targeted page resolution in PageServer) should apply to OrioleDB
# tables the same way it does to PG heap tables — *without* OrioleDB
# shipping any branching-specific code. If this test passes, 6.7.1 is
# closed. If it fails, the failure mode is an input into a missed gap
# in Phase 6.6, not a reason to add a patch in 6.7.
#
# Test shape
#   1. Fresh tenant; OrioleDB table populated with $ROWS rows.
#   2. CHECKPOINT so the FPI baseline is in PageServer. Capture the
#      LSN at that moment as BRANCH_LSN.
#   3. INSERT $ROWS_DIVERGE extra rows on the parent timeline.
#   4. cargo neon timeline branch from BRANCH_LSN.
#   5. Start an endpoint on the branch. SELECT from the OrioleDB
#      table on the branch should show exactly $ROWS (the branch
#      point), not $ROWS + $ROWS_DIVERGE.
#   6. INSERT on the branch. Re-SELECT on the parent; the parent
#      must still see only its own post-branch $ROWS_DIVERGE rows,
#      not the branch's insertions.
#   7. Stop/start each endpoint stateless; both must preserve their
#      divergent content through restart.
#
# This script is a scaffold — the neon_local timeline-branch
# subcommand surface isn't stable across releases, so the cargo neon
# commands below are deliberately simple and may need adjustment
# once this is exercised against the current neon_local. Treat any
# `NotImplemented` output here as a Phase 6.7 bring-up task, not a
# product bug.
#
# Exit codes
#   0  — branching preserves per-timeline row isolation.
#   77 — skipped because neon_local lacks the required subcommand
#        (caller decides whether to treat this as pass or fail; CI
#        should treat skip as fail once the skeleton goes live).
#   other — divergence or infrastructure error.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

ROWS="${ROWS:-200}"
ROWS_DIVERGE="${ROWS_DIVERGE:-100}"
PARENT_ENDPOINT="${PARENT_ENDPOINT:-main}"
BRANCH_ENDPOINT="${BRANCH_ENDPOINT:-branch1}"
BRANCH_TIMELINE="${BRANCH_TIMELINE:-branch1}"
PSQL_DB="${PSQL_DB:-postgres}"
PSQL_USER="${PSQL_USER:-cloud_admin}"
READY_TIMEOUT="${READY_TIMEOUT:-90}"

section() { printf '\n==> %s\n' "$*"; }

dump_logs() {
    echo ""
    echo "---- .neon/ log dump (last 300 lines per file) ----"
    if [ -d .neon ]; then
        find .neon -name '*.log' | while read -r f; do
            echo "### $f"; tail -300 "$f" 2>/dev/null || true
        done
    fi
}

cleanup() {
    local rc=$?
    if [ "$rc" -ne 0 ] && [ "$rc" -ne 77 ]; then dump_logs; fi
    cargo neon stop >/dev/null 2>&1 || true
    return "$rc"
}
trap cleanup EXIT

wait_for_psql() {
    local port="$1" deadline=$(( SECONDS + READY_TIMEOUT ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if pg_isready -h 127.0.0.1 -p "$port" -U "$PSQL_USER" -d "$PSQL_DB" \
             >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    echo "FAIL: compute not ready on 127.0.0.1:$port within ${READY_TIMEOUT}s" >&2
    return 1
}

# Probe whether neon_local exposes the timeline-branch subcommand we need.
# If not, this test is a no-op until neon_local grows it.
if ! cargo neon timeline --help 2>/dev/null | grep -qi 'branch'; then
    echo "SKIP: neon_local does not expose 'timeline branch' — Phase 6.7.1 scaffold pending CLI support" >&2
    exit 77
fi

section "[1/9] Reset .neon state"
cargo neon stop >/dev/null 2>&1 || true
rm -rf .neon

section "[2/9] Init + start + create tenant + parent endpoint"
cargo neon init
cargo neon start
sleep 3
cargo neon tenant create --set-default
cargo neon endpoint create "$PARENT_ENDPOINT"
cargo neon endpoint start  "$PARENT_ENDPOINT"

PARENT_PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$PARENT_ENDPOINT" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
PARENT_PORT="${PARENT_PORT:-55432}"
wait_for_psql "$PARENT_PORT"

psql_parent() {
    psql -p "$PARENT_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -v ON_ERROR_STOP=1 -Atq "$@"
}

section "[3/9] Populate OrioleDB table with $ROWS rows, CHECKPOINT"
psql_parent <<SQL
CREATE EXTENSION IF NOT EXISTS orioledb;
DROP TABLE IF EXISTS branch_verify;
CREATE TABLE branch_verify (id int primary key, payload text) USING orioledb;
INSERT INTO branch_verify
SELECT g, 'parent_base_' || g FROM generate_series(1, $ROWS) g;
CHECKPOINT;
SQL
BRANCH_LSN="$(psql_parent -c "SELECT pg_current_wal_lsn()")"
echo "branch LSN: $BRANCH_LSN"
BASE_COUNT=$(psql_parent -c "SELECT count(*) FROM branch_verify")
BASE_SUM=$(psql_parent -c "SELECT md5(string_agg(id::text||payload, ',' ORDER BY id)) FROM branch_verify")
echo "parent base: count=$BASE_COUNT sum=$BASE_SUM"

section "[4/9] Diverge parent with $ROWS_DIVERGE more rows (post-branch on parent)"
psql_parent <<SQL
INSERT INTO branch_verify
SELECT $ROWS + g, 'parent_diverge_' || g FROM generate_series(1, $ROWS_DIVERGE) g;
CHECKPOINT;
SQL
PARENT_AFTER_COUNT=$(psql_parent -c "SELECT count(*) FROM branch_verify")
echo "parent after diverge: count=$PARENT_AFTER_COUNT"
EXPECT_PARENT=$(( ROWS + ROWS_DIVERGE ))
if [ "$PARENT_AFTER_COUNT" != "$EXPECT_PARENT" ]; then
    echo "FAIL: parent should have $EXPECT_PARENT rows, got $PARENT_AFTER_COUNT" >&2
    exit 1
fi

section "[5/9] Create timeline branch at branch LSN"
cargo neon endpoint stop "$PARENT_ENDPOINT" >/dev/null 2>&1 || true
cargo neon timeline branch --branch-name "$BRANCH_TIMELINE" --ancestor-start-lsn "$BRANCH_LSN" \
    || {
        echo "FAIL: could not create timeline branch at $BRANCH_LSN" >&2
        exit 1
    }

section "[6/9] Start endpoint on branch; verify branch sees only pre-branch rows"
cargo neon endpoint create "$BRANCH_ENDPOINT" --branch-name "$BRANCH_TIMELINE"
cargo neon endpoint start "$BRANCH_ENDPOINT"
BRANCH_PORT="$(cargo neon endpoint list 2>/dev/null \
    | awk -v n="$BRANCH_ENDPOINT" '$0 ~ n { for (i=1;i<=NF;i++) if ($i ~ /^[0-9]{4,5}$/) print $i }' \
    | head -1)"
wait_for_psql "$BRANCH_PORT"

psql_branch() {
    psql -p "$BRANCH_PORT" -h 127.0.0.1 -U "$PSQL_USER" -d "$PSQL_DB" \
         -v ON_ERROR_STOP=1 -Atq "$@"
}

BRANCH_COUNT=$(psql_branch -c "SELECT count(*) FROM branch_verify")
BRANCH_SUM=$(psql_branch -c "SELECT md5(string_agg(id::text||payload, ',' ORDER BY id)) FROM branch_verify")
echo "branch: count=$BRANCH_COUNT sum=$BRANCH_SUM"
if [ "$BRANCH_COUNT" != "$BASE_COUNT" ]; then
    echo "FAIL: branch should have $BASE_COUNT rows (pre-branch state), got $BRANCH_COUNT" >&2
    echo "      This means OrioleDB state is leaking past the branch LSN boundary." >&2
    exit 1
fi
if [ "$BRANCH_SUM" != "$BASE_SUM" ]; then
    echo "FAIL: branch checksum diverges from pre-branch parent state" >&2
    exit 1
fi

section "[7/9] Write on branch, verify parent is unaffected"
psql_branch <<SQL
INSERT INTO branch_verify
SELECT $ROWS + 10000 + g, 'branch_only_' || g FROM generate_series(1, 50) g;
SQL

cargo neon endpoint start "$PARENT_ENDPOINT" >/dev/null
wait_for_psql "$PARENT_PORT"
PARENT_FINAL_COUNT=$(psql_parent -c "SELECT count(*) FROM branch_verify")
echo "parent after branch write: count=$PARENT_FINAL_COUNT"
if [ "$PARENT_FINAL_COUNT" != "$EXPECT_PARENT" ]; then
    echo "FAIL: parent row count changed after a write on the branch ($EXPECT_PARENT -> $PARENT_FINAL_COUNT)" >&2
    echo "      Branching isolation violated: branch writes leaked back to parent." >&2
    exit 1
fi

section "[8/9] Stateless restart on branch; verify branch-only row still present"
cargo neon endpoint stop "$BRANCH_ENDPOINT"
sleep 2
cargo neon endpoint start "$BRANCH_ENDPOINT"
wait_for_psql "$BRANCH_PORT"
BRANCH_FINAL_COUNT=$(psql_branch -c "SELECT count(*) FROM branch_verify")
EXPECT_BRANCH=$(( BASE_COUNT + 50 ))
if [ "$BRANCH_FINAL_COUNT" != "$EXPECT_BRANCH" ]; then
    echo "FAIL: branch lost its own writes across restart ($EXPECT_BRANCH -> $BRANCH_FINAL_COUNT)" >&2
    exit 1
fi

section "[9/9] PASS — OrioleDB branching preserves per-timeline isolation through restart"
echo "  parent rows:  $PARENT_FINAL_COUNT"
echo "  branch rows:  $BRANCH_FINAL_COUNT"
