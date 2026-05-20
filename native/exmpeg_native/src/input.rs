//! Input-source abstraction: a path on disk or an in-memory binary,
//! both opened through the same `AVFormatContextInput` API.
//!
//! Memory inputs are backed by an `AVIOContextCustom` with read + seek
//! callbacks operating on a `Vec<u8>` that lives for the lifetime of
//! the format context.

use std::ffi::CString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rsmpeg::avformat::{AVFormatContextInput, AVIOContextContainer, AVIOContextCustom};
use rsmpeg::avutil::AVMem;
use rsmpeg::ffi;
use rustler::types::tuple::get_tuple;
use rustler::{Binary, Decoder, Env, NifResult, Term};

use crate::errors::NativeError;

mod atoms {
    rustler::atoms! {
        memory,
    }
}

/// Caller-supplied input. `Path` is a regular filesystem path; `Memory`
/// holds the entire input buffered in memory.
pub(crate) enum InputSource {
    Path(String),
    Memory(Vec<u8>),
}

impl InputSource {
    /// Open the source as an `AVFormatContextInput`. For paths, that's
    /// the usual `avformat_open_input`. For memory inputs it constructs
    /// a custom AVIO context with seek support so demuxers that need to
    /// jump around the file (mp4 looking for `moov`, matroska reading
    /// cues) still work.
    pub(crate) fn open(self) -> Result<AVFormatContextInput, NativeError> {
        match self {
            InputSource::Path(path) => open_path(&path),
            InputSource::Memory(bytes) => open_memory(bytes),
        }
    }

    /// Best-effort short label for error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            InputSource::Path(p) => p.clone(),
            InputSource::Memory(bytes) => format!("<memory:{} bytes>", bytes.len()),
        }
    }
}

impl<'a> Decoder<'a> for InputSource {
    fn decode(term: Term<'a>) -> NifResult<Self> {
        // Plain string is a path.
        if let Ok(s) = String::decode(term) {
            return Ok(InputSource::Path(s));
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
    AVFormatContextInput::open(&url).map_err(NativeError::from)
}

fn open_memory(bytes: Vec<u8>) -> Result<AVFormatContextInput, NativeError> {
    if bytes.is_empty() {
        return Err(NativeError::new(
            "invalid_request",
            "memory input is an empty binary",
        ));
    }

    let cursor = Arc::new(AtomicUsize::new(0));
    let read_cursor = Arc::clone(&cursor);
    let seek_cursor = cursor;

    // `data` (the Vec<u8>) is moved into the AVIOContextCustom and
    // re-borrowed by each callback invocation. The cursor lives outside
    // in an Arc<AtomicUsize> so read and seek share state.
    let io = AVIOContextCustom::alloc_context(
        AVMem::new(IO_BUF_LEN),
        false,
        bytes,
        Some(Box::new(move |data, buf| {
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
        Some(Box::new(move |data, offset, whence| {
            // libavformat encodes `whence` as either a stdio `SEEK_*`
            // value (0/1/2) or the special `AVSEEK_SIZE` flag asking
            // for the total length. `AVSEEK_FORCE` may also be OR'd in;
            // we mask it off.
            let avseek_size = ffi::AVSEEK_SIZE as i32;
            let avseek_force = ffi::AVSEEK_FORCE as i32;
            let len = data.len() as i64;

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

    AVFormatContextInput::from_io_context(AVIOContextContainer::Custom(io))
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
