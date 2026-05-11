use std::path::PathBuf;
use std::sync::Arc;

use videofuser_muxer::Muxer;

/// Semantic key identifying a file or directory in the VFS.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum FileKey {
    Root,
    TorrentDir { torrent_id: String },
    MkvVirtual { torrent_id: String },
    Sidecar { torrent_id: String, exposed_name: String },
}

/// A sidecar subtitle file exposed in the VFS.
pub struct ExposedSub {
    /// "<base>.<lang>.<variant>.<ext>" (cap. 11.4)
    pub exposed_name: String,
    /// Real path on disk
    pub disk_path: PathBuf,
    pub inode: u64,
    pub size: u64,
}

/// State of a mounted torrent. Immutable after creation; fields that change
/// (visible sidecars, muxer) are replaced atomically via Arc::swap above.
pub struct MountedTorrent {
    pub torrent_id: String,
    /// Base name of the MKV without extension
    pub base_name: String,
    pub muxer: Arc<Muxer>,
    /// Sidecars that pass the filter and are downloaded
    pub visible_subs: Vec<ExposedSub>,
    pub dir_inode: u64,
    pub mkv_inode: u64,
}
