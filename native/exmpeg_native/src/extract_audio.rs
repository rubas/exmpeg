//! Audio extraction: decode the best audio stream of an input,
//! resample to the target sample rate / channel count, and write it
//! through the encoder matching the output extension.
//!
//! Replaces `ffmpeg -i in -vn out.<ext>` where `<ext>` is one of
//! `wav`, `mp3`, `m4a` / `aac`, `opus` / `ogg`, `flac`.

use std::ffi::CString;
use std::path::Path;

use rsmpeg::avcodec::{AVCodec, AVCodecContext, AVCodecRef};
use rsmpeg::avformat::{AVFormatContextInput, AVFormatContextOutput};
use rsmpeg::avutil::{AVAudioFifo, AVChannelLayout, AVFrame};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swresample::SwrContext;
use rustler::types::LocalPid;
use rustler::{Env, NifMap};

use crate::errors::NativeError;
use crate::ffi_helpers;
use crate::progress::ProgressEmitter;

#[derive(Default, NifMap)]
pub(crate) struct ExtractAudioOpts {
    /// Target sample rate in Hz. When `None`, the source sample rate is
    /// preserved.
    pub(crate) sample_rate: Option<i32>,
    /// Target channel count. Currently restricted to 1 (mono) or 2
    /// (stereo); other counts return `:invalid_request`. When `None`,
    /// the source channel count is preserved if it is mono or stereo;
    /// sources with more channels (5.1, 7.1, ...) return
    /// `:invalid_request` and require an explicit `:channels` value.
    pub(crate) channels: Option<i32>,
    /// Optional target bitrate in bits per second. Ignored for
    /// lossless codecs (`pcm_s16le`, `flac`); used as a hint for
    /// `libmp3lame` / `libopus` / `aac`.
    pub(crate) bitrate: Option<i64>,
    /// Optional pid that receives throttled `{:exmpeg_progress, %{...}}`
    /// messages during the decode/encode loop.
    pub(crate) progress: Option<LocalPid>,
}

#[derive(Debug, NifMap)]
pub(crate) struct ExtractAudioStats {
    pub(crate) sample_rate: i32,
    pub(crate) channels: i32,
    pub(crate) samples_written: u64,
    pub(crate) duration_s: f64,
    pub(crate) codec: String,
}

