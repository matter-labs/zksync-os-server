use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::L1SenderCommand;
use crate::config::L1SenderConfig;
use crate::run_l1_sender;
use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::providers::{Provider, WalletProvider};
use async_trait::async_trait;
use tokio::sync::mpsc;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Generic L1 Sender pipeline component
/// Can be used for commit, prove, or execute operations
pub struct L1Sender<P, C> {
    pub provider: P,
    pub config: L1SenderConfig<C>,
    pub to_address: Address,
}

#[async_trait]
impl<P, C> PipelineComponent for L1Sender<P, C>
where
    P: Provider + WalletProvider<Wallet = EthereumWallet> + Clone + Send + 'static,
    C: L1SenderCommand + Send + Sync + 'static,
{
    type Input = C;
    type Output = SignedBatchEnvelope<FriProof>;

    const NAME: &'static str = C::NAME;
    const OUTPUT_BUFFER_SIZE: usize = 1;

    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        run_l1_sender(input, output, self.to_address, self.provider, self.config).await
    }
}
