/// videofuser-manifest: parsing and serialization of the four manifest formats.
///
/// EBML element ID schema (2-byte IDs, first byte in 0x40–0x6F per spec §9.1):
use std::io::{self, Cursor, Read, Write};
use thiserror::Error;

// ─── Public types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub title: String,
    pub year: u16,
    pub original_mkv_filename: String,
    pub original_mkv_hash: [u8; 32],
    pub original_language: String,
    pub system_version: String,
    pub publisher: String,
    pub tracks: Vec<ManifestTrack>,
    pub variants_legend: Vec<VariantEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestTrack {
    pub track_id: u64,
    pub kind: TrackKind,
    pub language: Option<String>,
    pub codec: String,
    pub variant: Option<String>,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantEntry {
    pub id: String,
    pub description: String,
}

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid EBML encoding: {0}")]
    Ebml(&'static str),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// ─── EBML element ID constants (2-byte, range 0x40–0x6F for first byte) ─────

/// Root manifest container.
pub const EBML_ID_MANIFEST_FILE: u16 = 0x4040;
/// UTF-8 string: content title.
pub const EBML_ID_TITLE: u16 = 0x4041;
/// u16: production year.
pub const EBML_ID_YEAR: u16 = 0x4042;
/// UTF-8 string: original MKV filename.
pub const EBML_ID_ORIGINAL_MKV_FILENAME: u16 = 0x4043;
/// binary (32 bytes): SHA-256 of original MKV.
pub const EBML_ID_ORIGINAL_MKV_HASH: u16 = 0x4044;
/// UTF-8 string: ISO 639 language code of original audio.
pub const EBML_ID_ORIGINAL_LANGUAGE: u16 = 0x4045;
/// UTF-8 string: videofuser system version.
pub const EBML_ID_SYSTEM_VERSION: u16 = 0x4046;
/// UTF-8 string: publisher name.
pub const EBML_ID_PUBLISHER: u16 = 0x4047;
/// Master: contains TrackEntry elements.
pub const EBML_ID_TRACKS: u16 = 0x4048;
/// Master: a single track entry.
pub const EBML_ID_TRACK_ENTRY: u16 = 0x4049;
/// u64: track identifier.
pub const EBML_ID_TRACK_ID: u16 = 0x404A;
/// u8: 0=video, 1=audio, 2=subtitle.
pub const EBML_ID_TRACK_KIND: u16 = 0x404B;
/// UTF-8 string: optional ISO 639 language.
pub const EBML_ID_TRACK_LANGUAGE: u16 = 0x404C;
/// UTF-8 string: codec identifier.
pub const EBML_ID_TRACK_CODEC: u16 = 0x404D;
/// UTF-8 string: optional variant code.
pub const EBML_ID_TRACK_VARIANT: u16 = 0x404E;
/// UTF-8 string: optional resolution (video only).
pub const EBML_ID_TRACK_RESOLUTION: u16 = 0x404F;
/// Master: contains VariantEntry elements.
pub const EBML_ID_VARIANTS_LEGEND: u16 = 0x4050;
/// Master: a single variant entry.
pub const EBML_ID_VARIANT_ENTRY: u16 = 0x4051;
/// UTF-8 string: variant two-digit id.
pub const EBML_ID_VARIANT_ID: u16 = 0x4052;
/// UTF-8 string: human-readable variant description.
pub const EBML_ID_VARIANT_DESCRIPTION: u16 = 0x4053;

// ─── Low-level EBML helpers (2-byte IDs) ─────────────────────────────────────

fn write_vint_size(w: &mut impl Write, value: u64) -> io::Result<()> {
    let n = match value {
        0..=0x7F => 1,
        0..=0x3FFF => 2,
        0..=0x1FFFFF => 3,
        0..=0x0FFFFFFF => 4,
        0..=0x07FFFFFFFF => 5,
        0..=0x03FFFFFFFFFF => 6,
        0..=0x01FFFFFFFFFFFF => 7,
        _ => 8,
    };
    let marker = 0x80u8 >> (n - 1);
    let be = value.to_be_bytes();
    let mut out = [0u8; 8];
    out[..n].copy_from_slice(&be[8 - n..]);
    out[0] |= marker;
    w.write_all(&out[..n])
}

fn write_element2(w: &mut impl Write, id: u16, payload: &[u8]) -> io::Result<()> {
    w.write_all(&id.to_be_bytes())?;
    write_vint_size(w, payload.len() as u64)?;
    w.write_all(payload)
}

fn write_master2(w: &mut impl Write, id: u16, children: &[u8]) -> io::Result<()> {
    write_element2(w, id, children)
}

fn encode_uint(value: u64) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let be = value.to_be_bytes();
    let skip = be.iter().position(|&b| b != 0).unwrap_or(7);
    be[skip..].to_vec()
}

// ─── EBML deserialization helpers ─────────────────────────────────────────────

fn read_byte_m(r: &mut Cursor<&[u8]>) -> Result<u8, ManifestError> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).map_err(|_| ManifestError::Ebml("unexpected EOF"))?;
    Ok(b[0])
}

