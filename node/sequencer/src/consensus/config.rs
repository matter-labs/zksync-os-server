//! Configuration utilities for the consensus component.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::NonZeroUsize,
    time::Duration,
};

use anyhow::Context as _;
use secrecy::{ExposeSecret as _, SecretString};
use smart_config::{ByteSize, DescribeConfig, DeserializeConfig, ErrorWithOrigin, Serde};
use smart_config::de::{self, Qualified, WellKnown, WellKnownOption, Entries, DeserializeContext};
use smart_config::metadata::{BasicTypes, ParamMetadata, SizeUnit, TypeDescription};
use zksync_concurrency::{limiter, net, time};
use zksync_consensus_crypto::{Text, TextFmt};
use zksync_consensus_executor as executor;
use zksync_consensus_network as network;
use zksync_consensus_roles::{node, validator};
use zksync_protobuf::build::serde::{Deserialize, Serialize};
use zksync_types::{ethabi, L2ChainId};
use utils::Fallback;

/// `zksync_consensus_crypto::TextFmt` representation of `zksync_consensus_roles::validator::PublicKey`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ValidatorPublicKey(pub String);

impl WellKnown for ValidatorPublicKey {
    type Deserializer = Qualified<Serde![str]>;
    const DE: Self::Deserializer =
        Qualified::new(Serde![str], "has `validator:public:bls12_381:` prefix");
}

impl WellKnownOption for ValidatorPublicKey {}

/// `zksync_consensus_crypto::TextFmt` representation of `zksync_consensus_roles::node::PublicKey`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodePublicKey(pub String);

impl WellKnown for NodePublicKey {
    type Deserializer = Qualified<Serde![str]>;
    const DE: Self::Deserializer = Qualified::new(Serde![str], "has `node:public:ed25519:` prefix");
}

/// Copy-paste of `zksync_concurrency::net::Host`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Host(pub String);

impl WellKnown for Host {
    type Deserializer = Serde![str];
    const DE: Self::Deserializer = Serde![str];
}

/// Copy-paste of `zksync_consensus_roles::validator::ProtocolVersion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl WellKnown for ProtocolVersion {
    type Deserializer = Serde![int];
    const DE: Self::Deserializer = Serde![int];
}

#[derive(Clone, Debug, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct RpcConfig {
    /// Max number of blocks that can be sent from/to each peer.
    #[config(default_t = NonZeroUsize::new(10).unwrap())]
    pub get_block_rps: NonZeroUsize,
}

impl RpcConfig {
    pub fn get_block_rate(&self) -> limiter::Rate {
        let rps = self.get_block_rps.get();
        limiter::Rate {
            burst: rps,
            refresh: time::Duration::seconds(1) / (rps as f32),
        }
    }
}

// We cannot deserialize `Duration` directly because it expects an object with the `secs` (not `seconds`!) and `nanos` fields.
#[derive(Debug, Serialize, Deserialize)]
struct SerdeDuration {
    seconds: u64,
    nanos: u32,
}

#[derive(Debug)]
struct CustomDurationFormat;

impl de::DeserializeParam<Duration> for CustomDurationFormat {
    const EXPECTING: BasicTypes = BasicTypes::OBJECT;

    fn describe(&self, description: &mut TypeDescription) {
        description.set_details("object with `seconds` and `nanos` fields");
    }

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<Duration, ErrorWithOrigin> {
        let duration = SerdeDuration::deserialize(ctx.current_value_deserializer(param.name)?)?;
        Ok(Duration::new(duration.seconds, duration.nanos))
    }

    fn serialize_param(&self, param: &Duration) -> serde_json::Value {
        let duration = SerdeDuration {
            seconds: param.as_secs(),
            nanos: param.subsec_nanos(),
        };
        serde_json::to_value(duration).unwrap()
    }
}

/// Config (shared between main node and external node).
#[derive(Clone, Debug, PartialEq, DescribeConfig, DeserializeConfig)]
pub struct ConsensusConfig {
    pub port: Option<u16>,
    /// Local socket address to listen for the incoming connections.
    pub server_addr: std::net::SocketAddr,
    /// Public address of this node (should forward to `server_addr`)
    /// that will be advertised to peers, so that they can connect to this
    /// node.
    pub public_addr: Host,
    /// Maximal allowed size of the payload in bytes.
    #[config(default_t = ByteSize(2_500_000), with = Fallback(SizeUnit::Bytes))]
    pub max_payload_size: ByteSize,
    /// View timeout duration.
    #[config(default_t = Duration::from_secs(2), with = Fallback(CustomDurationFormat))]
    pub view_timeout: Duration,
    /// Maximal allowed size of the sync-batch payloads in bytes.
    ///
    /// The batch consists of block payloads and a Merkle proof of inclusion on L1 (~1kB),
    /// so the maximum batch size should be the maximum payload size times the maximum number
    /// of blocks in a batch.
    #[config(default_t = ByteSize(12_500_001_024), with = Fallback(SizeUnit::Bytes))]
    pub max_batch_size: ByteSize,

