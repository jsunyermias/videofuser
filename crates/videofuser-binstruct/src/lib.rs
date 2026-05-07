use std::io::{self, Cursor, Read, Write};

use thiserror::Error;

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BinstructError {
    #[error("invalid magic: expected 'VFUS', got {0:?}")]
    InvalidMagic([u8; 4]),
    #[error("unsupported version: {0} (expected 1)")]
    UnsupportedVersion(u64),
    #[error("missing required element: {0}")]
    MissingElement(&'static str),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("zstd error: {0}")]
    Zstd(String),
    #[error("invalid EBML encoding: {0}")]
    Ebml(&'static str),
}

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinstructFile {
    pub header: Header,
    pub source: Source,
    pub mkv_skeleton: MkvSkeleton,
    pub cluster_timestamps: ClusterTimestamps,
    pub track_policies: Vec<TrackPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: u64,
    /// bit 0: compressed with zstd
    pub config_flags: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub original_mkv_hash: [u8; 32],
    pub publisher_info: String,
    pub creation_timestamp: u64,
    pub original_language: String,
    pub original_default_track_id: u64,
    pub build_tools: Vec<ToolEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEntry {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MkvSkeleton {
    pub pre_tracks_blob: Vec<u8>,
    pub track_entries: Vec<TrackEntryRecord>,
    pub post_tracks_blob: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEntryRecord {
    pub track_id: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterTimestamps {
    pub cluster_count: u64,
    /// Signed deltas between consecutive cluster timestamps.
    pub deltas: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecType {
    Video = 0,
    Audio = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPolicy {
    pub track_id: u64,
    pub codec_type: CodecType,
    pub language_code: String,
    pub frame_count: u64,
    pub is_vbr: bool,
    pub frame_duration: u64,
    pub cbr_frame_size: Option<u64>,
    pub raw_file_hash: [u8; 32],
    pub vfr_file_hash: Option<[u8; 32]>,
}

// ---------------------------------------------------------------------------
// EBML element IDs (cap. 7.3)
// ---------------------------------------------------------------------------

const ID_BINSTRUCT_FILE:          u8 = 0x80;
const ID_HEADER:                  u8 = 0x81;
const ID_MAGIC:                   u8 = 0x82;
const ID_VERSION:                 u8 = 0x83;
const ID_CONFIG_FLAGS:            u8 = 0x84;
const ID_SOURCE:                  u8 = 0x85;
const ID_ORIGINAL_MKV_HASH:       u8 = 0x86;
const ID_PUBLISHER_INFO:          u8 = 0x87;
const ID_CREATION_TIMESTAMP:      u8 = 0x88;
const ID_ORIGINAL_LANGUAGE:       u8 = 0x89;
const ID_ORIGINAL_DEFAULT_TRACK:  u8 = 0x8A;
const ID_MKV_SKELETON:            u8 = 0x8B;
const ID_PRE_TRACKS_BLOB:         u8 = 0x8C;
const ID_TRACK_ENTRIES:           u8 = 0x8D;
const ID_TRACK_ENTRY_RECORD:      u8 = 0x8E;
const ID_TRACK_ENTRY_ID:          u8 = 0x8F;
const ID_TRACK_ENTRY_BYTES:       u8 = 0x90;
const ID_POST_TRACKS_BLOB:        u8 = 0x91;
const ID_CLUSTER_TIMESTAMPS:      u8 = 0x92;
const ID_CLUSTER_COUNT:           u8 = 0x93;
const ID_CLUSTER_TIMESTAMP_DELTAS:u8 = 0x94;
const ID_TRACK_POLICIES:          u8 = 0x95;
const ID_TRACK_POLICY:            u8 = 0x96;
const ID_TRACK_ID:                u8 = 0x97;
const ID_CODEC_TYPE:              u8 = 0x98;
const ID_LANGUAGE_CODE:           u8 = 0x99;
const ID_FRAME_COUNT:             u8 = 0x9A;
const ID_IS_VBR:                  u8 = 0x9B;
const ID_FRAME_DURATION:          u8 = 0x9C;
const ID_CBR_FRAME_SIZE:          u8 = 0x9D;
const ID_RAW_FILE_HASH:           u8 = 0x9E;
const ID_VFR_FILE_HASH:           u8 = 0x9F;
const ID_BUILD_TOOLS:             u8 = 0xA0;
const ID_TOOL_ENTRY:              u8 = 0xA1;
const ID_TOOL_NAME:               u8 = 0xA2;
const ID_TOOL_VERSION:            u8 = 0xA3;

const MAGIC_BYTES: &[u8; 4] = b"VFUS";

// ---------------------------------------------------------------------------
// Low-level EBML helpers
// ---------------------------------------------------------------------------

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

fn read_vint_size(r: &mut Cursor<&[u8]>) -> Result<u64, BinstructError> {
    let first = read_byte(r)?;
    let n = first.leading_zeros() as usize + 1;
    if n > 8 {
        return Err(BinstructError::Ebml("invalid VINT size byte"));
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (first & !mask) as u64;
    for _ in 1..n {
        value = (value << 8) | read_byte(r)? as u64;
    }
    Ok(value)
}

fn read_byte(r: &mut Cursor<&[u8]>) -> Result<u8, BinstructError> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).map_err(BinstructError::Io)?;
    Ok(b[0])
}

/// Write an EBML element: 1-byte ID + VINT size + payload.
fn write_element(w: &mut impl Write, id: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&[id])?;
    write_vint_size(w, payload.len() as u64)?;
    w.write_all(payload)
}

/// Write a master element wrapping pre-encoded children.
fn write_master(w: &mut impl Write, id: u8, children: &[u8]) -> io::Result<()> {
    write_element(w, id, children)
}

/// Read the next element header (id byte + vint size), return (id, payload_len).
fn read_element_header(r: &mut Cursor<&[u8]>) -> Result<(u8, u64), BinstructError> {
    let id = read_byte(r)?;
    let size = read_vint_size(r)?;
    Ok((id, size))
}

/// Read exactly `size` bytes from cursor.
fn read_bytes(r: &mut Cursor<&[u8]>, size: u64) -> Result<Vec<u8>, BinstructError> {
    let mut buf = vec![0u8; size as usize];
    r.read_exact(&mut buf).map_err(BinstructError::Io)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// UInt / string / binary encoders
// ---------------------------------------------------------------------------

fn encode_uint(value: u64) -> Vec<u8> {
    // Minimal big-endian encoding
    if value == 0 {
        return vec![0];
    }
    let be = value.to_be_bytes();
    let skip = be.iter().position(|&b| b != 0).unwrap_or(7);
    be[skip..].to_vec()
}

fn decode_uint(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in bytes {
        v = (v << 8) | b as u64;
    }
    v
}

// ---------------------------------------------------------------------------
// Signed delta varint encoding for ClusterTimestampDeltas
// EBML signed VINT: stored = actual + bias, bias = 2^(7n-1) - 1
// ---------------------------------------------------------------------------

fn signed_vint_byte_len(value: i64) -> usize {
    // bias for n bytes = 2^(7n-1) - 1
    // we need: 0 <= value + bias <= 2^(7n) - 2  (full range minus reserved all-ones)
    for n in 1usize..=8 {
        let bits = 7 * n;
        let bias = (1i64 << (bits - 1)) - 1;
        let stored = value + bias;
        let max = (1i64 << bits) - 2;
        if stored >= 0 && stored <= max {
            return n;
        }
    }
    8
}

fn write_signed_vint(w: &mut impl Write, value: i64) -> io::Result<()> {
    let n = signed_vint_byte_len(value);
    let bits = 7 * n;
    let bias = (1i64 << (bits - 1)) - 1;
    let stored = (value + bias) as u64;
    let marker = 0x80u8 >> (n - 1);
    let be = stored.to_be_bytes();
    let mut out = [0u8; 8];
    out[..n].copy_from_slice(&be[8 - n..]);
    out[0] |= marker;
    w.write_all(&out[..n])
}

fn read_signed_vint(r: &mut Cursor<&[u8]>) -> Result<i64, BinstructError> {
    let first = read_byte(r)?;
    let n = first.leading_zeros() as usize + 1;
    if n > 8 {
        return Err(BinstructError::Ebml("invalid signed VINT"));
    }
    let mask = 0x80u8 >> (n - 1);
    let mut stored = (first & !mask) as u64;
    for _ in 1..n {
        stored = (stored << 8) | read_byte(r)? as u64;
    }
    let bits = 7 * n;
    let bias = (1i64 << (bits - 1)) - 1;
    Ok(stored as i64 - bias)
}

// ---------------------------------------------------------------------------
// BinstructFile serialization
// ---------------------------------------------------------------------------

impl BinstructFile {
    /// Serialize to EBML bytes, optionally compressing with zstd (if config_flags bit 0 set).
    pub fn serialize(&self) -> Result<Vec<u8>, BinstructError> {
        let ebml = self.encode_ebml()?;
        if self.header.config_flags & 0x01 != 0 {
            zstd::encode_all(ebml.as_slice(), 9).map_err(|e| BinstructError::Zstd(e.to_string()))
        } else {
            Ok(ebml)
        }
    }

    /// Deserialize from bytes (auto-detects zstd via config_flags in Header element).
    pub fn deserialize(bytes: &[u8]) -> Result<Self, BinstructError> {
        // We need to peek at the Header's ConfigFlags to decide if we must decompress.
        // Strategy: parse the outer wrapper uncompressed to find ConfigFlags, then
        // decompress the body if needed. But since the whole file is compressed (not
        // just the body), we try to detect via zstd magic bytes.
        let raw: Vec<u8>;
        let data: &[u8];
        if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            // zstd magic
            raw = zstd::decode_all(bytes).map_err(|e| BinstructError::Zstd(e.to_string()))?;
            data = &raw;
        } else {
            data = bytes;
        }
        Self::decode_ebml(data)
    }

    // -----------------------------------------------------------------------

    fn encode_ebml(&self) -> Result<Vec<u8>, BinstructError> {
        let mut body = Vec::new();

        // Header
        {
            let mut h = Vec::new();
            write_element(&mut h, ID_MAGIC, MAGIC_BYTES)?;
            write_element(&mut h, ID_VERSION, &encode_uint(self.header.version))?;
            write_element(&mut h, ID_CONFIG_FLAGS, &[self.header.config_flags])?;
            write_master(&mut body, ID_HEADER, &h)?;
        }

        // Source
        {
            let mut s = Vec::new();
            write_element(&mut s, ID_ORIGINAL_MKV_HASH, &self.source.original_mkv_hash)?;
            write_element(&mut s, ID_PUBLISHER_INFO, self.source.publisher_info.as_bytes())?;
            write_element(&mut s, ID_CREATION_TIMESTAMP, &encode_uint(self.source.creation_timestamp))?;
            write_element(&mut s, ID_ORIGINAL_LANGUAGE, self.source.original_language.as_bytes())?;
            write_element(&mut s, ID_ORIGINAL_DEFAULT_TRACK, &encode_uint(self.source.original_default_track_id))?;
            // BuildTools
            {
                let mut bt = Vec::new();
                for tool in &self.source.build_tools {
                    let mut te = Vec::new();
                    write_element(&mut te, ID_TOOL_NAME, tool.name.as_bytes())?;
                    write_element(&mut te, ID_TOOL_VERSION, tool.version.as_bytes())?;
                    write_master(&mut bt, ID_TOOL_ENTRY, &te)?;
                }
                write_master(&mut s, ID_BUILD_TOOLS, &bt)?;
            }
            write_master(&mut body, ID_SOURCE, &s)?;
        }

        // MkvSkeleton
        {
            let mut sk = Vec::new();
            write_element(&mut sk, ID_PRE_TRACKS_BLOB, &self.mkv_skeleton.pre_tracks_blob)?;
            // TrackEntries
            {
                let mut te_block = Vec::new();
                for entry in &self.mkv_skeleton.track_entries {
                    let mut ter = Vec::new();
                    write_element(&mut ter, ID_TRACK_ENTRY_ID, &encode_uint(entry.track_id))?;
                    write_element(&mut ter, ID_TRACK_ENTRY_BYTES, &entry.bytes)?;
                    write_master(&mut te_block, ID_TRACK_ENTRY_RECORD, &ter)?;
                }
                write_master(&mut sk, ID_TRACK_ENTRIES, &te_block)?;
            }
            write_element(&mut sk, ID_POST_TRACKS_BLOB, &self.mkv_skeleton.post_tracks_blob)?;
            write_master(&mut body, ID_MKV_SKELETON, &sk)?;
        }

        // ClusterTimestamps
        {
            let mut ct = Vec::new();
            write_element(&mut ct, ID_CLUSTER_COUNT, &encode_uint(self.cluster_timestamps.cluster_count))?;
            // Encode deltas as packed signed VINTs
            let mut delta_bytes = Vec::new();
            for &d in &self.cluster_timestamps.deltas {
                write_signed_vint(&mut delta_bytes, d)?;
            }
            write_element(&mut ct, ID_CLUSTER_TIMESTAMP_DELTAS, &delta_bytes)?;
            write_master(&mut body, ID_CLUSTER_TIMESTAMPS, &ct)?;
        }

        // TrackPolicies
        {
            let mut tp_block = Vec::new();
            for policy in &self.track_policies {
                let mut tp = Vec::new();
                write_element(&mut tp, ID_TRACK_ID, &encode_uint(policy.track_id))?;
                write_element(&mut tp, ID_CODEC_TYPE, &[policy.codec_type.as_u8()])?;
                write_element(&mut tp, ID_LANGUAGE_CODE, policy.language_code.as_bytes())?;
                write_element(&mut tp, ID_FRAME_COUNT, &encode_uint(policy.frame_count))?;
                write_element(&mut tp, ID_IS_VBR, &[policy.is_vbr as u8])?;
                write_element(&mut tp, ID_FRAME_DURATION, &encode_uint(policy.frame_duration))?;
                if let Some(cbr) = policy.cbr_frame_size {
                    write_element(&mut tp, ID_CBR_FRAME_SIZE, &encode_uint(cbr))?;
                }
                write_element(&mut tp, ID_RAW_FILE_HASH, &policy.raw_file_hash)?;
                if let Some(ref h) = policy.vfr_file_hash {
                    write_element(&mut tp, ID_VFR_FILE_HASH, h)?;
                }
                write_master(&mut tp_block, ID_TRACK_POLICY, &tp)?;
            }
            write_master(&mut body, ID_TRACK_POLICIES, &tp_block)?;
        }

        // Wrap in BinstructFile master
        let mut out = Vec::new();
        write_master(&mut out, ID_BINSTRUCT_FILE, &body)?;
        Ok(out)
    }

    fn decode_ebml(data: &[u8]) -> Result<Self, BinstructError> {
        let mut r = Cursor::new(data);

        // Outer BinstructFile master
        let (id, size) = read_element_header(&mut r)?;
        if id != ID_BINSTRUCT_FILE {
            return Err(BinstructError::Ebml("expected BinstructFile element"));
        }
        let body_bytes = read_bytes(&mut r, size)?;
        let mut body = Cursor::new(body_bytes.as_slice());

        let mut header: Option<Header> = None;
        let mut source: Option<Source> = None;
        let mut mkv_skeleton: Option<MkvSkeleton> = None;
        let mut cluster_timestamps: Option<ClusterTimestamps> = None;
        let mut track_policies: Vec<TrackPolicy> = Vec::new();

        while (body.position() as usize) < body_bytes.len() {
            let (child_id, child_size) = read_element_header(&mut body)?;
            let child_bytes = read_bytes(&mut body, child_size)?;
            match child_id {
                ID_HEADER => { header = Some(decode_header(&child_bytes)?); }
                ID_SOURCE => { source = Some(decode_source(&child_bytes)?); }
                ID_MKV_SKELETON => { mkv_skeleton = Some(decode_mkv_skeleton(&child_bytes)?); }
                ID_CLUSTER_TIMESTAMPS => { cluster_timestamps = Some(decode_cluster_timestamps(&child_bytes)?); }
                ID_TRACK_POLICIES => { track_policies = decode_track_policies(&child_bytes)?; }
                _ => {} // ignore unknown
            }
        }

        Ok(BinstructFile {
            header: header.ok_or(BinstructError::MissingElement("Header"))?,
            source: source.ok_or(BinstructError::MissingElement("Source"))?,
            mkv_skeleton: mkv_skeleton.ok_or(BinstructError::MissingElement("MkvSkeleton"))?,
            cluster_timestamps: cluster_timestamps.ok_or(BinstructError::MissingElement("ClusterTimestamps"))?,
            track_policies,
        })
    }
}

// ---------------------------------------------------------------------------
// Decoders for each section
// ---------------------------------------------------------------------------

fn decode_header(data: &[u8]) -> Result<Header, BinstructError> {
    let mut r = Cursor::new(data);
    let mut magic: Option<[u8; 4]> = None;
    let mut version: Option<u64> = None;
    let mut config_flags: u8 = 0;

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_MAGIC => {
                if payload.len() != 4 {
                    return Err(BinstructError::Ebml("Magic must be 4 bytes"));
                }
                let m: [u8; 4] = payload.try_into().unwrap();
                if &m != MAGIC_BYTES {
                    return Err(BinstructError::InvalidMagic(m));
                }
                magic = Some(m);
            }
            ID_VERSION => {
                let v = decode_uint(&payload);
                if v != 1 {
                    return Err(BinstructError::UnsupportedVersion(v));
                }
                version = Some(v);
            }
            ID_CONFIG_FLAGS => {
                config_flags = *payload.first().ok_or(BinstructError::Ebml("ConfigFlags empty"))?;
            }
            _ => {}
        }
    }

    magic.ok_or(BinstructError::MissingElement("Magic"))?;
    Ok(Header {
        version: version.ok_or(BinstructError::MissingElement("Version"))?,
        config_flags,
    })
}

fn decode_source(data: &[u8]) -> Result<Source, BinstructError> {
    let mut r = Cursor::new(data);
    let mut original_mkv_hash: Option<[u8; 32]> = None;
    let mut publisher_info = String::new();
    let mut creation_timestamp: u64 = 0;
    let mut original_language = String::new();
    let mut original_default_track_id: u64 = 0;
    let mut build_tools: Vec<ToolEntry> = Vec::new();

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_ORIGINAL_MKV_HASH => {
                if payload.len() != 32 {
                    return Err(BinstructError::Ebml("OriginalMkvHash must be 32 bytes"));
                }
                original_mkv_hash = Some(payload.try_into().unwrap());
            }
            ID_PUBLISHER_INFO => {
                publisher_info = String::from_utf8(payload)
                    .map_err(|_| BinstructError::Ebml("PublisherInfo not valid UTF-8"))?;
            }
            ID_CREATION_TIMESTAMP => { creation_timestamp = decode_uint(&payload); }
            ID_ORIGINAL_LANGUAGE => {
                original_language = String::from_utf8(payload)
                    .map_err(|_| BinstructError::Ebml("OriginalLanguage not valid UTF-8"))?;
            }
            ID_ORIGINAL_DEFAULT_TRACK => { original_default_track_id = decode_uint(&payload); }
            ID_BUILD_TOOLS => { build_tools = decode_build_tools(&payload)?; }
            _ => {}
        }
    }

    Ok(Source {
        original_mkv_hash: original_mkv_hash.ok_or(BinstructError::MissingElement("OriginalMkvHash"))?,
        publisher_info,
        creation_timestamp,
        original_language,
        original_default_track_id,
        build_tools,
    })
}

fn decode_build_tools(data: &[u8]) -> Result<Vec<ToolEntry>, BinstructError> {
    let mut r = Cursor::new(data);
    let mut tools = Vec::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        if id == ID_TOOL_ENTRY {
            tools.push(decode_tool_entry(&payload)?);
        }
    }
    Ok(tools)
}

fn decode_tool_entry(data: &[u8]) -> Result<ToolEntry, BinstructError> {
    let mut r = Cursor::new(data);
    let mut name = String::new();
    let mut version = String::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_TOOL_NAME => {
                name = String::from_utf8(payload)
                    .map_err(|_| BinstructError::Ebml("ToolName not valid UTF-8"))?;
            }
            ID_TOOL_VERSION => {
                version = String::from_utf8(payload)
                    .map_err(|_| BinstructError::Ebml("ToolVersion not valid UTF-8"))?;
            }
            _ => {}
        }
    }
    Ok(ToolEntry { name, version })
}

