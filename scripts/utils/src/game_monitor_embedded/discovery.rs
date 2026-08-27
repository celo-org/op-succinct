use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use anyhow::Context;
use fault_proof::contract::{
    DisputeGameFactory::DisputeGameFactoryInstance, OPSuccinctFaultDisputeGame,
};

/// Game type filter — OP Succinct fault dispute game (verbatim from game_monitor.rs).
pub const GAME_TYPE: u32 = 42;

#[derive(Debug, Clone)]
pub struct GameData {
    pub game_index: u64,
    pub game_address: Address,
    pub start_block: u64,
    pub end_block: u64,
    pub created_at: SystemTime,
}

impl GameData {
    pub fn block_range(&self) -> u64 {
        self.end_block.saturating_sub(self.start_block)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchGameError {
    #[error("game {game_index} has type {game_type}, expected {expected}")]
    WrongGameType { game_index: u64, game_type: u32, expected: u32 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Lifted from game_monitor.rs (~1105-1140). Reads game metadata, filters type-42,
/// resolves the real [start, end] block range plus the factory `created_at`.
pub async fn fetch_game_data<P: alloy_provider::Provider + Clone>(
    game_index: u64,
    factory: &DisputeGameFactoryInstance<P>,
    l1_provider: P,
) -> Result<GameData, FetchGameError> {
    let game_info = factory
        .gameAtIndex(U256::from(game_index))
        .call()
        .await
        .context("failed to get game at index")?;

    let game_type = game_info.gameType;
    if game_type != GAME_TYPE {
        return Err(FetchGameError::WrongGameType { game_index, game_type, expected: GAME_TYPE });
    }

    let game_address = game_info.proxy;
    let created_at_secs = U256::from(game_info.timestamp).to::<u64>();
    let created_at = UNIX_EPOCH + Duration::from_secs(created_at_secs);

    let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider);
    let l2_block_number =
        game.l2BlockNumber().call().await.context("failed to get L2 block number")?.to::<u64>();
    let start_block = game
        .startingBlockNumber()
        .call()
        .await
        .context("failed to get starting block number")?
        .to::<u64>();

    Ok(GameData { game_index, game_address, start_block, end_block: l2_block_number, created_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_range_is_end_minus_start() {
        let g = GameData {
            game_index: 1,
            game_address: Address::ZERO,
            start_block: 100,
            end_block: 300,
            created_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(g.block_range(), 200);
    }

    #[test]
    fn game_type_constant_is_42() {
        assert_eq!(GAME_TYPE, 42);
    }
}
