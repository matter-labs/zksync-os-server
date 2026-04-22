use crate::{IChainAssetHandler, ZkChain};
use alloy::primitives::{Address, U256};
use alloy::providers::DynProvider;
use anyhow::Context;
use std::sync::Arc;

/// Settlement layer that a chain was committing to during a given batch range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntervalSettlementLayer {
    /// Settling on L1 directly.
    L1,
    /// Settling on a Gateway, identified by its chain ID.
    Gateway(u64),
}

/// Inclusive batch-number range during which the chain committed to a single settlement layer.
///
/// `last_batch` is `None` for the currently-active (open-ended) interval.
#[derive(Debug, Clone)]
pub struct SettlementLayerInterval {
    pub settlement_layer: IntervalSettlementLayer,
    pub first_batch: u64,
    pub last_batch: Option<u64>,
}

/// Settlement layer intervals for a chain paired with the diamond proxies needed to route batch
/// lookups to the correct RPC.
///
/// The intervals cover all batches from `1` upwards in ascending order, with the last entry being
/// open-ended (`last_batch = None`).
#[derive(Debug, Clone)]
pub struct SettlementLayerIntervals {
    intervals: Arc<Vec<SettlementLayerInterval>>,
    diamond_proxy_l1: ZkChain<DynProvider>,
    /// Diamond proxy of the chain's current settlement layer, paired with that SL's chain ID.
    /// `None` means the chain currently settles on L1, in which case lookups for historical
    /// Gateway intervals are unsupported (no Gateway provider is configured).
    diamond_proxy_gw: Option<(u64, ZkChain<DynProvider>)>,
}

impl SettlementLayerIntervals {
    /// Discovers the intervals on-chain from `IL1ChainAssetHandler.migrationInterval` and stores
    /// the diamond proxies needed to resolve future lookups.
    pub async fn discover(
        chain_asset_handler: Address,
        diamond_proxy_l1: ZkChain<DynProvider>,
        diamond_proxy_gw: Option<(u64, ZkChain<DynProvider>)>,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let intervals = find_settlement_layer_intervals(
            chain_asset_handler,
            diamond_proxy_l1.provider().clone(),
            chain_id,
        )
        .await
        .context("failed to discover settlement layer intervals")?;
        Ok(Self {
            intervals: Arc::new(intervals),
            diamond_proxy_l1,
            diamond_proxy_gw,
        })
    }

    pub fn intervals(&self) -> &[SettlementLayerInterval] {
        &self.intervals
    }

    /// Returns the diamond proxy that should be used to fetch data about `batch_number`, based on
    /// which settlement layer interval it falls into.
    pub fn resolve_proxy(&self, batch_number: u64) -> anyhow::Result<&ZkChain<DynProvider>> {
        let interval = self.find_interval(batch_number).with_context(|| {
            format!("batch {batch_number} does not belong to any known settlement layer interval")
        })?;
        match interval.settlement_layer {
            IntervalSettlementLayer::L1 => Ok(&self.diamond_proxy_l1),
            IntervalSettlementLayer::Gateway(chain_id) => match &self.diamond_proxy_gw {
                Some((gw_chain_id, gw)) if *gw_chain_id == chain_id => Ok(gw),
                Some((gw_chain_id, _)) => anyhow::bail!(
                    "batch {batch_number} was committed on Gateway with chain ID {chain_id} but \
                     the chain's current Gateway is {gw_chain_id}; no provider is available for \
                     the historical Gateway"
                ),
                None => anyhow::bail!(
                    "batch {batch_number} was committed on Gateway with chain ID {chain_id} but \
                     the chain currently settles on L1; no Gateway provider is configured"
                ),
            },
        }
    }

    fn find_interval(&self, batch_number: u64) -> Option<&SettlementLayerInterval> {
        self.intervals.iter().find(|i| {
            batch_number >= i.first_batch && i.last_batch.is_none_or(|last| batch_number <= last)
        })
    }
}

