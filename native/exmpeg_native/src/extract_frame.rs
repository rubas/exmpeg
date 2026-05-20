//! Single-frame extraction: open an input, seek to a timestamp, decode
//! one video frame, optionally rescale via `swscale`, and write it as an
//! image (`.jpg`, `.png`, `.bmp`, …) via the `image2` muxer.
//!
//! Replaces `ffmpeg -ss T -i in -frames:v 1 out.jpg`. Built entirely on
//! rsmpeg's safe wrappers — no `unsafe` in this module.

use std::ffi::CString;
use std::path::Path;

use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avformat::{AVFormatContextInput, AVFormatContextOutput};
use rsmpeg::avutil::{AVDictionary, AVFrame};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;
use rustler::NifMap;

use crate::errors::NativeError;

#[derive(Debug, Default, NifMap)]
pub(crate) struct ExtractFrameOpts {
    /// Timestamp in seconds where the frame should be captured. `0.0`
    /// for the first frame.
    pub(crate) timestamp_s: Option<f64>,
    /// Optional target width in pixels. When set without `:height`, the
    /// height is scaled proportionally.
    pub(crate) width: Option<i32>,
    /// Optional target height in pixels. When set without `:width`, the
    /// width is scaled proportionally.
    pub(crate) height: Option<i32>,
}

#[derive(Debug, NifMap)]
pub(crate) struct ExtractFrameStats {
    pub(crate) width: i32,
    pub(crate) height: i32,
    /// Source-side pts of the extracted frame, in seconds. `0.0` when
    /// the source stream carries no pts (`pts_known == false`); see
    /// `pts_known` to distinguish that from a frame at t=0.
    pub(crate) timestamp_s: f64,
    /// `true` when `timestamp_s` is derived from the source frame's
    /// pts; `false` when the source stream has no pts and the field
    /// falls back to `0.0`.
    pub(crate) pts_known: bool,
    pub(crate) codec: String,
}

pub(crate) fn extract_frame<Q: AsRef<Path>>(
    source: crate::input::InputSource,
    output_path: Q,
    opts: &ExtractFrameOpts,
) -> Result<ExtractFrameStats, NativeError> {
    let output_path = output_path.as_ref();
    let out_url = to_cstring(output_path)?;

    let mut input = source.open()?;

    let (video_index, decoder_codec) = find_video_stream(&input)?;

    // Drop the immutable borrow before any mutable use of `input` (seek
    // / read_packet). `apply_codecpar` only reads the parameters block,
    // so collapsing it into this scope keeps the codecpar reference
    // off the stack afterwards.
    let stream_time_base = input.streams()[video_index].time_base;
    let mut decoder = AVCodecContext::new(&decoder_codec);
    {
        let codecpar = input.streams()[video_index].codecpar();
        decoder.apply_codecpar(&codecpar)?;
    }
    decoder.set_time_base(stream_time_base);
    decoder.open(None)?;

    let target_s = opts.timestamp_s.unwrap_or(0.0);
    if target_s > 0.0 {
        seek_to(&mut input, video_index as i32, stream_time_base, target_s)?;
    }

    let frame = decode_target_frame(
        &mut input,
        &mut decoder,
        video_index,
        stream_time_base,
        target_s,
    )?;

    let encoder_codec_id = pick_image_codec(output_path)?;
    let encoder_codec = AVCodec::find_encoder(encoder_codec_id).ok_or_else(|| {
        NativeError::new("unsupported", "encoder not built into FFmpeg")
            .with_detail("codec_id", format!("{encoder_codec_id:?}"))
    })?;

    let (target_w, target_h) = resolve_target_size(decoder.width, decoder.height, opts);
    let encoder_pix_fmt = pick_encoder_pix_fmt(&encoder_codec, decoder.pix_fmt);

    // Capture the source pts before transferring ownership into
    // `scale_frame` — the returned frame preserves it, but reporting it
    // back to the caller is clearer when we just remember the value.
    let source_pts = frame.pts;
    let scaled_frame = scale_frame(
        frame,
        decoder.pix_fmt,
        decoder.width,
        decoder.height,
        encoder_pix_fmt,
        target_w,
        target_h,
    )?;

    let mut encoder = AVCodecContext::new(&encoder_codec);
    encoder.set_width(target_w);
    encoder.set_height(target_h);
    encoder.set_pix_fmt(encoder_pix_fmt);
    encoder.set_time_base(ffi::AVRational { num: 1, den: 25 });
    // FFmpeg 8 tightened mjpeg's pix_fmt rules: encoding into a
    // non-full-range YUV (yuv420p) without lowering the standards
    // strictness returns AVERROR(EINVAL). `FF_COMPLIANCE_UNOFFICIAL`
    // matches what the `ffmpeg` CLI does by default for image output.
    encoder.set_strict_std_compliance(ffi::FF_COMPLIANCE_UNOFFICIAL);
    encoder.open(None)?;

    let mut output = AVFormatContextOutput::create(&out_url)?;
    let codec_name;
    {
        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(encoder.extract_codecpar());
        out_stream.set_time_base(encoder.time_base);
        codec_name = encoder_codec.name().to_string_lossy().into_owned();
    }

    // image2 defaults to a filename pattern (`out%03d.jpg`) and warns
    // when one isn't found. `update=1` tells it the filename is literal
    // and the muxer should overwrite a single file - same flag the
    // `ffmpeg` CLI sets implicitly for `-frames:v 1`.
    let mut header_opts = Some(AVDictionary::new(c"update", c"1", 0));
    output.write_header(&mut header_opts)?;

    encoder.send_frame(Some(&scaled_frame))?;
    loop {
        match encoder.receive_packet() {
            Ok(mut packet) => {
                packet.set_stream_index(0);
                output.interleaved_write_frame(&mut packet)?;
            }
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        }
    }
    encoder.send_frame(None)?;
    loop {
        match encoder.receive_packet() {
            Ok(mut packet) => {
                packet.set_stream_index(0);
                output.interleaved_write_frame(&mut packet)?;
            }
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        }
    }
    output.write_trailer()?;

    let (observed_pts, pts_known) = if source_pts == ffi::AV_NOPTS_VALUE {
        (0.0, false)
    } else {
        let secs =
            source_pts as f64 * f64::from(stream_time_base.num) / f64::from(stream_time_base.den);
        (secs, true)
    };

    Ok(ExtractFrameStats {
        width: target_w,
        height: target_h,
        timestamp_s: observed_pts,
        pts_known,
        codec: codec_name,
    })
}

