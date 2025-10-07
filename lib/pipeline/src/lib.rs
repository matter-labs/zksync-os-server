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
pub mod peekable_receiver;
pub mod traits;

pub use builder::{Pipeline, PipelineWithOutput};
pub use peekable_receiver::PeekableReceiver;
pub use traits::{PipelineComponent, Sink, Source};
