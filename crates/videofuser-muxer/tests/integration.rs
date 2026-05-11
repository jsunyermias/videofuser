//! Integration tests for the muxer (4F).
//!
//! These tests synthesize a small but structurally valid MKV virtual layout
//! and validate the four layers end-to-end. The ffprobe-based validation is
//! gated behind `#[ignore]` because the binary may not be present in CI
//! environments — run it with `cargo test -p videofuser-muxer -- --include-ignored`.

use std::collections::HashMap;
use std::sync::Arc;

use videofuser_binstruct::{
    BinstructFile, ClusterTimestamps, CodecType, Header, MkvSkeleton, Source, ToolEntry,
    TrackEntryRecord, TrackPolicy,
};
use videofuser_muxer::{
    transform_avcc, FullyAvailable, MemRawFile, Muxer, ReadPolicy, TrackSource,
};
use videofuser_vfr::{FrameRecord, VfrFile};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal EBML Header bytes (`1A 45 DF A3 ...`) declaring DocType "matroska"
/// and version 4. Hardcoded; sufficient for ffprobe to identify the file.
fn ebml_header_bytes() -> Vec<u8> {
    let mut content = Vec::new();
    // EBMLVersion (0x4286) = 1
    content.extend_from_slice(&[0x42, 0x86, 0x81, 0x01]);
    // EBMLReadVersion (0x42F7) = 1
    content.extend_from_slice(&[0x42, 0xF7, 0x81, 0x01]);
    // EBMLMaxIDLength (0x42F2) = 4
    content.extend_from_slice(&[0x42, 0xF2, 0x81, 0x04]);
    // EBMLMaxSizeLength (0x42F3) = 8
    content.extend_from_slice(&[0x42, 0xF3, 0x81, 0x08]);
    // DocType (0x4282) = "matroska"
    content.extend_from_slice(&[0x42, 0x82, 0x88]);
    content.extend_from_slice(b"matroska");
    // DocTypeVersion (0x4287) = 4
    content.extend_from_slice(&[0x42, 0x87, 0x81, 0x04]);
    // DocTypeReadVersion (0x4285) = 2
    content.extend_from_slice(&[0x42, 0x85, 0x81, 0x02]);

    let mut out = Vec::new();
    out.extend_from_slice(&[0x1A, 0x45, 0xDF, 0xA3]); // EBML ID
    // 1-byte VINT size for content (which is < 127)
    assert!(content.len() < 0x80);
    out.push(0x80 | content.len() as u8);
    out.extend_from_slice(&content);
    out
}

/// Minimal Segment header (0x18538067) with unknown size (8-byte form), then
/// a minimal Info element with TimecodeScale = 1000000 (1 ms).
fn segment_and_info_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    // Segment ID + 8-byte unknown size
    out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
    out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);

    // Info element content
    let mut info = Vec::new();
    // TimecodeScale (0x2AD7B1) = 1000000
    info.extend_from_slice(&[0x2A, 0xD7, 0xB1, 0x83]); // ID + VINT size 3
    info.extend_from_slice(&[0x0F, 0x42, 0x40]); // 1000000 BE
    // MuxingApp (0x4D80) = "videofuser-test"
    let muxer = b"videofuser-test";
    info.extend_from_slice(&[0x4D, 0x80, 0x80 | muxer.len() as u8]);
    info.extend_from_slice(muxer);
    // WritingApp (0x5741) = "videofuser-test"
    let writer = b"videofuser-test";
    info.extend_from_slice(&[0x57, 0x41, 0x80 | writer.len() as u8]);
    info.extend_from_slice(writer);

    // Wrap Info
    out.extend_from_slice(&[0x15, 0x49, 0xA9, 0x66]);
    assert!(info.len() < 0x80);
    out.push(0x80 | info.len() as u8);
    out.extend_from_slice(&info);

    out
}

