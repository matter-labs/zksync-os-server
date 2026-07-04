//! Runs this node as one validator of the BFT committee.
//!
//! The consensus world lives on its own OS thread with its own async runtime and its
//! own networking stack, deliberately isolated from the node's main runtime: consensus
//! must keep making progress (or failing loudly) independently of RPC load or pipeline
//! stalls. The two worlds touch in exactly three places:
//!
//! - the execution environment (given to consensus at spawn), through which consensus
//!   builds, verifies, and commits blocks;
//! - the committed-payload channel, feeding finalized blocks into the node's
//!   persistence pipeline;
//! - a death signal back to the node — if consensus dies, the node must go down with
//!   it rather than keep serving a chain that stopped.

use anyhow::Context as _;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};
use commonware_cryptography::ed25519;
use commonware_p2p::authenticated::lookup;
use commonware_p2p::{Address, AddressableManager as _, Ingress};
use commonware_runtime::{Metrics as _, Quota, Runner as _};
use commonware_utils::TryCollect as _;
use commonware_utils::ordered::{BiMap, Map};
use commonware_utils::union_unique;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use zksync_os_consensus_core::types::Scheme;
use zksync_os_consensus_core::{Channels, NullReporter, StackConfig, start_validator};
use zksync_os_consensus_execution::NodeExecutionEnv;
use zksync_os_storage_api::{ReadStateHistory, WriteState};

use crate::config::ConsensusConfig;

/// Domain-separation namespace for everything this network signs and speaks.
const NAMESPACE: &[u8] = b"zksync-os-consensus";

/// Channel ids, one per consensus traffic class. Every validator must register the
/// same set — an unrecognized channel gets its sender banned.
const VOTES: u64 = 0;
const CERTIFICATES: u64 = 1;
const CERTIFICATE_BACKFILL: u64 = 2;
const BLOCK_BROADCAST: u64 = 3;
const BLOCK_BACKFILL: u64 = 4;

/// One committee member as configured: who they are (network identity, consensus key)
/// and where to reach them.
#[derive(Debug, Clone)]
pub struct CommitteeMember {
    pub network_key: ed25519::PublicKey,
    pub bls_key: <MinPk as Variant>::Public,
    pub address: SocketAddr,
}

/// Parses one `consensus.validators` entry: `<ed25519_hex>:<bls_hex>@<host:port>`.
pub fn parse_committee_member(entry: &str) -> anyhow::Result<CommitteeMember> {
    let (keys, address) = entry
        .split_once('@')
        .context("expected `<ed25519_hex>:<bls_hex>@<host:port>`")?;
    let (network_hex, bls_hex) = keys
        .split_once(':')
        .context("expected `<ed25519_hex>:<bls_hex>` before `@`")?;
    let network_key =
        decode_hex_key::<ed25519::PublicKey>(network_hex).context("invalid ed25519 public key")?;
    let bls_key =
        decode_hex_key::<<MinPk as Variant>::Public>(bls_hex).context("invalid BLS public key")?;
    let address: SocketAddr = address.parse().context("invalid validator address")?;
    Ok(CommitteeMember {
        network_key,
        bls_key,
        address,
    })
}

fn decode_hex_key<T: commonware_codec::DecodeExt<()>>(hex: &str) -> anyhow::Result<T> {
    let bytes = alloy::hex::decode(hex.trim()).context("invalid hex")?;
    T::decode(bytes.as_slice()).map_err(|err| anyhow::anyhow!("invalid key encoding: {err}"))
}

/// Everything the consensus thread needs, resolved and validated on the node side
/// before the thread spawns (so misconfiguration fails startup, not a background
/// thread).
pub struct ConsensusSetup {
    pub committee: Vec<CommitteeMember>,
    pub network_key: ed25519::PrivateKey,
    pub scheme: Scheme,
    pub listen_address: SocketAddr,
    pub allow_private_ips: bool,
    pub max_message_size: usize,
    pub storage_directory: PathBuf,
}

