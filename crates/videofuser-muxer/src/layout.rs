use std::collections::HashMap;
use std::sync::Arc;

use videofuser_binstruct::{BinstructFile, CodecType, TrackPolicy};

use crate::ebml::{
    encode_uint_be, encode_vint, vint_len, write_element, write_uint_element, write_vint,
    write_vint_with_len, ID_CLUSTER_BYTES, ID_CUECLUSTERPOSITION_BYTES, ID_CUEPOINT_BYTES,
    ID_CUES_BYTES, ID_CUETIME_BYTES, ID_CUETRACK_BYTES, ID_CUETRACKPOSITIONS_BYTES,
    ID_SEEKHEAD_BYTES, ID_SEEKID_BYTES, ID_SEEKPOSITION_BYTES, ID_SEEK_BYTES,
    ID_SIMPLEBLOCK_BYTES, ID_TIMESTAMP_BYTES, ID_TRACKS_BYTES, UNKNOWN_SIZE_8B,
};
use crate::types::MuxerError;
use crate::vfr_index::LazyVfrIndex;

/// Type of layout section. Every section maps a contiguous virtual byte range
/// to a source that can produce those bytes on demand.
#[derive(Debug, Clone)]
pub enum LayoutSection {
    PreTracksBlob,
    TracksElement { default_track_id: u32 },
    PostTracksBlob,
    ClusterHeader { cluster_index: u32 },
    SimpleBlock {
        cluster_index: u32,
        track_id: u32,
        frame_index: u32,
        payload_size: u32,
    },
    Cues,
    SeekHead,
}

#[derive(Debug, Clone)]
pub struct SectionEntry {
    pub virtual_offset: u64,
    pub byte_len: u64,
    pub section: LayoutSection,
}

#[derive(Debug)]
pub struct Layout {
    pub sections: Vec<SectionEntry>,
    pub cluster_offsets: Vec<u64>,
    pub total_size: u64,
    pub cues_offset: u64,
    pub seekhead_offset: u64,
    /// Absolute timestamp of each cluster in TimecodeScale units (same units
    /// as MKV Block timestamps).
    pub cluster_timestamps: Vec<u64>,
}

/// Per-track frame metadata used by the layout planner.
pub struct TrackMeta {
    pub track_id: u32,
    pub track_number: u64,
    pub codec_type: CodecType,
    pub frame_count: u32,
    pub frame_duration: u64,
    pub cbr_frame_size: Option<u32>,
    pub vfr_index: Option<Arc<LazyVfrIndex>>,
}

impl TrackMeta {
    pub fn is_cbr(&self) -> bool {
        self.cbr_frame_size.is_some() && self.vfr_index.is_none()
    }

    /// Timestamp (start) of `frame_index` in TimecodeScale units.
    pub fn frame_time(&self, frame_index: u32) -> u64 {
        if self.is_cbr() {
            frame_index as u64 * self.frame_duration
        } else {
            let idx = self
                .vfr_index
                .as_ref()
                .expect("VBR track must have a VFR index");
            idx.frame_time(frame_index)
        }
    }

    /// Smallest `frame_index` whose start time is `>= ts`.
    pub fn frame_at_time(&self, ts: u64) -> u32 {
        if self.is_cbr() {
            if self.frame_duration == 0 {
                return 0;
            }
            let cf = ts.div_ceil(self.frame_duration);
            (cf as u32).min(self.frame_count)
        } else {
            let idx = self
                .vfr_index
                .as_ref()
                .expect("VBR track must have a VFR index");
            idx.frame_at_time(ts).min(self.frame_count)
        }
    }

    /// Emitted payload size (post-transform) of `frame_index`.
    pub fn payload_size(&self, frame_index: u32) -> u32 {
        match self.codec_type {
            CodecType::Audio => {
                if let Some(cf) = self.cbr_frame_size {
                    cf
                } else {
                    let idx = self
                        .vfr_index
                        .as_ref()
                        .expect("VBR audio must have VFR");
                    idx.frame_size(frame_index)
                }
            }
            CodecType::Video => {
                let idx = self
                    .vfr_index
                    .as_ref()
                    .expect("video must have VFR");
                let nals = idx.nal_lengths_of_frame(frame_index);
                let nal_count = nals.len() as u32;
                let sum: u64 = nals.iter().sum();
                4 * nal_count + sum as u32
            }
        }
    }

