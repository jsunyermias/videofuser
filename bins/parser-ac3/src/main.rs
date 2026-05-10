use std::fs;
use std::io;

use clap::{Parser, Subcommand};
use videofuser_parser_common::{
    CbrMeta, CodecParser, FrameInfo, ParseError, ParseResult, emit_result,
};

/// AC-3 frame sizes in bytes per ATSC A/52 Table 4.13.
/// Indexed by [fscod][frmsizecod] where fscod: 0=48kHz, 1=44.1kHz, 2=32kHz.
/// Values are frame sizes in bytes (words × 2).
#[rustfmt::skip]
const AC3_FRAME_SIZE: [[u32; 38]; 3] = [
    // fscod=0, 48 kHz
    [
        128, 128, 160, 160, 192, 192, 224, 224, 256, 256,
        320, 320, 384, 384, 448, 448, 512, 512, 640, 640,
        768, 768, 896, 896, 1024, 1024, 1280, 1280, 1536, 1536,
        1792, 1792, 2048, 2048, 2304, 2304, 2560, 2560,
    ],
    // fscod=1, 44.1 kHz (from A/52 Table 4.13, words×2)
    [
        138, 140, 174, 174, 208, 210, 242, 244, 278, 278,
        348, 348, 416, 418, 486, 488, 556, 556, 696, 696,
        834, 836, 974, 976, 1114, 1114, 1392, 1392, 1670, 1672,
        1950, 1950, 2228, 2228, 2506, 2508, 2786, 2788,
    ],
    // fscod=2, 32 kHz
    [
        192, 192, 240, 240, 288, 288, 336, 336, 384, 384,
        480, 480, 576, 576, 672, 672, 768, 768, 960, 960,
        1152, 1152, 1344, 1344, 1536, 1536, 1920, 1920, 2304, 2304,
        2688, 2688, 3072, 3072, 3456, 3456, 3840, 3840,
    ],
];

// Samples per AC-3 frame
const AC3_SAMPLES_PER_FRAME: u64 = 1536;
// Sample rates indexed by fscod
const AC3_SAMPLE_RATES: [u64; 3] = [48000, 44100, 32000];

#[derive(Parser)]
#[command(name = "parser-ac3")]
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
        input: std::path::PathBuf,
    },
}

struct Ac3Parser;

