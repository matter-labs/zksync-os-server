//! Test contracts that can be deployed and interacted with during a test's lifetime.
//! See `./test-contracts/README.md` for instructions on how to build the artifacts.

use crate::assert_traits::ReceiptAssert;
use crate::network::Zksync;
use crate::provider::ZksyncApi;
use alloy::network::ReceiptResponse;
use alloy::primitives::{Address, U256, address};
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::{Log, TransactionReceipt};
use zksync_os_contract_interface::Bridgehub;
use zksync_os_types::ZkReceiptEnvelope;

alloy::sol!(
    /// Simple contract that can emit events on demand.
    #[sol(rpc)]
    EventEmitter,
    "test-contracts/out/EventEmitter.sol/EventEmitter.json"
);

alloy::sol! {
    #[sol(rpc)]
    interface IBaseToken {
        function withdraw(address _l1Receiver) external payable;
    }

    #[sol(rpc)]
    interface IL1AssetRouter {
        /// @dev Address of nullifier.
        IL1Nullifier public immutable L1_NULLIFIER;
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

    interface IL1Messenger {
        event L1MessageSent(address indexed _sender, bytes32 indexed _hash, bytes _message);
    }
}

const L1_MESSENGER_ADDRESS: Address = address!("0000000000000000000000000000000000008008");
const L2_BASE_TOKEN_ADDRESS: Address = address!("000000000000000000000000000000000000800a");

pub struct L2BaseToken<P: Provider<Zksync>>(IBaseToken::IBaseTokenInstance<P, Zksync>);

impl<P: Provider<Zksync>> L2BaseToken<P> {
    pub fn new(l2_provider: P) -> Self {
        Self(IBaseToken::new(L2_BASE_TOKEN_ADDRESS, l2_provider))
    }

    pub fn address(&self) -> &Address {
        self.0.address()
    }

    pub async fn withdraw(
        &self,
        l1_receiver: Address,
        value: U256,
    ) -> alloy::contract::Result<PendingTransactionBuilder<Zksync>> {
        self.0.withdraw(l1_receiver).value(value).send().await
    }
}

pub struct L1AssetRouter<P1: Provider, P2: Provider<Zksync>> {
    instance: IL1AssetRouter::IL1AssetRouterInstance<P1>,
    l2_provider: P2,
}

impl<P1: Provider + Clone, P2: Provider<Zksync> + Clone> L1AssetRouter<P1, P2> {
    pub async fn new(l1_provider: P1, l2_provider: P2) -> anyhow::Result<Self> {
        let bridgehub_address = l2_provider.get_bridgehub_contract().await.unwrap();
        let bridgehub = Bridgehub::new(
            bridgehub_address,
            &l1_provider,
            l2_provider.get_chain_id().await?,
        );
        bridgehub.shared_bridge_address().await?;
        Ok(Self {
            instance: IL1AssetRouter::new(bridgehub.shared_bridge_address().await?, l1_provider),
            l2_provider,
        })
    }

    pub fn address(&self) -> &Address {
        self.instance.address()
    }

    pub async fn l1_nullifier(&self) -> anyhow::Result<L1Nullifier<P1, P2>> {
        let l1_nullifier_address = self.instance.L1_NULLIFIER().call().await?;
        Ok(L1Nullifier::new(
            l1_nullifier_address,
            self.instance.provider().clone(),
            self.l2_provider.clone(),
        ))
    }
}

pub struct L1Nullifier<P1: Provider, P2: Provider<Zksync>> {
    instance: IL1Nullifier::IL1NullifierInstance<P1>,
    l2_provider: P2,
}

impl<P1: Provider, P2: Provider<Zksync>> L1Nullifier<P1, P2> {
    pub fn new(address: Address, l1_provider: P1, l2_provider: P2) -> Self {
        Self {
            instance: IL1Nullifier::new(address, l1_provider),
            l2_provider,
        }
    }

    pub fn address(&self) -> &Address {
        self.instance.address()
    }

    pub async fn finalize_withdrawal(
        &self,
        withdrawal_l2_receipt: TransactionReceipt<ZkReceiptEnvelope<Log>>,
    ) -> anyhow::Result<TransactionReceipt> {
        let l1_message_sent = withdrawal_l2_receipt
            .logs()
            .iter()
            .find_map(|log| {
                if log.address() != L1_MESSENGER_ADDRESS {
                    return None;
                }
                log.log_decode::<IL1Messenger::L1MessageSent>().ok()
            })
            .expect("no `L1MessageSent` events found in withdrawal receipt");
        let (l2_to_l1_log_index, l2_to_l1_log) = withdrawal_l2_receipt
            .inner
            .l2_to_l1_logs()
            .iter()
            .enumerate()
            .find(|(_, log)| log.sender == L1_MESSENGER_ADDRESS)
            .expect("no L2->L1 logs found in withdrawal receipt");
        let proof = self
            .l2_provider
            .get_l2_to_l1_log_proof(
                withdrawal_l2_receipt.transaction_hash(),
                l2_to_l1_log_index as u64,
            )
            .await?
            .expect("node failed to provide proof for withdrawal log");
        let sender = Address::from_slice(&l2_to_l1_log.key[12..]);
        self.instance
            .finalizeDeposit(IL1Nullifier::FinalizeL1DepositParams {
                chainId: U256::from(self.l2_provider.get_chain_id().await?),
                l2BatchNumber: U256::from(proof.batch_number),
                l2MessageIndex: U256::from(proof.id),
                l2Sender: sender,
                l2TxNumberInBatch: withdrawal_l2_receipt
                    .transaction_index
                    .unwrap()
                    .try_into()
                    .unwrap(),
                message: l1_message_sent.inner.data._message,
                merkleProof: proof.proof,
            })
            .send()
            .await?
            .expect_successful_receipt()
            .await
    }
}
