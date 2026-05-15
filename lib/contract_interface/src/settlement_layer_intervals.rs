use crate::{Bridgehub, IChainAssetHandler, ZkChain, is_method_missing};
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider};
use anyhow::Context;
use std::fmt;
use std::sync::Arc;

/// Settlement layer that a chain was committing to during a given batch range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntervalSettlementLayer {
    /// Settling on L1 directly.
    L1,
    /// Settling on a Gateway, identified by its chain ID.
    Gateway(u64),
}

impl fmt::Display for IntervalSettlementLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntervalSettlementLayer::L1 => f.write_str("L1"),
            IntervalSettlementLayer::Gateway(chain_id) => write!(f, "Gateway({chain_id})"),
        }
    }
}

/// Inclusive batch-number range during which the chain committed to a single settlement layer,
/// paired with the diamond proxy for that settlement layer.
///
/// `last_batch` is `None` for the currently-active (open-ended) interval.
#[derive(Debug, Clone)]
pub struct SettlementLayerInterval {
    pub settlement_layer: IntervalSettlementLayer,
    pub first_batch: u64,
    pub last_batch: Option<u64>,
    /// Diamond proxy on `settlement_layer`.
    pub proxy: ZkChain<DynProvider>,
}

impl fmt::Display for SettlementLayerInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.last_batch {
            Some(last) => write!(
                f,
                "{} batches {}..={}",
                self.settlement_layer, self.first_batch, last
            ),
            None => write!(
                f,
                "{} batches {}..",
                self.settlement_layer, self.first_batch
            ),
        }
    }
}

struct RawSettlementLayerInterval {
    settlement_layer: IntervalSettlementLayer,
    first_batch: u64,
    last_batch: Option<u64>,
}

/// Settlement layer intervals for a chain. Each entry carries the diamond proxy needed to route
/// batch lookups to the correct RPC.
///
/// The intervals cover all batches from `1` upwards in ascending order, with the last entry being
/// open-ended (`last_batch = None`).
#[derive(Debug, Clone)]
pub struct SettlementLayerIntervals {
    intervals: Arc<Vec<SettlementLayerInterval>>,
}

impl SettlementLayerIntervals {
    /// Discovers the intervals on-chain from `IL1ChainAssetHandler.migrationInterval` and attaches
    /// the matching diamond proxy to each. Fails if a historical Gateway interval references a
    /// chain that the configured `gateway_provider` cannot serve.
    pub async fn discover(
        chain_asset_handler: Address,
        diamond_proxy_l1: ZkChain<DynProvider>,
        gateway_provider: Option<DynProvider>,
        l2_chain_id: u64,
    ) -> anyhow::Result<Self> {
        let raw_intervals = find_settlement_layer_intervals(
            chain_asset_handler,
            diamond_proxy_l1.provider().clone(),
            l2_chain_id,
        )
        .await
        .context("failed to discover settlement layer intervals")?;
        // Resolve historical Gateway diamond proxy if the chain has any Gateway interval AND
        // gateway_provider is configured.
        let has_historical_gateway = raw_intervals
            .iter()
            .any(|i| matches!(i.settlement_layer, IntervalSettlementLayer::Gateway(_)));
        let diamond_proxy_gw =
            if has_historical_gateway && let Some(gateway_provider) = &gateway_provider {
                let gw_chain_id = gateway_provider.get_chain_id().await?;
                let bridgehub_gw = Bridgehub::new(
                    crate::l1_discovery::L2_BRIDGEHUB_ADDRESS,
                    gateway_provider.clone(),
                    l2_chain_id,
                );
                let historical_diamond_proxy_gw = bridgehub_gw
                    .zk_chain()
                    .await
                    .context("failed to resolve historical Gateway diamond proxy")?;
                Some((gw_chain_id, historical_diamond_proxy_gw))
            } else {
                None
            };

        let mut intervals = Vec::with_capacity(raw_intervals.len());
        for raw in raw_intervals {
            let proxy = match raw.settlement_layer {
                IntervalSettlementLayer::L1 => diamond_proxy_l1.clone(),
                IntervalSettlementLayer::Gateway(chain_id) => match &diamond_proxy_gw {
                    Some((gw_chain_id, gw)) if *gw_chain_id == chain_id => gw.clone(),
                    Some((gw_chain_id, _)) => anyhow::bail!(
                        "interval {}..{} was committed on Gateway with chain ID {chain_id} but \
                         the chain's current Gateway is {gw_chain_id}; no provider is available \
                         for the historical Gateway",
                        raw.first_batch,
                        raw.last_batch
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                    ),
                    None => anyhow::bail!(
                        "interval {}..{} was committed on Gateway with chain ID {chain_id} but \
                         the chain currently settles on L1; no Gateway provider is configured",
                        raw.first_batch,
                        raw.last_batch
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                    ),
                },
            };
            intervals.push(SettlementLayerInterval {
                settlement_layer: raw.settlement_layer,
                first_batch: raw.first_batch,
                last_batch: raw.last_batch,
                proxy,
            });
        }
        Ok(Self {
            intervals: Arc::new(intervals),
        })
    }

    pub fn intervals(&self) -> &[SettlementLayerInterval] {
        &self.intervals
    }

    /// Settlement layer of the currently-active (open-ended) interval — i.e. where the chain is
    /// currently committing batches.
    pub fn current_settlement_layer(&self) -> &IntervalSettlementLayer {
        &self
            .intervals
            .last()
            .expect("settlement layer intervals are never empty")
            .settlement_layer
    }

    /// `true` when the chain is currently committing batches to a Gateway.
    pub fn settles_on_gateway(&self) -> bool {
        matches!(
            self.current_settlement_layer(),
            IntervalSettlementLayer::Gateway(_)
        )
    }

    /// Returns the settlement layer interval containing `batch_number`.
    pub fn find_interval(&self, batch_number: u64) -> Option<&SettlementLayerInterval> {
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
) -> anyhow::Result<Vec<RawSettlementLayerInterval>> {
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
            return Ok(vec![RawSettlementLayerInterval {
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
        intervals.push(RawSettlementLayerInterval {
            settlement_layer: IntervalSettlementLayer::L1,
            first_batch: cursor,
            last_batch: Some(to_batch),
        });
        cursor = to_batch + 1;

        if raw.isActive {
            intervals.push(RawSettlementLayerInterval {
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
        intervals.push(RawSettlementLayerInterval {
            settlement_layer: IntervalSettlementLayer::Gateway(sl_chain_id),
            first_batch: cursor,
            last_batch: Some(from_batch),
        });
        cursor = from_batch + 1;
    }
    if !on_active_gw {
        intervals.push(RawSettlementLayerInterval {
            settlement_layer: IntervalSettlementLayer::L1,
            first_batch: cursor,
            last_batch: None,
        });
    }
    Ok(intervals)
}
