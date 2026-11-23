//! The batch-scoped and file-scoped state of an encode run.
//!
//! These two objects exist because the alternative was a parameter bag:
//! the same four things (params / cache / config / verdicts) were
//! threaded through every layer of the encode chain as loose
//! parameters, so each new requirement meant adding a parameter to
//! eight functions, and any code that wanted to reuse a layer had to
//! reassemble the bag by hand (design doc §3.10.4).
//!
//! # Ownership
//!
//! - [`BatchContext`] belongs to one batch: the GUI's batch worker
//!   thread, or one CLI command. It is created once, moved into
//!   whoever runs the batch, and lives as long as that batch.
//! - [`FilePlan`] belongs to one file: the result of the prepare step
//!   (probe + skip decision + argv build). It replaces the payload of
//!   `EncodePrepareResult::Ready`.
//!
//! # What is *not* in here
//!
//! Path-specific state stays a parameter: the CLI's `PartialOutputHandle`
//! and `ProcessOutput`, the GUI's `CancelToken` and progress sink.
//! Folding those in would make the context a union of everything, and
//! each path would carry fields it never reads.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};

use crate::encoder::nvenc::Capability;
use crate::types::{EncodeParams, EncoderCache, VideoMetadata};

/// Everything one batch carries: its parameters, the resolved encoder
/// cache and this machine's NVENC capability.
///
/// The effective configuration lives in `params` -- there is no second
/// config object to keep in sync (design doc §3.10.5).
///
/// The capability is the only mutable part: the NVENC fallback writes
/// into it when an encoder turns out to be unusable (design doc
/// §3.10.1.1 — the actor owns the authoritative copy, a worker owns a
/// snapshot). It carries the check's conclusion *and* the run's
/// corrections, so the whole chain reads one thing (design doc
/// §3.10.5).
#[derive(Clone, Debug)]
pub struct BatchContext {
    params: EncodeParams,
    cache: Arc<EncoderCache>,
    capability: Capability,
}

impl BatchContext {
    /// Build a context for one batch.
    pub fn new(params: EncodeParams, cache: Arc<EncoderCache>, capability: Capability) -> Self {
        Self {
            params,
            cache,
            capability,
        }
    }

    /// The batch's parameters.
    pub fn params(&self) -> &EncodeParams {
        &self.params
    }

    /// The resolved encoder cache (immutable snapshot).
    pub fn cache(&self) -> &EncoderCache {
        &self.cache
    }

    /// This machine's NVENC capability.
    pub fn capability(&self) -> &Capability {
        &self.capability
    }

    /// This machine's NVENC capability, mutably.
    ///
    /// Only the NVENC fallback path writes into it — one place, right
    /// after an NVENC encode failed.
    pub fn capability_mut(&mut self) -> &mut Capability {
        &mut self.capability
    }
}

/// One file's state after the prepare step: what the source looks like,
/// what argv we built for it, where the output goes, and when the
/// attempt started.
///
/// It replaces the payload of `EncodePrepareResult::Ready` (which
/// carried the same three values as separate fields). The two
/// timestamps are an *input* to the prepare step -- the skipped and
/// failed records need the start time -- so the caller creates them and
/// the plan carries them onward.
#[derive(Clone, Debug)]
pub struct FilePlan {
    metadata: VideoMetadata,
    argv: Vec<String>,
    output_path: PathBuf,
    started_at_meta: DateTime<Utc>,
    started_at_instant: Instant,
}

impl FilePlan {
    /// Build a plan for a file that is ready to encode.
    pub fn new(
        metadata: VideoMetadata,
        argv: Vec<String>,
        output_path: PathBuf,
        started_at_meta: DateTime<Utc>,
        started_at_instant: Instant,
    ) -> Self {
        Self {
            metadata,
            argv,
            output_path,
            started_at_meta,
            started_at_instant,
        }
    }

    /// What the source looks like (probe result).
    pub fn metadata(&self) -> &VideoMetadata {
        &self.metadata
    }

    /// The ffmpeg argv built for this file.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Take the argv (the runner needs its own copy; the record keeps
    /// the original).
    pub fn take_argv(&mut self) -> Vec<String> {
        std::mem::take(&mut self.argv)
    }

    /// Replace the argv (the NVENC fallback re-selects the encoder).
    pub fn set_argv(&mut self, argv: Vec<String>) {
        self.argv = argv;
    }

    /// Where the output goes.
    pub fn output_path(&self) -> &PathBuf {
        &self.output_path
    }

    /// Wall-clock start of the attempt.
    pub fn started_at_meta(&self) -> DateTime<Utc> {
        self.started_at_meta
    }

    /// Monotonic start of the attempt (for durations).
    pub fn started_at_instant(&self) -> Instant {
        self.started_at_instant
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> BatchContext {
        BatchContext::new(
            EncodeParams::default(),
            Arc::new(EncoderCache {
                encoders_output: String::new(),
            }),
            Capability::default(),
        )
    }

    #[test]
    fn context_exposes_its_parts() {
        let mut ctx = ctx();
        assert!(!ctx.params().format.default_extension().is_empty());
        assert!(ctx.cache().encoders_output.is_empty());
        assert!(ctx
            .params()
            .encoders
            .for_format(ctx.params().format)
            .is_none());
        assert!(!ctx.capability().verdicts().is_disabled("h264_nvenc"));
        // The verdicts are the mutable part.
        ctx.capability_mut().verdicts_mut().set("h264_nvenc", false);
        assert!(ctx.capability().verdicts().is_disabled("h264_nvenc"));
    }

    #[test]
    fn plan_hands_out_the_argv_once() {
        let mut plan = FilePlan::new(
            VideoMetadata::default(),
            vec!["ffmpeg".into(), "-i".into(), "in.mp4".into()],
            PathBuf::from("out.mkv"),
            Utc::now(),
            Instant::now(),
        );
        assert_eq!(plan.argv().len(), 3);
        let taken = plan.take_argv();
        assert_eq!(taken.len(), 3);
        assert!(plan.argv().is_empty());
        // The fallback re-selects the encoder and puts a new argv in.
        plan.set_argv(vec!["ffmpeg".into(), "-c:v".into(), "libx264".into()]);
        assert_eq!(plan.argv().len(), 3);
    }
}
