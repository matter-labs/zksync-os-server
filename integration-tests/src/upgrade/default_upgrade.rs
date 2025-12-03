use alloy::{
    primitives::{B256, keccak256},
    sol_types::SolValue as _,
};

use crate::{
    dyn_wallet_provider::EthDynProvider,
    upgrade::interfaces::{self, ProposedUpgrade},
};

#[derive(Debug)]
pub struct DefaultUpgrade {
    instance: interfaces::DefaultUpgrade::DefaultUpgradeInstance<EthDynProvider>,
    proposed_upgrade: ProposedUpgrade,
}

impl DefaultUpgrade {
    /// Deploys the DefaultUpgrade contract on L1.
    pub async fn deploy(
        l1_provider: &EthDynProvider,
        proposed_upgrade: &ProposedUpgrade,
    ) -> anyhow::Result<Self> {
        let instance = interfaces::DefaultUpgrade::deploy(l1_provider.clone()).await?;
        Ok(Self {
            instance,
            proposed_upgrade: proposed_upgrade.clone(),
        })
    }

    pub fn diamond_cut_data(&self) -> interfaces::DiamondCutData {
        let init_calldata = self
            .instance
            .upgrade(self.proposed_upgrade.clone())
            .calldata()
            .clone();

        interfaces::DiamondCutData {
            facetCuts: vec![],
            initAddress: *self.instance.address(),
            initCalldata: init_calldata,
        }
    }

    pub fn upgrade_tx_l2_hash(&self) -> B256 {
        // Reimplementation from `L2CanonicalTransaction` in server, since we're re-declaring this type.
        keccak256(self.proposed_upgrade.l2ProtocolUpgradeTx.abi_encode())
    }
}
