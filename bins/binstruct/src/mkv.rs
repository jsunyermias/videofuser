/// Streaming MKV/EBML scanner for binstruct gen.
///
/// Performs two-pass scanning using seek to avoid loading cluster payloads
/// (which can be tens of GBs in production MKVs).
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

// ─── MKV EBML element IDs ────────────────────────────────────────────────────

pub const ID_EBML_HEADER: u32 = 0x1A45DFA3;
pub const ID_SEGMENT: u32 = 0x18538067;
pub const ID_SEEK_HEAD: u32 = 0x114D9B74;
pub const ID_INFO: u32 = 0x1549A966;
pub const ID_TRACKS: u32 = 0x1654AE6B;
pub const ID_TRACK_ENTRY: u32 = 0x000000AE;
pub const ID_TRACK_NUMBER: u32 = 0x000000D7;
pub const ID_TRACK_TYPE: u32 = 0x00000083;
pub const ID_FLAG_DEFAULT: u32 = 0x00000088;
pub const ID_LANGUAGE: u32 = 0x0022B59C;
pub const ID_CODEC_ID: u32 = 0x00000086;
pub const ID_VIDEO: u32 = 0x000000E0;
pub const ID_PIXEL_WIDTH: u32 = 0x000000B0;
pub const ID_PIXEL_HEIGHT: u32 = 0x000000BA;
pub const ID_CLUSTER: u32 = 0x1F43B675;
pub const ID_TIMESTAMP: u32 = 0x000000E7;

// ─── Public result types ──────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ParsedMkv {
    /// Bytes from file start to just before Tracks element, with SeekHead excluded.
    pub pre_tracks_blob: Vec<u8>,
    /// TrackEntry elements (raw bytes + parsed metadata).
    pub track_entries: Vec<ParsedTrackEntry>,
    /// Bytes from end of Tracks element to start of first Cluster (may be empty).
    pub post_tracks_blob: Vec<u8>,
    /// Timestamps (ms) from each Cluster's Timestamp element, in order.
    pub cluster_timestamps: Vec<u64>,
}

#[derive(Debug)]
pub struct ParsedTrackEntry {
    pub track_id: u64,
    pub track_type: u8,
    pub flag_default: u8,
    pub language: String,
    pub codec_id: String,
    pub pixel_width: Option<u32>,
    pub pixel_height: Option<u32>,
    /// Verbatim bytes of the TrackEntry element (including its ID and size header).
    pub raw_bytes: Vec<u8>,
}

// ─── Element boundary info ────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ElementSpan {
    /// Absolute file offset of the first byte of this element (ID byte).
    start: u64,
    /// Absolute file offset of the byte PAST the last byte of this element.
    end: u64,
}

