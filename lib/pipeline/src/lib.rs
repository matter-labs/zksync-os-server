//! ZKsync OS Pipeline Framework
//!
//! This crate provides traits and utilities for building type-safe, composable
//! component pipelines. It's designed specifically for ZKsync OS's async
//! component orchestration needs.
//!
//! # Core Concepts
//!
//! - **Source**: Components that generate messages (command producers)
//! - **PipelineComponent**: Components that transform messages (e.g., batchers, provers)
//! - **Sink**: End of pipeline (e.g. BatchSink)

pub mod builder;
pub mod component_id;
pub mod has_block_seq;
pub mod peekable_receiver;
pub mod tracked_channel;
pub mod traits;

pub use builder::Pipeline;
pub use component_id::ComponentId;
pub use has_block_seq::HasBlockSeq;
pub use peekable_receiver::PeekableReceiver;
pub use tracked_channel::{
    TrackedUnboundedReceiver, TrackedUnboundedSender, tracked_unbounded_channel,
};
pub use traits::PipelineComponent;
