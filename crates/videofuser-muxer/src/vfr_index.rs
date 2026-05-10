use std::sync::Arc;

use videofuser_vfr::VfrFile;

/// Checkpoint stride for the lazy VFR index.
pub const CHECKPOINT_STRIDE: u32 = 1024;

/// Accumulated frame offsets and timestamps every `CHECKPOINT_STRIDE` frames.
///
/// `offsets_checkpoint[K]` = sum of `frame_size` for frames `[0, K*STRIDE)`.
/// `time_checkpoint[K]`    = sum of `(frame_duration + duration_delta)` for
/// frames `[0, K*STRIDE)`, in track timebase units.
/// `nal_offsets_checkpoint[K]` = sum of `nal_count` for frames `[0, K*STRIDE)`.
pub struct LazyVfrIndex {
    pub vfr: Arc<VfrFile>,
    /// Base `frame_duration` from the TrackPolicy, used as the implicit
    /// duration for VBR frames before adding the per-frame delta.
    pub base_duration: u64,
    pub offsets_checkpoint: Vec<u64>,
    pub time_checkpoint: Vec<u64>,
    pub nal_offsets_checkpoint: Vec<u64>,
}

impl LazyVfrIndex {
    pub fn new(vfr: Arc<VfrFile>, base_duration: u64) -> Self {
        let frame_count = vfr.frames.len();
        let checkpoint_count = frame_count / CHECKPOINT_STRIDE as usize + 1;
        let mut offsets = Vec::with_capacity(checkpoint_count);
        let mut times = Vec::with_capacity(checkpoint_count);
        let mut nals = Vec::with_capacity(checkpoint_count);

        let mut acc_off: u64 = 0;
        let mut acc_time: u64 = 0;
        let mut acc_nal: u64 = 0;

        for (i, fr) in vfr.frames.iter().enumerate() {
            if i % CHECKPOINT_STRIDE as usize == 0 {
                offsets.push(acc_off);
                times.push(acc_time);
                nals.push(acc_nal);
            }
            acc_off += fr.frame_size as u64;
            acc_time = acc_time
                .wrapping_add(base_duration as i128 as u64)
                .wrapping_add(fr.duration_delta as i64 as u64);
            acc_nal += fr.nal_count as u64;
        }
        // Sentinel after-last checkpoint so callers can binary-search safely.
        offsets.push(acc_off);
        times.push(acc_time);
        nals.push(acc_nal);

        Self {
            vfr,
            base_duration,
            offsets_checkpoint: offsets,
            time_checkpoint: times,
            nal_offsets_checkpoint: nals,
        }
    }

    pub fn frame_count(&self) -> u32 {
        self.vfr.frames.len() as u32
    }

    /// Byte offset of the start of `frame_index` inside the raw track file.
    pub fn frame_offset(&self, frame_index: u32) -> u64 {
        let k = (frame_index / CHECKPOINT_STRIDE) as usize;
        let mut off = self.offsets_checkpoint[k];
        let start = k * CHECKPOINT_STRIDE as usize;
        for i in start..frame_index as usize {
            off += self.vfr.frames[i].frame_size as u64;
        }
        off
    }

    /// Length in bytes of `frame_index` in the raw track file.
    pub fn frame_size(&self, frame_index: u32) -> u32 {
        self.vfr.frames[frame_index as usize].frame_size
    }

    /// Timestamp (start) of `frame_index` in track timebase units, computed as
    /// `sum_{i<frame_index} (base_duration + duration_delta_i)`.
    pub fn frame_time(&self, frame_index: u32) -> u64 {
        let k = (frame_index / CHECKPOINT_STRIDE) as usize;
        let mut t = self.time_checkpoint[k];
        let start = k * CHECKPOINT_STRIDE as usize;
        for i in start..frame_index as usize {
            t = t.wrapping_add(self.base_duration);
            t = t.wrapping_add(self.vfr.frames[i].duration_delta as i64 as u64);
        }
        t
    }