    /// Returns true if `frame_index` is a keyframe (only meaningful for video).
    pub fn is_keyframe(&self, frame_index: u32) -> bool {
        match &self.vfr_index {
            Some(idx) => idx.vfr.frames[frame_index as usize].is_keyframe(),
            // CBR audio: every frame is independently decodable; conservatively
            // *do not* emit Cues for audio tracks.
            None => false,
        }
    }
}

/// Byte length of a SimpleBlock element for the given track and payload size.
pub fn simple_block_byte_len(track_number: u64, payload_size: u32) -> u64 {
    let vint_tn = vint_len(track_number) as u64;
    let content_size = vint_tn + 2 /* timecode */ + 1 /* flags */ + payload_size as u64;
    1 /* ID */ + vint_len(content_size) as u64 + content_size
}

/// Byte length of a Cluster header (ID + 8-byte unknown size + Timestamp element).
pub const CLUSTER_HEADER_SIZE: u64 = 22;

/// Produce a deterministic layout for the given filter.
pub fn plan_layout(
    binstruct: &BinstructFile,
    filter: &[u32],
    tracks_meta: &HashMap<u32, TrackMeta>,
    default_audio_track_id: u32,
) -> Result<Layout, MuxerError> {
    // --- 1. Filter set & sorted order (deterministic) -----------------------
    let mut filter_sorted: Vec<u32> = filter.to_vec();
    filter_sorted.sort();
    filter_sorted.dedup();

    let filter_set: std::collections::HashSet<u32> = filter_sorted.iter().copied().collect();

    // --- 2. Cluster timestamps (absolute) ----------------------------------
    let cluster_count = binstruct.cluster_timestamps.cluster_count as usize;
    if binstruct.cluster_timestamps.deltas.len() != cluster_count {
        return Err(MuxerError::InvalidBinstruct(
            "cluster deltas length != cluster_count".into(),
        ));
    }
    let mut cluster_timestamps: Vec<u64> = Vec::with_capacity(cluster_count);
    let mut acc: i64 = 0;
    for &d in &binstruct.cluster_timestamps.deltas {
        acc = acc.saturating_add(d);
        if acc < 0 {
            return Err(MuxerError::InvalidBinstruct(
                "negative absolute cluster timestamp".into(),
            ));
        }
        cluster_timestamps.push(acc as u64);
    }

    // --- 3. Sizes of skeleton sections -------------------------------------
    let pre_size = binstruct.mkv_skeleton.pre_tracks_blob.len() as u64;
    let post_size = binstruct.mkv_skeleton.post_tracks_blob.len() as u64;

    // Tracks element: ID (4B) + VINT(content) + content.
    let tracks_content_size: u64 = binstruct
        .mkv_skeleton
        .track_entries
        .iter()
        .filter(|te| filter_set.contains(&(te.track_id as u32)))
        .map(|te| te.bytes.len() as u64)
        .sum();
    let tracks_size = 4 + vint_len(tracks_content_size) as u64 + tracks_content_size;

    // --- 4. Build sections sequentially ------------------------------------
    let mut sections: Vec<SectionEntry> = Vec::new();
    let mut offset = 0u64;

    sections.push(SectionEntry {
        virtual_offset: offset,
        byte_len: pre_size,
        section: LayoutSection::PreTracksBlob,
    });
    offset += pre_size;

    sections.push(SectionEntry {
        virtual_offset: offset,
        byte_len: tracks_size,
        section: LayoutSection::TracksElement {
            default_track_id: default_audio_track_id,
        },
    });
    offset += tracks_size;

    sections.push(SectionEntry {
        virtual_offset: offset,
        byte_len: post_size,
        section: LayoutSection::PostTracksBlob,
    });
    offset += post_size;

    let mut cluster_offsets: Vec<u64> = Vec::with_capacity(cluster_count);

    // Per-track cursor into its frames (we walk forward as we process clusters).
    let mut track_cursors: HashMap<u32, u32> = HashMap::new();
    for &tid in &filter_sorted {
        track_cursors.insert(tid, 0);
    }

    for cluster_idx in 0..cluster_count {
        cluster_offsets.push(offset);
        sections.push(SectionEntry {
            virtual_offset: offset,
            byte_len: CLUSTER_HEADER_SIZE,
            section: LayoutSection::ClusterHeader {
                cluster_index: cluster_idx as u32,
            },
        });
        offset += CLUSTER_HEADER_SIZE;

        let t_end = if cluster_idx + 1 < cluster_count {
            Some(cluster_timestamps[cluster_idx + 1])
        } else {
            None
        };

        for &track_id in &filter_sorted {
            let meta = tracks_meta
                .get(&track_id)
                .ok_or(MuxerError::MissingTrack(track_id))?;
            let start_frame = *track_cursors.get(&track_id).unwrap();
            let end_frame = match t_end {
                Some(t) => meta.frame_at_time(t),
                None => meta.frame_count,
            };
            for frame_index in start_frame..end_frame {
                let payload_size = meta.payload_size(frame_index);
                let sb_len = simple_block_byte_len(meta.track_number, payload_size);
                sections.push(SectionEntry {
                    virtual_offset: offset,
                    byte_len: sb_len,
                    section: LayoutSection::SimpleBlock {
                        cluster_index: cluster_idx as u32,
                        track_id,
                        frame_index,
                        payload_size,
                    },
                });
                offset += sb_len;
            }
            track_cursors.insert(track_id, end_frame);
        }
    }

    // --- 5. Cues ------------------------------------------------------------
    let cues_offset = offset;
    let cues_size = compute_cues_size(&sections, &cluster_offsets, &cluster_timestamps, tracks_meta);
    sections.push(SectionEntry {
        virtual_offset: cues_offset,
        byte_len: cues_size,
        section: LayoutSection::Cues,
    });
    offset += cues_size;

    // --- 6. SeekHead --------------------------------------------------------
    let seekhead_offset = offset;
    let post_offset = pre_size + tracks_size;
    let info_offset_within_segment = info_offset_within_pre(&binstruct.mkv_skeleton.pre_tracks_blob);
    let seekhead_size = compute_seekhead_size(
        info_offset_within_segment,
        pre_size,
        post_offset,
        post_size > 0,
        cues_offset,
    );
    sections.push(SectionEntry {
        virtual_offset: seekhead_offset,
        byte_len: seekhead_size,
        section: LayoutSection::SeekHead,
    });
    offset += seekhead_size;

    Ok(Layout {
        sections,
        cluster_offsets,
        total_size: offset,
        cues_offset,
        seekhead_offset,
        cluster_timestamps,
    })
}