fn read_bytes_m(r: &mut Cursor<&[u8]>, n: u64) -> Result<Vec<u8>, ManifestError> {
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf).map_err(|_| ManifestError::Ebml("unexpected EOF reading payload"))?;
    Ok(buf)
}

fn read_vint_size_m(r: &mut Cursor<&[u8]>) -> Result<u64, ManifestError> {
    let first = read_byte_m(r)?;
    let n = first.leading_zeros() as usize + 1;
    if n > 8 {
        return Err(ManifestError::Ebml("invalid VINT size byte"));
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (first & !mask) as u64;
    for _ in 1..n {
        value = (value << 8) | read_byte_m(r)? as u64;
    }
    Ok(value)
}

/// Read a 2-byte EBML element ID and payload size. Returns (id, payload_size).
fn read_element_header2(r: &mut Cursor<&[u8]>) -> Result<(u16, u64), ManifestError> {
    let b0 = read_byte_m(r)?;
    // 2-byte IDs have first byte in 0x40-0x7F
    if b0 < 0x40 || b0 >= 0x80 {
        return Err(ManifestError::Ebml("expected 2-byte EBML ID (first byte 0x40-0x7F)"));
    }
    let b1 = read_byte_m(r)?;
    let id = ((b0 as u16) << 8) | b1 as u16;
    let size = read_vint_size_m(r)?;
    Ok((id, size))
}

fn decode_uint_bytes(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in bytes {
        v = (v << 8) | b as u64;
    }
    v
}

fn bytes_to_str(bytes: Vec<u8>) -> Result<String, ManifestError> {
    String::from_utf8(bytes).map_err(|_| ManifestError::Ebml("invalid UTF-8 string"))
}

// ─── serialize_ebml ───────────────────────────────────────────────────────────

pub fn serialize_ebml(m: &Manifest) -> Result<Vec<u8>, ManifestError> {
    let mut body = Vec::new();

    write_element2(&mut body, EBML_ID_TITLE, m.title.as_bytes())?;
    write_element2(&mut body, EBML_ID_YEAR, &encode_uint(m.year as u64))?;
    write_element2(&mut body, EBML_ID_ORIGINAL_MKV_FILENAME, m.original_mkv_filename.as_bytes())?;
    write_element2(&mut body, EBML_ID_ORIGINAL_MKV_HASH, &m.original_mkv_hash)?;
    write_element2(&mut body, EBML_ID_ORIGINAL_LANGUAGE, m.original_language.as_bytes())?;
    write_element2(&mut body, EBML_ID_SYSTEM_VERSION, m.system_version.as_bytes())?;
    write_element2(&mut body, EBML_ID_PUBLISHER, m.publisher.as_bytes())?;

    // Tracks
    {
        let mut tracks_buf = Vec::new();
        for track in &m.tracks {
            let mut te = Vec::new();
            write_element2(&mut te, EBML_ID_TRACK_ID, &encode_uint(track.track_id))?;
            write_element2(&mut te, EBML_ID_TRACK_KIND, &[track.kind.as_u8()])?;
            if let Some(ref lang) = track.language {
                write_element2(&mut te, EBML_ID_TRACK_LANGUAGE, lang.as_bytes())?;
            }
            write_element2(&mut te, EBML_ID_TRACK_CODEC, track.codec.as_bytes())?;
            if let Some(ref v) = track.variant {
                write_element2(&mut te, EBML_ID_TRACK_VARIANT, v.as_bytes())?;
            }
            if let Some(ref res) = track.resolution {
                write_element2(&mut te, EBML_ID_TRACK_RESOLUTION, res.as_bytes())?;
            }
            write_master2(&mut tracks_buf, EBML_ID_TRACK_ENTRY, &te)?;
        }
        write_master2(&mut body, EBML_ID_TRACKS, &tracks_buf)?;
    }

    // VariantsLegend
    {
        let mut vl = Vec::new();
        for ve in &m.variants_legend {
            let mut entry = Vec::new();
            write_element2(&mut entry, EBML_ID_VARIANT_ID, ve.id.as_bytes())?;
            write_element2(&mut entry, EBML_ID_VARIANT_DESCRIPTION, ve.description.as_bytes())?;
            write_master2(&mut vl, EBML_ID_VARIANT_ENTRY, &entry)?;
        }
        write_master2(&mut body, EBML_ID_VARIANTS_LEGEND, &vl)?;
    }

    let mut out = Vec::new();
    write_master2(&mut out, EBML_ID_MANIFEST_FILE, &body)?;
    Ok(out)
}

// ─── parse_ebml ───────────────────────────────────────────────────────────────

pub fn parse_ebml(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    let mut r = Cursor::new(bytes);
    let (root_id, root_size) = read_element_header2(&mut r)?;
    if root_id != EBML_ID_MANIFEST_FILE {
        return Err(ManifestError::Ebml("expected ManifestFile root element"));
    }
    let body_bytes = read_bytes_m(&mut r, root_size)?;
    decode_manifest_body(&body_bytes)
}

fn decode_manifest_body(data: &[u8]) -> Result<Manifest, ManifestError> {
    let mut r = Cursor::new(data);
    let mut title: Option<String> = None;
    let mut year: u16 = 0;
    let mut original_mkv_filename = String::new();
    let mut original_mkv_hash: Option<[u8; 32]> = None;
    let mut original_language = String::new();
    let mut system_version = String::new();
    let mut publisher = String::new();
    let mut tracks = Vec::new();
    let mut variants_legend = Vec::new();

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header2(&mut r)?;
        let payload = read_bytes_m(&mut r, size)?;
        match id {
            EBML_ID_TITLE => { title = Some(bytes_to_str(payload)?); }
            EBML_ID_YEAR => { year = decode_uint_bytes(&payload) as u16; }
            EBML_ID_ORIGINAL_MKV_FILENAME => { original_mkv_filename = bytes_to_str(payload)?; }
            EBML_ID_ORIGINAL_MKV_HASH => {
                if payload.len() == 32 {
                    original_mkv_hash = Some(payload.try_into().unwrap());
                } else {
                    return Err(ManifestError::Ebml("OriginalMkvHash must be 32 bytes"));
                }
            }
            EBML_ID_ORIGINAL_LANGUAGE => { original_language = bytes_to_str(payload)?; }
            EBML_ID_SYSTEM_VERSION => { system_version = bytes_to_str(payload)?; }
            EBML_ID_PUBLISHER => { publisher = bytes_to_str(payload)?; }
            EBML_ID_TRACKS => { tracks = decode_tracks(&payload)?; }
            EBML_ID_VARIANTS_LEGEND => { variants_legend = decode_variants_legend(&payload)?; }
            _ => {}
        }
    }

    Ok(Manifest {
        title: title.ok_or(ManifestError::MissingField("title"))?,
        year,
        original_mkv_filename,
        original_mkv_hash: original_mkv_hash.ok_or(ManifestError::MissingField("original_mkv_hash"))?,
        original_language,
        system_version,
        publisher,
        tracks,
        variants_legend,
    })
}

