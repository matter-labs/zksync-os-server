use crate::execution::metrics::EXECUTION_METRICS;
use crate::execution::utils::ReadRecordingState;
use crate::model::blocks::BlockOutputWithReads;
use anyhow::Context;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::{
    sync::mpsc::{Receiver, Sender, channel},
    task::{JoinHandle, spawn_blocking},
};
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, AnyTxValidator};
use zksync_os_interface::traits::{EncodedTx, NextTxResponse, TxResultCallback, TxSource};
use zksync_os_interface::types::TxProcessingOutputOwned;
use zksync_os_storage_api::{BlockContext, ViewState};

/// Capacity of the channels between the async driver and the VM worker thread. Production feeds one
/// tx and awaits its result before sending the next, so it never uses more than one slot. The
/// direct-injection bench (see `submit_tx`/`next_result`) feeds eagerly to keep the VM worker busy,
/// so it relies on the deeper buffer. A bounded `tokio` channel only allocates slots lazily, so the
/// large capacity is free when unused.
const VM_CHANNEL_CAPACITY: usize = 1024;

/// A one‐by‐one driver around `run_block`, enabling `execute_next_tx` interface
/// (as opposed to pull interface of `run_block` in zksync-os)
/// consider changing that interface on zksync-os side, which will make this file redundant
pub struct VmWrapper {
    handle: Option<JoinHandle<Result<BlockOutputWithReads, anyhow::Error>>>,
    tx_sender: Sender<NextTxResponse>,
    tx_result_receiver: Receiver<Result<TxProcessingOutputOwned, InvalidTransaction>>,
}

impl VmWrapper {
    /// Spawn the VM runner in a blocking task.
    pub fn new(
        context: BlockContext,
        state_view: impl ViewState,
        mut tracer: impl AnyTracer + Send + 'static,
        mut validator: impl AnyTxValidator + Send + 'static,
        // Accumulates the time the VM worker thread spends blocked waiting for the next tx
        // (i.e. idle because the async side hasn't fed it yet). Used to confirm the per-tx
        // hand-off ping-pong.
        vm_idle_micros: Arc<AtomicU64>,
    ) -> Self {
        // Channel for sending NextTxResponse (Tx bytes or SealBlock).
        let (tx_sender, tx_receiver) = channel(VM_CHANNEL_CAPACITY);
        // Channel for receiving per‐tx execution results.
        let (res_sender, res_receiver) = channel(VM_CHANNEL_CAPACITY);

        // Wrap the channels in the traits run_block expects:
        let tx_source = ChannelTxSource::new(tx_receiver, vm_idle_micros);
        let tx_callback = ChannelTxResultCallback::new(res_sender);

        // Spawn the blocking run_block(...) call.
        let join_handle = spawn_blocking(move || {
            let (recording_state, recording_handle) = ReadRecordingState::new(state_view.clone());
            let block_output = zksync_os_multivm::run_block(
                context,
                recording_state,
                state_view,
                tx_source,
                tx_callback,
                &mut tracer,
                &mut validator,
            )?;

            let recording = recording_handle.into_recording();
            Ok(BlockOutputWithReads::new(
                block_output,
                recording.read_keys,
                recording.total_read_time,
                recording.read_count,
            ))
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
        raw_tx: EncodedTx,
    ) -> anyhow::Result<Result<TxProcessingOutputOwned, InvalidTransaction>> {
        let total_observer = EXECUTION_METRICS.tx_execution[&"total"].start();
        let sending_observer = EXECUTION_METRICS.tx_execution[&"sending"].start();

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
        sending_observer.observe();
        let sending_observer = EXECUTION_METRICS.tx_execution[&"waiting"].start();
        // Await the VM's callback.
        let res = match self.tx_result_receiver.recv().await {
            Some(Ok(output)) => Ok(Ok(output)),
            Some(Err(invalid)) => Ok(Err(invalid)),
            None => {
                let timeout_duration = Duration::from_secs(5);
                let task = self.handle.take().unwrap();
                match tokio::time::timeout(timeout_duration, task).await {
                    Ok(Ok(Ok(_))) => {
                        anyhow::bail!("`run_block` finished before `SealBlock` signal")
                    }
                    Ok(Ok(Err(e))) => anyhow::bail!("`run_block`: {e:?}"),
                    Ok(Err(e)) => anyhow::bail!("failed to join `run_block`: {e:?}"),
                    Err(_) => anyhow::bail!(
                        "`tx_result` channel closed unexpectedly and `run_block` did not finish in time"
                    ),
                }
            }
        };
        sending_observer.observe();
        total_observer.observe();
        res
    }

    /// Submit a transaction to the VM without awaiting its result. Combined with
    /// [`Self::next_result`], lets the caller keep several txs in flight so the VM worker runs them
    /// back-to-back instead of stalling on the per-tx hand-off. Backpressures on the channel.
    /// Only used by the direct-injection bench path (`PreparedBlockCommand::direct_injection`).
    pub async fn submit_tx(&self, raw_tx: EncodedTx) -> anyhow::Result<()> {
        if self
            .tx_sender
            .send(NextTxResponse::Tx(raw_tx))
            .await
            .is_err()
        {
            anyhow::bail!("BlockRunner: `tx_source` channel closed unexpectedly");
        }
        Ok(())
    }

    /// Await the next execution result (results arrive in submission order). See [`Self::submit_tx`].
    pub async fn next_result(
        &mut self,
    ) -> anyhow::Result<Result<TxProcessingOutputOwned, InvalidTransaction>> {
        match self.tx_result_receiver.recv().await {
            Some(Ok(output)) => Ok(Ok(output)),
            Some(Err(invalid)) => Ok(Err(invalid)),
            None => {
                let task = self.handle.take().unwrap();
                match tokio::time::timeout(Duration::from_secs(5), task).await {
                    Ok(Ok(Ok(_))) => {
                        anyhow::bail!("`run_block` finished before `SealBlock` signal")
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
    pub async fn seal_block(self) -> anyhow::Result<BlockOutputWithReads> {
        // Request batch seal.
        let _ = self.tx_sender.send(NextTxResponse::SealBlock).await;
        // Await the blocking task's result.
        self.handle
            .unwrap()
            .await
            .context("failed to join seal task")?
            .map_err(|e| anyhow::anyhow!("runner panicked: {e:?}"))
    }
}

/// A `TxSource` that drives `run_block` from a `tokio::sync::mpsc::Receiver`.
struct ChannelTxSource {
    receiver: Receiver<NextTxResponse>,
    /// Time spent blocked here waiting for the next tx = VM worker idle time.
    vm_idle_micros: Arc<AtomicU64>,
}

impl ChannelTxSource {
    fn new(receiver: Receiver<NextTxResponse>, vm_idle_micros: Arc<AtomicU64>) -> Self {
        Self {
            receiver,
            vm_idle_micros,
        }
    }
}

impl TxSource for ChannelTxSource {
    fn get_next_tx(&mut self) -> NextTxResponse {
        // Block until we get a request.
        // If the sender is dropped, default to sealing.
        // Time spent blocked here is the VM worker idle between transactions.
        let started_at = Instant::now();
        let response = self.receiver.blocking_recv();
        self.vm_idle_micros
            .fetch_add(started_at.elapsed().as_micros() as u64, Ordering::Relaxed);
        response.unwrap_or(NextTxResponse::SealBlock)
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
