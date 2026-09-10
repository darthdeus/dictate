//! `dictate doctor` dependency + storage + mic checks.
//! Ports the `doctor` block in `bin/dictate`.
//!
//! Each check returns `(ok, message)`; `render()` matches bash column output.
//! Pure parsers (`df`, `xset` mask) are unit-tested; process calls shell out
//! to the same binaries bash uses.

use anyhow::Result;

pub struct DoctorReport {
    /// `(ok, message)`. Warnings are `ok=true` with `warn` prefix in message
    /// (bash prints `warn` for non-fatal, `MISS` + rc=1 for fatal).
    pub lines: Vec<(bool, String)>,
    pub failed: bool,
}

impl DoctorReport {
    pub fn render(&self) -> String {
        self.lines
            .iter()
            .map(|(ok, msg)| {
                let tag = if msg.starts_with("warn ") {
                    "warn"
                } else if *ok {
                    "ok"
                } else {
                    "MISS"
                };
                let body = msg.strip_prefix("warn ").unwrap_or(msg);
                format!("  {tag}    {body}\n")
            })
            .collect()
    }
}

pub fn check_required_binaries() -> Result<DoctorReport> {
    let mut lines = vec![];
    let mut failed = false;
    for b in [
        "ffmpeg", "ffprobe", "xdotool", "pactl", "jq", "curl", "python3", "timeout", "flock",
        "pgrep", "pkill", "awk", "sha256sum",
    ] {
        let found = lookup_in_path(b);
        if !found {
            failed = true;
        }
        lines.push((found, b.to_string()));
    }
    Ok(DoctorReport { lines, failed })
}

/// Parse `df -Pk <dir>` second-line 4th field (avail KB).
/// Returns avail KB when the fs has >1GiB free (bash `ok` threshold).
pub fn parse_df_avail_kb(df_p_output: &str) -> Option<u64> {
    let mut rows = df_p_output.lines();
    rows.next()?; // header
    let row = rows.next()?;
    row.split_whitespace().nth(3)?.parse().ok()
}

pub fn df_has_space(df_p_output: &str) -> bool {
    parse_df_avail_kb(df_p_output).map(|kb| kb > 1024 * 1024).unwrap_or(false)
}

/// xset autorepeat mask probe (bash: 64-hex-char mask, LSB-first per byte).
/// `mask_hex` is the concatenated hex (no whitespace). Returns `Some(true)`
/// when autorepeat is ON for `keycode`.
pub fn autorepeat_enabled_for_key(mask_hex: &str, keycode: u32) -> Option<bool> {
    if mask_hex.len() != 64 || !mask_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte_index = (keycode / 8) as usize;
    let off = byte_index * 2;
    let byte = u8::from_str_radix(&mask_hex[off..off + 2], 16).ok()?;
    Some((byte >> (keycode % 8)) & 1 == 1)
}

/// Extract concatenated 16-hex runs from `xset q` output between the
/// `auto repeating keys` and `bell percent` markers (bash awk range).
pub fn extract_xset_mask(xset_output: &str) -> Option<String> {
    let mut in_range = false;
    let mut mask = String::new();
    for line in xset_output.lines() {
        if line.contains("auto repeating keys") {
            in_range = true;
        }
        if in_range {
            for token in line.split_whitespace() {
                if token.len() == 16 && token.chars().all(|c| c.is_ascii_hexdigit()) {
                    mask.push_str(token);
                }
            }
        }
        if in_range && line.contains("bell percent") {
            break;
        }
    }
    if mask.len() == 64 { Some(mask) } else { None }
}

pub fn lookup_in_path(bin: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(md) = std::fs::metadata(&candidate) {
                    if md.permissions().mode() & 0o111 != 0 {
                        return true;
                    }
                    continue;
                }
            }
            #[cfg(not(unix))]
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn df_space_threshold_is_1gib() {
        let ok = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 90000000 2000000 98% /\n";
        assert_eq!(parse_df_avail_kb(ok), Some(2000000));
        assert!(df_has_space(ok));
        let full = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 99500000 500000 99% /\n";
        assert!(!df_has_space(full));
        assert_eq!(parse_df_avail_kb("garbage"), None);
    }

    #[test]
    fn xset_mask_bit_math_matches_bash() {
        // keycode 197 -> byte 24, bit 5. Build mask with only that bit set.
        let mut bytes = [0u8; 32];
        bytes[24] = 1 << 5;
        let mask: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(autorepeat_enabled_for_key(&mask, 197), Some(true));
        assert_eq!(autorepeat_enabled_for_key(&mask, 196), Some(false));
        assert_eq!(autorepeat_enabled_for_key("short", 197), None);
    }

    #[test]
    fn xset_mask_extraction() {
        let out = "Keyboard Control:\n  auto repeating keys 0011223344556677 8899aabbccddeeff 0011223344556677 8899aabbccddeeff\n  bell percent 50\n";
        let mask = extract_xset_mask(out).unwrap();
        assert_eq!(mask.len(), 64);
        assert!(extract_xset_mask("no markers here").is_none());
        // Tokens outside the marker range are ignored.
        let outside = "0011223344556677\nKeyboard Control:\n  bell percent 50\n";
        assert!(extract_xset_mask(outside).is_none());
    }
}
