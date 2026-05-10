use std::collections::HashMap;
use std::sync::Arc;

use videofuser_binstruct::{BinstructFile, CodecType};

use crate::ebml::find_track_number;
use crate::layout::{
    build_cluster_header_bytes, build_cues_bytes, build_seekhead_bytes,
    build_simple_block_bytes, build_tracks_element_bytes, plan_layout, Layout, LayoutSection,
    SectionEntry, TrackMeta,
};
use crate::materializer::{compute_offset_and_len, materialize_frame, MaterializedFrame};
use crate::transformer::{transform_avcc, transform_raw};
use crate::types::{DownloadState, MuxerError, ReadPolicy, TrackSource};
use crate::vfr_index::LazyVfrIndex;

/// Stateless MKV virtual reconstruction engine.
///
/// The muxer is constructed once with a given `(binstruct, sources, filter,
/// default_audio_track_id)` tuple. Each `read()` call is fully self-contained
/// and may be issued concurrently from multiple threads (H.1).
pub struct Muxer {
    binstruct: Arc<BinstructFile>,
    sources: HashMap<u32, TrackSource>,
    tracks_meta: HashMap<u32, TrackMeta>,
    layout: Layout,
    tracks_element_bytes: Vec<u8>,
    cues_bytes: Vec<u8>,
    seekhead_bytes: Vec<u8>,
    policy: ReadPolicy,
    download: Arc<dyn DownloadState>,
}

impl Muxer {
    pub fn new(
        binstruct: Arc<BinstructFile>,
        sources: HashMap<u32, TrackSource>,
        filter: Vec<u32>,
        default_audio_track_id: u32,
        policy: ReadPolicy,
        download: Arc<dyn DownloadState>,
    ) -> Result<Self, MuxerError> {
        // --- Build TrackMeta for each track in filter -----------------------
        let mut tracks_meta: HashMap<u32, TrackMeta> = HashMap::new();
        for &tid in &filter {
            let policy = binstruct
                .track_policies
                .iter()
                .find(|p| p.track_id as u32 == tid)
                .ok_or(MuxerError::MissingTrack(tid))?;

            let track_number = binstruct
                .mkv_skeleton
                .track_entries
                .iter()
                .find(|te| te.track_id as u32 == tid)
                .and_then(|te| find_track_number(&te.bytes).ok().flatten())
                .unwrap_or(tid as u64);

            let vfr_index = sources
                .get(&tid)
                .and_then(|s| s.vfr.as_ref())
                .map(|v| Arc::new(LazyVfrIndex::new(v.clone(), policy.frame_duration)));

            tracks_meta.insert(
                tid,
                TrackMeta {
                    track_id: tid,
                    track_number,
                    codec_type: policy.codec_type.clone(),
                    frame_count: policy.frame_count as u32,
                    frame_duration: policy.frame_duration,
                    cbr_frame_size: policy.cbr_frame_size.map(|v| v as u32),
                    vfr_index,
                },
            );
        }

        // --- Plan layout ----------------------------------------------------
        let layout = plan_layout(&binstruct, &filter, &tracks_meta, default_audio_track_id)?;

        // --- Pre-built byte buffers ----------------------------------------
        let tracks_element_bytes =
            build_tracks_element_bytes(&binstruct, &filter, default_audio_track_id)?;

        let cues_bytes = build_cues_bytes(
            &layout.sections,
            &layout.cluster_offsets,
            &layout.cluster_timestamps,
            &tracks_meta,
        );

        let pre_size = binstruct.mkv_skeleton.pre_tracks_blob.len() as u64;
        let post_size = binstruct.mkv_skeleton.post_tracks_blob.len() as u64;
        let tracks_size = tracks_element_bytes.len() as u64;
        let seekhead_bytes = build_seekhead_bytes(
            0, // Info offset within segment (MVP approximation)
            pre_size,
            pre_size + tracks_size, // post-tracks offset
            post_size > 0,
            layout.cues_offset,
        );

        // --- Sanity-check that prebuilt sizes match what the planner claimed ---
        debug_assert_eq!(
            tracks_element_bytes.len() as u64,
            layout
                .sections
                .iter()
                .find(|s| matches!(s.section, LayoutSection::TracksElement { .. }))
                .map(|s| s.byte_len)
                .unwrap_or(0)
        );
        debug_assert_eq!(
            cues_bytes.len() as u64,
            layout
                .sections
                .iter()
                .find(|s| matches!(s.section, LayoutSection::Cues))
                .map(|s| s.byte_len)
                .unwrap_or(0)
        );
        debug_assert_eq!(
            seekhead_bytes.len() as u64,
            layout
                .sections
                .iter()
                .find(|s| matches!(s.section, LayoutSection::SeekHead))
                .map(|s| s.byte_len)
                .unwrap_or(0)
        );

        Ok(Self {
            binstruct,
            sources,
            tracks_meta,
            layout,
            tracks_element_bytes,
            cues_bytes,
            seekhead_bytes,
            policy,
            download,
        })
    }

    pub fn total_size(&self) -> u64 {
        self.layout.total_size
    }

    /// Returns the cluster_offsets array (used by tests and debug tools).
    pub fn cluster_offsets(&self) -> &[u64] {
        &self.layout.cluster_offsets
    }

