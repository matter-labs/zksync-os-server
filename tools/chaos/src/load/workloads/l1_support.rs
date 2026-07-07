//! Shared plumbing for the L1-operation sagas: a dedicated, self-funding L1
//! account per saga (so concurrent sagas never race one key's nonces), the
//! bridgehub deposit path returning the canonical L2 transaction hash, and
//! discovery of the withdrawal-finalization contract.

use crate::setup::Manifest;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Context as _;
use std::time::Duration;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_types::REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE;

/// Anvil's default account #0 — rich on the checked-in L1 state; funds the
/// sagas' dedicated accounts once at startup.
const ANVIL_RICH_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

alloy::sol! {
    #[sol(rpc)]
    interface IL1AssetRouter {
        address public immutable L1_NULLIFIER;
    }

    #[sol(rpc)]
    interface IL1Nullifier {
        struct FinalizeL1DepositParams {
            uint256 chainId;
            uint256 l2BatchNumber;
            uint256 l2MessageIndex;
            address l2Sender;
            uint16 l2TxNumberInBatch;
            bytes message;
            bytes32[] merkleProof;
        }

        function finalizeDeposit(FinalizeL1DepositParams calldata _finalizeWithdrawalParams) external;
    }
}

/// One saga's view of the L1: its own funded account, wired to the bridgehub.
pub struct L1Side {
    pub provider: DynProvider,
    pub bridgehub: Bridgehub<DynProvider>,
    pub l2_chain_id: u64,
}

impl L1Side {
    /// Derives the saga's L1 key from `(namespace, key_seed)`, funds it from
    /// anvil's rich account if it is empty, and connects the bridgehub.
    pub async fn new(
        manifest: &Manifest,
        l2_chain_id: u64,
        namespace: &[u8],
        key_seed: u64,
        fund_eth: u64,
    ) -> anyhow::Result<L1Side> {
        let url: reqwest::Url = format!("http://127.0.0.1:{}", manifest.host_l1_port).parse()?;
        let mut material = namespace.to_vec();
        material.extend_from_slice(&key_seed.to_be_bytes());
        let signer = PrivateKeySigner::from_bytes(&keccak256(material))?;
        let address = signer.address();
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(url.clone())
            .erased();

        let balance = provider
            .get_balance(address)
            .await
            .context("is the L1 up? cannot reach anvil")?;
        if balance == U256::ZERO {
            let rich: PrivateKeySigner = ANVIL_RICH_KEY.parse()?;
            let rich_provider = ProviderBuilder::new()
                .wallet(EthereumWallet::from(rich))
                .connect_http(url)
                .erased();
            let receipt = rich_provider
                .send_transaction(
                    TransactionRequest::default()
                        .to(address)
                        .value(U256::from(fund_eth) * U256::from(10u128.pow(18))),
                )
                .await?
                .get_receipt()
                .await?;
            anyhow::ensure!(receipt.status(), "funding the saga L1 account reverted");
        }

        let bridgehub_address: Address = manifest.bridgehub_address.parse()?;
        let bridgehub = Bridgehub::new(bridgehub_address, provider.clone(), l2_chain_id);
        Ok(L1Side {
            provider,
            bridgehub,
            l2_chain_id,
        })
    }

    /// A priority-op deposit through the bridgehub; returns the canonical L2
    /// transaction hash the relay will use (from the `NewPriorityRequest` log).
    pub async fn deposit(
        &self,
        to: Address,
        l2_value: U256,
        calldata: Vec<u8>,
        l2_gas: u64,
        refund_recipient: Address,
    ) -> anyhow::Result<B256> {
        let l1_gas_price = self.provider.get_gas_price().await?;
        let base_cost = self
            .bridgehub
            .l2_transaction_base_cost(
                l1_gas_price.saturating_mul(2),
                l2_gas,
                REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            )
            .await?;
        let mint_value = l2_value + base_cost;
        let receipt = tokio::time::timeout(
            Duration::from_secs(60),
            self.provider
                .send_transaction(
                    self.bridgehub
                        .request_l2_transaction_direct(
                            mint_value,
                            to,
                            l2_value,
                            calldata,
                            l2_gas,
                            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                            refund_recipient,
                        )
                        .value(mint_value)
                        .into_transaction_request(),
                )
                .await?
                .get_receipt(),
        )
        .await
        .context("deposit receipt timed out")??;
        anyhow::ensure!(receipt.status(), "the L1 deposit transaction reverted");
        let request = receipt
            .logs()
            .iter()
            .find_map(|log| log.log_decode::<NewPriorityRequest>().ok())
            .context("deposit produced no NewPriorityRequest log")?;
        Ok(request.inner.data.txHash)
    }

    /// The L1Nullifier (withdrawal finalization) address, discovered the way
    /// the protocol lays it out: bridgehub → shared bridge (the asset router)
    /// → its nullifier.
    pub async fn nullifier_address(&self) -> anyhow::Result<Address> {
        let asset_router = self
            .bridgehub
            .shared_bridge_address()
            .await
            .context("bridgehub.sharedBridge")?;
        let nullifier = IL1AssetRouter::new(asset_router, self.provider.clone())
            .L1_NULLIFIER()
            .call()
            .await
            .context("assetRouter.L1_NULLIFIER")?;
        Ok(nullifier)
    }
}
