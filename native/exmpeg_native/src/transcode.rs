//! Per-stream re-encode pipeline. Replaces
//! `ffmpeg -i in -c:v <enc> -b:v <br> -c:a <enc> -b:a <br> out`.
//!
//! Each input stream is either stream-copied or re-encoded based on the
//! caller's options. Video re-encode runs the decoded frame through
//! swscale (pix_fmt + optional resize / fps change) before encoding;
//! audio re-encode runs the decoded frame through swresample
//! (sample_fmt + sample rate + channel layout).
//!
//! Unsupported / unknown codec names surface as `:unsupported`.

use std::ffi::{CStr, CString};
use std::path::Path;

use rsmpeg::avcodec::{AVCodec, AVCodecContext, AVCodecRef};
use rsmpeg::avfilter::{AVFilter, AVFilterGraph, AVFilterInOut};
use rsmpeg::avformat::AVFormatContextOutput;
use rsmpeg::avutil::{AVAudioFifo, AVChannelLayout, AVFrame, av_rescale_q};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swresample::SwrContext;
use rustler::types::LocalPid;
use rustler::{Env, NifMap};

use crate::errors::NativeError;
use crate::ffi_helpers;
use crate::progress::ProgressEmitter;

#[derive(Default, NifMap)]
pub(crate) struct TranscodeOpts {
    /// Video codec selection. `"copy"` keeps the original packets,
    /// `None` defaults to copy, any other string is resolved through
    /// `avcodec_find_encoder_by_name` (e.g. `"libx264"`, `"libvpx-vp9"`,
    /// `"libaom-av1"`).
    pub(crate) video_codec: Option<String>,
    /// Audio codec selection. Same semantics as `video_codec`
    /// (`"aac"`, `"libopus"`, `"libmp3lame"`, `"flac"`).
    pub(crate) audio_codec: Option<String>,
    /// Video bitrate target in bits per second. Ignored on copy.
    pub(crate) video_bitrate: Option<i64>,
    /// Audio bitrate target in bits per second. Ignored on copy.
    pub(crate) audio_bitrate: Option<i64>,
    /// Target video width in pixels.
    pub(crate) width: Option<i32>,
    /// Target video height in pixels.
    pub(crate) height: Option<i32>,
    /// Target video framerate (numerator/denominator). `(0, 0)` means
    /// keep the source rate.
    pub(crate) fps: Option<(i32, i32)>,
    /// Target audio sample rate in Hz.
    pub(crate) sample_rate: Option<i32>,
    /// Target audio channel count (1 or 2).
    pub(crate) channels: Option<i32>,
    /// FFmpeg filter-graph spec applied between the video decoder and
    /// encoder (e.g. `"scale=720:-2,fps=30,crop=in_w:in_h-100:0:50"`).
    /// When set it overrides `:width` / `:height` / `:fps`.
    pub(crate) video_filter: Option<String>,
    /// Skip every audio stream (equivalent to `ffmpeg -an`).
    pub(crate) drop_audio: Option<bool>,
    /// Skip every video stream (equivalent to `ffmpeg -vn`).
    pub(crate) drop_video: Option<bool>,
    /// Skip every subtitle stream (equivalent to `ffmpeg -sn`).
    pub(crate) drop_subtitles: Option<bool>,
    /// Container-level metadata to attach to the output.
    pub(crate) tags: Option<Vec<(String, String)>>,
    /// Optional pid that receives throttled `{:exmpeg_progress, %{...}}`
    /// messages during the transcode loop.
    pub(crate) progress: Option<LocalPid>,
}

#[derive(Debug, NifMap)]
pub(crate) struct TranscodeStats {
    pub(crate) streams_copied: u32,
    pub(crate) streams_reencoded: u32,
    pub(crate) packets_written: u64,
    pub(crate) duration_s: f64,
}

/// Owns the lifetime-coupled filter graph + its source/sink contexts.
///
/// `AVFilterContextMut` borrows from the graph, so the graph has to
/// outlive the contexts. We achieve that by storing them next to each
/// other and accessing them through `with_contexts`.
struct VideoFilterGraph {
    graph: AVFilterGraph,
}

