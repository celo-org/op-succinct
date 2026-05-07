#!/bin/sh
# fetch-game-logs.sh -- tar matching log files for the given game indexes to
# stdout. Intended to be run via kubectl exec inside the game-monitor pod and
# piped into tar on the local machine.
#
# Usage: fetch-game-logs.sh GAME_INDEX [GAME_INDEX ...]
#
# Example:
#   kubectl exec succinct-game-monitor-0 -- ./fetch-game-logs.sh 25933 25934 \
#       | tar xf - -C ./fetched-logs
#
# Override LOGS_DIR (default /logs) when running outside the pod.

set -eu

LOGS_DIR=${LOGS_DIR:-/logs}

if [ $# -eq 0 ]; then
    echo "fetch-game-logs: missing required GAME_INDEX argument(s)" >&2
    echo "usage: fetch-game-logs.sh GAME_INDEX [GAME_INDEX ...]" >&2
    exit 2
fi

if [ ! -d "$LOGS_DIR" ]; then
    echo "fetch-game-logs: logs directory not found: $LOGS_DIR" >&2
    exit 1
fi

files=""

for GAME_INDEX in "$@"; do
    # Reject non-numeric indices.
    case "$GAME_INDEX" in
        *[!0-9]*)
            echo "fetch-game-logs: GAME_INDEX must be numeric, got: $GAME_INDEX" >&2
            exit 2
            ;;
    esac

    # Use a glob to find matching files, avoiding ls | grep (SC2010).
    found=0
    for path in "$LOGS_DIR"/cost-estimator-"${GAME_INDEX}"-*.log; do
        [ -f "$path" ] || continue
        files="$files $(basename "$path")"
        found=1
    done

    if [ "$found" -eq 0 ]; then
        echo "fetch-game-logs: no logs found for game $GAME_INDEX" >&2
        continue
    fi
done

if [ -z "$files" ]; then
    echo "fetch-game-logs: no log files found for any of the given indexes" >&2
    exit 1
fi

# shellcheck disable=SC2086
exec tar cf - -C "$LOGS_DIR" $files
