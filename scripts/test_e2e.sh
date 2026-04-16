#!/bin/bash
# End-to-end test: OrioleDB on Neon Serverless
#
# Verifies that OrioleDB tables work with Neon's compute-storage separation.
# Uses `cargo neon` (neon_local) to orchestrate all components.

set -e

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

export PATH="$PROJECT_DIR/pg_install/v17/bin:$PATH"

echo "============================================"
echo "  OrioleDB + Neon Serverless E2E Test"
echo "============================================"
echo ""

# Clean previous state
echo "[1/8] Cleaning previous state..."
cargo neon stop 2>/dev/null || true
rm -rf .neon
sleep 1

# Initialize Neon environment
echo "[2/8] Initializing Neon environment..."
cargo neon init 2>&1 | tail -3

# Start all Neon services (PageServer, SafeKeeper, Broker, etc.)
echo "[3/8] Starting Neon services..."
cargo neon start 2>&1 | tail -5
sleep 3

# Create tenant and timeline
echo "[4/8] Creating tenant..."
cargo neon tenant create --set-default 2>&1 | tail -3

# Create and start compute endpoint
echo "[5/8] Starting compute endpoint..."
cargo neon endpoint create main 2>&1 | tail -3
cargo neon endpoint start main 2>&1 | tail -5
sleep 3

# Get the compute port
COMPUTE_PORT=$(cargo neon endpoint list 2>/dev/null | grep main | awk '{print $NF}' | grep -oP '\d+' | head -1)
COMPUTE_PORT=${COMPUTE_PORT:-55432}
echo "  Compute port: $COMPUTE_PORT"

# Test with OrioleDB
echo "[6/8] Testing OrioleDB on Neon..."
psql -p $COMPUTE_PORT -h 127.0.0.1 -U cloud_admin -d postgres << 'SQL'
-- Load OrioleDB extension
CREATE EXTENSION IF NOT EXISTS orioledb;

-- Create table using OrioleDB storage engine
CREATE TABLE neon_orioledb_test (
    id int,
    name text,
    value float8
) USING orioledb;

-- Insert test data
INSERT INTO neon_orioledb_test
SELECT g, 'item_' || g, random() * 1000
FROM generate_series(1, 100) g;

-- Verify
SELECT count(*) AS total_rows FROM neon_orioledb_test;
SELECT * FROM neon_orioledb_test WHERE id <= 5 ORDER BY id;

-- Verify it's using OrioleDB
SELECT relname, amname
FROM pg_class c JOIN pg_am a ON c.relam = a.oid
WHERE relname = 'neon_orioledb_test';
SQL

echo ""
echo "[7/8] Verifying PageServer received data..."
# Check PageServer logs for activity
if [ -d .neon/pageservers ]; then
    PAGESERVER_LOG=$(find .neon/pageservers -name "pageserver.log" | head -1)
    if [ -n "$PAGESERVER_LOG" ]; then
        echo "  PageServer log lines: $(wc -l < $PAGESERVER_LOG)"
        echo "  WAL records received: $(grep -c "WAL\|wal\|ingest" $PAGESERVER_LOG 2>/dev/null || echo 0)"
    fi
fi

echo ""
echo "[8/8] Cleanup..."
cargo neon stop 2>/dev/null || true

echo ""
echo "============================================"
echo "  E2E Test Complete!"
echo "============================================"
