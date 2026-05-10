//! videofuser-muxer: stateless MKV virtual reconstruction engine.
//!
//! The muxer is organized as four strictly separated layers (cap. 13):
//!
//! 1. [`layout`]: pure layout planning (no I/O, no state).
//! 2. [`materializer`]: reads raw bytes from track files, consulting
//!    [`DownloadState`](types::DownloadState) for availability.
//! 3. [`transformer`]: pure codec transforms (Annex B → AVCC for video).
//! 4. [`coordinator`]: the public [`Muxer`] entry point that orchestrates
//!    the layers behind a stateless `read(virtual_offset, length)` API.

pub mod coordinator;
pub mod ebml;
pub mod layout;
pub mod materializer;
pub mod transformer;
pub mod types;
pub mod vfr_index;

pub use coordinator::Muxer;
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
