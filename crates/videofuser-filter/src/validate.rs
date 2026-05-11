use crate::prefs::Prefs;

const VALID_RESOLUTIONS: &[&str] =
    &["4k", "1440p", "1080p", "720p", "540p", "480p", "360p", "240p"];

#[derive(Debug)]
pub enum PrefsValidationError {
    TooManyAudioLangs { got: usize },
    TooManySubLangs { got: usize },
    InvalidReadMode(String),
    InvalidResolution(String),
}

/// Validates `prefs` and returns all errors found (not just the first one).
pub fn validate_prefs(prefs: &Prefs) -> Result<(), Vec<PrefsValidationError>> {
    let mut errors = Vec::new();

    if prefs.audio_langs.len() > 5 {
        errors.push(PrefsValidationError::TooManyAudioLangs {
            got: prefs.audio_langs.len(),
        });
    }

    if prefs.sub_langs.len() > 5 {
        errors.push(PrefsValidationError::TooManySubLangs {
            got: prefs.sub_langs.len(),
        });
    }

    if let Some(ref res) = prefs.video_resolution {
        if !VALID_RESOLUTIONS.contains(&res.as_str()) {
            errors.push(PrefsValidationError::InvalidResolution(res.clone()));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