#[allow(clippy::too_many_lines)] // Open -> decode -> resample -> encode -> mux is naturally linear.
pub(crate) fn extract_audio<Q: AsRef<Path>>(
    env: Env<'_>,
    source: crate::input::InputSource,
    output_path: Q,
    opts: &ExtractAudioOpts,
) -> Result<ExtractAudioStats, NativeError> {
    let output_path = output_path.as_ref();

    let ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let encoder_codec = pick_encoder(ext.as_deref())?;

    let out_url = to_cstring(output_path)?;

    let mut input = source.open()?;
    let (audio_index, decoder_codec) = find_audio_stream(&input)?;

    let stream_time_base = input.streams()[audio_index].time_base;
    let mut decoder = AVCodecContext::new(&decoder_codec);
    {
        let codecpar = input.streams()[audio_index].codecpar();
        decoder.apply_codecpar(&codecpar)?;
    }
    decoder.set_time_base(stream_time_base);
    decoder.open(None)?;

    let target_rate = pick_sample_rate(&encoder_codec, opts.sample_rate, decoder.sample_rate);
    let target_channels = resolve_channels(opts.channels, decoder.ch_layout.nb_channels)?;
    let target_layout = AVChannelLayout::from_nb_channels(target_channels);
    let target_fmt = pick_sample_fmt(&encoder_codec, decoder.sample_fmt);

    let mut encoder = AVCodecContext::new(&encoder_codec);
    encoder.set_sample_rate(target_rate);
    encoder.set_sample_fmt(target_fmt);
    encoder.set_ch_layout(target_layout.clone().into_inner());
    encoder.set_time_base(ffi::AVRational {
        num: 1,
        den: target_rate,
    });
    if let Some(br) = opts.bitrate {
        encoder.set_bit_rate(br);
    }
    encoder.open(None)?;

    // Fast path: when the decoded frames are already in the target
    // format/rate/layout and the encoder accepts arbitrary chunk sizes
    // (pcm_*, which report `frame_size == 0`), the resampler is a pure
    // copy and the FIFO only re-chunks for nothing. Skip both and send
    // each decoded frame straight to the encoder.
    //
    // The layout must match for this to be safe: a 2-channel source with
    // a *specific* non-stereo mask (e.g. WAVEFORMATEXTENSIBLE FL+LFE)
    // sent unremapped under the canonical target layout the output
    // stream advertises would mislabel/misorder its channels. An
    // *unspecified* source order with the matching channel count has no
    // order to preserve, so passthrough under the canonical target is
    // fine (and avoids the resampler for the common WAV case, which
    // reports its layout as unspecified).
    let layout_ok = ffi_helpers::channel_layouts_equal(&decoder.ch_layout, &target_layout)
        || (decoder.ch_layout.order == ffi::AV_CHANNEL_ORDER_UNSPEC
            && decoder.ch_layout.nb_channels == target_channels);
    let passthrough = encoder.frame_size == 0
        && decoder.sample_fmt == target_fmt
        && decoder.sample_rate == target_rate
        && layout_ok;

    let mut swr = if passthrough {
        None
    } else {
        let mut swr = SwrContext::new(
            &target_layout,
            target_fmt,
            target_rate,
            &decoder.ch_layout,
            decoder.sample_fmt,
            decoder.sample_rate,
        )?;
        swr.init()?;
        Some(swr)
    };

    let mut output = AVFormatContextOutput::create(&out_url)?;
    let codec_name = encoder_codec.name().to_string_lossy().into_owned();
    {
        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(encoder.extract_codecpar());
        out_stream.set_time_base(encoder.time_base);
    }
    let mut header_opts = None;
    output.write_header(&mut header_opts)?;

    // The muxer may override the requested stream time_base during
    // write_header (the Ogg/Opus muxer pins 1/48000); rescale encoder
    // packets into whatever it actually chose.
    let out_tb = output.streams()[0].time_base;

    // Codecs with a fixed `frame_size` (AAC, Opus, MP3) reject arbitrary
    // chunk sizes; we buffer through an AVAudioFifo and emit exactly
    // `frame_size` per encode call. PCM and FLAC accept any size and
    // run with chunk == 1024 by default.
    let frame_size = if encoder.frame_size > 0 {
        encoder.frame_size
    } else {
        1024
    };
    let mut fifo = if passthrough {
        None
    } else {
        Some(AVAudioFifo::new(target_fmt, target_channels, frame_size))
    };

    let mut samples_written: u64 = 0;
    let mut progress =
        ProgressEmitter::from_av_duration(env, opts.progress, "extract_audio", input.duration);

    while let Some(packet) = input.read_packet()? {
        if packet.stream_index as usize != audio_index {
            continue;
        }
        decoder.send_packet(Some(&packet))?;

        loop {
            let frame = match decoder.receive_frame() {
                Ok(f) => f,
                Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
                Err(err) => return Err(err.into()),
            };

            if let (Some(swr), Some(fifo)) = (swr.as_mut(), fifo.as_mut()) {
                let mut resampled =
                    alloc_resample_frame(&frame, &target_layout, target_fmt, target_rate)?;
                swr.convert_frame(Some(&frame), &mut resampled)?;
                if resampled.nb_samples > 0 {
                    ffi_helpers::write_fifo_frame(fifo, &resampled)?;
                }
                drain_fifo(
                    fifo,
                    frame_size,
                    target_fmt,
                    target_rate,
                    &target_layout,
                    &mut samples_written,
                    &mut encoder,
                    &mut output,
                    out_tb,
                    false,
                )?;
            } else {
                encode_pcm_frame(
                    frame,
                    &mut samples_written,
                    &mut encoder,
                    &mut output,
                    out_tb,
                )?;
            }
            progress.tick(
                samples_written,
                samples_written as f64 / f64::from(target_rate),
            );
        }
    }

    // Flush order matters end-to-end:
    //
    //   1. Tell the decoder there are no more packets and pull every
    //      remaining frame; codecs with decoder delay (AAC, MP3, ...)
    //      buffer the last few frames internally until they see EOF.
    //   2. Push each flushed frame through swresample + FIFO.
    //   3. Drain swresample with a None input so it emits its own
    //      buffered samples.
    //   4. Drain the FIFO including a possibly-partial last frame.
    //   5. Flush the encoder.
    //
    // Doing (3) before (1) -- the previous order -- discarded every
    // decoder-buffered tail frame, so codecs with priming on the input
    // side produced truncated output.
    decoder.send_packet(None)?;
    loop {
        let frame = match decoder.receive_frame() {
            Ok(f) => f,
            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
            Err(err) => return Err(err.into()),
        };
        if let (Some(swr), Some(fifo)) = (swr.as_mut(), fifo.as_mut()) {
            let mut resampled =
                alloc_resample_frame(&frame, &target_layout, target_fmt, target_rate)?;
            swr.convert_frame(Some(&frame), &mut resampled)?;
            if resampled.nb_samples > 0 {
                ffi_helpers::write_fifo_frame(fifo, &resampled)?;
            }
            drain_fifo(
                fifo,
                frame_size,
                target_fmt,
                target_rate,
                &target_layout,
                &mut samples_written,
                &mut encoder,
                &mut output,
                out_tb,
                false,
            )?;
        } else {
            encode_pcm_frame(
                frame,
                &mut samples_written,
                &mut encoder,
                &mut output,
                out_tb,
            )?;
        }
    }

    // The resampler and FIFO only exist on the conversion path; drain
    // both before flushing the encoder. The passthrough path has nothing
    // buffered between the decoder and the encoder.
    if let (Some(swr), Some(fifo)) = (swr.as_mut(), fifo.as_mut()) {
        loop {
            let mut tail = empty_resample_frame(&target_layout, target_fmt, target_rate)?;
            swr.convert_frame(None, &mut tail)?;
            if tail.nb_samples == 0 {
                break;
            }
            ffi_helpers::write_fifo_frame(fifo, &tail)?;
        }
        drain_fifo(
            fifo,
            frame_size,
            target_fmt,
            target_rate,
            &target_layout,
            &mut samples_written,
            &mut encoder,
            &mut output,
            out_tb,
            true,
        )?;
    }

    encoder.send_frame(None)?;
    write_drained_packets(&mut encoder, &mut output, out_tb)?;

    output.write_trailer()?;

    let duration_s = samples_written as f64 / f64::from(target_rate);
    progress.finish(samples_written, duration_s);

    Ok(ExtractAudioStats {
        sample_rate: target_rate,
        channels: target_channels,
        samples_written,
        duration_s,
        codec: codec_name,
    })
}

