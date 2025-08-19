use alloy_rlp::Encodable;

pub trait BlockExt {
    fn rlp_length(&self) -> usize;
}

impl<T: Encodable> BlockExt for alloy::consensus::Block<T> {
    fn rlp_length(&self) -> usize {
        // fixme: this is not correct when T is TxHash as it does not compute size of the full tx
        alloy::consensus::Block::rlp_length_for(&self.header, &self.body)
    }
}
