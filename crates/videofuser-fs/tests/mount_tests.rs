//! FUSE mount integration tests for videofuser-fs.
//!
//! These tests require the FUSE kernel module to be loaded and are gated
//! behind `#[ignore]`. Run with:
//!   cargo test -p videofuser-fs -- --include-ignored

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fuser::{BackgroundSession, MountOption};
use tempfile::TempDir;
use videofuser_binstruct::{
    BinstructFile, ClusterTimestamps, CodecType, Header, MkvSkeleton, Source, ToolEntry,
    TrackEntryRecord, TrackPolicy,
};
use videofuser_muxer::{FullyAvailable, MemRawFile, Muxer, ReadPolicy, TrackSource};
use videofuser_fs::VidFuserFs;

// ---------------------------------------------------------------------------
// Helpers (minimal muxer fixture)
// ---------------------------------------------------------------------------

fn ebml_header_bytes() -> Vec<u8> {
    let mut content = Vec::new();
    content.extend_from_slice(&[0x42, 0x86, 0x81, 0x01]);
    content.extend_from_slice(&[0x42, 0xF7, 0x81, 0x01]);
    content.extend_from_slice(&[0x42, 0xF2, 0x81, 0x04]);
    content.extend_from_slice(&[0x42, 0xF3, 0x81, 0x08]);
    content.extend_from_slice(&[0x42, 0x82, 0x88]);
    content.extend_from_slice(b"matroska");
    content.extend_from_slice(&[0x42, 0x87, 0x81, 0x04]);
    content.extend_from_slice(&[0x42, 0x85, 0x81, 0x02]);
    let mut out = Vec::new();
    out.extend_from_slice(&[0x1A, 0x45, 0xDF, 0xA3]);
    assert!(content.len() < 0x80);
    out.push(0x80 | content.len() as u8);
    out.extend_from_slice(&content);
    out
}

fn segment_and_info_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
    out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    let mut info = Vec::new();
    info.extend_from_slice(&[0x2A, 0xD7, 0xB1, 0x83]);
    info.extend_from_slice(&[0x0F, 0x42, 0x40]);
    let app = b"videofuser-test";
    info.extend_from_slice(&[0x4D, 0x80, 0x80 | app.len() as u8]);
    info.extend_from_slice(app);
    info.extend_from_slice(&[0x57, 0x41, 0x80 | app.len() as u8]);
    info.extend_from_slice(app);
    out.extend_from_slice(&[0x15, 0x49, 0xA9, 0x66]);
    assert!(info.len() < 0x80);
    out.push(0x80 | info.len() as u8);
    out.extend_from_slice(&info);
    out
}

fn audio_track_entry(track_number: u8, default: bool) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0xD7, 0x81, track_number]);
    body.extend_from_slice(&[0x73, 0xC5, 0x81, track_number * 10]);
    body.extend_from_slice(&[0x83, 0x81, 0x02]);
    body.extend_from_slice(&[0x88, 0x81, if default { 1 } else { 0 }]);
    let codec = b"A_PCM/INT/LIT";
    body.extend_from_slice(&[0x86, 0x80 | codec.len() as u8]);
    body.extend_from_slice(codec);
    let mut audio = Vec::new();
    audio.extend_from_slice(&[0xB5, 0x84]);
    audio.extend_from_slice(&8000.0f32.to_be_bytes());
    audio.extend_from_slice(&[0x9F, 0x81, 0x01]);
    audio.extend_from_slice(&[0x62, 0x64, 0x81, 0x08]);
    body.extend_from_slice(&[0xE1, 0x80 | audio.len() as u8]);
    body.extend_from_slice(&audio);
    let mut out = Vec::new();
    out.push(0xAE);
    assert!(body.len() < 0x80);
    out.push(0x80 | body.len() as u8);
    out.extend_from_slice(&body);
    out
}

