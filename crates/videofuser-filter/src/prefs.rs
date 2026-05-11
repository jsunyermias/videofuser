#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    /// Up to 5 preferred audio languages in priority order.
    /// Empty = fallback to all tracks (equivalent to disabling the filter).
    pub audio_langs: Vec<String>,

    /// Up to 5 preferred subtitle languages, independent of audio.
    pub sub_langs: Vec<String>,

    /// Preferred audio codecs in priority order.
    pub audio_codec: Vec<String>,

    /// Preferred video resolution. Used to prioritize which video track to
    /// download. Does not filter other tracks.
    pub video_resolution: Option<String>,

    pub read_mode: ReadMode,
    pub read_timeout_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadMode {
    Block,
    Timeout,
    NonBlock,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            audio_langs: vec![],
            sub_langs: vec![],
            audio_codec: vec![
                "truehd".into(),
                "eac3".into(),
                "dts".into(),
                "ac3".into(),
                "aac".into(),
                "mp3".into(),
            ],
            video_resolution: Some("1080p".into()),
            read_mode: ReadMode::Block,
            read_timeout_ms: 1000,
        }
    }
}