    /// Limit on the number of inbound connections outside the `static_inbound` set.
    #[config(default_t = 100)]
    pub gossip_dynamic_inbound_limit: usize,
    /// Inbound gossip connections that should be unconditionally accepted.
    #[config(default)]
    pub gossip_static_inbound: BTreeSet<NodePublicKey>,
    /// Outbound gossip connections that the node should actively try to
    /// establish and maintain.
    #[config(default, with = Entries::WELL_KNOWN.named("key", "addr"))]
    pub gossip_static_outbound: BTreeMap<NodePublicKey, Host>,

    /// MAIN NODE ONLY: consensus genesis specification.
    /// Used to (re)initialize genesis if needed.
    /// External nodes fetch the genesis from the main node.
    #[config(nest)]
    pub genesis_spec: Option<GenesisSpecConfig>,

    /// Rate limiting configuration for the p2p RPCs.
    #[config(nest)]
    pub rpc: RpcConfig,

    /// Local socket address to expose the node debug page.
    pub debug_page_addr: Option<std::net::SocketAddr>,
}

impl ConsensusConfig {
    pub fn rpc(&self) -> RpcConfig {
        self.rpc.clone()
    }
}

/// Secrets needed for consensus.
#[derive(Debug, Clone, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ConsensusSecrets {
    /// Has `validator:secret:bls12_381:` prefix.
    pub validator_key: Option<SecretString>,
    /// Has `node:secret:ed25519:` prefix.
    pub node_key: Option<SecretString>,
}

fn read_secret_text<T: TextFmt>(text: Option<&SecretString>) -> anyhow::Result<Option<T>> {
    text.map(|text| Text::new(text.expose_secret()).decode())
        .transpose()
        .map_err(|_| anyhow::format_err!("invalid format"))
}

pub(super) fn validator_key(
    secrets: &ConsensusSecrets,
) -> anyhow::Result<Option<validator::SecretKey>> {
    read_secret_text(secrets.validator_key.as_ref())
}

/// Consensus genesis specification.
/// It is a digest of the `validator::Genesis`,
/// which allows to initialize genesis (if not present) or
/// decide whether a hard fork is necessary (if present).
#[derive(Clone, Debug, PartialEq, DescribeConfig, DeserializeConfig)]
pub struct GenesisSpecConfig {
    /// Chain ID for L2.
    #[config(with = Serde![int])]
    pub chain_id: L2ChainId,
    /// Consensus protocol version.
    pub protocol_version: ProtocolVersion,
    /// The validator committee. Represents `zksync_consensus_roles::validator::Committee`.
    #[config(default, with = Entries::WELL_KNOWN.named("key", "weight"))]
    pub validators: Vec<(ValidatorPublicKey, u64)>,
    /// Leader of the committee.
    pub leader: Option<ValidatorPublicKey>,
    // /// Address of the registry contract.
    // pub registry_address: Option<Address>,
    /// Recommended list of peers to connect to.
    #[config(default, with = Entries::WELL_KNOWN.named("key", "addr"))]
    pub seed_peers: BTreeMap<NodePublicKey, Host>,
}

/// Consensus genesis specification.
/// It is a digest of the `validator::Genesis`,
/// which allows to initialize genesis (if not present)
/// decide whether a hard fork is necessary (if present).
#[derive(Debug, PartialEq)]
pub(super) struct GenesisSpec {
    pub(super) chain_id: validator::ChainId,
    pub(super) protocol_version: validator::ProtocolVersion,
    pub(super) validators: Option<validator::Schedule>,
    pub(super) registry_address: Option<ethabi::Address>,
    pub(super) seed_peers: BTreeMap<node::PublicKey, net::Host>,
}

impl GenesisSpec {
    pub(super) fn from_global_config(cfg: &GlobalConfig) -> Self {
        Self {
            chain_id: cfg.genesis.chain_id,
            protocol_version: cfg.genesis.protocol_version,
            validators: cfg.genesis.validators_schedule.clone(),
            registry_address: cfg.registry_address,
            seed_peers: cfg.seed_peers.clone(),
        }
    }