fn decode_mkv_skeleton(data: &[u8]) -> Result<MkvSkeleton, BinstructError> {
    let mut r = Cursor::new(data);
    let mut pre_tracks_blob = Vec::new();
    let mut track_entries = Vec::new();
    let mut post_tracks_blob = Vec::new();

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_PRE_TRACKS_BLOB => { pre_tracks_blob = payload; }
            ID_TRACK_ENTRIES => { track_entries = decode_track_entries(&payload)?; }
            ID_POST_TRACKS_BLOB => { post_tracks_blob = payload; }
            _ => {}
        }
    }

    Ok(MkvSkeleton { pre_tracks_blob, track_entries, post_tracks_blob })
}

fn decode_track_entries(data: &[u8]) -> Result<Vec<TrackEntryRecord>, BinstructError> {
    let mut r = Cursor::new(data);
    let mut entries = Vec::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        if id == ID_TRACK_ENTRY_RECORD {
            entries.push(decode_track_entry_record(&payload)?);
        }
    }
    Ok(entries)
}

fn decode_track_entry_record(data: &[u8]) -> Result<TrackEntryRecord, BinstructError> {
    let mut r = Cursor::new(data);
    let mut track_id: u64 = 0;
    let mut bytes = Vec::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_TRACK_ENTRY_ID => { track_id = decode_uint(&payload); }
            ID_TRACK_ENTRY_BYTES => { bytes = payload; }
            _ => {}
        }
    }
    Ok(TrackEntryRecord { track_id, bytes })
}

