//! Validity rules for leader-proposed blocks, checked before voting.
//!
//! Full re-execution (the environment's other verification half) proves a block's
//! *outcome* matches its declared state commitment — but it re-executes whatever inputs
//! the leader chose. These rules bound the inputs themselves, so a byzantine leader
//! cannot smuggle content that executes fine and is still wrong for the chain:
//! fabricated "L1" transactions, invented upgrades, drifting fees, far-future
//! timestamps, or altered chain constants.
//!
//! Verdicts are three-way. [`Verdict::Invalid`] is permanent: no honest chain state
//! makes the block acceptable. [`Verdict::Withhold`] means "I cannot vouch for this
//! *yet*" — typically an L1 input this validator's own watcher has not seen. Both
//! outcomes withhold the vote for the current round only (consensus re-verifies on
//! re-proposal), so a leader whose inputs outrun the committee's L1 view costs the
//! chain a view timeout, never a halt; the distinction exists for observability and
//! for operators triaging "attack" vs "my node lags L1".

use crate::builder::derive_next_cursors;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_sequencer::execution::{FeeConfig, FeeParams, fee_params_within_protocol_bounds};
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{
    BlockStartCursors, ExecutionVersion, L1PriorityEnvelope, L1TxSerialId, L1UpgradeEnvelope,
    ProtocolSemanticVersion, SystemTxType, UpgradeInfo, ZkEnvelope,
};

/// The outcome of checking one proposal against these rules.
#[derive(Debug)]
pub enum Verdict {
    Valid,
    /// Cannot be validated against this node's current knowledge (e.g. its L1 watcher
    /// has not seen a referenced transaction yet). Withhold the vote; a later round
    /// re-verifies from scratch.
    Withhold(String),
    /// No future knowledge can make this block valid.
    Invalid(String),
}

/// What the rules need to know about the parent block (the last agreed-on state the
/// proposal claims to extend).
pub struct ParentView {
    pub timestamp: u64,
    pub protocol_version: ProtocolSemanticVersion,
    pub next_cursors: BlockStartCursors,
    pub fee_params: FeeParams,
}

/// Chain constants every committee member is expected to run with. A proposal that
/// contradicts them is invalid — committees must be configured uniformly (documented
/// deployment invariant), so a mismatch means a lying leader, not a tuning difference.
pub struct ValidityConfig {
    /// How far a proposed timestamp may lead this validator's clock. Bounds what a
    /// leader can pre-date contracts and upgrade activations into; generous enough to
    /// absorb honest clock skew.
    pub max_timestamp_skew: Duration,
    pub chain_id: u64,
    /// The settlement layer's chain id — what the one-time `SetSLChainId` system
    /// transaction at the v31 boundary must carry. Wired from the same source as the
    /// builder's (`l1_chain_id`; the settlement layer is always L1 today).
    pub sl_chain_id: u64,
    pub fee_collector_address: alloy::primitives::Address,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    /// Upper bound on the number of transactions in one block. Wired from the same
    /// configuration the block builder reads, so an honest leader never exceeds it.
    pub max_transactions: usize,
    /// Upper bound on the encoded size of a block's replay record, in bytes. Wired
    /// from the committee network's message-size limit: a record that large could not
    /// have traveled the wire honestly anyway, so this is defense in depth — it turns
    /// "silently undeliverable" into an explicit verdict and stays load-bearing if
    /// the transport limit is ever raised for other reasons.
    pub max_encoded_record_size: usize,
    /// Fee production config; the fee rules accept exactly the values an honest
    /// producer could emit under it.
    pub fee: FeeConfig,
}

/// This validator's own view of L1-sourced inputs, fed by its own L1 watcher. The
/// authenticity oracle: a leader-proposed L1 input is accepted only if it matches
/// what this node saw on L1 itself.
#[async_trait]
pub trait LocalL1Inputs: Send + Sync {
    /// The locally-watched priority transaction with this serial id (if any), plus
    /// the highest id seen so far (to tell "not seen yet" from divergence).
    async fn seen_priority_tx(
        &self,
        id: L1TxSerialId,
    ) -> (Option<Arc<L1PriorityEnvelope>>, Option<L1TxSerialId>);

    /// The locally-watched upgrade targeting this protocol version, if any.
    async fn seen_upgrade(&self, version: &ProtocolSemanticVersion) -> Option<UpgradeInfo>;
}

#[async_trait]
impl LocalL1Inputs for zksync_os_mempool::L1InputsView {
    async fn seen_priority_tx(
        &self,
        id: L1TxSerialId,
    ) -> (Option<Arc<L1PriorityEnvelope>>, Option<L1TxSerialId>) {
        self.seen_priority_tx(id).await
    }

    async fn seen_upgrade(&self, version: &ProtocolSemanticVersion) -> Option<UpgradeInfo> {
        self.seen_upgrade(version).await
    }
}

macro_rules! invalid {
    ($($arg:tt)*) => { return Verdict::Invalid(format!($($arg)*)) };
}
macro_rules! withhold {
    ($($arg:tt)*) => { return Verdict::Withhold(format!($($arg)*)) };
}

