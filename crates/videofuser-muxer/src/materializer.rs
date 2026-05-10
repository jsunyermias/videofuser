use crate::types::{DownloadState, MuxerError, ReadPolicy, TrackSource};
use crate::vfr_index::LazyVfrIndex;

/// Read the raw bytes of `frame_index` from `source.raw_file`, respecting the
/// download state policy.
///
/// Returns:
/// - `Ok(Some(bytes))` on success.
/// - `Ok(None)` on timeout / non-block miss: callers MUST treat the frame as
///   "not yet available" and emit zero-padding of the correct length.
pub fn materialize_frame(
    source: &TrackSource,
    frame_index: u32,
    vfr_index: Option<&LazyVfrIndex>,
    download: &dyn DownloadState,
    policy: ReadPolicy,
) -> Result<MaterializedFrame, MuxerError> {
    let (frame_offset, frame_len) = compute_offset_and_len(source, frame_index, vfr_index)?;

    match download.wait_for(source.track_id, frame_offset, frame_len, policy)? {
        true => {
            let mut buf = vec![0u8; frame_len as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = source
                    .raw_file
                    .read_at(frame_offset + filled as u64, &mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled != buf.len() {
                return Err(MuxerError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "short read: expected {} got {} for track {} frame {}",
                        buf.len(),
                        filled,
                        source.track_id,
                        frame_index,
                    ),
                )));
            }
            Ok(MaterializedFrame::Bytes(buf))
        }
        false => Ok(MaterializedFrame::Unavailable {
            frame_len,
            frame_offset,
        }),
    }
}

/// Outcome of [`materialize_frame`].
pub enum MaterializedFrame {
    Bytes(Vec<u8>),
    /// The raw bytes are not yet on disk (timeout/non-block).
    Unavailable { frame_offset: u64, frame_len: u32 },
}

/// Computes `(frame_offset, frame_len)` in the raw track file.
pub fn compute_offset_and_len(
    source: &TrackSource,
    frame_index: u32,
    vfr_index: Option<&LazyVfrIndex>,
) -> Result<(u64, u32), MuxerError> {
    if let Some(idx) = vfr_index {
        let off = idx.frame_offset(frame_index);
        let len = idx.frame_size(frame_index);
        Ok((off, len))
    } else {
        // CBR audio
        let cbr = source.policy.cbr_frame_size.ok_or_else(|| {
            MuxerError::InvalidBinstruct(format!(
                "track {} marked CBR but missing cbr_frame_size",
                source.track_id
            ))
        })?;
        let off = frame_index as u64 * cbr;
        Ok((off, cbr as u32))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use videofuser_binstruct::{CodecType, TrackPolicy};

    use super::*;
    use crate::types::{FullyAvailable, MemRawFile};

    #[test]
    fn cbr_audio_offsets_are_linear() {
        let policy = TrackPolicy {
            track_id: 7,
            codec_type: CodecType::Audio,
            language_code: "eng".into(),
            frame_count: 4,
            is_vbr: false,
            frame_duration: 960,
            cbr_frame_size: Some(768),
            raw_file_hash: [0; 32],
            vfr_file_hash: None,
        };
        // 4 frames × 768 bytes each
        let data: Vec<u8> = (0u32..768 * 4).map(|v| (v & 0xFF) as u8).collect();
        let src = TrackSource {
            track_id: 7,
            raw_file: Arc::new(MemRawFile::new(data.clone())),
            vfr: None,
            policy,
        };
        let avail = FullyAvailable;
        for i in 0..4u32 {
            let frame = match materialize_frame(&src, i, None, &avail, ReadPolicy::Block).unwrap() {
                MaterializedFrame::Bytes(b) => b,
                _ => panic!("expected bytes"),
            };
            assert_eq!(frame.len(), 768);
            let start = (i as usize) * 768;
            assert_eq!(frame, data[start..start + 768]);
        }
    }
}
