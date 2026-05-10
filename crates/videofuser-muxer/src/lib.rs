//! videofuser-muxer: stateless MKV virtual reconstruction engine.
//!
//! Phase 4A: domain types and the [`DownloadState`] / [`RawFile`] traits.
//! Subsequent phases add layout planning, materialization, codec
//! transformation, and the read coordinator that ties them together.

pub mod types;

pub use types::{
    DiskRawFile, DownloadState, FullyAvailable, MemRawFile, MuxerError, RawFile, ReadPolicy,
    TrackSource,
};
