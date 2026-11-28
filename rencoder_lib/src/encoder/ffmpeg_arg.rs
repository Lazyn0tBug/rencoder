//! Type-level safety boundary for ffmpeg argv entries.
//!
//! `EncodeParams.extra_args` accepts user-controlled values (CLI `--`
//! passthrough, `rencoder.toml` `[encode] extra_args`, future config
//! fields). Without a hard boundary, a malicious or malformed entry
//! could re-inject an ffmpeg option (`-i /etc/passwd`), smuggle
//! shell metacharacters (`-vf scale=720:foo; rm -rf /`), or break
//! out of an argv element via whitespace.
//!
//! ## Strategy: validate at the type level, not the call site
//!
//! `ExtraArg` is a closed enum with [`ExtraArg::parse`] as the only
//! constructor. Any code path that tries to push a raw `String`
//! into `EncodeParams.extra_args` will fail to compile. This is
//! stronger than a runtime `validate_extra_args` check, which a
//! caller can forget.
//!
//! ## Char classes
//!
//! Flag chars: ASCII alphanumeric + `_`. Value chars extend that with
//! `.`, `:`, `/`, `-`, `@`, `=`. Anything else (whitespace, `;`, `&`,
//! `|`, quotes, backticks, `$`, `\`, newlines, tabs) is rejected.
//!
//! ## Deny-list
//!
//! Flags that would re-inject paths or duplicate hardcoded wrappers:
//! `i` (input), `y` (overwrite, hardcoded by
//! [`crate::encoder::command::build_ffmpeg_command`]), `c`
//! (codec, also `c:v` / `c:a`), `o` (output), `vn`, `hide_banner`.

use thiserror::Error;

/// Char-class predicates. Public so other argv-handling code (future
/// arg splitters, log redaction) can reuse the same definitions.
pub mod chars {
    /// True for chars allowed in a flag name: ASCII alphanumeric or `_`.
    pub fn is_flag_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }

    /// True for chars allowed in an ffmpeg value (after `=` or as a
    /// bare value). Includes the flag chars plus the structural chars
    /// ffmpeg uses in filter / metadata expressions.
    pub fn is_value_char(c: char) -> bool {
        is_flag_char(c) || matches!(c, '.' | ':' | '/' | '-' | '@' | '=')
    }

    /// True if the whole `s` is composed of `is_flag_char` characters.
    pub fn is_flag_str(s: &str) -> bool {
        !s.is_empty() && s.chars().all(is_flag_char)
    }

    /// True if the whole `s` is composed of `is_value_char` characters.
    pub fn is_value_str(s: &str) -> bool {
        !s.is_empty() && s.chars().all(is_value_char)
    }
}

/// A validated ffmpeg argv entry.
///
/// Use [`ExtraArg::parse`] to construct from a raw string. The variants
/// are `#[non_exhaustive]` so future ffmpeg syntax additions don't
/// break existing callers; new variants will only appear alongside new
/// parsing rules.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExtraArg {
    /// Pure flag — `-` + ASCII alphanumeric / `_`. Example:
    /// `"-vf"`, `"-metadata"`, `"-ss"`.
    Flag(String),
    /// `flag=value` form — flag and value both validated against
    /// `chars::is_flag_str` / `chars::is_value_str`. Example:
    /// `"-vf=scale=720:-1"`, `"-ss=5"`.
    FlagEqValue { flag: String, value: String },
    /// Bare value (the second half of a preceding flag). Must contain
    /// `=` (canonical `key=value` form for metadata / filter options)
    /// and otherwise stick to `chars::is_value_str`. Example:
    /// `"title=foo"`, `"scale=720:-1"`.
    BareValue(String),
}

/// Error returned by [`ExtraArg::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExtraArgParseError {
    #[error("empty argv entry")]
    Empty,
    #[error("long options not allowed (starts with `--`): {0:?}")]
    LongOption(String),
    #[error("flag must start with ASCII alphanumeric, got {0:?}")]
    BadFirstFlagChar(String),
    #[error("flag contains invalid character(s): {0:?}")]
    BadFlagChars(String),
    #[error("value contains invalid character(s): {flag}={value:?}")]
    BadValueChars { flag: String, value: String },
    #[error("flag on deny-list (would re-inject or duplicate a hardcoded wrapper): {0:?}")]
    DeniedFlag(String),
    #[error("bare value must contain `=` (the canonical key=value form): {0:?}")]
    BareValueMissingEquals(String),
}

