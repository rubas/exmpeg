//! Concatenation of multiple inputs into one output container without
//! re-encoding. Replaces `ffmpeg -f concat -i list.txt -c copy out`.
//!
//! Every input is opened, checked against the first input, and closed
//! before the first packet is written. The copy then reopens the inputs
//! one at a time, stream-copies their packets to the output, and moves
//! pts/dts from the input's own start time to the cumulative duration of
//! the preceding inputs, so the resulting timeline starts at zero and has
//! no gap at a join.
//!
//! All inputs must share the same stream layout (same number of streams,
//! same codec id per stream index) and the same codec parameters: the
//! same profile per stream, sample rate, sample format, and channel
//! layout per audio stream, and size and pixel format per video stream.
//! Only the time base may differ. Mismatches return `:invalid_request`.

use std::ffi::{CStr, CString};
use std::path::Path;

use rsmpeg::avcodec::AVCodecParameters;
use rsmpeg::avformat::{AVFormatContextInput, AVFormatContextOutput, AVOutputFormat};
use rsmpeg::avutil::{av_rescale_q, get_pix_fmt_name, get_sample_fmt_name};
use rsmpeg::ffi;
use rustler::types::LocalPid;
use rustler::{Env, NifMap};

use crate::cancel::CancelGuard;
use crate::errors::NativeError;
use crate::ffi_helpers;
use crate::progress::ProgressEmitter;

const AV_TIME_BASE_Q: ffi::AVRational = ffi::AVRational {
    num: 1,
    den: ffi::AV_TIME_BASE as i32,
};

#[derive(Default, NifMap)]
pub(crate) struct ConcatOpts {
    /// Optional pid that receives throttled `{:exmpeg_progress, %{...}}`
    /// messages during the copy loop.
    pub(crate) progress: Option<LocalPid>,
}

#[derive(Debug, NifMap)]
pub(crate) struct ConcatStats {
    pub(crate) packets_written: u64,
    pub(crate) inputs_joined: u32,
    pub(crate) streams_copied: u32,
    pub(crate) duration_s: f64,
}

pub(crate) fn concat<P: AsRef<Path>>(
    env: Env<'_>,
    sources: &[crate::input::InputSource],
    output_path: P,
    opts: &ConcatOpts,
) -> Result<ConcatStats, NativeError> {
    if sources.is_empty() {
        return Err(NativeError::new(
            "invalid_request",
            "concat requires at least one input",
        ));
    }

    let output_path = output_path.as_ref();
    let out_url = to_cstring(output_path)?;

    // No muxer matches the output extension: surface `unsupported` rather
    // than the generic io_error `create` would otherwise produce.
    if AVOutputFormat::guess_format(None, Some(&out_url), None).is_none() {
        return Err(
            NativeError::new("unsupported", "no muxer for the output extension")
                .with_detail("output", output_path.display().to_string()),
        );
    }

    let mut output = AVFormatContextOutput::create(&out_url)?;
    let mut cancel = CancelGuard::new(env);

    // Check every input before the first packet is written: stream copy
    // writes the first input's codec parameters into the output header,
    // so a later input that differs would be corrupt. Each checked input
    // is closed again, so only two inputs are open at any time.
    let first = open_input(&sources[0])?;
    for source in &sources[1..] {
        cancel.check()?;
        assert_layout_matches(&first, &open_input(source)?, &source.describe())?;
    }

    for in_stream in first.streams() {
        let mut new_codecpar = AVCodecParameters::new();
        new_codecpar.copy(&in_stream.codecpar());
        ffi_helpers::clear_codec_tag(&mut new_codecpar);

        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(new_codecpar);
        out_stream.set_time_base(in_stream.time_base);
    }
    let streams_copied = first.streams().len();

    let mut header_opts = None;
    output
        .write_header(&mut header_opts)
        .map_err(crate::errors::classify_write_header_error)?;

    // Snapshot the muxer's chosen output time_base. Some muxers (notably
    // mp4) override what we requested at new_stream time; we rescale
    // every packet into the actual time_base before writing.
    let out_time_bases: Vec<ffi::AVRational> =
        output.streams().iter().map(|s| s.time_base).collect();

    // Cumulative offset per stream, in that stream's output time_base.
    let mut pts_offset: Vec<i64> = vec![0; streams_copied];
    // Minimum dts the next packet of each stream must hit. Used to
    // patch over AAC encoder priming (negative pts) and other small
    // per-frame shifts that would otherwise produce a non-monotonic
    // dts at input boundaries.
    let mut next_min_dts: Vec<i64> = vec![i64::MIN / 2; streams_copied];
    let mut packets_written: u64 = 0;
    let mut total_duration_s: f64 = 0.0;
    // For concat the input duration is unknown up front (we'd need to
    // sum every input's container duration before opening), so report
    // `0.0` and let the caller infer progress from packet count.
    let mut progress = ProgressEmitter::new(env, opts.progress, "concat", 0.0);

    // The copy reopens the checked inputs one at a time.
    for input in std::iter::once(Ok(first)).chain(sources[1..].iter().map(open_input)) {
        let mut input = input?;
        process_input(
            &mut input,
            &mut output,
            &out_time_bases,
            &pts_offset,
            &mut next_min_dts,
            &mut packets_written,
            &mut cancel,
        )?;
        advance_offsets(
            &input,
            &out_time_bases,
            &mut pts_offset,
            &next_min_dts,
            &mut total_duration_s,
        );
        progress.tick(packets_written, total_duration_s);
    }

    output.write_trailer()?;
    progress.finish(packets_written, total_duration_s);

    Ok(ConcatStats {
        packets_written,
        inputs_joined: sources.len() as u32,
        streams_copied: streams_copied as u32,
        duration_s: total_duration_s,
    })
}

