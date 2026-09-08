//! Rust wrapper around the C probe bridge.
//!
//! Only compiled when the `c-probe` feature is enabled. Each call to
//! `probe_video_metadata_fast` invokes the C bridge in a fresh
//! `AVFormatContext`, so concurrent calls from rayon are safe (R17).

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

use crate::error::EncodeError;
use crate::fps::parse::parse_fps_value;
use crate::types::VideoMetadata;

/// C-side result struct; field order and types must match `video_probe.c`.
#[repr(C)]
struct ProbeResult {
    codec_name: *mut c_char,
    avg_frame_rate: *mut c_char,
    r_frame_rate: *mut c_char,
    duration: f64,
    container: *mut c_char,
    width: i32,
    height: i32,
    bit_rate: i64,
}

extern "C" {
    fn rencoder_probe(path: *const c_char) -> *mut ProbeResult;
    fn rencoder_free(result: *mut ProbeResult);
}

/// RAII guard that calls `rencoder_free` on drop. Ensures the C-side
/// malloc'd struct is released even if the read block panics.
struct ProbeGuard(*mut ProbeResult);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `rencoder_free` accepts the null pointer as a no-op
            // and otherwise frees the malloc'd struct + its strings.
            unsafe { rencoder_free(self.0) };
        }
    }
}

/// Probe a video file via libavformat. Returns None fields on probe
/// failure (matching the CLI probe's behavior).
pub fn probe_video_metadata_fast(input: &Path) -> Result<VideoMetadata, EncodeError> {
    let path_c = CString::new(input.to_string_lossy().into_owned())
        .map_err(|e| EncodeError::Probe(format!("path contains NUL: {e}")))?;
    let raw = unsafe { rencoder_probe(path_c.as_ptr()) };
    if raw.is_null() {
        return Err(EncodeError::Probe("c-probe: calloc failed".into()));
    }
    let _guard = ProbeGuard(raw); // rencoder_free runs on scope exit (incl. panic).

    // SAFETY: `raw` is a non-null pointer to a ProbeResult that the C
    // bridge allocated via calloc. Fields are either malloc'd CStrings or
    // a double (or NULL for missing data). We read each field into owned
    // Rust data before the guard frees the C memory.
    let result = unsafe { &*raw };

    let codec = read_cstr(result.codec_name);
    let avg_fps = read_cstr(result.avg_frame_rate)
        .and_then(|s| parse_fps_value(&s).ok());
    let r_frame_rate = read_cstr(result.r_frame_rate)
        .and_then(|s| parse_fps_value(&s).ok());
    let duration = if result.duration.is_nan() {
        None
    } else {
        Some(result.duration)
    };
    let container = read_cstr(result.container);
    let width = if result.width > 0 {
        Some(result.width as u32)
    } else {
        None
    };
    let height = if result.height > 0 {
        Some(result.height as u32)
    } else {
        None
    };
    let bit_rate = if result.bit_rate > 0 {
        Some(result.bit_rate as u64)
    } else {
        None
    };

    Ok(VideoMetadata {
        codec,
        avg_fps: avg_fps.or(r_frame_rate),
        nominal_fps: r_frame_rate,
        duration_secs: duration,
        container,
        width,
        height,
        bit_rate,
    })
}

/// Read a malloc'd C string into an owned `String`, returning `None` if the
/// pointer is null or the string isn't valid UTF-8.
fn read_cstr(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the pointer is a malloc'd C string with
    // a nul terminator, and the C memory remains valid until the
    // surrounding `ProbeGuard` drops at end of scope.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_str().ok().map(|s| s.to_owned())
}
