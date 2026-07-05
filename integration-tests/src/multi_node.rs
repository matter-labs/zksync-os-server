//! A multi-validator BFT cluster of real in-process nodes over one shared L1.
//!
//! Every validator is a full node — its own runtime, databases, RPC — configured into
//! one consensus committee. Validators must launch *concurrently*: a lone validator has
//! no quorum, produces no blocks, and would therefore never see the initial deposit
//! that node startup waits for.

use crate::l1_proxy::SeverableL1Proxy;
use crate::test_config::{build_node_config, disable_prover_input_generation};
use crate::utils::LockedPort;
use crate::{ChainLayout, PROTOCOL_VERSION, StoppedTester, Tester};
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

/// One validator slot: a running node, a stopped one (restartable on the same state
/// and keys), or the momentary in-between while a transition is in flight.
enum Validator {
    Running(Tester),
    Stopped(StoppedTester),
    Transitioning,
}

pub struct MultiNodeTester {
    validators: Vec<Validator>,
    /// Reservations for the consensus listen ports, held for the cluster's lifetime so
    /// a stopped validator can rebind the same address when it restarts.
    consensus_ports: Vec<LockedPort>,
}

impl MultiNodeTester {
    /// Starts `num_validators` validators as one committee. Exactly one node (the first)
    /// runs the batcher; all serve RPC.
    pub async fn start(num_validators: usize) -> anyhow::Result<Self> {
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;
        Self::start_inner(num_validators, chain_layout, l1).await
    }

    /// Like [`Self::start`], but every validator reaches L1 through a
    /// [`SeverableL1Proxy`] the test controls — sever it to emulate a shared L1
    /// RPC provider outage for the whole committee, restore it to end the outage.
    /// The returned tester's own L1 helpers keep a direct anvil connection, so
    /// tests can observe L1 while the committee cannot.
    pub async fn start_with_severable_l1(
        num_validators: usize,
    ) -> anyhow::Result<(Self, SeverableL1Proxy)> {
        let chain_layout = ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        };
        let l1 = crate::AnvilL1::start(chain_layout).await?;
        let proxy = SeverableL1Proxy::start(&l1.address).await?;
        // Nodes derive their L1 RPC URL from the `AnvilL1` handle they are launched
        // with; substituting the address routes every validator through the proxy
        // (the handle's own provider object stays directly connected).
        let mut proxied_l1 = l1.clone();
        proxied_l1.address = proxy.url();
        let tester = Self::start_inner(num_validators, chain_layout, proxied_l1).await?;
        Ok((tester, proxy))
    }

    async fn start_inner(
        num_validators: usize,
        chain_layout: ChainLayout<'static>,
        l1: crate::AnvilL1,
    ) -> anyhow::Result<Self> {
        assert!(
            num_validators >= 2,
            "a committee needs at least 2 validators"
        );

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
        Ok(Self {
            validators: nodes.into_iter().map(Validator::Running).collect(),
            consensus_ports,
        })
    }

    /// The running node at `index`. Panics if that validator is currently stopped —
    /// tests interact only with validators they know to be up.
    pub fn node(&self, index: usize) -> &Tester {
        match &self.validators[index] {
            Validator::Running(node) => node,
            _ => panic!("validator {index} is not running"),
        }
    }

    fn running(&self) -> impl Iterator<Item = (usize, &Tester)> {
        self.validators
            .iter()
            .enumerate()
            .filter_map(|(index, validator)| match validator {
                Validator::Running(node) => Some((index, node)),
                _ => None,
            })
    }

    pub fn len(&self) -> usize {
        self.validators.len()
    }

    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Gracefully stops one validator; its state, keys, and port reservation stay
    /// around for [`Self::start_validator`].
    pub async fn stop_validator(&mut self, index: usize) -> anyhow::Result<()> {
        let validator = std::mem::replace(&mut self.validators[index], Validator::Transitioning);
        let Validator::Running(node) = validator else {
            anyhow::bail!("validator {index} is not running");
        };
        self.validators[index] = Validator::Stopped(node.stop().await?);
        Ok(())
    }

    /// Restarts a stopped validator on its original state and keys. It rejoins the
    /// committee, backfills what it missed, and participates again.
    pub async fn start_validator(&mut self, index: usize) -> anyhow::Result<()> {
        let validator = std::mem::replace(&mut self.validators[index], Validator::Transitioning);
        let Validator::Stopped(stopped) = validator else {
            anyhow::bail!("validator {index} is not stopped");
        };
        // The previous instance's consensus thread winds down asynchronously after the
        // node runtime stops, holding its p2p listener and database handles until the
        // very end. Its storage lock is released last, so "the lock is acquirable"
        // means everything else is gone too — gate the relaunch on it, then on the
        // listen port being bindable. (The lockfile reservation keeps other tests
        // away from the port meanwhile.)
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let instance_lock = zksync_os_server::consensus::instance_lock_path(
            &stopped
                .config()
                .general_config
                .rocks_db_path
                .join("consensus"),
        );
        loop {
            if let Ok(probe) = std::fs::File::create(&instance_lock)
                && fs2::FileExt::try_lock_exclusive(&probe).is_ok()
            {
                drop(probe);
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the previous consensus instance did not release its storage in time",
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let port = self.consensus_ports[index].port;
        loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(probe) => {
                    drop(probe);
                    break;
                }
                Err(error) => {
                    anyhow::ensure!(
                        tokio::time::Instant::now() < deadline,
                        "consensus port {port} still bound long after shutdown: {error}",
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        self.validators[index] = Validator::Running(stopped.start().await?);
        Ok(())
    }

    /// The highest block height any running validator currently reports.
    pub async fn max_height(&self) -> anyhow::Result<u64> {
        use alloy::providers::Provider as _;
        let mut max = 0;
        for (_, node) in self.running() {
            max = max.max(node.l2_provider.get_block_number().await?);
        }
        Ok(max)
    }

    /// Waits until every *running* validator's RPC reports at least `height`.
    pub async fn wait_for_block_on_all(
        &self,
        height: u64,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()> {
        use alloy::providers::Provider as _;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut heights = Vec::with_capacity(self.validators.len());
            let mut all_reached = true;
            for validator in &self.validators {
                match validator {
                    Validator::Running(node) => {
                        let number = node.l2_provider.get_block_number().await.unwrap_or(0);
                        all_reached &= number >= height;
                        heights.push(number.to_string());
                    }
                    _ => heights.push("stopped".to_string()),
                }
            }
            if all_reached {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "running validators did not all reach block {height} within {timeout:?} \
                 (per-validator heights: {heights:?})",
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Asserts every *running* validator serves the identical block hash at `height` —
    /// the RPC-visible form of "all validators committed the same chain".
    pub async fn assert_block_hashes_agree(&self, height: u64) -> anyhow::Result<()> {
        use alloy::eips::BlockId;
        use alloy::providers::Provider as _;
        let mut reference = None;
        for (index, node) in self.running() {
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
        try_join_all(self.validators.into_iter().map(|validator| async move {
            match validator {
                Validator::Running(node) => node.shutdown().await,
                Validator::Stopped(stopped) => stopped.shutdown().await,
                Validator::Transitioning => Ok(()),
            }
        }))
        .await?;
        Ok(())
    }
}
