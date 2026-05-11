/// End-to-end integration tests for binstruct.
///
/// These tests build synthetic MKV files in memory, create the expected
/// torrent directory structure, invoke the binstruct binary, and verify output.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use videofuser_binstruct::{BinstructFile, ClusterTimestamps, CodecType, Header, MkvSkeleton, Source, ToolEntry, TrackEntryRecord, TrackPolicy};
use videofuser_manifest::{
    serialize_ebml, Manifest, ManifestTrack, TrackKind, VariantEntry,
};
use videofuser_vfr::{FrameRecord, VfrFile};

// ─── EBML building helpers (for synthetic MKV construction) ───────────────────

fn vint_encode(value: u64) -> Vec<u8> {
    let n = match value {
        0..=0x7E => 1,
        0..=0x3FFE => 2,
        0..=0x1FFFFE => 3,
        0..=0x0FFFFFFE => 4,
        _ => 5,
    };
    let marker = 0x80u8 >> (n - 1);
    let be = value.to_be_bytes();
    let mut out = vec![0u8; n];
    out[..n].copy_from_slice(&be[8 - n..]);
    out[0] |= marker;
    out
}

fn encode_uint_be(value: u64) -> Vec<u8> {
    if value == 0 { return vec![0]; }
    let be = value.to_be_bytes();
    let skip = be.iter().position(|&b| b != 0).unwrap_or(7);
    be[skip..].to_vec()
}

/// Write a complete EBML element with multi-byte ID (ID written as raw bytes).
fn elem(id_bytes: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(id_bytes);
    out.extend_from_slice(&vint_encode(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

/// Write an EBML element with 1-byte ID.
fn elem1(id: u8, payload: &[u8]) -> Vec<u8> {
    elem(&[id], payload)
}

/// Master element (same as elem, just named for clarity).
fn master(id_bytes: &[u8], children: &[u8]) -> Vec<u8> {
    elem(id_bytes, children)
}

/// Build a minimal EBML Header.
fn build_ebml_header() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(elem(&[0x42, 0x82], b"matroska")); // DocType
    body.extend(elem(&[0x42, 0x87], &encode_uint_be(4))); // DocTypeVersion
    body.extend(elem(&[0x42, 0x85], &encode_uint_be(2))); // DocTypeReadVersion
    master(&[0x1A, 0x45, 0xDF, 0xA3], &body)
}

/// Build a minimal Segment Info element.
fn build_info() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(elem(&[0x2A, 0xD7, 0xB1], &encode_uint_be(1_000_000))); // TimestampScale
    master(&[0x15, 0x49, 0xA9, 0x66], &body)
}

/// Build a TrackEntry for video (type=1).
fn build_video_track(track_id: u64) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(elem1(0xD7, &encode_uint_be(track_id))); // TrackNumber
    body.extend(elem(&[0x73, 0xC5], &encode_uint_be(track_id))); // TrackUID
    body.extend(elem1(0x83, &[1u8])); // TrackType = video
    body.extend(elem1(0x88, &[0u8])); // FlagDefault = 0
    body.extend(elem1(0x86, b"V_MPEG4/ISO/AVC")); // CodecID
    // Video sub-element
    let mut video = Vec::new();
    video.extend(elem1(0xB0, &encode_uint_be(1920))); // PixelWidth
    video.extend(elem1(0xBA, &encode_uint_be(1080))); // PixelHeight
    body.extend(elem1(0xE0, &video));
    elem1(0xAE, &body) // TrackEntry
}

/// Build a TrackEntry for audio (type=2) with optional FlagDefault.
fn build_audio_track(track_id: u64, flag_default: u8, lang: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(elem1(0xD7, &encode_uint_be(track_id))); // TrackNumber
    body.extend(elem(&[0x73, 0xC5], &encode_uint_be(track_id))); // TrackUID
    body.extend(elem1(0x83, &[2u8])); // TrackType = audio
    body.extend(elem1(0x88, &[flag_default])); // FlagDefault
    body.extend(elem(&[0x22, 0xB5, 0x9C], lang.as_bytes())); // Language
    body.extend(elem1(0x86, b"A_AC3")); // CodecID
    elem1(0xAE, &body) // TrackEntry
}

/// Build a minimal Cluster with a given timestamp (ms).
fn build_cluster(ts: u64) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(elem1(0xE7, &encode_uint_be(ts))); // Timestamp
    master(&[0x1F, 0x43, 0xB6, 0x75], &body)
}

