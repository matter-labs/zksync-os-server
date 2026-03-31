use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use alloy::primitives::TxHash;
use std::time::Duration;

/// Fee parameters attached to every submitted L1 transaction.
///
/// Carried through the `in_flight` channel so the Watcher can pass them back
/// to the Submitter when requesting a resubmission — the Submitter then
/// compares them against freshly estimated fees to decide whether a
/// replacement transaction is warranted.
#[derive(Clone, Debug)]
pub(crate) struct GasParams {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    /// `None` for non-blob (EIP-1559) transactions.
    pub max_fee_per_blob_gas: Option<u128>,
}

impl GasParams {
    /// Returns `true` when `other` is a sufficient fee bump to replace `self`
    /// in the mempool — every dimension must be at least 110% of the current value.
    pub fn is_sufficient_replacement(&self, other: &GasParams) -> bool {
        let fee_ok = other.max_fee_per_gas >= self.max_fee_per_gas * 11 / 10
            && other.max_priority_fee_per_gas >= self.max_priority_fee_per_gas * 11 / 10;

        let blob_ok = match (self.max_fee_per_blob_gas, other.max_fee_per_blob_gas) {
            (Some(old), Some(new)) => new >= old * 11 / 10,
            (None, _) => true,
            (Some(_), None) => false,
        };

        fee_ok && blob_ok
    }
}

/// Flows from Submitter → Watcher through the `in_flight` channel.
pub(crate) enum InFlightItem<Input> {
    /// A transaction that has been submitted to L1 and needs confirmation.
    Tx(InFlightTx<Input>),
    /// A batch that was already committed on L1; the Watcher forwards it
    /// immediately without awaiting a receipt.
    Passthrough(Box<SignedBatchEnvelope<FriProof>>),
}

/// A submitted L1 transaction awaiting confirmation.
pub(crate) struct InFlightTx<Input> {
    pub tx_hash: TxHash,
    pub gas_params: GasParams,
    /// Original command — kept so the Submitter can rebuild calldata on resubmission,
    /// and so the Watcher can forward it downstream on confirmation.
    pub command: Input,
    /// Nonce used when submitting this tx — required to issue a replacement
    /// transaction with the same nonce (EIP-1559 replacement rules).
    pub nonce: u64,
}

/// Sent from Watcher → Submitter when a tx confirmation times out.
pub(crate) struct ResubmitRequest<Input> {
    pub original_tx_hash: TxHash,
    pub original_gas_params: GasParams,
    pub command: Input,
    pub nonce: u64,
}

/// Exponential backoff for transient errors.
///
/// Delay sequence: 5 s → 10 s → 20 s → 40 s → 60 s (capped).
/// Call `reset()` after a successful operation to start from 5 s again.
pub(crate) struct Backoff {
    current: Duration,
}

impl Backoff {
    const INITIAL: Duration = Duration::from_secs(5);
    const MAX: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Self {
            current: Self::INITIAL,
        }
    }

    pub fn current(&self) -> Duration {
        self.current
    }

    /// Sleep for the current delay.
    pub async fn wait(&self) {
        tokio::time::sleep(self.current).await;
    }

    /// Double the delay, capped at `MAX`.
    pub fn advance(&mut self) {
        self.current = (self.current * 2).min(Self::MAX);
    }

    /// Reset to the initial delay.
    pub fn reset(&mut self) {
        self.current = Self::INITIAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_accepted_when_all_fees_at_exactly_110_percent() {
        let old = GasParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: None,
        };
        let new = GasParams {
            max_fee_per_gas: 110,
            max_priority_fee_per_gas: 11,
            max_fee_per_blob_gas: None,
        };
        assert!(old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_when_max_fee_below_threshold() {
        let old = GasParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: None,
        };
        let new = GasParams {
            max_fee_per_gas: 109,
            max_priority_fee_per_gas: 11,
            max_fee_per_blob_gas: None,
        };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_when_priority_fee_below_threshold() {
        let old = GasParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: None,
        };
        let new = GasParams {
            max_fee_per_gas: 110,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: None,
        };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_accepted_for_blob_tx_when_blob_fee_also_sufficient() {
        let old = GasParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: Some(50),
        };
        let new = GasParams {
            max_fee_per_gas: 110,
            max_priority_fee_per_gas: 11,
            max_fee_per_blob_gas: Some(55),
        };
        assert!(old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_for_blob_tx_when_blob_fee_below_threshold() {
        let old = GasParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: Some(50),
        };
        let new = GasParams {
            max_fee_per_gas: 110,
            max_priority_fee_per_gas: 11,
            max_fee_per_blob_gas: Some(54),
        };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_when_new_has_no_blob_fee_but_old_does() {
        let old = GasParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: Some(50),
        };
        let new = GasParams {
            max_fee_per_gas: 110,
            max_priority_fee_per_gas: 11,
            max_fee_per_blob_gas: None,
        };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn backoff_follows_doubling_sequence_capped_at_60s() {
        let mut b = Backoff::new();
        assert_eq!(b.current(), Duration::from_secs(5));
        b.advance();
        assert_eq!(b.current(), Duration::from_secs(10));
        b.advance();
        assert_eq!(b.current(), Duration::from_secs(20));
        b.advance();
        assert_eq!(b.current(), Duration::from_secs(40));
        b.advance();
        assert_eq!(b.current(), Duration::from_secs(60));
        b.advance();
        assert_eq!(b.current(), Duration::from_secs(60), "must not exceed cap");
    }

    #[test]
    fn backoff_resets_to_initial_delay() {
        let mut b = Backoff::new();
        b.advance();
        b.advance();
        b.advance();
        b.reset();
        assert_eq!(b.current(), Duration::from_secs(5));
    }
}
