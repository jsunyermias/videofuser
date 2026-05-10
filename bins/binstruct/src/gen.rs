/// binstruct gen — generate a binstruct EBML file from an intermediate MKV.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use videofuser_binstruct::{
    BinstructFile, ClusterTimestamps, CodecType, Header, MkvSkeleton, Source, ToolEntry,
    TrackEntryRecord, TrackPolicy,
};
use videofuser_vfr::VfrFile;

use crate::mkv;

// ─── CBR sidecar JSON schema ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CbrMeta {
    frame_count: u64,
    cbr_frame_size: u64,
    file_size_bytes: u64,
    #[serde(default)]
    frame_duration_ns: u64,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

pub struct GenArgs {
    pub mkv: PathBuf,
    pub torrent_root: PathBuf,
    pub publisher: String,
    pub compress: bool,
    pub output: PathBuf,
}

pub fn run(args: GenArgs) -> Result<()> {
    // ── PASO 1: parse MKV ────────────────────────────────────────────────────
    eprintln!("[gen] parsing MKV: {}", args.mkv.display());
    let parsed = mkv::parse_mkv(&args.mkv)
        .with_context(|| format!("parsing MKV: {}", args.mkv.display()))?;

    // ── PASO 2: validaciones obligatorias ────────────────────────────────────

    // V1: exactly one audio TrackEntry with FlagDefault=1
    let default_audio: Vec<_> = parsed
        .track_entries
        .iter()
        .filter(|t| t.track_type == 2 && t.flag_default == 1)
        .collect();
    match default_audio.len() {
        0 => {
            eprintln!("error: no audio track has FlagDefault=1; exactly one required");
            std::process::exit(6);
        }
        1 => {}
        n => {
            eprintln!("error: {} audio tracks have FlagDefault=1; exactly one required", n);
            std::process::exit(6);
        }
    }
    let default_track = default_audio[0];

    // V2: default audio track has valid ISO 639 language code
    let lang = default_track.language.trim();
    if lang.is_empty() || lang == "und" || !is_valid_iso639(lang) {
        eprintln!(
            "error: default audio track (id={}) has invalid/empty language '{}'; \
             must be a valid ISO 639 code",
            default_track.track_id, lang
        );
        std::process::exit(6);
    }
    let original_language = lang.to_string();
    let original_default_track_id = default_track.track_id;

    // V3–V5: for each video/audio track, validate files exist
    let mut track_raw_paths: Vec<(u64, PathBuf)> = Vec::new();
    let mut track_vfr_paths: Vec<(u64, Option<PathBuf>)> = Vec::new();

    for entry in &parsed.track_entries {
        match entry.track_type {
            1 => {
                // Video
                let raw = find_raw_video(&args.torrent_root, entry.track_id)?;
                track_raw_paths.push((entry.track_id, raw));

                // V4: VFR always required for video
                let vfr = find_vfr(&args.torrent_root, entry.track_id, true)?;
                track_vfr_paths.push((entry.track_id, Some(vfr)));
            }
            2 => {
                // Audio
                let raw = find_raw_audio(&args.torrent_root, entry.track_id)?;

                // V4: VFR optional for audio (present → VBR, absent → CBR)
                let vfr = find_vfr_optional(&args.torrent_root, entry.track_id, false);
                let is_vbr = vfr.is_some();

                if !is_vbr {
                    // V5: CBR consistency check
                    let cbr_path = cbr_json_path(&raw);
                    if !cbr_path.exists() {
                        eprintln!(
                            "error: audio track {} is CBR but no sidecar JSON found at {}",
                            entry.track_id,
                            cbr_path.display()
                        );
                        std::process::exit(6);
                    }
                    let meta = read_cbr_json(&cbr_path)?;
                    let expected_size = meta.frame_count * meta.cbr_frame_size;
                    // Check sidecar JSON consistency
                    if meta.file_size_bytes != expected_size {
                        eprintln!(
                            "error: CBR consistency check failed for audio track {}: \
                             file_size_bytes={} != frame_count({}) × cbr_frame_size({}) = {}",
                            entry.track_id,
                            meta.file_size_bytes,
                            meta.frame_count,
                            meta.cbr_frame_size,
                            expected_size
                        );
                        std::process::exit(6);
                    }
                    // Also verify actual on-disk file size matches
                    let actual_size = fs::metadata(&raw)
                        .with_context(|| format!("stat raw file: {}", raw.display()))?
                        .len();
                    if actual_size != expected_size {
                        eprintln!(
                            "error: CBR consistency check failed for audio track {}: \
                             actual file size {} != frame_count({}) × cbr_frame_size({}) = {}",
                            entry.track_id,
                            actual_size,
                            meta.frame_count,
                            meta.cbr_frame_size,
                            expected_size
                        );
                        std::process::exit(6);
                    }
                }

                track_raw_paths.push((entry.track_id, raw));
                track_vfr_paths.push((entry.track_id, vfr));
            }
            _ => {} // subtitles: skip
        }
    }

    // V6: capture tool versions
    let build_tools = capture_tool_versions();

    // ── PASO 3: build TrackPolicies ──────────────────────────────────────────
    let mut track_policies: Vec<TrackPolicy> = Vec::new();

    for entry in &parsed.track_entries {
        if entry.track_type != 1 && entry.track_type != 2 {
            continue;
        }

        let raw_path = track_raw_paths
            .iter()
            .find(|(id, _)| *id == entry.track_id)
            .map(|(_, p)| p.as_path())
            .unwrap();

        let vfr_path = track_vfr_paths
            .iter()
            .find(|(id, _)| *id == entry.track_id)
            .and_then(|(_, p)| p.as_deref());

        let is_vbr = vfr_path.is_some();

        let (frame_count, frame_duration, cbr_frame_size) = if is_vbr {
            let vfr = read_vfr(vfr_path.unwrap())?;
            (vfr.frames.len() as u64, 0u64, None)
        } else {
            let cbr_path = cbr_json_path(raw_path);
            let meta = read_cbr_json(&cbr_path)?;
            (meta.frame_count, meta.frame_duration_ns, Some(meta.cbr_frame_size))
        };

        let raw_file_hash = sha256_file(raw_path)?;
        let vfr_file_hash = if let Some(vp) = vfr_path {
            Some(sha256_file(vp)?)
        } else {
            None
        };

        let codec_type = if entry.track_type == 1 {
            CodecType::Video
        } else {
            CodecType::Audio
        };

        let language_code = if entry.language.is_empty() {
            "und".to_string()
        } else {
            entry.language.clone()
        };

        track_policies.push(TrackPolicy {
            track_id: entry.track_id,
            codec_type,
            language_code,
            frame_count,
            is_vbr,
            frame_duration,
            cbr_frame_size,
            raw_file_hash,
            vfr_file_hash,
        });
    }

    // ── PASO 4: assemble and serialize ───────────────────────────────────────
    let original_mkv_hash = sha256_file(&args.mkv)?;
    let creation_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Encode cluster timestamps as deltas
    let cluster_count = parsed.cluster_timestamps.len() as u64;
    let deltas = timestamps_to_deltas(&parsed.cluster_timestamps);

    let mkv_skeleton = MkvSkeleton {
        pre_tracks_blob: parsed.pre_tracks_blob,
        track_entries: parsed
            .track_entries
            .into_iter()
            .map(|e| TrackEntryRecord {
                track_id: e.track_id,
                bytes: e.raw_bytes,
            })
            .collect(),
        post_tracks_blob: parsed.post_tracks_blob,
    };

    let binstruct = BinstructFile {
        header: Header {
            version: 1,
            config_flags: if args.compress { 0x01 } else { 0x00 },
        },
        source: Source {
            original_mkv_hash,
            publisher_info: args.publisher,
            creation_timestamp,
            original_language,
            original_default_track_id,
            build_tools,
        },
        mkv_skeleton,
        cluster_timestamps: ClusterTimestamps {
            cluster_count,
            deltas,
        },
        track_policies,
    };

    eprintln!("[gen] serializing binstruct");
    let bytes = binstruct
        .serialize()
        .context("serializing BinstructFile")?;

    fs::write(&args.output, &bytes)
        .with_context(|| format!("writing output: {}", args.output.display()))?;

    eprintln!(
        "[gen] wrote {} bytes to {}",
        bytes.len(),
        args.output.display()
    );
    Ok(())
}

