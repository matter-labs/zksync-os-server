//! A multi-validator BFT cluster of real in-process nodes over one shared L1.
//!
//! Every validator is a full node — its own runtime, databases, RPC — configured into
//! one consensus committee. Validators must launch *concurrently*: a lone validator has
//! no quorum, produces no blocks, and would therefore never see the initial deposit
//! that node startup waits for.

use crate::test_config::{build_node_config, disable_prover_input_generation};
use crate::utils::LockedPort;
use crate::{ChainLayout, PROTOCOL_VERSION, Tester};
use anyhow::Context as _;
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::Signer as _;
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519;
use futures::future::try_join_all;
use zksync_os_types::NodeRole;

/// One validator's key set, generated fresh per cluster.
struct ValidatorKeys {
    network: ed25519::PrivateKey,
    bls: commonware_cryptography::bls12381::primitives::group::Private,
    committee_entry_keys: String,
}

fn generate_validator_keys() -> ValidatorKeys {
    let mut rng = rand08::rngs::OsRng;
    let mut seed = [0u8; 32];
    rand08::RngCore::fill_bytes(&mut rng, &mut seed);
    let network =
        ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes are a valid key");
    let (bls, bls_public) = ops::keypair::<_, MinPk>(&mut rng);
    let committee_entry_keys = format!(
        "{}:{}",
        alloy::hex::encode(network.public_key().encode()),
        alloy::hex::encode(bls_public.encode()),
    );
    ValidatorKeys {
        network,
        bls,
        committee_entry_keys,
    }
}

pub struct MultiNodeTester {
    nodes: Vec<Tester>,
}

impl MultiNodeTester {
    /// Starts `num_validators` validators as one committee. Exactly one node (the first)
    /// runs the batcher; all serve RPC.
    pub async fn start(num_validators: usize) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;

        let keys: Vec<ValidatorKeys> = (0..num_validators)
            .map(|_| generate_validator_keys())
            .collect();
        // Chain-level constants must be configured identically across the committee
        // (verification pins them); the per-node defaults randomize this one.
        let fee_collector = alloy::primitives::Address::random();
        // Consensus listen ports are allocated here and stay locked until all nodes are
        // up (the node harness allocates its own RPC/network ports separately).
        let mut consensus_ports = Vec::with_capacity(num_validators);
        for _ in 0..num_validators {
            consensus_ports.push(LockedPort::acquire_unused().await?);
        }
        let committee: Vec<String> = keys
            .iter()
            .zip(&consensus_ports)
            .map(|(keys, port)| format!("{}@127.0.0.1:{}", keys.committee_entry_keys, port.port))
            .collect();

        let launches =
            keys.iter()
                .zip(&consensus_ports)
                .enumerate()
                .map(|(index, (keys, consensus_port))| {
                    let l1 = l1.clone();
                    let committee = committee.clone();
                    let network_key = alloy::hex::encode(keys.network.encode());
                    let bls_key = alloy::hex::encode(keys.bls.encode());
                    let listen_address = format!("127.0.0.1:{}", consensus_port.port);
                    async move {
                        let mut config = build_node_config(&l1, chain_layout, false).await?;
                        disable_prover_input_generation(&mut config);
                        config.general_config.node_role = NodeRole::MainNode;
                        config.sequencer_config.fee_collector_address = fee_collector;
                        // Exactly one batcher; every other validator is sequencing-only.
                        config.batcher_config.enabled = index == 0;
                        config.consensus_config.enabled = true;
                        config.consensus_config.network_key = Some(network_key);
                        config.consensus_config.bls_key = Some(bls_key);
                        config.consensus_config.listen_address = listen_address;
                        config.consensus_config.validators = committee;
                        // Everything runs on localhost.
                        config.consensus_config.allow_private_ips = true;
                        Tester::launch_with_new_runtime(l1, chain_layout, config)
                            .await
                            .with_context(|| format!("failed to launch validator {index}"))
                    }
                });
        let nodes = try_join_all(launches).await?;
        Ok(Self { nodes })
    }

    pub fn node(&self, index: usize) -> &Tester {
        &self.nodes[index]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Waits until every validator's RPC reports at least `height`.
    pub async fn wait_for_block_on_all(
        &self,
        height: u64,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()> {
        use alloy::providers::Provider as _;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut heights = Vec::with_capacity(self.nodes.len());
            for node in &self.nodes {
                heights.push(node.l2_provider.get_block_number().await.unwrap_or(0));
            }
            if heights.iter().all(|&number| number >= height) {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "validators did not all reach block {height} within {timeout:?} \
                 (per-validator heights: {heights:?})",
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Asserts every validator serves the identical block hash at `height` — the
    /// RPC-visible form of "all validators committed the same chain".
    pub async fn assert_block_hashes_agree(&self, height: u64) -> anyhow::Result<()> {
        use alloy::eips::BlockId;
        use alloy::providers::Provider as _;
        let mut reference = None;
        for (index, node) in self.nodes.iter().enumerate() {
            let block = node
                .l2_provider
                .get_block(BlockId::number(height))
                .await?
                .with_context(|| format!("validator {index} is missing block {height}"))?;
            let hash = block.header.hash;
            match &reference {
                None => reference = Some(hash),
                Some(expected) => anyhow::ensure!(
                    *expected == hash,
                    "validator {index} serves a different block at height {height}: \
                     {hash} != {expected}",
                ),
            }
        }
        Ok(())
    }

    /// Shuts all validators down (concurrently, since a sequential shutdown would make
    /// the remaining quorum-less validators hang on in-flight work).
    pub async fn shutdown_all(self) -> anyhow::Result<()> {
        try_join_all(self.nodes.into_iter().map(|node| node.shutdown())).await?;
        Ok(())
    }
}