fn open_input(source: &crate::input::InputSource) -> Result<AVFormatContextInput, NativeError> {
    source
        .clone()
        .open()
        .map_err(|e| e.with_detail("path", source.describe()))
}

fn process_input(
    input: &mut AVFormatContextInput,
    output: &mut AVFormatContextOutput,
    out_time_bases: &[ffi::AVRational],
    pts_offset: &[i64],
    next_min_dts: &mut [i64],
    packets_written: &mut u64,
    cancel: &mut CancelGuard,
) -> Result<(), NativeError> {
    // Move the input to a zero origin before the cumulative offset, as
    // `ffmpeg -f concat` does. An MPEG-TS capture or an MP4 with an
    // edit-list offset starts well after zero, and that start would
    // otherwise stay in the join as a gap. One origin for the whole
    // container, not one per stream, so the streams of an input keep
    // their offsets to each other.
    let start_time = if input.start_time == ffi::AV_NOPTS_VALUE {
        0
    } else {
        input.start_time
    };
    let origin: Vec<i64> = out_time_bases
        .iter()
        .map(|&tb| av_rescale_q(start_time, AV_TIME_BASE_Q, tb))
        .collect();
    while let Some(mut packet) = input.read_packet()? {
        cancel.check()?;
        let idx = packet.stream_index as usize;
        if idx >= out_time_bases.len() {
            continue;
        }
        let in_tb = input.streams()[idx].time_base;
        let out_tb = out_time_bases[idx];

        // Rescale into the output time_base first, then offset (also in
        // output time_base). Mixing scales by applying the offset before
        // the rescale loses monotonicity whenever in_tb and out_tb
        // differ.
        packet.rescale_ts(in_tb, out_tb);

        let offset = pts_offset[idx] - origin[idx];
        if packet.pts != ffi::AV_NOPTS_VALUE {
            packet.set_pts(packet.pts + offset);
        }
        if packet.dts != ffi::AV_NOPTS_VALUE {
            packet.set_dts(packet.dts + offset);
        }

        // Enforce monotonic dts. AAC frames carry an encoder-priming
        // offset (the first packet has a small negative pts), so a
        // duration-derived offset isn't enough to push the first
        // packet of a new input past the last packet of the previous
        // one. If a packet would go backward, shift both dts and pts
        // by the deficit; future packets stay aligned because we ratchet
        // `next_min_dts` forward by the original duration.
        if packet.dts != ffi::AV_NOPTS_VALUE && packet.dts < next_min_dts[idx] {
            let shift = next_min_dts[idx] - packet.dts;
            packet.set_dts(packet.dts + shift);
            if packet.pts != ffi::AV_NOPTS_VALUE {
                packet.set_pts(packet.pts + shift);
            }
        }

        let advance = if packet.duration > 0 {
            packet.duration
        } else {
            1
        };
        if packet.dts != ffi::AV_NOPTS_VALUE {
            next_min_dts[idx] = packet.dts + advance;
        }
        packet.set_stream_index(idx as i32);

        // `write_frame` (non-interleaved) is required across input
        // boundaries: libavformat's interleaved buffer reorders packets
        // across calls by dts, so the buffered tail of input N gets
        // flushed AFTER we begin adjusting offsets for input N+1, which
        // surfaces as non-monotonic dts. Each input is already correctly
        // interleaved by the demuxer, so handing packets to the muxer in
        // arrival order produces a valid output.
        output.write_frame(&mut packet)?;
        *packets_written += 1;
    }
    Ok(())
}