fn pick_encoder(ext: Option<&str>) -> Result<AVCodecRef<'static>, NativeError> {
    let (codec_name, supported) = match ext {
        Some("wav") => ("pcm_s16le", true),
        Some("mp3") => ("libmp3lame", true),
        Some("m4a" | "aac") => ("aac", true),
        Some("opus" | "ogg") => ("libopus", true),
        Some("flac") => ("flac", true),
        other => (other.unwrap_or("<none>"), false),
    };
    if !supported {
        return Err(NativeError::new(
            "invalid_request",
            "unsupported audio output extension; use .wav, .mp3, .m4a/.aac, .opus/.ogg, or .flac",
        )
        .with_detail("extension", ext.unwrap_or("<none>").to_owned()));
    }
    let cname = CString::new(codec_name).expect("static codec name has no NUL");
    AVCodec::find_encoder_by_name(&cname).ok_or_else(|| {
        NativeError::new(
            "unsupported",
            "encoder for this audio extension is not built into FFmpeg",
        )
        .with_detail("encoder", codec_name.to_owned())
    })
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

fn pick_sample_rate(codec: &AVCodecRef<'static>, requested: Option<i32>, src: i32) -> i32 {
    let candidate = requested.unwrap_or(src).max(1);
    if let Some(rates) = codec.supported_samplerates() {
        // The encoder accepts only a fixed list (libopus: 8/12/16/24/48k;
        // some others have similar lists). If the candidate is not on
        // it, snap to the closest supported value so we surface a real
        // output instead of an encoder-open error.
        if !rates.is_empty() && !rates.contains(&candidate) {
            return *rates
                .iter()
                .min_by_key(|r| (candidate - **r).abs())
                .unwrap_or(&candidate);
        }
    }
    candidate
}

