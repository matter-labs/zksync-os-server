//! The batch range the aggregation lane is keyed by.

use std::fmt;

/// A closed range of consecutive batches, `from..=to`.
///
/// The aggregation lane keys every collection by range and reports ranges in
/// its errors and logs, so the pair is worth a type: `from <= to` is checked
/// once here instead of assumed at a dozen call sites, and an API that takes a
/// `BatchRange` cannot be called with its bounds swapped.
///
/// Ordering is by lower bound first, which is the order ranges settle in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BatchRange {
    from: u64,
    to: u64,
}

/// A range whose bounds are the wrong way round. Reachable from the prover API,
/// where the bounds arrive on the wire.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid batch range {from}..={to}: the lower bound is above the upper bound")]
pub struct InvalidBatchRange {
    pub from: u64,
    pub to: u64,
}

impl BatchRange {
    pub fn new(from: u64, to: u64) -> Result<Self, InvalidBatchRange> {
        if from > to {
            return Err(InvalidBatchRange { from, to });
        }
        Ok(Self { from, to })
    }

    /// For bounds this process derived rather than received — a SNARK range it
    /// just formed, a test fixture. Panics on inverted bounds, which would be a
    /// logic error rather than bad input.
    pub fn of(from: u64, to: u64) -> Self {
        Self::new(from, to).expect("locally derived batch range must be ordered")
    }

    pub fn from(&self) -> u64 {
        self.from
    }

    pub fn to(&self) -> u64 {
        self.to
    }

    /// How many batches the range covers. Never zero.
    pub fn width(&self) -> u64 {
        self.to - self.from + 1
    }

    pub fn contains(&self, batch_number: u64) -> bool {
        (self.from..=self.to).contains(&batch_number)
    }

    /// Every batch number in the range, in proving order.
    pub fn batches(&self) -> std::ops::RangeInclusive<u64> {
        self.from..=self.to
    }
}

impl fmt::Display for BatchRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..={}", self.from, self.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_checked_once() {
        assert_eq!(
            BatchRange::new(5, 4),
            Err(InvalidBatchRange { from: 5, to: 4 })
        );
        let single = BatchRange::new(7, 7).expect("a range of one batch is valid");
        assert_eq!(single.width(), 1);
        assert!(single.contains(7));
        assert!(!single.contains(8));
    }
}