/// Checks a proposed record against its parent, this validator's L1 view, and the
/// committee's chain constants. Cheap structural rules run first; L1 lookups last.
///
/// `encoded_record_size` is the byte length of the record's wire encoding — callers
/// have it at hand (the record arrived encoded), so it is passed in rather than
/// re-serialized here.
pub async fn check_proposal(
    parent: &ParentView,
    record: &ReplayRecord,
    encoded_record_size: usize,
    now_epoch_seconds: u64,
    inputs: &dyn LocalL1Inputs,
    config: &ValidityConfig,
) -> Verdict {
    let context = &record.block_context;

    // Block size: both bounds are committee constants an honest builder stays under,
    // so exceeding either means a lying leader. Checked before anything else — an
    // oversized block earns no further work.
    if record.transactions.len() > config.max_transactions {
        invalid!(
            "{} transactions exceed the per-block cap of {}",
            record.transactions.len(),
            config.max_transactions
        );
    }
    if encoded_record_size > config.max_encoded_record_size {
        invalid!(
            "encoded record of {encoded_record_size} bytes exceeds the cap of {} bytes",
            config.max_encoded_record_size
        );
    }

    // Chain constants: fixed for every block by committee configuration (and by what
    // the v1 builder emits). The fee collector matters economically — a leader must
    // not redirect fees to itself.
    if context.chain_id != config.chain_id {
        invalid!("chain id {} != {}", context.chain_id, config.chain_id);
    }
    if context.coinbase != config.fee_collector_address {
        invalid!(
            "fee collector {} != {}",
            context.coinbase,
            config.fee_collector_address
        );
    }
    if context.gas_limit != config.gas_limit {
        invalid!("gas limit {} != {}", context.gas_limit, config.gas_limit);
    }
    if context.pubdata_limit != config.pubdata_limit {
        invalid!(
            "pubdata limit {} != {}",
            context.pubdata_limit,
            config.pubdata_limit
        );
    }
    if context.mix_hash != alloy::primitives::U256::ZERO {
        invalid!("nonzero mix hash");
    }
    if context.blob_fee != alloy::primitives::U256::ONE {
        invalid!("blob fee {} != 1", context.blob_fee);
    }
    let expected_execution_version: ExecutionVersion = match (&record.protocol_version).try_into() {
        Ok(version) => version,
        Err(_) => invalid!("unsupported protocol version {}", record.protocol_version),
    };
    if context.execution_version != expected_execution_version as u32 {
        invalid!(
            "execution version {} does not correspond to protocol version {}",
            context.execution_version,
            record.protocol_version
        );
    }

    // Timestamps: never behind the parent, and not ahead of this validator's clock by
    // more than the allowed skew. Monotonicity is non-strict — at sub-second block
    // cadence several blocks share a second, and demanding a strict increase would
    // make chain time outrun the wall clock straight into the skew bound. (A verdict
    // only withholds this round's vote, so a proposal rejected purely for clock skew
    // self-heals on re-proposal once clocks catch up.)
    if context.timestamp < parent.timestamp {
        invalid!(
            "timestamp {} regresses behind the parent's {}",
            context.timestamp,
            parent.timestamp
        );
    }
    let max_timestamp = now_epoch_seconds.saturating_add(config.max_timestamp_skew.as_secs());
    if context.timestamp > max_timestamp {
        // Withhold, not invalid: the verdict is inherently round-scoped — it
        // compares against *this validator's clock right now*, and self-heals
        // on re-proposal once clocks catch up. Classifying it invalid would
        // fire the byzantine/divergence alarm (`verify_verdicts{invalid}`,
        // which operations treat as never-fires-on-honest-committees) for
        // plain NTP drift on either side.
        withhold!(
            "timestamp {} is further than {:?} ahead of this validator's clock ({now_epoch_seconds})",
            context.timestamp,
            config.max_timestamp_skew
        );
    }

    // L1-source cursors: a block consumes L1 inputs exactly from where its parent
    // stopped. (Where each cursor may advance *to* is enforced per input below and by
    // re-execution; interop cursors cannot move at all until interop is supported
    // under consensus.)
    if record.starting_cursors != parent.next_cursors {
        invalid!(
            "starting cursors {:?} do not continue the parent's {:?}",
            record.starting_cursors,
            parent.next_cursors
        );
    }

    // System transactions: only what the builder emits may ride consensus, because a
    // system transaction executes fine regardless of provenance — re-execution alone
    // would accept whatever a leader injected. The v1 builder emits exactly one kind:
    // the one-time `SetSLChainId` placeholder in the first v31 block. Everything else
    // is rejected until it has an authenticity rule of its own.
    //
    // TODO(consensus): interop support — authenticate `ImportInteropRoots` (and the
    // interop fee, once it is an L1-authenticated or committee-derivable input) the
    // way L1 priority transactions are: a local watcher-backed oracle
    // (`LocalL1Inputs`-style), contiguous-cursor advancement in the builder's
    // `derive_next_cursors`, matching checks here, and finalized-boundary ingestion.
    // Until then the cursor-equality rule above keeps interop cursors frozen and this
    // rule keeps the transactions out.
    //
    // TODO(consensus): live settlement migrations — a real (non-placeholder)
    // `SetSLChainId` must only be accepted against an L1-observed migration event;
    // rejected outright until that rule exists.
    let v31_upgrade_boundary =
        record.protocol_version.minor == 31 && parent.protocol_version.minor < 31;
    // The genesis-shaped boundary: block 1 of a chain born on v31. The builder emits
    // the system transaction there too, but its *absence* is tolerated (unlike on the
    // upgrade boundary) — nothing consumes the field yet, and requiring it would
    // reject hand-assembled fixture chains for no security gain. Injection, not
    // omission, is the dangerous direction.
    let sl_chain_id_boundary =
        v31_upgrade_boundary || (record.protocol_version.minor == 31 && context.block_number == 1);
    let mut sl_chain_id_txs = 0usize;
    for tx in &record.transactions {
        let Some(subtype) = tx.as_system_tx_type() else {
            continue;
        };
        match subtype {
            SystemTxType::SetSLChainId(chain_id, migration_number) => {
                if !sl_chain_id_boundary {
                    invalid!("SetSLChainId system transaction outside the v31 activation block");
                }
                if *chain_id != config.sl_chain_id {
                    invalid!(
                        "SetSLChainId targets chain id {chain_id} where the settlement layer \
                         is {}",
                        config.sl_chain_id
                    );
                }
                if *migration_number != u64::MAX {
                    invalid!(
                        "SetSLChainId carries migration number {migration_number}; live \
                         settlement migrations are not supported under consensus"
                    );
                }
                sl_chain_id_txs += 1;
                if sl_chain_id_txs > 1 {
                    invalid!("duplicate SetSLChainId system transaction");
                }
            }
            SystemTxType::ImportInteropRoots(_) => {
                invalid!("interop root imports are not yet supported under consensus");
            }
            SystemTxType::SetInteropFee(_) => {
                invalid!("interop fee updates are not yet supported under consensus");
            }
        }
    }
    if v31_upgrade_boundary && sl_chain_id_txs == 0 {
        invalid!("the v31 upgrade block must carry the SetSLChainId system transaction");
    }

    // Fees: the values an honest fee provider could produce on top of the parent —
    // overrides pinned, per-block movement clamped, basefee derived. Leaders track
    // the fee market; verifiers only bound them, so validators' fee oracles never
    // have to agree exactly.
    if let Err(reason) = fee_params_within_protocol_bounds(
        &parent.fee_params,
        &FeeParams {
            eip1559_basefee: context.eip1559_basefee,
            native_price: context.native_price,
            pubdata_price: context.pubdata_price,
        },
        &config.fee,
    ) {
        invalid!("{reason}");
    }

    // Upgrades: a version change (or an included upgrade transaction) is legitimate
    // only if this validator saw the same upgrade on L1 itself.
    let upgrade_txs: Vec<(usize, &L1UpgradeEnvelope)> = record
        .transactions
        .iter()
        .enumerate()
        .filter_map(|(index, tx)| match tx.envelope() {
            ZkEnvelope::Upgrade(upgrade) => Some((index, upgrade)),
            _ => None,
        })
        .collect();
    match check_upgrade(parent, record, &upgrade_txs, inputs).await {
        Verdict::Valid => {}
        other => return other,
    }

    // L1 priority transactions: contiguous ids from the parent's cursor, each
    // matching — byte for byte — what this validator's own L1 watcher saw.
    let mut expected_id = parent.next_cursors.l1_priority_id;
    for tx in &record.transactions {
        let ZkEnvelope::L1(proposed) = tx.envelope() else {
            continue;
        };
        let id = proposed.priority_id();
        if id != expected_id {
            invalid!("L1 transaction has priority id {id} where {expected_id} was expected");
        }
        match inputs.seen_priority_tx(id).await {
            (Some(local), _) => {
                if proposed != local.as_ref() {
                    invalid!(
                        "L1 transaction {id} does not match the transaction watched on L1 \
                         (local hash {}, proposed hash {})",
                        local.hash(),
                        proposed.hash()
                    );
                }
            }
            (None, watermark) => {
                // Not an error: this validator's L1 watcher may simply be behind the
                // leader's. (An id below the watermark yet missing locally should be
                // impossible — the watcher emits sequentially — so it is treated the
                // same conservative way rather than trusted.)
                withhold!(
                    "L1 transaction {id} not seen by the local L1 watcher yet (seen up to {watermark:?})"
                );
            }
        }
        expected_id += 1;
    }

    Verdict::Valid
}

