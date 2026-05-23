//! Rustler NIF over the `rsmpeg` Rust bindings for FFmpeg 8. Every entry
//! point returns `{:ok, value}` or `{:error, %{type, message, details}}`
//! and runs on a dirty scheduler when the underlying work blocks on disk
//! I/O, libavformat state, or codec processing.

#![deny(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use rustler::{Encoder, Env, Term};

mod atomic_output;
mod concat;
mod errors;
mod extract_audio;
mod extract_frame;
mod ffi_helpers;
mod input;
mod probe;
mod progress;
mod remux;
mod transcode;
mod version;

use errors::NativeError;

#[allow(missing_docs)]
mod atoms {
    rustler::atoms! {
        ok,
        error,
    }
}
use atoms::{error, ok};

fn run_with_panic_protection<T, F>(f: F) -> Result<T, NativeError>
where
    F: FnOnce() -> Result<T, NativeError>,
{
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic_info| {
        let message = panic_info
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic_info.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        Err(NativeError::new("nif_panic", message))
    })
}

fn encode_result<T: Encoder>(env: Env<'_>, result: Result<T, NativeError>) -> Term<'_> {
    match result {
        Ok(value) => (ok(), value).encode(env),
        Err(err) => (error(), err).encode(env),
    }
}

/// Reports the version of every FFmpeg sub-library this NIF is linked
/// against, plus the configure flags used to build them.
#[rustler::nif]
fn nif_version(env: Env<'_>) -> Term<'_> {
    let result = run_with_panic_protection(|| Ok(version::version_info()));
    encode_result(env, result)
}

/// Probes a media file via `avformat_open_input` +
/// `avformat_find_stream_info` and returns a structured report.
#[rustler::nif(schedule = "DirtyIo")]
#[allow(clippy::needless_pass_by_value)] // Rustler decodes NIF args by value.
fn nif_probe(env: Env<'_>, source: input::InputSource) -> Term<'_> {
    let result = run_with_panic_protection(|| probe::probe(source));
    encode_result(env, result)
}

/// Stream-copies an input container to an output container with optional
/// start / duration window.
#[rustler::nif(schedule = "DirtyIo")]
#[allow(clippy::needless_pass_by_value)] // Rustler decodes NIF args by value.
fn nif_remux(
    env: Env<'_>,
    source: input::InputSource,
    output: String,
    opts: remux::RemuxOpts,
) -> Term<'_> {
    let result = run_with_panic_protection(|| {
        atomic_output::run(&output, |partial| remux::remux(env, source, partial, &opts))
    });
    encode_result(env, result)
}

/// Decodes a single video frame at a timestamp, optionally rescales it,
/// and writes it as an image via the `image2` muxer.
#[rustler::nif(schedule = "DirtyCpu")]
#[allow(clippy::needless_pass_by_value)] // Rustler decodes NIF args by value.
fn nif_extract_frame(
    env: Env<'_>,
    source: input::InputSource,
    output: String,
    opts: extract_frame::ExtractFrameOpts,
) -> Term<'_> {
    let result = run_with_panic_protection(|| {
        atomic_output::run(&output, |partial| {
            extract_frame::extract_frame(source, partial, &opts)
        })
    });
    encode_result(env, result)
}

/// Decodes the best audio stream and writes it as a 16-bit PCM WAV file.
#[rustler::nif(schedule = "DirtyCpu")]
#[allow(clippy::needless_pass_by_value)] // Rustler decodes NIF args by value.
fn nif_extract_audio(
    env: Env<'_>,
    source: input::InputSource,
    output: String,
    opts: extract_audio::ExtractAudioOpts,
) -> Term<'_> {
    let result = run_with_panic_protection(|| {
        atomic_output::run(&output, |partial| {
            extract_audio::extract_audio(env, source, partial, &opts)
        })
    });
    encode_result(env, result)
}

/// Stream-copy concat: joins a list of inputs sharing the same stream
/// layout into a single output without re-encoding.
#[rustler::nif(schedule = "DirtyIo")]
#[allow(clippy::needless_pass_by_value)] // Rustler decodes NIF args by value.
fn nif_concat(
    env: Env<'_>,
    sources: Vec<input::InputSource>,
    output: String,
    opts: concat::ConcatOpts,
) -> Term<'_> {
    let result = run_with_panic_protection(|| {
        atomic_output::run(&output, |partial| {
            concat::concat(env, sources, partial, &opts)
        })
    });
    encode_result(env, result)
}

/// Per-stream re-encode with codec / bitrate / scale / fps / sample-rate
/// selection. Streams whose codec opt is `"copy"` are stream-copied;
/// the rest go through a decoder + (swscale | swresample) + encoder
/// pipeline.
#[rustler::nif(schedule = "DirtyCpu")]
#[allow(clippy::needless_pass_by_value)] // Rustler decodes NIF args by value.
fn nif_transcode(
    env: Env<'_>,
    source: input::InputSource,
    output: String,
    opts: transcode::TranscodeOpts,
) -> Term<'_> {
    let result = run_with_panic_protection(|| {
        atomic_output::run(&output, |partial| {
            transcode::transcode(env, source, partial, &opts)
        })
    });
    encode_result(env, result)
}

rustler::init!("Elixir.Exmpeg.Native");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_panic_protection_catches_string_panic() {
        let result: Result<(), _> = run_with_panic_protection(|| panic!("boom"));
        let err = result.unwrap_err();
        assert_eq!(err.r#type, "nif_panic");
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn run_with_panic_protection_passes_through_ok_and_err() {
        let ok_result = run_with_panic_protection(|| Ok::<_, NativeError>(42));
        assert_eq!(ok_result.unwrap(), 42);

        let err_result: Result<(), _> =
            run_with_panic_protection(|| Err(NativeError::new("io_error", "x")));
        assert_eq!(err_result.unwrap_err().r#type, "io_error");
    }
}
