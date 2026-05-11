//! Unit tests for videofuser-fs that require access to pub(crate) internals.

use std::collections::HashMap;
use std::sync::Arc;

use videofuser_binstruct::{
    BinstructFile, ClusterTimestamps, CodecType, Header, MkvSkeleton, Source, ToolEntry,
    TrackEntryRecord, TrackPolicy,
};
use videofuser_muxer::{FullyAvailable, MemRawFile, Muxer, ReadPolicy, TrackSource};

use crate::fs::VidFuserFs;
use crate::types::FileKey;

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

pub(crate) fn build_test_muxer() -> Arc<Muxer> {
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

#[test]
fn test_add_remove_torrent_unit() {
    let fs = VidFuserFs::new();
    let muxer = build_test_muxer();

    fs.add_torrent("t1".into(), "video".into(), muxer, vec![]);
    {
        let inner = fs.inner.read().unwrap();
        assert!(inner.torrents.contains_key("t1"));
        // dir and mkv inodes should exist
        let t = inner.torrents.get("t1").unwrap();
        assert!(inner.inode_table.get_key(t.dir_inode).is_some());
        assert!(inner.inode_table.get_key(t.mkv_inode).is_some());
    }

    fs.remove_torrent("t1");
    {
        let inner = fs.inner.read().unwrap();
        assert!(!inner.torrents.contains_key("t1"));
    }
}

#[test]
fn test_update_visible_subs_unit() {
    let fs = VidFuserFs::new();
    let muxer = build_test_muxer();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"sub content").unwrap();

    fs.add_torrent("t1".into(), "video".into(), muxer, vec![]);

    let (appeared, disappeared) = fs.update_visible_subs(
        "t1",
        vec![("video.es.0.srt".to_string(), tmp.path().to_path_buf())],
    );
    assert_eq!(appeared, vec!["video.es.0.srt".to_string()]);
    assert!(disappeared.is_empty(), "unexpected disappeared: {:?}", disappeared);

    let (appeared2, disappeared2) = fs.update_visible_subs("t1", vec![]);
    assert!(appeared2.is_empty(), "unexpected appeared: {:?}", appeared2);
    assert_eq!(disappeared2, vec!["video.es.0.srt".to_string()]);
}

#[test]
fn test_update_muxer_unit() {
    let fs = VidFuserFs::new();
    let muxer1 = build_test_muxer();
    let size1 = muxer1.total_size();

    fs.add_torrent("t1".into(), "video".into(), muxer1, vec![]);

    let muxer2 = build_test_muxer();
    let size2 = muxer2.total_size();
    fs.update_muxer("t1", muxer2);

    let inner = fs.inner.read().unwrap();
    let t = inner.torrents.get("t1").unwrap();
    assert_eq!(t.muxer.total_size(), size2);
    assert_eq!(size1, size2); // same spec
}

#[test]
fn test_inode_table_root() {
    let fs = VidFuserFs::new();
    let inner = fs.inner.read().unwrap();
    assert_eq!(inner.inode_table.get_key(1), Some(&FileKey::Root));
}

#[test]
fn test_inode_table_bidirectional() {
    let fs = VidFuserFs::new();
    let muxer = build_test_muxer();
    fs.add_torrent("abc".into(), "file".into(), muxer, vec![]);

    let inner = fs.inner.read().unwrap();
    let t = inner.torrents.get("abc").unwrap();

    // dir inode → key
    let dir_key = inner.inode_table.get_key(t.dir_inode).unwrap();
    assert_eq!(dir_key, &FileKey::TorrentDir { torrent_id: "abc".into() });

    // key → inode (round-trip)
    let back = inner.inode_table.get_inode(&FileKey::TorrentDir { torrent_id: "abc".into() });
    assert_eq!(back, Some(t.dir_inode));
}
