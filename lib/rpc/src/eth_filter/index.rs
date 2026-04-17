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
pub(crate) fn candidates(
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
    pub bitmap: RoaringBitmap,
    pub covered: Range<u64>,
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
