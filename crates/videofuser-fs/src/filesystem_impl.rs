use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, Request,
};
use libc::{EACCES, EIO, ENOENT, ENOTDIR};

use crate::fs::VidFuserFs;
use crate::types::FileKey;

/// Fixed mtime for virtual MKV files so players don't treat them as empty.
fn mkv_mtime() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn dir_attr(ino: u64, nlink: u32) -> FileAttr {
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o555,
        nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

fn file_attr(ino: u64, size: u64, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino,
        size,
        blocks: (size + 511) / 512,
        atime: UNIX_EPOCH,
        mtime,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o444,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

const TTL: Duration = Duration::from_secs(1);

impl Filesystem for VidFuserFs {
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let inner = self.inner.read().unwrap();

        let key = match inner.inode_table.get_key(ino) {
            Some(k) => k.clone(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        match key {
            FileKey::Root => {
                let nlink = 2 + inner.torrents.len() as u32;
                reply.attr(&TTL, &dir_attr(1, nlink));
            }
            FileKey::TorrentDir { torrent_id } => {
                let torrent = match inner.torrents.get(&torrent_id) {
                    Some(t) => t,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };
                // entries: ".", "..", "<base>.mkv", + sidecars
                let nlink = 2 + 1 + torrent.visible_subs.len() as u32;
                reply.attr(&TTL, &dir_attr(ino, nlink));
            }
            FileKey::MkvVirtual { torrent_id } => {
                let torrent = match inner.torrents.get(&torrent_id) {
                    Some(t) => t,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };
                let size = torrent.muxer.total_size();
                reply.attr(&TTL, &file_attr(ino, size, mkv_mtime()));
            }
            FileKey::Sidecar { torrent_id, exposed_name } => {
                let torrent = match inner.torrents.get(&torrent_id) {
                    Some(t) => t,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };
                let sub = match torrent.visible_subs.iter().find(|s| s.exposed_name == exposed_name) {
                    Some(s) => s,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };
                reply.attr(&TTL, &file_attr(ino, sub.size, UNIX_EPOCH));
            }
        }
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let inner = self.inner.read().unwrap();

        let parent_key = match inner.inode_table.get_key(parent) {
            Some(k) => k.clone(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        match parent_key {
            FileKey::Root => {
                // name should be a torrent_id (directory)
                let torrent = match inner.torrents.get(name_str) {
                    Some(t) => t,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };
                let nlink = 2 + 1 + torrent.visible_subs.len() as u32;
                let attr = dir_attr(torrent.dir_inode, nlink);
                reply.entry(&TTL, &attr, 0);
            }
            FileKey::TorrentDir { torrent_id } => {
                let torrent = match inner.torrents.get(&torrent_id) {
                    Some(t) => t,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };

                let expected_mkv = format!("{}.mkv", torrent.base_name);
                if name_str == expected_mkv {
                    let size = torrent.muxer.total_size();
                    let attr = file_attr(torrent.mkv_inode, size, mkv_mtime());
                    reply.entry(&TTL, &attr, 0);
                } else if let Some(sub) = torrent.visible_subs.iter().find(|s| s.exposed_name == name_str) {
                    let attr = file_attr(sub.inode, sub.size, UNIX_EPOCH);
                    reply.entry(&TTL, &attr, 0);
                } else {
                    reply.error(ENOENT);
                }
            }
            _ => {
                reply.error(ENOENT);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let inner = self.inner.read().unwrap();

        let key = match inner.inode_table.get_key(ino) {
            Some(k) => k.clone(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        match key {
            FileKey::Root => {
                // "." and ".." plus one entry per torrent
                let entries: Vec<(u64, FileType, String)> = {
                    let mut v = vec![
                        (1u64, FileType::Directory, ".".to_string()),
                        (1u64, FileType::Directory, "..".to_string()),
                    ];
                    for torrent in inner.torrents.values() {
                        v.push((torrent.dir_inode, FileType::Directory, torrent.torrent_id.clone()));
                    }
                    v
                };

                for (i, (entry_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                    // offset cookie is position + 1
                    let full = reply.add(*entry_ino, (i + 1) as i64, *kind, name);
                    if full {
                        break;
                    }
                }
                reply.ok();
            }
            FileKey::TorrentDir { torrent_id } => {
                let torrent = match inner.torrents.get(&torrent_id) {
                    Some(t) => t,
                    None => {
                        reply.error(ENOENT);
                        return;
                    }
                };

                let mut entries: Vec<(u64, FileType, String)> = vec![
                    (torrent.dir_inode, FileType::Directory, ".".to_string()),
                    (1u64, FileType::Directory, "..".to_string()),
                    (
                        torrent.mkv_inode,
                        FileType::RegularFile,
                        format!("{}.mkv", torrent.base_name),
                    ),
                ];
                for sub in &torrent.visible_subs {
                    entries.push((sub.inode, FileType::RegularFile, sub.exposed_name.clone()));
                }

                for (i, (entry_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                    let full = reply.add(*entry_ino, (i + 1) as i64, *kind, name);
                    if full {
                        break;
                    }
                }
                reply.ok();
            }
            _ => {
                reply.error(ENOTDIR);
            }
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        // Reject any write-access flags
        let access_mode = flags & libc::O_ACCMODE;
        if access_mode != libc::O_RDONLY {
            reply.error(EACCES);
            return;
        }

        let inner = self.inner.read().unwrap();
        match inner.inode_table.get_key(ino) {
            Some(FileKey::MkvVirtual { .. }) | Some(FileKey::Sidecar { .. }) => {
                reply.opened(0, 0);
            }
            _ => {
                reply.error(ENOENT);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(EIO);
            return;
        }
        let offset = offset as u64;

        // Take read lock to get what we need, then release before any blocking I/O
        let action = {
            let inner = self.inner.read().unwrap();
            let key = match inner.inode_table.get_key(ino) {
                Some(k) => k.clone(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };

            match key {
                FileKey::MkvVirtual { ref torrent_id } => {
                    let torrent = match inner.torrents.get(torrent_id.as_str()) {
                        Some(t) => t,
                        None => {
                            reply.error(ENOENT);
                            return;
                        }
                    };
                    let total = torrent.muxer.total_size();
                    if offset >= total {
                        // past EOF
                        return reply.data(&[]);
                    }
                    // Clone the Arc<Muxer> so we can release the lock
                    ReadAction::Mux { muxer: torrent.muxer.clone(), offset, size, total }
                }
                FileKey::Sidecar { ref torrent_id, ref exposed_name } => {
                    let torrent = match inner.torrents.get(torrent_id.as_str()) {
                        Some(t) => t,
                        None => {
                            reply.error(ENOENT);
                            return;
                        }
                    };
                    let sub = match torrent.visible_subs.iter().find(|s| &s.exposed_name == exposed_name) {
                        Some(s) => s,
                        None => {
                            reply.error(ENOENT);
                            return;
                        }
                    };
                    if offset >= sub.size {
                        return reply.data(&[]);
                    }
                    ReadAction::Disk { path: sub.disk_path.clone(), offset, size, total: sub.size }
                }
                _ => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };
        // Lock is now released. Perform I/O.

        match action {
            ReadAction::Mux { muxer, offset, size, total } => {
                let clamped = (size as u64).min(total.saturating_sub(offset)) as u32;
                match muxer.read(offset, clamped) {
                    Ok(bytes) => reply.data(&bytes),
                    Err(_) => reply.error(EIO),
                }
            }
            ReadAction::Disk { path, offset, size, total } => {
                let clamped = (size as u64).min(total.saturating_sub(offset)) as usize;
                match std::fs::File::open(&path) {
                    Ok(file) => {
                        let mut buf = vec![0u8; clamped];
                        match file.read_at(&mut buf, offset) {
                            Ok(n) => reply.data(&buf[..n]),
                            Err(_) => reply.error(EIO),
                        }
                    }
                    Err(_) => reply.error(EIO),
                }
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

/// Helper enum to carry the I/O decision out of the RwLock guard scope.
enum ReadAction {
    Mux {
        muxer: std::sync::Arc<videofuser_muxer::Muxer>,
        offset: u64,
        size: u32,
        total: u64,
    },
    Disk {
        path: std::path::PathBuf,
        offset: u64,
        size: u32,
        total: u64,
    },
}
