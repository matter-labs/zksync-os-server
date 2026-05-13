use reth_network_peers::PeerId;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RaftConsensusConfig {
    /// This node's identity in the Raft cluster, derived from the network secret key.
    pub node_id: PeerId,
    /// Full list of peer IDs forming the cluster, including this node.
    /// Must contain `node_id`.
    pub peer_ids: Vec<PeerId>,
    /// If `true`, this node will attempt to initialize the Raft cluster on startup,
    /// waiting for all peers to connect first. This may be enabled on every
    /// consensus node: the first initializer wins and the others safely skip.
    pub bootstrap: bool,
    /// Lower bound for the randomized election timeout. A follower that receives no
    /// heartbeat within a randomly chosen interval `[min, max]` starts a new election.
    pub election_timeout_min: Duration,
    /// Upper bound for the randomized election timeout. Wider range reduces split-vote
    /// probability in clusters with many nodes.
    pub election_timeout_max: Duration,
    /// How often the leader sends heartbeats to suppress follower elections.
    /// Should be significantly smaller than `election_timeout_min`.
    pub heartbeat_interval: Duration,
    /// Directory where the Raft log and state-machine metadata are stored.
    pub storage_path: PathBuf,
}
