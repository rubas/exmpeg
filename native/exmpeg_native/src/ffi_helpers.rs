//! Single audit surface for every `unsafe` block in this crate.
//!
//! Each function below is a thin safe wrapper around one specific FFmpeg
//! interaction that rsmpeg does not currently expose with a safe API. By
//! quarantining the `unsafe` to this file, the rest of the crate is
//! built entirely on rsmpeg's safe wrappers (`#![deny(unsafe_code)]`
//! catches any new `unsafe` block elsewhere).
//!
//! If rsmpeg gains safe equivalents (e.g. an `AVAudioFifo::write_frame`
//! or `AVCodecParameters::set_codec_tag`), replace these helpers and
//! drop the file-level `allow`.

#![allow(unsafe_code)]

use std::ffi::CString;

use rsmpeg::avcodec::AVCodecParameters;
use rsmpeg::avformat::AVFormatContextOutput;
use rsmpeg::avutil::{AVAudioFifo, AVDictionary, AVFrame};
use rsmpeg::error::RsmpegError;

/// Reconstruct an `Env` struct from a raw `NIF_ENV` pointer.
///
/// SAFETY: The raw `NIF_ENV` pointer must be a valid environment pointer handed to the NIF
/// by Rustler, and the returned `Env` must not outlive the NIF execution context.
pub(crate) fn reconstruct_env<'a>(c_env: rustler::wrapper::NIF_ENV) -> rustler::Env<'a> {
    // SAFETY: We assume the caller provides a valid `c_env` pointer from the NIF context.
    // Creating an Env is unsafe because it lets the caller create arbitrary lifetime references,
    // which is sound as long as the returned Env does not escape the NIF call.
    unsafe { rustler::Env::new(&(), c_env) }
}

/// Zero the `codec_tag` field of an `AVCodecParameters` so the muxer
/// picks the correct fourCC for the output container. Required when
/// remuxing between containers that use different tags for the same
/// codec (e.g. Matroska -> MP4); rsmpeg does not expose a safe setter
/// for this single i32 field.
pub(crate) fn clear_codec_tag(params: &mut AVCodecParameters) {
    // SAFETY: `as_mut_ptr()` returns a unique, valid pointer for the
    // duration of the `&mut` borrow. `codec_tag` is a plain `u32` field
    // (no allocation, no references); writing it is a single primitive
    // store.
    unsafe {
        (*params.as_mut_ptr()).codec_tag = 0;
    }
}

/// Append every sample of `frame` to `fifo`. `frame` must already match
/// the FIFO's sample format and channel count.
pub(crate) fn write_fifo_frame(fifo: &mut AVAudioFifo, frame: &AVFrame) -> Result<(), RsmpegError> {
    // SAFETY: `frame.extended_data` is a valid `*mut *mut u8` array of
    // per-channel pointers for `frame.nb_samples` samples (rsmpeg
    // guarantees this on a frame returned from `get_buffer`).
    // `av_audio_fifo_write` copies the data out and retains no pointer.
    unsafe { fifo.write(frame.extended_data.cast_const(), frame.nb_samples) }
}

/// Read up to `nb_samples` from `fifo` into `frame`. Returns the number
/// of samples actually read. `frame` must have buffers sized to hold
/// `nb_samples` in the FIFO's sample format.
pub(crate) fn read_fifo_into_frame(
    fifo: &mut AVAudioFifo,
    frame: &mut AVFrame,
    nb_samples: i32,
) -> Result<i32, RsmpegError> {
    // SAFETY: see `write_fifo_frame`; the frame's buffers are sized to
    // hold `nb_samples` and `av_audio_fifo_read` writes at most that
    // many. The caller validates that the FIFO has at least
    // `nb_samples` available before calling.
    unsafe { fifo.read(frame.extended_data.cast_const(), nb_samples) }
}

