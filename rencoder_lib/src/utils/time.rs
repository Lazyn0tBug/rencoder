//! UTC timestamp formatting for record filenames and log prefixes.
//!
//! The CLI's default data path (`./data/encoding-<UTC-ISO8601>Z.parquet`)
//! is the primary consumer; centralizing the format here keeps
//! `data/store.rs` and the CLI's `default_data_path` consistent.

use chrono::Utc;

/// ISO 8601 UTC timestamp formatted for filesystem use (colons
/// replaced with `-` for macOS / Windows safety). Example:
/// `2026-09-08T16-53-00Z`.
pub fn utc_timestamp_filename() -> String {
    Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_iso8601_with_z_suffix() {
        let s = utc_timestamp_filename();
        // Length is 20 chars: YYYY-MM-DDTHH-MM-SSZ.
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
        // Dashes (not colons) for filesystem safety.
        assert!(!s.contains(':'));
    }
}