/// The upgrade-related rules: version transitions and upgrade-transaction content.
///
/// Mirrors what the builder emits: a version bump carries the upgrade transaction and
/// the upgrade's force preimages; a patch upgrade bumps the version with no
/// transaction; the one equal-version case is a not-yet-applied upgrade at the chain's
/// current version (a fresh chain's genesis upgrade), which carries the transaction
/// but applies no metadata.
async fn check_upgrade(
    parent: &ParentView,
    record: &ReplayRecord,
    upgrade_txs: &[(usize, &L1UpgradeEnvelope)],
    inputs: &dyn LocalL1Inputs,
) -> Verdict {
    let version = &record.protocol_version;
    if upgrade_txs.len() > 1 {
        invalid!("more than one upgrade transaction in a block");
    }
    let proposed_upgrade = upgrade_txs.first();
    if let Some((index, _)) = proposed_upgrade
        && *index != 0
    {
        invalid!("upgrade transaction is not the first transaction of its block");
    }

    let version_changed = version != &parent.protocol_version;
    if version < &parent.protocol_version {
        invalid!(
            "protocol version regresses from {} to {version}",
            parent.protocol_version
        );
    }
    if !version_changed && proposed_upgrade.is_none() {
        // No upgrade involvement; nothing may ride along.
        if !record.force_preimages.is_empty() {
            invalid!("force preimages present without an upgrade");
        }
        return Verdict::Valid;
    }

    // Any upgrade involvement must correspond to an upgrade this validator saw on L1.
    let Some(local) = inputs.seen_upgrade(version).await else {
        withhold!("upgrade to {version} not seen by the local upgrade watcher yet");
    };
    if local.metadata.timestamp > record.block_context.timestamp.saturating_add(5) {
        invalid!(
            "upgrade to {version} activates at {} — after this block's timestamp {}",
            local.metadata.timestamp,
            record.block_context.timestamp
        );
    }
    match (proposed_upgrade, &local.tx) {
        (Some((_, proposed)), Some(local_tx)) => {
            if *proposed != local_tx {
                invalid!(
                    "upgrade transaction for {version} does not match the one watched on L1 \
                     (local hash {}, proposed hash {})",
                    local_tx.hash(),
                    proposed.hash()
                );
            }
        }
        (Some(_), None) => invalid!("upgrade transaction present but {version} is a patch upgrade"),
        (None, Some(_)) => invalid!("upgrade to {version} is missing its upgrade transaction"),
        (None, None) => {}
    }

    let expected_preimages: &[(alloy::primitives::B256, Vec<u8>)] = if version_changed {
        // The upgrade's metadata applies with the version bump.
        &local.metadata.force_preimages
    } else {
        // The equal-version (genesis) shape: the transaction executes, the metadata
        // does not apply.
        &[]
    };
    if record.force_preimages != expected_preimages {
        invalid!("force preimages do not match the upgrade's");
    }

    Verdict::Valid
}