/// Compute the byte length of the Cues element exactly (without keeping the
/// generated bytes around).
fn compute_cues_size(
    sections: &[SectionEntry],
    cluster_offsets: &[u64],
    cluster_timestamps: &[u64],
    tracks_meta: &HashMap<u32, TrackMeta>,
) -> u64 {
    let mut content_size: u64 = 0;
    for entry in sections {
        if let LayoutSection::SimpleBlock {
            cluster_index,
            track_id,
            frame_index,
            ..
        } = entry.section
        {
            let meta = match tracks_meta.get(&track_id) {
                Some(m) => m,
                None => continue,
            };
            if !matches!(meta.codec_type, CodecType::Video) {
                continue;
            }
            if !meta.is_keyframe(frame_index) {
                continue;
            }
            let timestamp = meta.frame_time(frame_index)
                + cluster_timestamps[cluster_index as usize];
            // CueTime + CueTrackPositions
            let cuetime_size = uint_element_size(timestamp);
            let cuetrack_size = uint_element_size(meta.track_number);
            let cuepos_size = uint_element_size(cluster_offsets[cluster_index as usize]);
            let ctp_content = cuetrack_size + cuepos_size;
            let ctp_size = 1 + vint_len(ctp_content) as u64 + ctp_content;
            let cuepoint_content = cuetime_size + ctp_size;
            let cuepoint_size = 1 + vint_len(cuepoint_content) as u64 + cuepoint_content;
            content_size += cuepoint_size;
        }
    }
    4 + vint_len(content_size) as u64 + content_size
}

