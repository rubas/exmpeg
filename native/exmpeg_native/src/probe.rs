//! `ffprobe`-style media inspection: open an input, run
//! `avformat_find_stream_info`, and return a structured report of format
//! and stream metadata.
//!
//! A handful of helpers read raw C strings off `AVFormatContext`,
//! `AVStream`, and `AVCodecParameters` fields, plus walk the
//! `AVDictionary` metadata pointer. Those need explicit `unsafe`; the
//! file-level `allow` is scoped to this module so the rest of the crate
//! keeps the crate-wide `deny(unsafe_code)` default.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr;

use rsmpeg::avcodec::AVCodec;
use rsmpeg::avformat::AVFormatContextInput;
use rsmpeg::ffi;
use rustler::NifMap;

use crate::errors::NativeError;

/// Whole-file probe report: container-level format info plus a stream
/// entry per discovered stream.
#[derive(Debug, NifMap)]
pub(crate) struct ProbeReport {
    pub(crate) format: ProbeFormat,
    pub(crate) streams: Vec<ProbeStream>,
}

/// Container-level metadata.
#[derive(Debug, NifMap)]
pub(crate) struct ProbeFormat {
    /// Container short name (`"mov,mp4,m4a,3gp,3g2,mj2"`, `"matroska,webm"`).
    pub(crate) name: String,
    /// Human-readable long name when available (`"QuickTime / MOV"`).
    pub(crate) long_name: Option<String>,
    /// Duration in seconds (derived from `AV_TIME_BASE` ticks).
    pub(crate) duration_s: Option<f64>,
    /// Container bit rate in bits per second; `0` when ffmpeg could not
    /// estimate it.
    pub(crate) bit_rate: i64,
    /// Stream start time in seconds; `None` when unset.
    pub(crate) start_time_s: Option<f64>,
    /// Number of streams in the container.
    pub(crate) nb_streams: u32,
    /// Free-form key/value metadata pulled from the container header.
    pub(crate) tags: Vec<(String, String)>,
}

/// Per-stream metadata. `kind` is one of `"video"`, `"audio"`,
/// `"subtitle"`, `"data"`, `"attachment"`, or `"unknown"`.
#[derive(Debug, NifMap)]
pub(crate) struct ProbeStream {
    pub(crate) index: u32,
    pub(crate) kind: String,
    /// Codec short name (`"h264"`, `"aac"`); `"unknown"` when ffmpeg has
    /// no decoder registered for the codec id.
    pub(crate) codec: String,
    pub(crate) codec_long_name: Option<String>,
    /// Stream-level bit rate in bps; `0` when unset.
    pub(crate) bit_rate: i64,
    /// Stream time base as `{num, den}`.
    pub(crate) time_base: (i32, i32),
    /// Duration in seconds, derived from the stream's own time base.
    pub(crate) duration_s: Option<f64>,
    /// Number of frames if the demuxer reports one; `None` otherwise.
    pub(crate) nb_frames: Option<i64>,
    /// Audio-specific fields. `None` for non-audio streams.
    pub(crate) audio: Option<AudioInfo>,
    /// Video-specific fields. `None` for non-video streams.
    pub(crate) video: Option<VideoInfo>,
}

#[derive(Debug, NifMap)]
pub(crate) struct AudioInfo {
    pub(crate) sample_rate: i32,
    pub(crate) channels: i32,
    /// Sample format short name (`"fltp"`, `"s16"`); `None` when ffmpeg
    /// cannot resolve a name for the raw enum value.
    pub(crate) sample_format: Option<String>,
}

#[derive(Debug, NifMap)]
pub(crate) struct VideoInfo {
    pub(crate) width: i32,
    pub(crate) height: i32,
    /// Pixel format short name (`"yuv420p"`); `None` when unresolved.
    pub(crate) pixel_format: Option<String>,
    /// Average frame rate as `{num, den}`. `(0, 1)` means unknown.
    pub(crate) frame_rate: (i32, i32),
}

pub(crate) fn probe<P: AsRef<Path>>(path: P) -> Result<ProbeReport, NativeError> {
    let path = path.as_ref();
    if !path.is_file() {
        return Err(
            NativeError::new("invalid_request", "input path is not a regular file")
                .with_detail("path", path.display().to_string()),
        );
    }

    let url = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "input path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })?;

    let input = AVFormatContextInput::open(&url, None, &mut None)?;
    let streams = collect_streams(&input);
    let format = collect_format(&input);

    Ok(ProbeReport { format, streams })
}

fn collect_format(input: &AVFormatContextInput) -> ProbeFormat {
    let iformat = input.iformat();
    let name = c_str_or_empty(iformat.name);
    let long_name = c_str_opt(iformat.long_name);

    let tags = read_dict(input.metadata);

    let duration_s = ticks_to_seconds(input.duration, i64::from(ffi::AV_TIME_BASE));
    let start_time_s = ticks_to_seconds(input.start_time, i64::from(ffi::AV_TIME_BASE));

    ProbeFormat {
        name,
        long_name,
        duration_s,
        bit_rate: input.bit_rate,
        start_time_s,
        nb_streams: input.nb_streams,
        tags,
    }
}

