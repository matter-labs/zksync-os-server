use crate::{
    rlp::{self, Rlp},
    vec_to_u64_be,
};

#[derive(Debug, Clone)]
pub struct ZkOSTx {
    pub tx_type: ZkOsTxType,

    pub chain_id: u64,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u64,
    pub max_priority_fee_per_gas: u64,
    pub to: String,
    pub value: u64,

    // only for L1
    pub to_mint: Option<String>,
    pub refund_recipient: Option<String>,
    pub factory_deps: Vec<String>,
    pub tx_hash: Option<String>,
    pub from: Option<String>,
    pub gas_per_pubdata_byte_limit: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum ZkOsTxType {
    L1,
    L2,
}

impl std::fmt::Display for ZkOSTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));
        writeln!(f, "Tx {{")?;
        writeln!(f, "{}    tx_type: {:?},", pad, self.tx_type)?;
        writeln!(f, "{}    chain_id: {},", pad, self.chain_id)?;
        writeln!(f, "{}    nonce: {},", pad, self.nonce)?;
        writeln!(f, "{}    gas_limit: {},", pad, self.gas_limit)?;
        writeln!(f, "{}    max_fee_per_gas: {},", pad, self.max_fee_per_gas)?;
        writeln!(
            f,
            "{}    max_priority_fee_per_gas: {},",
            pad, self.max_priority_fee_per_gas
        )?;
        writeln!(f, "{}    to: {},", pad, self.to)?;
        writeln!(f, "{}    value: {}", pad, self.value)?;
        if let Some(to_mint) = &self.to_mint {
            writeln!(f, "{pad}    to_mint: {to_mint},")?;
        }

        if let Some(refund_recipient) = &self.refund_recipient {
            writeln!(f, "{pad}    refund_recipient: {refund_recipient},")?;
        }

        if !self.factory_deps.is_empty() {
            writeln!(
                f,
                "{}    factory_deps_len: {:?},",
                pad,
                self.factory_deps.len()
            )?;
        }

        if let Some(from) = &self.from {
            writeln!(f, "{pad}    from: {from},")?;
        }

        if let Some(tx_hash) = &self.tx_hash {
            writeln!(f, "{pad}    tx_hash: {tx_hash},")?;
        }
        if let Some(gas_per_pubdata_byte_limit) = self.gas_per_pubdata_byte_limit {
            writeln!(
                f,
                "{pad}    gas_per_pubdata_byte_limit: {gas_per_pubdata_byte_limit},",
            )?;
        }

        writeln!(f, "{pad}}}")?;
        Ok(())
    }
}

impl ZkOSTx {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let tx_type = match bytes[0] {
            42 => Some(ZkOsTxType::L1),
            // TODO: there are other L2 types too.
            2 => Some(ZkOsTxType::L2),
            _ => None,
        }?;

        let rlp = rlp::decode(&bytes[1..])
            .map_err(|e| anyhow::anyhow!("Failed to decode transaction RLP: {:?}", e))
            .unwrap();

        let data = rlp.as_list().unwrap();