/// Returns all batch-number intervals during which the chain committed to a single settlement
/// layer, in ascending order and covering all batches from `1` upwards.
///
/// The intervals are reconstructed from `IL1ChainAssetHandler.migrationInterval(chainId, i)`
/// for each known migration slot (`i ∈ [1, migrationNumber(chainId)]`):
///
/// - Each populated slot describes one L1 → Gateway → L1 round-trip, giving us the chain's own
///   batch number at which the migration to the Gateway happened (`migrateToGWBatchNumber`) and
///   the one at which it returned (`migrateFromGWBatchNumber`, or `isActive = true` if the
///   chain has not returned yet).
/// - Slot `0` is reserved for the legacy Gateway and is intentionally skipped — legacy-GW chains
///   are not supported here.
/// - `MAX_ALLOWED_NUMBER_OF_MIGRATIONS = 2` on-chain, so at most two cycles are supported.
async fn find_settlement_layer_intervals(
    chain_asset_handler: Address,
    provider: DynProvider,
    chain_id: u64,
) -> anyhow::Result<Vec<SettlementLayerInterval>> {
    let cah = IChainAssetHandler::new(chain_asset_handler, provider);
    let total_migrations: u64 = match cah.migrationNumber(U256::from(chain_id)).call().await {
        Ok(n) => n
            .try_into()
            .map_err(|e| anyhow::anyhow!("migrationNumber overflow: {e}"))?,
        // Pre-V31 `ChainAssetHandler` does not expose `migrationNumber`. In that era Gateway
        // migrations are not possible, so the chain has always committed to L1.
        Err(e) if is_method_missing(&e) => {
            tracing::debug!(
                "ChainAssetHandler does not expose migrationNumber; assuming pre-V31 protocol \
                 with no Gateway migrations: {e}"
            );
            return Ok(vec![SettlementLayerInterval {
                settlement_layer: IntervalSettlementLayer::L1,
                first_batch: 1,
                last_batch: None,
            }]);
        }
        Err(e) => return Err(anyhow::Error::new(e).context("failed to fetch migrationNumber")),
    };

    let raw = futures::future::try_join_all((1..=total_migrations).map(|i| {
        let cah = &cah;
        async move {
            let interval = cah
                .migrationInterval(U256::from(chain_id), U256::from(i))
                .call()
                .await
                .with_context(|| format!("failed to fetch migrationInterval({chain_id}, {i})"))?;
            anyhow::Ok(interval)
        }
    }))
    .await?;

    let mut intervals = Vec::new();
    let mut cursor: u64 = 1;
    let mut on_active_gw = false;
    for raw in raw {
        // Uninitialized slots have all fields zero; skip them.
        if raw.settlementLayerChainId.is_zero() {
            continue;
        }
        let sl_chain_id: u64 = raw
            .settlementLayerChainId
            .try_into()
            .map_err(|e| anyhow::anyhow!("settlementLayerChainId overflow: {e}"))?;
        let to_batch: u64 = raw
            .migrateToGWBatchNumber
            .try_into()
            .map_err(|e| anyhow::anyhow!("migrateToGWBatchNumber overflow: {e}"))?;

        // L1 interval leading up to this migration (if the chain committed any batches to L1
        // before it).
        anyhow::ensure!(
            to_batch + 1 >= cursor,
            "settlement layer interval is not in order: {} < {}",
            to_batch + 1,
            cursor
        );
        intervals.push(SettlementLayerInterval {
            settlement_layer: IntervalSettlementLayer::L1,
            first_batch: cursor,
            last_batch: Some(to_batch),
        });
        cursor = to_batch + 1;

        if raw.isActive {
            intervals.push(SettlementLayerInterval {
                settlement_layer: IntervalSettlementLayer::Gateway(sl_chain_id),
                first_batch: cursor,
                last_batch: None,
            });
            on_active_gw = true;
            break;
        }
        let from_batch: u64 = raw
            .migrateFromGWBatchNumber
            .try_into()
            .map_err(|e| anyhow::anyhow!("migrateFromGWBatchNumber overflow: {e}"))?;
        intervals.push(SettlementLayerInterval {
            settlement_layer: IntervalSettlementLayer::Gateway(sl_chain_id),
            first_batch: cursor,
            last_batch: Some(from_batch),
        });
        cursor = from_batch + 1;
    }
    if !on_active_gw {
        intervals.push(SettlementLayerInterval {
            settlement_layer: IntervalSettlementLayer::L1,
            first_batch: cursor,
            last_batch: None,
        });
    }
    Ok(intervals)
}

/// Returns `true` if the error came from the contract itself (empty return data or a revert),
/// which is how an EVM reports a call to a function selector that the deployed code does not
/// implement. Network / transport failures return `false` so they can be propagated.
fn is_method_missing(err: &alloy::contract::Error) -> bool {
    match err {
        alloy::contract::Error::ZeroData(..) => true,
        alloy::contract::Error::TransportError(te) => te.as_error_resp().is_some(),
        _ => false,
    }
}