impl ConsensusSetup {
    /// Resolves keys and the committee from configuration.
    pub fn from_config(
        config: &ConsensusConfig,
        storage_directory: PathBuf,
    ) -> anyhow::Result<Self> {
        let committee: Vec<CommitteeMember> = config
            .validators
            .iter()
            .map(|entry| {
                parse_committee_member(entry)
                    .with_context(|| format!("bad `consensus.validators` entry: {entry}"))
            })
            .collect::<anyhow::Result<_>>()?;

        let network_key = decode_hex_key::<ed25519::PrivateKey>(
            config
                .network_key
                .as_ref()
                .context("`consensus.network_key` is required")?,
        )
        .context("invalid `consensus.network_key`")?;
        let bls_key = decode_hex_key::<group::Private>(
            config
                .bls_key
                .as_ref()
                .context("`consensus.bls_key` is required")?,
        )
        .context("invalid `consensus.bls_key`")?;

        // The scheme is the committee: the ordered (network identity → consensus key)
        // map every validator must agree on, plus this validator's own signing key.
        let participants: BiMap<ed25519::PublicKey, <MinPk as Variant>::Public> = committee
            .iter()
            .map(|member| (member.network_key.clone(), member.bls_key))
            .try_collect()
            .map_err(|err| anyhow::anyhow!("duplicate committee member: {err:?}"))?;
        let namespace = union_unique(NAMESPACE, b"_CONSENSUS");
        let scheme = Scheme::signer(&namespace, participants, bls_key).context(
            "this validator's BLS key does not belong to any configured committee member",
        )?;

        use commonware_cryptography::Signer as _;
        anyhow::ensure!(
            committee
                .iter()
                .any(|member| member.network_key == network_key.public_key()),
            "this validator's network key does not appear in `consensus.validators`",
        );

        Ok(Self {
            committee,
            network_key,
            scheme,
            listen_address: config
                .listen_address
                .parse()
                .context("invalid `consensus.listen_address`")?,
            allow_private_ips: config.allow_private_ips,
            max_message_size: config.max_message_size,
            storage_directory,
        })
    }
}

/// Spawns the consensus world. Returns the thread handle and a receiver that fires when
/// consensus dies — the node must treat that as fatal.
pub fn spawn<S>(
    setup: ConsensusSetup,
    env: NodeExecutionEnv<S>,
) -> (
    std::thread::JoinHandle<anyhow::Result<()>>,
    tokio::sync::oneshot::Receiver<()>,
)
where
    S: ReadStateHistory + WriteState + Clone + Send + Sync + 'static,
{
    let (dead_sender, dead_receiver) = tokio::sync::oneshot::channel();
    let handle = std::thread::Builder::new()
        .name("consensus".to_string())
        .spawn(move || {
            let result = run(setup, env);
            // Fire unconditionally: the node must learn about consensus death whether
            // it was an error or an impossible clean exit.
            let _ = dead_sender.send(());
            result
        })
        .expect("failed to spawn consensus thread");
    (handle, dead_receiver)
}

