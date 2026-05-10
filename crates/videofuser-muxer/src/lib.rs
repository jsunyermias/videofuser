//! videofuser-muxer: stateless MKV virtual reconstruction engine.
//!
//! Phases 4A–4D: domain types, pure layout planner, track materializer, and
//! the pure codec transformer (Annex B → AVCC for video, identity for audio).

pub mod ebml;
pub mod layout;
pub mod materializer;
pub mod transformer;
pub mod types;
pub mod vfr_index;

pub use layout::{
    build_cluster_header_bytes, build_simple_block_bytes, build_tracks_element_bytes,
    plan_layout, Layout, LayoutSection, SectionEntry, TrackMeta,
};
pub use materializer::{compute_offset_and_len, materialize_frame, MaterializedFrame};
pub use transformer::{transform_avcc, transform_raw};
pub use types::{
    DiskRawFile, DownloadState, FullyAvailable, MemRawFile, MuxerError, RawFile, ReadPolicy,
    TrackSource,
};
pub use vfr_index::LazyVfrIndex;