fn decode_tracks(data: &[u8]) -> Result<Vec<ManifestTrack>, ManifestError> {
    let mut r = Cursor::new(data);
    let mut tracks = Vec::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header2(&mut r)?;
        let payload = read_bytes_m(&mut r, size)?;
        if id == EBML_ID_TRACK_ENTRY {
            tracks.push(decode_track_entry(&payload)?);
        }
    }
    Ok(tracks)
}

fn decode_track_entry(data: &[u8]) -> Result<ManifestTrack, ManifestError> {
    let mut r = Cursor::new(data);
    let mut track_id: u64 = 0;
    let mut kind = TrackKind::Video;
    let mut language: Option<String> = None;
    let mut codec = String::new();
    let mut variant: Option<String> = None;
    let mut resolution: Option<String> = None;

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header2(&mut r)?;
        let payload = read_bytes_m(&mut r, size)?;
        match id {
            EBML_ID_TRACK_ID => { track_id = decode_uint_bytes(&payload); }
            EBML_ID_TRACK_KIND => {
                kind = match payload.first().copied().unwrap_or(0) {
                    1 => TrackKind::Audio,
                    2 => TrackKind::Subtitle,
                    _ => TrackKind::Video,
                };
            }
            EBML_ID_TRACK_LANGUAGE => { language = Some(bytes_to_str(payload)?); }
            EBML_ID_TRACK_CODEC => { codec = bytes_to_str(payload)?; }
            EBML_ID_TRACK_VARIANT => { variant = Some(bytes_to_str(payload)?); }
            EBML_ID_TRACK_RESOLUTION => { resolution = Some(bytes_to_str(payload)?); }
            _ => {}
        }
    }
    Ok(ManifestTrack { track_id, kind, language, codec, variant, resolution })
}