fn find_video_stream(
    input: &AVFormatContextInput,
) -> Result<(usize, rsmpeg::avcodec::AVCodecRef<'static>), NativeError> {
    match input.find_best_stream(ffi::AVMEDIA_TYPE_VIDEO) {
        Ok(Some(pair)) => Ok(pair),
        Ok(None) => Err(NativeError::new(
            "invalid_request",
            "input contains no video stream",
        )),
        Err(err) => Err(err.into()),
    }
}

fn seek_to(
    input: &mut AVFormatContextInput,
    stream_index: i32,
    time_base: ffi::AVRational,
    target_s: f64,
) -> Result<(), NativeError> {
    let pts = (target_s * f64::from(time_base.den) / f64::from(time_base.num)) as i64;
    let flags = ffi::AVSEEK_FLAG_BACKWARD as i32;
    input
        .seek(stream_index, pts, flags)
        .map_err(NativeError::from)
}

fn decode_target_frame(
    input: &mut AVFormatContextInput,
    decoder: &mut AVCodecContext,
    video_index: usize,
    time_base: ffi::AVRational,
    target_s: f64,
) -> Result<AVFrame, NativeError> {
    let target_pts =
        (target_s * f64::from(time_base.den) / f64::from(time_base.num)).round() as i64;

    // Track the most recently decoded frame so that a request past the
    // end of the file still returns the closest available frame rather
    // than an error. For codecs with no decoder delay (mjpeg, png) the
    // flush phase emits nothing, so the fallback has to be primed in
    // the main loop too.
    let mut last_seen: Option<AVFrame> = None;

    while let Some(packet) = input.read_packet()? {
        if packet.stream_index as usize != video_index {
            continue;
        }

        decoder.send_packet(Some(&packet))?;

        loop {
            match decoder.receive_frame() {
                Ok(frame) => {
                    let pts = if frame.pts == ffi::AV_NOPTS_VALUE {
                        frame.best_effort_timestamp
                    } else {
                        frame.pts
                    };
                    if target_s <= 0.0 || pts >= target_pts {
                        return Ok(frame);
                    }
                    last_seen = Some(frame);
                }
                Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
                Err(err) => return Err(err.into()),
            }
        }
    }

    // Flush the decoder and drain every buffered frame. Codecs with
    // decoder delay (B-frames, certain audio codecs) return more than
    // one frame after `send_packet(None)`; the single-call version
    // could return an earlier frame before the buffered target frame
    // was emitted.
    decoder.send_packet(None)?;
    loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                let pts = if frame.pts == ffi::AV_NOPTS_VALUE {
                    frame.best_effort_timestamp
                } else {
                    frame.pts
                };
                if target_s <= 0.0 || pts >= target_pts {
                    return Ok(frame);
                }
                last_seen = Some(frame);
            }
            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        }
    }

    if let Some(frame) = last_seen {
        return Ok(frame);
    }

    Err(NativeError::new(
        "decode_error",
        "decoder produced no frames at or after the requested timestamp",
    ))
}

