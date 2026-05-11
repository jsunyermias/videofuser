/// binstruct inspect — human-readable dump of a binstruct EBML file.
use std::path::Path;

use anyhow::{Context, Result};
use videofuser_binstruct::{BinstructFile, CodecType};

pub fn run(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading binstruct file: {}", path.display()))?;
    let bf = BinstructFile::deserialize(&bytes)
        .map_err(|e| anyhow::anyhow!("deserializing binstruct: {}", e))?;

    let compressed = bf.header.config_flags & 0x01 != 0;
    println!("=== Header ===");
    println!("  version:      {}", bf.header.version);
    println!("  config_flags: 0x{:02X}", bf.header.config_flags);
    println!("  compressed:   {}", compressed);

    let hash_hex = hex_encode(&bf.source.original_mkv_hash);
    let ts_secs = bf.source.creation_timestamp / 1000;
    println!("\n=== Source ===");
    println!("  original_mkv_hash:       {}", hash_hex);
    println!("  publisher:               {}", bf.source.publisher_info);
    println!("  creation_timestamp:      {} ({})", bf.source.creation_timestamp, fmt_iso8601(ts_secs));
    println!("  original_language:       {}", bf.source.original_language);
    println!("  original_default_track:  {}", bf.source.original_default_track_id);
    println!("  build_tools:");
    for t in &bf.source.build_tools {
        println!("    {}: {}", t.name, t.version);
    }

    println!("\n=== MkvSkeleton ===");
    println!("  pre_tracks_blob:   {} bytes", bf.mkv_skeleton.pre_tracks_blob.len());
    println!("  track_entries:     {} entries", bf.mkv_skeleton.track_entries.len());
    for te in &bf.mkv_skeleton.track_entries {
        println!("    track_id={} ({} bytes)", te.track_id, te.bytes.len());
    }
    println!("  post_tracks_blob:  {} bytes", bf.mkv_skeleton.post_tracks_blob.len());

    let first_deltas: Vec<String> = bf
        .cluster_timestamps
        .deltas
        .iter()
        .take(5)
        .map(|d| d.to_string())
        .collect();
    println!("\n=== ClusterTimestamps ===");
    println!("  cluster_count: {}", bf.cluster_timestamps.cluster_count);
    println!("  first 5 deltas: [{}]", first_deltas.join(", "));

    println!("\n=== TrackPolicies ===");
    println!(
        "  {:<10} {:<8} {:<6} {:<6} {:<12} {:<14} {:<10} {:<10}",
        "track_id", "type", "lang", "vbr", "frame_count", "cbr_frame_sz", "raw_hash", "vfr_hash"
    );
    println!("  {}", "-".repeat(86));
    for p in &bf.track_policies {
        let kind = match p.codec_type {
            CodecType::Video => "video",
            CodecType::Audio => "audio",
        };
        let cbr = p.cbr_frame_size
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        let raw_short = hex_encode(&p.raw_file_hash)[..8].to_string();
        let vfr_short = p.vfr_file_hash
            .map(|h| hex_encode(&h)[..8].to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<10} {:<8} {:<6} {:<6} {:<12} {:<14} {:<10} {:<10}",
            p.track_id, kind, p.language_code, p.is_vbr, p.frame_count, cbr, raw_short, vfr_short
        );
    }

    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn fmt_iso8601(unix_secs: u64) -> String {
    // Minimal ISO-8601-like formatting without external deps
    let mut remaining = unix_secs;
    let secs = remaining % 60; remaining /= 60;
    let mins = remaining % 60; remaining /= 60;
    let hours = remaining % 24; remaining /= 24;

    // Days since Unix epoch → calendar date (Gregorian)
    let mut days = remaining as i64;
    let mut year: i64 = 1970;
    loop {
        let dy = days_in_year(year);
        if days < dy { break; }
        days -= dy;
        year += 1;
    }
    let mut month = 1i64;
    loop {
        let dm = days_in_month(year, month);
        if days < dm { break; }
        days -= dm;
        month += 1;
    }
    let day = days + 1;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, mins, secs
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_year(y: i64) -> i64 {
    if is_leap(y) { 366 } else { 365 }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 30,
    }
}
