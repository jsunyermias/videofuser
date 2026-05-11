// videofuser-fs: FUSE multiplexor filesystem.
// Serves virtual MKV files and subtitle sidecars under a single mountpoint.

pub mod filesystem_impl;
pub mod fs;
pub mod inode;
pub mod types;

#[cfg(test)]
mod tests;

pub use fs::VidFuserFs;
pub use types::{ExposedSub, FileKey, MountedTorrent};
