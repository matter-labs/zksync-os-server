//! [`EventSink`] implementations that let `l1_watcher` push decoded items into the subpools
//! without depending on this crate. Each impl simply delegates to the subpool's own insert
//! logic.
//!
//! [`InteropFeeSink`] follows the same inversion so `interop_fee_updater` can feed the
//! [`InteropFeeSubpool`] without depending on this crate.

use crate::subpools::interop_fee::InteropFeeSubpool;
use crate::subpools::interop_roots::InteropRootsSubpool;
use crate::subpools::l1::L1Subpool;
use crate::subpools::sl_chain_id::SlChainIdSubpool;
use crate::subpools::upgrade::UpgradeSubpool;
use alloy::primitives::U256;
use async_trait::async_trait;
use std::sync::Arc;
use zksync_os_interop_fee_updater::InteropFeeSink;
use zksync_os_l1_watcher::EventSink;
use zksync_os_types::{IndexedInteropRoot, L1PriorityEnvelope, SystemTxEnvelope, UpgradeInfo};

#[async_trait]
impl EventSink<Arc<L1PriorityEnvelope>> for L1Subpool {
    async fn push(&mut self, item: Arc<L1PriorityEnvelope>) {
        self.insert(item).await
    }
}

#[async_trait]
impl EventSink<UpgradeInfo> for UpgradeSubpool {
    async fn push(&mut self, item: UpgradeInfo) {
        self.insert(item).await
    }
}

#[async_trait]
impl EventSink<SystemTxEnvelope> for SlChainIdSubpool {
    async fn push(&mut self, item: SystemTxEnvelope) {
        self.insert(item).await
    }
}

#[async_trait]
impl EventSink<IndexedInteropRoot> for InteropRootsSubpool {
    async fn push(&mut self, item: IndexedInteropRoot) {
        self.add_root(item).await
    }
}

#[async_trait]
impl InteropFeeSink for InteropFeeSubpool {
    async fn insert(&self, interop_fee: U256) {
        InteropFeeSubpool::insert(self, interop_fee).await
    }
}
