/// binstruct verify-manifest — cross-check the four manifest formats for consistency.
use std::path::Path;

use anyhow::{Context, Result};
use videofuser_manifest::{parse_ebml, parse_md, parse_nfo, parse_xml, Manifest, TrackKind};

pub fn run(info_dir: &Path) -> Result<()> {
    // Detect the four manifest files by extension
    let ebml_path = find_manifest(info_dir, ".ebml");
    let xml_path = find_manifest(info_dir, ".xml");
    let nfo_path = find_manifest(info_dir, ".nfo");
    let md_path = find_manifest(info_dir, ".md");

    // Parse the canonical .ebml
    let ebml_path = ebml_path.ok_or_else(|| {
        anyhow::anyhow!("no manifest.ebml found in {}", info_dir.display())
    })?;
    let ebml_bytes = std::fs::read(&ebml_path)
        .with_context(|| format!("reading {}", ebml_path.display()))?;
    let truth = parse_ebml(&ebml_bytes)
        .map_err(|e| anyhow::anyhow!("parsing manifest.ebml: {}", e))?;

    let mut errors: Vec<String> = Vec::new();

    // Compare XML
    if let Some(ref path) = xml_path {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        match parse_xml(&text) {
            Ok(m) => compare(&truth, &m, "xml", &mut errors),
            Err(e) => eprintln!("warning: xml: parse error: {}", e),
        }
    } else {
        eprintln!("warning: no manifest.xml found");
    }

    // Compare NFO
    if let Some(ref path) = nfo_path {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        match parse_nfo(&text) {
            Ok(m) => compare_best_effort(&truth, &m, "nfo", &mut errors),
            Err(e) => eprintln!("warning: nfo: parse error: {}", e),
        }
    } else {
        eprintln!("warning: no manifest.nfo found");
    }

    // Compare MD
    if let Some(ref path) = md_path {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        match parse_md(&text) {
            Ok(m) => compare_best_effort(&truth, &m, "md", &mut errors),
            Err(e) => eprintln!("warning: md: parse error: {}", e),
        }
    } else {
        eprintln!("warning: no manifest.md found");
    }

    if errors.is_empty() {
        std::process::exit(0);
    } else {
        for e in &errors {
            eprintln!("{}", e);
        }
        std::process::exit(1);
    }
}

fn find_manifest(dir: &Path, ext: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        // Must have the right extension and contain "manifest" in the name
        if p.to_string_lossy().ends_with(ext)
            && p.file_name()
                .map(|n| n.to_string_lossy().contains("manifest"))
                .unwrap_or(false)
        {
            Some(p)
        } else {
            None
        }
    })
}

/// Full comparison (for XML which carries all fields).
fn compare(truth: &Manifest, other: &Manifest, fmt: &str, errors: &mut Vec<String>) {
    if !eq_ignore_case(&truth.title, &other.title) {
        errors.push(format!(
            "format {fmt}: title differs: ebml='{}', got='{}'",
            truth.title, other.title
        ));
    }
    if truth.year != other.year && other.year != 0 {
        errors.push(format!(
            "format {fmt}: year differs: ebml={}, got={}",
            truth.year, other.year
        ));
    }
    if !other.original_language.is_empty()
        && truth.original_language != other.original_language
    {
        errors.push(format!(
            "format {fmt}: original_language differs: ebml='{}', got='{}'",
            truth.original_language, other.original_language
        ));
    }
    if other.original_mkv_hash != [0u8; 32]
        && truth.original_mkv_hash != other.original_mkv_hash
    {
        errors.push(format!(
            "format {fmt}: original_mkv_hash differs"
        ));
    }

    // Track comparison (by track_id)
    for tt in &truth.tracks {
        if let Some(ot) = other.tracks.iter().find(|t| t.track_id == tt.track_id) {
            if tt.kind != ot.kind {
                errors.push(format!(
                    "format {fmt}: track {}: kind differs: ebml={:?}, got={:?}",
                    tt.track_id, tt.kind, ot.kind
                ));
            }
            if !ot.codec.is_empty() && !eq_ignore_case(&tt.codec, &ot.codec) {
                errors.push(format!(
                    "format {fmt}: track {}: codec differs: ebml='{}', got='{}'",
                    tt.track_id, tt.codec, ot.codec
                ));
            }
            if tt.language != ot.language && ot.language.is_some() {
                errors.push(format!(
                    "format {fmt}: track {}: language differs: ebml='{:?}', got='{:?}'",
                    tt.track_id, tt.language, ot.language
                ));
            }
            if tt.variant != ot.variant && ot.variant.is_some() {
                errors.push(format!(
                    "format {fmt}: track {}: variant differs: ebml='{:?}', got='{:?}'",
                    tt.track_id, tt.variant, ot.variant
                ));
            }
        } else if !other.tracks.is_empty() {
            errors.push(format!(
                "format {fmt}: track {} missing from format",
                tt.track_id
            ));
        }
    }

    // Variants legend
    for ve in &truth.variants_legend {
        if let Some(ove) = other.variants_legend.iter().find(|v| v.id == ve.id) {
            if !eq_ignore_case(&ve.description, &ove.description) {
                errors.push(format!(
                    "format {fmt}: variant '{}' description differs: ebml='{}', got='{}'",
                    ve.id, ve.description, ove.description
                ));
            }
        } else if !other.variants_legend.is_empty() {
            errors.push(format!(
                "format {fmt}: variant '{}' missing",
                ve.id
            ));
        }
    }
}

/// Best-effort comparison for NFO/MD (only non-empty/non-zero fields are compared).
fn compare_best_effort(truth: &Manifest, other: &Manifest, fmt: &str, errors: &mut Vec<String>) {
    if !other.title.is_empty() && !eq_ignore_case(&truth.title, &other.title) {
        errors.push(format!(
            "format {fmt}: title differs: ebml='{}', got='{}'",
            truth.title, other.title
        ));
    }
    if other.year != 0 && truth.year != other.year {
        errors.push(format!(
            "format {fmt}: year differs: ebml={}, got={}",
            truth.year, other.year
        ));
    }
    if !other.original_language.is_empty()
        && truth.original_language != other.original_language
    {
        errors.push(format!(
            "format {fmt}: original_language differs: ebml='{}', got='{}'",
            truth.original_language, other.original_language
        ));
    }
    if other.original_mkv_hash != [0u8; 32]
        && truth.original_mkv_hash != other.original_mkv_hash
    {
        errors.push(format!("format {fmt}: original_mkv_hash differs"));
    }
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.to_ascii_lowercase() == b.to_ascii_lowercase()
}
