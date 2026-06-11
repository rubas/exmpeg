//! Input-source abstraction: a path on disk, an in-memory binary, or a
//! pre-loaded `Exmpeg.Buffer` resource, all opened through the same
//! `AVFormatContextInput` API.
//!
//! In-memory inputs (both `{:memory, binary}` and a loaded buffer) are
//! backed by an `AVIOContextCustom` with read + seek callbacks. The
//! bytes live in a `SharedBytes` handle captured by the callbacks: for
//! `{:memory, _}` the binary is copied into an owned `Vec` once per
//! call; for a loaded buffer the bytes live in a refcounted resource, so
//! repeated operations on the same buffer (probe then transcode, or N
//! concat inputs) share one copy instead of copying per call.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rsmpeg::avformat::{AVFormatContextInput, AVIOContextContainer, AVIOContextCustom};
use rsmpeg::avutil::{AVDictionary, AVMem};
use rsmpeg::ffi;
use rustler::types::tuple::get_tuple;
use rustler::{Binary, Decoder, Env, NifResult, Resource, ResourceArc, Term};

use crate::errors::NativeError;

mod atoms {
    rustler::atoms! {
        memory,
    }
}

/// Refcounted byte buffer handed across the NIF boundary as an opaque
/// `Exmpeg.Buffer`. Holding the input in a resource lets the caller load
/// it once and reuse it across operations without re-copying the binary
/// into the Rust heap on every call.
pub(crate) struct BufferResource {
    pub(crate) bytes: Vec<u8>,
}

#[rustler::resource_impl]
impl Resource for BufferResource {}

/// Shared, clonable handle to in-memory input bytes. Both variants are
/// cheap to clone (a refcount bump) and read-only, so the read and seek
/// AVIO callbacks each capture their own clone.
#[derive(Clone)]
enum SharedBytes {
    /// Bytes copied once from a `{:memory, binary}` term.
    Owned(Arc<Vec<u8>>),
    /// Bytes living in a loaded `Exmpeg.Buffer` resource (no copy).
    Resource(ResourceArc<BufferResource>),
}

impl SharedBytes {
    fn slice(&self) -> &[u8] {
        match self {
            SharedBytes::Owned(v) => v.as_slice(),
            SharedBytes::Resource(r) => r.bytes.as_slice(),
        }
    }
}

/// Caller-supplied input. `Path` is a regular filesystem path; `Memory`
/// holds a per-call copy of an in-memory binary; `Buffer` references a
/// pre-loaded resource that several calls can share.
pub(crate) enum InputSource {
    Path(String),
    Memory(Vec<u8>),
    Buffer(ResourceArc<BufferResource>),
}

impl InputSource {
    /// Open the source as an `AVFormatContextInput`. For paths, that's
    /// the usual `avformat_open_input`. For in-memory inputs it
    /// constructs a custom AVIO context with seek support so demuxers
    /// that need to jump around the file (mp4 looking for `moov`,
    /// matroska reading cues) still work.
    pub(crate) fn open(self) -> Result<AVFormatContextInput, NativeError> {
        match self {
            InputSource::Path(path) => open_path(&path),
            InputSource::Memory(bytes) => {
                if bytes.is_empty() {
                    return Err(NativeError::new(
                        "invalid_request",
                        "memory input is an empty binary",
                    ));
                }
                open_avio(SharedBytes::Owned(Arc::new(bytes)))
            }
            InputSource::Buffer(buf) => {
                if buf.bytes.is_empty() {
                    return Err(NativeError::new("invalid_request", "buffer input is empty"));
                }
                open_avio(SharedBytes::Resource(buf))
            }
        }
    }

    /// Best-effort short label for error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            InputSource::Path(p) => p.clone(),
            InputSource::Memory(bytes) => format!("<memory:{} bytes>", bytes.len()),
            InputSource::Buffer(buf) => format!("<buffer:{} bytes>", buf.bytes.len()),
        }
    }
}

impl<'a> Decoder<'a> for InputSource {
    fn decode(term: Term<'a>) -> NifResult<Self> {
        // Plain string is a path.
        if let Ok(s) = String::decode(term) {
            return Ok(InputSource::Path(s));
        }
        // An `Exmpeg.Buffer` reference decodes as our resource type.
        if let Ok(buf) = ResourceArc::<BufferResource>::decode(term) {
            return Ok(InputSource::Buffer(buf));
        }
        // Tagged tuple `{:memory, <<...>>}` is an in-memory binary.
        let Ok(items) = get_tuple(term) else {
            return Err(rustler::Error::BadArg);
        };

        if items.len() != 2 {
            return Err(rustler::Error::BadArg);
        }

        let Ok(atom) = rustler::Atom::from_term(items[0]) else {
            return Err(rustler::Error::BadArg);
        };

        if atom != atoms::memory() {
            return Err(rustler::Error::BadArg);
        }

        let Ok(bin) = Binary::from_term(items[1]) else {
            return Err(rustler::Error::BadArg);
        };

        Ok(InputSource::Memory(bin.as_slice().to_vec()))
    }
}

