pub use self::builder::ProtocolUpgradeBuilder;
pub use self::default_upgrade::DefaultUpgrade;
pub use self::interfaces::ZKSYNC_OS_TESTNET_VERIFIER_DEPLOYED_BYTECODE;
pub use self::interfaces::{
    Action, CommitterFacetV31, CommitterFacetV32, ExecutorFacetV32, FacetCut,
};
pub use self::tester::UpgradeTester;

mod builder;
mod default_upgrade;
mod interfaces;
mod tester;

use alloy::primitives::{FixedBytes, U256};
use alloy::providers::Provider as _;

/// Facet cuts installing the era-contracts#2323 Executor + Committer.
///
/// A v31 -> v32 upgrade must carry these: from protocol v32 the server commits the
/// chain-id-less `batchOutputHash` and folds the chain config hash into the batch proof
/// public input, and only these facets compute the matching values on-chain. Selectors match
/// the set `local-chains/v32.0/regenerate.sh` replaces.
pub async fn v32_facet_cuts(upgrade_tester: &UpgradeTester<'_>) -> anyhow::Result<Vec<FacetCut>> {
    let l1_chain_id = upgrade_tester.tester.l1_provider().get_chain_id().await?;
    let executor = ExecutorFacetV32::deploy(
        upgrade_tester.tester.l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;
    let committer = CommitterFacetV32::deploy(
        upgrade_tester.tester.l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;
    Ok(vec![
        FacetCut {
            facet: *executor.address(),
            action: Action::Replace,
            isFreezable: true,
            selectors: vec![
                FixedBytes([0xa0, 0x85, 0x34, 0x4d]),
                FixedBytes([0x7c, 0xa4, 0xef, 0xf7]),
                FixedBytes([0x92, 0x71, 0xe4, 0x50]),
            ],
        },
        FacetCut {
            facet: *committer.address(),
            action: Action::Replace,
            isFreezable: true,
            selectors: vec![
                FixedBytes([0x0b, 0x6d, 0xb8, 0x20]),
                FixedBytes([0x0d, 0xb9, 0xeb, 0x87]),
            ],
        },
    ])
}