#[allow(clippy::too_many_arguments)]
fn drain_fifo(
    fifo: &mut AVAudioFifo,
    frame_size: i32,
    fmt: i32,
    sample_rate: i32,
    layout: &AVChannelLayout,
    samples_written: &mut u64,
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_tb: ffi::AVRational,
    drain_partial: bool,
) -> Result<(), NativeError> {
    loop {
        let available = fifo.size();
        if available == 0 {
            return Ok(());
        }
        let take = if available >= frame_size {
            frame_size
        } else if drain_partial {
            available
        } else {
            return Ok(());
        };

        let mut frame = AVFrame::new();
        frame.set_nb_samples(take);
        frame.set_sample_rate(sample_rate);
        frame.set_format(fmt);
        frame.set_ch_layout(layout.clone().into_inner());
        frame.get_buffer(0)?;
        let read = ffi_helpers::read_fifo_into_frame(fifo, &mut frame, take)?;
        if read != take {
            frame.set_nb_samples(read);
        }
        // Monotonic PTS in encoder time_base (1 / sample_rate). Some
        // muxers (WAV) print a warning otherwise.
        frame.set_pts(*samples_written as i64);
        *samples_written += u64::try_from(read).unwrap_or(0);

        encoder.send_frame(Some(&frame))?;
        write_drained_packets(encoder, output, out_tb)?;
    }
}

/// Encode a decoded frame straight through (the passthrough path), with
/// a monotonically-increasing pts in the encoder time_base (1 / rate),
/// mirroring what `drain_fifo` does on the conversion path.
fn encode_pcm_frame(
    mut frame: AVFrame,
    samples_written: &mut u64,
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_tb: ffi::AVRational,
) -> Result<(), NativeError> {
    frame.set_pts(*samples_written as i64);
    *samples_written += u64::try_from(frame.nb_samples).unwrap_or(0);
    encoder.send_frame(Some(&frame))?;
    write_drained_packets(encoder, output, out_tb)?;
    Ok(())
}

fn write_drained_packets(
    encoder: &mut AVCodecContext,
    output: &mut AVFormatContextOutput,
    out_tb: ffi::AVRational,
) -> Result<(), NativeError> {
    let enc_tb = encoder.time_base;
    loop {
        match encoder.receive_packet() {
            Ok(mut packet) => {
                packet.set_stream_index(0);
                // Encoder packets carry timestamps in the encoder
                // time_base (1/sample_rate); rescale into the muxer's
                // chosen stream time_base before writing.
                packet.rescale_ts(enc_tb, out_tb);
                output.interleaved_write_frame(&mut packet)?;
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

/// Worst-case output sample count for a resample step, with a small
/// margin so the FIFO never has to grow at write time. Computed in i64
/// and clamped to a safe i32 ceiling: pathological inputs (e.g. a
/// corrupt `src_rate == 0` clamped to 1 with a high target rate) would
/// otherwise overflow `as i32` and produce a negative `nb_samples`
/// that crashes `AVFrame::get_buffer`.
fn compute_resample_capacity(src_nb_samples: i32, src_rate: i32, dst_rate: i32) -> i32 {
    const MAX_NB_SAMPLES: i64 = 1 << 20; // 1 Mi-samples is far past any real audio frame.
    if src_nb_samples <= 0 {
        return 4096;
    }
    let raw = i64::from(src_nb_samples) * i64::from(dst_rate.max(1)) / i64::from(src_rate.max(1));
    raw.saturating_add(256).clamp(1, MAX_NB_SAMPLES) as i32
}

fn resolve_channels(requested: Option<i32>, src: i32) -> Result<i32, NativeError> {
    // When the caller hasn't asked for a specific layout we only carry
    // mono / stereo sources through unchanged. A source with more
    // channels (5.1, 7.1, ...) would otherwise be silently downmixed,
    // which hides the layout change from downstream callers and
    // violates the project's no-hidden-fallbacks rule. Force the
    // caller to opt in to mono or stereo explicitly via `:channels`.
    let target = if let Some(value) = requested {
        value
    } else if (1..=2).contains(&src) {
        src
    } else {
        return Err(NativeError::new(
            "invalid_request",
            "source has more than 2 channels; pass `:channels` (1 or 2) to choose mono or stereo",
        )
        .with_detail("source_channels", src.to_string()));
    };
    if !(1..=2).contains(&target) {
        return Err(
            NativeError::new("invalid_request", "channels must be 1 (mono) or 2 (stereo)")
                .with_detail("channels", target.to_string()),
        );
    }
    Ok(target)
}

fn find_audio_stream(
    input: &AVFormatContextInput,
) -> Result<(usize, rsmpeg::avcodec::AVCodecRef<'static>), NativeError> {
    match input.find_best_stream(ffi::AVMEDIA_TYPE_AUDIO) {
        Ok(Some(pair)) => Ok(pair),
        Ok(None) => Err(NativeError::new(
            "invalid_request",
            "input contains no audio stream",
        )),
        Err(err) => Err(err.into()),
    }
}

fn to_cstring(path: &Path) -> Result<CString, NativeError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })
}
