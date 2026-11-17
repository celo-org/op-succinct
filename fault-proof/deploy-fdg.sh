#!/usr/bin/env bash
# Entrypoint of the fdg-deployer image (fault-proof/Dockerfile.deployer.celo):
# the image equivalent of `just deploy-fdg-contracts`. It reads its
# configuration from the environment (L1_RPC, L2_RPC, L2_NODE_RPC, PRIVATE_KEY,
# GAME_TYPE, ...), as the env file of that recipe would provide it.
set -euo pipefail

cd /app
# Both steps take an env file; the configuration is in the environment instead.
touch .env

echo "=== Fetching FDG configuration ==="
RUST_LOG="${RUST_LOG:-info}" fetch-fault-dispute-game-config --env-file .env

echo "=== Deploying FDG contracts ==="
just _deploy-fdg-contracts .env
