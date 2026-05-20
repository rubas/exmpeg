//! Throttled progress reporting from inside a long-running NIF call.
//!
//! Each operation creates a `ProgressEmitter` once; the hot loops call
//! `tick(stats)` after every meaningful step. The emitter coalesces
//! ticks so the caller receives at most one message every
//! `MIN_INTERVAL` (default 100 ms). The final tick at end-of-stream is
//! sent unconditionally via `finish` so the caller sees the closing
//! counts.
//!
//! Implementation note: `Env::send` from inside a NIF requires the
//! calling NIF's env (a process-bound, scheduler-thread-managed env).
//! `OwnedEnv::send_and_clear` panics on managed threads. We capture the
//! raw `NIF_ENV` pointer from the entry-point env at construction time
//! and reconstruct an `Env<'_>` for each emission. This is sound: the
//! pointer is valid for the duration of the NIF call (Rustler
//! guarantees this), and the emitter cannot outlive the call because
//! the NIF function consumes ownership of it before returning.

use std::time::{Duration, Instant};

use rsmpeg::ffi;
use rustler::types::LocalPid;
use rustler::wrapper::NIF_ENV;
use rustler::{Encoder, Env, NifMap};

mod atoms {
    rustler::atoms! {
        exmpeg_progress,
    }
}

const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Snapshot sent to the caller. Field naming mirrors the per-operation
/// stats so subscribers can render the same UI for any op.
#[derive(Debug, Clone, NifMap)]
pub(crate) struct ProgressUpdate {
    /// What this NIF is doing - `"probe"`, `"remux"`, `"transcode"`,
    /// `"extract_audio"`, `"extract_frame"`, `"concat"`.
    pub(crate) op: String,
    /// Packets handed to the muxer so far (or decoded for ops without
    /// a writable output, e.g. probe).
    pub(crate) packets_written: u64,
    /// Current output PTS in seconds. `0.0` when unknown.
    pub(crate) current_pts_s: f64,
    /// Container duration in seconds, if known up front (input.duration
    /// from the demuxer). `0.0` when unknown.
    pub(crate) total_duration_s: f64,
}

pub(crate) struct ProgressEmitter {
    inner: Option<Inner>,
}

struct Inner {
    pid: LocalPid,
    /// Raw `NIF_ENV` pointer for the calling process. Valid for the
    /// lifetime of the NIF call that constructed this emitter.
    env_ptr: NIF_ENV,
    op: &'static str,
    total_duration_s: f64,
    last_emit: Option<Instant>,
}

impl ProgressEmitter {
    /// Build an emitter. `env` is the calling NIF's environment (used
    /// to send messages from the dirty-scheduler thread). `pid` is the
    /// BEAM pid messages go to; if `None`, the emitter is a no-op.
    pub(crate) fn new(
        env: Env<'_>,
        pid: Option<LocalPid>,
        op: &'static str,
        total_duration_s: f64,
    ) -> Self {
        Self {
            inner: pid.map(|pid| Inner {
                pid,
                env_ptr: env.as_c_arg(),
                op,
                total_duration_s,
                last_emit: None,
            }),
        }
    }

    pub(crate) fn from_av_duration(
        env: Env<'_>,
        pid: Option<LocalPid>,
        op: &'static str,
        av_duration_ticks: i64,
    ) -> Self {
        let total = if av_duration_ticks > 0 {
            av_duration_ticks as f64 / f64::from(ffi::AV_TIME_BASE)
        } else {
            0.0
        };
        Self::new(env, pid, op, total)
    }

    /// Send a progress message unless we sent one within the throttle
    /// window. Cheap when there's no subscriber.
    pub(crate) fn tick(&mut self, packets_written: u64, current_pts_s: f64) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let now = Instant::now();
        if let Some(last) = inner.last_emit {
            if now.duration_since(last) < MIN_INTERVAL {
                return;
            }
        }
        inner.last_emit = Some(now);
        send(inner, packets_written, current_pts_s);
    }

    /// Send the closing tick unconditionally - bypasses throttling so
    /// the caller always observes the final counters.
    pub(crate) fn finish(&mut self, packets_written: u64, current_pts_s: f64) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        inner.last_emit = Some(Instant::now());
        send(inner, packets_written, current_pts_s);
    }
}

fn send(inner: &Inner, packets_written: u64, current_pts_s: f64) {
    let update = ProgressUpdate {
        op: inner.op.to_owned(),
        packets_written,
        current_pts_s,
        total_duration_s: inner.total_duration_s,
    };
    let env = crate::ffi_helpers::reconstruct_env(inner.env_ptr);
    // Errors here are advisory: process gone, mailbox full, etc. Never
    // block a transcode on a slow subscriber.
    let _ = env.send(&inner.pid, (atoms::exmpeg_progress(), update).encode(env));
}
