use anyhow::Context;
use std::time::Duration;
use tokio::{
    sync::mpsc::{Receiver, Sender, channel},
    task::{JoinHandle, spawn_blocking},
};
use zk_ee::system::tracer::NopTracer;
use zk_os_forward_system::run::errors::ForwardSubsystemError;
use zk_os_forward_system::run::result_keeper::TxProcessingOutputOwned;
use zk_os_forward_system::run::{
    BlockContext, BlockOutput, InvalidTransaction, NextTxResponse, TxResultCallback, TxSource,
    run_batch,
};
use zksync_os_state::StateView;

/// A one‐by‐one driver around `run_batch`, enabling `execute_next_tx` interface
/// (as opposed to pull interface of `run_batch` in zksync-os)
/// consider changing that interface on zksync-os side, which will make this file redundant
pub struct VmWrapper {
    handle: Option<JoinHandle<Result<BlockOutput, ForwardSubsystemError>>>,
    tx_sender: Sender<NextTxResponse>,
    tx_result_receiver: Receiver<Result<TxProcessingOutputOwned, InvalidTransaction>>,
}

impl VmWrapper {
    /// Spawn the VM runner in a blocking task.
    pub fn new(context: BlockContext, state_view: StateView) -> Self {
        // Channel for sending NextTxResponse (Tx bytes or SealBatch).
        let (tx_sender, tx_receiver) = channel(1);
        // Channel for receiving per‐tx execution results.
        let (res_sender, res_receiver) = channel(1);

        // Wrap the channels in the traits run_batch expects:
        let tx_source = ChannelTxSource::new(tx_receiver);
        let tx_callback = ChannelTxResultCallback::new(res_sender);

        // Spawn the blocking run_batch(...) call.
        let join_handle = spawn_blocking(move || {
            run_batch(
                context,
                state_view.clone(),
                state_view,
                tx_source,
                tx_callback,
                &mut NopTracer::default(),
            )
        });

        Self {
            handle: Some(join_handle),
            tx_sender,
            tx_result_receiver: res_receiver,
        }
    }

    /// Send one transaction to the VM and await its execution result.
    ///
    /// Returns Ok(output) on success, or Err(InvalidTransaction) if the VM
    /// rejected it. In case of an error, you can then call `seal_block()`
    /// to finish the block.
    pub async fn execute_next_tx(
        &mut self,
        raw_tx: Vec<u8>,
    ) -> anyhow::Result<Result<TxProcessingOutputOwned, InvalidTransaction>> {
        // Send the next‐tx request.
        // If this fails, the runner has already shut down.
        if self
            .tx_sender
            .send(NextTxResponse::Tx(raw_tx))
            .await
            .is_err()
        {
            anyhow::bail!("BlockRunner: `tx_source` channel closed unexpectedly");
        }
        // Await the VM's callback.
        match self.tx_result_receiver.recv().await {
            Some(Ok(output)) => Ok(Ok(output)),
            Some(Err(invalid)) => Ok(Err(invalid)),
            None => {
                let timeout_duration = Duration::from_secs(5);
                let task = self.handle.take().unwrap();
                match tokio::time::timeout(timeout_duration, task).await {
                    Ok(Ok(Ok(_))) => {
                        anyhow::bail!("`run_block` finished before `SealBatch` signal")
                    }
                    Ok(Ok(Err(e))) => anyhow::bail!("`run_block`: {e:?}"),
                    Ok(Err(e)) => anyhow::bail!("failed to join `run_block`: {e:?}"),
                    Err(_) => anyhow::bail!(
                        "`tx_result` channel closed unexpectedly and `run_block` did not finish in time"
                    ),
                }
            }
        }
    }

    /// Tell the VM to seal the block and return the final `BlockOutput`.
    pub async fn seal_block(self) -> anyhow::Result<BlockOutput> {
        // Request batch seal.
        let _ = self.tx_sender.send(NextTxResponse::SealBatch).await;
        // Await the blocking task's result.
        self.handle
            .unwrap()
            .await
            .context("failed to join seal task")?
            .map_err(|e| anyhow::anyhow!("runner panicked: {e:?}"))
    }
}

/// A `TxSource` that drives `run_batch` from a `tokio::sync::mpsc::Receiver`.
struct ChannelTxSource {
    receiver: Receiver<NextTxResponse>,
}

impl ChannelTxSource {
    fn new(receiver: Receiver<NextTxResponse>) -> Self {
        Self { receiver }
    }
}

impl TxSource for ChannelTxSource {
    fn get_next_tx(&mut self) -> NextTxResponse {
        // Block until we get a request.
        // If the sender is dropped, default to sealing.
        self.receiver
            .blocking_recv()
            .unwrap_or(NextTxResponse::SealBatch)
    }
}

/// A `TxResultCallback` that forwards each result into a `tokio::sync::mpsc::Sender`.
struct ChannelTxResultCallback {
    sender: Sender<Result<TxProcessingOutputOwned, InvalidTransaction>>,
}

impl ChannelTxResultCallback {
    fn new(sender: Sender<Result<TxProcessingOutputOwned, InvalidTransaction>>) -> Self {
        Self { sender }
    }
}

impl TxResultCallback for ChannelTxResultCallback {
    fn tx_executed(
        &mut self,
        tx_execution_result: Result<TxProcessingOutputOwned, InvalidTransaction>,
    ) {
        // Fire-and-forget the result into the channel.
        // We're on the blocking thread, so use blocking_send.
        let _ = self.sender.blocking_send(tx_execution_result);
    }
}