fn pick_image_codec(output_path: &Path) -> Result<ffi::AVCodecID, NativeError> {
    let ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    match ext.as_deref() {
        Some("jpg" | "jpeg") => Ok(ffi::AV_CODEC_ID_MJPEG),
        Some("png") => Ok(ffi::AV_CODEC_ID_PNG),
        Some("bmp") => Ok(ffi::AV_CODEC_ID_BMP),
        Some("webp") => Ok(ffi::AV_CODEC_ID_WEBP),
        Some(other) => Err(NativeError::new(
            "invalid_request",
            "unsupported image extension; use .jpg, .png, .bmp or .webp",
        )
        .with_detail("extension", other.to_owned())),
        None => Err(NativeError::new(
            "invalid_request",
            "output path must have a recognised image extension (.jpg, .png, .bmp, .webp)",
        )),
    }
}

fn pick_encoder_pix_fmt(codec: &rsmpeg::avcodec::AVCodecRef<'static>, src: i32) -> i32 {
    if let Some(fmts) = codec.pix_fmts() {
        if fmts.contains(&src) {
            return src;
        }
        if let Some(first) = fmts.first() {
            return *first;
        }
    }
    src
}

fn resolve_target_size(src_w: i32, src_h: i32, opts: &ExtractFrameOpts) -> (i32, i32) {
    match (opts.width, opts.height) {
        (Some(w), Some(h)) => (round_even(w), round_even(h)),
        (Some(w), None) => {
            let scaled = (i64::from(w) * i64::from(src_h) / i64::from(src_w.max(1))) as i32;
            (round_even(w), round_even(scaled))
        }
        (None, Some(h)) => {
            let scaled = (i64::from(h) * i64::from(src_w) / i64::from(src_h.max(1))) as i32;
            (round_even(scaled), round_even(h))
        }
        // No resize requested: still snap the source dimensions to
        // even values. Encoders targeting yuv420p (the default pix_fmt
        // for h264 / jpeg / png) reject odd width or height, which
        // happens with a few oddly-cropped source files in the wild.
        (None, None) => (round_even(src_w), round_even(src_h)),
    }
}

#[inline]
fn round_even(n: i32) -> i32 {
    // Round down to the nearest even value, clamping to 2 for inputs
    // <= 1. `1 & !1` is 0 (which is an invalid video dimension), so the
    // guard must catch 1 as well as 0 and negative values.
    if n <= 1 { 2 } else { n & !1 }
}

#[allow(clippy::too_many_arguments)]
fn scale_frame(
    src: AVFrame,
    src_pix_fmt: i32,
    src_w: i32,
    src_h: i32,
    dst_pix_fmt: i32,
    dst_w: i32,
    dst_h: i32,
) -> Result<AVFrame, NativeError> {
    // No conversion needed: the decoded frame already matches the
    // encoder's pix_fmt and target dimensions. We own the frame here,
    // so handing it on saves an allocation and avoids needing a copy.
    if src_pix_fmt == dst_pix_fmt && src_w == dst_w && src_h == dst_h {
        return Ok(src);
    }

    let mut sws = SwsContext::get_context(
        src_w,
        src_h,
        src_pix_fmt,
        dst_w,
        dst_h,
        dst_pix_fmt,
        rsmpeg::ffi::SWS_BILINEAR,
        None,
        None,
        None,
    )
    .ok_or_else(|| NativeError::new("runtime_error", "failed to allocate sws context"))?;

    let mut dst = AVFrame::new();
    dst.set_width(dst_w);
    dst.set_height(dst_h);
    dst.set_format(dst_pix_fmt);
    dst.alloc_buffer()?;

    sws.scale_frame(&src, 0, src_h, &mut dst)?;
    dst.set_pts(src.pts);
    Ok(dst)
}

fn to_cstring(path: &Path) -> Result<CString, NativeError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })
}
