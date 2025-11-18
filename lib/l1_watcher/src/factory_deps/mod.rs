//! TODO: This is a temporary solution to process factory deps for
//! upgrade transactions until we have a working bytecodes supplier
//! contract.
//! This file is to be removed once the sequencer can load factory deps
//! from L1 directly.

use alloy::{
    hex::FromHex,
    primitives::{B256, hex},
};
use serde::Deserialize;
use std::collections::HashMap;
const CONTRACTS_JSON: &str = include_str!("contracts.json");

#[derive(Debug, Deserialize)]
struct ContractEntry {
    bytecode_hash: String,
    bytecode: String,
}

pub fn load_factory_deps() -> anyhow::Result<Vec<(B256, Vec<u8>)>> {
    let map: HashMap<String, ContractEntry> = serde_json::from_str(CONTRACTS_JSON)?;

    let mut out = Vec::with_capacity(map.len());

    for (_name, entry) in map {
        let hash = B256::from_hex(entry.bytecode_hash.as_str())?;

        let bytecode = hex::decode(entry.bytecode)?;

        out.push((hash, bytecode));
    }

    Ok(out)
}