// ─── File search helpers ──────────────────────────────────────────────────────

fn find_raw_video(root: &Path, track_id: u64) -> Result<PathBuf> {
    let pattern = format!("_v{:02}_", track_id);
    find_in_dir(&root.join("video"), &pattern).with_context(|| {
        format!(
            "no raw video file for track {} in {}/video/ (pattern: *{}*)",
            track_id,
            root.display(),
            pattern
        )
    })
}

fn find_raw_audio(root: &Path, track_id: u64) -> Result<PathBuf> {
    let pattern = format!("_a{:03}_", track_id);
    find_in_dir(&root.join("audio"), &pattern).with_context(|| {
        format!(
            "no raw audio file for track {} in {}/audio/ (pattern: *{}*)",
            track_id,
            root.display(),
            pattern
        )
    })
}

fn find_vfr(root: &Path, track_id: u64, is_video: bool) -> Result<PathBuf> {
    let vfr_dir = root.join("info").join("vfr");
    let pattern = if is_video {
        format!("_v{:02}_", track_id)
    } else {
        format!("_a{:03}_", track_id)
    };
    find_in_dir_vfr(&vfr_dir, &pattern).with_context(|| {
        format!(
            "no VFR file for track {} in {} (pattern: *{}*.vfr*)",
            track_id,
            vfr_dir.display(),
            pattern
        )
    })
}

