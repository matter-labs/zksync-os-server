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
pub mod has_block_range_end;
pub mod peekable_receiver;
pub mod send_and_record;
pub mod traits;

pub use builder::Pipeline;
pub use component_id::ComponentId;
pub use has_block_range_end::HasBlockRangeEnd;
pub use peekable_receiver::PeekableReceiver;
pub use send_and_record::{PipelineSendError, SendAndRecordExt};
pub use traits::PipelineComponent;