/// Attach container-level metadata tags (`title`, `artist`, `encoder`,
/// etc.) to an output context. Entries whose key or value contain a
/// NUL byte are silently skipped — they cannot survive a round-trip
/// through libavformat's C-string-based dictionary.
pub(crate) fn set_format_metadata(output: &mut AVFormatContextOutput, tags: &[(String, String)]) {
    let mut dict: Option<AVDictionary> = None;
    for (k, v) in tags {
        let (Ok(key), Ok(val)) = (CString::new(k.as_str()), CString::new(v.as_str())) else {
            continue;
        };
        dict = Some(match dict.take() {
            None => AVDictionary::new(&key, &val, 0),
            Some(existing) => existing.set(&key, &val, 0),
        });
    }
    if let Some(dict) = dict {
        // SAFETY: `AVDictionary::into_raw` transfers ownership of the
        // underlying `*mut AVDictionary` allocation. We assign it to
        // `output.metadata`, which libavformat takes ownership of and
        // frees at `avformat_free_context` time. The previous metadata
        // pointer (if any) is overwritten; on a freshly-created output
        // context that field is NULL.
        unsafe {
            (*output.as_mut_ptr()).metadata = dict.into_raw().as_ptr();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn read_codec_tag(params: &AVCodecParameters) -> u32 {
        // SAFETY: `as_ptr()` is a valid pointer for the lifetime of the
        // `&` borrow. `codec_tag` is a plain `u32` field, no allocation,
        // no references; reading it is a single primitive load.
        unsafe { (*params.as_ptr()).codec_tag }
    }

    #[test]
    fn clear_codec_tag_zeros_the_field() {
        let mut params = AVCodecParameters::new();
        // SAFETY: same justification as `clear_codec_tag` itself - we
        // need a non-zero start state to prove the helper zeroes it.
        unsafe {
            (*params.as_mut_ptr()).codec_tag = 0x6134_706D; // "mp4a"
        }
        assert_eq!(read_codec_tag(&params), 0x6134_706D);

        clear_codec_tag(&mut params);
        assert_eq!(read_codec_tag(&params), 0);
    }

    fn read_metadata_entry<'a>(output: &'a AVFormatContextOutput, key: &CStr) -> Option<&'a CStr> {
        // SAFETY: `as_ptr()` is valid for the borrow. `av_dict_get` is
        // safe to call against a NULL dictionary pointer (it returns
        // NULL). The returned entry borrows from the dictionary; we
        // immediately re-borrow as a `&CStr` tied to `output`'s
        // lifetime.
        unsafe {
            let metadata = (*output.as_ptr()).metadata;
            if metadata.is_null() {
                return None;
            }
            let entry = rsmpeg::ffi::av_dict_get(metadata, key.as_ptr(), std::ptr::null(), 0);
            if entry.is_null() {
                return None;
            }
            Some(CStr::from_ptr((*entry).value))
        }
    }

    #[test]
    fn set_format_metadata_round_trips_tags() {
        // Use an .mp4 path to pick a real muxer; we never write the
        // file (no `write_header` call), so it never touches disk.
        let url = CString::new("/tmp/exmpeg_ffi_helpers_test.mp4").unwrap();
        let mut output = AVFormatContextOutput::create(&url).expect("muxer alloc");

        let tags = vec![
            ("title".to_owned(), "demo".to_owned()),
            ("artist".to_owned(), "exmpeg".to_owned()),
        ];
        set_format_metadata(&mut output, &tags);

        assert_eq!(read_metadata_entry(&output, c"title"), Some(c"demo"));
        assert_eq!(read_metadata_entry(&output, c"artist"), Some(c"exmpeg"));
    }

    #[test]
    fn set_format_metadata_skips_entries_with_nul_bytes() {
        let url = CString::new("/tmp/exmpeg_ffi_helpers_nul.mp4").unwrap();
        let mut output = AVFormatContextOutput::create(&url).expect("muxer alloc");

        let tags = vec![
            ("title".to_owned(), "ok".to_owned()),
            ("bad\0key".to_owned(), "dropped".to_owned()),
            ("bad_value".to_owned(), "x\0y".to_owned()),
        ];
        set_format_metadata(&mut output, &tags);

        assert_eq!(read_metadata_entry(&output, c"title"), Some(c"ok"));
        assert_eq!(read_metadata_entry(&output, c"bad_value"), None);
    }
}