fn decode_variants_legend(data: &[u8]) -> Result<Vec<VariantEntry>, ManifestError> {
    let mut r = Cursor::new(data);
    let mut entries = Vec::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header2(&mut r)?;
        let payload = read_bytes_m(&mut r, size)?;
        if id == EBML_ID_VARIANT_ENTRY {
            entries.push(decode_variant_entry(&payload)?);
        }
    }
    Ok(entries)
}

fn decode_variant_entry(data: &[u8]) -> Result<VariantEntry, ManifestError> {
    let mut r = Cursor::new(data);
    let mut id = String::new();
    let mut description = String::new();
    while (r.position() as usize) < data.len() {
        let (eid, size) = read_element_header2(&mut r)?;
        let payload = read_bytes_m(&mut r, size)?;
        match eid {
            EBML_ID_VARIANT_ID => { id = bytes_to_str(payload)?; }
            EBML_ID_VARIANT_DESCRIPTION => { description = bytes_to_str(payload)?; }
            _ => {}
        }
    }
    Ok(VariantEntry { id, description })
}

// ─── parse_xml (best-effort, videofuser-manifest XML format) ─────────────────

pub fn parse_xml(text: &str) -> Result<Manifest, ManifestError> {
    let title = xml_text(text, "title").unwrap_or_default();
    let year = xml_text(text, "year")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let original_mkv_filename = xml_text(text, "original-mkv-filename")
        .or_else(|| xml_text(text, "original_mkv_filename"))
        .unwrap_or_default();
    let hash_hex = xml_text(text, "hash")
        .or_else(|| xml_text(text, "original-mkv-hash"))
        .unwrap_or_default();
    let original_mkv_hash = parse_hex32(&hash_hex).unwrap_or([0u8; 32]);
    let original_language = xml_attr_first(text, "original-language")
        .or_else(|| xml_text(text, "original-language"))
        .or_else(|| xml_text(text, "original_language"))
        .unwrap_or_default();
    let system_version = xml_attr_first(text, "system-version")
        .or_else(|| xml_text(text, "system-version"))
        .unwrap_or_default();
    let publisher = xml_text(text, "publisher").unwrap_or_default();

    let tracks = parse_xml_tracks(text);
    let variants_legend = parse_xml_variants(text);

    Ok(Manifest {
        title,
        year,
        original_mkv_filename,
        original_mkv_hash,
        original_language,
        system_version,
        publisher,
        tracks,
        variants_legend,
    })
}

