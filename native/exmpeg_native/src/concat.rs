//! Concatenation of multiple inputs into one output container without
//! re-encoding. Replaces `ffmpeg -f concat -i list.txt -c copy out`.
//!
//! Every input is opened in sequence, packets are stream-copied to the
//! output, and pts/dts are shifted by the cumulative duration of the
//! preceding inputs so the resulting timeline is monotonic.
//!
//! All inputs must share the same stream layout (same number of streams,
//! same codec id per stream index). Mismatches return `:invalid_request`.

use std::ffi::CString;
use std::path::Path;

use rsmpeg::avcodec::AVCodecParameters;
use rsmpeg::avformat::{AVFormatContextInput, AVFormatContextOutput};
use rsmpeg::ffi;
use rustler::types::LocalPid;
use rustler::{Env, NifMap};

use crate::errors::NativeError;
use crate::ffi_helpers;
use crate::progress::ProgressEmitter;

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
    sources: Vec<crate::input::InputSource>,
    output_path: P,
    opts: &ConcatOpts,
) -> Result<ConcatStats, NativeError> {
    if sources.is_empty() {
        return Err(NativeError::new(
            "invalid_request",
            "concat requires at least one input",
        ));
    }

    let total_inputs = sources.len();
    let output_path = output_path.as_ref();
    let out_url = to_cstring(output_path)?;
    let mut output = AVFormatContextOutput::create(&out_url)?;

    let mut sources = sources.into_iter();
    let first_source = sources.next().expect("non-empty checked above");
    let first_label = first_source.describe();
    // Open the first input to mint the output's stream layout.
    // Subsequent inputs reuse the layout; if they don't match we return
    // early.
    let mut first = first_source
        .open()
        .map_err(|e| e.with_detail("path", first_label.clone()))?;

    let mut codec_ids: Vec<ffi::AVCodecID> = Vec::new();
    for in_stream in first.streams() {
        let codecpar = in_stream.codecpar();
        let mut new_codecpar = AVCodecParameters::new();
        new_codecpar.copy(&codecpar);
        ffi_helpers::clear_codec_tag(&mut new_codecpar);

        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(new_codecpar);
        out_stream.set_time_base(in_stream.time_base);

        codec_ids.push(codecpar.codec_id);
    }
    let streams_copied = codec_ids.len() as u32;

    let mut header_opts = None;
    output.write_header(&mut header_opts)?;

    // Snapshot the muxer's chosen output time_base. Some muxers (notably
    // mp4) override what we requested at new_stream time; we rescale
    // every packet into the actual time_base before writing.
    let out_time_bases: Vec<ffi::AVRational> =
        output.streams().iter().map(|s| s.time_base).collect();

    // Cumulative offset per stream, in that stream's output time_base.
    let mut pts_offset: Vec<i64> = vec![0; codec_ids.len()];
    // Minimum dts the next packet of each stream must hit. Used to
    // patch over AAC encoder priming (negative pts) and other small
    // per-frame shifts that would otherwise produce a non-monotonic
    // dts at input boundaries.
    let mut next_min_dts: Vec<i64> = vec![i64::MIN / 2; codec_ids.len()];
    let mut packets_written: u64 = 0;
    let mut total_duration_s: f64 = 0.0;
    // For concat the input duration is unknown up front (we'd need to
    // sum every input's container duration before opening), so report
    // `0.0` and let the caller infer progress from packet count.
    let mut progress = ProgressEmitter::new(env, opts.progress, "concat", 0.0);

    process_input(
        &mut first,
        &mut output,
        &out_time_bases,
        &pts_offset,
        &mut next_min_dts,
        &mut packets_written,
    )?;
    advance_offsets(
        &first,
        &out_time_bases,
        &mut pts_offset,
        &mut total_duration_s,
    );
    progress.tick(packets_written, total_duration_s);

    for next in sources {
        let label = next.describe();
        let mut input = next
            .open()
            .map_err(|e| e.with_detail("path", label.clone()))?;
        assert_layout_matches(&input, &codec_ids, &label)?;
        process_input(
            &mut input,
            &mut output,
            &out_time_bases,
            &pts_offset,
            &mut next_min_dts,
            &mut packets_written,
        )?;
        advance_offsets(
            &input,
            &out_time_bases,
            &mut pts_offset,
            &mut total_duration_s,
        );
        progress.tick(packets_written, total_duration_s);
    }

    output.write_trailer()?;
    progress.finish(packets_written, total_duration_s);

    Ok(ConcatStats {
        packets_written,
        inputs_joined: total_inputs as u32,
        streams_copied,
        duration_s: total_duration_s,
    })
}

fn process_input(
    input: &mut AVFormatContextInput,
    output: &mut AVFormatContextOutput,
    out_time_bases: &[ffi::AVRational],
    pts_offset: &[i64],
    next_min_dts: &mut [i64],
    packets_written: &mut u64,
) -> Result<(), NativeError> {
    while let Some(mut packet) = input.read_packet()? {
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

        let offset = pts_offset[idx];
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
    total_duration_s: &mut f64,
) {
    // Use the container's total duration as the increment for every
    // stream. This sidesteps per-packet bookkeeping (which is fragile
    // across timestamp gaps, dts-leading-pts B-frame streams, and
    // packets with `AV_NOPTS_VALUE`).
    let duration_ticks = input.duration; // in AV_TIME_BASE units.
    if duration_ticks <= 0 {
        return;
    }
    let dur_s = duration_ticks as f64 / f64::from(ffi::AV_TIME_BASE);
    *total_duration_s += dur_s;

    for (idx, tb) in out_time_bases.iter().enumerate() {
        let increment = (dur_s * f64::from(tb.den) / f64::from(tb.num)).round() as i64;
        pts_offset[idx] += increment;
    }
}

fn assert_layout_matches(
    input: &AVFormatContextInput,
    template: &[ffi::AVCodecID],
    path: &str,
) -> Result<(), NativeError> {
    if input.streams().len() != template.len() {
        return Err(NativeError::new(
            "invalid_request",
            "input stream count does not match the first input",
        )
        .with_detail("path", path.to_owned())
        .with_detail("expected", template.len().to_string())
        .with_detail("got", input.streams().len().to_string()));
    }
    for (idx, stream) in input.streams().iter().enumerate() {
        let expected_codec = template[idx];
        let got = stream.codecpar().codec_id;
        if got != expected_codec {
            return Err(NativeError::new(
                "invalid_request",
                "input stream codec id does not match the first input",
            )
            .with_detail("path", path.to_owned())
            .with_detail("stream", idx.to_string())
            .with_detail("expected", format!("{expected_codec:?}"))
            .with_detail("got", format!("{got:?}")));
        }
    }
    Ok(())
}

fn to_cstring(path: &Path) -> Result<CString, NativeError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })
}
