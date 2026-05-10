//! videofuser-muxer: stateless MKV virtual reconstruction engine.
//!
//! Phases 4A–4B: domain types plus the pure layout planner that computes a
//! deterministic byte layout for the MKV virtual file given a filter.

pub mod ebml;
pub mod layout;
pub mod types;
pub mod vfr_index;

pub use layout::{
    build_cluster_header_bytes, build_simple_block_bytes, build_tracks_element_bytes,
    plan_layout, Layout, LayoutSection, SectionEntry, TrackMeta,
};
pub use types::{
    DiskRawFile, DownloadState, FullyAvailable, MemRawFile, MuxerError, RawFile, ReadPolicy,
    TrackSource,
};
pub use vfr_index::LazyVfrIndex;
