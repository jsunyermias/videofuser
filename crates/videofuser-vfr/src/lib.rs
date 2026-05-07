use std::io::{self, Read, Write};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use thiserror::Error;

const MAGIC: &[u8; 4] = b"VFRF";
const VERSION: u8 = 1;
/// Byte offset of the first FrameRecord from the start of the file.
const HEADER_SIZE: u64 = 24; // 4 magic + 1 version + 1 flags + 2 reserved + 8 frame_count + 8 nal_lengths_offset

#[derive(Debug, Error)]
pub enum VfrError {
    #[error("invalid magic: expected 'VFRF', got {0:?}")]
    InvalidMagic([u8; 4]),
    #[error("unsupported version: {0}")]
    UnsupportedVersion(u8),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRecord {
    /// Byte size of this frame in the raw track file.
    pub frame_size: u32,
    /// bit 0: keyframe; bit 1: has_nal_lengths.
    pub flags: u8,
    /// Number of NAL units (video) or 0 (audio).
    pub nal_count: u8,
    /// Delta of this frame's duration relative to TrackPolicy.frame_duration.
    pub duration_delta: i16,
}

impl FrameRecord {
    pub fn is_keyframe(&self) -> bool {
        self.flags & 0x01 != 0
    }
    pub fn has_nal_lengths(&self) -> bool {
        self.flags & 0x02 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfrFile {
    pub version: u8,
    /// bit 0: file has a NalLengths table.
    pub flags: u8,
    pub frames: Vec<FrameRecord>,
    /// Packed NAL unit lengths as unsigned integers (one per NAL unit, in frame order).
    /// Empty for audio-only files.
    pub nal_lengths: Vec<u64>,
}

impl VfrFile {
    pub fn has_nal_lengths_table(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub fn write_to(&self, w: &mut impl Write) -> Result<(), VfrError> {
        let frame_count = self.frames.len() as u64;
        let nal_lengths_offset: u64 = if self.has_nal_lengths_table() && !self.nal_lengths.is_empty() {
            HEADER_SIZE + frame_count * 8
        } else {
            0
        };

        w.write_all(MAGIC)?;
        w.write_u8(self.version)?;
        w.write_u8(self.flags)?;
        w.write_u16::<LittleEndian>(0)?; // reserved
        w.write_u64::<LittleEndian>(frame_count)?;
        w.write_u64::<LittleEndian>(nal_lengths_offset)?;

        for frame in &self.frames {
            w.write_u32::<LittleEndian>(frame.frame_size)?;
            w.write_u8(frame.flags)?;
            w.write_u8(frame.nal_count)?;
            w.write_i16::<LittleEndian>(frame.duration_delta)?;
        }

        if self.has_nal_lengths_table() {
            for &len in &self.nal_lengths {
                write_vint(w, len)?;
            }
        }

        Ok(())
    }

    pub fn read_from(r: &mut impl Read) -> Result<Self, VfrError> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(VfrError::InvalidMagic(magic));
        }

        let version = r.read_u8()?;
        if version != VERSION {
            return Err(VfrError::UnsupportedVersion(version));
        }

        let flags = r.read_u8()?;
        let _reserved = r.read_u16::<LittleEndian>()?;
        let frame_count = r.read_u64::<LittleEndian>()?;
        let _nal_lengths_offset = r.read_u64::<LittleEndian>()?;

        let mut frames = Vec::with_capacity(frame_count as usize);
        for _ in 0..frame_count {
            let frame_size = r.read_u32::<LittleEndian>()?;
            let frame_flags = r.read_u8()?;
            let nal_count = r.read_u8()?;
            let duration_delta = r.read_i16::<LittleEndian>()?;
            frames.push(FrameRecord { frame_size, flags: frame_flags, nal_count, duration_delta });
        }

        let has_nal = flags & 0x01 != 0;
        let mut nal_lengths = Vec::new();
        if has_nal {
            let total_nal_count: usize = frames.iter().map(|f| f.nal_count as usize).sum();
            let mut buf = Vec::new();
            r.read_to_end(&mut buf)?;
            let mut cursor = buf.as_slice();
            for _ in 0..total_nal_count {
                let (val, rest) = read_vint_from_slice(cursor)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                nal_lengths.push(val);
                cursor = rest;
            }
        }

        Ok(VfrFile { version, flags, frames, nal_lengths })
    }
}

// ---------------------------------------------------------------------------
// EBML VINT encoding (unsigned, big-endian, self-terminating)
// ---------------------------------------------------------------------------

fn vint_byte_len(value: u64) -> usize {
    match value {
        0..=0x7E => 1,
        0..=0x3FFE => 2,
        0..=0x1FFFFE => 3,
        0..=0x0FFFFFFE => 4,
        0..=0x07FFFFFFFE => 5,
        0..=0x03FFFFFFFFFE => 6,
        0..=0x01FFFFFFFFFFFE => 7,
        _ => 8,
    }
}

pub fn write_vint(w: &mut impl Write, value: u64) -> io::Result<()> {
    let n = vint_byte_len(value);
    let marker = 0x80u8 >> (n - 1);
    // Value packed into big-endian n bytes
    let be = value.to_be_bytes();
    let mut out = [0u8; 8];
    out[..n].copy_from_slice(&be[8 - n..]);
    out[0] |= marker;
    w.write_all(&out[..n])
}

pub fn read_vint_from_slice(buf: &[u8]) -> Result<(u64, &[u8]), &'static str> {
    if buf.is_empty() {
        return Err("unexpected end of VINT data");
    }
    let first = buf[0];
    let n = first.leading_zeros() as usize + 1;
    if n > 8 || buf.len() < n {
        return Err("invalid VINT encoding");
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (first & !mask) as u64;
    for &byte in &buf[1..n] {
        value = (value << 8) | byte as u64;
    }
    Ok((value, &buf[n..]))
}

pub fn read_vint(r: &mut impl Read) -> io::Result<u64> {
    let first = r.read_u8()?;
    let n = first.leading_zeros() as usize + 1;
    if n > 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid VINT"));
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (first & !mask) as u64;
    for _ in 1..n {
        value = (value << 8) | r.read_u8()? as u64;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(vfr: &VfrFile) -> VfrFile {
        let mut buf = Vec::new();
        vfr.write_to(&mut buf).unwrap();
        VfrFile::read_from(&mut buf.as_slice()).unwrap()
    }

    #[test]
    fn round_trip_video_with_nal_lengths() {
        let frames = vec![
            FrameRecord { frame_size: 4096, flags: 0x03, nal_count: 2, duration_delta: 0 },
            FrameRecord { frame_size: 1024, flags: 0x02, nal_count: 1, duration_delta: -5 },
            FrameRecord { frame_size: 2048, flags: 0x03, nal_count: 3, duration_delta: 10 },
        ];
        // 2 + 1 + 3 = 6 NAL lengths
        let nal_lengths = vec![200u64, 3896, 1024, 100u64, 924, 1024];
        let vfr = VfrFile { version: 1, flags: 0x01, frames, nal_lengths };

        let got = round_trip(&vfr);
        assert_eq!(got.version, 1);
        assert_eq!(got.flags, 0x01);
        assert_eq!(got.frames, vfr.frames);
        assert_eq!(got.nal_lengths, vfr.nal_lengths);
    }

    #[test]
    fn round_trip_audio_no_nal_lengths() {
        let frames = vec![
            FrameRecord { frame_size: 768, flags: 0x00, nal_count: 0, duration_delta: 0 },
            FrameRecord { frame_size: 768, flags: 0x00, nal_count: 0, duration_delta: 0 },
            FrameRecord { frame_size: 800, flags: 0x00, nal_count: 0, duration_delta: 3 },
        ];
        let vfr = VfrFile { version: 1, flags: 0x00, frames, nal_lengths: vec![] };

        let got = round_trip(&vfr);
        assert_eq!(got.frames, vfr.frames);
        assert!(got.nal_lengths.is_empty());
    }

    #[test]
    fn nal_lengths_offset_points_to_correct_byte() {
        let frames = vec![
            FrameRecord { frame_size: 100, flags: 0x03, nal_count: 2, duration_delta: 0 },
        ];
        let nal_lengths = vec![50u64, 50];
        let vfr = VfrFile { version: 1, flags: 0x01, frames, nal_lengths };

        let mut buf = Vec::new();
        vfr.write_to(&mut buf).unwrap();

        // NalLengthsOffset is at bytes 16..24 (u64 LE)
        let offset = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        // HEADER_SIZE=24, 1 frame × 8 bytes = 8 → offset should be 32
        assert_eq!(offset, 32);
        // And the bytes at that offset are valid VINTs
        let (v0, rest) = read_vint_from_slice(&buf[32..]).unwrap();
        let (v1, _) = read_vint_from_slice(rest).unwrap();
        assert_eq!(v0, 50);
        assert_eq!(v1, 50);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let mut bad = b"NOPE\x01\x00\x00\x00".to_vec();
        bad.extend_from_slice(&0u64.to_le_bytes()); // frame_count
        bad.extend_from_slice(&0u64.to_le_bytes()); // nal_offset
        let err = VfrFile::read_from(&mut bad.as_slice()).unwrap_err();
        assert!(matches!(err, VfrError::InvalidMagic(_)));
    }

    #[test]
    fn bad_version_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.push(99); // bad version
        buf.push(0);  // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
        buf.extend_from_slice(&0u64.to_le_bytes()); // frame_count
        buf.extend_from_slice(&0u64.to_le_bytes()); // nal_offset
        let err = VfrFile::read_from(&mut buf.as_slice()).unwrap_err();
        assert!(matches!(err, VfrError::UnsupportedVersion(99)));
    }

    #[test]
    fn vint_round_trips() {
        // Max encodable in 8-byte EBML VINT: 2^56 - 2
        for &v in &[0u64, 1, 63, 126, 127, 200, 16382, 16383, 100_000, 72_057_594_037_927_934] {
            let mut buf = Vec::new();
            write_vint(&mut buf, v).unwrap();
            let (decoded, rest) = read_vint_from_slice(&buf).unwrap();
            assert_eq!(decoded, v, "vint round-trip failed for {v}");
            assert!(rest.is_empty());
        }
    }
}
