use std::io::Write;

use thiserror::Error;
use videofuser_vfr::{FrameRecord, VfrFile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInfo {
    pub frame_size: u32,
    /// bit 0: keyframe; bit 1: has_nal_lengths
    pub flags: u8,
    pub nal_count: u8,
    pub duration_delta: i16,
    /// Payload length of each NAL unit (no start code); empty for audio.
    pub nal_lengths: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct CbrMeta {
    pub frame_count: u64,
    pub cbr_frame_size: u32,
    pub frame_duration_ns: u64,
}

#[derive(Debug)]
pub enum ParseResult {
    Vbr(Vec<FrameInfo>),
    Cbr(CbrMeta),
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ParseError(pub String);

impl ParseError {
    pub fn new(msg: impl Into<String>) -> Self {
        ParseError(msg.into())
    }
}

pub trait CodecParser {
    fn parse(&self, input: &[u8]) -> Result<ParseResult, ParseError>;
}

/// For `ParseResult::Vbr` — builds a `VfrFile` and writes it to `stdout`.
/// For `ParseResult::Cbr` — writes the metadata JSON to `stderr`.
pub fn emit_result(
    result: &ParseResult,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    match result {
        ParseResult::Vbr(frames) => {
            let has_nal = frames.iter().any(|f| !f.nal_lengths.is_empty());
            let file_flags: u8 = if has_nal { 1 } else { 0 };

            let records: Vec<FrameRecord> = frames
                .iter()
                .map(|f| FrameRecord {
                    frame_size: f.frame_size,
                    flags: f.flags,
                    nal_count: f.nal_count,
                    duration_delta: f.duration_delta,
                })
                .collect();

            let nal_lengths: Vec<u64> = frames
                .iter()
                .flat_map(|f| f.nal_lengths.iter().map(|&l| l as u64))
                .collect();

            let vfr = VfrFile {
                version: 1,
                flags: file_flags,
                frames: records,
                nal_lengths,
            };

            vfr.write_to(stdout)?;
        }
        ParseResult::Cbr(meta) => {
            let json = format!(
                "{{\"is_vbr\":false,\"frame_count\":{},\"cbr_frame_size\":{},\"frame_duration_ns\":{}}}",
                meta.frame_count, meta.cbr_frame_size, meta.frame_duration_ns
            );
            writeln!(stderr, "{json}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use videofuser_vfr::VfrFile;

    #[test]
    fn emit_vbr_round_trips_through_vfrfile() {
        let frames = vec![
            FrameInfo {
                frame_size: 100,
                flags: 0x01,
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            },
            FrameInfo {
                frame_size: 200,
                flags: 0x00,
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            },
        ];
        let result = ParseResult::Vbr(frames);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        emit_result(&result, &mut stdout, &mut stderr).unwrap();

        let vfr = VfrFile::read_from(&mut stdout.as_slice()).unwrap();
        assert_eq!(vfr.frames.len(), 2);
        assert_eq!(vfr.frames[0].frame_size, 100);
        assert_eq!(vfr.frames[1].frame_size, 200);
        assert!(stderr.is_empty());
    }

    #[test]
    fn emit_cbr_writes_json_to_stderr() {
        let meta = CbrMeta {
            frame_count: 42,
            cbr_frame_size: 128,
            frame_duration_ns: 23219954,
        };
        let result = ParseResult::Cbr(meta);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        emit_result(&result, &mut stdout, &mut stderr).unwrap();

        assert!(stdout.is_empty());
        let s = String::from_utf8(stderr).unwrap();
        assert!(s.contains("\"is_vbr\":false"));
        assert!(s.contains("\"frame_count\":42"));
        assert!(s.contains("\"cbr_frame_size\":128"));
        assert!(s.contains("\"frame_duration_ns\":23219954"));
    }
}
