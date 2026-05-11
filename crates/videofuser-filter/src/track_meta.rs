use videofuser_binstruct::BinstructFile;

/// Extracted view of a track for use by the filter; does not depend on EBML.
#[derive(Clone, Debug)]
pub struct TrackMeta {
    pub track_id: u32,
    pub kind: TrackKind,
    /// ISO 639; "und" if unknown.
    pub language: String,
    /// CodecID verbatim from TrackEntry EBML, e.g. "A_EAC3".
    pub codec_id: String,
    /// Normalised codec name, e.g. "eac3", "h264", "aac".
    pub codec_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrackKind {
    Video,
    Audio,
}

/// Extracts TrackMeta from a BinstructFile by parsing the CodecID (EBML ID 0x86)
/// and Language (EBML ID 0x22B59C) from each TrackEntry blob.
pub fn extract_track_metas(binstruct: &BinstructFile) -> Vec<TrackMeta> {
    binstruct
        .mkv_skeleton
        .track_entries
        .iter()
        .map(|entry| {
            let (codec_id, language) = parse_track_entry_blob(&entry.bytes);
            let codec_name = normalise_codec(&codec_id);
            let kind = if codec_id.to_uppercase().starts_with("V_") {
                TrackKind::Video
            } else {
                TrackKind::Audio
            };
            TrackMeta {
                track_id: entry.track_id as u32,
                kind,
                language,
                codec_name,
                codec_id,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Codec normalisation
// ---------------------------------------------------------------------------

fn normalise_codec(codec_id: &str) -> String {
    match codec_id.to_uppercase().as_str() {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "h265",
        "V_AV1" => "av1",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_TRUEHD" => "truehd",
        "A_DTS" | "A_DTS/MA" => "dts",
        "A_AAC" | "A_AAC/MPEG2/LC" | "A_AAC/MPEG4/LC" => "aac",
        "A_MPEG/L3" => "mp3",
        other if other.starts_with("V_") => "video_unknown",
        other if other.starts_with("A_") => "audio_unknown",
        _ => "unknown",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Micro EBML parser for MKV TrackEntry blobs
// ---------------------------------------------------------------------------
// MKV EBML element IDs of interest:
//   CodecID  : 0x000086 (1-byte ID)
//   Language : 0x22B59C (3-byte ID)

const EBML_ID_CODEC_ID: u32 = 0x86;
const EBML_ID_LANGUAGE: u32 = 0x22B59C;

fn parse_track_entry_blob(blob: &[u8]) -> (String, String) {
    let mut codec_id = String::new();
    let mut language = String::new();
    scan_ebml_elements(blob, &mut codec_id, &mut language);
    if language.is_empty() {
        language = "und".to_string();
    }
    (codec_id, language)
}

/// Recursively scans EBML elements for CodecID and Language fields.
fn scan_ebml_elements(data: &[u8], codec_id: &mut String, language: &mut String) {
    let mut pos = 0;
    while pos < data.len() {
        let Some(id) = read_mkv_id(data, &mut pos) else { break };
        let Some(size) = read_mkv_size(data, &mut pos) else { break };
        if pos + size > data.len() {
            break;
        }
        let payload = &data[pos..pos + size];
        pos += size;

        match id {
            EBML_ID_CODEC_ID => {
                *codec_id = String::from_utf8_lossy(payload).into_owned();
            }
            EBML_ID_LANGUAGE => {
                *language = String::from_utf8_lossy(payload).into_owned();
            }
            _ => {
                // Recurse into any master element that might contain our targets.
                scan_ebml_elements(payload, codec_id, language);
            }
        }
    }
}

/// Reads an EBML element ID (marker bits are kept, per EBML spec).
/// Supports IDs up to 4 bytes wide.
fn read_mkv_id(data: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos >= data.len() {
        return None;
    }
    let first = data[*pos];
    let width = if first & 0x80 != 0 {
        1
    } else if first & 0x40 != 0 {
        2
    } else if first & 0x20 != 0 {
        3
    } else if first & 0x10 != 0 {
        4
    } else {
        return None;
    };
    if *pos + width > data.len() {
        return None;
    }
    let mut id = 0u32;
    for i in 0..width {
        id = (id << 8) | data[*pos + i] as u32;
    }
    *pos += width;
    Some(id)
}

/// Reads an EBML size VINT (marker bit is cleared, per EBML spec).
fn read_mkv_size(data: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos >= data.len() {
        return None;
    }
    let first = data[*pos];
    let n = (first.leading_zeros() as usize) + 1;
    if n > 8 || *pos + n > data.len() {
        return None;
    }
    let mask = 0x80u8 >> (n - 1);
    let mut size = (first & !mask) as usize;
    for i in 1..n {
        size = (size << 8) | data[*pos + i] as usize;
    }
    *pos += n;
    Some(size)
}