/// Build a complete minimal MKV with 1 video + 1 audio track and N clusters.
fn build_synthetic_mkv(
    video_track_id: u64,
    audio_track_id: u64,
    audio_flag_default: u8,
    audio_lang: &str,
    cluster_timestamps: &[u64],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_ebml_header());

    let mut seg_body = Vec::new();
    seg_body.extend(build_info());

    // Tracks
    let mut tracks_body = Vec::new();
    tracks_body.extend(build_video_track(video_track_id));
    tracks_body.extend(build_audio_track(audio_track_id, audio_flag_default, audio_lang));
    seg_body.extend(master(&[0x16, 0x54, 0xAE, 0x6B], &tracks_body));

    // Clusters
    for &ts in cluster_timestamps {
        seg_body.extend(build_cluster(ts));
    }

    out.extend(master(&[0x18, 0x53, 0x80, 0x67], &seg_body));
    out
}

// ─── Torrent directory structure builders ─────────────────────────────────────

fn setup_torrent_root(
    root: &Path,
    video_track_id: u64,
    audio_track_id: u64,
    cbr_frame_count: Option<u64>,
    cbr_frame_size: Option<u64>,
) {
    fs::create_dir_all(root.join("video")).unwrap();
    fs::create_dir_all(root.join("audio")).unwrap();
    fs::create_dir_all(root.join("info").join("vfr")).unwrap();

    // Raw video file
    let video_path = root.join("video").join(format!("base_v{:02}_1080p_00.h264", video_track_id));
    fs::write(&video_path, vec![0u8; 1024]).unwrap();

    // VFR for video (always VBR)
    let vfr_video_path = root.join("info").join("vfr").join(format!("base_v{:02}_00.vfr", video_track_id));
    let vfr = VfrFile {
        version: 1,
        flags: 0,
        frames: vec![
            FrameRecord { frame_size: 512, flags: 0x01, nal_count: 0, duration_delta: 0 },
            FrameRecord { frame_size: 256, flags: 0x00, nal_count: 0, duration_delta: 0 },
            FrameRecord { frame_size: 256, flags: 0x01, nal_count: 0, duration_delta: 0 },
        ],
        nal_lengths: vec![],
    };
    let mut vfr_bytes = Vec::new();
    vfr.write_to(&mut vfr_bytes).unwrap();
    fs::write(&vfr_video_path, &vfr_bytes).unwrap();

    // Raw audio file + CBR sidecar or VFR for VBR
    let audio_raw_path = root.join("audio").join(format!("base_a{:03}_spa_00_00.ac3", audio_track_id));

    if let (Some(fc), Some(fs_)) = (cbr_frame_count, cbr_frame_size) {
        // CBR audio
        let file_size = fc * fs_;
        fs::write(&audio_raw_path, vec![0u8; file_size as usize]).unwrap();

        // CBR sidecar JSON
        let cbr_json = serde_json::json!({
            "is_vbr": false,
            "frame_count": fc,
            "cbr_frame_size": fs_,
            "frame_duration_ns": 32_000_000u64,
            "file_size_bytes": file_size,
        });
        let cbr_path = audio_raw_path.with_extension("ac3.cbr.json");
        fs::write(&cbr_path, cbr_json.to_string()).unwrap();
    } else {
        // VBR audio — write raw file and VFR
        fs::write(&audio_raw_path, vec![0u8; 2048]).unwrap();
        let vfr_audio_path = root.join("info").join("vfr").join(
            format!("base_a{:03}_00_00.vfr", audio_track_id),
        );
        let audio_vfr = VfrFile {
            version: 1,
            flags: 0,
            frames: vec![
                FrameRecord { frame_size: 768, flags: 0x01, nal_count: 0, duration_delta: 0 },
                FrameRecord { frame_size: 768, flags: 0x01, nal_count: 0, duration_delta: 0 },
            ],
            nal_lengths: vec![],
        };
        let mut avfr_bytes = Vec::new();
        audio_vfr.write_to(&mut avfr_bytes).unwrap();
        fs::write(&vfr_audio_path, &avfr_bytes).unwrap();
    }
}

