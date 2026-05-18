#!/bin/sh
# rerun-cost-estimator.sh -- replay a cost-estimator run from the header
# game-monitor records at the top of every cost-estimator-*.log file initiated
# by the game-monitor. Intended for use within the game-monitor pod.
#
# Usage: rerun-cost-estimator.sh GAME_INDEX
#   Picks the most recent cost-estimator-<GAME_INDEX>-*.log under $LOGS_DIR.
#
# Locates `cost-estimator` via `which`, writes the env block verbatim to
# .env_<GAME_INDEX> in $PWD, and re-execs the binary with the recorded args
# (with --env-file rewritten to .env_<GAME_INDEX>).
#
# Override LOGS_DIR (default /logs) when running outside the pod.

set -eu
umask 077

LOGS_DIR=${LOGS_DIR:-/logs}
GAME_INDEX=${1:-}

if [ -z "$GAME_INDEX" ]; then
    echo "rerun-cost-estimator: missing required GAME_INDEX argument" >&2
    echo "usage: rerun-cost-estimator.sh GAME_INDEX" >&2
    exit 2
fi

# Reject non-numeric indices so they cannot smuggle regex/filename metachars.
case "$GAME_INDEX" in
    *[!0-9]*)
        echo "rerun-cost-estimator: GAME_INDEX must be numeric, got: $GAME_INDEX" >&2
        exit 2
        ;;
esac

if [ ! -d "$LOGS_DIR" ]; then
    echo "rerun-cost-estimator: logs directory not found: $LOGS_DIR" >&2
    exit 1
fi

# Anchor the index as a complete token so 312 cannot match 31200.
pattern="^cost-estimator-${GAME_INDEX}-.*\\.log\$"

# `ls -t` lists newest first, so retryN logs naturally win over the initial
# attempt for the same game index.
LOG_FILE=$(
    ls -1t -- "$LOGS_DIR" 2>/dev/null \
        | grep -E "$pattern" \
        | head -n 1 \
        || true
)

if [ -z "$LOG_FILE" ]; then
    echo "rerun-cost-estimator: no cost-estimator-${GAME_INDEX}-*.log under $LOGS_DIR" >&2
    exit 1
fi

LOG_PATH="$LOGS_DIR/$LOG_FILE"

echo "rerun-cost-estimator: log   = $LOG_PATH"   >&2
echo "rerun-cost-estimator: index = $GAME_INDEX" >&2

BIN=$(which cost-estimator 2>/dev/null || true)
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
    echo "rerun-cost-estimator: cost-estimator binary not found in PATH" >&2
    exit 1
fi
echo "rerun-cost-estimator: bin   = $BIN" >&2

# Header layout written by game_monitor.rs:
#   === Cost Estimator Command ===
#   <binary path> <space-joined args>
#   === Cost Estimator ENV ===
#   KEY=VALUE...
#   === Output ===
#   <blank line, then captured stdout/stderr>

CMD_LINE=$(
    awk '
        /^=== Cost Estimator Command ===[[:space:]]*$/ { in_cmd = 1; next }
        /^=== Cost Estimator ENV ===[[:space:]]*$/     { in_cmd = 0 }
        in_cmd && NF                                   { print; exit }
    ' "$LOG_PATH"
)

if [ -z "$CMD_LINE" ]; then
    echo "rerun-cost-estimator: no Cost Estimator Command block in $LOG_PATH" >&2
    exit 1
fi

ENV_FILE=".env_${GAME_INDEX}"

awk '
    /^=== Cost Estimator ENV ===[[:space:]]*$/ { in_env = 1; next }
    /^=== Output ===[[:space:]]*$/             { in_env = 0; exit }
    in_env                                     { print }
' "$LOG_PATH" > "$ENV_FILE"

if [ ! -s "$ENV_FILE" ]; then
    echo "rerun-cost-estimator: Cost Estimator ENV block was empty in $LOG_PATH" >&2
    rm -f "$ENV_FILE"
    exit 1
fi

echo "rerun-cost-estimator: env   = $ENV_FILE" >&2

# Drop the leading binary token (we use $BIN from `which`), then retarget
# --env-file at the new env file.
ARGS=$(
    printf '%s\n' "$CMD_LINE" \
        | sed \
            -e 's/^[[:space:]]\{1,\}//' \
            -e 's/^[^[:space:]]\{1,\}[[:space:]]\{1,\}//' \
            -e 's|--env-file[[:space:]]\{1,\}[^[:space:]][^[:space:]]*|--env-file '"$ENV_FILE"'|'
)

# Defensive: if the recorded command somehow had no --env-file, append one.
case " $ARGS " in
    *" --env-file "*) ;;
    *)                ARGS="$ARGS --env-file $ENV_FILE" ;;
esac

echo "rerun-cost-estimator: exec  = $BIN $ARGS" >&2

# Word-splitting on $ARGS is deliberate -- the recorded args are space-
# separated tokens with no quoting (game_monitor.rs uses args.join(" ")).
# Do NOT quote $ARGS or the script breaks.
# shellcheck disable=SC2086
exec "$BIN" $ARGS