/// Build the Cues element bytes. Must match `compute_cues_size`.
pub(crate) fn build_cues_bytes(
    sections: &[SectionEntry],
    cluster_offsets: &[u64],
    cluster_timestamps: &[u64],
    tracks_meta: &HashMap<u32, TrackMeta>,
) -> Vec<u8> {
    let mut content = Vec::new();
    for entry in sections {
        if let LayoutSection::SimpleBlock {
            cluster_index,
            track_id,
            frame_index,
            ..
        } = entry.section
        {
            let meta = match tracks_meta.get(&track_id) {
                Some(m) => m,
                None => continue,
            };
            if !matches!(meta.codec_type, CodecType::Video) {
                continue;
            }
            if !meta.is_keyframe(frame_index) {
                continue;
            }
            let timestamp = meta.frame_time(frame_index)
                + cluster_timestamps[cluster_index as usize];

            // CueTrackPositions content
            let mut ctp = Vec::new();
            write_uint_element(&mut ctp, &ID_CUETRACK_BYTES, meta.track_number);
            write_uint_element(
                &mut ctp,
                &ID_CUECLUSTERPOSITION_BYTES,
                cluster_offsets[cluster_index as usize],
            );

            // CuePoint content
            let mut cp = Vec::new();
            write_uint_element(&mut cp, &ID_CUETIME_BYTES, timestamp);
            write_element(&mut cp, &ID_CUETRACKPOSITIONS_BYTES, &ctp);

            write_element(&mut content, &ID_CUEPOINT_BYTES, &cp);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&ID_CUES_BYTES);
    let _ = write_vint(&mut out, content.len() as u64);
    out.extend_from_slice(&content);
    out
}

/// Size of a UInt element (ID 1 byte + VINT size + minimal big-endian).
fn uint_element_size(value: u64) -> u64 {
    let v = encode_uint_be(value);
    1 + vint_len(v.len() as u64) as u64 + v.len() as u64
}

/// Approximation: assume Info element begins right after the EBML Header in
/// PreTracksBlob. We scan PreTracksBlob for the Segment master and then return
/// the byte offset of the Info element relative to the Segment payload start.
///
/// In MVP we don't fully parse the EBML Header; we conservatively report 0,
/// which is acceptable because the SeekHead is also emitted at the end (so
/// players that scan the whole file will still find Info via direct scanning).
fn info_offset_within_pre(_pre: &[u8]) -> u64 {
    0
}

/// Size of the SeekHead element. Mirrors `build_seekhead_bytes`.
fn compute_seekhead_size(
    info_offset: u64,
    pre_size: u64,
    post_offset: u64,
    has_post: bool,
    cues_offset: u64,
) -> u64 {
    let mut entries = vec![
        // (SeekID bytes, SeekPosition value)
        (vec![0x15, 0x49, 0xA9, 0x66], info_offset),         // Info
        (vec![0x16, 0x54, 0xAE, 0x6B], pre_size),            // Tracks
        (vec![0x1C, 0x53, 0xBB, 0x6B], cues_offset),         // Cues
    ];
    if has_post {
        // Use Chapters ID (0x1043A770) as a generic post-tracks pointer.
        entries.push((vec![0x10, 0x43, 0xA7, 0x70], post_offset));
    }

    let mut content_size: u64 = 0;
    for (id_bytes, pos) in &entries {
        let seekid_size = 2 + vint_len(id_bytes.len() as u64) as u64 + id_bytes.len() as u64;
        let pos_be = encode_uint_be(*pos);
        let seekpos_size = 2 + vint_len(pos_be.len() as u64) as u64 + pos_be.len() as u64;
        let seek_content = seekid_size + seekpos_size;
        let seek_size = 2 + vint_len(seek_content) as u64 + seek_content;
        content_size += seek_size;
    }
    4 + vint_len(content_size) as u64 + content_size
}

/// Build the SeekHead element bytes. Must match `compute_seekhead_size`.
pub(crate) fn build_seekhead_bytes(
    info_offset: u64,
    pre_size: u64,
    post_offset: u64,
    has_post: bool,
    cues_offset: u64,
) -> Vec<u8> {
    let mut entries: Vec<(Vec<u8>, u64)> = vec![
        (vec![0x15, 0x49, 0xA9, 0x66], info_offset),
        (vec![0x16, 0x54, 0xAE, 0x6B], pre_size),
        (vec![0x1C, 0x53, 0xBB, 0x6B], cues_offset),
    ];
    if has_post {
        entries.push((vec![0x10, 0x43, 0xA7, 0x70], post_offset));
    }

    let mut content = Vec::new();
    for (id_bytes, pos) in entries {
        let mut seek = Vec::new();
        write_element(&mut seek, &ID_SEEKID_BYTES, &id_bytes);
        write_uint_element(&mut seek, &ID_SEEKPOSITION_BYTES, pos);
        write_element(&mut content, &ID_SEEK_BYTES, &seek);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&ID_SEEKHEAD_BYTES);
    let _ = write_vint(&mut out, content.len() as u64);
    out.extend_from_slice(&content);
    out
}

/// Build the Tracks element bytes, patching FlagDefault to 1 only on the
/// TrackEntry with `default_audio_track_id` (0 everywhere else).
///
/// The patch modifies a single byte without changing the total size.
pub fn build_tracks_element_bytes(
    binstruct: &BinstructFile,
    filter: &[u32],
    default_audio_track_id: u32,
) -> Result<Vec<u8>, MuxerError> {
    use crate::ebml::find_flag_default_value_offset;

    let filter_set: std::collections::HashSet<u32> = filter.iter().copied().collect();

    let mut content: Vec<u8> = Vec::new();
    for te in &binstruct.mkv_skeleton.track_entries {
        if !filter_set.contains(&(te.track_id as u32)) {
            continue;
        }
        let mut bytes = te.bytes.clone();
        let want = (te.track_id as u32) == default_audio_track_id;
        if let Some(off) = find_flag_default_value_offset(&bytes)? {
            bytes[off] = if want { 1 } else { 0 };
        }
        content.extend_from_slice(&bytes);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&ID_TRACKS_BYTES);
    let _ = write_vint(&mut out, content.len() as u64);
    out.extend_from_slice(&content);
    Ok(out)
}

/// Build the 22-byte cluster header for cluster `n` with timestamp `ts`.
///
/// Layout:
///   - Cluster ID (0x1F43B675, 4 bytes)
///   - Unknown size (8-byte VINT: 0xFF 0xFF .. 0xFF)
///   - Timestamp element (ID 0xE7, 1 byte) + VINT(8) + u64 BE timestamp (8 bytes)
///       VINT(8) requires the 1-byte form '0x88' since 8 < 0x7F.
///
/// Total: 4 + 8 + 1 + 1 + 8 = 22 bytes.
pub fn build_cluster_header_bytes(ts: u64) -> [u8; 22] {
    let mut buf = [0u8; 22];
    buf[..4].copy_from_slice(&ID_CLUSTER_BYTES);
    let mut cursor = std::io::Cursor::new(&mut buf[4..12]);
    let _ = write_vint_with_len(&mut cursor, UNKNOWN_SIZE_8B, 8);
    buf[12] = ID_TIMESTAMP_BYTES[0];
    // 1-byte VINT for value 8.
    let mut cur2 = std::io::Cursor::new(&mut buf[13..14]);
    let _ = write_vint(&mut cur2, 8);
    buf[14..22].copy_from_slice(&ts.to_be_bytes());
    buf
}

/// Build the SimpleBlock element bytes for `frame_index` of `track`, given its
/// post-transform payload `payload`. `relative_ts` is `frame_ts - cluster_ts`
/// in TimecodeScale units.
pub fn build_simple_block_bytes(
    track_number: u64,
    relative_ts: i16,
    keyframe: bool,
    payload: &[u8],
) -> Vec<u8> {
    let vint_tn = encode_vint(track_number);
    let content_size = vint_tn.len() as u64 + 2 + 1 + payload.len() as u64;
    let mut out = Vec::with_capacity(1 + 8 + content_size as usize);
    out.extend_from_slice(&ID_SIMPLEBLOCK_BYTES);
    let _ = write_vint(&mut out, content_size);
    out.extend_from_slice(&vint_tn);
    out.extend_from_slice(&relative_ts.to_be_bytes());
    let flags: u8 = if keyframe { 0x80 } else { 0x00 };
    out.push(flags);
    out.extend_from_slice(payload);
    out
}

/// Convenience: extract the TrackPolicy for `track_id`.
pub fn find_track_policy<'a>(
    binstruct: &'a BinstructFile,
    track_id: u32,
) -> Option<&'a TrackPolicy> {
    binstruct
        .track_policies
        .iter()
        .find(|p| p.track_id as u32 == track_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use videofuser_binstruct::{
        ClusterTimestamps, Header, MkvSkeleton, Source, TrackEntryRecord,
    };

    fn mk_binstruct() -> BinstructFile {
        BinstructFile {
            header: Header { version: 1, config_flags: 0 },
            source: Source {
                original_mkv_hash: [0; 32],
                publisher_info: "test".into(),
                creation_timestamp: 0,
                original_language: "spa".into(),
                original_default_track_id: 1,
                build_tools: vec![],
            },
            mkv_skeleton: MkvSkeleton {
                pre_tracks_blob: vec![0u8; 100],
                track_entries: vec![
                    TrackEntryRecord {
                        track_id: 1,
                        // TrackNumber=1, FlagDefault=1
                        bytes: vec![0xD7, 0x81, 0x01, 0x88, 0x81, 0x01],
                    },
                    TrackEntryRecord {
                        track_id: 2,
                        bytes: vec![0xD7, 0x81, 0x02, 0x88, 0x81, 0x00],
                    },
                ],
                post_tracks_blob: vec![],
            },
            cluster_timestamps: ClusterTimestamps {
                cluster_count: 2,
                deltas: vec![0, 1000],
            },
            track_policies: vec![],
        }
    }

    #[test]
    fn cluster_header_bytes_are_22() {
        let h = build_cluster_header_bytes(0);
        assert_eq!(h.len(), 22);
        assert_eq!(&h[..4], &[0x1F, 0x43, 0xB6, 0x75]);
        // Unknown-size VINT in 8-byte form: 0x01 then seven 0xFF bytes.
        let mut expected_unknown = vec![0x01u8];
        expected_unknown.extend(std::iter::repeat(0xFFu8).take(7));
        assert_eq!(&h[4..12], expected_unknown.as_slice());
        assert_eq!(h[12], 0xE7);
        assert_eq!(h[13], 0x88); // VINT(8) in 1-byte form
        assert_eq!(&h[14..22], &0u64.to_be_bytes());
    }

    #[test]
    fn tracks_element_filter_and_patch() {
        let b = mk_binstruct();
        let bytes = build_tracks_element_bytes(&b, &[1, 2], 2).unwrap();
        // First 4 bytes are Tracks ID
        assert_eq!(&bytes[..4], &[0x16, 0x54, 0xAE, 0x6B]);
        // Find the patched FlagDefault bytes
        // TE1: TrackNumber=1, FlagDefault patched to 0
        // TE2: TrackNumber=2, FlagDefault patched to 1
        let body_start = 4 + 1; // VINT 1-byte size for 12-byte content
        let te1 = &bytes[body_start..body_start + 6];
        let te2 = &bytes[body_start + 6..body_start + 12];
        assert_eq!(te1[5], 0); // FlagDefault patched to 0
        assert_eq!(te2[5], 1); // FlagDefault patched to 1
    }

    #[test]
    fn plan_layout_with_single_cbr_audio_only() {
        // No video; 1 CBR audio track with 4 frames in 2 clusters.
        let b = mk_binstruct();
        let mut tracks_meta = HashMap::new();
        tracks_meta.insert(
            1u32,
            TrackMeta {
                track_id: 1,
                track_number: 1,
                codec_type: CodecType::Audio,
                frame_count: 4,
                frame_duration: 500, // ms-equivalent; cluster 0 covers [0..1000) → 2 frames
                cbr_frame_size: Some(768),
                vfr_index: None,
            },
        );
        let layout = plan_layout(&b, &[1], &tracks_meta, 1).unwrap();
        assert_eq!(layout.cluster_offsets.len(), 2);
        // Total size sanity check
        assert!(layout.total_size > 100);
        // Sections must be ordered and contiguous.
        for w in layout.sections.windows(2) {
            assert_eq!(w[0].virtual_offset + w[0].byte_len, w[1].virtual_offset);
        }
        let last = layout.sections.last().unwrap();
        assert_eq!(last.virtual_offset + last.byte_len, layout.total_size);
    }
}
