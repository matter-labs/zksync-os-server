use std::path::Path;

use alloy::primitives::{Address, FixedBytes};
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SetupRecord {
    pub l1_chain_id: u64,
    pub chain_a_id: u64,
    pub chain_b_id: u64,
    pub l1_token_address: Address,
    pub l2_token_address_chain_a: Address,
    pub l2_native_token_vault: Address,
    pub l2_base_token: Address,
    pub erc20_asset_id: FixedBytes<32>,
    pub base_token_asset_id: FixedBytes<32>,
    pub event_emitter_chain_a: Address,
    pub event_emitter_chain_b: Address,
    pub interop_recipient: Address,
    pub deposited_amount: String,
}

pub fn load(path: &Path) -> anyhow::Result<SetupRecord> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read setup file {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse setup file {}", path.display()))
}
