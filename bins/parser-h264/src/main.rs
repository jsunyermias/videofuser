use std::{
    fs,
    io::{self},
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use videofuser_parser_common::{detect_cbr, write_vfr, FrameEntry};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "parser-h264", about = "H.264/AVC Annex B bitstream indexer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index an H.264 Annex B raw file and produce a VFR on stdout.
    Index {
        #[arg(long)]
        variant: String,

        #[arg(long = "version")]
        ver: String,

        /// H.264 profile (baseline, main, high).
        #[arg(long, default_value = "high")]
        profile: String,

        /// H.264 level (e.g. 40, 41, 51).
        #[arg(long, default_value = "40")]
        level: String,

        /// Language code (unused for video but kept for CLI contract uniformity).
        #[arg(long, default_value = "")]
        language: String,

        /// Input raw H.264 Annex B file.
        input: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { input, .. } => {
            let data = fs::read(&input).unwrap_or_else(|e| {
                eprintln!("parser-h264: cannot read {}: {}", input.display(), e);
                std::process::exit(2);
            });

            match index_h264(&data) {
                Ok(frames) => {
                    if frames.is_empty() {
                        eprintln!("parser-h264: no H.264 frames found");
                        std::process::exit(3);
                    }

                    // Video is always emitted as VBR (spec §10.4).
                    let _cbr = detect_cbr(&frames);

                    let stdout = io::stdout();
                    let mut out = stdout.lock();
                    write_vfr(&frames, true, &mut out).unwrap_or_else(|e| {
                        eprintln!("parser-h264: write error: {e}");
                        std::process::exit(2);
                    });
                }
                Err(e) => {
                    eprintln!("parser-h264: parse error: {e}");
                    std::process::exit(3);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NAL unit types relevant to access unit boundary detection
// ---------------------------------------------------------------------------

const NAL_NON_IDR_SLICE: u8 = 1;
const NAL_IDR_SLICE: u8 = 5;
pub const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9; // Access Unit Delimiter

// ---------------------------------------------------------------------------
// Annex B start code scanner
// ---------------------------------------------------------------------------

/// Locate all Annex B start codes in `data`.
/// Returns `(sc_offset, nal_data_offset)` pairs, where `sc_offset` is the
/// position of the first zero byte of the start code and `nal_data_offset`
/// is the position of the first byte of the NAL unit data (after the start code).
fn find_start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if i + 4 <= data.len() && data[i + 2] == 0x00 && data[i + 3] == 0x01 {
                positions.push((i, i + 4)); // 4-byte start code
                i += 4;
                continue;
            } else if data[i + 2] == 0x01 {
                positions.push((i, i + 3)); // 3-byte start code
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    positions
}

// ---------------------------------------------------------------------------
// H.264 Annex B indexer
// ---------------------------------------------------------------------------

pub fn index_h264(data: &[u8]) -> Result<Vec<FrameEntry>, String> {
    if data.is_empty() {
        return Ok(vec![]);
    }

    let scs = find_start_codes(data);
    if scs.is_empty() {
        return Err("no Annex B start codes found".to_string());
    }

    // Build NAL descriptor list.
    struct Nal {
        sc_offset: usize,
        data_offset: usize,
        nal_type: u8,
        end: usize,
    }

    let mut nals: Vec<Nal> = Vec::with_capacity(scs.len());
    for (idx, &(sc_off, data_off)) in scs.iter().enumerate() {
        if data_off >= data.len() {
            continue;
        }
        let nal_type = data[data_off] & 0x1F;
        let end = if idx + 1 < scs.len() { scs[idx + 1].0 } else { data.len() };
        nals.push(Nal { sc_offset: sc_off, data_offset: data_off, nal_type, end });
    }

    if nals.is_empty() {
        return Err("no valid NAL units".to_string());
    }

    // Group NAL units into access units (frames).
    //
    // A new AU starts when:
    //   1. We encounter an AUD (type 9), OR
    //   2. We encounter an SPS (type 7) and the current AU already has a slice, OR
    //   3. We encounter a slice (type 1 or 5) and the current AU already has a slice.
    //
    // This correctly handles the common patterns:
    //   - Streams with AUD markers (most encoders with -bf > 0)
    //   - Streams without AUD (some real-time encoders)
    //   - SPS/PPS inline repeats per GOP

    struct AccessUnit {
        first_nal_idx: usize,
        last_nal_idx: usize,
        has_idr: bool,
        nal_indices: Vec<usize>,
    }

    let mut aus: Vec<AccessUnit> = Vec::new();
    let mut current: Option<AccessUnit> = None;
    let mut seen_slice = false;

    for (idx, nal) in nals.iter().enumerate() {
        let is_slice = nal.nal_type == NAL_NON_IDR_SLICE || nal.nal_type == NAL_IDR_SLICE;
        let is_aud = nal.nal_type == NAL_AUD;
        let is_sps = nal.nal_type == NAL_SPS;

        let start_new = is_aud
            || (is_slice && seen_slice)
            || (is_sps && seen_slice);

        if start_new {
            if let Some(au) = current.take() {
                aus.push(au);
            }
            seen_slice = false;
        }

        if current.is_none() {
            current = Some(AccessUnit {
                first_nal_idx: idx,
                last_nal_idx: idx,
                has_idr: false,
                nal_indices: Vec::new(),
            });
        }

        let au = current.as_mut().unwrap();
        au.last_nal_idx = idx;
        au.nal_indices.push(idx);
        if nal.nal_type == NAL_IDR_SLICE {
            au.has_idr = true;
        }
        if is_slice {
            seen_slice = true;
        }
    }

    if let Some(au) = current.take() {
        aus.push(au);
    }

    // Convert AUs to FrameEntry, skipping header-only AUs (no slice).
    let mut frames: Vec<FrameEntry> = Vec::with_capacity(aus.len());

    for au in &aus {
        let has_slice = au.nal_indices.iter().any(|&i| {
            nals[i].nal_type == NAL_NON_IDR_SLICE || nals[i].nal_type == NAL_IDR_SLICE
        });
        if !has_slice {
            continue;
        }

        // Collect NAL payload lengths (excluding start code), skipping AUD.
        let mut nal_lengths: Vec<u64> = Vec::new();
        for &i in &au.nal_indices {
            let nal = &nals[i];
            if nal.nal_type == NAL_AUD {
                continue;
            }
            let payload_len = (nal.end - nal.data_offset) as u64;
            nal_lengths.push(payload_len);
            if nal_lengths.len() == 255 {
                break; // nal_count saturated to u8::MAX
            }
        }

        let au_sc_start = nals[au.first_nal_idx].sc_offset;
        let au_end = nals[au.last_nal_idx].end;
        let frame_size = (au_end - au_sc_start) as u32;
        let nal_count = nal_lengths.len() as u8;
        let is_keyframe = au.has_idr;
        // bit0=keyframe, bit1=has_nal_lengths (always true for video)
        let flags = if is_keyframe { 0x03 } else { 0x02 };

        frames.push(FrameEntry {
            frame_size,
            flags,
            nal_count,
            duration_delta: 0,
            nal_lengths,
        });
    }

    if frames.is_empty() {
        return Err("no slice NAL units found after header filtering".to_string());
    }

    Ok(frames)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SC4: &[u8] = &[0x00, 0x00, 0x00, 0x01];
    const SC3: &[u8] = &[0x00, 0x00, 0x01];

    fn nal4(nal_type: u8, payload_size: usize) -> Vec<u8> {
        let mut v = SC4.to_vec();
        v.push(nal_type & 0x1F);
        v.extend(vec![0xAA; payload_size]);
        v
    }

    fn nal3(nal_type: u8, payload_size: usize) -> Vec<u8> {
        let mut v = SC3.to_vec();
        v.push(nal_type & 0x1F);
        v.extend(vec![0xBB; payload_size]);
        v
    }

    #[test]
    fn single_idr_frame() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_SPS, 20));
        data.extend(nal4(NAL_PPS, 5));
        data.extend(nal4(NAL_IDR_SLICE, 100));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_keyframe());
        assert!(frames[0].nal_count >= 1);
    }

    #[test]
    fn idr_followed_by_p_frames() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_IDR_SLICE, 200));
        data.extend(nal4(NAL_NON_IDR_SLICE, 80));
        data.extend(nal4(NAL_NON_IDR_SLICE, 70));
        data.extend(nal4(NAL_NON_IDR_SLICE, 60));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames.len(), 4);
        assert!(frames[0].is_keyframe());
        assert!(!frames[1].is_keyframe());
        assert!(!frames[2].is_keyframe());
        assert!(!frames[3].is_keyframe());
    }

    #[test]
    fn aud_separates_frames() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_AUD, 1));
        data.extend(nal4(NAL_IDR_SLICE, 100));
        data.extend(nal4(NAL_AUD, 1));
        data.extend(nal4(NAL_NON_IDR_SLICE, 50));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_keyframe());
        assert!(!frames[1].is_keyframe());
    }

    #[test]
    fn nal_lengths_recorded_correctly() {
        let mut data = Vec::new();
        // SPS: 20 bytes payload → nal_len = 21 (1 nal_type byte + 20 payload)
        data.extend(nal4(NAL_SPS, 20));
        // IDR: 100 bytes payload → nal_len = 101
        data.extend(nal4(NAL_IDR_SLICE, 100));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames.len(), 1);
        // SPS (not AUD, so counted) + IDR = 2 NALs
        assert_eq!(frames[0].nal_count, 2);
        assert_eq!(frames[0].nal_lengths.len(), 2);
        assert_eq!(frames[0].nal_lengths[0], 21); // SPS
        assert_eq!(frames[0].nal_lengths[1], 101); // IDR
    }

    #[test]
    fn three_byte_start_codes_accepted() {
        let mut data = Vec::new();
        data.extend(nal3(NAL_IDR_SLICE, 50));
        data.extend(nal3(NAL_NON_IDR_SLICE, 40));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn sei_included_in_frame() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_SEI, 10));
        data.extend(nal4(NAL_IDR_SLICE, 200));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_keyframe());
        assert_eq!(frames[0].nal_count, 2); // SEI + IDR
    }

    #[test]
    fn frame_size_spans_full_au() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_IDR_SLICE, 100));
        let frames = index_h264(&data).unwrap();
        assert_eq!(frames[0].frame_size as usize, data.len());
    }

    #[test]
    fn empty_input_returns_empty() {
        let frames = index_h264(&[]).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn no_slice_nal_returns_error_or_empty() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_SPS, 20));
        data.extend(nal4(NAL_PPS, 5));
        let result = index_h264(&data);
        assert!(result.is_err() || result.unwrap().is_empty());
    }

    #[test]
    fn find_start_codes_4byte() {
        let data = [0x00u8, 0x00, 0x00, 0x01, 0x67, 0x00, 0x00, 0x00, 0x01, 0x68];
        let scs = find_start_codes(&data);
        assert_eq!(scs.len(), 2);
        assert_eq!(scs[0], (0, 4));
        assert_eq!(scs[1], (5, 9));
    }

    #[test]
    fn find_start_codes_3byte() {
        let data = [0x00u8, 0x00, 0x01, 0x65, 0x00, 0x00, 0x01, 0x41];
        let scs = find_start_codes(&data);
        assert_eq!(scs.len(), 2);
        assert_eq!(scs[0], (0, 3));
        assert_eq!(scs[1], (4, 7));
    }

    #[test]
    fn aud_not_counted_in_nal_lengths() {
        let mut data = Vec::new();
        data.extend(nal4(NAL_AUD, 1));
        data.extend(nal4(NAL_IDR_SLICE, 80));
        let frames = index_h264(&data).unwrap();
        // AUD skipped → only IDR counted
        assert_eq!(frames[0].nal_count, 1);
        assert_eq!(frames[0].nal_lengths.len(), 1);
    }
}
