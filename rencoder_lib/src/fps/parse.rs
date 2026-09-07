//! FPS value parsing.
//!
//! Accepts fractions ("30000/1001"), integers ("25"), and decimals ("29.97").
//! Mirrors `encode.zsh`'s `parse_fps_value` semantics.

use crate::error::EncodeError;

/// Parse a raw fps string into a `f64`.
///
/// Returns `EncodeError::FpsParse` on:
/// - empty input
/// - non-numeric input
/// - zero or negative result
/// - NaN / infinity
/// - zero denominator in `num/den` form
pub fn parse_fps_value(raw: &str) -> Result<f64, EncodeError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(EncodeError::FpsParse("empty fps string".into()));
    }

    if let Some((num, den)) = raw.split_once('/') {
        return parse_fraction(num.trim(), den.trim());
    }

    let value: f64 = raw
        .parse()
        .map_err(|e| EncodeError::FpsParse(format!("cannot parse fps `{raw}`: {e}")))?;
    validate_fps(value)
}

/// Parse `"<numerator>/<denominator>"` into a single `f64`.
///
/// Decisions (per user):
/// - both sides parsed as `f64` (rejects nonsense like `"29.97/1"` cleanly
///   only via `validate_fps`; the explicit types keep the parse path uniform)
/// - zero denominator is rejected explicitly with a specific message,
///   not deferred to `validate_fps` (which would surface `"invalid fps: inf"`)
/// - bad numerator and bad denominator produce distinct error messages
fn parse_fraction(num: &str, den: &str) -> Result<f64, EncodeError> {
    let n: f64 = num
        .parse()
        .map_err(|e| EncodeError::FpsParse(format!("bad numerator `{num}`: {e}")))?;
    let d: f64 = den
        .parse()
        .map_err(|e| EncodeError::FpsParse(format!("bad denominator `{den}`: {e}")))?;
    if d == 0.0 {
        return Err(EncodeError::FpsParse("zero denominator".into()));
    }
    validate_fps(n / d)
}

/// Guard: an fps value must be positive and finite.
fn validate_fps(value: f64) -> Result<f64, EncodeError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(EncodeError::FpsParse(format!("invalid fps: {value}")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn parses_fraction() {
        let v = parse_fps_value("30000/1001").unwrap();
        assert!(approx_eq(v, 29.97002997002997), "got {v}");
    }

    #[test]
    fn parses_integer() {
        assert_eq!(parse_fps_value("25").unwrap(), 25.0);
    }

    #[test]
    fn parses_decimal() {
        assert_eq!(parse_fps_value("29.97").unwrap(), 29.97);
    }

    #[test]
    fn parses_decimal_with_trailing_zero() {
        assert_eq!(parse_fps_value("30.0").unwrap(), 30.0);
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_fps_value("  30  ").unwrap(), 30.0);
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(parse_fps_value(""), Err(EncodeError::FpsParse(_))));
        assert!(matches!(parse_fps_value("   "), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_zero_fraction_numerator() {
        // "0/1" -> 0.0; validate_fps rejects <= 0.
        assert!(matches!(parse_fps_value("0/1"), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_zero_denominator() {
        assert!(matches!(parse_fps_value("30/0"), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_negative_integer() {
        assert!(matches!(parse_fps_value("-30"), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_negative_decimal() {
        assert!(matches!(parse_fps_value("-29.97"), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(parse_fps_value("abc"), Err(EncodeError::FpsParse(_))));
        assert!(matches!(parse_fps_value("30abc"), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_nan() {
        // `f64::from_str("nan")` parses successfully; validate_fps catches it.
        assert!(matches!(parse_fps_value("nan"), Err(EncodeError::FpsParse(_))));
    }

    #[test]
    fn rejects_infinity() {
        assert!(matches!(parse_fps_value("inf"), Err(EncodeError::FpsParse(_))));
    }
}
