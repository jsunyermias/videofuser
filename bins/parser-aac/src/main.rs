use std::fs;
use std::io;

use clap::{Parser, Subcommand};
use videofuser_parser_common::{
    CbrMeta, CodecParser, FrameInfo, ParseError, ParseResult, emit_result,
};

// ADTS sampling frequency index table (ISO 13818-7 Table 35)
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

// AAC: 1024 samples per frame
const SAMPLES_PER_FRAME: u64 = 1024;

#[derive(Parser)]
#[command(name = "parser-aac")]
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

struct AacParser;

impl CodecParser for AacParser {
    fn parse(&self, input: &[u8]) -> Result<ParseResult, ParseError> {
        if input.is_empty() {
            return Err(ParseError::new("empty input"));
        }

        // Validate first syncword
        if input.len() < 2 || (input[0] != 0xFF || (input[1] & 0xF0) != 0xF0) {
            return Err(ParseError::new("invalid ADTS syncword at offset 0"));
        }

        let mut frames: Vec<FrameInfo> = Vec::new();
        let mut pos: usize = 0;
        let mut sample_rate_ns: Option<u64> = None;

        while pos < input.len() {
            // Need at least 7 bytes for the minimal ADTS header
            if pos + 7 > input.len() {
                eprintln!(
                    "parser-aac: truncated frame at offset {pos}, only {} bytes remain — skipping",
                    input.len() - pos
                );
                break;
            }

            // Verify syncword
            if input[pos] != 0xFF || (input[pos + 1] & 0xF0) != 0xF0 {
                return Err(ParseError::new(format!(
                    "invalid ADTS syncword at offset {pos}"
                )));
            }

            let protection_absent = input[pos + 1] & 0x01;
            let header_len: usize = if protection_absent == 0 { 9 } else { 7 };

            // Extract aac_frame_length (13 bits):
            //   byte[3] bits[1:0] → frame_length[12:11]
            //   byte[4]           → frame_length[10:3]
            //   byte[5] bits[7:5] → frame_length[2:0]
            let frame_length = (((input[pos + 3] as u32) & 0x03) << 11)
                | ((input[pos + 4] as u32) << 3)
                | ((input[pos + 5] as u32) >> 5);

            if frame_length < header_len as u32 {
                return Err(ParseError::new(format!(
                    "frame_length {frame_length} < header_len {header_len} at offset {pos}"
                )));
            }

            if pos + frame_length as usize > input.len() {
                eprintln!(
                    "parser-aac: frame at offset {pos} declares length {frame_length} but only {} bytes remain — truncating",
                    input.len() - pos
                );
                // Record what we can
                frames.push(FrameInfo {
                    frame_size: (input.len() - pos) as u32,
                    flags: 0x01, // keyframe
                    nal_count: 0,
                    duration_delta: 0,
                    nal_lengths: vec![],
                });
                break;
            }

            // Extract sampling_frequency_index from byte[2] bits[5:2]
            if sample_rate_ns.is_none() {
                let sfi = ((input[pos + 2] >> 2) & 0x0F) as usize;
                if sfi < SAMPLE_RATES.len() {
                    let sr = SAMPLE_RATES[sfi] as u64;
                    sample_rate_ns = Some(SAMPLES_PER_FRAME * 1_000_000_000 / sr);
                }
            }

            frames.push(FrameInfo {
                frame_size: frame_length,
                flags: 0x01, // keyframe (audio)
                nal_count: 0,
                duration_delta: 0,
                nal_lengths: vec![],
            });

            pos += frame_length as usize;
        }

        if frames.is_empty() {
            return Err(ParseError::new("no ADTS frames found"));
        }

        let frame_duration_ns = sample_rate_ns.unwrap_or(0);

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
            eprintln!("parser-aac: cannot read {}: {e}", input.display());
            std::process::exit(1);
        }
    };

    let parser = AacParser;
    let result = match parser.parse(&data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("parser-aac: parse error: {e}");
            std::process::exit(1);
        }
    };

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    if let Err(e) = emit_result(&result, &mut out, &mut err) {
        eprintln!("parser-aac: emit error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ADTS frame with the given `frame_length` (including header).
    /// Uses protection_absent=1 (7-byte header), MPEG-4, 44.1 kHz (sfi=4), 2ch.
    fn adts_frame(frame_length: u32) -> Vec<u8> {
        let mut buf = vec![0u8; frame_length as usize];
        buf[0] = 0xFF;
        buf[1] = 0xF1; // syncword[3:0]=F, ID=0 (MPEG-4), layer=0, protection_absent=1
        // byte[2]: profile=01(LC), sfi=0100(44.1kHz), private=0, channel_MSB=0
        buf[2] = 0x50; // 0b01010000
        // byte[3]: channel[1:0]=10, original=0, home=0, cpyrt_id=0, cpyrt_start=0,
        //          frame_length[12:11] = 0b00 (for typical small sizes)
        let fl12_11 = ((frame_length >> 11) & 0x03) as u8;
        buf[3] = (0b10 << 6) | fl12_11; // channel config = 2 (stereo)
        let fl10_3 = ((frame_length >> 3) & 0xFF) as u8;
        buf[4] = fl10_3;
        let fl2_0 = ((frame_length & 0x07) as u8) << 5;
        buf[5] = fl2_0; // buffer_fullness bits are 0 (ok for test)
        buf[6] = 0x00;
        buf
    }

    #[test]
    fn cbr_three_frames() {
        // 3 identical frames of 128 bytes each
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend_from_slice(&adts_frame(128));
        }
        let parser = AacParser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Cbr(meta) => {
                assert_eq!(meta.frame_count, 3);
                assert_eq!(meta.cbr_frame_size, 128);
                // 44.1 kHz → frame_duration = 1024 * 1e9 / 44100 ≈ 23219954 ns
                assert_eq!(meta.frame_duration_ns, 1024 * 1_000_000_000 / 44100);
            }
            ParseResult::Vbr(_) => panic!("expected CBR"),
        }
    }

    #[test]
    fn vbr_two_different_sizes() {
        let mut data = Vec::new();
        data.extend_from_slice(&adts_frame(128));
        data.extend_from_slice(&adts_frame(192));
        data.extend_from_slice(&adts_frame(128));

        let parser = AacParser;
        let result = parser.parse(&data).unwrap();

        match result {
            ParseResult::Vbr(frames) => {
                assert_eq!(frames.len(), 3);
                assert_eq!(frames[0].frame_size, 128);
                assert_eq!(frames[1].frame_size, 192);
                assert_eq!(frames[2].frame_size, 128);
                // All audio frames are keyframes
                assert!(frames.iter().all(|f| f.flags & 0x01 == 1));
            }
            ParseResult::Cbr(_) => panic!("expected VBR"),
        }
    }

    #[test]
    fn empty_input_errors() {
        let parser = AacParser;
        assert!(parser.parse(&[]).is_err());
    }

    #[test]
    fn bad_syncword_errors() {
        let parser = AacParser;
        assert!(parser.parse(&[0x00, 0x00, 0x00]).is_err());
    }
}
