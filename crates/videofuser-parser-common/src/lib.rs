use std::io;

use byteorder::{LittleEndian, WriteBytesExt};

// ---------------------------------------------------------------------------
// CBR output (emitted to stderr as JSON when all frames have equal size)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbrInfo {
    pub frame_count: u64,
    pub cbr_frame_size: u32,
    /// Duration of each frame in nanoseconds.
    pub frame_duration_ns: u64,
    /// ISO 639 language code (passed in from CLI, may be empty).
    pub language: String,
}

impl CbrInfo {
    pub fn write_json(&self, w: &mut impl io::Write) -> io::Result<()> {
        writeln!(
            w,
            r#"{{"is_vbr": false, "frame_count": {fc}, "cbr_frame_size": {fs}, "frame_duration_ns": {fd}, "language": "{lang}"}}"#,
            fc   = self.frame_count,
            fs   = self.cbr_frame_size,
            fd   = self.frame_duration_ns,
            lang = self.language,
        )
    }
}

// ---------------------------------------------------------------------------
// Result type returned by all parsers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEntry {
    /// Byte size of the frame in the raw file.
    pub frame_size: u32,
    /// bit 0: keyframe; bit 1: has_nal_lengths.
    pub flags: u8,
    /// Number of NAL units (video) or 0 (audio).
    pub nal_count: u8,
    /// Delta vs. the codec's nominal frame duration.
    pub duration_delta: i16,
    /// NAL unit payload lengths (excluding start code), in order.
    /// Empty for audio frames.
    pub nal_lengths: Vec<u64>,
}

impl FrameEntry {
    pub fn is_keyframe(&self) -> bool {
        self.flags & 0x01 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    Vbr {
        frames: Vec<FrameEntry>,
        /// Whether the file contains a NalLengths table (video parsers set this).
        has_nal_lengths: bool,
    },
    Cbr(CbrInfo),
}

// ---------------------------------------------------------------------------
// VFR serializer helper (common to all parsers that produce VBR output)
// ---------------------------------------------------------------------------

pub fn write_vfr(
    frames: &[FrameEntry],
    has_nal_lengths: bool,
    w: &mut impl io::Write,
) -> io::Result<()> {
    const MAGIC: &[u8; 4] = b"VFRF";
    const VERSION: u8 = 1;
    const HEADER_SIZE: u64 = 24;

    let frame_count = frames.len() as u64;
    let flags: u8 = if has_nal_lengths { 0x01 } else { 0x00 };
    let nal_lengths_offset: u64 = if has_nal_lengths {
        HEADER_SIZE + frame_count * 8
    } else {
        0
    };

    w.write_all(MAGIC)?;
    w.write_u8(VERSION)?;
    w.write_u8(flags)?;
    w.write_u16::<LittleEndian>(0)?; // reserved
    w.write_u64::<LittleEndian>(frame_count)?;
    w.write_u64::<LittleEndian>(nal_lengths_offset)?;

    for f in frames {
        w.write_u32::<LittleEndian>(f.frame_size)?;
        w.write_u8(f.flags)?;
        w.write_u8(f.nal_count)?;
        w.write_i16::<LittleEndian>(f.duration_delta)?;
    }

    if has_nal_lengths {
        for f in frames {
            for &len in &f.nal_lengths {
                write_vint(w, len)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared utility: detect CBR from a frame list
// ---------------------------------------------------------------------------

pub fn detect_cbr(frames: &[FrameEntry]) -> Option<u32> {
    if frames.is_empty() {
        return None;
    }
    let first = frames[0].frame_size;
    if frames.iter().all(|f| f.frame_size == first) {
        Some(first)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// EBML VINT (same encoding as videofuser-vfr; duplicated to avoid dep cycle)
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

pub fn write_vint(w: &mut impl io::Write, value: u64) -> io::Result<()> {
    let n = vint_byte_len(value);
    let marker = 0x80u8 >> (n - 1);
    let be = value.to_be_bytes();
    let mut out = [0u8; 8];
    out[..n].copy_from_slice(&be[8 - n..]);
    out[0] |= marker;
    w.write_all(&out[..n])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbr_info_json_format() {
        let info = CbrInfo {
            frame_count: 1000,
            cbr_frame_size: 768,
            frame_duration_ns: 32_000_000,
            language: "es".to_string(),
        };
        let mut buf = Vec::new();
        info.write_json(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains(r#""is_vbr": false"#));
        assert!(s.contains(r#""frame_count": 1000"#));
        assert!(s.contains(r#""cbr_frame_size": 768"#));
        assert!(s.contains(r#""frame_duration_ns": 32000000"#));
        assert!(s.contains(r#""language": "es""#));
    }

    #[test]
    fn detect_cbr_all_same() {
        let frames: Vec<FrameEntry> = (0..5)
            .map(|_| FrameEntry {
                frame_size: 512,
                flags: 0x01,
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            })
            .collect();
        assert_eq!(detect_cbr(&frames), Some(512));
    }

    #[test]
    fn detect_cbr_variable() {
        let sizes = [512u32, 512, 1024, 512];
        let frames: Vec<FrameEntry> = sizes
            .iter()
            .map(|&s| FrameEntry {
                frame_size: s,
                flags: 0x01,
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            })
            .collect();
        assert_eq!(detect_cbr(&frames), None);
    }

    #[test]
    fn write_vfr_audio_no_nals() {
        let frames = vec![
            FrameEntry {
                frame_size: 768,
                flags: 0x01,
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            };
            3
        ];
        let mut buf = Vec::new();
        write_vfr(&frames, false, &mut buf).unwrap();
        assert_eq!(&buf[..4], b"VFRF");
        assert_eq!(buf[4], 1);    // version
        assert_eq!(buf[5], 0);    // no nal flag
        let fc = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(fc, 3);
    }

    #[test]
    fn write_vfr_video_with_nals() {
        let frames = vec![
            FrameEntry {
                frame_size: 1000,
                flags: 0x03,
                nal_count: 2,
                duration_delta: 0,
                nal_lengths: vec![400, 600],
            },
            FrameEntry {
                frame_size: 500,
                flags: 0x02,
                nal_count: 1,
                duration_delta: 0,
                nal_lengths: vec![500],
            },
        ];
        let mut buf = Vec::new();
        write_vfr(&frames, true, &mut buf).unwrap();
        assert_eq!(&buf[..4], b"VFRF");
        assert_eq!(buf[5], 0x01); // has_nal_lengths flag
        let fc = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(fc, 2);
        // NalLengthsOffset = 24 + 2*8 = 40
        let offset = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        assert_eq!(offset, 40);
    }
}
