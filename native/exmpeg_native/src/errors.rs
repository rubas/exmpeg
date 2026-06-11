//! Categorized errors for the NIF boundary.
//!
//! Every entry point returns `Result<T, NativeError>` and `NativeError` is
//! encoded as `{:error, %{type, message, details}}` on the Elixir side.
//! `type` carries the category (`"invalid_request"`, `"io_error"`,
//! `"decode_error"`, `"encode_error"`, `"unsupported"`, `"runtime_error"`,
//! `"nif_panic"`), and Elixir maps it back to a `Exmpeg.Error.reason` atom.

use std::collections::HashMap;

use rustler::NifMap;

/// Structured error returned to Elixir as a `NifMap`.
#[derive(Debug, NifMap)]
pub(crate) struct NativeError {
    /// Error category. Stable, matches `Exmpeg.Error` reason strings.
    pub(crate) r#type: String,
    /// Human-readable message. Safe to surface to end users.
    pub(crate) message: String,
    /// Free-form key/value details (paths, codec names, offsets, etc.).
    pub(crate) details: HashMap<String, String>,
}

impl NativeError {
    pub(crate) fn new(type_name: &str, message: impl Into<String>) -> Self {
        Self {
            r#type: type_name.to_owned(),
            message: message.into(),
            details: HashMap::new(),
        }
    }

    pub(crate) fn with_detail(mut self, key: &str, value: impl Into<String>) -> Self {
        self.details.insert(key.to_owned(), value.into());
        self
    }
}

/// Convenience: maps an `rsmpeg::error::RsmpegError` to a categorized
/// `NativeError`. The mapping is best-effort: file-not-found / format
/// problems become `io_error`, codec/decoder problems become
/// `decode_error`, and everything else falls through to `runtime_error`.
impl From<rsmpeg::error::RsmpegError> for NativeError {
    fn from(err: rsmpeg::error::RsmpegError) -> Self {
        // RsmpegError is a flat enum; classify by the variant's debug name
        // so we stay forward-compatible across minor rsmpeg releases that
        // add or rename variant payloads. The Display impl carries the
        // human-readable message.
        let kind = classify_rsmpeg_error(&err);
        NativeError::new(kind, format!("{err}"))
    }
}

/// Classify a `write_header` failure on a copy op (remux, concat). The
/// streams and output file are already set up by that point, so a codec
/// the chosen muxer cannot hold (e.g. h264 into a `.wav`) fails with
/// AVERROR(EINVAL) - map only that to `unsupported`. Any other failure
/// here is a real output I/O error (a full or quota-limited filesystem
/// returns AVERROR(ENOSPC), an unwritable target EACCES/EIO, ...), so
/// keep its default classification (`io_error`) rather than mislabeling a
/// recoverable storage failure as a codec problem. The unknown-extension
/// case is caught earlier with an explicit muxer lookup, so this does not
/// shadow a missing muxer.
pub(crate) fn classify_write_header_error(err: rsmpeg::error::RsmpegError) -> NativeError {
    // EINVAL is 22 on every supported platform; FFmpeg's `AVERROR(e)`
    // negates the errno.
    const AVERROR_EINVAL: i32 = -22;
    if matches!(err, rsmpeg::error::RsmpegError::AVError(code) if code == AVERROR_EINVAL) {
        return NativeError::new(
            "unsupported",
            "output container does not support one of the input codecs",
        )
        .with_detail("cause", format!("{err}"));
    }
    NativeError::from(err)
}

fn classify_rsmpeg_error(err: &rsmpeg::error::RsmpegError) -> &'static str {
    let dbg = format!("{err:?}");
    if dbg.starts_with("OpenInputError")
        || dbg.starts_with("FindStreamInfoError")
        || dbg.starts_with("AVError")
    {
        "io_error"
    } else if dbg.starts_with("SendPacket")
        || dbg.starts_with("ReceiveFrame")
        || dbg.starts_with("Decoder")
    {
        "decode_error"
    } else if dbg.starts_with("SendFrame")
        || dbg.starts_with("ReceivePacket")
        || dbg.starts_with("Encoder")
    {
        "encode_error"
    } else {
        // BufferSink, Bitstream, AVFrame*, TryFromIntError, Unknown all
        // surface as runtime faults rather than user-attributable errors.
        "runtime_error"
    }
}
