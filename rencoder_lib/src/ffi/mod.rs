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
}

extern "C" {
    fn rencoder_probe(path: *const c_char) -> *mut ProbeResult;
    fn rencoder_free(result: *mut ProbeResult);
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
    // SAFETY: `raw` is a non-null pointer to a ProbeResult that the C
    // bridge allocated via calloc. Fields are either malloc'd CStrings or
    // a double (or NULL for missing data). We read each field, copy the
    // data, then call rencoder_free.
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

    unsafe { rencoder_free(raw) };

    Ok(VideoMetadata {
        codec,
        avg_fps: avg_fps.or(r_frame_rate),
        nominal_fps: r_frame_rate,
        duration_secs: duration,
    })
}

/// Read a malloc'd C string into an owned `String`, returning `None` if the
/// pointer is null or the string isn't valid UTF-8.
fn read_cstr(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees the pointer is a malloc'd C string with
    // a nul terminator, and we own the memory until rencoder_free is
    // called. The pointer remains valid until we call rencoder_free, which
    // happens after we return from this function.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_str().ok().map(|s| s.to_owned())
}