enum StreamPipeline {
    Copy {
        in_idx: usize,
        out_idx: i32,
        in_tb: ffi::AVRational,
    },
    Video {
        in_idx: usize,
        out_idx: i32,
        decoder: AVCodecContext,
        encoder: AVCodecContext,
        graph: VideoFilterGraph,
        next_pts: i64,
    },
    Audio {
        in_idx: usize,
        out_idx: i32,
        decoder: AVCodecContext,
        encoder: AVCodecContext,
        swr: SwrContext,
        fifo: AVAudioFifo,
        frame_size: i32,
        dst_layout: AVChannelLayout,
        dst_fmt: i32,
        dst_rate: i32,
        samples_written: i64,
    },
}

#[allow(clippy::too_many_lines)] // The transcode setup + drain loop is naturally linear.
pub(crate) fn transcode<Q: AsRef<Path>>(
    env: Env<'_>,
    source: crate::input::InputSource,
    output_path: Q,
    opts: &TranscodeOpts,
) -> Result<TranscodeStats, NativeError> {
    let output_path = output_path.as_ref();
    let out_url = to_cstring(output_path)?;

    let mut input = source.open()?;
    let mut output = AVFormatContextOutput::create(&out_url)?;

    let drop_audio = opts.drop_audio.unwrap_or(false);
    let drop_video = opts.drop_video.unwrap_or(false);
    let drop_subtitles = opts.drop_subtitles.unwrap_or(false);

    let mut pipelines: Vec<StreamPipeline> = Vec::new();
    let mut streams_copied: u32 = 0;
    let mut streams_reencoded: u32 = 0;

    for (in_idx, stream) in input.streams().iter().enumerate() {
        let codecpar = stream.codecpar();
        let codec_type = codecpar.codec_type;
        let in_tb = stream.time_base;

        if drop_codec_type(codec_type, drop_audio, drop_video, drop_subtitles) {
            // Stream is filtered out entirely; no pipeline entry means
            // packets read from this stream are dropped at dispatch
            // time.
            continue;
        }

        let want_reencode = should_reencode(codec_type, opts);

        if !want_reencode {
            let mut new_codecpar = rsmpeg::avcodec::AVCodecParameters::new();
            new_codecpar.copy(&codecpar);
            ffi_helpers::clear_codec_tag(&mut new_codecpar);

            let out_idx;
            {
                let mut out_stream = output.new_stream();
                out_stream.set_codecpar(new_codecpar);
                out_stream.set_time_base(in_tb);
                out_idx = out_stream.index;
            }
            pipelines.push(StreamPipeline::Copy {
                in_idx,
                out_idx,
                in_tb,
            });
            streams_copied += 1;
            continue;
        }

        // `should_reencode` returns true only for video / audio, so by
        // the time we reach here the codec_type is guaranteed to be
        // one of those two. Subtitles / data already left the loop
        // through the `if !want_reencode` branch above.
        match codec_type {
            ffi::AVMEDIA_TYPE_VIDEO => {
                let pipeline = build_video_pipeline(&mut output, &codecpar, in_idx, in_tb, opts)?;
                pipelines.push(pipeline);
                streams_reencoded += 1;
            }
            ffi::AVMEDIA_TYPE_AUDIO => {
                let pipeline = build_audio_pipeline(&mut output, &codecpar, in_idx, in_tb, opts)?;
                pipelines.push(pipeline);
                streams_reencoded += 1;
            }
            other => {
                return Err(NativeError::new(
                    "runtime_error",
                    "should_reencode signalled re-encode for an unexpected codec type",
                )
                .with_detail("codec_type", other.to_string()));
            }
        }
    }

    if streams_copied + streams_reencoded == 0 {
        return Err(NativeError::new(
            "invalid_request",
            "transcode dropped every input stream; nothing left to write",
        ));
    }

    if let Some(tags) = opts.tags.as_ref() {
        ffi_helpers::set_format_metadata(&mut output, tags);
    }

    let mut header_opts = None;
    output.write_header(&mut header_opts)?;

    let out_time_bases: Vec<ffi::AVRational> =
        output.streams().iter().map(|s| s.time_base).collect();

    let mut packets_written: u64 = 0;
    let mut progress =
        ProgressEmitter::from_av_duration(env, opts.progress, "transcode", input.duration);

    // Re-encoded streams get zero-based timestamps (the audio sample
    // counter and the video pts cursor both start at 0). Copied streams
    // would otherwise keep their source timestamps, which for a source
    // that does not start at 0 (MPEG-TS captures, edit-list offsets)
    // leaves a constant A/V desync equal to the container start time.
    // Normalise the whole output to a zero origin by subtracting the
    // container start time from copied packets too.
    let input_start_time = input.start_time;

    while let Some(packet) = input.read_packet()? {
        let idx = packet.stream_index as usize;
        let in_tb = input.streams()[idx].time_base;
        let pts_s = if packet.pts == ffi::AV_NOPTS_VALUE {
            0.0
        } else {
            packet.pts as f64 * f64::from(in_tb.num) / f64::from(in_tb.den)
        };

        let Some(pipeline) = pipelines.iter_mut().find(|p| p.in_index() == idx) else {
            continue;
        };

        match pipeline {
            StreamPipeline::Copy { out_idx, in_tb, .. } => {
                let mut packet = packet;
                if let Some(start) = start_offset_in_tb(input_start_time, *in_tb) {
                    if packet.pts != ffi::AV_NOPTS_VALUE {
                        packet.set_pts(packet.pts - start);
                    }
                    if packet.dts != ffi::AV_NOPTS_VALUE {
                        packet.set_dts(packet.dts - start);
                    }
                }
                packet.rescale_ts(*in_tb, out_time_bases[*out_idx as usize]);
                packet.set_stream_index(*out_idx);
                output.interleaved_write_frame(&mut packet)?;
                packets_written += 1;
            }
            StreamPipeline::Video { .. } => {
                process_video_packet(
                    pipeline,
                    Some(&packet),
                    &mut output,
                    &out_time_bases,
                    &mut packets_written,
                )?;
            }
            StreamPipeline::Audio { .. } => {
                process_audio_packet(
                    pipeline,
                    Some(&packet),
                    &mut output,
                    &out_time_bases,
                    &mut packets_written,
                )?;
            }
        }

        progress.tick(packets_written, pts_s);
    }

    // Drain every re-encoded stream.
    for pipeline in &mut pipelines {
        match pipeline {
            StreamPipeline::Video { .. } => {
                process_video_packet(
                    pipeline,
                    None,
                    &mut output,
                    &out_time_bases,
                    &mut packets_written,
                )?;
            }
            StreamPipeline::Audio { .. } => {
                process_audio_packet(
                    pipeline,
                    None,
                    &mut output,
                    &out_time_bases,
                    &mut packets_written,
                )?;
            }
            StreamPipeline::Copy { .. } => {}
        }
    }

    output.write_trailer()?;

    let duration_s = if input.duration > 0 {
        input.duration as f64 / f64::from(ffi::AV_TIME_BASE)
    } else {
        0.0
    };
    progress.finish(packets_written, duration_s);

    Ok(TranscodeStats {
        streams_copied,
        streams_reencoded,
        packets_written,
        duration_s,
    })
}