fn run<S>(setup: ConsensusSetup, env: NodeExecutionEnv<S>) -> anyhow::Result<()>
where
    S: ReadStateHistory + WriteState + Clone + Send + Sync + 'static,
{
    let runtime_config = commonware_runtime::tokio::Config::default()
        .with_tcp_nodelay(Some(true))
        .with_worker_threads(3)
        .with_storage_directory(setup.storage_directory.clone())
        .with_catch_panics(false);
    let runner = commonware_runtime::tokio::Runner::new(runtime_config);
    runner.start(|context| async move {
        let quota = Quota::per_second(NonZeroU32::new(128).expect("nonzero"));
        // Block traffic is bulkier and rarer than votes; keep its rate low so backfill
        // cannot starve the vote channels.
        let block_quota = Quota::per_second(NonZeroU32::new(8).expect("nonzero"));
        const BACKLOG: usize = 16_384;

        // TODO(consensus): timing/rate/backlog constants here and in `StackConfig` are
        // fixed at values suitable for small committees on good links; expose the ones
        // staging shows a need to tune (leader timeout, quotas, message size already is).
        let p2p_config = lookup::Config {
            namespace: union_unique(NAMESPACE, b"_P2P"),
            crypto: setup.network_key.clone(),
            listen: setup.listen_address,
            max_message_size: setup.max_message_size as u32,
            mailbox_size: BACKLOG,
            send_batch_size: commonware_utils::NZUsize!(8),
            bypass_ip_check: false,
            allow_private_ips: setup.allow_private_ips,
            allow_dns: true,
            tracked_peer_sets: commonware_utils::NZUsize!(3),
            synchrony_bound: std::time::Duration::from_secs(5),
            max_handshake_age: std::time::Duration::from_secs(10),
            handshake_timeout: std::time::Duration::from_secs(5),
            max_concurrent_handshakes: NonZeroU32::new(512).expect("nonzero"),
            block_duration: std::time::Duration::from_secs(4 * 60 * 60),
            dial_frequency: std::time::Duration::from_secs(1),
            ping_frequency: std::time::Duration::from_secs(50),
            // Validators start together (deploys, tests), so first dials routinely race
            // each other or a not-yet-bound listener. A long cooldown after such a
            // failure delays committee formation by that much — keep it short; the dial
            // frequency already bounds redial traffic.
            peer_connection_cooldown: std::time::Duration::from_secs(5),
            // Committee members may share an IP (co-located validators, every node in
            // an in-process test). The limit only shields against handshake floods, so
            // it just needs to comfortably exceed committee size.
            allowed_handshake_rate_per_ip: Quota::per_second(NonZeroU32::new(64).expect("nonzero")),
            allowed_handshake_rate_per_subnet: Quota::per_second(
                NonZeroU32::new(64).expect("nonzero"),
            ),
        };
        let (mut network, mut oracle) = lookup::Network::new(context.with_label("p2p"), p2p_config);

        // The static committee is peer set 0; validator-set changes later mean
        // tracking new sets under new indices.
        let peers: Map<ed25519::PublicKey, Address> = setup
            .committee
            .iter()
            .map(|member| {
                (
                    member.network_key.clone(),
                    Address::Asymmetric {
                        ingress: Ingress::Socket(member.address),
                        egress: SocketAddr::from((member.address.ip(), 0)),
                    },
                )
            })
            .try_collect()
            .expect("duplicate validator network identity");
        let peers: commonware_p2p::AddressableTrackedPeers<ed25519::PublicKey> = peers.into();
        oracle.track(0, peers).await;

        // Channels must all be registered before the network starts.
        let channels = Channels {
            votes: network.register(VOTES, quota, BACKLOG),
            certificates: network.register(CERTIFICATES, quota, BACKLOG),
            certificate_backfill: network.register(CERTIFICATE_BACKFILL, quota, BACKLOG),
            block_broadcast: network.register(BLOCK_BROADCAST, block_quota, BACKLOG),
            block_backfill: network.register(BLOCK_BACKFILL, block_quota, BACKLOG),
        };
        let network_handle = network.start();

        use commonware_cryptography::Signer as _;
        let identity = setup.network_key.public_key();
        let stack = start_validator(
            context.with_label("validator"),
            StackConfig::new("consensus"),
            identity,
            setup.scheme.clone(),
            env,
            oracle.clone(),
            oracle,
            channels,
            (),
            NullReporter::new(),
        )
        .await;

        // Any component exiting is fatal: these tasks run for the life of the node.
        tokio::select! {
            _ = network_handle => anyhow::bail!("consensus networking exited unexpectedly"),
            _ = stack.engine => anyhow::bail!("consensus engine exited unexpectedly"),
            _ = stack.marshal => anyhow::bail!("consensus marshal exited unexpectedly"),
            _ = stack.broadcast => anyhow::bail!("consensus broadcast exited unexpectedly"),
        }
    })
}