struct ScanResult {
    seekhead_span: Option<ElementSpan>,
    tracks_span: ElementSpan,
    first_cluster_start: u64,
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn parse_mkv(path: &Path) -> Result<ParsedMkv> {
    let mut file = File::open(path)
        .with_context(|| format!("opening MKV file: {}", path.display()))?;

    // Pass 1: scan element boundaries
    let scan = scan_boundaries(&mut file)
        .context("scanning MKV element boundaries")?;

    // Pass 2: collect raw bytes and parse track entries
    collect_data(&mut file, &scan)
        .context("collecting MKV data")
}

// ─── Pass 1: boundary scanning ───────────────────────────────────────────────

fn scan_boundaries(file: &mut File) -> Result<ScanResult> {
    file.seek(SeekFrom::Start(0))?;
    let mut r = BufReader::new(file);

    // Skip EBML header
    let (id, size) = read_element_header(&mut r)?;
    if id != ID_EBML_HEADER {
        bail!("expected EBML header (0x1A45DFA3), got 0x{:08X}", id);
    }
    skip_bytes(&mut r, size.unwrap_or(0))?;

    // Segment
    let (id, seg_size) = read_element_header(&mut r)?;
    if id != ID_SEGMENT {
        bail!("expected Segment element, got 0x{:08X}", id);
    }
    let seg_body_start = tell(&mut r)?;
    let seg_body_end = match seg_size {
        None => u64::MAX, // unknown size → parse until EOF
        Some(s) => seg_body_start + s,
    };

    let mut seekhead_span: Option<ElementSpan> = None;
    let mut tracks_span: Option<ElementSpan> = None;
    let mut first_cluster_start: Option<u64> = None;

    while tell(&mut r)? < seg_body_end {
        let elem_start = tell(&mut r)?;
        let (id, size) = match read_element_header(&mut r) {
            Ok(v) => v,
            Err(_) => break, // EOF or truncated
        };
        let size = match size {
            Some(s) => s,
            None => {
                // Unknown-size element at segment level (another Segment or Cluster)
                // For Cluster: scan its body for the timestamp, then stop.
                if id == ID_CLUSTER {
                    first_cluster_start = Some(elem_start);
                    break;
                }
                bail!("unexpected unknown-size element 0x{:08X} in Segment", id);
            }
        };
        let elem_end = tell(&mut r)? + size;

        match id {
            ID_SEEK_HEAD => {
                seekhead_span = Some(ElementSpan { start: elem_start, end: elem_end });
                skip_bytes(&mut r, size)?;
            }
            ID_TRACKS => {
                tracks_span = Some(ElementSpan { start: elem_start, end: elem_end });
                skip_bytes(&mut r, size)?;
            }
            ID_CLUSTER => {
                first_cluster_start = Some(elem_start);
                break;
            }
            _ => {
                skip_bytes(&mut r, size)?;
            }
        }
    }

    let tracks_span = tracks_span.ok_or_else(|| anyhow::anyhow!("no Tracks element found in MKV"))?;
    let first_cluster_start = first_cluster_start
        .ok_or_else(|| anyhow::anyhow!("no Cluster element found in MKV"))?;

    Ok(ScanResult { seekhead_span, tracks_span, first_cluster_start })
}

// ─── Pass 2: data collection ──────────────────────────────────────────────────

fn collect_data(file: &mut File, scan: &ScanResult) -> Result<ParsedMkv> {
    // 1. PreTracksBlob: [0, tracks_start), minus seekhead_span
    let pre_tracks_blob = {
        let mut blob = Vec::new();
        let tracks_start = scan.tracks_span.start;

        if let Some(ref sh) = scan.seekhead_span {
            // Bytes before SeekHead
            if sh.start > 0 {
                blob.extend(read_range(file, 0, sh.start)?);
            }
            // Bytes after SeekHead, before Tracks
            if sh.end < tracks_start {
                blob.extend(read_range(file, sh.end, tracks_start)?);
            }
        } else {
            blob = read_range(file, 0, tracks_start)?;
        }
        blob
    };

    // 2. Parse Tracks body
    let tracks_body = read_range(file, scan.tracks_span.start, scan.tracks_span.end)?;
    let track_entries = parse_tracks_body(&tracks_body)?;

    // 3. PostTracksBlob: [tracks_end, first_cluster_start)
    let post_tracks_blob = if scan.tracks_span.end < scan.first_cluster_start {
        read_range(file, scan.tracks_span.end, scan.first_cluster_start)?
    } else {
        Vec::new()
    };

    // 4. Cluster timestamps: scan all clusters
    let cluster_timestamps = collect_cluster_timestamps(file, scan.first_cluster_start)?;

    Ok(ParsedMkv {
        pre_tracks_blob,
        track_entries,
        post_tracks_blob,
        cluster_timestamps,
    })
}

// ─── Tracks body parsing ──────────────────────────────────────────────────────

fn parse_tracks_body(body: &[u8]) -> Result<Vec<ParsedTrackEntry>> {
    // body includes the Tracks element header (ID + size), then TrackEntry children
    let mut r = io::Cursor::new(body);
    // Read Tracks element header
    let (id, size) = read_element_header_cursor(&mut r)?;
    if id != ID_TRACKS {
        bail!("expected Tracks element, got 0x{:08X}", id);
    }
    let tracks_body_start = r.position();
    let tracks_body_end = tracks_body_start + size.unwrap_or(body.len() as u64);

    let mut entries = Vec::new();
    while r.position() < tracks_body_end {
        let entry_start = r.position() as usize;
        let (id, size) = read_element_header_cursor(&mut r)?;
        let size = size.unwrap_or(0);
        let payload_start = r.position() as usize;
        let payload_end = payload_start + size as usize;
        if payload_end > body.len() {
            break;
        }

        if id == ID_TRACK_ENTRY {
            let entry_raw = body[entry_start..payload_end].to_vec();
            let entry = parse_track_entry(&body[payload_start..payload_end], entry_raw)?;
            entries.push(entry);
        }
        r.set_position(payload_end as u64);
    }
    Ok(entries)
}

fn parse_track_entry(payload: &[u8], raw_bytes: Vec<u8>) -> Result<ParsedTrackEntry> {
    let mut r = io::Cursor::new(payload);
    let end = payload.len() as u64;
    let mut track_id: u64 = 0;
    let mut track_type: u8 = 0;
    let mut flag_default: u8 = 0;
    let mut language = String::new();
    let mut codec_id = String::new();
    let mut pixel_width: Option<u32> = None;
    let mut pixel_height: Option<u32> = None;

    while r.position() < end {
        let (id, size) = match read_element_header_cursor(&mut r) {
            Ok(v) => v,
            Err(_) => break,
        };
        let size = size.unwrap_or(0);
        let pos = r.position() as usize;
        let next = pos + size as usize;
        if next > payload.len() { break; }
        let elem_payload = &payload[pos..next];

        match id {
            ID_TRACK_NUMBER => { track_id = decode_uint(elem_payload); }
            ID_TRACK_TYPE => { track_type = elem_payload.first().copied().unwrap_or(0); }
            ID_FLAG_DEFAULT => { flag_default = elem_payload.first().copied().unwrap_or(0); }
            ID_LANGUAGE => {
                language = String::from_utf8(elem_payload.to_vec()).unwrap_or_default();
            }
            ID_CODEC_ID => {
                codec_id = String::from_utf8(elem_payload.to_vec()).unwrap_or_default();
            }
            ID_VIDEO => {
                let (pw, ph) = parse_video_element(elem_payload);
                pixel_width = pw;
                pixel_height = ph;
            }
            _ => {}
        }
        r.set_position(next as u64);
    }

    Ok(ParsedTrackEntry {
        track_id,
        track_type,
        flag_default,
        language,
        codec_id,
        pixel_width,
        pixel_height,
        raw_bytes,
    })
}

fn parse_video_element(data: &[u8]) -> (Option<u32>, Option<u32>) {
    let mut r = io::Cursor::new(data);
    let end = data.len() as u64;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    while r.position() < end {
        let (id, size) = match read_element_header_cursor(&mut r) {
            Ok(v) => v,
            Err(_) => break,
        };
        let size = size.unwrap_or(0);
        let pos = r.position() as usize;
        let next = pos + size as usize;
        if next > data.len() { break; }
        let payload = &data[pos..next];
        match id {
            ID_PIXEL_WIDTH => { width = Some(decode_uint(payload) as u32); }
            ID_PIXEL_HEIGHT => { height = Some(decode_uint(payload) as u32); }
            _ => {}
        }
        r.set_position(next as u64);
    }
    (width, height)
}

// ─── Cluster timestamp scanning ───────────────────────────────────────────────

fn collect_cluster_timestamps(file: &mut File, first_cluster_start: u64) -> Result<Vec<u64>> {
    file.seek(SeekFrom::Start(first_cluster_start))?;
    let mut r = BufReader::new(file);
    let mut timestamps = Vec::new();

    loop {
        let cluster_elem_start = tell(&mut r)?;
        let (id, size) = match read_element_header(&mut r) {
            Ok(v) => v,
            Err(_) => break,
        };
        if id != ID_CLUSTER {
            // Non-cluster element after clusters (e.g. Cues, Tags); done.
            break;
        }
        let cluster_body_size = match size {
            Some(s) => s,
            None => {
                // Unknown-size cluster: read until next Cluster ID or EOF
                let ts = scan_cluster_timestamp_unknown_size(&mut r)?;
                if let Some(t) = ts { timestamps.push(t); }
                // Re-scan for next cluster
                continue;
            }
        };
        let cluster_body_end = tell(&mut r)? + cluster_body_size;

        // Read just the Timestamp element (should be first in cluster)
        let mut found_ts = false;
        while tell(&mut r)? < cluster_body_end {
            let (child_id, child_size) = match read_element_header(&mut r) {
                Ok(v) => v,
                Err(_) => break,
            };
            let child_size = child_size.unwrap_or(0);
            if child_id == ID_TIMESTAMP {
                let mut buf = vec![0u8; child_size as usize];
                r.read_exact(&mut buf)?;
                timestamps.push(decode_uint(&buf));
                found_ts = true;
                break;
            } else {
                skip_bytes(&mut r, child_size)?;
            }
        }
        // Seek to end of cluster body
        r.seek(SeekFrom::Start(cluster_body_end))?;
        let _ = found_ts;
    }

    Ok(timestamps)
}

fn scan_cluster_timestamp_unknown_size(r: &mut BufReader<&mut File>) -> Result<Option<u64>> {
    // Scan children until we find Timestamp or hit next cluster boundary
    loop {
        let (id, size) = match read_element_header(r) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let size = size.unwrap_or(0);
        if id == ID_TIMESTAMP {
            let mut buf = vec![0u8; size as usize];
            r.read_exact(&mut buf)?;
            return Ok(Some(decode_uint(&buf)));
        }
        // If we hit another large 4-byte ID it's likely a new element; stop
        if id & 0xFF000000 != 0 {
            // Back up and return
            return Ok(None);
        }
        skip_bytes(r, size)?;
    }
}

// ─── File I/O utilities ───────────────────────────────────────────────────────

fn read_range(file: &mut File, start: u64, end: u64) -> Result<Vec<u8>> {
    if end <= start {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn tell<R: Seek>(r: &mut R) -> io::Result<u64> {
    r.seek(SeekFrom::Current(0))
}

fn skip_bytes<R: Read>(r: &mut R, n: u64) -> io::Result<()> {
    io::copy(&mut r.take(n), &mut io::sink())?;
    Ok(())
}

// ─── EBML ID and size readers ─────────────────────────────────────────────────

/// Read an EBML element ID (1–4 bytes) and its VINT size. Returns (id, Option<size>).
/// None size means "unknown" (all data bits set).
pub fn read_element_header<R: Read>(r: &mut R) -> Result<(u32, Option<u64>)> {
    let id = read_ebml_id(r)?;
    let size = read_ebml_vint_size(r)?;
    Ok((id, size))
}

fn read_element_header_cursor(r: &mut io::Cursor<&[u8]>) -> Result<(u32, Option<u64>)> {
    let id = read_ebml_id_cursor(r)?;
    let size = read_ebml_vint_size_cursor(r)?;
    Ok((id, size))
}

fn read_ebml_id<R: Read>(r: &mut R) -> Result<u32> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let b0 = first[0];
    let n = if b0 >= 0x80 { 1 }
            else if b0 >= 0x40 { 2 }
            else if b0 >= 0x20 { 3 }
            else if b0 >= 0x10 { 4 }
            else { bail!("invalid EBML ID byte: 0x{:02X}", b0) };
    let mut id = b0 as u32;
    let mut rest = vec![0u8; n - 1];
    r.read_exact(&mut rest)?;
    for b in rest {
        id = (id << 8) | b as u32;
    }
    Ok(id)
}

fn read_ebml_id_cursor(r: &mut io::Cursor<&[u8]>) -> Result<u32> {
    let mut b0 = [0u8; 1];
    if r.read(&mut b0)? == 0 {
        bail!("EOF reading EBML ID");
    }
    let b0 = b0[0];
    let n = if b0 >= 0x80 { 1 }
            else if b0 >= 0x40 { 2 }
            else if b0 >= 0x20 { 3 }
            else if b0 >= 0x10 { 4 }
            else { bail!("invalid EBML ID byte: 0x{:02X}", b0) };
    let mut id = b0 as u32;
    for _ in 1..n {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        id = (id << 8) | b[0] as u32;
    }
    Ok(id)
}

fn read_ebml_vint_size<R: Read>(r: &mut R) -> Result<Option<u64>> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let b0 = first[0];
    let n = b0.leading_zeros() as usize + 1;
    if n > 8 {
        bail!("invalid EBML VINT size byte: 0x{:02X}", b0);
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (b0 & !mask) as u64;
    let mut rest = vec![0u8; n - 1];
    r.read_exact(&mut rest)?;
    for b in &rest {
        value = (value << 8) | *b as u64;
    }
    // Unknown size: all data bits set
    let max_data = (1u64 << (7 * n)) - 1;
    if value == max_data {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn read_ebml_vint_size_cursor(r: &mut io::Cursor<&[u8]>) -> Result<Option<u64>> {
    let mut b0 = [0u8; 1];
    if r.read(&mut b0)? == 0 {
        bail!("EOF reading VINT size");
    }
    let b0 = b0[0];
    let n = b0.leading_zeros() as usize + 1;
    if n > 8 {
        bail!("invalid EBML VINT size byte");
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (b0 & !mask) as u64;
    for _ in 1..n {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        value = (value << 8) | b[0] as u64;
    }
    let max_data = (1u64 << (7 * n)) - 1;
    if value == max_data { Ok(None) } else { Ok(Some(value)) }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

pub fn decode_uint(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in bytes {
        v = (v << 8) | b as u64;
    }
    v
}
