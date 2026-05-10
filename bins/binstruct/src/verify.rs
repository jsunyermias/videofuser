/// binstruct verify — verify integrity of a binstruct against torrent files.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use videofuser_binstruct::{BinstructFile, CodecType};

use crate::gen::sha256_file;

pub fn run(binstruct_path: &Path, torrent_root: &Path) -> Result<()> {
    let bytes = std::fs::read(binstruct_path)
        .with_context(|| format!("reading binstruct: {}", binstruct_path.display()))?;
    let bf = BinstructFile::deserialize(&bytes)
        .map_err(|e| anyhow::anyhow!("deserializing binstruct: {}", e))?;

    let mut errors: Vec<String> = Vec::new();

    for policy in &bf.track_policies {
        // Locate raw file
        let raw_path = match policy.codec_type {
            CodecType::Video => find_raw_video(torrent_root, policy.track_id),
            CodecType::Audio => find_raw_audio(torrent_root, policy.track_id),
        };

        match raw_path {
            Err(e) => {
                errors.push(format!("track {}: {}", policy.track_id, e));
                continue;
            }
            Ok(ref p) => {
                // SHA-256 of raw file
                match sha256_file(p) {
                    Err(e) => {
                        errors.push(format!("track {}: SHA-256 error on {}: {}", policy.track_id, p.display(), e));
                    }
                    Ok(hash) => {
                        if hash != policy.raw_file_hash {
                            errors.push(format!(
                                "track {}: raw file hash mismatch ({})",
                                policy.track_id,
                                p.display()
                            ));
                        }
                    }
                }

                // CBR size check
                if !policy.is_vbr {
                    if let Some(cbr_size) = policy.cbr_frame_size {
                        let expected = policy.frame_count * cbr_size;
                        match p.metadata() {
                            Ok(meta) => {
                                if meta.len() != expected {
                                    errors.push(format!(
                                        "track {}: CBR size mismatch: file={} expected={}",
                                        policy.track_id,
                                        meta.len(),
                                        expected
                                    ));
                                }
                            }
                            Err(e) => {
                                errors.push(format!("track {}: stat error: {}", policy.track_id, e));
                            }
                        }
                    }
                }
            }
        }

        // VFR hash check
        if let Some(expected_vfr_hash) = policy.vfr_file_hash {
            let vfr_path = if matches!(policy.codec_type, CodecType::Video) {
                find_vfr_video(torrent_root, policy.track_id)
            } else {
                find_vfr_audio(torrent_root, policy.track_id)
            };
            match vfr_path {
                Err(e) => {
                    errors.push(format!("track {}: VFR not found: {}", policy.track_id, e));
                }
                Ok(vp) => match sha256_file(&vp) {
                    Err(e) => {
                        errors.push(format!("track {}: VFR SHA-256 error: {}", policy.track_id, e));
                    }
                    Ok(hash) => {
                        if hash != expected_vfr_hash {
                            errors.push(format!(
                                "track {}: VFR hash mismatch ({})",
                                policy.track_id,
                                vp.display()
                            ));
                        }
                    }
                },
            }
        }
    }

    if errors.is_empty() {
        println!("OK");
        Ok(())
    } else {
        for e in &errors {
            eprintln!("ERROR: {}", e);
        }
        std::process::exit(1);
    }
}

// ─── File locators (same logic as gen) ────────────────────────────────────────

fn find_raw_video(root: &Path, track_id: u64) -> Result<PathBuf> {
    let pattern = format!("_v{:02}_", track_id);
    find_in_dir(&root.join("video"), &pattern)
}

fn find_raw_audio(root: &Path, track_id: u64) -> Result<PathBuf> {
    let pattern = format!("_a{:03}_", track_id);
    find_in_dir(&root.join("audio"), &pattern)
}

fn find_vfr_video(root: &Path, track_id: u64) -> Result<PathBuf> {
    let pattern = format!("_v{:02}_", track_id);
    find_in_dir_vfr(&root.join("info").join("vfr"), &pattern)
}

fn find_vfr_audio(root: &Path, track_id: u64) -> Result<PathBuf> {
    let pattern = format!("_a{:03}_", track_id);
    find_in_dir_vfr(&root.join("info").join("vfr"), &pattern)
}

fn find_in_dir(dir: &Path, pattern: &str) -> Result<PathBuf> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading directory: {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().contains(pattern) {
            return Ok(entry.path());
        }
    }
    anyhow::bail!("no file matching '{}' in {}", pattern, dir.display())
}

fn find_in_dir_vfr(dir: &Path, pattern: &str) -> Result<PathBuf> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading VFR directory: {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(pattern) && (name.ends_with(".vfr") || name.contains(".vfr.")) {
            return Ok(entry.path());
        }
    }
    anyhow::bail!("no VFR file matching '{}' in {}", pattern, dir.display())
}
