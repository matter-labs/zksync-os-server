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
        write!(f, "Tx {{\n")?;
        write!(f, "{}    tx_type: {:?},\n", pad, self.tx_type)?;
        write!(f, "{}    chain_id: {},\n", pad, self.chain_id)?;
        write!(f, "{}    nonce: {},\n", pad, self.nonce)?;
        write!(f, "{}    gas_limit: {},\n", pad, self.gas_limit)?;
        write!(f, "{}    max_fee_per_gas: {},\n", pad, self.max_fee_per_gas)?;
        write!(
            f,
            "{}    max_priority_fee_per_gas: {},\n",
            pad, self.max_priority_fee_per_gas
        )?;
        write!(f, "{}    to: {},\n", pad, self.to)?;
        write!(f, "{}    value: {}\n", pad, self.value)?;
        if let Some(to_mint) = &self.to_mint {
            write!(f, "{}    to_mint: {},\n", pad, to_mint)?;
        }

        if let Some(refund_recipient) = &self.refund_recipient {
            write!(f, "{}    refund_recipient: {},\n", pad, refund_recipient)?;
        }

        if !self.factory_deps.is_empty() {
            write!(
                f,
                "{}    factory_deps_len: {:?},\n",
                pad,
                self.factory_deps.len()
            )?;
        }

        if let Some(from) = &self.from {
            write!(f, "{}    from: {},\n", pad, from)?;
        }

        if let Some(tx_hash) = &self.tx_hash {
            write!(f, "{}    tx_hash: {},\n", pad, tx_hash)?;
        }
        if let Some(gas_per_pubdata_byte_limit) = self.gas_per_pubdata_byte_limit {
            write!(
                f,
                "{}    gas_per_pubdata_byte_limit: {},\n",
                pad, gas_per_pubdata_byte_limit
            )?;
        }

        write!(f, "{}}}", pad)?;
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
        let logs: Vec<Log> = logs.iter().map(|log| Log::from_rlp(log)).collect();

        let l2_to_l1_logs = data[4].as_list().unwrap();

        let l2_to_l1_logs: Vec<L2L1Log> = l2_to_l1_logs
            .iter()
            .map(|log| L2L1Log::from_rlp(log))
            .collect();

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
        write!(f, "Receipt  {{\n")?;
        write!(f, "{}    tx_type: {:?},\n", pad, self.tx_type)?;
        write!(f, "{}    status: {}\n", pad, self.status)?;
        write!(f, "{}    gas_used: {}\n", pad, self.gas_used)?;
        write!(f, "{}    logs_len: {}\n", pad, self.logs.len())?;
        for log in &self.logs {
            write!(f, "{}    - {}\n", pad, log)?;
        }
        write!(
            f,
            "{}    l2_to_l1_logs_len: {}\n",
            pad,
            self.l2_to_l1_logs.len()
        )?;
        for log in &self.l2_to_l1_logs {
            write!(f, "{}    - {}\n", pad, log)?;
        }
        write!(f, "{}}}", pad)?;
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
        write!(f, "MetaTx {{\n")?;
        write!(f, "{}    block_hash: {},\n", pad, self.block_hash)?;
        write!(f, "{}    block_number: {},\n", pad, self.block_number)?;
        write!(f, "{}    block_timestamp: {},\n", pad, self.block_timestamp)?;
        write!(
            f,
            "{}    tx_index_in_block: {},\n",
            pad, self.tx_index_in_block
        )?;
        write!(
            f,
            "{}    effective_gas_price: {},\n",
            pad, self.effective_gas_price
        )?;
        write!(
            f,
            "{}    number_of_logs_before_this_tx: {},\n",
            pad, self.number_of_logs_before_this_tx
        )?;
        write!(f, "{}    gas_used: {},\n", pad, self.gas_used)?;
        if let Some(ref contract_address) = self.contract_address {
            write!(f, "{}    contract_address: {},\n", pad, contract_address)?;
        }
        write!(f, "{}}}", pad)?;
        Ok(())
    }
}
