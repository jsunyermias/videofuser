//! videofuser-muxer: stateless MKV virtual reconstruction engine.
//!
//! Phases 4A–4C: domain types, pure layout planner, and track materializer
//! (reads raw bytes from track files, consulting `DownloadState` for
//! availability).

pub mod ebml;
pub mod layout;
pub mod materializer;
pub mod types;
pub mod vfr_index;

pub use layout::{
    build_cluster_header_bytes, build_simple_block_bytes, build_tracks_element_bytes,
    plan_layout, Layout, LayoutSection, SectionEntry, TrackMeta,
};
pub use materializer::{compute_offset_and_len, materialize_frame, MaterializedFrame};
pub use types::{
    DiskRawFile, DownloadState, FullyAvailable, MemRawFile, MuxerError, RawFile, ReadPolicy,
    TrackSource,
};
pub use vfr_index::LazyVfrIndex;