    /// Starting index into `VfrFile::nal_lengths` for `frame_index`.
    pub fn nal_offset(&self, frame_index: u32) -> u64 {
        let k = (frame_index / CHECKPOINT_STRIDE) as usize;
        let mut n = self.nal_offsets_checkpoint[k];
        let start = k * CHECKPOINT_STRIDE as usize;
        for i in start..frame_index as usize {
            n += self.vfr.frames[i].nal_count as u64;
        }
        n
    }

    /// Number of NAL units in `frame_index`.
    pub fn nal_count(&self, frame_index: u32) -> u32 {
        self.vfr.frames[frame_index as usize].nal_count as u32
    }

    /// Returns the slice of NAL lengths corresponding to `frame_index`.
    pub fn nal_lengths_of_frame(&self, frame_index: u32) -> &[u64] {
        let start = self.nal_offset(frame_index) as usize;
        let count = self.nal_count(frame_index) as usize;
        &self.vfr.nal_lengths[start..start + count]
    }

    /// Returns the smallest `frame_index` whose start timestamp is `>= ts`.
    /// Used to bucket frames into clusters.
    pub fn frame_at_time(&self, ts: u64) -> u32 {
        // Binary-search the checkpoints first, then refine linearly.
        let cps = &self.time_checkpoint;
        let mut lo = 0usize;
        let mut hi = cps.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cps[mid] < ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // After the loop, lo is the first checkpoint with cps[lo] >= ts.
        // The target frame is in [(lo-1)*STRIDE, lo*STRIDE) — walk backwards.
        let block_start = if lo == 0 {
            0
        } else {
            ((lo - 1) * CHECKPOINT_STRIDE as usize).min(self.vfr.frames.len())
        };
        let mut t = if lo == 0 { 0 } else { cps[lo - 1] };
        let frame_count = self.vfr.frames.len();
        for i in block_start..frame_count {
            if t >= ts {
                return i as u32;
            }
            t = t.wrapping_add(self.base_duration);
            t = t.wrapping_add(self.vfr.frames[i].duration_delta as i64 as u64);
        }
        frame_count as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videofuser_vfr::FrameRecord;

    fn mk_vfr(frame_sizes: &[u32], deltas: &[i16]) -> Arc<VfrFile> {
        let frames: Vec<FrameRecord> = frame_sizes
            .iter()
            .zip(deltas.iter())
            .map(|(&size, &d)| FrameRecord {
                frame_size: size,
                flags: 0x01, // keyframe (so cues tests find them)
                nal_count: 0,
                duration_delta: d,
            })
            .collect();
        Arc::new(VfrFile {
            version: 1,
            flags: 0,
            frames,
            nal_lengths: vec![],
        })
    }

    #[test]
    fn frame_offset_and_time_consistent() {
        let vfr = mk_vfr(&[100, 200, 50, 75], &[0, 0, 0, 0]);
        let idx = LazyVfrIndex::new(vfr, 1000);
        assert_eq!(idx.frame_offset(0), 0);
        assert_eq!(idx.frame_offset(1), 100);
        assert_eq!(idx.frame_offset(2), 300);
        assert_eq!(idx.frame_offset(3), 350);
        assert_eq!(idx.frame_time(0), 0);
        assert_eq!(idx.frame_time(1), 1000);
        assert_eq!(idx.frame_time(2), 2000);
        assert_eq!(idx.frame_time(3), 3000);
    }

    #[test]
    fn frame_at_time_locates_correctly() {
        let vfr = mk_vfr(&[10, 10, 10, 10, 10], &[0, 0, 0, 0, 0]);
        let idx = LazyVfrIndex::new(vfr, 100);
        // Frame times: 0, 100, 200, 300, 400.
        assert_eq!(idx.frame_at_time(0), 0);
        assert_eq!(idx.frame_at_time(50), 1);
        assert_eq!(idx.frame_at_time(100), 1);
        assert_eq!(idx.frame_at_time(250), 3);
    }
}
