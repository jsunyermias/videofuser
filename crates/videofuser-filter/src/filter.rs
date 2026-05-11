use crate::prefs::Prefs;
use crate::track_meta::{TrackKind, TrackMeta};

// ---------------------------------------------------------------------------
// TrackFilter — result of audio/video track resolution
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TrackFilter {
    /// Track IDs included in the virtual MKV (all video + filtered audio), sorted.
    pub included_track_ids: Vec<u32>,
    /// Which included audio track carries FlagDefault=1.
    pub default_audio_track_id: u32,
}

/// Resolves which tracks to include and which audio track is default.
///
/// Algorithm (spec caps. 12.1, 15.1):
///
/// Step 1 — Build the audio candidate set:
///   - All audio tracks whose language matches any entry in prefs.audio_langs.
///   - If that preferred set is non-empty, also add tracks whose language ==
///     original_language (the original movie language).
///   - If the preferred set is empty → fallback: all audio tracks.
///
/// Step 2 — Always include all video tracks.
///
/// Step 3 — Select the default audio track (caps. 12.3, 15.3):
///   - Iterate prefs.audio_langs in order; for the first language L that has
///     tracks in the candidate set, pick the best-codec track for L.
///   - "Best codec" = lowest index in prefs.audio_codec; ties broken by
///     lowest track_id.
///   - If no preferred language has tracks, search by original_language.
///   - If still none → use original_default_track_id.
pub fn resolve_filter(
    tracks: &[TrackMeta],
    original_language: &str,
    original_default_track_id: u32,
    prefs: &Prefs,
) -> TrackFilter {
    let audio_tracks: Vec<&TrackMeta> =
        tracks.iter().filter(|t| t.kind == TrackKind::Audio).collect();
    let video_tracks: Vec<&TrackMeta> =
        tracks.iter().filter(|t| t.kind == TrackKind::Video).collect();

    // Step 1 — audio candidates.
    let preferred: Vec<&TrackMeta> = audio_tracks
        .iter()
        .copied()
        .filter(|t| prefs.audio_langs.iter().any(|l| l == &t.language))
        .collect();

    let candidates: Vec<&TrackMeta> = if preferred.is_empty() {
        // Fallback: include everything.
        audio_tracks.iter().copied().collect()
    } else {
        // Include preferred + original-language tracks.
        let mut set = preferred;
        for t in &audio_tracks {
            if t.language == original_language && !set.iter().any(|s| s.track_id == t.track_id) {
                set.push(t);
            }
        }
        set
    };

    // Step 2 — collect all included track IDs.
    let mut included: Vec<u32> = video_tracks.iter().map(|t| t.track_id).collect();
    for t in &candidates {
        included.push(t.track_id);
    }
    included.sort_unstable();
    included.dedup();

    // Step 3 — choose default audio track.
    let default_audio = select_default_audio(&candidates, original_language, original_default_track_id, prefs);

    TrackFilter {
        included_track_ids: included,
        default_audio_track_id: default_audio,
    }
}

/// Selects the default audio track ID following the priority rules in cap. 12.3.
fn select_default_audio(
    candidates: &[&TrackMeta],
    original_language: &str,
    original_default_track_id: u32,
    prefs: &Prefs,
) -> u32 {
    // Try each preferred language in order.
    for lang in &prefs.audio_langs {
        let lang_tracks: Vec<&&TrackMeta> =
            candidates.iter().filter(|t| &t.language == lang).collect();
        if !lang_tracks.is_empty() {
            return best_codec_track(&lang_tracks, &prefs.audio_codec);
        }
    }

    // No preferred language matched — try the original language.
    let original_tracks: Vec<&&TrackMeta> = candidates
        .iter()
        .filter(|t| t.language == original_language)
        .collect();
    if !original_tracks.is_empty() {
        return best_codec_track(&original_tracks, &prefs.audio_codec);
    }

    // Final fallback.
    original_default_track_id
}

/// Among `tracks`, returns the track_id with the best codec rank.
/// Ties broken by lowest track_id.
fn best_codec_track(tracks: &[&&TrackMeta], audio_codec: &[String]) -> u32 {
    tracks
        .iter()
        .min_by_key(|t| {
            let rank = audio_codec
                .iter()
                .position(|c| c == &t.codec_name)
                .unwrap_or(usize::MAX);
            (rank, t.track_id)
        })
        .map(|t| t.track_id)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// SubFile — subtitle sidecar file descriptor
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SubFile {
    /// Full sidecar filename.
    pub filename: String,
    /// Language code extracted from the filename's <lang> field.
    pub language: String,
    /// Variant extracted from the filename's <variant> field.
    pub variant: String,
}

impl SubFile {
    /// Parses a sidecar filename following the pattern:
    ///   `<base>_s<NNN>_<lang>_<variant>_<vv>.<ext>`
    ///
    /// Returns `None` if the pattern is not matched.
    pub fn from_filename(filename: &str) -> Option<Self> {
        let parts: Vec<&str> = filename.split('_').collect();

        // Find the first segment matching s<NNN> (s followed only by ASCII digits).
        let track_idx = parts.iter().position(|p| {
            p.starts_with('s')
                && p.len() >= 2
                && p[1..].chars().all(|c| c.is_ascii_digit())
        })?;

        // Need lang, variant, and vv.ext after the s<NNN> segment.
        if track_idx + 3 >= parts.len() {
            return None;
        }

        // The last segment must carry the file extension.
        let last = *parts.last()?;
        if !last.contains('.') {
            return None;
        }

        let language = parts[track_idx + 1].to_string();
        let variant = parts[track_idx + 2].to_string();

        Some(SubFile {
            filename: filename.to_string(),
            language,
            variant,
        })
    }
}

/// Returns the subset of `sub_files` whose language is in `prefs.sub_langs`.
/// Falls back to all files if no candidates match (spec caps. 12.2, 15.2).
pub fn resolve_subs_filter<'a>(sub_files: &'a [SubFile], prefs: &Prefs) -> Vec<&'a SubFile> {
    if prefs.sub_langs.is_empty() {
        return sub_files.iter().collect();
    }

    let candidates: Vec<&SubFile> = sub_files
        .iter()
        .filter(|s| prefs.sub_langs.iter().any(|l| l == &s.language))
        .collect();

    if candidates.is_empty() {
        sub_files.iter().collect()
    } else {
        candidates
    }
}
