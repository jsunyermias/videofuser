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
#[command(name = "parser-aac", about = "AAC ADTS bitstream indexer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index an AAC ADTS raw file and produce a VFR on stdout.
    Index {
        /// Variant field (2-digit string), passed through to metadata.
        #[arg(long)]
        variant: String,

        /// Version field (2-digit string), passed through to metadata.
        #[arg(long = "version")]
        ver: String,

        /// AAC profile variant: lc, he, hev2.
        #[arg(long, default_value = "lc")]
        profile: String,

        /// Language code (ISO 639, stored in CBR JSON output).
        #[arg(long, default_value = "")]
        language: String,

        /// Input raw AAC file.
        input: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { input, language, .. } => {
            let data = fs::read(&input).unwrap_or_else(|e| {
                eprintln!("parser-aac: cannot read {}: {}", input.display(), e);
                std::process::exit(2);
            });

            match index_adts(&data) {
                Ok(frames) => {
                    if frames.is_empty() {
                        eprintln!("parser-aac: no ADTS frames found");
                        std::process::exit(3);
                    }

                    // AAC frame duration: 1024 samples at standard rates.
                    // Detect sample rate from first frame header for ns calculation.
                    let sample_rate = adts_sample_rate(&data).unwrap_or(48000);
                    let frame_duration_ns = 1_024_000_000_000u64 / sample_rate as u64;

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
                            eprintln!("parser-aac: write error: {e}");
                            std::process::exit(2);
                        });
                    }
                }
                Err(e) => {
                    eprintln!("parser-aac: parse error: {e}");
                    std::process::exit(3);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ADTS parser
// ---------------------------------------------------------------------------

fn index_adts(data: &[u8]) -> Result<Vec<FrameEntry>, String> {
    let mut frames = Vec::new();
    let mut pos = 0usize;

    while pos + 7 <= data.len() {
        // Syncword: 12 bits = 0xFFF
        if data[pos] != 0xFF || (data[pos + 1] & 0xF0) != 0xF0 {
            return Err(format!("invalid ADTS syncword at byte {pos}"));
        }

        let protection_absent = (data[pos + 1] & 0x01) != 0;
        let header_len = if protection_absent { 7 } else { 9 };

        if pos + header_len > data.len() {
            break;
        }

        // aac_frame_length (13 bits): bits 30-42 of the header
        // byte[3] bits 1-0, byte[4] bits 7-0, byte[5] bits 7-5
        let frame_length = (((data[pos + 3] & 0x03) as u32) << 11)
            | ((data[pos + 4] as u32) << 3)
            | ((data[pos + 5] as u32) >> 5);

        if frame_length < header_len as u32 || pos + frame_length as usize > data.len() {
            // Truncated or invalid final frame — stop without error.
            break;
        }

        frames.push(FrameEntry {
            frame_size: frame_length,
            flags: 0x01, // all AAC frames are keyframes
            nal_count: 0,
            duration_delta: 0,
            nal_lengths: vec![],
        });

        pos += frame_length as usize;
    }

    Ok(frames)
}

/// Extract sample rate from the first ADTS frame header (sampling_frequency_index).
fn adts_sample_rate(data: &[u8]) -> Option<u32> {
    if data.len() < 4 {
        return None;
    }
    let sfi = (data[2] >> 2) & 0x0F; // bits 18-21 (4 bits)
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    RATES.get(sfi as usize).copied()
}

// ---------------------------------------------------------------------------
// Unit tests (synthetic bitstreams)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_adts_frame(frame_length: u16, protection_absent: bool, sample_rate_idx: u8) -> Vec<u8> {
        // Build a minimal ADTS header + zero payload.
        // byte[0] = 0xFF
        // byte[1] = 0xF1 (protection_absent=1) or 0xF0 (protection_absent=0)
        // byte[2] = profile(2) | sfi(4) | private(1) | chan_msb(1)
        // byte[3] = chan_lsb(2) | orig(2) | fl_top2(2)... wait let me be more careful

        let mut h = [0u8; 7];
        h[0] = 0xFF;
        h[1] = 0xF0 | if protection_absent { 0x01 } else { 0x00 };
        // profile=01 (LC), sfi=sample_rate_idx, private=0, channel_config=2 (stereo)
        h[2] = (0b01 << 6) | ((sample_rate_idx & 0x0F) << 2) | 0x00; // channel_config MSB=0
        // channel_config LSB = 010 = 2 (stereo), aac_frame_length top 2 bits
        let fl = frame_length as u32;
        h[3] = (0b010 << 5) | 0x00 | (((fl >> 11) & 0x03) as u8);
        h[4] = ((fl >> 3) & 0xFF) as u8;
        h[5] = (((fl & 0x07) as u8) << 5) | 0x1F; // buffer_fullness (11 bits) = 0x7FF (VBR)
        h[6] = 0xFC; // buffer_fullness low + number_of_raw_data_blocks=0

        let mut frame = h.to_vec();
        frame.resize(frame_length as usize, 0x00); // zero payload
        frame
    }

    #[test]
    fn single_frame_parsed() {
        // Build one valid ADTS frame of 128 bytes at 48kHz (sfi=3)
        let frame = make_adts_frame(128, true, 3);
        let frames = index_adts(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_size, 128);
        assert!(frames[0].is_keyframe());
        assert_eq!(frames[0].nal_count, 0);
    }

    #[test]
    fn multiple_frames_parsed() {
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend(make_adts_frame(200, true, 3));
        }
        let frames = index_adts(&data).unwrap();
        assert_eq!(frames.len(), 5);
        for f in &frames {
            assert_eq!(f.frame_size, 200);
        }
    }

    #[test]
    fn cbr_detected_for_equal_frames() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend(make_adts_frame(300, true, 3));
        }
        let frames = index_adts(&data).unwrap();
        assert_eq!(detect_cbr(&frames), Some(300));
    }

    #[test]
    fn vbr_detected_for_variable_frames() {
        let mut data = Vec::new();
        data.extend(make_adts_frame(200, true, 3));
        data.extend(make_adts_frame(300, true, 3));
        data.extend(make_adts_frame(200, true, 3));
        let frames = index_adts(&data).unwrap();
        assert!(detect_cbr(&frames).is_none());
    }

    #[test]
    fn sample_rate_extracted() {
        let frame = make_adts_frame(128, true, 3); // sfi=3 → 48000
        assert_eq!(adts_sample_rate(&frame), Some(48000));
    }

    #[test]
    fn invalid_syncword_returns_error() {
        let data = vec![0x00u8; 10];
        assert!(index_adts(&data).is_err());
    }

    #[test]
    fn truncated_data_returns_partial() {
        // Two complete frames + partial third
        let mut data = Vec::new();
        data.extend(make_adts_frame(100, true, 3));
        data.extend(make_adts_frame(100, true, 3));
        data.extend(&[0xFF, 0xF1, 0x50, 0x80, 0x00, 0x00, 0xFC]); // header only, no payload
        let frames = index_adts(&data).unwrap();
        // The last incomplete frame is silently dropped
        assert_eq!(frames.len(), 2);
    }
}