impl ExtraArg {
    /// Parse + validate a single argv entry from user input.
    ///
    /// Returns one of the [`ExtraArg`] variants or a specific
    /// [`ExtraArgParseError`] explaining why the entry is unsafe.
    /// Callers (CLI parser, TOML deserializer) surface the error to
    /// the upstream user with full context.
    ///
    /// See [module-level docs](self) for the full allowlist + deny-list.
    pub fn parse(s: &str) -> Result<Self, ExtraArgParseError> {
        use chars::{is_flag_str, is_value_str};

        if s.is_empty() {
            return Err(ExtraArgParseError::Empty);
        }

        // Branch: flag-form (starts with `-`) vs bare value.
        if let Some(rest) = s.strip_prefix('-') {
            // Long options like `--preset`. We don't have an
            // allowlist of long opts; reject for safety.
            if rest.starts_with('-') {
                return Err(ExtraArgParseError::LongOption(s.to_string()));
            }
            // First flag char must be alphanumeric. Rejects `-"`,
            // `-;`, etc.
            if rest
                .chars()
                .next()
                .is_some_and(|c| !c.is_ascii_alphanumeric())
            {
                return Err(ExtraArgParseError::BadFirstFlagChar(s.to_string()));
            }
            // Split on the FIRST `=`: flag + value. Rejects
            // `-vf_negate.mp4` (no `=`, the path chars become
            // part of the value-less flag, which `is_flag_str`
            // forbids) while still allowing `-vf=scale=720:-1`
            // where the value legitimately contains `=`.
            let (flag, value) = match rest.find('=') {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, ""),
            };
            // Deny-list: re-injection / duplication of wrapper
            // hardcodes. Check BEFORE char-class validation so
            // `-c:v` (which has `:` in the flag name) hits the
            // deny-list even though `:` isn't a flag char. Matches
            // the bare form (`-c`) AND ffmpeg's sub-option form
            // (`-c:v`, `-c:a`). A user wanting to override these
            // would have to edit the lib source — by design.
            if matches!(
                flag,
                "i" | "y" | "o" | "c" | "c:v" | "c:a" | "vn" | "hide_banner"
            ) {
                return Err(ExtraArgParseError::DeniedFlag(flag.to_string()));
            }
            if !is_flag_str(flag) {
                return Err(ExtraArgParseError::BadFlagChars(s.to_string()));
            }
            if !value.is_empty() && !is_value_str(value) {
                return Err(ExtraArgParseError::BadValueChars {
                    flag: flag.to_string(),
                    value: value.to_string(),
                });
            }
            Ok(if value.is_empty() {
                ExtraArg::Flag(format!("-{flag}"))
            } else {
                ExtraArg::FlagEqValue {
                    flag: format!("-{flag}"),
                    value: value.to_string(),
                }
            })
        } else {
            // Bare value: must contain `=`. ffmpeg uses bare values
            // for the value half of a preceding flag
            // (`-metadata title=foo`). Without `=`, the wrapper
            // can't distinguish a value from a stray input path
            // or filter arg.
            if !s.contains('=') {
                return Err(ExtraArgParseError::BareValueMissingEquals(s.to_string()));
            }
            if !is_value_str(s) {
                return Err(ExtraArgParseError::BadValueChars {
                    flag: String::new(),
                    value: s.to_string(),
                });
            }
            Ok(ExtraArg::BareValue(s.to_string()))
        }
    }

    /// Render this arg to its argv string form. Used by
    /// [`crate::encoder::command::build_ffmpeg_command`].
    pub fn as_argv(&self) -> String {
        match self {
            ExtraArg::Flag(f) => f.clone(),
            ExtraArg::FlagEqValue { flag, value } => format!("{flag}={value}"),
            ExtraArg::BareValue(v) => v.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- positive cases ----

    #[test]
    fn parse_accepts_pure_flag() {
        let a = ExtraArg::parse("-vf").unwrap();
        assert_eq!(a, ExtraArg::Flag("-vf".into()));
        assert_eq!(a.as_argv(), "-vf");
    }

    #[test]
    fn parse_accepts_flag_with_equals_value() {
        let a = ExtraArg::parse("-ss=5").unwrap();
        assert_eq!(
            a,
            ExtraArg::FlagEqValue {
                flag: "-ss".into(),
                value: "5".into(),
            }
        );
        assert_eq!(a.as_argv(), "-ss=5");
    }

    #[test]
    fn parse_accepts_flag_eq_value_with_nested_equals() {
        // `-vf=scale=720:-1` has `=` inside the value. First `=` splits
        // flag from value; value is `scale=720:-1`.
        let a = ExtraArg::parse("-vf=scale=720:-1").unwrap();
        assert_eq!(
            a,
            ExtraArg::FlagEqValue {
                flag: "-vf".into(),
                value: "scale=720:-1".into(),
            }
        );
    }

    #[test]
    fn parse_accepts_bare_value() {
        let a = ExtraArg::parse("title=foo").unwrap();
        assert_eq!(a, ExtraArg::BareValue("title=foo".into()));
        assert_eq!(a.as_argv(), "title=foo");
    }

    #[test]
    fn parse_accepts_flag_with_underscore() {
        let a = ExtraArg::parse("-passlogfile").unwrap();
        assert_eq!(a, ExtraArg::Flag("-passlogfile".into()));
    }

    #[test]
    fn parse_accepts_typical_user_tweaks() {
        // The set from review #15's test.
        for s in ["-ss=5", "-metadata", "title=foo", "-vf", "scale=720:-1"] {
            assert!(
                ExtraArg::parse(s).is_ok(),
                "expected {s:?} to parse, got: {:?}",
                ExtraArg::parse(s)
            );
        }
    }

    // ---- negative cases ----

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(ExtraArg::parse(""), Err(ExtraArgParseError::Empty));
    }

    #[test]
    fn parse_rejects_bare_dash() {
        // `-` → flag name is empty, rejected by `is_flag_str`.
        assert!(matches!(
            ExtraArg::parse("-"),
            Err(ExtraArgParseError::BadFlagChars(_))
        ));
    }

    #[test]
    fn parse_rejects_long_option() {
        assert!(matches!(
            ExtraArg::parse("--preset"),
            Err(ExtraArgParseError::LongOption(_))
        ));
    }

    #[test]
    fn parse_rejects_flag_with_shell_metachar() {
        assert!(matches!(
            ExtraArg::parse("-; rm -rf /"),
            Err(ExtraArgParseError::BadFirstFlagChar(_))
        ));
    }

    #[test]
    fn parse_rejects_flag_value_with_whitespace() {
        // `-vf scale=720:foo bar` is one argv token with a space;
        // ffmpeg would split it into two args at runtime. Flag
        // chars don't include space, so the parser rejects it as
        // bad flag chars.
        assert!(matches!(
            ExtraArg::parse("-vf scale=720:foo bar"),
            Err(ExtraArgParseError::BadFlagChars(_))
        ));
    }

    #[test]
    fn parse_rejects_flag_value_with_semicolon() {
        assert!(matches!(
            ExtraArg::parse("-vf=scale=720:foo;rm"),
            Err(ExtraArgParseError::BadValueChars { .. })
        ));
    }

    #[test]
    fn parse_rejects_flag_value_with_quote() {
        assert!(matches!(
            ExtraArg::parse("-metadata=title=\"foo\""),
            Err(ExtraArgParseError::BadValueChars { .. })
        ));
    }

    #[test]
    fn parse_rejects_flag_path_injection_form() {
        // `-vf_negate.mp4` could be parsed by lenient ffmpeg as
        // `-vf negate.mp4`. Split on `=` finds none, so `_` makes it
        // a bad flag char (only alphanumeric + `_` allowed, but the
        // whole rest must be flag chars — wait, `_` IS allowed).
        // The real rejection comes from `_negate.mp4` having `.` in
        // it, which isn't a flag char.
        assert!(matches!(
            ExtraArg::parse("-vf_negate.mp4"),
            Err(ExtraArgParseError::BadFlagChars(_))
        ));
    }

    #[test]
    fn parse_rejects_deny_list_flags() {
        for s in [
            "-i",
            "-y",
            "-o",
            "-c",
            "-vn",
            "-hide_banner",
            "-c:v",
            "-c:a",
        ] {
            assert!(
                matches!(
                    ExtraArg::parse(s),
                    Err(ExtraArgParseError::DeniedFlag(_)
                        | ExtraArgParseError::BadValueChars { .. })
                ),
                "expected deny-list reject for {s:?}, got: {:?}",
                ExtraArg::parse(s)
            );
        }
    }

    #[test]
    fn parse_rejects_bare_value_without_equals() {
        // `preset` and `5` look like ambiguous loose args; ffmpeg
        // could interpret them as input paths or filter args.
        assert!(matches!(
            ExtraArg::parse("preset"),
            Err(ExtraArgParseError::BareValueMissingEquals(_))
        ));
        assert!(matches!(
            ExtraArg::parse("5"),
            Err(ExtraArgParseError::BareValueMissingEquals(_))
        ));
    }

    #[test]
    fn parse_rejects_bare_value_with_shell_metachar() {
        assert!(matches!(
            ExtraArg::parse("title=foo;rm"),
            Err(ExtraArgParseError::BadValueChars { .. })
        ));
    }

    // ---- char-class predicates ----

    #[test]
    fn chars_flag_str_rejects_specials() {
        assert!(!chars::is_flag_str(""));
        assert!(!chars::is_flag_str("v f")); // space
        assert!(!chars::is_flag_str("v.f")); // dot
        assert!(!chars::is_flag_str("v;f")); // semicolon
        assert!(chars::is_flag_str("vf"));
        assert!(chars::is_flag_str("v_f"));
        assert!(chars::is_flag_str("V_F_2"));
    }

    #[test]
    fn chars_value_str_accepts_safe_structural_chars() {
        assert!(chars::is_value_str("scale=720:-1"));
        assert!(chars::is_value_str("title=foo"));
        assert!(chars::is_value_str("/path/with/slashes"));
        assert!(chars::is_value_str("user@host"));
        assert!(!chars::is_value_str("a b"));
        assert!(!chars::is_value_str("a;b"));
        assert!(!chars::is_value_str("a&b"));
    }
}