fn parse_xml_tracks(text: &str) -> Vec<ManifestTrack> {
    let mut tracks = Vec::new();
    let mut search = text;
    while let Some(start) = search.find("<track ") {
        let rest = &search[start..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..=end];
        let track_id = xml_attr_in(tag, "id")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let kind = match xml_attr_in(tag, "type").as_deref() {
            Some("audio") => TrackKind::Audio,
            Some("subtitle") => TrackKind::Subtitle,
            _ => TrackKind::Video,
        };
        let language = xml_attr_in(tag, "language");
        let codec = xml_attr_in(tag, "codec").unwrap_or_default();
        let variant = xml_attr_in(tag, "variant");
        let resolution = xml_attr_in(tag, "resolution");
        tracks.push(ManifestTrack { track_id, kind, language, codec, variant, resolution });
        search = &search[start + 7..];
    }
    tracks
}

fn parse_xml_variants(text: &str) -> Vec<VariantEntry> {
    let mut entries = Vec::new();
    let mut search = text;
    while let Some(start) = search.find("<variant ") {
        let rest = &search[start..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..=end];
        let id = xml_attr_in(tag, "id").unwrap_or_default();
        let description = xml_attr_in(tag, "description").unwrap_or_default();
        entries.push(VariantEntry { id, description });
        search = &search[start + 9..];
    }
    entries
}

// ─── parse_nfo (best-effort, Kodi-compatible NFO/XML) ────────────────────────

pub fn parse_nfo(text: &str) -> Result<Manifest, ManifestError> {
    let title = xml_text(text, "title").unwrap_or_default();
    let year = xml_text(text, "year")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    // NFO doesn't carry most videofuser-specific fields; best-effort defaults
    Ok(Manifest {
        title,
        year,
        original_mkv_filename: String::new(),
        original_mkv_hash: [0u8; 32],
        original_language: String::new(),
        system_version: String::new(),
        publisher: String::new(),
        tracks: Vec::new(),
        variants_legend: Vec::new(),
    })
}

// ─── parse_md (best-effort, Markdown) ────────────────────────────────────────

pub fn parse_md(text: &str) -> Result<Manifest, ManifestError> {
    // Extract title from first H1 line: "# Title (Year)" or "# Title"
    let mut title = String::new();
    let mut year: u16 = 0;
    let mut original_language = String::new();
    let mut original_mkv_hash = [0u8; 32];
    let mut publisher = String::new();
    let mut system_version = String::new();
    let mut original_mkv_filename = String::new();

    for line in text.lines() {
        let line = line.trim();
        if title.is_empty() {
            if let Some(h1) = line.strip_prefix("# ") {
                // "Title (Year)" pattern
                if let (Some(lp), Some(rp)) = (h1.rfind('('), h1.rfind(')')) {
                    if lp < rp {
                        let yr = &h1[lp + 1..rp];
                        if let Ok(y) = yr.parse::<u16>() {
                            year = y;
                            title = h1[..lp].trim().to_string();
                            continue;
                        }
                    }
                }
                title = h1.to_string();
                continue;
            }
        }
        // Key-value lines: "- **Key**: value" or "**Key**: value"
        let kv_line = line.trim_start_matches('-').trim();
        if let Some(kv) = kv_line.strip_prefix("**") {
            if let Some(colon) = kv.find("**:") {
                let key = &kv[..colon];
                let val = kv[colon + 3..].trim().to_string();
                match key.to_ascii_lowercase().as_str() {
                    "año" | "year" | "año de producción" => {
                        if let Ok(y) = val.parse::<u16>() { year = y; }
                    }
                    "idioma original" | "original language" => { original_language = val; }
                    "hash del mkv original" | "original mkv hash" | "hash" => {
                        original_mkv_hash = parse_hex32(&val).unwrap_or([0u8; 32]);
                    }
                    "publicador" | "publisher" => { publisher = val; }
                    "system version" | "versión del sistema" => { system_version = val; }
                    "mkv original" | "original mkv filename" => { original_mkv_filename = val; }
                    _ => {}
                }
            }
        }
    }

    Ok(Manifest {
        title,
        year,
        original_mkv_filename,
        original_mkv_hash,
        original_language,
        system_version,
        publisher,
        tracks: Vec::new(),
        variants_legend: Vec::new(),
    })
}

// ─── XML string utilities (best-effort, no external XML dep) ─────────────────

/// Extract the text content of a simple XML tag: <tag>content</tag>
fn xml_text(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
}

