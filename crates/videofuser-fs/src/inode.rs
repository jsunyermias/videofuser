use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::FileKey;

/// Inode allocator. FUSE requires that inode 1 is the root.
pub struct InodeAllocator {
    next: AtomicU64,
}

impl InodeAllocator {
    pub fn new() -> Self {
        Self { next: AtomicU64::new(2) }
    }

    pub fn alloc(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for InodeAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Bidirectional inode ↔ FileKey table.
pub struct InodeTable {
    inode_to_key: HashMap<u64, FileKey>,
    key_to_inode: HashMap<FileKey, u64>,
}

impl InodeTable {
    pub fn new() -> Self {
        Self {
            inode_to_key: HashMap::new(),
            key_to_inode: HashMap::new(),
        }
    }

    pub fn insert(&mut self, inode: u64, key: FileKey) {
        self.key_to_inode.insert(key.clone(), inode);
        self.inode_to_key.insert(inode, key);
    }

    pub fn get_key(&self, inode: u64) -> Option<&FileKey> {
        self.inode_to_key.get(&inode)
    }

    pub fn get_inode(&self, key: &FileKey) -> Option<u64> {
        self.key_to_inode.get(key).copied()
    }

    pub fn remove_by_inode(&mut self, inode: u64) {
        if let Some(key) = self.inode_to_key.remove(&inode) {
            self.key_to_inode.remove(&key);
        }
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}