// FFmpeg's `protocol_whitelist` gates every protocol open libavformat
// performs, including the nested opens that container-of-references
// demuxers (HLS, DASH, the `concat` protocol, sidecar segments) trigger
// from inside attacker-controlled media. Without it, a crafted input can
// drive FFmpeg into opening `http://169.254.169.254/...` (SSRF) or a
// local file it picks. We pin the whitelist on every input so untrusted
// media can never reach the network or an unexpected file.
//
// Normal single-file demuxers (mp4, mkv, ...) perform no nested opens, so
// the whitelist is invisible to them; it only constrains the reference
// demuxers, which is exactly the intent.

// File inputs: the top-level open uses the `file` protocol, and a local
// HLS/DASH playlist may legitimately reference sibling segment files, so
// `file` stays in the set. `crypto` and `data` cover encrypted segments
// and inline `data:` URIs. Network protocols are excluded. This matches
// the default the `file:` protocol already implies; pinning it makes the
// guarantee explicit and immune to FFmpeg default changes.
const FILE_PROTOCOL_WHITELIST: &CStr = c"file,crypto,data";

// Memory inputs are documented for buffering an entire untrusted upload.
// Such input never legitimately needs to reach the filesystem or network,
// so the whitelist excludes `file` as well, leaving only `crypto,data`.
const MEMORY_PROTOCOL_WHITELIST: &CStr = c"crypto,data";

fn open_path(path: &str) -> Result<AVFormatContextInput, NativeError> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(
            NativeError::new("invalid_request", "input path is not a regular file")
                .with_detail("path", path.to_owned()),
        );
    }
    let url = CString::new(path.as_bytes()).map_err(|_| {
        NativeError::new("invalid_request", "input path contains NUL bytes")
            .with_detail("path", path.to_owned())
    })?;
    let mut options = Some(AVDictionary::new(
        c"protocol_whitelist",
        FILE_PROTOCOL_WHITELIST,
        0,
    ));
    AVFormatContextInput::builder()
        .url(&url)
        .options(&mut options)
        .open()
        .map_err(NativeError::from)
}

fn open_avio(bytes: SharedBytes) -> Result<AVFormatContextInput, NativeError> {
    let cursor = Arc::new(AtomicUsize::new(0));
    let read_cursor = Arc::clone(&cursor);
    let read_bytes = bytes.clone();
    let seek_cursor = cursor;
    let seek_bytes = bytes;

    // The AVIO `data` slot is unused: the bytes come from the captured
    // `SharedBytes` handle (an owned `Arc<Vec>` for `{:memory, _}`, or a
    // resource refcount for a loaded buffer) so the buffer path never
    // copies. The cursor lives outside in an `Arc<AtomicUsize>` so read
    // and seek share state.
    let io = AVIOContextCustom::alloc_context(
        AVMem::new(IO_BUF_LEN),
        false,
        Vec::new(),
        Some(Box::new(move |_data, buf| {
            let data = read_bytes.slice();
            let cur = read_cursor.load(Ordering::Relaxed);
            if cur >= data.len() {
                return ffi::AVERROR_EOF;
            }
            let remaining = data.len() - cur;
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&data[cur..cur + n]);
            read_cursor.store(cur + n, Ordering::Relaxed);
            n as i32
        })),
        None,
        Some(Box::new(move |_data, offset, whence| {
            // libavformat encodes `whence` as either a stdio `SEEK_*`
            // value (0/1/2) or the special `AVSEEK_SIZE` flag asking
            // for the total length. `AVSEEK_FORCE` may also be OR'd in;
            // we mask it off.
            let avseek_size = ffi::AVSEEK_SIZE as i32;
            let avseek_force = ffi::AVSEEK_FORCE as i32;
            let len = seek_bytes.slice().len() as i64;

            let cleaned = whence & !avseek_force;
            if cleaned == avseek_size {
                return len;
            }
            let new_pos: i64 = match cleaned {
                0 => offset,                                              // SEEK_SET
                1 => seek_cursor.load(Ordering::Relaxed) as i64 + offset, // SEEK_CUR
                2 => len + offset,                                        // SEEK_END
                _ => return -1,
            };
            if new_pos < 0 || new_pos > len {
                return -1;
            }
            seek_cursor.store(new_pos as usize, Ordering::Relaxed);
            new_pos
        })),
    );

    let mut options = Some(AVDictionary::new(
        c"protocol_whitelist",
        MEMORY_PROTOCOL_WHITELIST,
        0,
    ));
    AVFormatContextInput::builder()
        .io_context(AVIOContextContainer::Custom(io))
        .options(&mut options)
        .open()
        .map_err(NativeError::from)
}

// 64 KiB scratch buffer — big enough that demuxers can probe headers
// without trickling 4 KiB reads, small enough to stay light. libavformat
// is free to swap this out internally; the size is just an initial hint.
const IO_BUF_LEN: usize = 64 * 1024;

/// Lightweight `Env<'_>`-based decoding for callers that already hold
/// the env (e.g. when we want to construct an `InputSource` outside the
/// derive macros). Currently unused but kept so the symbol exists in
/// case we need it from custom NIF entry points.
#[allow(dead_code)]
pub(crate) fn decode_in<'a>(env: Env<'a>, term: Term<'a>) -> NifResult<InputSource> {
    let _ = env;
    InputSource::decode(term)
}
