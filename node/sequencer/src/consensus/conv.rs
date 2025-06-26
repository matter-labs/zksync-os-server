//! Protobuf conversion functions.
use anyhow::Context as _;
use ruint::aliases::{B160, U256};
use zksync_concurrency::net;
use zksync_consensus_roles::node;
use crate::model::{BatchContext, BlockHashes, ReplayRecord};
use zksync_protobuf::{read_required, required, ProtoFmt, ProtoRepr};
use zksync_types::{abi, ethabi, parse_h160, Transaction};

use super::*;
use crate::consensus::{
    config::GlobalConfig,
    types::{BlockCertificate, BlockMetadata, Payload},
};

impl ProtoFmt for BlockCertificate {
    type Proto = proto::BlockCertificate;

    fn read(r: &Self::Proto) -> anyhow::Result<Self> {
        use proto::block_certificate::T;
        Ok(match r.t.as_ref().context("missing t")? {
            T::V1(v1) => Self::V1(ProtoFmt::read(v1).context("v1")?),
            T::V2(v2) => Self::V2(ProtoFmt::read(v2).context("v2")?),
        })
    }

    fn build(&self) -> Self::Proto {
        use proto::block_certificate::T;

        let t = match self {
            Self::V1(qc) => T::V1(qc.build()),
            Self::V2(qc) => T::V2(qc.build()),
        };

        Self::Proto { t: Some(t) }
    }
}

impl ProtoFmt for BlockMetadata {
    type Proto = proto::BlockMetadata;
    fn read(r: &Self::Proto) -> anyhow::Result<Self> {
        Ok(Self {
            payload_hash: read_required(&r.payload_hash).context("payload_hash")?,
        })
    }
    fn build(&self) -> Self::Proto {
        Self::Proto {
            payload_hash: Some(self.payload_hash.build()),
        }
    }
}

impl ProtoRepr for proto::NodeAddr {
    type Type = (node::PublicKey, net::Host);
    fn read(&self) -> anyhow::Result<Self::Type> {
        Ok((
            read_required(&self.key).context("key")?,
            net::Host(required(&self.addr).context("addr")?.clone()),
        ))
    }
    fn build(this: &Self::Type) -> Self {
        Self {
            key: Some(this.0.build()),
            addr: Some(this.1 .0.clone()),
        }
    }
}

impl ProtoFmt for GlobalConfig {
    type Proto = proto::GlobalConfig;

    fn read(r: &Self::Proto) -> anyhow::Result<Self> {
        Ok(Self {
            genesis: read_required(&r.genesis).context("genesis")?,
            registry_address: r
                .registry_address
                .as_ref()
                .map(|a| parse_h160(a))
                .transpose()
                .context("registry_address")?,
            seed_peers: r
                .seed_peers
                .iter()
                .enumerate()
                .map(|(i, e)| e.read().context(i))
                .collect::<Result<_, _>>()
                .context("seed_peers")?,
        })
    }

    fn build(&self) -> Self::Proto {
        Self::Proto {
            genesis: Some(self.genesis.build()),
            registry_address: self.registry_address.map(|a| a.as_bytes().to_vec()),
            seed_peers: self
                .seed_peers
                .iter()
                .map(|(k, v)| ProtoRepr::build(&(k.clone(), v.clone())))
                .collect(),
        }
    }
}

impl ProtoFmt for Payload {
    type Proto = proto::Payload;