/// Builds the parent view for the fee/cursor/timestamp rules out of a parent record.
pub fn parent_view_of_record(record: &ReplayRecord) -> ParentView {
    ParentView {
        timestamp: record.block_context.timestamp,
        protocol_version: record.protocol_version.clone(),
        next_cursors: derive_next_cursors(record),
        fee_params: FeeParams {
            eip1559_basefee: record.block_context.eip1559_basefee,
            native_price: record.block_context.native_price,
            pubdata_price: record.block_context.pubdata_price,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, Bytes, U256};
    use std::collections::BTreeMap;
    use zksync_os_types::{L1Envelope, L1Tx, UpgradeMetadata, ZkTransaction};

    /// Local L1 knowledge for tests: whatever the maps say this validator saw.
    #[derive(Default)]
    struct StubInputs {
        priority_txs: BTreeMap<L1TxSerialId, Arc<L1PriorityEnvelope>>,
        upgrades: BTreeMap<ProtocolSemanticVersion, UpgradeInfo>,
    }

    #[async_trait]
    impl LocalL1Inputs for StubInputs {
        async fn seen_priority_tx(
            &self,
            id: L1TxSerialId,
        ) -> (Option<Arc<L1PriorityEnvelope>>, Option<L1TxSerialId>) {
            (
                self.priority_txs.get(&id).cloned(),
                self.priority_txs.keys().max().copied(),
            )
        }

        async fn seen_upgrade(&self, version: &ProtocolSemanticVersion) -> Option<UpgradeInfo> {
            self.upgrades.get(version).cloned()
        }
    }

    fn version(v: &str) -> ProtocolSemanticVersion {
        v.parse().expect("valid version")
    }

    fn priority_tx(id: u64, value: u64) -> L1PriorityEnvelope {
        L1Envelope {
            inner: L1Tx {
                hash: B256::repeat_byte(id as u8 + 1),
                initiator: Address::repeat_byte(1),
                to: Address::repeat_byte(2),
                gas_limit: 500_000,
                gas_per_pubdata_byte_limit: 800,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                nonce: id,
                value: U256::from(value),
                to_mint: U256::from(value),
                refund_recipient: Address::repeat_byte(1),
                input: Bytes::new(),
                factory_deps: Vec::new(),
                marker: std::marker::PhantomData,
            },
        }
    }

    fn upgrade_tx(target: &ProtocolSemanticVersion) -> L1UpgradeEnvelope {
        L1Envelope {
            inner: L1Tx {
                hash: B256::repeat_byte(0xAA),
                initiator: Address::repeat_byte(3),
                to: Address::repeat_byte(4),
                gas_limit: 72_000_000,
                gas_per_pubdata_byte_limit: 800,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                nonce: target.minor,
                value: U256::ZERO,
                to_mint: U256::ZERO,
                refund_recipient: Address::repeat_byte(3),
                input: Bytes::new(),
                factory_deps: Vec::new(),
                marker: std::marker::PhantomData,
            },
        }
    }

    const PARENT_TIMESTAMP: u64 = 1_000;
    const NOW: u64 = 1_001;

    fn parent() -> ParentView {
        ParentView {
            timestamp: PARENT_TIMESTAMP,
            protocol_version: version("0.31.0"),
            next_cursors: BlockStartCursors::default(),
            fee_params: FeeParams {
                eip1559_basefee: U256::from(1_000),
                native_price: U256::from(1_000),
                pubdata_price: U256::from(100),
            },
        }
    }

    /// Encoded-record size passed for tests that are not about the size rule; well
    /// under `config()`'s cap.
    const HONEST_ENCODED_SIZE: usize = 1_000;

    /// The settlement layer chain id `config()` pins (distinct from the L2 chain id).
    const SL_CHAIN_ID: u64 = 900;

    fn config() -> ValidityConfig {
        ValidityConfig {
            max_timestamp_skew: Duration::from_secs(10),
            chain_id: 270,
            sl_chain_id: SL_CHAIN_ID,
            fee_collector_address: Address::repeat_byte(9),
            gas_limit: 100_000_000,
            pubdata_limit: 110_000,
            max_transactions: 100,
            max_encoded_record_size: 10_000,
            fee: FeeConfig {
                native_price_usd: num::rational::Ratio::from_integer(1u32.into()),
                base_fee_override: None,
                native_per_gas: 1,
                pubdata_price_override: None,
                pubdata_price_cap: None,
                native_price_override: None,
            },
        }
    }

    /// A record that passes every rule against `parent()` + `config()`; tests tamper
    /// with one aspect each.
    fn valid_record(transactions: Vec<ZkTransaction>) -> ReplayRecord {
        let protocol_version = version("0.31.0");
        let execution_version: ExecutionVersion =
            (&protocol_version).try_into().expect("supported version");
        ReplayRecord {
            block_context: zksync_os_storage_api::BlockContext {
                chain_id: 270,
                block_number: 5,
                block_hashes: Default::default(),
                timestamp: PARENT_TIMESTAMP + 1,
                execution_version: execution_version as u32,
                gas_limit: 100_000_000,
                pubdata_limit: 110_000,
                coinbase: Address::repeat_byte(9),
                eip1559_basefee: U256::from(1_000),
                native_price: U256::from(1_000),
                pubdata_price: U256::from(100),
                blob_fee: U256::ONE,
                mix_hash: U256::ZERO,
            },
            transactions,
            previous_block_timestamp: PARENT_TIMESTAMP,
            node_version: "0.0.0".parse().expect("valid semver"),
            protocol_version,
            block_output_hash: B256::ZERO,
            force_preimages: Vec::new(),
            starting_cursors: BlockStartCursors::default(),
        }
    }

    async fn check(record: &ReplayRecord, inputs: &StubInputs) -> Verdict {
        check_proposal(
            &parent(),
            record,
            HONEST_ENCODED_SIZE,
            NOW,
            inputs,
            &config(),
        )
        .await
    }

    macro_rules! assert_verdict {
        ($verdict:expr, $pattern:pat) => {
            let verdict = $verdict;
            assert!(
                matches!(verdict, $pattern),
                "unexpected verdict: {verdict:?}"
            );
        };
    }

    #[tokio::test]
    async fn interop_system_transactions_are_rejected() {
        use zksync_os_types::SystemTxEnvelope;
        let roots = valid_record(vec![ZkTransaction::from(
            SystemTxEnvelope::import_interop_roots(Vec::new(), 0),
        )]);
        assert_verdict!(
            check(&roots, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        let fee = valid_record(vec![ZkTransaction::from(
            SystemTxEnvelope::set_interop_fee(U256::from(1), 0),
        )]);
        assert_verdict!(
            check(&fee, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );
    }

    #[tokio::test]
    async fn sl_chain_id_tx_is_pinned_to_the_v31_boundary() {
        use zksync_os_types::SystemTxEnvelope;
        let sl_tx =
            || ZkTransaction::from(SystemTxEnvelope::set_sl_chain_id(SL_CHAIN_ID, u64::MAX));

        // Outside any boundary (`valid_record` is block 5 with a v31 parent): invalid.
        let outside = valid_record(vec![sl_tx()]);
        assert_verdict!(
            check(&outside, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        // At the genesis-shaped boundary (block 1 of a v31 chain): correct content
        // passes, and its absence is tolerated there.
        let mut at_genesis = valid_record(vec![sl_tx()]);
        at_genesis.block_context.block_number = 1;
        assert_verdict!(
            check(&at_genesis, &StubInputs::default()).await,
            Verdict::Valid
        );
        let mut absent = valid_record(Vec::new());
        absent.block_context.block_number = 1;
        assert_verdict!(check(&absent, &StubInputs::default()).await, Verdict::Valid);

        // Wrong settlement chain id, a real (non-placeholder) migration number, and
        // duplicates: all invalid even at the boundary.
        let mut wrong_chain = valid_record(vec![ZkTransaction::from(
            SystemTxEnvelope::set_sl_chain_id(SL_CHAIN_ID + 1, u64::MAX),
        )]);
        wrong_chain.block_context.block_number = 1;
        assert_verdict!(
            check(&wrong_chain, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );
        let mut real_migration = valid_record(vec![ZkTransaction::from(
            SystemTxEnvelope::set_sl_chain_id(SL_CHAIN_ID, 3),
        )]);
        real_migration.block_context.block_number = 1;
        assert_verdict!(
            check(&real_migration, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );
        let mut duplicated = valid_record(vec![sl_tx(), sl_tx()]);
        duplicated.block_context.block_number = 1;
        assert_verdict!(
            check(&duplicated, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );
    }

    #[tokio::test]
    async fn v31_upgrade_block_requires_the_sl_chain_id_tx() {
        let mut parent = parent();
        parent.protocol_version = version("0.30.0");
        let record = valid_record(Vec::new());
        let verdict = check_proposal(
            &parent,
            &record,
            HONEST_ENCODED_SIZE,
            NOW,
            &StubInputs::default(),
            &config(),
        )
        .await;
        assert_verdict!(verdict, Verdict::Invalid(_));
    }

    #[tokio::test]
    async fn empty_block_passes() {
        assert_verdict!(
            check(&valid_record(Vec::new()), &StubInputs::default()).await,
            Verdict::Valid
        );
    }

    #[tokio::test]
    async fn chain_constants_are_pinned() {
        type Tamper = Box<dyn Fn(&mut ReplayRecord)>;
        let tamperings: Vec<Tamper> = vec![
            Box::new(|record| record.block_context.chain_id = 271),
            Box::new(|record| record.block_context.coinbase = Address::repeat_byte(7)),
            Box::new(|record| record.block_context.gas_limit += 1),
            Box::new(|record| record.block_context.pubdata_limit += 1),
            Box::new(|record| record.block_context.mix_hash = U256::from(1)),
            Box::new(|record| record.block_context.blob_fee = U256::from(2)),
            Box::new(|record| record.block_context.execution_version += 1),
        ];
        for tamper in tamperings {
            let mut record = valid_record(Vec::new());
            tamper(&mut record);
            assert_verdict!(
                check(&record, &StubInputs::default()).await,
                Verdict::Invalid(_)
            );
        }
    }

    #[tokio::test]
    async fn oversized_blocks_are_rejected() {
        // Two authentic, contiguous L1 transactions — a record that passes every
        // other rule, so the caps are the only thing under test here.
        let mut inputs = StubInputs::default();
        inputs.priority_txs.insert(0, Arc::new(priority_tx(0, 100)));
        inputs.priority_txs.insert(1, Arc::new(priority_tx(1, 100)));
        let record = valid_record(vec![
            ZkTransaction::from(priority_tx(0, 100)),
            ZkTransaction::from(priority_tx(1, 100)),
        ]);

        // At both caps exactly: still valid.
        let mut config = config();
        config.max_transactions = 2;
        assert_verdict!(
            check_proposal(
                &parent(),
                &record,
                config.max_encoded_record_size,
                NOW,
                &inputs,
                &config
            )
            .await,
            Verdict::Valid
        );

        // One transaction over the cap: invalid, even though every transaction is
        // individually authentic.
        config.max_transactions = 1;
        assert_verdict!(
            check_proposal(
                &parent(),
                &record,
                HONEST_ENCODED_SIZE,
                NOW,
                &inputs,
                &config
            )
            .await,
            Verdict::Invalid(_)
        );

        // One byte over the size cap: invalid.
        config.max_transactions = 2;
        assert_verdict!(
            check_proposal(
                &parent(),
                &record,
                config.max_encoded_record_size + 1,
                NOW,
                &inputs,
                &config
            )
            .await,
            Verdict::Invalid(_)
        );
    }

    #[tokio::test]
    async fn timestamps_never_regress_and_never_outrun_the_clock() {
        // Behind the parent: invalid.
        let mut record = valid_record(Vec::new());
        record.block_context.timestamp = PARENT_TIMESTAMP - 1;
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        // Equal to the parent: routine at sub-second block cadence.
        let mut record = valid_record(Vec::new());
        record.block_context.timestamp = PARENT_TIMESTAMP;
        assert_verdict!(check(&record, &StubInputs::default()).await, Verdict::Valid);

        // Beyond the verifier's clock plus skew: *withhold*, not invalid — the
        // comparison is against this validator's clock right now and self-heals
        // on re-proposal; classifying it invalid would fire the byzantine/
        // divergence alarm for plain NTP drift.
        let mut record = valid_record(Vec::new());
        record.block_context.timestamp = NOW + 11; // skew allowance is 10s
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Withhold(_)
        );

        let mut record = valid_record(Vec::new());
        record.block_context.timestamp = NOW + 9;
        assert_verdict!(check(&record, &StubInputs::default()).await, Verdict::Valid);

        // Exactly at the skew bound: still valid — the rule rejects strictly
        // *beyond* the allowance, so the bound itself belongs to the leader.
        let mut record = valid_record(Vec::new());
        record.block_context.timestamp = NOW + 10;
        assert_verdict!(check(&record, &StubInputs::default()).await, Verdict::Valid);
    }

    #[tokio::test]
    async fn cursors_continue_the_parent() {
        let mut record = valid_record(Vec::new());
        record.starting_cursors.l1_priority_id = 3;
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );
    }

    #[tokio::test]
    async fn fees_move_within_protocol_bounds() {
        // Native price beyond the ±12.5% clamp.
        let mut record = valid_record(Vec::new());
        record.block_context.native_price = U256::from(1_126);
        record.block_context.eip1559_basefee = U256::from(1_126);
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        // Within the clamp, basefee derived: fine.
        let mut record = valid_record(Vec::new());
        record.block_context.native_price = U256::from(1_125);
        record.block_context.eip1559_basefee = U256::from(1_125);
        assert_verdict!(check(&record, &StubInputs::default()).await, Verdict::Valid);

        // Basefee not derived from the native price.
        let mut record = valid_record(Vec::new());
        record.block_context.eip1559_basefee = U256::from(999);
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        // Pubdata price rising faster than 1.5x.
        let mut record = valid_record(Vec::new());
        record.block_context.pubdata_price = U256::from(151);
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        // Pubdata price falling freely: fine.
        let mut record = valid_record(Vec::new());
        record.block_context.pubdata_price = U256::from(1);
        assert_verdict!(check(&record, &StubInputs::default()).await, Verdict::Valid);
    }

    #[tokio::test]
    async fn l1_transactions_match_the_local_watcher() {
        let seen = priority_tx(0, 100);
        let mut inputs = StubInputs::default();
        inputs.priority_txs.insert(0, Arc::new(seen.clone()));

        // The exact transaction this validator watched: fine.
        let record = valid_record(vec![ZkTransaction::from(seen.clone())]);
        assert_verdict!(check(&record, &inputs).await, Verdict::Valid);

        // Same id, different content: fabricated.
        let record = valid_record(vec![ZkTransaction::from(priority_tx(0, 999_999))]);
        assert_verdict!(check(&record, &inputs).await, Verdict::Invalid(_));

        // Not seen locally at all: withhold, not reject.
        let record = valid_record(vec![ZkTransaction::from(priority_tx(1, 100))]);
        let empty = StubInputs::default();
        assert_verdict!(check(&record, &empty).await, Verdict::Invalid(_)); // id 1 != cursor 0

        let record = valid_record(vec![ZkTransaction::from(priority_tx(0, 100))]);
        assert_verdict!(check(&record, &empty).await, Verdict::Withhold(_));

        // Two transactions, only the first known locally: withhold on the second.
        let mut record = valid_record(vec![
            ZkTransaction::from(seen),
            ZkTransaction::from(priority_tx(1, 100)),
        ]);
        record.block_context.timestamp = PARENT_TIMESTAMP + 1;
        assert_verdict!(check(&record, &inputs).await, Verdict::Withhold(_));
    }

    #[tokio::test]
    async fn l1_transaction_ids_are_contiguous() {
        let mut inputs = StubInputs::default();
        inputs.priority_txs.insert(0, Arc::new(priority_tx(0, 100)));
        inputs.priority_txs.insert(2, Arc::new(priority_tx(2, 100)));

        // A gap in the ids (0 then 2) is invalid regardless of authenticity.
        let record = valid_record(vec![
            ZkTransaction::from(priority_tx(0, 100)),
            ZkTransaction::from(priority_tx(2, 100)),
        ]);
        assert_verdict!(check(&record, &inputs).await, Verdict::Invalid(_));
    }

    fn upgrade_info(target: &str, tx: bool, preimages: Vec<(B256, Vec<u8>)>) -> UpgradeInfo {
        let target = version(target);
        UpgradeInfo {
            tx: tx.then(|| upgrade_tx(&target)),
            metadata: UpgradeMetadata {
                timestamp: 0,
                protocol_version: target,
                force_preimages: preimages,
            },
        }
    }

    #[tokio::test]
    async fn a_same_version_upgrade_tx_still_requires_the_watched_upgrade() {
        // No version bump, but an upgrade transaction rides along. That is
        // upgrade involvement (the genesis shape), never a free pass: it must
        // match an upgrade the local watcher saw, so unseen means withhold.
        let record = valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.31.0")))]);
        assert_verdict!(
            check(&record, &StubInputs::default()).await,
            Verdict::Withhold(_)
        );
    }

    #[tokio::test]
    async fn upgrade_activation_may_lead_the_block_by_the_grace_bound_only() {
        let block_timestamp = PARENT_TIMESTAMP + 1;
        let honest = |activation: u64| {
            let target = version("0.32.0");
            let mut inputs = StubInputs::default();
            let mut info = upgrade_info("0.32.0", true, Vec::new());
            info.metadata.timestamp = activation;
            inputs.upgrades.insert(target.clone(), info);
            let mut record = valid_record(vec![ZkTransaction::from(upgrade_tx(&target))]);
            record.protocol_version = target;
            (record, inputs)
        };

        // Activation exactly at the +5s grace bound belongs to the block…
        let (record, inputs) = honest(block_timestamp + 5);
        assert_verdict!(check(&record, &inputs).await, Verdict::Valid);

        // …strictly beyond it does not.
        let (record, inputs) = honest(block_timestamp + 6);
        assert_verdict!(check(&record, &inputs).await, Verdict::Invalid(_));
    }

    #[tokio::test]
    async fn version_bumps_require_the_locally_watched_upgrade() {
        let preimages = vec![(B256::repeat_byte(5), vec![1, 2, 3])];
        let mut inputs = StubInputs::default();
        inputs.upgrades.insert(
            version("0.32.0"),
            upgrade_info("0.32.0", true, preimages.clone()),
        );

        // The honest shape: bump + matching tx + the upgrade's preimages.
        let mut record = valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.32.0")))]);
        record.protocol_version = version("0.32.0");
        record.force_preimages = preimages.clone();
        assert_verdict!(check(&record, &inputs).await, Verdict::Valid);

        // Upgrade this validator has not seen: withhold.
        let mut unseen = valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.32.0")))]);
        unseen.protocol_version = version("0.32.0");
        unseen.force_preimages = preimages.clone();
        assert_verdict!(
            check(&unseen, &StubInputs::default()).await,
            Verdict::Withhold(_)
        );

        // Bump without the upgrade transaction the local watcher expects: invalid.
        let mut missing_tx = valid_record(Vec::new());
        missing_tx.protocol_version = version("0.32.0");
        missing_tx.force_preimages = preimages.clone();
        assert_verdict!(check(&missing_tx, &inputs).await, Verdict::Invalid(_));

        // Bump with a different transaction than watched: invalid.
        let mut wrong_tx = valid_record(vec![ZkTransaction::from({
            let mut tx = upgrade_tx(&version("0.32.0"));
            tx.inner.to = Address::repeat_byte(0xEE);
            tx
        })]);
        wrong_tx.protocol_version = version("0.32.0");
        wrong_tx.force_preimages = preimages.clone();
        assert_verdict!(check(&wrong_tx, &inputs).await, Verdict::Invalid(_));

        // Wrong preimages: invalid.
        let mut wrong_preimages =
            valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.32.0")))]);
        wrong_preimages.protocol_version = version("0.32.0");
        assert_verdict!(check(&wrong_preimages, &inputs).await, Verdict::Invalid(_));

        // Version regression: invalid.
        let mut regression = valid_record(Vec::new());
        regression.protocol_version = version("0.30.0");
        assert_verdict!(
            check(&regression, &StubInputs::default()).await,
            Verdict::Invalid(_)
        );

        // Preimages without any upgrade: invalid.
        let mut stray = valid_record(Vec::new());
        stray.force_preimages = preimages;
        assert_verdict!(check(&stray, &inputs).await, Verdict::Invalid(_));
    }

    #[tokio::test]
    async fn patch_upgrades_bump_the_version_without_a_transaction() {
        let mut inputs = StubInputs::default();
        inputs
            .upgrades
            .insert(version("0.31.1"), upgrade_info("0.31.1", false, Vec::new()));

        let mut record = valid_record(Vec::new());
        record.protocol_version = version("0.31.1");
        assert_verdict!(check(&record, &inputs).await, Verdict::Valid);

        // A transaction where the watched upgrade has none: invalid.
        let mut with_tx = valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.31.1")))]);
        with_tx.protocol_version = version("0.31.1");
        assert_verdict!(check(&with_tx, &inputs).await, Verdict::Invalid(_));
    }

    #[tokio::test]
    async fn genesis_upgrade_executes_at_the_current_version() {
        // A fresh chain's first block carries the genesis upgrade transaction at the
        // chain's own version; the metadata (preimages) does not apply.
        let mut inputs = StubInputs::default();
        inputs.upgrades.insert(
            version("0.31.0"),
            upgrade_info(
                "0.31.0",
                true,
                vec![(B256::repeat_byte(6), vec![9])], // baked into genesis, not the block
            ),
        );

        let record = valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.31.0")))]);
        assert_verdict!(check(&record, &inputs).await, Verdict::Valid);

        // Carrying the metadata preimages anyway: invalid.
        let mut with_preimages =
            valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.31.0")))]);
        with_preimages.force_preimages = vec![(B256::repeat_byte(6), vec![9])];
        assert_verdict!(check(&with_preimages, &inputs).await, Verdict::Invalid(_));
    }

    #[tokio::test]
    async fn upgrades_do_not_activate_early() {
        let mut inputs = StubInputs::default();
        let mut info = upgrade_info("0.32.0", true, Vec::new());
        info.metadata.timestamp = NOW + 3_600; // activates an hour from now
        inputs.upgrades.insert(version("0.32.0"), info);

        let mut record = valid_record(vec![ZkTransaction::from(upgrade_tx(&version("0.32.0")))]);
        record.protocol_version = version("0.32.0");
        assert_verdict!(check(&record, &inputs).await, Verdict::Invalid(_));
    }

    #[tokio::test]
    async fn upgrade_transaction_must_come_first() {
        let seen = priority_tx(0, 100);
        let mut inputs = StubInputs::default();
        inputs.priority_txs.insert(0, Arc::new(seen.clone()));
        inputs
            .upgrades
            .insert(version("0.32.0"), upgrade_info("0.32.0", true, Vec::new()));

        let mut record = valid_record(vec![
            ZkTransaction::from(seen),
            ZkTransaction::from(upgrade_tx(&version("0.32.0"))),
        ]);
        record.protocol_version = version("0.32.0");
        assert_verdict!(check(&record, &inputs).await, Verdict::Invalid(_));
    }
}
