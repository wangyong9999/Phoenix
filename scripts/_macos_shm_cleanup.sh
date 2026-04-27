#!/bin/bash
# Helper: clean orphaned PG SysV SHM segments left behind by a SIGKILL'd
# postgres on macOS. Linux releases SHM on process exit; XNU does not, so
# repeated crash-test runs accumulate /var/lib-style ipcs slots and fail
# the next initdb with "could not create shared memory segment: No space
# left on device".
#
# Source from the test scripts at script start. No-op on Linux.

shm_cleanup_macos() {
    if [[ "$(uname)" != "Darwin" ]]; then
        return 0
    fi
    local me="${USER:-$(id -un)}"
    ipcs -m 2>/dev/null \
        | awk -v u="$me" '/^m / && $5 == u {print $2}' \
        | xargs -I{} ipcrm -m {} 2>/dev/null \
        || true
}