fn should_reencode(codec_type: i32, opts: &TranscodeOpts) -> bool {
    match codec_type {
        ffi::AVMEDIA_TYPE_VIDEO => !is_copy(opts.video_codec.as_deref()),
        ffi::AVMEDIA_TYPE_AUDIO => !is_copy(opts.audio_codec.as_deref()),
        _ => false,
    }
}

fn is_copy(opt: Option<&str>) -> bool {
    matches!(opt, None | Some("" | "copy"))
}

fn find_encoder_by_name_owned(name: &str) -> Result<AVCodecRef<'static>, NativeError> {
    let cname = CString::new(name)
        .map_err(|_| NativeError::new("invalid_request", "codec name contains a NUL byte"))?;
    AVCodec::find_encoder_by_name(&cname).ok_or_else(|| {
        NativeError::new("unsupported", "encoder not available in this FFmpeg build")
            .with_detail("name", name.to_owned())
    })
}

fn build_video_pipeline(
    output: &mut AVFormatContextOutput,
    codecpar: &rsmpeg::avcodec::AVCodecParametersRef<'_>,
    in_idx: usize,
    in_tb: ffi::AVRational,
    opts: &TranscodeOpts,
) -> Result<StreamPipeline, NativeError> {
    let decoder_codec = AVCodec::find_decoder(codecpar.codec_id).ok_or_else(|| {
        NativeError::new("unsupported", "no decoder available for input video stream")
    })?;
    let mut decoder = AVCodecContext::new(&decoder_codec);
    decoder.apply_codecpar(codecpar)?;
    decoder.set_time_base(in_tb);
    decoder.open(None)?;

    let encoder_codec = find_encoder_by_name_owned(opts.video_codec.as_deref().unwrap_or("h264"))?;

    let src_w = decoder.width;
    let src_h = decoder.height;
    let src_fmt = decoder.pix_fmt;
    let src_sar = decoder.sample_aspect_ratio;

    let (heuristic_w, heuristic_h) = resolve_target_size(src_w, src_h, opts.width, opts.height);
    let dst_fmt = pick_pix_fmt(&encoder_codec, src_fmt);

    let fps = opts.fps.unwrap_or_else(|| {
        let src_fps = decoder.framerate;
        if src_fps.den == 0 || src_fps.num == 0 {
            (25, 1)
        } else {
            (src_fps.num, src_fps.den)
        }
    });

    let filter_spec = build_video_filter_spec(opts, heuristic_w, heuristic_h, fps, dst_fmt);
    let graph = build_video_graph(src_w, src_h, src_fmt, in_tb, src_sar, dst_fmt, &filter_spec)?;

    // Use the post-config buffersink dimensions / pix_fmt to drive the
    // encoder. This makes the user's `:video_filter` authoritative
    // (e.g. `crop` + `scale` chained changes the final dimensions away
    // from the heuristic above).
    let (out_w, out_h, out_fmt, out_tb, out_frame_rate) = {
        let sink = graph
            .graph
            .get_filter(c"out")
            .ok_or_else(|| NativeError::new("runtime_error", "buffersink missing after config"))?;
        let tb = sink.get_time_base();
        let fr = sink.get_frame_rate();
        let frame_rate = if fr.den == 0 || fr.num == 0 {
            ffi::AVRational {
                num: fps.0,
                den: fps.1,
            }
        } else {
            fr
        };
        (
            sink.get_w(),
            sink.get_h(),
            sink.get_format(),
            tb,
            frame_rate,
        )
    };

    let mut encoder = AVCodecContext::new(&encoder_codec);
    encoder.set_width(out_w);
    encoder.set_height(out_h);
    encoder.set_pix_fmt(out_fmt);
    encoder.set_time_base(out_tb);
    encoder.set_framerate(out_frame_rate);
    if let Some(br) = opts.video_bitrate {
        encoder.set_bit_rate(br);
    }
    encoder.open(None)?;

    let out_idx;
    {
        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(encoder.extract_codecpar());
        out_stream.set_time_base(encoder.time_base);
        out_idx = out_stream.index;
    }

    Ok(StreamPipeline::Video {
        in_idx,
        out_idx,
        decoder,
        encoder,
        graph,
        next_pts: 0,
    })
}

