use ruint::aliases::{B160, U256};

use crate::{rlp::Rlp, vec_to_u64_be};

#[derive(Debug, Clone)]
pub struct Block {
    pub block_number: u64,
    pub parent_hash: String,
    pub state_root: String,
    pub transactions: Vec<String>,
    pub transaction_root: String,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub timestamp: u64,
    pub base_fee_per_gas: u64,
}

impl std::fmt::Display for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));
        writeln!(f, "Block {{")?;
        writeln!(f, "{}  block_number: {},", pad, self.block_number)?;
        writeln!(f, "{}  parent_hash: {},", pad, self.parent_hash)?;
        writeln!(f, "{}  state_root: {},", pad, self.state_root)?;
        writeln!(f, "{}  transaction_root: {},", pad, self.transaction_root)?;
        writeln!(f, "{}  gas_limit: {},", pad, self.gas_limit)?;
        writeln!(f, "{}  gas_used: {},", pad, self.gas_used)?;
        writeln!(f, "{}  timestamp: {},", pad, self.timestamp)?;
        writeln!(f, "{}  base_fee_per_gas: {}", pad, self.base_fee_per_gas)?;
        writeln!(f, "{pad}}}")?;
        Ok(())
    }
}

impl Block {
    pub fn new_from_rlp(rlp: &Rlp) -> Self {
        let elems = rlp.as_list().unwrap();

        let header = &elems[0].as_list().unwrap();
        let transactions = &elems[1].as_list().unwrap();

        let transactions = transactions
            .iter()
            .map(|x| hex::encode(x.as_bytes().unwrap()))
            .collect();

        let header = header
            .iter()
            .map(|x| x.as_bytes().unwrap())
            .collect::<Vec<_>>();

        let parent_hash = hex::encode(header[0]);
        // 1 - ommers
        // 2 - beneficiary
        let state_root = hex::encode(header[3]);
        let transaction_root = hex::encode(header[4]);
        // 5 - receipts root
        // 6 - bloom
        // 7 - difficulty
        let block_number = vec_to_u64_be(header[8]);
        let gas_limit = vec_to_u64_be(header[9]);
        let gas_used = vec_to_u64_be(header[10]);
        let timestamp = vec_to_u64_be(header[11]);
        // 12 - extra data
        // 13 - mix hash
        // 14 - nonce
        let base_fee_per_gas = vec_to_u64_be(header[15]);

        Block {
            block_number,
            parent_hash,
            state_root,
            transactions,
            transaction_root,
            gas_limit,
            gas_used,
            timestamp,
            base_fee_per_gas,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct BlockMetadata {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hashes: Vec<U256>,
    pub timestamp: u64,
    pub eip1559_basefee: U256,
    pub gas_per_pubdata: U256,
    pub native_price: U256,
    pub coinbase: B160,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    /// Source of randomness, currently holds the value
    /// of prevRandao.
    pub mix_hash: U256,
}

impl std::fmt::Display for BlockMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));

        writeln!(f, "BlockMetadata {{")?;
        writeln!(f, "{}  chain_id: {},", pad, self.chain_id)?;
        writeln!(f, "{}  block_number: {},", pad, self.block_number)?;
        writeln!(
            f,
            "{}  block_hashes: len: {},",
            pad,
            self.block_hashes.len()
        )?;
        writeln!(f, "{}  timestamp: {},", pad, self.timestamp)?;
        writeln!(f, "{}  eip1559_basefee: {},", pad, self.eip1559_basefee)?;
        writeln!(f, "{}  gas_per_pubdata: {},", pad, self.gas_per_pubdata)?;
        writeln!(f, "{}  native_price: {},", pad, self.native_price)?;
        writeln!(f, "{}  coinbase: {:?},", pad, self.coinbase)?;
        writeln!(f, "{}  gas_limit: {},", pad, self.gas_limit)?;
        writeln!(f, "{}  pubdata_limit: {},", pad, self.pubdata_limit)?;
        writeln!(f, "{}  mix_hash: {},", pad, self.mix_hash)?;
        writeln!(f, "{pad}}}")?;
        Ok(())
    }
}