/// Build a TrackEntry blob for an audio track (CBR, codec A_PCM/INT/LIT).
fn audio_track_entry(track_number: u8, default: bool) -> Vec<u8> {
    let mut body = Vec::new();
    // TrackNumber (0xD7)
    body.extend_from_slice(&[0xD7, 0x81, track_number]);
    // TrackUID (0x73C5)
    body.extend_from_slice(&[0x73, 0xC5, 0x81, track_number * 10]);
    // TrackType (0x83) = 2 (audio)
    body.extend_from_slice(&[0x83, 0x81, 0x02]);
    // FlagDefault (0x88)
    body.extend_from_slice(&[0x88, 0x81, if default { 1 } else { 0 }]);
    // CodecID (0x86) = "A_PCM/INT/LIT"
    let codec = b"A_PCM/INT/LIT";
    body.extend_from_slice(&[0x86, 0x80 | codec.len() as u8]);
    body.extend_from_slice(codec);
    // Audio (0xE1) container
    let mut audio = Vec::new();
    // SamplingFrequency (0xB5, Float 4 bytes) = 8000.0
    audio.extend_from_slice(&[0xB5, 0x84]);
    audio.extend_from_slice(&8000.0f32.to_be_bytes());
    // Channels (0x9F) = 1
    audio.extend_from_slice(&[0x9F, 0x81, 0x01]);
    // BitDepth (0x6264) = 8
    audio.extend_from_slice(&[0x62, 0x64, 0x81, 0x08]);
    body.extend_from_slice(&[0xE1, 0x80 | audio.len() as u8]);
    body.extend_from_slice(&audio);

    // Wrap in TrackEntry (0xAE)
    let mut out = Vec::new();
    out.push(0xAE);
    assert!(body.len() < 0x80);
    out.push(0x80 | body.len() as u8);
    out.extend_from_slice(&body);
    out
}

/// Build a TrackEntry blob for a video track (H.264).
fn video_track_entry(track_number: u8, default: bool) -> Vec<u8> {
    let mut body = Vec::new();
    // TrackNumber
    body.extend_from_slice(&[0xD7, 0x81, track_number]);
    // TrackUID
    body.extend_from_slice(&[0x73, 0xC5, 0x81, track_number * 10]);
    // TrackType = 1 (video)
    body.extend_from_slice(&[0x83, 0x81, 0x01]);
    // FlagDefault
    body.extend_from_slice(&[0x88, 0x81, if default { 1 } else { 0 }]);
    // CodecID = "V_MPEG4/ISO/AVC"
    let codec = b"V_MPEG4/ISO/AVC";
    body.extend_from_slice(&[0x86, 0x80 | codec.len() as u8]);
    body.extend_from_slice(codec);
    // CodecPrivate (0x63A2): a minimal AVCDecoderConfigurationRecord placeholder
    // (not a valid one for decoding; sufficient for structural parsing).
    let codec_priv: Vec<u8> = vec![
        0x01, 0x42, 0xC0, 0x1E, 0xFF, // version, profile, profile_compat, level, length size
        0xE1, 0x00, 0x00, // num SPS = 1, SPS length = 0
        0x01, 0x00, 0x00, // num PPS = 1, PPS length = 0
    ];
    body.extend_from_slice(&[0x63, 0xA2, 0x80 | codec_priv.len() as u8]);
    body.extend_from_slice(&codec_priv);
    // Video container
    let mut video = Vec::new();
    // PixelWidth (0xB0) = 16
    video.extend_from_slice(&[0xB0, 0x81, 0x10]);
    // PixelHeight (0xBA) = 16
    video.extend_from_slice(&[0xBA, 0x81, 0x10]);
    body.extend_from_slice(&[0xE0, 0x80 | video.len() as u8]);
    body.extend_from_slice(&video);

    // Wrap in TrackEntry
    let mut out = Vec::new();
    out.push(0xAE);
    assert!(body.len() < 0x80);
    out.push(0x80 | body.len() as u8);
    out.extend_from_slice(&body);
    out
}

/// Build a synthetic Annex B stream for a video frame with the given NAL
/// lengths. Uses 4-byte start codes. NAL payload bytes are filled with
/// `(track_id + frame_index + nal_index) & 0xFF`-ish patterns so we can
/// verify downstream.
fn synth_annexb_frame(nal_lengths: &[u64], seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, &nl) in nal_lengths.iter().enumerate() {
        out.extend_from_slice(&[0, 0, 0, 1]);
        for j in 0..nl as usize {
            out.push(seed.wrapping_add(i as u8).wrapping_add(j as u8));
        }
    }
    out
}

/// Single test scenario used by multiple tests below.
struct Fixture {
    muxer: Muxer,
}