fn find_vfr_optional(root: &Path, track_id: u64, is_video: bool) -> Option<PathBuf> {
    find_vfr(root, track_id, is_video).ok()
}

fn find_in_dir(dir: &Path, pattern: &str) -> Result<PathBuf> {
    if !dir.exists() {
        bail!("directory does not exist: {}", dir.display());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(pattern) {
            return Ok(entry.path());
        }
    }
    bail!("no file matching pattern '{}' in {}", pattern, dir.display())
}

fn find_in_dir_vfr(dir: &Path, pattern: &str) -> Result<PathBuf> {
    if !dir.exists() {
        bail!("VFR directory does not exist: {}", dir.display());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(pattern) && (name.ends_with(".vfr") || name.contains(".vfr.")) {
            return Ok(entry.path());
        }
    }
    bail!("no VFR file matching pattern '{}' in {}", pattern, dir.display())
}

fn cbr_json_path(raw: &Path) -> PathBuf {
    let mut p = raw.to_path_buf();
    let ext = p
        .extension()
        .map(|e| format!("{}.cbr.json", e.to_string_lossy()))
        .unwrap_or_else(|| "cbr.json".to_string());
    p.set_extension(ext);
    p
}

fn read_cbr_json(path: &Path) -> Result<CbrMeta> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading CBR JSON: {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing CBR JSON: {}", path.display()))
}

fn read_vfr(path: &Path) -> Result<VfrFile> {
    let bytes = fs::read(path)?;
    // Detect zstd compression
    let data = if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        zstd::decode_all(bytes.as_slice())?
    } else {
        bytes
    };
    VfrFile::read_from(&mut data.as_slice()).map_err(|e| anyhow::anyhow!("{}", e))
}

// ─── SHA-256 ─────────────────────────────────────────────────────────────────

pub fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    use std::io::Read;
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening for SHA-256: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

// ─── Tool version capture ─────────────────────────────────────────────────────

fn capture_tool_versions() -> Vec<ToolEntry> {
    let tools = [("ffmpeg", &["ffmpeg", "-version"] as &[&str]),
                 ("mkvmerge", &["mkvmerge", "--version"]),
                 ("mkvextract", &["mkvextract", "--version"])];

    tools
        .iter()
        .map(|(name, argv)| {
            let version = run_version_cmd(argv[0], &argv[1..]);
            ToolEntry {
                name: name.to_string(),
                version,
            }
        })
        .collect()
}

fn run_version_cmd(prog: &str, args: &[&str]) -> String {
    match Command::new(prog).args(args).output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.lines().next().unwrap_or("").trim().to_string()
        }
        Err(_) => {
            eprintln!("warning: '{}' not found in PATH; version recorded as 'not found'", prog);
            "not found".to_string()
        }
    }
}

// ─── Timestamp deltas ─────────────────────────────────────────────────────────

fn timestamps_to_deltas(ts: &[u64]) -> Vec<i64> {
    if ts.is_empty() {
        return Vec::new();
    }
    let mut deltas = Vec::with_capacity(ts.len());
    deltas.push(ts[0] as i64);
    for w in ts.windows(2) {
        deltas.push(w[1] as i64 - w[0] as i64);
    }
    deltas
}

// ─── ISO 639 validation (lightweight) ────────────────────────────────────────

fn is_valid_iso639(code: &str) -> bool {
    // Accept any 2-3 character ASCII alphabetic code that isn't empty or "und"
    let len = code.len();
    (len == 2 || len == 3) && code.bytes().all(|b| b.is_ascii_alphabetic())
}