/// Build a minimal but structurally valid Muxer for tests.
pub fn build_test_muxer() -> Arc<Muxer> {
    let mut pre = ebml_header_bytes();
    pre.extend(segment_and_info_bytes());
    let audio_te = audio_track_entry(1, true);

    let binstruct = BinstructFile {
        header: Header { version: 1, config_flags: 0 },
        source: Source {
            original_mkv_hash: [0; 32],
            publisher_info: "test".into(),
            creation_timestamp: 0,
            original_language: "und".into(),
            original_default_track_id: 1,
            build_tools: vec![ToolEntry { name: "test".into(), version: "0".into() }],
        },
        mkv_skeleton: MkvSkeleton {
            pre_tracks_blob: pre,
            track_entries: vec![TrackEntryRecord { track_id: 1, bytes: audio_te }],
            post_tracks_blob: vec![],
        },
        cluster_timestamps: ClusterTimestamps { cluster_count: 1, deltas: vec![0] },
        track_policies: vec![TrackPolicy {
            track_id: 1,
            codec_type: CodecType::Audio,
            language_code: "und".into(),
            frame_count: 2,
            is_vbr: false,
            frame_duration: 500,
            cbr_frame_size: Some(100),
            raw_file_hash: [0; 32],
            vfr_file_hash: None,
        }],
    };

    let audio_raw: Vec<u8> = (0u32..200).map(|v| (v & 0xFF) as u8).collect();
    let audio_source = TrackSource {
        track_id: 1,
        raw_file: Arc::new(MemRawFile::new(audio_raw)),
        vfr: None,
        policy: TrackPolicy {
            track_id: 1,
            codec_type: CodecType::Audio,
            language_code: "und".into(),
            frame_count: 2,
            is_vbr: false,
            frame_duration: 500,
            cbr_frame_size: Some(100),
            raw_file_hash: [0; 32],
            vfr_file_hash: None,
        },
    };

    let mut sources = HashMap::new();
    sources.insert(1u32, audio_source);

    Arc::new(
        Muxer::new(
            Arc::new(binstruct),
            sources,
            vec![1],
            1,
            ReadPolicy::NonBlock,
            Arc::new(FullyAvailable),
        )
        .expect("build_test_muxer"),
    )
}

// ---------------------------------------------------------------------------
// Mount helper
// ---------------------------------------------------------------------------

struct TestMount {
    _tmpdir: TempDir,
    pub mount_path: PathBuf,
    #[allow(dead_code)]
    pub session: BackgroundSession,
}

impl TestMount {
    fn new(fs: VidFuserFs) -> Self {
        let tmpdir = TempDir::new().expect("TempDir::new");
        let mount_path = tmpdir.path().to_path_buf();
        let session = fuser::spawn_mount2(
            fs,
            &mount_path,
            &[MountOption::RO, MountOption::AutoUnmount],
        )
        .expect("spawn_mount2 failed — is FUSE available?");
        Self { _tmpdir: tmpdir, mount_path, session }
    }
}

// ---------------------------------------------------------------------------
// FUSE tests (gated with #[ignore])
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_mount_and_readdir_root() {
    let fs = VidFuserFs::new();
    let mount = TestMount::new(fs);

    let entries: Vec<_> = std::fs::read_dir(&mount.mount_path)
        .expect("read_dir failed")
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();

    assert!(
        entries.is_empty(),
        "expected empty root directory, got: {:?}",
        entries
    );
}

#[test]
#[ignore]
fn test_mount_single_torrent_visible() {
    let fs = VidFuserFs::new();
    let muxer = build_test_muxer();
    let total_size = muxer.total_size();

    fs.add_torrent("mi_torrent".into(), "mi_video".into(), muxer, vec![]);

    let mount = TestMount::new(fs);

    let entries: Vec<_> = std::fs::read_dir(&mount.mount_path)
        .expect("read_dir root")
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries.contains(&"mi_torrent".to_string()),
        "expected 'mi_torrent' in root, got {:?}",
        entries
    );

    let sub_path = mount.mount_path.join("mi_torrent");
    let sub_entries: Vec<_> = std::fs::read_dir(&sub_path)
        .expect("read_dir subdir")
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        sub_entries.contains(&"mi_video.mkv".to_string()),
        "expected 'mi_video.mkv' in torrent dir, got {:?}",
        sub_entries
    );

    let mkv_path = sub_path.join("mi_video.mkv");
    let meta = std::fs::metadata(&mkv_path).expect("metadata failed");
    assert_eq!(meta.len(), total_size, "mkv size mismatch");
    assert!(meta.len() > 0, "mkv file should be non-empty");
}