/// Extract the value of the first occurrence of an attribute in text (loose search).
fn xml_attr_first(text: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = text.find(&needle)? + needle.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

/// Extract attribute from a single tag string.
fn xml_attr_in(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 { return None; }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ─── TrackKind helper ─────────────────────────────────────────────────────────

impl TrackKind {
    pub fn as_u8(&self) -> u8 {
        match self {
            TrackKind::Video => 0,
            TrackKind::Audio => 1,
            TrackKind::Subtitle => 2,
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> Manifest {
        Manifest {
            title: "Película Maravillosa".to_string(),
            year: 1995,
            original_mkv_filename: "pelicula.mkv".to_string(),
            original_mkv_hash: [0xAB; 32],
            original_language: "spa".to_string(),
            system_version: "1.0".to_string(),
            publisher: "Test Publisher".to_string(),
            tracks: vec![
                ManifestTrack {
                    track_id: 1,
                    kind: TrackKind::Video,
                    language: None,
                    codec: "V_MPEG4/ISO/AVC".to_string(),
                    variant: Some("00".to_string()),
                    resolution: Some("1080p".to_string()),
                },
                ManifestTrack {
                    track_id: 100,
                    kind: TrackKind::Audio,
                    language: Some("spa".to_string()),
                    codec: "A_AC3".to_string(),
                    variant: Some("00".to_string()),
                    resolution: None,
                },
            ],
            variants_legend: vec![
                VariantEntry { id: "00".to_string(), description: "Doblaje principal".to_string() },
            ],
        }
    }

    #[test]
    fn ebml_round_trip() {
        let m = sample_manifest();
        let bytes = serialize_ebml(&m).unwrap();
        let decoded = parse_ebml(&bytes).unwrap();
        assert_eq!(decoded.title, m.title);
        assert_eq!(decoded.year, m.year);
        assert_eq!(decoded.original_mkv_hash, m.original_mkv_hash);
        assert_eq!(decoded.original_language, m.original_language);
        assert_eq!(decoded.tracks.len(), 2);
        assert_eq!(decoded.tracks[0].codec, "V_MPEG4/ISO/AVC");
        assert_eq!(decoded.tracks[1].language, Some("spa".to_string()));
        assert_eq!(decoded.variants_legend.len(), 1);
        assert_eq!(decoded.variants_legend[0].id, "00");
    }

    #[test]
    fn parse_xml_basic() {
        let xml = r#"<videofuser-manifest version="1">
  <title>Película Maravillosa</title>
  <year>1995</year>
  <hash>abababababababababababababababababababababababababababababababababab</hash>
  <publisher>Test Publisher</publisher>
  <original-language>spa</original-language>
  <tracks>
    <track id="1" type="video" codec="h264" resolution="1080p" variant="00" />
    <track id="100" type="audio" language="spa" codec="eac3" variant="00" />
  </tracks>
  <variants>
    <variant id="00" description="Doblaje principal" />
  </variants>
</videofuser-manifest>"#;
        let m = parse_xml(xml).unwrap();
        assert_eq!(m.title, "Película Maravillosa");
        assert_eq!(m.year, 1995);
        assert_eq!(m.tracks.len(), 2);
        assert!(matches!(m.tracks[0].kind, TrackKind::Video));
        assert!(matches!(m.tracks[1].kind, TrackKind::Audio));
        assert_eq!(m.variants_legend.len(), 1);
    }

    #[test]
    fn parse_nfo_basic() {
        let nfo = r#"<movie>
  <title>Película Maravillosa</title>
  <year>1995</year>
  <plot>Una película increíble.</plot>
</movie>"#;
        let m = parse_nfo(nfo).unwrap();
        assert_eq!(m.title, "Película Maravillosa");
        assert_eq!(m.year, 1995);
    }

    #[test]
    fn parse_md_basic() {
        let md = r#"# Película Maravillosa (1995)

## Información general
- **Año**: 1995
- **Idioma original**: spa
- **Publicador**: Test Publisher
- **Hash del MKV original**: abababababababababababababababababababababababababababababababababab
"#;
        let m = parse_md(md).unwrap();
        assert_eq!(m.title, "Película Maravillosa");
        assert_eq!(m.year, 1995);
        assert_eq!(m.original_language, "spa");
        assert_eq!(m.publisher, "Test Publisher");
    }
}