    /// Stateless read: returns the bytes for `[virtual_offset, virtual_offset+length)`.
    ///
    /// May be called concurrently from any number of threads. Output length is
    /// exactly `length` bytes, unless `virtual_offset + length > total_size`,
    /// in which case the read is clamped.
    pub fn read(&self, virtual_offset: u64, length: u32) -> Result<Vec<u8>, MuxerError> {
        let total = self.layout.total_size;
        if virtual_offset >= total {
            return Ok(Vec::new());
        }
        let req_end = (virtual_offset + length as u64).min(total);
        let want = (req_end - virtual_offset) as usize;
        let mut out = Vec::with_capacity(want);

        // Find first section that overlaps [virtual_offset, req_end).
        let sections = &self.layout.sections;
        let mut idx = sections.partition_point(|e| e.virtual_offset + e.byte_len <= virtual_offset);

        while idx < sections.len() && (out.len() as u64) < (want as u64) {
            let entry = &sections[idx];
            if entry.virtual_offset >= req_end {
                break;
            }
            let section_end = entry.virtual_offset + entry.byte_len;
            let overlap_start = virtual_offset.max(entry.virtual_offset);
            let overlap_end = req_end.min(section_end);
            let local_start = (overlap_start - entry.virtual_offset) as usize;
            let local_end = (overlap_end - entry.virtual_offset) as usize;
            self.append_section_range(entry, local_start, local_end, &mut out)?;
            idx += 1;
        }

        Ok(out)
    }

    fn append_section_range(
        &self,
        entry: &SectionEntry,
        local_start: usize,
        local_end: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), MuxerError> {
        match &entry.section {
            LayoutSection::PreTracksBlob => {
                let src = &self.binstruct.mkv_skeleton.pre_tracks_blob;
                out.extend_from_slice(&src[local_start..local_end]);
            }
            LayoutSection::TracksElement { .. } => {
                out.extend_from_slice(&self.tracks_element_bytes[local_start..local_end]);
            }
            LayoutSection::PostTracksBlob => {
                let src = &self.binstruct.mkv_skeleton.post_tracks_blob;
                out.extend_from_slice(&src[local_start..local_end]);
            }
            LayoutSection::ClusterHeader { cluster_index } => {
                let ts = self.layout.cluster_timestamps[*cluster_index as usize];
                let bytes = build_cluster_header_bytes(ts);
                out.extend_from_slice(&bytes[local_start..local_end]);
            }
            LayoutSection::SimpleBlock {
                cluster_index,
                track_id,
                frame_index,
                payload_size,
            } => {
                let bytes = self.build_simple_block(
                    *cluster_index,
                    *track_id,
                    *frame_index,
                    *payload_size,
                )?;
                debug_assert_eq!(bytes.len(), entry.byte_len as usize);
                out.extend_from_slice(&bytes[local_start..local_end]);
            }
            LayoutSection::Cues => {
                out.extend_from_slice(&self.cues_bytes[local_start..local_end]);
            }
            LayoutSection::SeekHead => {
                out.extend_from_slice(&self.seekhead_bytes[local_start..local_end]);
            }
        }
        Ok(())
    }

    fn build_simple_block(
        &self,
        cluster_index: u32,
        track_id: u32,
        frame_index: u32,
        payload_size: u32,
    ) -> Result<Vec<u8>, MuxerError> {
        let meta = self
            .tracks_meta
            .get(&track_id)
            .ok_or(MuxerError::MissingTrack(track_id))?;
        let source = self
            .sources
            .get(&track_id)
            .ok_or(MuxerError::MissingTrack(track_id))?;
        let cluster_ts = self.layout.cluster_timestamps[cluster_index as usize];
        let frame_ts = meta.frame_time(frame_index);
        let relative = frame_ts as i64 - cluster_ts as i64;
        if !(-32768..=32767).contains(&relative) {
            return Err(MuxerError::InvalidBinstruct(format!(
                "SimpleBlock relative timestamp {relative} overflows i16 for track {track_id} frame {frame_index}"
            )));
        }
        let relative = relative as i16;

        let keyframe = match meta.codec_type {
            CodecType::Video => meta.is_keyframe(frame_index),
            CodecType::Audio => true, // audio frames are independently decodable
        };

        let vfr_index = meta.vfr_index.as_deref();

        // Step 1: materialize raw bytes.
        let materialized = materialize_frame(
            source,
            frame_index,
            vfr_index,
            self.download.as_ref(),
            self.policy,
        )?;

        // Step 2: transform.
        let payload: Vec<u8> = match materialized {
            MaterializedFrame::Bytes(raw) => match meta.codec_type {
                CodecType::Audio => transform_raw(&raw),
                CodecType::Video => {
                    let idx = vfr_index.ok_or_else(|| {
                        MuxerError::InvalidBinstruct(format!(
                            "video track {track_id} requires VFR for AVCC transform"
                        ))
                    })?;
                    let nal_lengths = idx.nal_lengths_of_frame(frame_index);
                    transform_avcc(&raw, nal_lengths)?
                }
            },
            MaterializedFrame::Unavailable { .. } => vec![0u8; payload_size as usize],
        };

        if payload.len() != payload_size as usize {
            return Err(MuxerError::InvalidBitstream(format!(
                "transformed payload size mismatch: expected {payload_size} got {} for track {track_id} frame {frame_index}",
                payload.len(),
            )));
        }

        Ok(build_simple_block_bytes(
            meta.track_number,
            relative,
            keyframe,
            &payload,
        ))
    }

    /// Returns the raw byte length of frame `frame_index` of track `track_id`
    /// in its raw file (helper used by tests).
    pub fn raw_frame_len(&self, track_id: u32, frame_index: u32) -> Result<u32, MuxerError> {
        let source = self
            .sources
            .get(&track_id)
            .ok_or(MuxerError::MissingTrack(track_id))?;
        let meta = self
            .tracks_meta
            .get(&track_id)
            .ok_or(MuxerError::MissingTrack(track_id))?;
        let (_off, len) = compute_offset_and_len(source, frame_index, meta.vfr_index.as_deref())?;
        Ok(len)
    }
}
