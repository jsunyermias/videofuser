use std::io::{self, Write};

use crate::types::MuxerError;

/// Byte length of the EBML VINT encoding of `value` (unsigned).
pub fn vint_len(value: u64) -> usize {
    // The maximum representable value with n VINT bytes is 2^(7n) - 2, because
    // the all-ones pattern is reserved for "unknown size".
    match value {
        0..=0x7E => 1,
        0..=0x3FFE => 2,
        0..=0x1FFFFE => 3,
        0..=0x0FFFFFFE => 4,
        0..=0x07FFFFFFFE => 5,
        0..=0x03FFFFFFFFFE => 6,
        0..=0x01FFFFFFFFFFFE => 7,
        _ => 8,
    }
}

/// Write a VINT (unsigned, big-endian, self-delimiting) of `value` using the
/// minimum number of bytes.
pub fn write_vint(w: &mut impl Write, value: u64) -> io::Result<()> {
    let n = vint_len(value);
    write_vint_with_len(w, value, n)
}

/// Write a VINT forcing a specific byte width (useful for the unknown-size
/// pattern). `n` must be in 1..=8.
pub fn write_vint_with_len(w: &mut impl Write, value: u64, n: usize) -> io::Result<()> {
    debug_assert!((1..=8).contains(&n));
    let marker = 0x80u8 >> (n - 1);
    let be = value.to_be_bytes();
    let mut out = [0u8; 8];
    out[..n].copy_from_slice(&be[8 - n..]);
    out[0] |= marker;
    w.write_all(&out[..n])
}

/// Encode a VINT into a heap buffer.
pub fn encode_vint(value: u64) -> Vec<u8> {
    let n = vint_len(value);
    let mut buf = Vec::with_capacity(n);
    write_vint_with_len(&mut buf, value, n).expect("Vec write never fails");
    buf
}

/// EBML "unknown size" pattern packed into 8 bytes (all 1s after the leading
/// marker bit).
pub const UNKNOWN_SIZE_8B: u64 = 0x00FF_FFFF_FFFF_FFFFu64;

/// Encode a uint as the minimal big-endian payload (no length prefix).
pub fn encode_uint_be(value: u64) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let be = value.to_be_bytes();
    let skip = be.iter().position(|&b| b != 0).unwrap_or(7);
    be[skip..].to_vec()
}

// ---------------------------------------------------------------------------
// Matroska element IDs we touch directly.
// ---------------------------------------------------------------------------

pub const ID_SEGMENT_BYTES: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
pub const ID_SEEKHEAD_BYTES: [u8; 4] = [0x11, 0x4D, 0x9B, 0x74];
pub const ID_SEEK_BYTES: [u8; 2] = [0x4D, 0xBB];
pub const ID_SEEKID_BYTES: [u8; 2] = [0x53, 0xAB];
pub const ID_SEEKPOSITION_BYTES: [u8; 2] = [0x53, 0xAC];
pub const ID_INFO_BYTES: [u8; 4] = [0x15, 0x49, 0xA9, 0x66];
pub const ID_TRACKS_BYTES: [u8; 4] = [0x16, 0x54, 0xAE, 0x6B];
pub const ID_CLUSTER_BYTES: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
pub const ID_TIMESTAMP_BYTES: [u8; 1] = [0xE7];
pub const ID_SIMPLEBLOCK_BYTES: [u8; 1] = [0xA3];
pub const ID_CUES_BYTES: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];
pub const ID_CUEPOINT_BYTES: [u8; 1] = [0xBB];
pub const ID_CUETIME_BYTES: [u8; 1] = [0xB3];
pub const ID_CUETRACKPOSITIONS_BYTES: [u8; 1] = [0xB7];
pub const ID_CUETRACK_BYTES: [u8; 1] = [0xF7];
pub const ID_CUECLUSTERPOSITION_BYTES: [u8; 1] = [0xF1];

