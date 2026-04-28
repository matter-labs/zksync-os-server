use std::ops::{BitAnd, BitOr, Range};

use alloy::{
    primitives::{Address, B256},
    rpc::types::{Filter, FilterSet},
};
use roaring::RoaringBitmap;
use zksync_os_storage_api::{LogIndex, RepositoryResult};

trait IteratorExt: Iterator {
    /// Same as nightly `Iterator::try_reduce`, for stable Rust.
    fn try_reduce<T, E, F>(mut self, mut f: F) -> Result<Option<T>, E>
    where
        Self: Sized + Iterator<Item = Result<T, E>>,
        F: FnMut(T, T) -> T,
    {
        let first = match self.next() {
            None => return Ok(None),
            Some(Err(e)) => return Err(e),
            Some(Ok(x)) => x,
        };
        self.try_fold(first, |acc, item| item.map(|x| f(acc, x)))
            .map(Some)
    }
}

impl<I: Iterator> IteratorExt for I {}

/// Builds a candidate-block bitmap from the log index for `filter` over `range`.
///
/// Returns the candidates for the filter over `range`.
/// Blocks outside `covered` must be checked via bloom filter.
/// Returns empty candidates if the filter has no address or topic constraints.
pub fn candidates(
    repo: &dyn LogIndex,
    filter: &Filter,
    range: Range<u64>,
) -> RepositoryResult<Candidates> {
    // Within a group bitmaps are OR'd (any match); groups are AND'd (all must match).
    let mut groups: Vec<Candidates> = vec![];

    if !filter.address.is_empty() {
        groups.push(address_candidates(repo, &filter.address, range.clone())?);
    }
    for topics in filter.topics.iter().filter(|ts| !ts.is_empty()) {
        groups.push(topic_candidates(repo, topics, range.clone())?);
    }

    Ok(groups.into_iter().reduce(|a, b| a & b).unwrap_or_default())
}

/// OR's the bitmaps for all addresses in the filter.
fn address_candidates(
    repo: &dyn LogIndex,
    addresses: &FilterSet<Address>,
    range: Range<u64>,
) -> RepositoryResult<Candidates> {
    Ok(addresses
        .iter()
        .map(|addr| {
            repo.blocks_for_address(*addr, range.clone())
                .map(Candidates::from)
        })
        .try_reduce(|a, b| a | b)?
        .unwrap_or_default())
}

/// OR's the bitmaps for all topics in a single topic position.
fn topic_candidates(
    repo: &dyn LogIndex,
    topics: &FilterSet<B256>,
    range: Range<u64>,
) -> RepositoryResult<Candidates> {
    Ok(topics
        .iter()
        .map(|topic| {
            repo.blocks_for_topic(*topic, range.clone())
                .map(Candidates::from)
        })
        .try_reduce(|a, b| a | b)?
        .unwrap_or_default())
}

/// A set of candidate blocks from the log index, together with the range of blocks the index covers.
/// Blocks outside `covered` must be checked via bloom filter regardless of `bitmap`.
pub struct Candidates {
    bitmap: RoaringBitmap,
    covered: Range<u64>,
}

impl Candidates {
    /// Returns `true` if the block at `number` may contain matching logs.
    /// Blocks outside the covered range always return `true` (must fall back to bloom).
    pub fn may_contain(&self, block_number: u64) -> bool {
        !self.covered.contains(&block_number) || self.bitmap.contains(block_number as u32)
    }

    /// Returns the number of blocks the index covers.
    pub fn covered_len(&self) -> u64 {
        self.covered.end.saturating_sub(self.covered.start)
    }
}

impl BitOr for Candidates {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self {
            bitmap: self.bitmap | other.bitmap,
            covered: intersect(self.covered, other.covered),
        }
    }
}

impl BitAnd for Candidates {
    type Output = Self;
    fn bitand(self, other: Self) -> Self {
        Self {
            bitmap: self.bitmap & other.bitmap,
            covered: intersect(self.covered, other.covered),
        }
    }
}

impl Default for Candidates {
    fn default() -> Self {
        Self {
            bitmap: RoaringBitmap::new(),
            covered: 0..0,
        }
    }
}

impl From<(RoaringBitmap, Range<u64>)> for Candidates {
    fn from((bitmap, covered): (RoaringBitmap, Range<u64>)) -> Self {
        Self { bitmap, covered }
    }
}

