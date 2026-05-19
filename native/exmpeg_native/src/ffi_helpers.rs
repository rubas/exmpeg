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