fn decode_cluster_timestamps(data: &[u8]) -> Result<ClusterTimestamps, BinstructError> {
    let mut r = Cursor::new(data);
    let mut cluster_count: u64 = 0;
    let mut deltas = Vec::new();

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_CLUSTER_COUNT => { cluster_count = decode_uint(&payload); }
            ID_CLUSTER_TIMESTAMP_DELTAS => {
                let mut dr = Cursor::new(payload.as_slice());
                while (dr.position() as usize) < payload.len() {
                    deltas.push(read_signed_vint(&mut dr)?);
                }
            }
            _ => {}
        }
    }

    Ok(ClusterTimestamps { cluster_count, deltas })
}

fn decode_track_policies(data: &[u8]) -> Result<Vec<TrackPolicy>, BinstructError> {
    let mut r = Cursor::new(data);
    let mut policies = Vec::new();
    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        if id == ID_TRACK_POLICY {
            policies.push(decode_track_policy(&payload)?);
        }
    }
    Ok(policies)
}

fn decode_track_policy(data: &[u8]) -> Result<TrackPolicy, BinstructError> {
    let mut r = Cursor::new(data);
    let mut track_id: u64 = 0;
    let mut codec_type = CodecType::Video;
    let mut language_code = String::new();
    let mut frame_count: u64 = 0;
    let mut is_vbr = false;
    let mut frame_duration: u64 = 0;
    let mut cbr_frame_size: Option<u64> = None;
    let mut raw_file_hash = [0u8; 32];
    let mut vfr_file_hash: Option<[u8; 32]> = None;

    while (r.position() as usize) < data.len() {
        let (id, size) = read_element_header(&mut r)?;
        let payload = read_bytes(&mut r, size)?;
        match id {
            ID_TRACK_ID => { track_id = decode_uint(&payload); }
            ID_CODEC_TYPE => {
                codec_type = match payload.first().copied().unwrap_or(0) {
                    0 => CodecType::Video,
                    _ => CodecType::Audio,
                };
            }
            ID_LANGUAGE_CODE => {
                language_code = String::from_utf8(payload)
                    .map_err(|_| BinstructError::Ebml("LanguageCode not valid UTF-8"))?;
            }
            ID_FRAME_COUNT => { frame_count = decode_uint(&payload); }
            ID_IS_VBR => { is_vbr = payload.first().copied().unwrap_or(0) != 0; }
            ID_FRAME_DURATION => { frame_duration = decode_uint(&payload); }
            ID_CBR_FRAME_SIZE => { cbr_frame_size = Some(decode_uint(&payload)); }
            ID_RAW_FILE_HASH => {
                if payload.len() == 32 {
                    raw_file_hash = payload.try_into().unwrap();
                }
            }
            ID_VFR_FILE_HASH => {
                if payload.len() == 32 {
                    vfr_file_hash = Some(payload.try_into().unwrap());
                }
            }
            _ => {}
        }
    }

    Ok(TrackPolicy {
        track_id, codec_type, language_code, frame_count,
        is_vbr, frame_duration, cbr_frame_size, raw_file_hash, vfr_file_hash,
    })
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

impl CodecType {
    fn as_u8(&self) -> u8 {
        match self {
            CodecType::Video => 0,
            CodecType::Audio => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sample() -> BinstructFile {
        BinstructFile {
            header: Header { version: 1, config_flags: 0 },
            source: Source {
                original_mkv_hash: [0xAB; 32],
                publisher_info: "TestPublisher v1.0".to_string(),
                creation_timestamp: 1_746_576_000_000,
                original_language: "spa".to_string(),
                original_default_track_id: 2,
                build_tools: vec![
                    ToolEntry { name: "ffmpeg".to_string(), version: "ffmpeg version 6.1".to_string() },
                    ToolEntry { name: "mkvmerge".to_string(), version: "mkvmerge v80.0".to_string() },
                    ToolEntry { name: "mkvextract".to_string(), version: "mkvextract v80.0".to_string() },
                ],
            },
            mkv_skeleton: MkvSkeleton {
                pre_tracks_blob: (0u8..=127).collect(),
                track_entries: vec![
                    TrackEntryRecord { track_id: 1, bytes: vec![0x01, 0x02, 0x03, 0xAA, 0xBB] },
                    TrackEntryRecord { track_id: 2, bytes: vec![0x04, 0x05, 0xCC, 0xDD, 0xEE, 0xFF] },
                    TrackEntryRecord { track_id: 3, bytes: vec![0x10; 20] },
                ],
                post_tracks_blob: vec![0xDE, 0xAD, 0xBE, 0xEF],
            },
            cluster_timestamps: ClusterTimestamps {
                cluster_count: 5,
                deltas: vec![0, 1000, 1000, 999, 1001],
            },
            track_policies: vec![
                TrackPolicy {
                    track_id: 1,
                    codec_type: CodecType::Video,
                    language_code: "und".to_string(),
                    frame_count: 172_800,
                    is_vbr: true,
                    frame_duration: 512,
                    cbr_frame_size: None,
                    raw_file_hash: [0x11; 32],
                    vfr_file_hash: Some([0x22; 32]),
                },
                TrackPolicy {
                    track_id: 2,
                    codec_type: CodecType::Audio,
                    language_code: "spa".to_string(),
                    frame_count: 340_000,
                    is_vbr: true,
                    frame_duration: 960,
                    cbr_frame_size: None,
                    raw_file_hash: [0x33; 32],
                    vfr_file_hash: Some([0x44; 32]),
                },
                TrackPolicy {
                    track_id: 3,
                    codec_type: CodecType::Audio,
                    language_code: "eng".to_string(),
                    frame_count: 340_000,
                    is_vbr: false,
                    frame_duration: 960,
                    cbr_frame_size: Some(768),
                    raw_file_hash: [0x55; 32],
                    vfr_file_hash: None,
                },
            ],
        }
    }

    #[test]
    fn round_trip_uncompressed() {
        let original = make_sample();
        let bytes = original.serialize().unwrap();
        let decoded = BinstructFile::deserialize(&bytes).unwrap();
        assert_eq!(decoded.header.version, original.header.version);
        assert_eq!(decoded.header.config_flags, original.header.config_flags);
        assert_eq!(decoded.source.original_mkv_hash, original.source.original_mkv_hash);
        assert_eq!(decoded.source.publisher_info, original.source.publisher_info);
        assert_eq!(decoded.source.creation_timestamp, original.source.creation_timestamp);
        assert_eq!(decoded.source.original_language, original.source.original_language);
        assert_eq!(decoded.source.original_default_track_id, original.source.original_default_track_id);
        assert_eq!(decoded.source.build_tools.len(), 3);
        assert_eq!(decoded.source.build_tools[0].name, "ffmpeg");
        assert_eq!(decoded.source.build_tools[1].name, "mkvmerge");
        assert_eq!(decoded.source.build_tools[2].name, "mkvextract");
        assert_eq!(decoded.mkv_skeleton.pre_tracks_blob, original.mkv_skeleton.pre_tracks_blob);
        assert_eq!(decoded.mkv_skeleton.track_entries.len(), 3);
        assert_eq!(decoded.mkv_skeleton.track_entries[0].track_id, 1);
        assert_eq!(decoded.mkv_skeleton.track_entries[1].bytes, original.mkv_skeleton.track_entries[1].bytes);
        assert_eq!(decoded.mkv_skeleton.post_tracks_blob, original.mkv_skeleton.post_tracks_blob);
        assert_eq!(decoded.cluster_timestamps.cluster_count, 5);
        assert_eq!(decoded.cluster_timestamps.deltas, vec![0, 1000, 1000, 999, 1001]);
        assert_eq!(decoded.track_policies.len(), 3);
        assert!(matches!(decoded.track_policies[0].codec_type, CodecType::Video));
        assert_eq!(decoded.track_policies[0].vfr_file_hash, Some([0x22; 32]));
        assert!(matches!(decoded.track_policies[2].codec_type, CodecType::Audio));
        assert_eq!(decoded.track_policies[2].cbr_frame_size, Some(768));
        assert_eq!(decoded.track_policies[2].vfr_file_hash, None);
    }

    #[test]
    fn round_trip_zstd_compressed() {
        let mut original = make_sample();
        original.header.config_flags = 0x01; // enable zstd
        let bytes = original.serialize().unwrap();
        // Should start with zstd magic
        assert_eq!(&bytes[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        let decoded = BinstructFile::deserialize(&bytes).unwrap();
        assert_eq!(decoded.header.config_flags, 0x01);
        assert_eq!(decoded.source.publisher_info, original.source.publisher_info);
        assert_eq!(decoded.cluster_timestamps.deltas, original.cluster_timestamps.deltas);
        assert_eq!(decoded.track_policies.len(), 3);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let mut original = make_sample();
        original.header.config_flags = 0;
        let mut bytes = original.serialize().unwrap();
        // Corrupt the magic bytes inside the EBML structure.
        // BinstructFile master: [0x80, <size vint>] then Header master: [0x81, <size>]
        // then Magic element: [0x82, <size=4>] then 4 magic bytes.
        // Find magic bytes and overwrite them.
        let pos = bytes.windows(4).position(|w| w == b"VFUS").unwrap();
        bytes[pos..pos + 4].copy_from_slice(b"XXXX");
        let err = BinstructFile::deserialize(&bytes).unwrap_err();
        assert!(matches!(err, BinstructError::InvalidMagic(_)));
    }

    #[test]
    fn bad_version_is_rejected() {
        let mut original = make_sample();
        original.header.version = 99;
        // We have to manually build bytes since serialize checks version implicitly;
        // bypass by encoding directly.
        let bytes = original.encode_ebml().unwrap();
        let err = BinstructFile::deserialize(&bytes).unwrap_err();
        assert!(matches!(err, BinstructError::UnsupportedVersion(99)));
    }

    #[test]
    fn signed_vint_round_trips() {
        let values: &[i64] = &[0, 1, -1, 63, -63, 64, -64, 1000, -1000, 100_000, -100_000];
        for &v in values {
            let mut buf = Vec::new();
            write_signed_vint(&mut buf, v).unwrap();
            let mut cur = Cursor::new(buf.as_slice());
            let decoded = read_signed_vint(&mut cur).unwrap();
            assert_eq!(decoded, v, "signed vint round-trip failed for {v}");
        }
    }
}
