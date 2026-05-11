use std::{
    fs,
    io::{self},
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use videofuser_parser_common::{detect_cbr, write_vfr, CbrInfo, FrameEntry};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "parser-ac3", about = "AC-3 ATSC A/52 bitstream indexer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index an AC-3 raw file and produce a VFR on stdout (or CBR JSON on stderr).
    Index {
        #[arg(long)]
        variant: String,

        #[arg(long = "version")]
        ver: String,

        /// Language code (ISO 639, stored in CBR JSON output).
        #[arg(long, default_value = "")]
        language: String,

        /// Input raw AC-3 file.
        input: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { input, language, .. } => {
            let data = fs::read(&input).unwrap_or_else(|e| {
                eprintln!("parser-ac3: cannot read {}: {}", input.display(), e);
                std::process::exit(2);
            });

            match index_ac3(&data) {
                Ok(frames) => {
                    if frames.is_empty() {
                        eprintln!("parser-ac3: no AC-3 sync frames found");
                        std::process::exit(3);
                    }

                    // AC-3 frame duration: 1536 samples.
                    let sample_rate = ac3_sample_rate(&data).unwrap_or(48000);
                    let frame_duration_ns = 1_536_000_000_000u64 / sample_rate as u64;

                    if let Some(cbr_size) = detect_cbr(&frames) {
                        let info = CbrInfo {
                            frame_count: frames.len() as u64,
                            cbr_frame_size: cbr_size,
                            frame_duration_ns,
                            language,
                        };
                        info.write_json(&mut io::stderr()).unwrap();
                    } else {
                        let stdout = io::stdout();
                        let mut out = stdout.lock();
                        write_vfr(&frames, false, &mut out).unwrap_or_else(|e| {
                            eprintln!("parser-ac3: write error: {e}");
                            std::process::exit(2);
                        });
                    }
                }
                Err(e) => {
                    eprintln!("parser-ac3: parse error: {e}");
                    std::process::exit(3);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AC-3 frame size table (ATSC A/52, Table 4.1)
// Index: fscod (0-2) × 38 frmsizecod values → frame size in bytes (words × 2).
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const AC3_FRAME_SIZES: [[u16; 38]; 3] = [
    // fscod=0: 48 kHz
    [256,256,320,320,384,384,448,448,512,512,640,640,768,768,896,896,
     1024,1024,1280,1280,1536,1536,1792,1792,2048,2048,2304,2304,2560,2560,
     2816,2816,3072,3072,3328,3328,3584,3584],
    // fscod=1: 44.1 kHz
    [276,280,348,350,416,418,484,488,556,560,696,700,834,836,974,976,
     1114,1116,1394,1396,1664,1670,1950,1954,2228,2232,2506,2512,2786,2790,
     3066,3070,3346,3350,3626,3630,3906,3908],
    // fscod=2: 32 kHz
    [384,384,480,480,576,576,672,672,768,768,960,960,1152,1152,1344,1344,
     1536,1536,1920,1920,2304,2304,2688,2688,3072,3072,3456,3456,3840,3840,
     4224,4224,4608,4608,4992,4992,5376,5376],
];

const AC3_SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];

fn ac3_frame_size(header: &[u8]) -> Option<u32> {
    if header.len() < 5 {
        return None;
    }
    let fscod = (header[4] >> 6) as usize;
    let frmsizecod = (header[4] & 0x3F) as usize;
    if fscod >= 3 || frmsizecod >= 38 {
        return None;
    }
    Some(AC3_FRAME_SIZES[fscod][frmsizecod] as u32)
}

fn ac3_sample_rate(data: &[u8]) -> Option<u32> {
    if data.len() < 5 {
        return None;
    }
    let fscod = (data[4] >> 6) as usize;
    AC3_SAMPLE_RATES.get(fscod).copied()
}

fn index_ac3(data: &[u8]) -> Result<Vec<FrameEntry>, String> {
    let mut frames = Vec::new();
    let mut pos = 0usize;

    while pos + 7 <= data.len() {
        // Syncword 0x0B77
        if data[pos] != 0x0B || data[pos + 1] != 0x77 {
            return Err(format!("invalid AC-3 syncword at byte {pos}"));
        }

        let Some(frame_size) = ac3_frame_size(&data[pos..]) else {
            return Err(format!("cannot determine frame size at byte {pos}"));
        };

        if pos + frame_size as usize > data.len() {
            // Truncated final frame — stop.
            break;
        }

        frames.push(FrameEntry {
            frame_size,
            flags: 0x01, // all AC-3 frames are independent (keyframe)
            nal_count: 0,
            duration_delta: 0,
            nal_lengths: vec![],
        });

        pos += frame_size as usize;
    }

    Ok(frames)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal AC-3 sync frame: header + zero-padded payload.
    fn make_ac3_frame(fscod: u8, frmsizecod: u8) -> Vec<u8> {
        let size = AC3_FRAME_SIZES[fscod as usize][frmsizecod as usize] as usize;
        let mut frame = vec![0u8; size];
        frame[0] = 0x0B;
        frame[1] = 0x77;
        frame[4] = (fscod << 6) | (frmsizecod & 0x3F);
        frame[5] = (8u8 << 3) | 0; // bsid=8, bsmod=0
        frame
    }

    #[test]
    fn single_frame_48khz() {
        // fscod=0 (48kHz), frmsizecod=0 → 256 bytes
        let frame = make_ac3_frame(0, 0);
        assert_eq!(frame.len(), 256);
        let frames = index_ac3(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_size, 256);
        assert!(frames[0].is_keyframe());
    }

    #[test]
    fn multiple_frames_parsed() {
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend(make_ac3_frame(0, 8)); // 512 bytes each
        }
        let frames = index_ac3(&data).unwrap();
        assert_eq!(frames.len(), 3);
        for f in &frames {
            assert_eq!(f.frame_size, 512);
        }
    }

    #[test]
    fn cbr_detected() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend(make_ac3_frame(0, 4)); // 384 bytes each
        }
        let frames = index_ac3(&data).unwrap();
        assert_eq!(detect_cbr(&frames), Some(384));
    }

    #[test]
    fn sample_rate_48khz() {
        let frame = make_ac3_frame(0, 0);
        assert_eq!(ac3_sample_rate(&frame), Some(48000));
    }

    #[test]
    fn sample_rate_44100hz() {
        let frame = make_ac3_frame(1, 0);
        assert_eq!(ac3_sample_rate(&frame), Some(44100));
    }

    #[test]
    fn sample_rate_32khz() {
        let frame = make_ac3_frame(2, 0);
        assert_eq!(ac3_sample_rate(&frame), Some(32000));
    }

    #[test]
    fn invalid_syncword_returns_error() {
        let data = vec![0xAA, 0xBB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(index_ac3(&data).is_err());
    }

    #[test]
    fn truncated_frame_ignored() {
        let mut combined = Vec::new();
        for _ in 0..2 {
            combined.extend(make_ac3_frame(0, 0)); // 256 bytes each
        }
        // Append a truncated frame (just the header bytes)
        let mut partial = make_ac3_frame(0, 0);
        partial.truncate(10);
        combined.extend(partial);
        let frames = index_ac3(&combined).unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn frame_duration_48khz() {
        let sample_rate = 48000u64;
        let duration = 1_536_000_000_000u64 / sample_rate;
        assert_eq!(duration, 32_000_000); // 32 ms
    }

    #[test]
    fn all_frmsizecod_entries_nonzero() {
        for fscod in 0..3 {
            for frmsizecod in 0..38 {
                assert!(
                    AC3_FRAME_SIZES[fscod][frmsizecod] > 0,
                    "zero size at fscod={fscod} frmsizecod={frmsizecod}"
                );
            }
        }
    }
}
