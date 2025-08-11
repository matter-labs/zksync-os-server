//! Collection of types with lazy write caching behavior.
//!
//! Persistence still happens in the background. Writing to these types is usually fast provided
//! that the cache is not full, otherwise writing is blocked until enough data has been persisted to
//! free up cache.

mod repository;
pub use repository::RepositoryManager;
