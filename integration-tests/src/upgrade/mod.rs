pub use self::builder::ProtocolUpgradeBuilder;
pub use self::default_upgrade::DefaultUpgrade;
pub use self::interfaces::{Action, CommitterFacetV31, FacetCut};
pub use self::tester::UpgradeTester;

use crate::{SettlementLayer, TestCase, Tester};
use alloy::primitives::U256;
use std::collections::BTreeMap;
use zksync_os_server::default_protocol_version::PROTOCOL_VERSION_V31_0;
use zksync_os_types::ProtocolSemanticVersion;

mod builder;
mod default_upgrade;
mod interfaces;
mod tester;

pub async fn prepare_gateway_chain_at_v32_1() -> anyhow::Result<Tester> {
    let target_protocol_version = ProtocolSemanticVersion::new(0, 32, 1);
    let env = TestCase {
        protocol_version: PROTOCOL_VERSION_V31_0,
        settlement_layer: SettlementLayer::Gateway,
    }
    .environment()
    .await?;
    let tester = env.launch_default().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(&tester).await?;

    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .set_version(target_protocol_version.clone())
        .with_force_deployments(BTreeMap::new())
        .with_timestamp(U256::from(1))
        .build();

    upgrade_tester
        .execute_default_upgrade(&protocol_upgrade, U256::MAX, U256::from(1), false, vec![])
        .await?;

    let upgraded_protocol_version = ProtocolSemanticVersion::try_from(
        upgrade_tester
            .ctm_sl
            .getProtocolVersion(U256::from(upgrade_tester.chain_id))
            .call()
            .await?,
    )
    .expect("invalid protocol version stored in CTM");
    anyhow::ensure!(
        upgraded_protocol_version == target_protocol_version,
        "gateway chain was upgraded to {upgraded_protocol_version}, expected {target_protocol_version}"
    );

    tester.restart().await
}