// TrackEntry-level IDs we may need to find inside a TrackEntry blob.
pub const TE_TRACKNUMBER_ID: [u8; 1] = [0xD7];
pub const TE_FLAGDEFAULT_ID: [u8; 1] = [0x88];
pub const TE_TRACKENTRY_ID: [u8; 1] = [0xAE];

/// Read a VINT from `bytes[pos..]`. Returns `(value, n)` where `n` is the
/// number of bytes consumed.
pub fn read_vint(bytes: &[u8], pos: usize) -> Result<(u64, usize), MuxerError> {
    if pos >= bytes.len() {
        return Err(MuxerError::InvalidBitstream(
            "unexpected EOF reading VINT".into(),
        ));
    }
    let first = bytes[pos];
    if first == 0 {
        return Err(MuxerError::InvalidBitstream("invalid VINT leading 0".into()));
    }
    let n = first.leading_zeros() as usize + 1;
    if n > 8 || pos + n > bytes.len() {
        return Err(MuxerError::InvalidBitstream("truncated VINT".into()));
    }
    let mask = 0x80u8 >> (n - 1);
    let mut value = (first & !mask) as u64;
    for i in 1..n {
        value = (value << 8) | bytes[pos + i] as u64;
    }
    Ok((value, n))
}

/// Read a Matroska element ID at `bytes[pos..]`. Returns `(id_bytes, n)`,
/// preserving the leading marker bit (Matroska IDs are stored "raw").
pub fn read_element_id<'a>(
    bytes: &'a [u8],
    pos: usize,
) -> Result<(&'a [u8], usize), MuxerError> {
    if pos >= bytes.len() {
        return Err(MuxerError::InvalidBitstream(
            "unexpected EOF reading element ID".into(),
        ));
    }
    let first = bytes[pos];
    if first == 0 {
        return Err(MuxerError::InvalidBitstream("invalid element ID".into()));
    }
    let n = first.leading_zeros() as usize + 1;
    if n > 4 || pos + n > bytes.len() {
        return Err(MuxerError::InvalidBitstream("truncated element ID".into()));
    }
    Ok((&bytes[pos..pos + n], n))
}

/// Find the *absolute byte offset* (within `blob`) of the VINT-sized payload
/// of the first occurrence of the element whose ID matches `id`. Returns the
/// offset of the **first content byte** along with the content length.
///
/// `blob` is expected to be a *master element body* (the bytes that go after
/// the master's ID and VINT size). We scan top-level children only.
pub fn find_child_payload(
    blob: &[u8],
    id: &[u8],
) -> Result<Option<(usize, usize)>, MuxerError> {
    let mut pos = 0;
    while pos < blob.len() {
        let (cid, idn) = read_element_id(blob, pos)?;
        pos += idn;
        let (size, sn) = read_vint(blob, pos)?;
        pos += sn;
        let payload_start = pos;
        let payload_end = pos + size as usize;
        if payload_end > blob.len() {
            return Err(MuxerError::InvalidBitstream(
                "child element overflows blob".into(),
            ));
        }
        if cid == id {
            return Ok(Some((payload_start, size as usize)));
        }
        pos = payload_end;
    }
    Ok(None)
}

/// A TrackEntry blob can be stored either as the entire `0xAE <size> <body>`
/// element or as just `<body>`. This helper returns the body slice.
pub fn track_entry_body(blob: &[u8]) -> Result<&[u8], MuxerError> {
    if blob.is_empty() {
        return Err(MuxerError::InvalidBitstream("empty TrackEntry blob".into()));
    }
    if blob[0] == TE_TRACKENTRY_ID[0] {
        // Skip ID (1 byte) and VINT size.
        let (size, sn) = read_vint(blob, 1)?;
        let body_start = 1 + sn;
        let body_end = body_start + size as usize;
        if body_end > blob.len() {
            return Err(MuxerError::InvalidBitstream(
                "TrackEntry size overflows blob".into(),
            ));
        }
        Ok(&blob[body_start..body_end])
    } else {
        Ok(blob)
    }
}