        match tx_type {
            ZkOsTxType::L1 => {
                let chain_id = 0;
                let tx_hash = hex::encode(data[0].as_bytes().unwrap());
                let from = hex::encode(data[1].as_bytes().unwrap());
                let to = hex::encode(data[2].as_bytes().unwrap());
                let gas_limit = vec_to_u64_be(data[3].as_bytes().unwrap());
                let gas_per_pubdata_byte_limit = vec_to_u64_be(data[4].as_bytes().unwrap());
                let max_fee_per_gas = vec_to_u64_be(data[5].as_bytes().unwrap());
                let max_priority_fee_per_gas = vec_to_u64_be(data[6].as_bytes().unwrap());
                let nonce = vec_to_u64_be(data[7].as_bytes().unwrap());
                let value = vec_to_u64_be(data[8].as_bytes().unwrap());
                let to_mint = Some(hex::encode(data[9].as_bytes().unwrap()));
                let refund_recipient = Some(hex::encode(data[10].as_bytes().unwrap()));
                // 11 is input.
                let factory_deps = data[12].as_list().unwrap();
                let factory_deps = factory_deps
                    .iter()
                    .map(|x| hex::encode(x.as_bytes().unwrap()))
                    .collect();

                Some(ZkOSTx {
                    tx_type,
                    chain_id,
                    nonce,
                    gas_limit,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    to,
                    value,
                    to_mint,
                    refund_recipient,
                    factory_deps,
                    from: Some(from),
                    tx_hash: Some(tx_hash),
                    gas_per_pubdata_byte_limit: Some(gas_per_pubdata_byte_limit),
                })
            }
            ZkOsTxType::L2 => {
                let chain_id = vec_to_u64_be(data[0].as_bytes().unwrap());
                let nonce = vec_to_u64_be(data[1].as_bytes().unwrap());
                let gas_limit = vec_to_u64_be(data[2].as_bytes().unwrap());
                let max_fee_per_gas = vec_to_u64_be(data[3].as_bytes().unwrap());
                let max_priority_fee_per_gas = vec_to_u64_be(data[4].as_bytes().unwrap());
                let to = hex::encode(data[5].as_bytes().unwrap());
                let value = vec_to_u64_be(data[6].as_bytes().unwrap());

                Some(ZkOSTx {
                    tx_type,
                    chain_id,
                    nonce,
                    gas_limit,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    to,
                    value,
                    to_mint: None,
                    refund_recipient: None,
                    factory_deps: vec![],
                    from: None,
                    tx_hash: None,
                    gas_per_pubdata_byte_limit: None,
                })
            }
        }
    }
}

pub struct ZkOsReceipt {
    pub tx_type: ZkOsTxType,
    pub status: bool,
    pub gas_used: u64,
    pub logs: Vec<Log>,
    pub l2_to_l1_logs: Vec<L2L1Log>,
}

pub struct L2L1Log {
    pub sender: String,
    pub key: String,
    pub value: String,
}

impl L2L1Log {
    pub fn from_rlp(rlp: &Rlp) -> Self {
        let elems = rlp.as_list().unwrap();
        let sender = hex::encode(elems[0].as_bytes().unwrap());
        let key = hex::encode(elems[1].as_bytes().unwrap());
        let value = hex::encode(elems[2].as_bytes().unwrap());

        L2L1Log { sender, key, value }
    }
}

impl std::fmt::Display for L2L1Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));
        write!(
            f,
            "{}L2L1Log {{ sender: {}, key: {}, value: {} }}",
            pad, self.sender, self.key, self.value
        )
    }
}

pub struct Log {
    pub sender: String,
    pub topics: Vec<String>,
    pub data_len: usize,
}

impl Log {
    pub fn from_rlp(rlp: &Rlp) -> Self {
        let elems = rlp.as_list().unwrap();
        let sender = hex::encode(elems[0].as_bytes().unwrap());
        let topics = elems[1]
            .as_list()
            .unwrap()
            .iter()
            .map(|x| hex::encode(x.as_bytes().unwrap()))
            .collect::<Vec<_>>();
        let data_len = elems[2].as_bytes().unwrap().len();

        Log {
            sender,
            topics,
            data_len,
        }
    }
}

impl std::fmt::Display for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));
        write!(
            f,
            "{}Log {{ sender: {}, topics: {:?}, data_len: {} }}",
            pad, self.sender, self.topics, self.data_len
        )
    }
}

impl ZkOsReceipt {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let tx_type = match bytes[0] {
            42 => Some(ZkOsTxType::L1),
            // TODO: there are other L2 types too.
            2 => Some(ZkOsTxType::L2),
            _ => None,
        }?;

        let rlp = rlp::decode(&bytes[1..])
            .map_err(|e| anyhow::anyhow!("Failed to decode transaction RLP: {:?}", e))
            .unwrap();

        let data = rlp.as_list().unwrap();
        let status = vec_to_u64_be(data[0].as_bytes().unwrap());
        let status = status != 0;

        let gas_used = vec_to_u64_be(data[1].as_bytes().unwrap());
        // data[2] is bloom