fn advance_offsets(
    input: &AVFormatContextInput,
    out_time_bases: &[ffi::AVRational],
    pts_offset: &mut [i64],
    next_min_dts: &[i64],
    total_duration_s: &mut f64,
) {
    let duration_ticks = input.duration; // in AV_TIME_BASE units.
    if duration_ticks > 0 {
        // Container duration is known: use it as the increment for every
        // stream. This sidesteps per-packet bookkeeping (which is fragile
        // across timestamp gaps, dts-leading-pts B-frame streams, and
        // packets with `AV_NOPTS_VALUE`).
        let dur_s = duration_ticks as f64 / f64::from(ffi::AV_TIME_BASE);
        *total_duration_s += dur_s;
        for (idx, tb) in out_time_bases.iter().enumerate() {
            let increment = (dur_s * f64::from(tb.den) / f64::from(tb.num)).round() as i64;
            pts_offset[idx] += increment;
        }
        return;
    }

    // Unknown container duration (mkv/webm from a non-seekable sink:
    // MediaRecorder, an interrupted capture). `process_input` ratchets
    // `next_min_dts[idx]` to `dts + packet duration` of the last written
    // packet, i.e. the absolute output-time end this input reached.
    // Advance the offset to that end so the next input starts cleanly
    // after it. Without this the offset stayed put and every packet of
    // the following input tripped the monotonic-dts ratchet, flattening
    // its real (VFR) inter-frame gaps to the filled packet duration.
    //
    // Use one segment end - the latest end across all streams - and
    // advance *every* stream to it, mirroring the known-duration path
    // (which adds the same increment to each stream). Advancing each
    // stream to its own end instead would offset streams by different
    // wall-clock amounts when they end at slightly different times (audio
    // is often a little shorter than video because of encoder padding),
    // so the next input's audio would start before/after its video and
    // the segment boundary would lose A/V sync.
    let sentinel = i64::MIN / 2;
    let mut segment_end_s = *total_duration_s;
    for (idx, tb) in out_time_bases.iter().enumerate() {
        if next_min_dts[idx] > sentinel {
            let end_s = next_min_dts[idx] as f64 * f64::from(tb.num) / f64::from(tb.den);
            if end_s > segment_end_s {
                segment_end_s = end_s;
            }
        }
    }
    for (idx, tb) in out_time_bases.iter().enumerate() {
        let end_ticks = (segment_end_s * f64::from(tb.den) / f64::from(tb.num)).round() as i64;
        // Never move an offset backwards.
        if end_ticks > pts_offset[idx] {
            pts_offset[idx] = end_ticks;
        }
    }
    *total_duration_s = segment_end_s;
}

fn assert_layout_matches(
    first: &AVFormatContextInput,
    input: &AVFormatContextInput,
    path: &str,
) -> Result<(), NativeError> {
    if input.streams().len() != first.streams().len() {
        return Err(NativeError::new(
            "invalid_request",
            "input stream count does not match the first input",
        )
        .with_detail("path", path.to_owned())
        .with_detail("expected", first.streams().len().to_string())
        .with_detail("got", input.streams().len().to_string()));
    }
    for (idx, (a, b)) in first.streams().iter().zip(input.streams()).enumerate() {
        if let Some((field, expected, got)) = codecpar_mismatch(&a.codecpar(), &b.codecpar()) {
            return Err(NativeError::new(
                "invalid_request",
                format!("input stream {field} does not match the first input"),
            )
            .with_detail("path", path.to_owned())
            .with_detail("stream", idx.to_string())
            .with_detail("field", field)
            .with_detail("expected", expected)
            .with_detail("got", got));
        }
    }
    Ok(())
}

/// The first codec parameter in which `got` differs from `expected`, as
/// `(field, expected, got)`. Only the time base may differ between
/// inputs: packets are rescaled, but their payloads are copied as they
/// are.
fn codecpar_mismatch(
    expected: &AVCodecParameters,
    got: &AVCodecParameters,
) -> Option<(&'static str, String, String)> {
    let int = |field, e: i32, g: i32| (e != g).then(|| (field, e.to_string(), g.to_string()));
    let named = |field, e: i32, g: i32, name: fn(i32) -> Option<&'static CStr>| {
        let show =
            |v: i32| name(v).map_or_else(|| v.to_string(), |c| c.to_string_lossy().into_owned());
        (e != g).then(|| (field, show(e), show(g)))
    };
    let layout = |p: &AVCodecParameters| {
        p.ch_layout()
            .describe()
            .map_or_else(|_| String::new(), |c| c.to_string_lossy().into_owned())
    };

    int("codec_id", expected.codec_id as i32, got.codec_id as i32)
        .or_else(|| int("profile", expected.profile, got.profile))
        .or_else(|| match expected.codec_type {
            ffi::AVMEDIA_TYPE_AUDIO => int("sample_rate", expected.sample_rate, got.sample_rate)
                .or_else(|| {
                    named(
                        "sample_format",
                        expected.format,
                        got.format,
                        get_sample_fmt_name,
                    )
                })
                .or_else(|| {
                    (!ffi_helpers::channel_layouts_equal(&expected.ch_layout, &got.ch_layout))
                        .then(|| ("channel_layout", layout(expected), layout(got)))
                }),
            ffi::AVMEDIA_TYPE_VIDEO => int("width", expected.width, got.width)
                .or_else(|| int("height", expected.height, got.height))
                .or_else(|| {
                    named(
                        "pixel_format",
                        expected.format,
                        got.format,
                        get_pix_fmt_name,
                    )
                }),
            _ => None,
        })
}

fn to_cstring(path: &Path) -> Result<CString, NativeError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })
}
