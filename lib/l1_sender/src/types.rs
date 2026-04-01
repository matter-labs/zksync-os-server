use std::time::Duration;

// ==============================================================================
// Gas Parameters
// ==============================================================================

/// EIP-1559 gas parameters for a submitted transaction.
///
/// Carried through the submit → poll → resubmit loop so that the resubmission
/// logic can compare fresh network estimates against what was already sent.
#[derive(Clone, Debug)]
pub struct GasParams {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

impl GasParams {
    /// Minimum fee increase required by miners to accept a replacement transaction
    /// over the one already in the mempool, expressed as a percentage multiplier.
    /// EIP-1559 / geth require a 10% bump on both fee fields.
    const REPLACEMENT_BUMP_PCT: u128 = 110;

    /// Returns `true` if `self` satisfies the 10% bump rule relative to `previous`,
    /// meaning a replacement transaction with these fees would be accepted by the
    /// mempool in place of the earlier one.
    pub fn is_sufficient_replacement(&self, previous: &GasParams) -> bool {
        self.max_fee_per_gas * 100 >= previous.max_fee_per_gas * Self::REPLACEMENT_BUMP_PCT
            && self.max_priority_fee_per_gas * 100
                >= previous.max_priority_fee_per_gas * Self::REPLACEMENT_BUMP_PCT
    }
}

// ==============================================================================
// Backoff
// ==============================================================================

/// Exponential-backoff helper for polling loops.
///
/// Each call to [`Backoff::next`] returns the current sleep duration and
/// multiplies it by `factor` for the next call, up to `max`.
#[derive(Clone, Debug)]
pub struct Backoff {
    current: Duration,
    max: Duration,
    factor: u32,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration, factor: u32) -> Self {
        Self {
            current: initial,
            max,
            factor,
        }
    }

    /// Returns the next sleep duration and advances the internal state.
    pub fn next_duration(&mut self) -> Duration {
        let sleep = self.current;
        self.current = std::cmp::min(self.current * self.factor, self.max);
        sleep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_params_is_sufficient_replacement() {
        let cases: &[(u128, u128, u128, u128, bool, &str)] = &[
            (100, 10, 110, 11, true, "exact 10% bump on both fields"),
            (100, 10, 109, 11, false, "max_fee_per_gas bump < 10%"),
            (
                100,
                10,
                110,
                10,
                false,
                "max_priority_fee_per_gas not bumped",
            ),
            (100, 10, 200, 20, true, "large fee bump qualifies"),
            (0, 0, 0, 0, true, "both zero fees are trivially sufficient"),
        ];
        for &(old_fee, old_pri, new_fee, new_pri, expected, msg) in cases {
            let old = GasParams {
                max_fee_per_gas: old_fee,
                max_priority_fee_per_gas: old_pri,
            };
            let new = GasParams {
                max_fee_per_gas: new_fee,
                max_priority_fee_per_gas: new_pri,
            };
            assert_eq!(new.is_sufficient_replacement(&old), expected, "case: {msg}");
        }
    }

    #[test]
    fn backoff_advances_and_caps_at_max() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(1_000), 2);
        assert_eq!(b.next_duration(), Duration::from_millis(100));
        assert_eq!(b.next_duration(), Duration::from_millis(200));
        assert_eq!(b.next_duration(), Duration::from_millis(400));
        assert_eq!(b.next_duration(), Duration::from_millis(800));
        // Capped at max from here on.
        assert_eq!(b.next_duration(), Duration::from_millis(1_000));
        assert_eq!(b.next_duration(), Duration::from_millis(1_000));
    }
}