fn binstruct_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // remove test binary name
    // In Cargo test layout: target/debug/deps/<test>; binary is target/debug/binstruct
    if p.ends_with("deps") { p.pop(); }
    p.push("binstruct");
    p
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn test_gen_from_synthetic_mkv() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mkv_bytes = build_synthetic_mkv(1, 2, 1, "spa", &[0, 1000, 2000]);
    let mkv_path = root.join("test.mkv");
    fs::write(&mkv_path, &mkv_bytes).unwrap();

    // CBR audio: frame_count=10, cbr_frame_size=128 → file_size=1280
    setup_torrent_root(root, 1, 2, Some(10), Some(128));

    let output = root.join("out.binstruct.ebml");
    let status = Command::new(binstruct_bin())
        .args([
            "gen",
            "--mkv", mkv_path.to_str().unwrap(),
            "--torrent-root", root.to_str().unwrap(),
            "--publisher", "TestPub",
            "--output", output.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run binstruct");
    assert!(status.success(), "binstruct gen failed: {:?}", status);

    assert!(output.exists(), "output file not created");
    let bytes = fs::read(&output).unwrap();
    let bf = BinstructFile::deserialize(&bytes).expect("deserialize binstruct");

    assert_eq!(bf.source.original_language, "spa");
    assert_eq!(bf.track_policies.len(), 2);
    assert_eq!(bf.cluster_timestamps.cluster_count, 3);
    assert_eq!(bf.source.publisher_info, "TestPub");
}

#[test]
fn test_gen_fails_no_flagdefault() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // audio FlagDefault=0
    let mkv_bytes = build_synthetic_mkv(1, 2, 0, "spa", &[0, 1000]);
    let mkv_path = root.join("test.mkv");
    fs::write(&mkv_path, &mkv_bytes).unwrap();
    setup_torrent_root(root, 1, 2, Some(10), Some(128));

    let output = root.join("out.binstruct.ebml");
    let status = Command::new(binstruct_bin())
        .args([
            "gen",
            "--mkv", mkv_path.to_str().unwrap(),
            "--torrent-root", root.to_str().unwrap(),
            "--publisher", "TestPub",
            "--output", output.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run binstruct");
    assert_eq!(status.code(), Some(6), "expected exit code 6, got {:?}", status);
}

#[test]
fn test_gen_fails_cbr_inconsistency() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mkv_bytes = build_synthetic_mkv(1, 2, 1, "spa", &[0, 1000]);
    let mkv_path = root.join("test.mkv");
    fs::write(&mkv_path, &mkv_bytes).unwrap();

    fs::create_dir_all(root.join("video")).unwrap();
    fs::create_dir_all(root.join("audio")).unwrap();
    fs::create_dir_all(root.join("info").join("vfr")).unwrap();

    // Raw video + VFR
    let video_path = root.join("video").join("base_v01_1080p_00.h264");
    fs::write(&video_path, vec![0u8; 1024]).unwrap();
    let vfr_video_path = root.join("info").join("vfr").join("base_v01_00.vfr");
    let vfr = VfrFile {
        version: 1, flags: 0,
        frames: vec![FrameRecord { frame_size: 512, flags: 0x01, nal_count: 0, duration_delta: 0 }],
        nal_lengths: vec![],
    };
    let mut vfr_bytes = Vec::new();
    vfr.write_to(&mut vfr_bytes).unwrap();
    fs::write(&vfr_video_path, &vfr_bytes).unwrap();

    // Audio: CBR sidecar says frame_count=100, cbr_frame_size=128 → expected 12800 bytes
    // But actual file is 12801 bytes → inconsistency
    let audio_path = root.join("audio").join("base_a002_spa_00_00.ac3");
    fs::write(&audio_path, vec![0u8; 12801]).unwrap();
    let cbr_json = serde_json::json!({
        "is_vbr": false,
        "frame_count": 100u64,
        "cbr_frame_size": 128u64,
        "frame_duration_ns": 32_000_000u64,
        "file_size_bytes": 12800u64, // correct according to sidecar
    });
    let cbr_path = audio_path.with_extension("ac3.cbr.json");
    fs::write(&cbr_path, cbr_json.to_string()).unwrap();

    let output = root.join("out.binstruct.ebml");
    let status = Command::new(binstruct_bin())
        .args([
            "gen",
            "--mkv", mkv_path.to_str().unwrap(),
            "--torrent-root", root.to_str().unwrap(),
            "--publisher", "TestPub",
            "--output", output.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run binstruct");
    assert_eq!(status.code(), Some(6), "expected exit code 6, got {:?}", status);
}

#[test]
fn test_inspect_round_trip() {
    // Build a synthetic BinstructFile, serialize it, then inspect it.
    let bf = BinstructFile {
        header: Header { version: 1, config_flags: 0 },
        source: Source {
            original_mkv_hash: [0x42u8; 32],
            publisher_info: "InspectPub".to_string(),
            creation_timestamp: 1_700_000_000_000,
            original_language: "fra".to_string(),
            original_default_track_id: 7,
            build_tools: vec![
                ToolEntry { name: "ffmpeg".to_string(), version: "ffmpeg version 6.1".to_string() },
            ],
        },
        mkv_skeleton: MkvSkeleton {
            pre_tracks_blob: vec![0x01, 0x02],
            track_entries: vec![
                TrackEntryRecord { track_id: 7, bytes: vec![0xAA, 0xBB] },
            ],
            post_tracks_blob: vec![],
        },
        cluster_timestamps: ClusterTimestamps {
            cluster_count: 2,
            deltas: vec![0, 1000],
        },
        track_policies: vec![
            TrackPolicy {
                track_id: 7,
                codec_type: CodecType::Audio,
                language_code: "fra".to_string(),
                frame_count: 1000,
                is_vbr: false,
                frame_duration: 960,
                cbr_frame_size: Some(512),
                raw_file_hash: [0x11u8; 32],
                vfr_file_hash: None,
            },
        ],
    };

    let dir = tempfile::tempdir().unwrap();
    let binstruct_path = dir.path().join("test.binstruct.ebml");
    fs::write(&binstruct_path, bf.serialize().unwrap()).unwrap();

    let output = Command::new(binstruct_bin())
        .args(["inspect", binstruct_path.to_str().unwrap()])
        .output()
        .expect("failed to run binstruct inspect");

    assert!(output.status.success(), "inspect failed: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fra"), "language 'fra' not found in inspect output");
    assert!(stdout.contains("7"), "track_id 7 not found in inspect output");
    assert!(stdout.contains("InspectPub"), "publisher not in inspect output");
}

#[test]
fn test_verify_manifest_detects_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let info_dir = dir.path().join("info");
    fs::create_dir_all(&info_dir).unwrap();

    let m = Manifest {
        title: "Test Movie".to_string(),
        year: 2020,
        original_mkv_filename: "test.mkv".to_string(),
        original_mkv_hash: [0xBBu8; 32],
        original_language: "eng".to_string(),
        system_version: "1.0".to_string(),
        publisher: "TestPub".to_string(),
        tracks: vec![ManifestTrack {
            track_id: 1,
            kind: TrackKind::Video,
            language: None,
            codec: "V_MPEG4/ISO/AVC".to_string(),
            variant: Some("00".to_string()),
            resolution: Some("1080p".to_string()),
        }],
        variants_legend: vec![VariantEntry {
            id: "00".to_string(),
            description: "Main".to_string(),
        }],
    };

    // Write canonical EBML manifest
    let ebml_bytes = serialize_ebml(&m).unwrap();
    fs::write(info_dir.join("base_manifest_00.ebml"), &ebml_bytes).unwrap();

    // Write coherent XML manifest
    let hash_hex: String = m.original_mkv_hash.iter().map(|b| format!("{:02x}", b)).collect();
    let xml = format!(
        r#"<videofuser-manifest version="1">
  <title>Test Movie</title>
  <year>2020</year>
  <hash>{}</hash>
  <publisher>TestPub</publisher>
  <original-language>eng</original-language>
  <tracks>
    <track id="1" type="video" codec="V_MPEG4/ISO/AVC" resolution="1080p" variant="00" />
  </tracks>
  <variants>
    <variant id="00" description="Main" />
  </variants>
</videofuser-manifest>"#,
        hash_hex
    );
    fs::write(info_dir.join("base_manifest_00.xml"), &xml).unwrap();

    // Write NFO
    let nfo = r#"<movie>
  <title>Test Movie</title>
  <year>2020</year>
</movie>"#;
    fs::write(info_dir.join("base_manifest_00.nfo"), nfo).unwrap();

    // Write MD
    let md = "# Test Movie (2020)\n\n- **Año**: 2020\n- **Idioma original**: eng\n";
    fs::write(info_dir.join("base_manifest_00.md"), md).unwrap();

    // First run: all coherent → exit 0
    let status = Command::new(binstruct_bin())
        .args(["verify-manifest", info_dir.to_str().unwrap()])
        .status()
        .expect("failed to run verify-manifest");
    assert_eq!(status.code(), Some(0), "expected exit 0 for coherent manifests");

    // Modify year in XML → should fail
    let bad_xml = xml.replace("<year>2020</year>", "<year>1999</year>");
    fs::write(info_dir.join("base_manifest_00.xml"), &bad_xml).unwrap();

    let output = Command::new(binstruct_bin())
        .args(["verify-manifest", info_dir.to_str().unwrap()])
        .output()
        .expect("failed to run verify-manifest");
    assert_eq!(output.status.code(), Some(1), "expected exit 1 after year mismatch");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("year"),
        "stderr should mention 'year': {}", stderr
    );
}
