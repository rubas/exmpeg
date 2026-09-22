//! Cooperative cancellation for long-running dirty NIFs.
//!
//! A transcode or concat can run for minutes to hours on a dirty
//! scheduler thread. If the BEAM process that called the NIF dies (a
//! `Task` timeout, supervisor shutdown, LiveView disconnect, a kill),
//! nothing stops the native work: it keeps decoding and encoding to
//! completion, holding a dirty-scheduler slot and burning CPU and disk
//! for output nobody will read.
//!
//! `CancelGuard` closes that gap. The NIF entry point captures the
//! calling pid, and the hot loops call [`CancelGuard::check`] on a time
//! throttle. When the caller is gone the check returns a `"cancelled"`
//! error, which unwinds through `atomic_output::run` so the partial
//! output is removed and no final file is produced.
//!
//! The guard does not cover the input open. rsmpeg's
//! `AVFormatContextInput::builder().open()` runs `avformat_open_input`
//! and `avformat_find_stream_info` in one call and gives no way to set
//! `AVFormatContext::interrupt_callback` before it, so a dead caller
//! cannot stop the open. FFmpeg's `probesize` (5 MB) and
//! `analyzeduration` (5 to 90 s of media, by format) defaults limit only
//! the stream analysis. Nothing limits the header read in
//! `avformat_open_input`: an mp4 `moov` index grows with the sample
//! count. `probe` is only an open and takes no guard. A read that blocks in the kernel is not
//! interruptible even with a callback. A real interrupt needs its own
//! open in `ffi_helpers.rs` or an rsmpeg builder parameter.
//!
//! This is deliberately separate from `ProgressEmitter`: the emitter is
//! a no-op when no subscriber is set, so its throttle state never
//! advances in the common case and cannot carry a liveness check.

use std::time::{Duration, Instant};

use rustler::Env;
use rustler::types::LocalPid;
use rustler::wrapper::NIF_ENV;

use crate::errors::NativeError;

/// `enif_is_process_alive` takes a scheduler lock, so the check is
/// throttled to at most once per interval rather than run per packet.
const CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Watches whether the calling BEAM process is still alive so a
/// long-running NIF can bail out when its caller dies.
pub(crate) struct CancelGuard {
    pid: LocalPid,
    /// Raw `NIF_ENV` pointer for the calling process, reconstructed into
    /// an `Env<'_>` for each liveness check the same way `ProgressEmitter`
    /// reconstructs it to send messages. Valid for the lifetime of the
    /// NIF call that constructed this guard.
    env_ptr: NIF_ENV,
    last_check: Option<Instant>,
}

impl CancelGuard {
    /// Capture the calling process from the entry-point env.
    pub(crate) fn new(env: Env<'_>) -> Self {
        Self {
            pid: env.pid(),
            env_ptr: env.as_c_arg(),
            last_check: None,
        }
    }

    /// Return `Err("cancelled")` when the calling process has died.
    /// Throttled to [`CHECK_INTERVAL`]; calls between checks return
    /// `Ok(())` without touching the scheduler lock. Call it once near
    /// the top of every packet loop iteration.
    pub(crate) fn check(&mut self) -> Result<(), NativeError> {
        let now = Instant::now();
        if let Some(last) = self.last_check
            && now.duration_since(last) < CHECK_INTERVAL
        {
            return Ok(());
        }
        self.last_check = Some(now);

        let env = crate::ffi_helpers::reconstruct_env(self.env_ptr);
        if env.is_process_alive(self.pid) {
            Ok(())
        } else {
            Err(NativeError::new(
                "cancelled",
                "calling process exited before the operation completed",
            ))
        }
    }
}