/// Returns the byte offset of the FlagDefault *value byte* within `blob`, if
/// the TrackEntry has a FlagDefault child of size 1. Offsets are relative to
/// the start of `blob`.
pub fn find_flag_default_value_offset(blob: &[u8]) -> Result<Option<usize>, MuxerError> {
    // Find body start (so we can rebase offsets).
    let body_offset = if !blob.is_empty() && blob[0] == TE_TRACKENTRY_ID[0] {
        let (_size, sn) = read_vint(blob, 1)?;
        1 + sn
    } else {
        0
    };
    let body = &blob[body_offset..];
    let found = find_child_payload(body, &TE_FLAGDEFAULT_ID)?;
    match found {
        Some((payload_off, size)) => {
            if size != 1 {
                return Err(MuxerError::InvalidBitstream(
                    "FlagDefault must be 1 byte".into(),
                ));
            }
            Ok(Some(body_offset + payload_off))
        }
        None => Ok(None),
    }
}

/// Reads the TrackNumber value from a TrackEntry blob, if present. Used for
/// SimpleBlock track-number serialization (which is required to match the
/// TrackEntry).
pub fn find_track_number(blob: &[u8]) -> Result<Option<u64>, MuxerError> {
    let body = track_entry_body(blob)?;
    if let Some((off, size)) = find_child_payload(body, &TE_TRACKNUMBER_ID)? {
        let bytes = &body[off..off + size];
        let mut v = 0u64;
        for &b in bytes {
            v = (v << 8) | b as u64;
        }
        Ok(Some(v))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Small helpers for byte-emission
// ---------------------------------------------------------------------------

/// Write `id_bytes` + VINT(payload.len()) + payload.
pub fn write_element(w: &mut Vec<u8>, id_bytes: &[u8], payload: &[u8]) {
    w.extend_from_slice(id_bytes);
    let _ = write_vint(w, payload.len() as u64);
    w.extend_from_slice(payload);
}

/// Write `id_bytes` + VINT(value_be.len()) + value_be (minimal UInt).
pub fn write_uint_element(w: &mut Vec<u8>, id_bytes: &[u8], value: u64) {
    let v = encode_uint_be(value);
    write_element(w, id_bytes, &v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_roundtrip_small() {
        for v in [0u64, 1, 126, 127, 128, 16382, 16383, 16384, 0xFFFF, 1_000_000] {
            let buf = encode_vint(v);
            let (decoded, n) = read_vint(&buf, 0).unwrap();
            assert_eq!(decoded, v, "value {v}");
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn unknown_size_pattern_is_canonical_ebml() {
        // For n=8, the marker bit consumes the first byte entirely (no data
        // bits there); the remaining 7 bytes carry 56 data bits all set to 1.
        // Therefore the canonical 8-byte "unknown size" VINT is:
        //   0x01 0xFF 0xFF 0xFF 0xFF 0xFF 0xFF 0xFF
        let mut buf = Vec::new();
        write_vint_with_len(&mut buf, UNKNOWN_SIZE_8B, 8).unwrap();
        let mut expected = vec![0x01u8];
        expected.extend(std::iter::repeat(0xFFu8).take(7));
        assert_eq!(buf, expected);
    }

    #[test]
    fn find_flag_default_in_track_entry_body() {
        // Body with: TrackNumber=0xD7 (1 byte size, value 7), FlagDefault=0x88 (size 1, value 1)
        let body: Vec<u8> = vec![
            0xD7, 0x81, 0x07, // TrackNumber = 7
            0x88, 0x81, 0x01, // FlagDefault = 1
        ];
        let off = find_flag_default_value_offset(&body).unwrap().unwrap();
        assert_eq!(off, 5);
        assert_eq!(body[off], 1);

        let tn = find_track_number(&body).unwrap().unwrap();
        assert_eq!(tn, 7);
    }
}
