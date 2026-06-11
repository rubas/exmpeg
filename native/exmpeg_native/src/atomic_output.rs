//! Atomic output writes for operations that mux to disk.
//!
//! Every output-producing op (remux, extract_frame, extract_audio,
//! concat, transcode) runs against a sibling
//! `<stem>.partial.<nonce>.<ext>` path and renames onto the final
//! destination only after the muxer trailer has been written
//! successfully. On error the partial file is removed.
//!
//! The partial path lives in the same directory as the destination so
//! the final rename is a single inode link rename (atomic on POSIX)
//! and so we do not silently spill output across filesystems.
//!
//! The `.partial.<nonce>` infix lands before the file extension, not
//! after: libavformat picks the muxer from the extension, and
//! `out.mp4.partial` has no known extension. `out.mp4` becomes
//! `out.partial.<nonce>.mp4`.
//!
//! The `<nonce>` (a per-process random base + the OS process id + a
//! process-wide counter + a nanosecond timestamp) makes the partial path
//! unique per call, including across two nodes on shared storage that
//! happen to share a pid. Two concurrent
//! writes to the same destination - duplicate jobs, a retry racing a
//! slow first attempt, two nodes on shared storage - therefore never
//! share a partial file, so one call can never unlink or rename another
//! call's in-progress output. The guarantee is last-complete-rename-wins:
//! every state an observer can see at the destination is a complete file.
//!
//! Because each partial path is unique it never pre-exists, so there is
//! no stale-partial cleanup on entry. A hard crash (SIGKILL, power loss)
//! mid-write can therefore leave a `<stem>.partial.*` sibling behind; it
//! is never renamed onto the destination and can be swept by the caller.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::errors::NativeError;

/// Run `body` against a unique `<final>.partial.<nonce>` path, then
/// rename the resulting file onto `final_path`. On error the partial
/// file is removed (best effort) so a failed call never leaves a
/// half-written output on disk.
pub(crate) fn run<T, F>(final_path: &str, body: F) -> Result<T, NativeError>
where
    F: FnOnce(&Path) -> Result<T, NativeError>,
{
    let final_path = PathBuf::from(final_path);
    let partial = partial_path_for(&final_path, &partial_nonce());

    // Remove the partial on any early exit - an `Err` from `body`, a
    // failed rename, OR a panic unwinding out of `body` (which
    // `run_with_panic_protection` catches one frame up, after this scope
    // has already dropped). The guard is disarmed only once the rename
    // onto the destination has succeeded.
    let mut guard = PartialGuard::new(&partial);

    let value = body(&partial)?;

    std::fs::rename(&partial, &final_path).map_err(|err| {
        NativeError::new(
            "io_error",
            "could not rename partial output onto destination",
        )
        .with_detail("from", partial.display().to_string())
        .with_detail("to", final_path.display().to_string())
        .with_detail("cause", err.to_string())
    })?;

    guard.disarm();
    Ok(value)
}

/// Removes the `.partial` file when dropped unless disarmed. This covers
/// the panic path the explicit error branches cannot: a panic inside
/// `body` unwinds past `run` before any value is produced, and
/// `catch_unwind` sits a frame up in `lib.rs`, so without this guard the
/// partial would leak on disk.
struct PartialGuard<'a> {
    partial: &'a Path,
    armed: bool,
}

impl<'a> PartialGuard<'a> {
    fn new(partial: &'a Path) -> Self {
        Self {
            partial,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartialGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: a NotFound (the muxer never created the file)
            // is fine, and we must not mask the original failure.
            let _ = std::fs::remove_file(self.partial);
        }
    }
}

/// A per-call token that makes the partial path unique. Combines a
/// per-process random base (separates two OS processes even if they
/// share a pid on shared storage and issue their nth write in the same
/// `SystemTime` tick - the case a pid + counter + timestamp alone could
/// still collide on), the OS process id, a process-wide monotonic
/// counter (separates calls within one process), and a nanosecond
/// timestamp.
fn partial_nonce() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // `RandomState` is seeded from the OS RNG, so a hash off a fresh one
    // is effectively a random per-process u64. Computed once and reused.
    static RAND_BASE: OnceLock<u64> = OnceLock::new();
    let base = *RAND_BASE.get_or_init(|| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(0);
        hasher.finish()
    });

    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{base:016x}.{}.{seq}.{nanos}", std::process::id())
}

