use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use videofuser_binstruct::TrackPolicy;
use videofuser_vfr::VfrFile;

#[derive(Debug, Error)]
pub enum MuxerError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("range out of bounds: offset={offset}, len={len}, total={total}")]
    OutOfBounds { offset: u64, len: u64, total: u64 },
    #[error("download state shutting down")]
    Shutdown,
    #[error("missing track {0}")]
    MissingTrack(u32),
    #[error("invalid bitstream: {0}")]
    InvalidBitstream(String),
    #[error("invalid binstruct: {0}")]
    InvalidBinstruct(String),
}

/// Blocking policy used when consulting a [`DownloadState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadPolicy {
    /// Block forever until the data is available.
    Block,
    /// Block until available or the timeout elapses.
    Timeout(Duration),
    /// Never block; return immediately.
    NonBlock,
}

/// Reports whether bytes of a raw track file are present on disk.
pub trait DownloadState: Send + Sync {
    /// Returns true if the range `[offset, offset+len)` of the raw file of
    /// `track_id` is fully available locally.
    fn is_available(&self, track_id: u32, offset: u64, len: u32) -> bool;

    /// Blocks until the range is available or the policy elapses.
    /// `Ok(true)` = available, `Ok(false)` = timeout / non-block miss, `Err` = shutdown.
    fn wait_for(
        &self,
        track_id: u32,
        offset: u64,
        len: u32,
        policy: ReadPolicy,
    ) -> Result<bool, MuxerError>;
}

/// Mock implementation: every byte is always available.
pub struct FullyAvailable;

impl DownloadState for FullyAvailable {
    fn is_available(&self, _track_id: u32, _offset: u64, _len: u32) -> bool {
        true
    }

    fn wait_for(
        &self,
        _track_id: u32,
        _offset: u64,
        _len: u32,
        _policy: ReadPolicy,
    ) -> Result<bool, MuxerError> {
        Ok(true)
    }
}

/// Byte source for the raw payload of a track.
pub trait RawFile: Send + Sync {
    /// Read up to `buf.len()` bytes starting at `offset`. Returns the number
    /// of bytes actually written into `buf` (may be less than buf.len() at EOF).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, MuxerError>;

    /// Total size of the underlying raw track file.
    fn size(&self) -> u64;
}

/// Real `RawFile` backed by a `std::fs::File` using `pread`.
pub struct DiskRawFile {
    file: File,
    size: u64,
}

impl DiskRawFile {
    pub fn new(path: &std::path::Path) -> Result<Self, MuxerError> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl RawFile for DiskRawFile {
    #[cfg(unix)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, MuxerError> {
        let n = self.file.read_at(buf, offset)?;
        Ok(n)
    }

    #[cfg(not(unix))]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, MuxerError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = self.file.try_clone()?;
        f.seek(SeekFrom::Start(offset))?;
        let n = f.read(buf)?;
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// In-memory `RawFile` for tests / synthetic data.
pub struct MemRawFile {
    data: Vec<u8>,
}

impl MemRawFile {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl RawFile for MemRawFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, MuxerError> {
        if offset >= self.data.len() as u64 {
            return Ok(0);
        }
        let start = offset as usize;
        let end = (start + buf.len()).min(self.data.len());
        let n = end - start;
        buf[..n].copy_from_slice(&self.data[start..end]);
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.data.len() as u64
    }
}

/// Per-track byte source plus optional VFR index plus its policy.
pub struct TrackSource {
    pub track_id: u32,
    pub raw_file: Arc<dyn RawFile>,
    /// `None` for CBR audio; `Some` for VBR audio and all video tracks.
    pub vfr: Option<Arc<VfrFile>>,
    pub policy: TrackPolicy,
}