fn build_video_filter_spec(
    opts: &TranscodeOpts,
    dst_w: i32,
    dst_h: i32,
    fps: (i32, i32),
    dst_fmt: i32,
) -> String {
    // User-supplied filter chain takes precedence and we simply pin the
    // output pix_fmt by appending `format=<dst_fmt>`. Otherwise build a
    // minimal chain from the convenience options.
    let pix_fmt_pin = format!("format={dst_fmt}");
    if let Some(user) = opts.video_filter.as_deref() {
        return format!("{user},{pix_fmt_pin}");
    }
    // Defense in depth: the Elixir validator already rejects fps tuples
    // with a non-positive denominator, but a 0-denom slipping through
    // here would emit `fps=N/0` and break the filter graph at parse
    // time. Substitute a sensible default rather than producing an
    // invalid spec.
    let (fps_num, fps_den) = if fps.0 <= 0 || fps.1 <= 0 {
        (25, 1)
    } else {
        fps
    };
    format!("scale={dst_w}:{dst_h},fps={fps_num}/{fps_den},{pix_fmt_pin}")
}

fn build_video_graph(
    src_w: i32,
    src_h: i32,
    src_fmt: i32,
    src_tb: ffi::AVRational,
    src_sar: ffi::AVRational,
    _dst_fmt: i32,
    filter_spec: &str,
) -> Result<VideoFilterGraph, NativeError> {
    let graph = AVFilterGraph::new();
    let buffersrc = AVFilter::get_by_name(c"buffer")
        .ok_or_else(|| NativeError::new("runtime_error", "filter `buffer` missing from build"))?;
    let buffersink = AVFilter::get_by_name(c"buffersink").ok_or_else(|| {
        NativeError::new("runtime_error", "filter `buffersink` missing from build")
    })?;

    let sar_num = if src_sar.num == 0 { 1 } else { src_sar.num };
    let sar_den = if src_sar.den == 0 { 1 } else { src_sar.den };
    let buffer_args = CString::new(format!(
        "video_size={src_w}x{src_h}:pix_fmt={src_fmt}:time_base={}/{}:pixel_aspect={sar_num}/{sar_den}",
        src_tb.num, src_tb.den
    ))
    .map_err(|_| {
        NativeError::new("runtime_error", "buffersrc args could not be encoded as a C string")
    })?;

    let spec = CString::new(filter_spec)
        .map_err(|_| NativeError::new("invalid_request", "video_filter contains a NUL byte"))?;

    // The buffersrc / buffersink contexts borrow from `graph`. We
    // restrict their lifetime to this inner scope so `graph` is free to
    // move into `VideoFilterGraph` at the end. The contexts are looked
    // up again per-frame via `graph.get_filter("in" | "out")` in the
    // hot path.
    {
        let mut src_ctx = graph
            .create_filter_context(&buffersrc, c"in", Some(&buffer_args))
            .map_err(|err| {
                NativeError::new("runtime_error", "failed to create buffersrc")
                    .with_detail("reason", err.to_string())
            })?;
        let mut sink_ctx = graph
            .alloc_filter_context(&buffersink, c"out")
            .ok_or_else(|| NativeError::new("runtime_error", "failed to allocate buffersink"))?;
        sink_ctx.init_dict(&mut None).map_err(|err: RsmpegError| {
            NativeError::new("runtime_error", "failed to init buffersink")
                .with_detail("reason", err.to_string())
        })?;

        let outputs = AVFilterInOut::new(c"in", &mut src_ctx, 0);
        let inputs = AVFilterInOut::new(c"out", &mut sink_ctx, 0);

        graph
            .parse_ptr(&spec, Some(inputs), Some(outputs))
            .map_err(|err| {
                NativeError::new("invalid_request", "failed to parse video_filter spec")
                    .with_detail("spec", filter_spec.to_owned())
                    .with_detail("reason", err.to_string())
            })?;
    }

    graph.config().map_err(|err| {
        NativeError::new("invalid_request", "video filter graph failed to validate")
            .with_detail("spec", filter_spec.to_owned())
            .with_detail("reason", err.to_string())
    })?;

    Ok(VideoFilterGraph { graph })
}