        // data[3] is logs, data[4] is l2 to l1 logs.
        let logs = data[3].as_list().unwrap();
        let logs: Vec<Log> = logs.iter().map(Log::from_rlp).collect();

        let l2_to_l1_logs = data[4].as_list().unwrap();

        let l2_to_l1_logs: Vec<L2L1Log> = l2_to_l1_logs.iter().map(L2L1Log::from_rlp).collect();

        // there should be 'logs' and 'l2 to l1 logs'..

        Some(ZkOsReceipt {
            tx_type,
            status,
            gas_used,
            logs,
            l2_to_l1_logs,
        })
    }
}

impl std::fmt::Display for ZkOsReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));
        writeln!(f, "Receipt  {{")?;
        writeln!(f, "{}    tx_type: {:?},", pad, self.tx_type)?;
        writeln!(f, "{}    status: {},", pad, self.status)?;
        writeln!(f, "{}    gas_used: {},", pad, self.gas_used)?;
        writeln!(f, "{}    logs_len: {},", pad, self.logs.len())?;
        for log in &self.logs {
            writeln!(f, "{pad}    - {log}")?;
        }
        writeln!(
            f,
            "{}    l2_to_l1_logs_len: {}",
            pad,
            self.l2_to_l1_logs.len()
        )?;
        for log in &self.l2_to_l1_logs {
            writeln!(f, "{pad}    - {log}")?;
        }
        writeln!(f, "{pad}}}")?;
        Ok(())
    }
}

pub struct ZkOsTxMeta {
    pub block_hash: String,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub tx_index_in_block: u64,
    pub effective_gas_price: u64,
    pub number_of_logs_before_this_tx: u64,
    pub gas_used: u64,
    pub contract_address: Option<String>,
}

impl ZkOsTxMeta {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let rlp = rlp::decode(bytes).ok()?;
        let elems = rlp.as_list().unwrap();
        let block_hash = hex::encode(elems[0].as_bytes().unwrap());
        let block_number = vec_to_u64_be(elems[1].as_bytes().unwrap());
        let block_timestamp = vec_to_u64_be(elems[2].as_bytes().unwrap());
        let tx_index_in_block = vec_to_u64_be(elems[3].as_bytes().unwrap());
        let effective_gas_price = vec_to_u64_be(elems[4].as_bytes().unwrap());
        let number_of_logs_before_this_tx = vec_to_u64_be(elems[5].as_bytes().unwrap());
        let gas_used = vec_to_u64_be(elems[6].as_bytes().unwrap());
        let contract_address = if elems.len() < 8 {
            None // Not enough elements for contract address
        } else {
            Some(hex::encode(elems[7].as_bytes().unwrap()))
        };

        Some(ZkOsTxMeta {
            block_hash,
            block_number,
            block_timestamp,
            tx_index_in_block,
            effective_gas_price,
            number_of_logs_before_this_tx,
            gas_used,
            contract_address,
        })
    }
}

impl std::fmt::Display for ZkOsTxMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = " ".repeat(f.width().unwrap_or(0));
        writeln!(f, "MetaTx {{")?;
        writeln!(f, "{}    block_hash: {},", pad, self.block_hash)?;
        writeln!(f, "{}    block_number: {},", pad, self.block_number)?;
        writeln!(f, "{}    block_timestamp: {},", pad, self.block_timestamp)?;
        writeln!(
            f,
            "{}    tx_index_in_block: {},",
            pad, self.tx_index_in_block
        )?;
        writeln!(
            f,
            "{}    effective_gas_price: {},",
            pad, self.effective_gas_price
        )?;
        writeln!(
            f,
            "{}    number_of_logs_before_this_tx: {},",
            pad, self.number_of_logs_before_this_tx
        )?;
        writeln!(f, "{}    gas_used: {},", pad, self.gas_used)?;
        if let Some(ref contract_address) = self.contract_address {
            writeln!(f, "{pad}    contract_address: {contract_address},")?;
        }
        writeln!(f, "{pad}}}")?;
        Ok(())
    }
}
