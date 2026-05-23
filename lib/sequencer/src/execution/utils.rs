use alloy::primitives::{B256, keccak256};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::cell::OnceCell;
use std::collections::HashSet;
use std::mem;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};
use zksync_os_interface::traits::ReadStorage;
use zksync_os_interface::types::BlockOutput;
use zksync_os_storage_api::BlockContext;
use zksync_os_types::ZkTransaction;

/// [`ReadStorage`] wrapper that tracks read storage slots.
#[derive(Debug)]
pub(super) struct ReadRecordingState<S> {
    inner: S,
    local_read_keys: HashSet<B256>,
    read_keys_handle: Rc<OnceCell<HashSet<B256>>>,
}

impl<S: ReadStorage> ReadRecordingState<S> {
    pub(super) fn new(inner: S) -> (Self, ReadRecordingHandle) {
        let handle = ReadRecordingHandle(Rc::default());
        let this = Self {
            inner,
            local_read_keys: HashSet::new(),
            read_keys_handle: handle.0.clone(),
        };
        (this, handle)
    }
}

impl<S: ReadStorage> ReadStorage for ReadRecordingState<S> {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.local_read_keys.insert(key);
        self.inner.read(key)
    }
}

impl<S> Drop for ReadRecordingState<S> {
    fn drop(&mut self) {
        let read_keys = mem::take(&mut self.local_read_keys);
        // `unwrap()` is safe: the recording state is never duplicated
        self.read_keys_handle.set(read_keys).unwrap();
    }
}

/// Handle for [`ReadRecordingState`] that allows to extract read storage slots after the state is dropped.
#[derive(Debug)]
pub(super) struct ReadRecordingHandle(Rc<OnceCell<HashSet<B256>>>);

impl ReadRecordingHandle {
    pub(super) fn into_read_keys(self) -> HashSet<B256> {
        Rc::try_unwrap(self.0)
            .expect("`into_read_keys()` called before the recording state is dropped")
            .into_inner()
            .expect("recording state didn't set read keys")
    }
}

// Hash of the block output, which is used to identify divergences in block execution.
// It's incomplete, in a sense that it does not include all the data from the block output.
// Hash includes the most important pieces of data that are likely to change in case of a divergence.
pub(crate) fn hash_block_output(block_output: &BlockOutput) -> B256 {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(block_output.header.hash().as_slice());
    for tx in block_output.tx_results.iter().flatten() {
        preimage.extend_from_slice(&[tx.is_success() as u8]);
        preimage.extend_from_slice(&tx.gas_used.to_be_bytes());
    }
    for storage_log in &block_output.storage_writes {
        preimage.extend_from_slice(storage_log.key.as_slice());
        preimage.extend_from_slice(storage_log.value.as_slice());
    }

    keccak256(preimage)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockDump {
    pub ctx: BlockContext,
    pub txs: Vec<ZkTransaction>,
    pub error: String,
}

pub(crate) fn save_dump(path: PathBuf, dump: BlockDump) -> anyhow::Result<()> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Incorrect system time")
        .as_secs();
    let file_name = format!("dump_{}_{seconds}.json", dump.ctx.block_number);
    let bytes = serde_json::to_vec(&dump).context("failed to serialize dump")?;
    std::fs::create_dir_all(&path).context("create_dir_all")?;
    std::fs::write(path.join(file_name), bytes).context("failed to write dump file")?;

    Ok(())
}