#[test]
#[ignore]
fn test_mount_sidecar_visible_and_readable() {
    let subtitle_content = "Test subtitle";
    let srt_file = tempfile::NamedTempFile::new().expect("NamedTempFile::new");
    std::fs::write(srt_file.path(), subtitle_content).expect("write srt");

    let fs = VidFuserFs::new();
    let muxer = build_test_muxer();
    let exposed_name = "mi_video.es.0.srt".to_string();

    fs.add_torrent(
        "torrent1".into(),
        "mi_video".into(),
        muxer,
        vec![(exposed_name.clone(), srt_file.path().to_path_buf())],
    );

    let mount = TestMount::new(fs);
    let sub_path = mount.mount_path.join("torrent1");

    let sub_entries: Vec<_> = std::fs::read_dir(&sub_path)
        .expect("read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        sub_entries.contains(&exposed_name),
        "expected sidecar '{}' in dir, got {:?}",
        exposed_name,
        sub_entries
    );

    let content = std::fs::read_to_string(sub_path.join(&exposed_name))
        .expect("read_to_string sidecar");
    assert_eq!(content, subtitle_content, "sidecar content mismatch");
}

#[test]
#[ignore]
fn test_update_visible_subs_triggers_change() {
    let srt_file = tempfile::NamedTempFile::new().expect("NamedTempFile::new");
    std::fs::write(srt_file.path(), "hello").expect("write srt");

    // Mount without sidecars and verify empty
    {
        let fs = VidFuserFs::new();
        let muxer = build_test_muxer();
        fs.add_torrent("t1".into(), "video".into(), muxer, vec![]);
        let mount = TestMount::new(fs);
        let sub_path = mount.mount_path.join("t1");

        let srt_entries: Vec<_> = std::fs::read_dir(&sub_path)
            .expect("read_dir before")
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".srt"))
            .collect();
        assert!(srt_entries.is_empty(), "expected no sidecars initially, got {:?}", srt_entries);
    }

    // Mount with sidecar and verify it appears
    let exposed = "video.es.0.srt".to_string();
    {
        let fs = VidFuserFs::new();
        let muxer = build_test_muxer();
        fs.add_torrent(
            "t1".into(),
            "video".into(),
            muxer,
            vec![(exposed.clone(), srt_file.path().to_path_buf())],
        );
        let mount = TestMount::new(fs);
        let sub_path = mount.mount_path.join("t1");

        let entries_with: Vec<_> = std::fs::read_dir(&sub_path)
            .expect("read_dir with sidecar")
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".srt"))
            .collect();
        assert!(
            entries_with.contains(&exposed),
            "expected sidecar '{}', got {:?}",
            exposed,
            entries_with
        );
    }
}

#[test]
#[ignore]
fn test_mkv_virtual_readable_in_chunks() {
    let fs = VidFuserFs::new();
    let muxer = build_test_muxer();
    let total_size = muxer.total_size();

    fs.add_torrent("t1".into(), "video".into(), muxer, vec![]);
    let mount = TestMount::new(fs);

    let mkv_path = mount.mount_path.join("t1").join("video.mkv");
    let mut file = std::fs::File::open(&mkv_path).expect("open mkv");
    let mut all_bytes = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        use std::io::Read;
        let n = file.read(&mut buf).expect("read chunk");
        if n == 0 {
            break;
        }
        all_bytes.extend_from_slice(&buf[..n]);
    }

    assert_eq!(
        all_bytes.len() as u64,
        total_size,
        "chunked read total size mismatch (got {}, expected {})",
        all_bytes.len(),
        total_size
    );
}
