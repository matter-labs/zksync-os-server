use crate::config::Config;
use crate::init_tx_forwarder::parse_consensus_rpc_forwarder;
use alloy::primitives::{BlockHash, Sealed};
use anyhow::Context;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::time::timeout;
use zksync_os_rpc_api::unstable::{SealedReplayRecord, UnstableApiClient};
use zksync_os_storage_api::{ReplayRecord, WriteReplay};

struct CatchupPeer {
    peer_id: String,
    rpc_url: String,
    client: HttpClient,
    head: u64,
}

pub async fn run_consensus_pre_bootstrap_catchup<Replay>(
    config: &Config,
    replay: Replay,
) -> anyhow::Result<()>
where
    Replay: WriteReplay + Clone,
{
    if !config.consensus_config.enabled || !config.consensus_config.pre_bootstrap_catchup {
        return Ok(());
    }

    anyhow::ensure!(
        !config.consensus_config.tx_forwarding_rpc_urls.is_empty(),
        "`consensus.pre_bootstrap_catchup=true` requires `consensus.tx_forwarding_rpc_urls`"
    );

    let local_peer_id = config
        .network_config
        .derived_peer_id()
        .context("failed to derive local consensus peer id")?
        .to_string();
    let local_head = replay.latest_record();
    let mut peers = vec![];

    for endpoint in &config.consensus_config.tx_forwarding_rpc_urls {
        let (peer_id, rpc_url) = parse_consensus_rpc_forwarder(endpoint)
            .with_context(|| format!("invalid consensus tx RPC forwarder `{endpoint}`"))?;
        if peer_id == local_peer_id {
            continue;
        }

        let client = HttpClientBuilder::new()
            .build(&rpc_url)
            .with_context(|| format!("failed to build RPC client for consensus peer {peer_id}"))?;
        let head = match timeout(
            config
                .consensus_config
                .pre_bootstrap_catchup_request_timeout,
            client.get_replay_head(),
        )
        .await
        {
            Ok(Ok(head)) => head.block_number,
            Ok(Err(err)) => {
                tracing::warn!(
                    %peer_id,
                    %rpc_url,
                    %err,
                    "failed to read consensus peer replay head for pre-bootstrap catchup"
                );
                continue;
            }
            Err(_) => {
                tracing::warn!(
                    %peer_id,
                    %rpc_url,
                    timeout = ?config.consensus_config.pre_bootstrap_catchup_request_timeout,
                    "timed out reading consensus peer replay head for pre-bootstrap catchup"
                );
                continue;
            }
        };

        peers.push(CatchupPeer {
            peer_id,
            rpc_url,
            client,
            head,
        });
    }
    anyhow::ensure!(
        !peers.is_empty(),
        "`consensus.pre_bootstrap_catchup=true` but no remote consensus RPC peer was reachable"
    );

    let Some(peer) = choose_catchup_peer(&config, &replay, peers, local_head).await? else {
        tracing::info!(
            local_head,
            "consensus pre-bootstrap catchup found no peer ahead of local WAL"
        );
        return Ok(());
    };

    tracing::info!(
        peer_id = %peer.peer_id,
        rpc_url = %peer.rpc_url,
        local_head,
        remote_head = peer.head,
        "starting consensus pre-bootstrap catchup"
    );

    for block_number in (local_head + 1)..=peer.head {
        let sealed = get_peer_replay_record(config, &peer, block_number)
            .await?
            .with_context(|| {
                format!(
                    "consensus peer {} is missing replay record {block_number}",
                    peer.peer_id
                )
            })?;
        anyhow::ensure!(
            sealed.record.block_context.block_number == block_number,
            "consensus peer {} returned replay record for block {}, expected {block_number}",
            peer.peer_id,
            sealed.record.block_context.block_number
        );
        replay
            .write(Sealed::new_unchecked(sealed.record, sealed.hash), false)
            .await
            .with_context(|| {
                format!(
                    "failed to append replay record {block_number} from consensus peer {}",
                    peer.peer_id
                )
            })?;
    }

    let caught_up_head = replay.latest_record();
    let caught_up_hash = replay
        .get_canonical_block_hash(caught_up_head)
        .context("local WAL does not expose canonical hash after catchup")?;
    tracing::info!(
        peer_id = %peer.peer_id,
        caught_up_head,
        %caught_up_hash,
        "consensus pre-bootstrap catchup completed"
    );
    Ok(())
}