fn intersect(a: Range<u64>, b: Range<u64>) -> Range<u64> {
    a.start.max(b.start)..a.end.min(b.end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256};
    use alloy::rpc::types::Filter;
    use std::collections::HashMap;
    use zksync_os_storage_api::RepositoryResult;

    #[derive(Debug, Default)]
    struct MockIndex {
        addresses: HashMap<Address, (RoaringBitmap, Range<u64>)>,
        topics: HashMap<B256, (RoaringBitmap, Range<u64>)>,
    }

    impl MockIndex {
        fn with_address(mut self, addr: Address, blocks: &[u64], covered: Range<u64>) -> Self {
            self.addresses
                .insert(addr, (blocks.iter().map(|&b| b as u32).collect(), covered));
            self
        }

        fn with_topic(mut self, topic: B256, blocks: &[u64], covered: Range<u64>) -> Self {
            self.topics
                .insert(topic, (blocks.iter().map(|&b| b as u32).collect(), covered));
            self
        }
    }

    impl LogIndex for MockIndex {
        fn blocks_for_address(
            &self,
            address: Address,
            _range: Range<u64>,
        ) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
            Ok(self.addresses.get(&address).cloned().unwrap_or_default())
        }

        fn blocks_for_topic(
            &self,
            topic: B256,
            _range: Range<u64>,
        ) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
            Ok(self.topics.get(&topic).cloned().unwrap_or_default())
        }
    }

    fn blocks(c: &Candidates) -> Vec<u32> {
        c.bitmap.iter().collect()
    }

    #[test]
    fn unconstrained_filter_returns_empty_candidates() {
        let index = MockIndex::default();
        let filter = Filter::new();
        let c = candidates(&index, &filter, 0..100).unwrap();
        assert!(c.bitmap.is_empty());
        assert_eq!(c.covered, 0..0);
    }

    #[test]
    fn single_address_uses_address_index() {
        let addr = Address::repeat_byte(0x01);
        let index = MockIndex::default().with_address(addr, &[2, 4, 6], 0..10);
        let filter = Filter::new().address(addr);
        let c = candidates(&index, &filter, 0..10).unwrap();
        assert_eq!(blocks(&c), vec![2, 4, 6]);
        assert_eq!(c.covered, 0..10);
    }

    #[test]
    fn multiple_addresses_are_ored() {
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        let index = MockIndex::default()
            .with_address(a, &[1, 3], 0..10)
            .with_address(b, &[3, 5], 0..10);
        let filter = Filter::new().address(vec![a, b]);
        let c = candidates(&index, &filter, 0..10).unwrap();
        assert_eq!(blocks(&c), vec![1, 3, 5]);
    }

    #[test]
    fn address_and_topic_are_anded() {
        let addr = Address::repeat_byte(0x01);
        let topic = B256::repeat_byte(0x42);
        let index = MockIndex::default()
            .with_address(addr, &[1, 2, 3], 0..10)
            .with_topic(topic, &[2, 3, 4], 0..10);
        let filter = Filter::new().address(addr).event_signature(topic);
        let c = candidates(&index, &filter, 0..10).unwrap();
        assert_eq!(blocks(&c), vec![2, 3]);
    }

    #[test]
    fn multiple_topics_at_same_position_are_ored() {
        let t1 = B256::repeat_byte(0x01);
        let t2 = B256::repeat_byte(0x02);
        let index = MockIndex::default()
            .with_topic(t1, &[1, 3], 0..10)
            .with_topic(t2, &[3, 5], 0..10);
        let filter = Filter::new().event_signature(vec![t1, t2]);
        let c = candidates(&index, &filter, 0..10).unwrap();
        assert_eq!(blocks(&c), vec![1, 3, 5]);
    }

    #[test]
    fn coverage_is_intersected_across_groups() {
        let addr = Address::repeat_byte(0x01);
        let topic = B256::repeat_byte(0x42);
        let index = MockIndex::default()
            .with_address(addr, &[5], 0..10)
            .with_topic(topic, &[5], 4..12);
        let filter = Filter::new().address(addr).event_signature(topic);
        let c = candidates(&index, &filter, 0..12).unwrap();
        assert_eq!(c.covered, 4..10);
    }

    #[test]
    fn no_index_coverage_returns_empty_covered() {
        let addr = Address::repeat_byte(0x01);
        let index = MockIndex::default(); // no index entries → default (empty, 0..0)
        let filter = Filter::new().address(addr);
        let c = candidates(&index, &filter, 0..10).unwrap();
        assert!(c.bitmap.is_empty());
        assert_eq!(c.covered, 0..0);
    }
}
