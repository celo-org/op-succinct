# Prove Dry-Run

A utility that performs an end-to-end dry-run of OP Succinct Fault Dispute Game proof flow for a specific game index.

- Fetches the target game and its parent from the `DisputeGameFactory`.
- Generates a Range Proof and validates public values against on-chain state.
- Generates an Aggregation Proof and validates verification key commitments.
- Spins up a local Anvil fork of L1 and submits the proof to `prove` for a full preflight.

This is intended for local validation without sending real transactions on L1.

## Usage

### Basic Command

```bash
cargo run --release --bin prove-dryrun --features eigenda -- \
  --index <GAME_INDEX> [--env-file <PATH_TO_ENV>]
```

- `--index <GAME_INDEX>`: index of the game in the `DisputeGameFactory`.
- `--env-file <PATH>`: Path to environment file. Default: `.env.prove_dryrun`.

## Environment

Use `.env.example.prove_dryrun` as a template. Relevant variables:

- `L1_RPC`: L1 execution RPC URL
- `L1_BEACON_RPC`: L1 consensus RPC URL
- `L2_RPC`: L2 execution RPC URL
- `L2_NODE_RPC`: L2 consensus RPC URL
- `EIGENDA_PROXY_ADDRESS`: EigenDA proxy URL
- `USE_KMS_REQUESTER`: If `false`, expects `NETWORK_PRIVATE_KEY` to be an AWS KMS key ARN
- `NETWORK_PRIVATE_KEY`: Succinct Prover Network private key
- `RANGE_PROOF_STRATEGY`: Range proof fulfillment strategy (default: `reserved`)
- `AGG_PROOF_STRATEGY`: Aggregation proof fulfillment strategy (default: `reserved`)
- `AGG_PROOF_MODE`: Aggregation proof mode: `plonk` (default) or `groth16`
- `FACTORY_ADDRESS`: Address of the `DisputeGameFactory`
- `GAME_TYPE`: Fault dispute game type (e.g., `42`)
- `PROPOSER_ADDRESS`: Proposer address
- `PRIVATE_KEY`: Transaction signer private key (funded on L1 fork or Anvil; used when calling `prove` on Anvil)

## What It Does

1. Fetches parent game and target game from the `DisputeGameFactory`.
2. Builds Range Proof witness using the host, runs SP1 network proving, and saves the proof to:
   - `data/<L2_CHAIN_ID>/proofs/range/<L2_START>-<L2_END>.bin`
3. Validates public values and commitments against on-chain game state:
   - L1 head hash
   - L2 post-state root
   - Rollup config hash
   - Range verification key hash
4. Builds Aggregation Proof (PLONK or Groth16, determined by `AGG_PROOF_MODE`), validates aggregation vkey commitment, and saves to:
   - `data/<L2_CHAIN_ID>/proofs/agg/agg.bin`
5. Starts a local Anvil node forked from `L1_RPC` at `l1_head + 1`, submits `prove(agg_proof)` to the game, mines a block, and asserts the final status is `UnchallengedAndValidProofProvided`.

## Examples

Run with default env file and a specific game index:

```bash
cargo run --release --bin prove-dryrun -- --index 100
```

Run with a custom env file:

```bash
cargo run --release --bin prove-dryrun -- \
  --env-file .env.mainnet \
  --index 2072
```