fn build_audio_pipeline(
    output: &mut AVFormatContextOutput,
    codecpar: &rsmpeg::avcodec::AVCodecParametersRef<'_>,
    in_idx: usize,
    in_tb: ffi::AVRational,
    opts: &TranscodeOpts,
) -> Result<StreamPipeline, NativeError> {
    let decoder_codec = AVCodec::find_decoder(codecpar.codec_id).ok_or_else(|| {
        NativeError::new("unsupported", "no decoder available for input audio stream")
    })?;
    let mut decoder = AVCodecContext::new(&decoder_codec);
    decoder.apply_codecpar(codecpar)?;
    decoder.set_time_base(in_tb);
    decoder.open(None)?;

    let encoder_codec = find_encoder_by_name_owned(opts.audio_codec.as_deref().unwrap_or("aac"))?;
    let mut encoder = AVCodecContext::new(&encoder_codec);

    let dst_rate = opts.sample_rate.unwrap_or(decoder.sample_rate).max(1);
    // Share extract_audio's policy: an omitted `:channels` carries a
    // mono/stereo source through, but a surround source (>2 channels)
    // must opt in explicitly rather than be silently downmixed.
    let dst_channels =
        crate::audio::resolve_channels(opts.channels, decoder.ch_layout.nb_channels)?;
    let dst_layout = AVChannelLayout::from_nb_channels(dst_channels);
    let dst_fmt = pick_sample_fmt(&encoder_codec, decoder.sample_fmt);

    encoder.set_sample_rate(dst_rate);
    encoder.set_sample_fmt(dst_fmt);
    encoder.set_ch_layout(dst_layout.clone().into_inner());
    encoder.set_time_base(ffi::AVRational {
        num: 1,
        den: dst_rate,
    });
    if let Some(br) = opts.audio_bitrate {
        encoder.set_bit_rate(br);
    }
    encoder.open(None)?;

    let mut swr = SwrContext::new(
        &dst_layout,
        dst_fmt,
        dst_rate,
        &decoder.ch_layout,
        decoder.sample_fmt,
        decoder.sample_rate,
    )?;
    swr.init()?;

    let out_idx;
    {
        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(encoder.extract_codecpar());
        out_stream.set_time_base(encoder.time_base);
        out_idx = out_stream.index;
    }

    let _ = in_tb;

    // AAC / many other audio encoders require fixed `frame_size`
    // samples per call (except for the final flush). We buffer
    // resampled samples in an `AVAudioFifo` and emit one chunk per
    // encoder send_frame. `frame_size == 0` means the encoder can
    // accept any size (e.g. PCM); we then treat the resampled frame as
    // the encoder unit directly.
    let frame_size = encoder.frame_size;
    let fifo = AVAudioFifo::new(dst_fmt, dst_channels, frame_size.max(1));

    Ok(StreamPipeline::Audio {
        in_idx,
        out_idx,
        decoder,
        encoder,
        swr,
        fifo,
        frame_size,
        dst_layout,
        dst_fmt,
        dst_rate,
        samples_written: 0,
    })
}

