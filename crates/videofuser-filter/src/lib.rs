pub mod filter;
pub mod prefs;
pub mod track_meta;
pub mod validate;

pub use filter::{resolve_filter, resolve_subs_filter, SubFile, TrackFilter};
pub use prefs::{Prefs, ReadMode};
pub use track_meta::{extract_track_metas, TrackKind, TrackMeta};
pub use validate::{validate_prefs, PrefsValidationError};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::filter::{resolve_filter, resolve_subs_filter, SubFile};
    use super::prefs::Prefs;
    use super::track_meta::{TrackKind, TrackMeta};
    use super::validate::{validate_prefs, PrefsValidationError};

    fn make_track(id: u32, kind: TrackKind, lang: &str, codec: &str) -> TrackMeta {
        TrackMeta {
            track_id: id,
            kind,
            language: lang.to_string(),
            codec_id: String::new(),
            codec_name: codec.to_string(),
        }
    }

    fn make_sub(filename: &str, lang: &str) -> SubFile {
        SubFile {
            filename: filename.to_string(),
            language: lang.to_string(),
            variant: "00".to_string(),
        }
    }

    // ── AUDIO BÁSICO ──────────────────────────────────────────────────────────

    #[test]
    fn test_audio_filter_exact_match() {
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "eac3"),
            make_track(20, TrackKind::Audio, "en", "eac3"),
            make_track(30, TrackKind::Audio, "fr", "ac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["es".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.included_track_ids, vec![1, 10, 20]);
        assert_eq!(f.default_audio_track_id, 10);
    }

    #[test]
    fn test_audio_filter_includes_original_always() {
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "ac3"),
            make_track(20, TrackKind::Audio, "en", "aac"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["es".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.included_track_ids, vec![1, 10, 20]);
        assert_eq!(f.default_audio_track_id, 10);
    }

    #[test]
    fn test_audio_filter_fallback_when_no_match() {
        // "ja" is not found among any track, so preferred set is empty → fallback = all audio.
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "eac3"),
            make_track(20, TrackKind::Audio, "en", "eac3"),
            make_track(30, TrackKind::Audio, "fr", "ac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["ja".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.included_track_ids, vec![1, 10, 20, 30]);
        assert_eq!(f.default_audio_track_id, 20);
    }

    #[test]
    fn test_audio_filter_empty_langs_is_fallback() {
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "eac3"),
            make_track(20, TrackKind::Audio, "en", "eac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec![],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.included_track_ids, vec![1, 10, 20]);
        assert_eq!(f.default_audio_track_id, 20);
    }

    // ── SELECCIÓN DE CODEC ────────────────────────────────────────────────────

    #[test]
    fn test_codec_preference_respected() {
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "ac3"),
            make_track(11, TrackKind::Audio, "es", "eac3"),
            make_track(12, TrackKind::Audio, "es", "truehd"),
            make_track(20, TrackKind::Audio, "en", "aac"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["es".into()],
            audio_codec: vec!["truehd".into(), "eac3".into(), "ac3".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.default_audio_track_id, 12);
    }

    #[test]
    fn test_codec_preference_fallback_to_next() {
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "ac3"),
            make_track(11, TrackKind::Audio, "es", "eac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["es".into()],
            audio_codec: vec!["truehd".into(), "eac3".into(), "ac3".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 0, &prefs);
        assert_eq!(f.default_audio_track_id, 11);
    }

    #[test]
    fn test_codec_unknown_not_in_prefs_list() {
        // Neither aac nor mp3 is in prefs.audio_codec → both rank usize::MAX → lowest id wins.
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "aac"),
            make_track(11, TrackKind::Audio, "es", "mp3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["es".into()],
            audio_codec: vec!["truehd".into(), "eac3".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 0, &prefs);
        assert_eq!(f.default_audio_track_id, 10);
    }

    // ── PRIORIDAD DE IDIOMA ───────────────────────────────────────────────────

    #[test]
    fn test_lang_priority_l1_over_l2() {
        // "it" is L1; even though "es" has a better codec, L1 takes precedence.
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(5, TrackKind::Audio, "it", "aac"),
            make_track(10, TrackKind::Audio, "es", "eac3"),
            make_track(20, TrackKind::Audio, "en", "eac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["it".into(), "es".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.default_audio_track_id, 5);
    }

    #[test]
    fn test_lang_l1_absent_falls_to_l2() {
        // "ja" (L1) has no tracks → fall to "es" (L2).
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(10, TrackKind::Audio, "es", "eac3"),
            make_track(20, TrackKind::Audio, "en", "eac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["ja".into(), "es".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert_eq!(f.default_audio_track_id, 10);
    }

    // ── VÍDEO ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_all_video_tracks_always_included() {
        let tracks = vec![
            make_track(1, TrackKind::Video, "und", "h264"),
            make_track(2, TrackKind::Video, "und", "h265"),
            make_track(3, TrackKind::Video, "und", "av1"),
            make_track(10, TrackKind::Audio, "es", "eac3"),
            make_track(20, TrackKind::Audio, "en", "eac3"),
        ];
        let prefs = Prefs {
            audio_langs: vec!["es".into()],
            ..Prefs::default()
        };
        let f = resolve_filter(&tracks, "en", 20, &prefs);
        assert!(f.included_track_ids.contains(&1));
        assert!(f.included_track_ids.contains(&2));
        assert!(f.included_track_ids.contains(&3));
        assert!(f.included_track_ids.contains(&10));
    }

    // ── SUBTÍTULOS ────────────────────────────────────────────────────────────

    #[test]
    fn test_subs_filter_exact_match() {
        let subs = vec![
            make_sub("movie_s001_es_00_01.srt", "es"),
            make_sub("movie_s002_en_00_01.srt", "en"),
            make_sub("movie_s003_fr_00_01.srt", "fr"),
        ];
        let prefs = Prefs {
            sub_langs: vec!["es".into(), "en".into()],
            ..Prefs::default()
        };
        let result = resolve_subs_filter(&subs, &prefs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].language, "es");
        assert_eq!(result[1].language, "en");
    }

    #[test]
    fn test_subs_filter_fallback_when_no_match() {
        let subs = vec![
            make_sub("movie_s001_es_00_01.srt", "es"),
            make_sub("movie_s002_en_00_01.srt", "en"),
        ];
        let prefs = Prefs {
            sub_langs: vec!["ja".into()],
            ..Prefs::default()
        };
        let result = resolve_subs_filter(&subs, &prefs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_subs_filter_empty_langs_fallback() {
        let subs = vec![
            make_sub("movie_s001_es_00_01.srt", "es"),
            make_sub("movie_s002_en_00_01.srt", "en"),
        ];
        let prefs = Prefs {
            sub_langs: vec![],
            ..Prefs::default()
        };
        let result = resolve_subs_filter(&subs, &prefs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_subfile_from_filename_valid() {
        let sf = SubFile::from_filename("Pelicula_s005_es_00_01.srt").unwrap();
        assert_eq!(sf.language, "es");
        assert_eq!(sf.variant, "00");
    }

    #[test]
    fn test_subfile_from_filename_invalid() {
        assert!(SubFile::from_filename("Pelicula_v00_4k_00.h265").is_none());
    }

    // ── VALIDACIÓN DE PREFS ───────────────────────────────────────────────────

    #[test]
    fn test_prefs_too_many_audio_langs() {
        let prefs = Prefs {
            audio_langs: vec![
                "a".into(), "b".into(), "c".into(), "d".into(), "e".into(), "f".into(),
            ],
            ..Prefs::default()
        };
        let errs = validate_prefs(&prefs).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, PrefsValidationError::TooManyAudioLangs { got: 6 })));
    }

    #[test]
    fn test_prefs_invalid_resolution() {
        let prefs = Prefs {
            video_resolution: Some("8k".into()),
            ..Prefs::default()
        };
        let errs = validate_prefs(&prefs).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, PrefsValidationError::InvalidResolution(_))));
    }

    #[test]
    fn test_prefs_multiple_errors_collected() {
        let prefs = Prefs {
            audio_langs: vec![
                "a".into(), "b".into(), "c".into(), "d".into(), "e".into(), "f".into(),
            ],
            sub_langs: vec![
                "a".into(), "b".into(), "c".into(), "d".into(), "e".into(), "f".into(), "g".into(),
            ],
            ..Prefs::default()
        };
        let errs = validate_prefs(&prefs).unwrap_err();
        assert_eq!(errs.len(), 2);
    }
}