impl CodecParser for Ac3Parser {
    fn parse(&self, input: &[u8]) -> Result<ParseResult, ParseError> {
        if input.len() < 5 {
            return Err(ParseError::new("input too short for AC-3"));
        }

        // Validate first syncword
        if input[0] != 0x0B || input[1] != 0x77 {
            return Err(ParseError::new(format!(
                "invalid AC-3 syncword at offset 0: {:02X} {:02X}",
                input[0], input[1]
            )));
        }

        let mut frames: Vec<FrameInfo> = Vec::new();
        let mut pos: usize = 0;

        while pos < input.len() {
            if pos + 5 > input.len() {
                eprintln!(
                    "parser-ac3: truncated frame at offset {pos}, only {} bytes remain — skipping",
                    input.len() - pos
                );
                break;
            }

            // Verify syncword
            if input[pos] != 0x0B || input[pos + 1] != 0x77 {
                return Err(ParseError::new(format!(
                    "invalid AC-3 syncword at offset {pos}"
                )));
            }

            // byte[4]: bits[7:6] = fscod, bits[5:0] = frmsizecod
            let fscod = (input[pos + 4] >> 6) as usize;
            let frmsizecod = (input[pos + 4] & 0x3F) as usize;

            if fscod == 3 {
                return Err(ParseError::new(format!(
                    "reserved fscod=3 at offset {pos}"
                )));
            }
            if frmsizecod >= 38 {
                return Err(ParseError::new(format!(
                    "invalid frmsizecod={frmsizecod} at offset {pos}"
                )));
            }

            let frame_size = AC3_FRAME_SIZE[fscod][frmsizecod];

            if pos + frame_size as usize > input.len() {
                eprintln!(
                    "parser-ac3: frame at offset {pos} declares size {frame_size} but only {} bytes remain — truncating",
                    input.len() - pos
                );
                frames.push(FrameInfo {
                    frame_size: (input.len() - pos) as u32,
                    flags: 0x01,
                    nal_count: 0,
                    duration_delta: 0,
                    nal_lengths: vec![],
                });
                break;
            }

            frames.push(FrameInfo {
                frame_size,
                flags: 0x01, // keyframe (audio)
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            });

            pos += frame_size as usize;
        }

        if frames.is_empty() {
            return Err(ParseError::new("no AC-3 frames found"));
        }

        // Derive frame_duration_ns from fscod of first frame
        let fscod_first = (input[4] >> 6) as usize;
        let frame_duration_ns = if fscod_first < 3 {
            AC3_SAMPLES_PER_FRAME * 1_000_000_000 / AC3_SAMPLE_RATES[fscod_first]
        } else {
            0
        };

        // CBR: all frame sizes equal
        let first_size = frames[0].frame_size;
        if frames.iter().all(|f| f.frame_size == first_size) {
            Ok(ParseResult::Cbr(CbrMeta {
                frame_count: frames.len() as u64,
                cbr_frame_size: first_size,
                frame_duration_ns,
            }))
        } else {
            Ok(ParseResult::Vbr(frames))
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let Cmd::Index { input, .. } = cli.cmd;

    let data = match fs::read(&input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("parser-ac3: cannot read {}: {e}", input.display());
            std::process::exit(1);
        }
    };

    let parser = Ac3Parser;
    let result = match parser.parse(&data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("parser-ac3: parse error: {e}");
            std::process::exit(1);
        }
    };

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    if let Err(e) = emit_result(&result, &mut out, &mut err) {
        eprintln!("parser-ac3: emit error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal AC-3 frame with the given `fscod` and `frmsizecod`.
    /// Fills the frame with zeros beyond the 5-byte header.
    fn ac3_frame(fscod: u8, frmsizecod: u8) -> Vec<u8> {
        let size = AC3_FRAME_SIZE[fscod as usize][frmsizecod as usize] as usize;
        let mut buf = vec![0u8; size];
        buf[0] = 0x0B;
        buf[1] = 0x77;
        // crc1 at bytes 2-3, leave as 0
        buf[4] = (fscod << 6) | (frmsizecod & 0x3F);
        buf
    }

    #[test]
    fn cbr_three_frames_48khz() {
        // fscod=0, frmsizecod=0 → 128 bytes per frame
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend_from_slice(&ac3_frame(0, 0));
        }
        assert_eq!(data.len(), 384);

        let parser = Ac3Parser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Cbr(meta) => {
                assert_eq!(meta.frame_count, 3);
                assert_eq!(meta.cbr_frame_size, 128);
                // 48 kHz → 1536 * 1e9 / 48000 = 32_000_000 ns
                assert_eq!(meta.frame_duration_ns, 1536 * 1_000_000_000 / 48000);
            }
            ParseResult::Vbr(_) => panic!("expected CBR"),
        }
    }

    #[test]
    fn vbr_variable_frmsizecod() {
        let mut data = Vec::new();
        data.extend_from_slice(&ac3_frame(0, 0)); // 128 bytes
        data.extend_from_slice(&ac3_frame(0, 2)); // 160 bytes
        data.extend_from_slice(&ac3_frame(0, 0)); // 128 bytes

        let parser = Ac3Parser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Vbr(frames) => {
                assert_eq!(frames.len(), 3);
                assert_eq!(frames[0].frame_size, 128);
                assert_eq!(frames[1].frame_size, 160);
                assert_eq!(frames[2].frame_size, 128);
                assert!(frames.iter().all(|f| f.flags & 0x01 == 1));
            }
            ParseResult::Cbr(_) => panic!("expected VBR"),
        }
    }

    #[test]
    fn bad_syncword_errors() {
        let parser = Ac3Parser;
        assert!(parser.parse(&[0x00; 10]).is_err());
    }

    #[test]
    fn reserved_fscod_errors() {
        let mut buf = ac3_frame(0, 0);
        buf[4] = 0b11000000; // fscod=3
        let parser = Ac3Parser;
        assert!(parser.parse(&buf).is_err());
    }
}
