use crate::model::blocks::PreparedBlockCommand;
use alloy::primitives::B256;
use std::fmt;
use zksync_os_interface::types::{BlockContext, BlockOutput};

struct BlockContextDbg<'a>(&'a BlockContext);
impl<'a> fmt::Debug for BlockContextDbg<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_block_context(self.0, f)
    }
}

impl<'a> fmt::Debug for PreparedBlockCommand<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ds = f.debug_struct("PreparedBlockCommand");
        ds.field("block_context", &BlockContextDbg(&self.block_context));
        ds.field("seal_policy", &self.seal_policy);
        ds.field("invalid_tx_policy", &self.invalid_tx_policy);
        // ds.field("tx_source", &"<skipped>");
        ds.field("starting_l1_priority_id", &self.starting_l1_priority_id);
        ds.field("metrics_label", &self.metrics_label);
        ds.field("node_version", &self.node_version);
        ds.field(
            "expected_block_output_hash",
            &self.expected_block_output_hash,
        );
        ds.field("previous_block_timestamp", &self.previous_block_timestamp);
        ds.finish()
    }
}

fn fmt_block_context(bc: &BlockContext, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Keep the log concise: take refs to the ends and show just those two (as opposed to 256 of them)
    let block_hashes_ends = format!("{}, {}", &bc.block_hashes.0[0], &bc.block_hashes.0[255]);

    f.debug_struct("BlockContext")
        .field("chain_id", &bc.chain_id)
        .field("block_number", &bc.block_number)
        .field("block_hashes", &block_hashes_ends)
        .field("timestamp", &bc.timestamp)
        .field("eip1559_basefee", &bc.eip1559_basefee)
        .field("gas_per_pubdata", &bc.gas_per_pubdata)
        .field("native_price", &bc.native_price)
        .field("coinbase", &bc.coinbase)
        .field("gas_limit", &bc.gas_limit)
        .field("pubdata_limit", &bc.pubdata_limit)
        .field("mix_hash", &bc.mix_hash)
        .field("execution_version", &bc.execution_version)
        .finish()
}

pub struct BlockOutputDebug<'a>(pub &'a BlockOutput);

// Helper that prints bytes as 0x...
struct Hex<'a>(&'a [u8]);
impl<'a> fmt::Debug for Hex<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl<'a> fmt::Debug for BlockOutputDebug<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = self.0;

        // Build view of (hash, 0x<hex>) for logging
        let preimages: Vec<(&B256, Hex<'_>)> = o
            .published_preimages
            .iter()
            .map(|(h, bytes)| (h, Hex(bytes)))
            .collect();

        f.debug_struct("BlockOutput")
            .field("header", &o.header)
            .field("tx_results", &o.tx_results)
            .field("storage_writes", &o.storage_writes)
            .field("account_diffs", &o.account_diffs)
            .field("published_preimages", &preimages)
            .field("pubdata", &Hex(&o.pubdata))
            .field("computaional_native_used", &o.computaional_native_used)
            .finish()
    }
}
