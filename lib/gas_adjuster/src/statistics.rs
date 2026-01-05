use std::collections::VecDeque;

/// Helper structure responsible for collecting the data about recent transactions,
/// calculating the median base fee.
#[derive(Debug, Clone, Default)]
pub(crate) struct GasStatistics<T> {
    samples: VecDeque<T>,
    median_cached: T,
    max_samples: usize,
    last_processed_block: u64,
}

impl<T: Ord + Copy + Default> GasStatistics<T> {
    pub fn new(max_samples: usize, block: u64, fee_history: impl IntoIterator<Item = T>) -> Self {
        let mut statistics = Self {
            max_samples,
            samples: VecDeque::with_capacity(max_samples),
            median_cached: T::default(),
            last_processed_block: 0,
        };

        statistics.add_samples(fee_history);

        Self {
            last_processed_block: block,
            ..statistics
        }
    }

    pub fn median(&self) -> T {
        self.median_cached
    }

    pub fn add_samples(&mut self, fees: impl IntoIterator<Item = T>) {
        let old_len = self.samples.len();
        self.samples.extend(fees);
        let processed_blocks = self.samples.len() - old_len;
        self.last_processed_block += processed_blocks as u64;

        let extra = self.samples.len().saturating_sub(self.max_samples);
        self.samples.drain(..extra);

        let mut samples: Vec<_> = self.samples.iter().cloned().collect();

        if !self.samples.is_empty() {
            let (_, &mut median, _) = samples.select_nth_unstable(self.samples.len() / 2);
            self.median_cached = median;
        }
    }

    pub fn last_processed_block(&self) -> u64 {
        self.last_processed_block
    }
}

#[cfg(test)]
mod tests {
    use super::GasStatistics;
    use std::collections::VecDeque;

    /// Check that we compute the median correctly
    #[test]
    fn median() {
        // sorted: 4 4 6 7 8
        assert_eq!(GasStatistics::new(5, 5, [6, 4, 7, 8, 4]).median(), 6);
        // sorted: 4 4 8 10
        assert_eq!(GasStatistics::new(4, 4, [8, 4, 4, 10]).median(), 8);
    }

    /// Check that we properly manage the block base fee queue
    #[test]
    fn samples_queue() {
        let mut stats = GasStatistics::new(5, 5, [6, 4, 7, 8, 4, 5]);

        assert_eq!(stats.samples, VecDeque::from([4, 7, 8, 4, 5]));

        stats.add_samples([18, 18, 18]);

        assert_eq!(stats.samples, VecDeque::from([4, 5, 18, 18, 18]));
    }
}
