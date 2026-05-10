use crate::types::MuxerError;

/// Identity transform (audio): copy bytes verbatim.
pub fn transform_raw(frame_bytes: &[u8]) -> Vec<u8> {
    frame_bytes.to_vec()
}

/// Convert an Annex B encoded frame to AVCC.
///
/// `nal_lengths` lists the lengths of each NAL unit *payload* (excluding the
/// start code). The function detects 3-byte (`0x00 0x00 0x01`) or 4-byte
/// (`0x00 0x00 0x00 0x01`) start codes dynamically (H.5).
///
/// Output layout: for each NAL, a 4-byte big-endian length prefix followed by
/// the NAL payload. Total size: `4 * nal_lengths.len() + sum(nal_lengths)`.
pub fn transform_avcc(frame_bytes: &[u8], nal_lengths: &[u64]) -> Result<Vec<u8>, MuxerError> {
    let total_payload: u64 = nal_lengths.iter().sum();
    let mut out = Vec::with_capacity(4 * nal_lengths.len() + total_payload as usize);
    let mut pos = 0usize;
    for &nal_len in nal_lengths {
        if pos + 4 <= frame_bytes.len()
            && frame_bytes[pos] == 0
            && frame_bytes[pos + 1] == 0
            && frame_bytes[pos + 2] == 0
            && frame_bytes[pos + 3] == 1
        {
            pos += 4;
        } else if pos + 3 <= frame_bytes.len()
            && frame_bytes[pos] == 0
            && frame_bytes[pos + 1] == 0
            && frame_bytes[pos + 2] == 1
        {
            pos += 3;
        } else {
            return Err(MuxerError::InvalidBitstream(format!(
                "Annex B start code not found at offset {pos}"
            )));
        }
        if pos + (nal_len as usize) > frame_bytes.len() {
            return Err(MuxerError::InvalidBitstream(format!(
                "NAL payload overruns frame: pos={pos}, nal_len={nal_len}, frame_len={}",
                frame_bytes.len()
            )));
        }
        let nal_u32 = u32::try_from(nal_len).map_err(|_| {
            MuxerError::InvalidBitstream(format!("NAL length {nal_len} exceeds u32"))
        })?;
        out.extend_from_slice(&nal_u32.to_be_bytes());
        out.extend_from_slice(&frame_bytes[pos..pos + nal_len as usize]);
        pos += nal_len as usize;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_identity() {
        let input = vec![1, 2, 3, 4, 5];
        assert_eq!(transform_raw(&input), input);
    }

    #[test]
    fn avcc_4byte_start_code() {
        let mut input = Vec::new();
        input.extend_from_slice(&[0, 0, 0, 1]);
        input.extend((0u8..10u8).collect::<Vec<_>>());
        input.extend_from_slice(&[0, 0, 0, 1]);
        input.extend(vec![0xAA; 5]);
        let nal_lengths = vec![10u64, 5];
        let got = transform_avcc(&input, &nal_lengths).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&10u32.to_be_bytes());
        expected.extend((0u8..10u8).collect::<Vec<_>>());
        expected.extend_from_slice(&5u32.to_be_bytes());
        expected.extend(vec![0xAA; 5]);
        assert_eq!(got, expected);
    }

    #[test]
    fn avcc_3byte_start_code() {
        let mut input = Vec::new();
        input.extend_from_slice(&[0, 0, 1]);
        input.extend((0u8..10u8).collect::<Vec<_>>());
        input.extend_from_slice(&[0, 0, 1]);
        input.extend(vec![0xAA; 5]);
        let nal_lengths = vec![10u64, 5];
        let got = transform_avcc(&input, &nal_lengths).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&10u32.to_be_bytes());
        expected.extend((0u8..10u8).collect::<Vec<_>>());
        expected.extend_from_slice(&5u32.to_be_bytes());
        expected.extend(vec![0xAA; 5]);
        assert_eq!(got, expected);
    }

    #[test]
    fn avcc_mixed_start_codes() {
        let mut input = Vec::new();
        // First NAL with 4-byte start code
        input.extend_from_slice(&[0, 0, 0, 1]);
        input.extend(vec![0x11; 4]);
        // Second NAL with 3-byte start code
        input.extend_from_slice(&[0, 0, 1]);
        input.extend(vec![0x22; 3]);
        let got = transform_avcc(&input, &[4u64, 3]).unwrap();
        assert_eq!(&got[..4], &4u32.to_be_bytes());
        assert_eq!(&got[4..8], &[0x11; 4]);
        assert_eq!(&got[8..12], &3u32.to_be_bytes());
        assert_eq!(&got[12..15], &[0x22; 3]);
    }

    #[test]
    fn avcc_missing_start_code_errors() {
        let input = vec![1, 2, 3, 4]; // no start code
        let err = transform_avcc(&input, &[4]).unwrap_err();
        assert!(matches!(err, MuxerError::InvalidBitstream(_)));
    }
}
