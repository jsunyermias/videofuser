use std::fs;
use std::io;

use clap::{Parser, Subcommand};
use videofuser_parser_common::{CodecParser, FrameInfo, ParseError, ParseResult, emit_result};

#[derive(Parser)]
#[command(name = "parser-h264")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Index {
        #[arg(long)]
        variant: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        level: Option<String>,
        input: std::path::PathBuf,
    },
}

struct H264Parser;

/// Scan `data` for Annex B start codes (3-byte or 4-byte).
/// Returns `(byte_offset, start_code_len)` for each NAL unit found.
fn find_nal_positions(data: &[u8]) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if i + 4 <= data.len() && data[i + 2] == 0x00 && data[i + 3] == 0x01 {
                positions.push((i, 4));
                i += 4;
                continue;
            } else if data[i + 2] == 0x01 {
                positions.push((i, 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    positions
}

impl CodecParser for H264Parser {
    fn parse(&self, input: &[u8]) -> Result<ParseResult, ParseError> {
        let nal_positions = find_nal_positions(input);

        if nal_positions.is_empty() {
            return Err(ParseError::new("no Annex B start codes found"));
        }

        // Collect (start_pos, start_code_len, nal_type) for each NAL
        let nals: Vec<(usize, usize, u8)> = nal_positions
            .iter()
            .filter_map(|&(pos, sc_len)| {
                let payload_start = pos + sc_len;
                if payload_start < input.len() {
                    let nal_type = input[payload_start] & 0x1F;
                    Some((pos, sc_len, nal_type))
                } else {
                    None
                }
            })
            .collect();

        if nals.is_empty() {
            return Err(ParseError::new("no valid NAL units found"));
        }

        // Skip leading SPS (type 7) and PPS (type 8) injected by mkvextract
        let mut idx = 0;
        while idx < nals.len() && (nals[idx].2 == 7 || nals[idx].2 == 8) {
            idx += 1;
        }

        if idx == nals.len() {
            return Err(ParseError::new("only SPS/PPS found, no slice NALs"));
        }

        // Group remaining NALs into frames.
        // Strategy: accumulate non-slice NALs; when a slice NAL is seen, emit
        // a frame containing all accumulated NALs + the slice NAL.
        // Slice types: IDR=5, non-IDR=1
        let remaining = &nals[idx..];
        let mut frames: Vec<FrameInfo> = Vec::new();
        let mut pending: Vec<(usize, usize, u8)> = Vec::new(); // (pos, sc_len, nal_type)

        for i in 0..remaining.len() {
            let (pos, sc_len, nal_type) = remaining[i];
            let is_slice = nal_type == 1 || nal_type == 5;
            pending.push((pos, sc_len, nal_type));

            if is_slice {
                // Determine byte end of this frame
                let frame_end = if i + 1 < remaining.len() {
                    remaining[i + 1].0
                } else {
                    input.len()
                };

                let frame_start = pending[0].0;
                let frame_size = (frame_end - frame_start) as u32;
                let nal_count = pending.len() as u8;
                let is_keyframe = pending.iter().any(|&(_, _, t)| t == 5);

                // Compute per-NAL payload lengths (without start code bytes)
                let mut nal_lengths = Vec::with_capacity(pending.len());
                for j in 0..pending.len() {
                    let (nal_pos, nal_sc, _) = pending[j];
                    let payload_start = nal_pos + nal_sc;
                    let payload_end = if j + 1 < pending.len() {
                        pending[j + 1].0
                    } else {
                        frame_end
                    };
                    let len = payload_end.saturating_sub(payload_start) as u32;
                    nal_lengths.push(len);
                }

                let flags = if is_keyframe { 0x03 } else { 0x02 }; // keyframe | has_nal_lengths
                frames.push(FrameInfo {
                    frame_size,
                    flags,
                    nal_count,
                    duration_delta: 0,
                    nal_lengths,
                });

                pending.clear();
            }
        }

        if frames.is_empty() {
            return Err(ParseError::new("no slice NALs found after SPS/PPS"));
        }

        Ok(ParseResult::Vbr(frames))
    }
}

fn main() {
    let cli = Cli::parse();
    let Cmd::Index { input, .. } = cli.cmd;

    let data = match fs::read(&input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("parser-h264: cannot read {}: {e}", input.display());
            std::process::exit(1);
        }
    };

    let parser = H264Parser;
    let result = match parser.parse(&data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("parser-h264: parse error: {e}");
            std::process::exit(1);
        }
    };

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    if let Err(e) = emit_result(&result, &mut out, &mut err) {
        eprintln!("parser-h264: emit error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal NAL unit with a 4-byte start code and a 1-byte header
    /// plus `extra` payload bytes (all zeros).
    fn nal(nal_type: u8, extra_payload: usize) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        v.push(nal_type); // nal_unit_type in low 5 bits; no forbidden/nal_ref_idc bits set
        v.extend(vec![0u8; extra_payload]);
        v
    }

    #[test]
    fn sps_pps_skipped_idr_non_idr_detected() {
        // Layout: SPS PPS IDR non-IDR
        // SPS payload byte: 0x67 (0b01100111 → type = 7)
        // PPS payload byte: 0x68 (0b01101000 → type = 8)
        // IDR payload byte: 0x65 (0b01100101 → type = 5)
        // non-IDR payload byte: 0x41 (0b01000001 → type = 1)
        let mut data = Vec::new();
        data.extend_from_slice(&nal(0x67, 0)); // SPS
        data.extend_from_slice(&nal(0x68, 0)); // PPS
        data.extend_from_slice(&nal(0x65, 0)); // IDR
        data.extend_from_slice(&nal(0x41, 0)); // non-IDR

        let parser = H264Parser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Vbr(frames) => {
                // Exactly 2 frames (SPS and PPS excluded)
                assert_eq!(frames.len(), 2, "expected 2 frames, got {}", frames.len());

                // Frame 0: IDR → keyframe flag set
                assert_eq!(frames[0].flags & 0x01, 1, "frame 0 should be keyframe");
                assert_eq!(frames[0].nal_count, 1);
                // Frame 1: non-IDR → no keyframe
                assert_eq!(frames[1].flags & 0x01, 0, "frame 1 should not be keyframe");
                assert_eq!(frames[1].nal_count, 1);

                // Each frame has 1 NAL; payload = 1 header byte + 0 extra = 1 byte
                assert_eq!(frames[0].nal_lengths, vec![1]);
                assert_eq!(frames[1].nal_lengths, vec![1]);

                // frame_size: 4-byte start code + 1 payload byte = 5 bytes each
                assert_eq!(frames[0].frame_size, 5);
                assert_eq!(frames[1].frame_size, 5);
            }
            ParseResult::Cbr(_) => panic!("H.264 must always be VBR"),
        }
    }

    #[test]
    fn three_byte_start_code_supported() {
        // 3-byte start code: 0x00 0x00 0x01
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x01, 0x65]); // IDR, 3-byte sc
        data.extend_from_slice(&[0x00, 0x00, 0x01, 0x41]); // non-IDR, 3-byte sc

        let parser = H264Parser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Vbr(frames) => {
                assert_eq!(frames.len(), 2);
                assert_eq!(frames[0].flags & 0x01, 1); // keyframe
                assert_eq!(frames[1].flags & 0x01, 0); // not keyframe
            }
            ParseResult::Cbr(_) => panic!("expected VBR"),
        }
    }

    #[test]
    fn sei_before_slice_belongs_to_same_frame() {
        // SEI (type 6) then IDR → one frame with 2 NALs
        let mut data = Vec::new();
        data.extend_from_slice(&nal(0x06, 2)); // SEI
        data.extend_from_slice(&nal(0x65, 0)); // IDR

        let parser = H264Parser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Vbr(frames) => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].nal_count, 2);
                assert_eq!(frames[0].flags & 0x01, 1); // keyframe
                assert_eq!(frames[0].nal_lengths.len(), 2);
            }
            ParseResult::Cbr(_) => panic!("expected VBR"),
        }
    }

    #[test]
    fn no_start_codes_errors() {
        let parser = H264Parser;
        assert!(parser.parse(&[0xDE, 0xAD, 0xBE, 0xEF]).is_err());
    }
}
