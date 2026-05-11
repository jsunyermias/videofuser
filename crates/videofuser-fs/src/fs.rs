use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use videofuser_muxer::Muxer;

use crate::inode::{InodeAllocator, InodeTable};
use crate::types::{ExposedSub, FileKey, MountedTorrent};

pub struct VidFuserFs {
    pub(crate) inner: Arc<RwLock<FsInner>>,
}

pub(crate) struct FsInner {
    /// torrent_id → mounted state
    pub torrents: HashMap<String, MountedTorrent>,
    pub inode_table: InodeTable,
    pub allocator: InodeAllocator,
    /// Always 1 (FUSE requirement)
    #[allow(dead_code)]
    pub root_inode: u64,
}

impl VidFuserFs {
    pub fn new() -> Self {
        let mut inode_table = InodeTable::new();
        inode_table.insert(1, FileKey::Root);

        let inner = FsInner {
            torrents: HashMap::new(),
            inode_table,
            allocator: InodeAllocator::new(),
            root_inode: 1,
        };

        Self { inner: Arc::new(RwLock::new(inner)) }
    }

    /// Adds an active torrent to the filesystem. Assigns inodes for the
    /// directory, the virtual MKV, and each visible sidecar.
    /// Called by the daemon when a torrent becomes Active.
    pub fn add_torrent(
        &self,
        torrent_id: String,
        base_name: String,
        muxer: Arc<Muxer>,
        visible_subs: Vec<(String, PathBuf)>,
    ) {
        let mut inner = self.inner.write().unwrap();

        let dir_inode = inner.allocator.alloc();
        inner.inode_table.insert(
            dir_inode,
            FileKey::TorrentDir { torrent_id: torrent_id.clone() },
        );

        let mkv_inode = inner.allocator.alloc();
        inner.inode_table.insert(
            mkv_inode,
            FileKey::MkvVirtual { torrent_id: torrent_id.clone() },
        );

        let exposed_subs: Vec<ExposedSub> = visible_subs
            .into_iter()
            .map(|(exposed_name, disk_path)| {
                let inode = inner.allocator.alloc();
                let size = std::fs::metadata(&disk_path).map(|m| m.len()).unwrap_or(0);
                inner.inode_table.insert(
                    inode,
                    FileKey::Sidecar {
                        torrent_id: torrent_id.clone(),
                        exposed_name: exposed_name.clone(),
                    },
                );
                ExposedSub { exposed_name, disk_path, inode, size }
            })
            .collect();

        let torrent = MountedTorrent {
            torrent_id: torrent_id.clone(),
            base_name,
            muxer,
            visible_subs: exposed_subs,
            dir_inode,
            mkv_inode,
        };

        inner.torrents.insert(torrent_id, torrent);
    }

    /// Removes a torrent from the filesystem, freeing all its inodes.
    /// Called by the daemon when a torrent is removed.
    pub fn remove_torrent(&self, torrent_id: &str) {
        let mut inner = self.inner.write().unwrap();

        if let Some(torrent) = inner.torrents.remove(torrent_id) {
            inner.inode_table.remove_by_inode(torrent.dir_inode);
            inner.inode_table.remove_by_inode(torrent.mkv_inode);
            for sub in &torrent.visible_subs {
                inner.inode_table.remove_by_inode(sub.inode);
            }
        }
    }

    /// Replaces the visible sidecar set for a torrent. Returns
    /// (appeared, disappeared) by exposed_name so the daemon can call
    /// inval_entry for each.
    pub fn update_visible_subs(
        &self,
        torrent_id: &str,
        new_subs: Vec<(String, PathBuf)>,
    ) -> (Vec<String>, Vec<String>) {
        let mut inner = self.inner.write().unwrap();

        let torrent = match inner.torrents.get_mut(torrent_id) {
            Some(t) => t,
            None => return (vec![], vec![]),
        };

        let old_names: Vec<String> = torrent.visible_subs.iter().map(|s| s.exposed_name.clone()).collect();
        let new_names: Vec<String> = new_subs.iter().map(|(n, _)| n.clone()).collect();

        let disappeared: Vec<String> = old_names
            .iter()
            .filter(|n| !new_names.contains(n))
            .cloned()
            .collect();
        let appeared_names: Vec<String> = new_names
            .iter()
            .filter(|n| !old_names.contains(n))
            .cloned()
            .collect();

        // Drain old subs and remove disappeared inodes from the table.
        // We must do this without holding the get_mut borrow on torrents.
        let old_subs: Vec<ExposedSub> = inner.torrents.get_mut(torrent_id).unwrap().visible_subs.drain(..).collect();
        for sub in &old_subs {
            if disappeared.contains(&sub.exposed_name) {
                inner.inode_table.remove_by_inode(sub.inode);
            }
        }

        // Rebuild visible_subs with fresh inodes for newly-appearing names.
        let tid = torrent_id.to_string();
        let new_exposed: Vec<ExposedSub> = new_subs
            .into_iter()
            .map(|(exposed_name, disk_path)| {
                let key = FileKey::Sidecar {
                    torrent_id: tid.clone(),
                    exposed_name: exposed_name.clone(),
                };
                let inode = match inner.inode_table.get_inode(&key) {
                    Some(existing) => existing,
                    None => {
                        let fresh = inner.allocator.alloc();
                        inner.inode_table.insert(fresh, key);
                        fresh
                    }
                };
                let size = std::fs::metadata(&disk_path).map(|m| m.len()).unwrap_or(0);
                ExposedSub { exposed_name, disk_path, inode, size }
            })
            .collect();

        inner.torrents.get_mut(torrent_id).unwrap().visible_subs = new_exposed;

        (appeared_names, disappeared)
    }

    /// Replaces the muxer for a torrent (e.g., when audio filter prefs change).
    pub fn update_muxer(&self, torrent_id: &str, muxer: Arc<Muxer>) {
        let mut inner = self.inner.write().unwrap();
        if let Some(torrent) = inner.torrents.get_mut(torrent_id) {
            torrent.muxer = muxer;
        }
    }
}

impl Default for VidFuserFs {
    fn default() -> Self {
        Self::new()
    }
}
