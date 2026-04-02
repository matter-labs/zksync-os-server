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

    /// Returns a new `GasParams` that is guaranteed to satisfy the EIP-1559 10% bump
    /// rule relative to `previous`.
    ///
    /// Each field is `max(self, ceil(previous * 1.1))`, so the result is always the
    /// higher of the fresh network estimate and the minimum required for mempool
    /// acceptance.
    pub fn with_minimum_bump(&self, previous: &GasParams) -> GasParams {
        // Ceiling division: (previous * 110 + 99) / 100 ensures the result
        // always satisfies `result * 100 >= previous * 110`.
        let min_fee = (previous.max_fee_per_gas * Self::REPLACEMENT_BUMP_PCT + 99) / 100;
        let min_priority =
            (previous.max_priority_fee_per_gas * Self::REPLACEMENT_BUMP_PCT + 99) / 100;
        GasParams {
            max_fee_per_gas: self.max_fee_per_gas.max(min_fee),
            max_priority_fee_per_gas: self.max_priority_fee_per_gas.max(min_priority),
        }
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
    fn gas_params_with_minimum_bump() {
        // Each case: (prev_fee, prev_pri, fresh_fee, fresh_pri, expected_fee, expected_pri, label)
        let cases: &[(u128, u128, u128, u128, u128, u128, &str)] = &[
            // Fresh estimate already above the 10% threshold — use it as-is.
            (100, 10, 120, 15, 120, 15, "fresh already above bump threshold"),
            // Fresh estimate below threshold — floor is applied.
            (100, 10, 105, 10, 110, 11, "fresh below threshold, floor applied"),
            // Fresh estimate exactly at the threshold.
            (100, 10, 110, 11, 110, 11, "fresh exactly at threshold"),
            // Ceiling division: 9 * 110 = 990, ceil(990/100) = 10.
            (9, 9, 0, 0, 10, 10, "ceiling division for non-round values"),
            // Zero previous fees — no bump needed.
            (0, 0, 5, 5, 5, 5, "zero previous fees"),
        ];
        for &(prev_fee, prev_pri, fresh_fee, fresh_pri, exp_fee, exp_pri, label) in cases {
            let previous = GasParams {
                max_fee_per_gas: prev_fee,
                max_priority_fee_per_gas: prev_pri,
            };
            let fresh = GasParams {
                max_fee_per_gas: fresh_fee,
                max_priority_fee_per_gas: fresh_pri,
            };
            let bumped = fresh.with_minimum_bump(&previous);
            assert_eq!(bumped.max_fee_per_gas, exp_fee, "fee: {label}");
            assert_eq!(bumped.max_priority_fee_per_gas, exp_pri, "priority: {label}");
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