fn build_fixture() -> Fixture {
    // 2 clusters at timestamps 0 and 1000 (1 second apart in TimecodeScale=1ms).
    // 1 video track (id=1) VBR with 3 frames of variable length.
    // 1 audio track (id=2) CBR with 4 frames of 100 bytes each.

    let mut pre = ebml_header_bytes();
    pre.extend(segment_and_info_bytes());

    let video_te = video_track_entry(1, false);
    let audio_te = audio_track_entry(2, true);

    let binstruct = BinstructFile {
        header: Header { version: 1, config_flags: 0 },
        source: Source {
            original_mkv_hash: [0; 32],
            publisher_info: "test".into(),
            creation_timestamp: 0,
            original_language: "und".into(),
            original_default_track_id: 2,
            build_tools: vec![ToolEntry {
                name: "test".into(),
                version: "0".into(),
            }],
        },
        mkv_skeleton: MkvSkeleton {
            pre_tracks_blob: pre,
            track_entries: vec![
                TrackEntryRecord { track_id: 1, bytes: video_te },
                TrackEntryRecord { track_id: 2, bytes: audio_te },
            ],
            post_tracks_blob: vec![],
        },
        cluster_timestamps: ClusterTimestamps {
            cluster_count: 2,
            deltas: vec![0, 1000],
        },
        track_policies: vec![
            TrackPolicy {
                track_id: 1,
                codec_type: CodecType::Video,
                language_code: "und".into(),
                frame_count: 3,
                is_vbr: true,
                frame_duration: 500, // 500ms per frame → frame 0@0, frame 1@500, frame 2@1000
                cbr_frame_size: None,
                raw_file_hash: [0; 32],
                vfr_file_hash: Some([0; 32]),
            },
            TrackPolicy {
                track_id: 2,
                codec_type: CodecType::Audio,
                language_code: "und".into(),
                frame_count: 4,
                is_vbr: false,
                frame_duration: 500, // 500ms per audio frame
                cbr_frame_size: Some(100),
                raw_file_hash: [0; 32],
                vfr_file_hash: None,
            },
        ],
    };

    // Video VFR: 3 frames, each with 2 NAL units. Keyframe = first and last.
    // NAL lengths: frame 0 → [10, 20], frame 1 → [15, 25], frame 2 → [5, 30].
    let video_nal_lengths: Vec<u64> = vec![10, 20, 15, 25, 5, 30];
    let video_frames = vec![
        FrameRecord { frame_size: (10 + 20 + 4 + 4) as u32, flags: 0x03, nal_count: 2, duration_delta: 0 },
        FrameRecord { frame_size: (15 + 25 + 4 + 4) as u32, flags: 0x02, nal_count: 2, duration_delta: 0 },
        FrameRecord { frame_size: (5 + 30 + 4 + 4) as u32, flags: 0x03, nal_count: 2, duration_delta: 0 },
    ];
    let video_vfr = Arc::new(VfrFile {
        version: 1,
        flags: 0x01,
        frames: video_frames,
        nal_lengths: video_nal_lengths.clone(),
    });

    // Video raw bytes: concatenation of Annex B encoded frames.
    let mut video_raw = Vec::new();
    video_raw.extend(synth_annexb_frame(&video_nal_lengths[0..2], 0x10));
    video_raw.extend(synth_annexb_frame(&video_nal_lengths[2..4], 0x20));
    video_raw.extend(synth_annexb_frame(&video_nal_lengths[4..6], 0x30));

    // Audio raw bytes: 4 frames × 100 bytes each, sequential u8 pattern.
    let audio_raw: Vec<u8> = (0u32..400).map(|v| (v & 0xFF) as u8).collect();

    let video_source = TrackSource {
        track_id: 1,
        raw_file: Arc::new(MemRawFile::new(video_raw)),
        vfr: Some(video_vfr),
        policy: TrackPolicy {
            track_id: 1,
            codec_type: CodecType::Video,
            language_code: "und".into(),
            frame_count: 3,
            is_vbr: true,
            frame_duration: 500,
            cbr_frame_size: None,
            raw_file_hash: [0; 32],
            vfr_file_hash: Some([0; 32]),
        },
    };
    let audio_source = TrackSource {
        track_id: 2,
        raw_file: Arc::new(MemRawFile::new(audio_raw)),
        vfr: None,
        policy: TrackPolicy {
            track_id: 2,
            codec_type: CodecType::Audio,
            language_code: "und".into(),
            frame_count: 4,
            is_vbr: false,
            frame_duration: 500,
            cbr_frame_size: Some(100),
            raw_file_hash: [0; 32],
            vfr_file_hash: None,
        },
    };

    let mut sources = HashMap::new();
    sources.insert(1u32, video_source);
    sources.insert(2u32, audio_source);

    let muxer = Muxer::new(
        Arc::new(binstruct),
        sources,
        vec![1, 2],
        2, // default audio track
        ReadPolicy::Block,
        Arc::new(FullyAvailable),
    )
    .unwrap();

    Fixture { muxer }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn layout_sizes_consistent() {
    let fx = build_fixture();
    let total = fx.muxer.total_size();
    assert!(total > 0);

    // Reading [0, total) should return exactly `total` bytes.
    let bytes = fx.muxer.read(0, total as u32).unwrap();
    assert_eq!(bytes.len() as u64, total);

    // Reading past total should clamp.
    let bytes = fx.muxer.read(total - 5, 100).unwrap();
    assert_eq!(bytes.len(), 5);

    // Reading at or beyond total should be empty.
    let bytes = fx.muxer.read(total, 100).unwrap();
    assert!(bytes.is_empty());
}

#[test]
fn transform_avcc_basic_via_public_api() {
    // 2 NALs, 10 and 5 bytes, 4-byte start codes.
    let mut input = Vec::new();
    input.extend_from_slice(&[0, 0, 0, 1]);
    input.extend(vec![1u8; 10]);
    input.extend_from_slice(&[0, 0, 0, 1]);
    input.extend(vec![2u8; 5]);
    let got = transform_avcc(&input, &[10, 5]).unwrap();
    assert_eq!(&got[..4], &10u32.to_be_bytes());
    assert_eq!(&got[4..14], &[1u8; 10]);
    assert_eq!(&got[14..18], &5u32.to_be_bytes());
    assert_eq!(&got[18..23], &[2u8; 5]);
}

#[test]
fn transform_avcc_3byte_startcode_via_public_api() {
    // Same lengths but with 3-byte start codes.
    let mut input = Vec::new();
    input.extend_from_slice(&[0, 0, 1]);
    input.extend(vec![1u8; 10]);
    input.extend_from_slice(&[0, 0, 1]);
    input.extend(vec![2u8; 5]);
    let got = transform_avcc(&input, &[10, 5]).unwrap();
    assert_eq!(&got[..4], &10u32.to_be_bytes());
    assert_eq!(&got[4..14], &[1u8; 10]);
    assert_eq!(&got[14..18], &5u32.to_be_bytes());
    assert_eq!(&got[18..23], &[2u8; 5]);
}

#[test]
fn read_partial_ranges_match_full_read() {
    let fx = build_fixture();
    let total = fx.muxer.total_size();
    let full = fx.muxer.read(0, total as u32).unwrap();
    assert_eq!(full.len() as u64, total);

    // Read in 4 KB chunks (well below total) and concatenate.
    let chunk = 4096u32;
    let mut concatenated = Vec::with_capacity(total as usize);
    let mut offset = 0u64;
    while offset < total {
        let bytes = fx.muxer.read(offset, chunk).unwrap();
        assert!(!bytes.is_empty(), "muxer returned 0 bytes at offset {offset}");
        concatenated.extend_from_slice(&bytes);
        offset += bytes.len() as u64;
    }
    assert_eq!(concatenated, full);

    // Also try unaligned chunks (113 bytes — prime, exercises section boundaries).
    let chunk = 113u32;
    let mut concatenated = Vec::with_capacity(total as usize);
    let mut offset = 0u64;
    while offset < total {
        let bytes = fx.muxer.read(offset, chunk).unwrap();
        assert!(!bytes.is_empty(), "muxer returned 0 bytes at offset {offset}");
        concatenated.extend_from_slice(&bytes);
        offset += bytes.len() as u64;
    }
    assert_eq!(concatenated, full);
}

#[test]
fn read_starting_in_middle_of_simple_block() {
    let fx = build_fixture();
    let total = fx.muxer.total_size();
    let full = fx.muxer.read(0, total as u32).unwrap();
    // Pick an offset roughly in the middle of the file and verify a short
    // read aligns with the corresponding slice of the full read.
    let mid = total / 2;
    let len = 256u32.min((total - mid) as u32);
    let partial = fx.muxer.read(mid, len).unwrap();
    assert_eq!(partial, &full[mid as usize..(mid + len as u64) as usize]);
}

#[test]
#[ignore]
fn read_coordinator_produces_ffprobe_compatible_mkv() {
    // Run with: `cargo test -p videofuser-muxer -- --include-ignored`.
    //
    // This test writes the muxer output to a tempfile and shells out to
    // ffprobe to verify that the structure is interpretable. The CodecPrivate
    // payloads in `build_fixture()` are placeholders, so we only assert that
    // ffprobe parses the EBML/Matroska container and lists 2 streams. It is
    // expected that decoding would fail.
    let ffprobe = std::process::Command::new("ffprobe")
        .arg("-version")
        .output();
    if ffprobe.is_err() {
        eprintln!("ffprobe not available, skipping");
        return;
    }

    let fx = build_fixture();
    let total = fx.muxer.total_size();
    let bytes = fx.muxer.read(0, total as u32).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.mkv");
    std::fs::write(&path, &bytes).unwrap();

    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=index", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stream_count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(stream_count, 2, "expected 2 streams, got: {stdout}");
}