fn process_video_packet(
    pipeline: &mut StreamPipeline,
    packet: Option<&rsmpeg::avcodec::AVPacket>,
    output: &mut AVFormatContextOutput,
    out_time_bases: &[ffi::AVRational],
    packets_written: &mut u64,
) -> Result<(), NativeError> {
    let StreamPipeline::Video {
        out_idx,
        decoder,
        encoder,
        graph,
        next_pts,
        ..
    } = pipeline
    else {
        return Ok(());
    };

    decoder.send_packet(packet)?;
    loop {
        let frame = match decoder.receive_frame() {
            Ok(f) => f,
            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        };

        push_video_frame_through_graph(graph, Some(&frame))?;
        drain_filter_and_encode(
            graph,
            encoder,
            output,
            *out_idx,
            out_time_bases,
            next_pts,
            packets_written,
        )?;
    }

    if packet.is_none() {
        // Signal EOF to the filter graph, drain everything still buffered
        // there, then flush the encoder.
        push_video_frame_through_graph(graph, None)?;
        drain_filter_and_encode(
            graph,
            encoder,
            output,
            *out_idx,
            out_time_bases,
            next_pts,
            packets_written,
        )?;
        encoder.send_frame(None)?;
        write_drained_packets(encoder, output, *out_idx, out_time_bases, packets_written)?;
    }
    Ok(())
}

fn push_video_frame_through_graph(
    graph: &VideoFilterGraph,
    frame: Option<&AVFrame>,
) -> Result<(), NativeError> {
    let mut src = graph
        .graph
        .get_filter(c"in")
        .ok_or_else(|| NativeError::new("runtime_error", "buffersrc context vanished"))?;
    src.buffersrc_add_frame(frame.cloned(), None)
        .map_err(NativeError::from)
}

