//! Atomic output writes for operations that mux to disk.
//!
//! Every output-producing op (remux, extract_frame, extract_audio,
//! concat, transcode) runs against a sibling `<stem>.partial.<ext>`
//! path and renames onto the final destination only after the muxer
//! trailer has been written successfully. On error the partial file
//! is removed so the destination is never left half-written.
//!
//! The partial path lives in the same directory as the destination so
//! the final rename is a single inode link rename (atomic on POSIX)
//! and so we do not silently spill output across filesystems.
//!
//! The `.partial` infix lands before the file extension, not after:
//! libavformat picks the muxer from the extension, and `out.mp4.partial`
//! has no known extension. `out.mp4` becomes `out.partial.mp4`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::errors::NativeError;

/// Run `body` against a `<final>.partial` path, then rename the
/// resulting file onto `final_path`. On error the partial file is
/// removed (best effort) so a failed call never leaves a half-written
/// output on disk.
pub(crate) fn run<T, F>(final_path: &str, body: F) -> Result<T, NativeError>
where
    F: FnOnce(&Path) -> Result<T, NativeError>,
{
    let final_path = PathBuf::from(final_path);
    let partial = partial_path_for(&final_path);

    // If a prior crashed run left a partial file behind, clear it.
    // Ignoring NotFound is fine; surfacing anything else as io_error
    // keeps the failure visible rather than papering over a permission
    // problem.
    if let Err(err) = std::fs::remove_file(&partial) {
        if err.kind() != std::io::ErrorKind::NotFound {
            return Err(
                NativeError::new("io_error", "could not clear stale partial output")
                    .with_detail("path", partial.display().to_string())
                    .with_detail("cause", err.to_string()),
            );
        }
    }

    match body(&partial) {
        Ok(value) => match std::fs::rename(&partial, &final_path) {
            Ok(()) => Ok(value),
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                Err(NativeError::new(
                    "io_error",
                    "could not rename partial output onto destination",
                )
                .with_detail("from", partial.display().to_string())
                .with_detail("to", final_path.display().to_string())
                .with_detail("cause", err.to_string()))
            }
        },
        Err(err) => {
            // Best-effort cleanup: if the muxer never created the file
            // (e.g. invalid path) remove_file will return NotFound and
            // we ignore it. We do not want to mask the original error
            // with a cleanup error.
            let _ = std::fs::remove_file(&partial);
            Err(err)
        }
    }
}

fn partial_path_for(final_path: &Path) -> PathBuf {
    // libavformat picks the muxer from the file extension, so the
    // marker has to land BEFORE the extension: `out.mp4` becomes
    // `out.partial.mp4`, never `out.mp4.partial`. Files without an
    // extension fall back to a trailing `.partial` suffix.
    let stem = final_path.file_stem().map(OsString::from);
    let ext = final_path.extension();

    let mut name = match (stem, ext) {
        (Some(stem), Some(ext)) => {
            let mut n = stem;
            n.push(".partial.");
            n.push(ext);
            n
        }
        (Some(stem), None) => {
            let mut n = stem;
            n.push(".partial");
            n
        }
        _ => OsString::from(".partial"),
    };
    if name.is_empty() {
        name = OsString::from(".partial");
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
        let partial = partial_path_for(&final_path);

        let result = run(final_path.to_str().unwrap(), |p| {
            assert_eq!(p, partial);
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(b"payload").unwrap();
            Ok::<_, NativeError>(())
        });

        assert!(result.is_ok());
        assert!(final_path.exists());
        assert!(!partial.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"payload");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_partial_and_does_not_create_destination_on_error() {
        let dir = tempdir();
        let final_path = dir.join("out.bin");
        let partial = partial_path_for(&final_path);

        let result: Result<(), NativeError> = run(final_path.to_str().unwrap(), |p| {
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(b"half-written").unwrap();
            Err(NativeError::new("decode_error", "boom"))
        });

        assert!(result.is_err());
        assert!(!final_path.exists());
        assert!(!partial.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clears_stale_partial_from_prior_run() {
        let dir = tempdir();
        let final_path = dir.join("out.bin");
        let partial = partial_path_for(&final_path);
        std::fs::write(&partial, b"stale").unwrap();

        let result = run(final_path.to_str().unwrap(), |p| {
            assert!(!p.exists(), "stale partial should have been removed");
            std::fs::write(p, b"fresh").unwrap();
            Ok::<_, NativeError>(())
        });

        assert!(result.is_ok());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"fresh");
        std::fs::remove_dir_all(&dir).ok();
    }
}