    fn read(r: &Self::Proto) -> anyhow::Result<Self> {
        let mut transactions = vec![];
        for (i, tx) in r.transactions.iter().enumerate() {
            transactions.push(tx.read().with_context(|| format!("transactions[{i}]"))?);
        }
        let c = r.batch_context.as_ref().context("batch_context")?;
        let block_hashes_vec = c
            .block_hashes
            .iter()
            .map(|x| U256::from_be_slice(x))
            .collect::<Vec<_>>();
        anyhow::ensure!(block_hashes_vec.len() == 256, "block_hashes.len() != 256");
        let block_hashes: [U256; 256] = block_hashes_vec.try_into().unwrap(); // Safe because we checked the length above.

        let context = BatchContext {
            chain_id: *required(&c.chain_id).context("chain_id")?,
            block_number: *required(&c.block_number).context("block_number")?,
            block_hashes: BlockHashes(block_hashes),
            timestamp: *required(&c.timestamp).context("timestamp")?,
            eip1559_basefee: required(&c.eip1559_basefee)
                .map(|x| U256::from_be_slice(x))
                .context("eip1559_basefee")?,
            gas_per_pubdata: required(&c.gas_per_pubdata)
                .map(|x| U256::from_be_slice(x))
                .context("gas_per_pubdata")?,
            native_price: required(&c.native_price)
                .map(|x| U256::from_be_slice(x))
                .context("native_price")?,
            coinbase: required(&c.coinbase)
                .and_then(|x| B160::try_from_be_slice(x).context("try_from_be_slice"))
                .context("coinbase")?,
            gas_limit: *required(&c.gas_limit).context("gas_limit")?,
        };
        Ok(Self(ReplayRecord {
            context,
            transactions,
        }))
    }

    fn build(&self) -> Self::Proto {
        let batch_context = proto::BatchContext {
            chain_id: Some(self.0.context.chain_id),
            block_number: Some(self.0.context.block_number),
            block_hashes: self
                .0
                .context
                .block_hashes
                .0
                .iter()
                .map(|x| x.to_be_bytes::<32>().to_vec())
                .collect(),
            timestamp: Some(self.0.context.timestamp),
            eip1559_basefee: Some(self.0.context.eip1559_basefee.to_be_bytes::<32>().to_vec()),
            gas_per_pubdata: Some(self.0.context.gas_per_pubdata.to_be_bytes::<32>().to_vec()),
            native_price: Some(self.0.context.native_price.to_be_bytes::<32>().to_vec()),
            coinbase: Some(self.0.context.coinbase.to_be_bytes::<20>().to_vec()),
            gas_limit: Some(self.0.context.gas_limit),
        };
        Self::Proto {
            batch_context: Some(batch_context),
            transactions: self.0.transactions.iter().map(ProtoRepr::build).collect(),
        }
    }
}

impl ProtoRepr for proto::Transaction {
    type Type = Transaction;

    fn read(&self) -> anyhow::Result<Self::Type> {
        use proto::transaction::T;
        let tx = match required(&self.t)? {
            T::L1(l1) => abi::Transaction::L1 {
                tx: required(&l1.rlp)
                    .and_then(|x| {
                        let tokens = ethabi::decode(&[abi::L2CanonicalTransaction::schema()], x)
                            .context("ethabi::decode()")?;
                        // Unwrap is safe because `ethabi::decode` does the verification.
                        let tx =
                            abi::L2CanonicalTransaction::decode(tokens.into_iter().next().unwrap())
                                .context("L2CanonicalTransaction::decode()")?;
                        Ok(tx)
                    })
                    .context("rlp")?
                    .into(),
                factory_deps: l1.factory_deps.clone(),
                eth_block: 0,
            },
            T::L2(l2) => abi::Transaction::L2(required(&l2.rlp).context("rlp")?.clone()),
        };
        Transaction::from_abi(tx, true)
    }

    fn build(tx: &Self::Type) -> Self {
        let tx = abi::Transaction::try_from(tx.clone()).unwrap();
        use proto::transaction::T;
        Self {
            t: Some(match tx {
                abi::Transaction::L1 {
                    tx, factory_deps, ..
                } => T::L1(proto::L1Transaction {
                    rlp: Some(ethabi::encode(&[tx.encode()])),
                    factory_deps,
                }),
                abi::Transaction::L2(tx) => T::L2(proto::L2Transaction { rlp: Some(tx) }),
            }),
        }
    }
}