fn drain_filter_and_encode(
    graph: &VideoFilterGraph,
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_idx: i32,
    out_time_bases: &[ffi::AVRational],
    next_pts: &mut i64,
    packets_written: &mut u64,
) -> Result<(), NativeError> {
    loop {
        let mut sink = graph
            .graph
            .get_filter(c"out")
            .ok_or_else(|| NativeError::new("runtime_error", "buffersink context vanished"))?;
        let mut filtered = match sink.buffersink_get_frame(None) {
            Ok(f) => f,
            Err(RsmpegError::BufferSinkDrainError | RsmpegError::BufferSinkEofError) => {
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };

        // Re-time to a monotonically-increasing pts in the encoder's
        // time_base (1/fps). The filter graph reports time_base on the
        // sink, but normalising here keeps the encoder happy regardless
        // of what fps the user requested via :video_filter.
        filtered.set_pts(*next_pts);
        *next_pts += 1;

        encoder.send_frame(Some(&filtered))?;
        write_drained_packets(encoder, output, out_idx, out_time_bases, packets_written)?;
    }
}

fn process_audio_packet(
    pipeline: &mut StreamPipeline,
    packet: Option<&rsmpeg::avcodec::AVPacket>,
    output: &mut AVFormatContextOutput,
    out_time_bases: &[ffi::AVRational],
    packets_written: &mut u64,
) -> Result<(), NativeError> {
    let StreamPipeline::Audio {
        out_idx,
        decoder,
        encoder,
        swr,
        fifo,
        frame_size,
        dst_layout,
        dst_fmt,
        dst_rate,
        samples_written,
        ..
    } = pipeline
    else {
        return Ok(());
    };

    decoder.send_packet(packet)?;
    loop {
        let frame = match decoder.receive_frame() {
            Ok(f) => f,
            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        };

        let mut resampled = alloc_resample_frame(&frame, dst_layout, *dst_fmt, *dst_rate)?;
        swr.convert_frame(Some(&frame), &mut resampled)?;
        if resampled.nb_samples > 0 {
            ffi_helpers::write_fifo_frame(fifo, &resampled)?;
        }

        flush_fifo_to_encoder(
            fifo,
            *frame_size,
            *dst_fmt,
            *dst_rate,
            dst_layout,
            samples_written,
            encoder,
            output,
            *out_idx,
            out_time_bases,
            packets_written,
            false,
        )?;
    }

    if packet.is_none() {
        // Drain swresample, then flush the FIFO including a possibly
        // partial last frame.
        loop {
            let mut tail = empty_resample_frame(dst_layout, *dst_fmt, *dst_rate)?;
            swr.convert_frame(None, &mut tail)?;
            if tail.nb_samples == 0 {
                break;
            }
            ffi_helpers::write_fifo_frame(fifo, &tail)?;
        }
        flush_fifo_to_encoder(
            fifo,
            *frame_size,
            *dst_fmt,
            *dst_rate,
            dst_layout,
            samples_written,
            encoder,
            output,
            *out_idx,
            out_time_bases,
            packets_written,
            true,
        )?;
        encoder.send_frame(None)?;
        write_drained_packets(encoder, output, *out_idx, out_time_bases, packets_written)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn flush_fifo_to_encoder(
    fifo: &mut AVAudioFifo,
    frame_size: i32,
    fmt: i32,
    sample_rate: i32,
    layout: &AVChannelLayout,
    samples_written: &mut i64,
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_idx: i32,
    out_time_bases: &[ffi::AVRational],
    packets_written: &mut u64,
    drain_partial: bool,
) -> Result<(), NativeError> {
    let chunk = if frame_size > 0 { frame_size } else { 1024 };
    loop {
        let available = fifo.size();
        if available == 0 {
            return Ok(());
        }
        let take = if available >= chunk {
            chunk
        } else if drain_partial {
            available
        } else {
            return Ok(());
        };
        let frame = read_fifo_frame(fifo, take, fmt, sample_rate, layout)?;
        encode_audio_frame(
            frame,
            samples_written,
            encoder,
            output,
            out_idx,
            out_time_bases,
            packets_written,
        )?;
    }
}

fn read_fifo_frame(
    fifo: &mut AVAudioFifo,
    nb_samples: i32,
    fmt: i32,
    sample_rate: i32,
    layout: &AVChannelLayout,
) -> Result<AVFrame, NativeError> {
    let mut frame = AVFrame::new();
    frame.set_nb_samples(nb_samples);
    frame.set_sample_rate(sample_rate);
    frame.set_format(fmt);
    frame.set_ch_layout(layout.clone().into_inner());
    frame.get_buffer(0)?;

    let read = ffi_helpers::read_fifo_into_frame(fifo, &mut frame, nb_samples)?;
    if read != nb_samples {
        // Truncate frame length so the encoder sees only what was read.
        frame.set_nb_samples(read);
    }
    Ok(frame)
}

#[allow(clippy::too_many_arguments)]
fn encode_audio_frame(
    mut frame: AVFrame,
    samples_written: &mut i64,
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_idx: i32,
    out_time_bases: &[ffi::AVRational],
    packets_written: &mut u64,
) -> Result<(), NativeError> {
    frame.set_pts(*samples_written);
    *samples_written += i64::from(frame.nb_samples);
    encoder.send_frame(Some(&frame))?;
    write_drained_packets(encoder, output, out_idx, out_time_bases, packets_written)
}

fn write_drained_packets(
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_idx: i32,
    out_time_bases: &[ffi::AVRational],
    packets_written: &mut u64,
) -> Result<(), NativeError> {
    loop {
        match encoder.receive_packet() {
            Ok(mut packet) => {
                packet.rescale_ts(encoder.time_base, out_time_bases[out_idx as usize]);
                packet.set_stream_index(out_idx);
                output.interleaved_write_frame(&mut packet)?;
                *packets_written += 1;
            }
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn alloc_resample_frame(
    src: &AVFrame,
    layout: &AVChannelLayout,
    fmt: i32,
    sample_rate: i32,
) -> Result<AVFrame, NativeError> {
    let nb_samples = compute_resample_capacity(src.nb_samples, src.sample_rate, sample_rate);
    let mut dst = AVFrame::new();
    dst.set_nb_samples(nb_samples);
    dst.set_sample_rate(sample_rate);
    dst.set_format(fmt);
    dst.set_ch_layout(layout.clone().into_inner());
    dst.get_buffer(0)?;
    Ok(dst)
}

/// Worst-case output sample count for a resample step, with a small
/// margin so the FIFO never has to grow at write time. Computed in i64
/// and clamped to a safe i32 ceiling: pathological inputs (e.g. a
/// corrupt `src.sample_rate == 0` clamped to 1 with a high target
/// rate) would otherwise overflow `as i32` and produce a negative
/// `nb_samples` that crashes `AVFrame::get_buffer`.
fn compute_resample_capacity(src_nb_samples: i32, src_rate: i32, dst_rate: i32) -> i32 {
    const MAX_NB_SAMPLES: i64 = 1 << 20; // 1 Mi-samples is far past any real audio frame.
    if src_nb_samples <= 0 {
        return 4096;
    }
    let raw = i64::from(src_nb_samples) * i64::from(dst_rate.max(1)) / i64::from(src_rate.max(1));
    raw.saturating_add(256).clamp(1, MAX_NB_SAMPLES) as i32
}

fn empty_resample_frame(
    layout: &AVChannelLayout,
    fmt: i32,
    sample_rate: i32,
) -> Result<AVFrame, NativeError> {
    let mut dst = AVFrame::new();
    dst.set_nb_samples(4096);
    dst.set_sample_rate(sample_rate);
    dst.set_format(fmt);
    dst.set_ch_layout(layout.clone().into_inner());
    dst.get_buffer(0)?;
    Ok(dst)
}

fn pick_pix_fmt(codec: &AVCodecRef<'static>, src: i32) -> i32 {
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

fn pick_sample_fmt(codec: &AVCodecRef<'static>, src: i32) -> i32 {
    if let Some(fmts) = codec.sample_fmts() {
        if fmts.contains(&src) {
            return src;
        }
        if let Some(first) = fmts.first() {
            return *first;
        }
    }
    src
}

fn resolve_target_size(src_w: i32, src_h: i32, w: Option<i32>, h: Option<i32>) -> (i32, i32) {
    let (w, h) = match (w, h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let h = (i64::from(w) * i64::from(src_h) / i64::from(src_w.max(1))) as i32;
            (w, h)
        }
        (None, Some(h)) => {
            let w = (i64::from(h) * i64::from(src_w) / i64::from(src_h.max(1))) as i32;
            (w, h)
        }
        (None, None) => (src_w, src_h),
    };
    (round_even(w), round_even(h))
}

#[inline]
fn round_even(n: i32) -> i32 {
    // Round down to the nearest even value, clamping to 2 for inputs
    // <= 1. `1 & !1` is 0 (which is an invalid video dimension), so the
    // guard must catch 1 as well as 0 and negative values.
    if n <= 1 { 2 } else { n & !1 }
}

fn to_cstring(path: &Path) -> Result<CString, NativeError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })
}

/// The container start time, rescaled from `AV_TIME_BASE` units into a
/// stream's time_base, to subtract from copied packets. `None` when the
/// source already starts at zero or carries no start time, so the common
/// case adds no work and no shift.
fn start_offset_in_tb(start_time: i64, stream_tb: ffi::AVRational) -> Option<i64> {
    if start_time == ffi::AV_NOPTS_VALUE || start_time <= 0 {
        return None;
    }
    let time_base_q = ffi::AVRational {
        num: 1,
        den: ffi::AV_TIME_BASE as i32,
    };
    Some(av_rescale_q(start_time, time_base_q, stream_tb))
}

fn drop_codec_type(
    codec_type: i32,
    drop_audio: bool,
    drop_video: bool,
    drop_subtitles: bool,
) -> bool {
    match codec_type {
        ffi::AVMEDIA_TYPE_AUDIO => drop_audio,
        ffi::AVMEDIA_TYPE_VIDEO => drop_video,
        ffi::AVMEDIA_TYPE_SUBTITLE => drop_subtitles,
        _ => false,
    }
}

impl StreamPipeline {
    fn in_index(&self) -> usize {
        match self {
            StreamPipeline::Copy { in_idx, .. }
            | StreamPipeline::Video { in_idx, .. }
            | StreamPipeline::Audio { in_idx, .. } => *in_idx,
        }
    }
}

// Suppress unused-import warning: `CStr` is used via the `c"..."` literal
// macro in error metadata. Keeping the import explicit avoids an
// unused-import warning while making future use obvious.
#[allow(dead_code)]
const _UNUSED: &CStr = c"";
