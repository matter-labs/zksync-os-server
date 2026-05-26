use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::Address;
use alloy::providers::{Provider, WalletProvider};
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Generic L1 Sender pipeline component
/// Can be used for commit, prove, or execute operations
pub struct L1Sender<P: Provider<Ethereum>, C> {
    pub provider: P,
    pub config: L1SenderConfig<C>,
    pub to_address: Address,
    pub gateway: bool,
    pub commit_submitted_tx: Option<watch::Sender<u64>>,
    /// SL block number at which `getTotalBatches*` was read on startup; passed through to
    /// `run_l1_sender` to keep the confirmed-nonce baseline consistent with the inbound queue.
    pub sl_block_number: u64,
}

#[async_trait]
impl<P, C> PipelineComponent for L1Sender<P, C>
where
    P: Provider<Ethereum> + WalletProvider<Wallet = EthereumWallet> + Clone + 'static,
    C: SendToL1 + Send + Sync + 'static,
{
    type Input = L1SenderCommand<C>;
    type Output = SignedBatchEnvelope<FriProof>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId = C::COMPONENT_ID;

    async fn run(
        mut self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        self.register_operator().await?;
        tokio::select! {
            result = self.run_l1_sender(input, output, state_reporter) => result,
            result = self.report_operator_metrics_loop() => result,
        }
    }
}