fn partial_path_for(final_path: &Path, nonce: &str) -> PathBuf {
    // libavformat picks the muxer from the file extension, so the
    // marker has to land BEFORE the extension: `out.mp4` becomes
    // `out.partial.<nonce>.mp4`, never `out.mp4.partial`. Files without
    // an extension fall back to a trailing `.partial.<nonce>` suffix.
    let stem = final_path.file_stem().map(OsString::from);
    let ext = final_path.extension();

    let mut name = match (stem, ext) {
        (Some(stem), Some(ext)) => {
            let mut n = stem;
            n.push(".partial.");
            n.push(nonce);
            n.push(".");
            n.push(ext);
            n
        }
        (Some(stem), None) => {
            let mut n = stem;
            n.push(".partial.");
            n.push(nonce);
            n
        }
        _ => OsString::from(format!(".partial.{nonce}")),
    };
    if name.is_empty() {
        name = OsString::from(format!(".partial.{nonce}"));
    }
    match final_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "exmpeg-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn renames_partial_onto_destination_on_success() {
        let dir = tempdir();
        let final_path = dir.join("out.bin");
        let seen_partial = std::cell::RefCell::new(PathBuf::new());

        let result = run(final_path.to_str().unwrap(), |p| {
            *seen_partial.borrow_mut() = p.to_path_buf();
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(b"payload").unwrap();
            Ok::<_, NativeError>(())
        });

        let partial = seen_partial.into_inner();
        assert!(result.is_ok());
        assert!(final_path.exists());
        assert!(!partial.exists());
        // The partial is a `<stem>.partial.<nonce>.<ext>` sibling whose
        // extension is preserved last for muxer detection.
        let name = partial.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("out.partial."));
        assert_eq!(partial.extension().unwrap(), "bin");
        assert_eq!(partial.parent(), final_path.parent());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"payload");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_partial_and_does_not_create_destination_on_error() {
        let dir = tempdir();
        let final_path = dir.join("out.bin");
        let seen_partial = std::cell::RefCell::new(PathBuf::new());

        let result: Result<(), NativeError> = run(final_path.to_str().unwrap(), |p| {
            *seen_partial.borrow_mut() = p.to_path_buf();
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(b"half-written").unwrap();
            Err(NativeError::new("decode_error", "boom"))
        });

        assert!(result.is_err());
        assert!(!final_path.exists());
        assert!(!seen_partial.into_inner().exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_path_is_unique_per_call() {
        let final_path = PathBuf::from("/tmp/out.mp4");
        let a = partial_path_for(&final_path, &partial_nonce());
        let b = partial_path_for(&final_path, &partial_nonce());

        // Two calls against the same destination never collide.
        assert_ne!(a, b);
        // The extension stays last so the muxer still detects the format.
        assert_eq!(a.extension().unwrap(), "mp4");
        assert_eq!(b.extension().unwrap(), "mp4");
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("out.partial.")
        );
    }

    #[test]
    fn extensionless_partial_keeps_nonce_suffix() {
        let partial = partial_path_for(&PathBuf::from("/tmp/out"), "X");
        assert_eq!(partial.file_name().unwrap(), "out.partial.X");
    }

    #[test]
    fn removes_partial_when_body_panics() {
        let dir = tempdir();
        let final_path = dir.join("out.bin");
        // The partial path carries a per-call random nonce, so capture the
        // actual one the closure was handed rather than predicting it.
        let seen_partial = std::cell::RefCell::new(PathBuf::new());

        // Mirror the production layering: `catch_unwind` wraps `run` a
        // frame up, so the guard must remove the partial during unwind.
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(final_path.to_str().unwrap(), |p| {
                *seen_partial.borrow_mut() = p.to_path_buf();
                std::fs::write(p, b"half-written").unwrap();
                panic!("boom");
                #[allow(unreachable_code)]
                Ok::<_, NativeError>(())
            })
        }));

        assert!(caught.is_err(), "the panic should propagate out of run");
        assert!(
            !seen_partial.into_inner().exists(),
            "the partial must be cleaned up on panic"
        );
        assert!(!final_path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