async fn choose_catchup_peer<Replay>(
    config: &Config,
    replay: &Replay,
    mut peers: Vec<CatchupPeer>,
    local_head: u64,
) -> anyhow::Result<Option<CatchupPeer>>
where
    Replay: WriteReplay + Clone,
{
    peers.sort_by_key(|peer| std::cmp::Reverse(peer.head));

    let mut saw_peer_ahead = false;
    let mut saw_peer_at_local_or_ahead = false;
    for peer in peers {
        if peer.head < local_head {
            continue;
        }
        saw_peer_at_local_or_ahead = true;
        if peer.head == local_head {
            match verify_local_tip(config, replay, &peer, local_head).await {
                Ok(()) => return Ok(None),
                Err(err) => {
                    tracing::warn!(
                        peer_id = %peer.peer_id,
                        rpc_url = %peer.rpc_url,
                        remote_head = peer.head,
                        %err,
                        "consensus peer has local-height head but does not share local WAL tip"
                    );
                    continue;
                }
            }
        }
        saw_peer_ahead = true;

        match verify_local_tip(config, replay, &peer, local_head).await {
            Ok(()) => return Ok(Some(peer)),
            Err(err) => {
                tracing::warn!(
                    peer_id = %peer.peer_id,
                    rpc_url = %peer.rpc_url,
                    remote_head = peer.head,
                    %err,
                    "consensus peer cannot be used for pre-bootstrap catchup"
                );
            }
        }
    }

    if !saw_peer_ahead {
        if saw_peer_at_local_or_ahead {
            anyhow::bail!(
                "consensus.pre_bootstrap_catchup=true but no configured peer at local head \
                 {local_head} shares the local WAL prefix"
            );
        } else {
            return Ok(None);
        }
    }

    anyhow::bail!(
        "consensus.pre_bootstrap_catchup=true but no configured peer with head > {local_head} \
         shares the local WAL prefix"
    );
}

async fn verify_local_tip<Replay>(
    config: &Config,
    replay: &Replay,
    peer: &CatchupPeer,
    local_head: u64,
) -> anyhow::Result<()>
where
    Replay: WriteReplay + Clone,
{
    let local_hash = replay
        .get_canonical_block_hash(local_head)
        .with_context(|| format!("local WAL does not expose canonical hash for {local_head}"))?;
    let remote = get_peer_replay_record(config, peer, local_head)
        .await?
        .with_context(|| {
            format!(
                "consensus peer {} is missing local tip replay record {local_head}",
                peer.peer_id
            )
        })?;
    anyhow::ensure!(
        remote.hash == local_hash,
        "local tip hash mismatch at block {local_head}: local={local_hash}, peer={}",
        remote.hash
    );
    if let Some(local_record) = replay.get_replay_record(local_head) {
        ensure_record_prefix_matches(local_head, local_hash, &local_record, &remote)?;
    }
    Ok(())
}

fn ensure_record_prefix_matches(
    local_head: u64,
    local_hash: BlockHash,
    local_record: &ReplayRecord,
    remote: &SealedReplayRecord,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        &remote.record == local_record,
        "local replay record differs from peer at block {local_head} ({local_hash})"
    );
    Ok(())
}

async fn get_peer_replay_record(
    config: &Config,
    peer: &CatchupPeer,
    block_number: u64,
) -> anyhow::Result<Option<SealedReplayRecord>> {
    timeout(
        config
            .consensus_config
            .pre_bootstrap_catchup_request_timeout,
        peer.client.get_replay_record(block_number),
    )
    .await
    .with_context(|| {
        format!(
            "timed out fetching replay record {block_number} from consensus peer {}",
            peer.peer_id
        )
    })?
    .with_context(|| {
        format!(
            "failed to fetch replay record {block_number} from consensus peer {}",
            peer.peer_id
        )
    })
}