    pub(super) fn parse(x: &GenesisSpecConfig) -> anyhow::Result<Self> {
        let schedule = if x.validators.is_empty() || x.leader.is_none() {
            None
        } else {
            let leader = x.leader.as_ref().unwrap(); // safe to unwrap because of the check above

            let validators: Vec<_> = x
                .validators
                .iter()
                .enumerate()
                .map(|(i, (key, weight))| {
                    Ok(validator::ValidatorInfo {
                        key: Text::new(&key.0).decode().context("key").context(i)?,
                        weight: *weight,
                        leader: key == leader,
                    })
                })
                .collect::<anyhow::Result<_>>()
                .context("validators")?;

            Some(
                validator::Schedule::new(validators, validator::LeaderSelection::default())
                    .context("schedule")?,
            )
        };

        // anyhow::ensure!(
        //     schedule.is_some() || x.registry_address.is_some(),
        //     "either validators or registry_address must be present"
        // );

        Ok(Self {
            chain_id: validator::ChainId(x.chain_id.as_u64()),
            protocol_version: validator::ProtocolVersion(x.protocol_version.0),
            validators: schedule,
            registry_address: None,
            seed_peers: x
                .seed_peers
                .iter()
                .map(|(key, addr)| {
                    anyhow::Ok((
                        Text::new(&key.0)
                            .decode::<node::PublicKey>()
                            .context("key")?,
                        net::Host(addr.0.clone()),
                    ))
                })
                .collect::<Result<_, _>>()
                .context("seed_peers")?,
        })
    }
}

pub(super) fn node_key(secrets: &ConsensusSecrets) -> anyhow::Result<Option<node::SecretKey>> {
    read_secret_text(secrets.node_key.as_ref())
}

pub(super) fn executor(
    cfg: &ConsensusConfig,
    secrets: &ConsensusSecrets,
    global_config: &GlobalConfig,
    build_version: Option<semver::Version>,
) -> anyhow::Result<executor::Config> {
    // Always connect to seed peers.
    // Once we implement dynamic peer discovery,
    // we won't establish a persistent connection to seed peers
    // but rather just ask them for more peers.
    let mut gossip_static_outbound: HashMap<_, _> = global_config
        .seed_peers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    {
        let mut append = |key: &NodePublicKey, addr: &Host| {
            gossip_static_outbound.insert(
                Text::new(&key.0).decode().context("key")?,
                net::Host(addr.0.clone()),
            );
            anyhow::Ok(())
        };
        for (i, (k, v)) in cfg.gossip_static_outbound.iter().enumerate() {
            append(k, v).with_context(|| format!("gossip_static_outbound[{i}]"))?;
        }
    }

    let mut rpc = executor::RpcConfig::default();
    rpc.get_block_rate = cfg.rpc().get_block_rate();

    let debug_page = cfg
        .debug_page_addr
        .map(|addr| network::debug_page::Config { addr });

    Ok(executor::Config {
        build_version,
        server_addr: cfg.server_addr,
        public_addr: net::Host(cfg.public_addr.0.clone()),
        max_payload_size: cfg.max_payload_size.0 as usize,
        view_timeout: cfg.view_timeout.try_into().context("view_timeout")?,
        node_key: node_key(secrets)
            .context("node_key")?
            .context("missing node_key")?,
        validator_key: validator_key(secrets).context("validator_key")?,
        gossip_dynamic_inbound_limit: cfg.gossip_dynamic_inbound_limit,
        gossip_static_inbound: cfg
            .gossip_static_inbound
            .iter()
            .enumerate()
            .map(|(i, x)| Text::new(&x.0).decode().context(i))
            .collect::<Result<_, _>>()
            .context("gossip_static_inbound")?,
        gossip_static_outbound,
        rpc,
        debug_page,
    })
}

/// Global config of the consensus.
#[derive(Debug, PartialEq, Clone)]
pub struct GlobalConfig {
    pub genesis: validator::Genesis,
    pub registry_address: Option<ethabi::Address>,
    pub seed_peers: BTreeMap<node::PublicKey, net::Host>,
}

mod utils {
    use smart_config::{
        de::{DeserializeContext, DeserializeParam, WellKnown},
        metadata::{BasicTypes, ParamMetadata, TypeDescription},
        ErrorWithOrigin,
    };

    /// Combines the standard deserializer with another one having lower priority (wrapped by the type).
    /// The fallback deserializer will only be invoked if the standard deserializer fails. If both deserializers fail,
    /// both their errors will be reported.
    #[derive(Debug)]
    pub(crate) struct Fallback<De>(pub(crate) De);

    impl<T, De> DeserializeParam<T> for Fallback<De>
    where
        T: 'static + WellKnown,
        De: DeserializeParam<T>,
    {
        const EXPECTING: BasicTypes = <T::Deserializer>::EXPECTING.or(De::EXPECTING);

        fn describe(&self, description: &mut TypeDescription) {
            T::DE.describe(description);
            description.set_fallback(&self.0);
        }

        fn deserialize_param(
            &self,
            mut ctx: DeserializeContext<'_>,
            param: &'static ParamMetadata,
        ) -> Result<T, ErrorWithOrigin> {
            let main_err = match T::DE.deserialize_param(ctx.borrow(), param) {
                Ok(value) => return Ok(value),
                Err(err) => err,
            };
            self.0
                .deserialize_param(ctx.borrow(), param)
                .map_err(|fallback_err| {
                    // Push both errors into the context.
                    ctx.push_error(fallback_err);
                    main_err
                })
        }

        fn serialize_param(&self, param: &T) -> serde_json::Value {
            // The main deserializer always has priority
            self.0.serialize_param(param)
        }
    }
}