fn collect_streams(input: &AVFormatContextInput) -> Vec<ProbeStream> {
    input
        .streams()
        .iter()
        .map(|stream| {
            let codecpar = stream.codecpar();
            let codec_id = codecpar.codec_id;
            let codec_type = codecpar.codec_type;

            let kind = match codec_type {
                ffi::AVMEDIA_TYPE_VIDEO => "video",
                ffi::AVMEDIA_TYPE_AUDIO => "audio",
                ffi::AVMEDIA_TYPE_SUBTITLE => "subtitle",
                ffi::AVMEDIA_TYPE_DATA => "data",
                ffi::AVMEDIA_TYPE_ATTACHMENT => "attachment",
                _ => "unknown",
            };

            let (codec, codec_long_name) = resolve_codec(codec_id);

            let time_base = (stream.time_base.num, stream.time_base.den);
            let duration_s = ticks_to_seconds(stream.duration, i64::from(stream.time_base.den))
                .map(|sec| sec * f64::from(stream.time_base.num));
            let nb_frames = if stream.nb_frames > 0 {
                Some(stream.nb_frames)
            } else {
                None
            };

            let audio = if codec_type == ffi::AVMEDIA_TYPE_AUDIO {
                Some(AudioInfo {
                    sample_rate: codecpar.sample_rate,
                    channels: codecpar.ch_layout.nb_channels,
                    sample_format: sample_fmt_name(codecpar.format),
                })
            } else {
                None
            };

            let video = if codec_type == ffi::AVMEDIA_TYPE_VIDEO {
                let avg = stream.avg_frame_rate;
                let frame_rate = if avg.den == 0 {
                    (0, 1)
                } else {
                    (avg.num, avg.den)
                };
                Some(VideoInfo {
                    width: codecpar.width,
                    height: codecpar.height,
                    pixel_format: pix_fmt_name(codecpar.format),
                    frame_rate,
                })
            } else {
                None
            };

            ProbeStream {
                index: stream.index as u32,
                kind: kind.to_owned(),
                codec,
                codec_long_name,
                bit_rate: codecpar.bit_rate,
                time_base,
                duration_s,
                nb_frames,
                audio,
                video,
            }
        })
        .collect()
}

fn resolve_codec(codec_id: ffi::AVCodecID) -> (String, Option<String>) {
    AVCodec::find_decoder(codec_id).map_or_else(
        || ("unknown".to_owned(), None),
        |codec| {
            let name = codec.name().to_string_lossy().into_owned();
            let long = c_str_opt(codec.long_name);
            (name, long)
        },
    )
}

fn sample_fmt_name(fmt: i32) -> Option<String> {
    // SAFETY: `av_get_sample_fmt_name` accepts any i32; an invalid enum
    // value returns NULL, which we handle below.
    let ptr = unsafe { ffi::av_get_sample_fmt_name(fmt) };
    if ptr.is_null() {
        None
    } else {
        // SAFETY: `ptr` is a non-NULL, NUL-terminated, static string
        // owned by libavutil.
        let cstr = unsafe { CStr::from_ptr(ptr) };
        Some(cstr.to_string_lossy().into_owned())
    }
}

fn pix_fmt_name(fmt: i32) -> Option<String> {
    // SAFETY: `av_get_pix_fmt_name` accepts any i32; an invalid enum
    // value returns NULL, which we handle below.
    let ptr = unsafe { ffi::av_get_pix_fmt_name(fmt) };
    if ptr.is_null() {
        None
    } else {
        // SAFETY: `ptr` is a non-NULL, NUL-terminated, static string
        // owned by libavutil.
        let cstr = unsafe { CStr::from_ptr(ptr) };
        Some(cstr.to_string_lossy().into_owned())
    }
}

fn read_dict(dict: *mut ffi::AVDictionary) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if dict.is_null() {
        return out;
    }
    let mut entry: *mut ffi::AVDictionaryEntry = ptr::null_mut();
    let empty = CString::new("").expect("static empty CString");
    loop {
        // SAFETY: `dict` is non-NULL (checked above); `empty.as_ptr()` is
        // a valid NUL-terminated C string; `entry` is either NULL on the
        // first pass or the previous return value from the same call.
        entry = unsafe {
            ffi::av_dict_get(
                dict,
                empty.as_ptr(),
                entry,
                ffi::AV_DICT_IGNORE_SUFFIX as i32,
            )
        };
        if entry.is_null() {
            break;
        }
        // SAFETY: `entry` is non-NULL (checked above) and both `key` and
        // `value` are owned by the dictionary for the duration of this
        // iteration; we copy them into `String` before continuing.
        let key = unsafe { CStr::from_ptr((*entry).key) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: see above.
        let value = unsafe { CStr::from_ptr((*entry).value) }
            .to_string_lossy()
            .into_owned();
        out.push((key, value));
    }
    out
}

#[inline]
fn ticks_to_seconds(ticks: i64, denom: i64) -> Option<f64> {
    if ticks == ffi::AV_NOPTS_VALUE || denom == 0 {
        None
    } else {
        Some(ticks as f64 / denom as f64)
    }
}

fn c_str_or_empty(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: `ptr` is non-NULL (checked above) and points at a
        // NUL-terminated string owned by libavformat.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

fn c_str_opt(ptr: *const std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: see `c_str_or_empty`.
        let s = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}
