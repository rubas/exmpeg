//! `ffprobe`-style media inspection: open an input, run
//! `avformat_find_stream_info`, and return a structured report of format
//! and stream metadata.
//!
//! Built entirely on rsmpeg's safe wrappers — no `unsafe` in this module.

use rsmpeg::avcodec::AVCodec;
use rsmpeg::avformat::AVFormatContextInput;
use rsmpeg::avutil::{get_pix_fmt_name, get_sample_fmt_name};
use rsmpeg::ffi;
use rustler::NifMap;

use crate::errors::NativeError;
use crate::input::InputSource;

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

pub(crate) fn probe(source: InputSource) -> Result<ProbeReport, NativeError> {
    let input = source.open()?;
    let streams = collect_streams(&input);
    let format = collect_format(&input);
    Ok(ProbeReport { format, streams })
}

fn collect_format(input: &AVFormatContextInput) -> ProbeFormat {
    let iformat = input.iformat();
    let name = iformat.name().to_string_lossy().into_owned();
    let long_name = iformat
        .long_name()
        .to_str()
        .ok()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let tags = read_metadata(input);

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
                    sample_format: get_sample_fmt_name(codecpar.format)
                        .map(|c| c.to_string_lossy().into_owned()),
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
                    pixel_format: get_pix_fmt_name(codecpar.format)
                        .map(|c| c.to_string_lossy().into_owned()),
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
            let long = codec
                .long_name()
                .to_str()
                .ok()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            (name, long)
        },
    )
}

fn read_metadata(input: &AVFormatContextInput) -> Vec<(String, String)> {
    input.metadata().map_or_else(Vec::new, |dict| {
        dict.iter()
            .map(|entry| {
                (
                    entry.key().to_string_lossy().into_owned(),
                    entry.value().to_string_lossy().into_owned(),
                )
            })
            .collect()
    })
}

#[inline]
fn ticks_to_seconds(ticks: i64, denom: i64) -> Option<f64> {
    if ticks == ffi::AV_NOPTS_VALUE || denom == 0 {
        None
    } else {
        Some(ticks as f64 / denom as f64)
    }
}
