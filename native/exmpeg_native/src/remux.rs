//! Stream-copy remuxing: read packets from an input container and write
//! them to an output container without re-encoding. Replaces
//! `ffmpeg -i in.mkv -c copy out.mp4` and the common
//! `-ss START -t DURATION -c copy` cut-without-reencode pattern.

use std::ffi::CString;
use std::path::Path;

use rsmpeg::avcodec::AVCodecParameters;
use rsmpeg::avformat::AVFormatContextOutput;
use rsmpeg::ffi;
use rustler::types::LocalPid;
use rustler::{Env, NifMap};

use crate::errors::NativeError;
use crate::ffi_helpers;
use crate::progress::ProgressEmitter;

/// Caller-supplied options for `remux`.
#[derive(Default, NifMap)]
pub(crate) struct RemuxOpts {
    /// Optional start offset in seconds. When set, packets with a pts/dts
    /// earlier than `start_s` (in the source's stream time base) are
    /// dropped. Skips keyframe alignment: container readers expect the
    /// first packet of a video stream to be a keyframe, so callers that
    /// need precise cuts should pass a value that lands on or before one.
    pub(crate) start_s: Option<f64>,
    /// Optional duration in seconds. Packets whose pts is at least
    /// `start_s + duration_s` are skipped and the loop terminates.
    pub(crate) duration_s: Option<f64>,
    /// Skip every audio stream (equivalent to `ffmpeg -an`).
    pub(crate) drop_audio: Option<bool>,
    /// Skip every video stream (equivalent to `ffmpeg -vn`).
    pub(crate) drop_video: Option<bool>,
    /// Skip every subtitle stream (equivalent to `ffmpeg -sn`).
    pub(crate) drop_subtitles: Option<bool>,
    /// Container-level metadata to attach to the output (title, artist,
    /// encoder, comment, etc.).
    pub(crate) tags: Option<Vec<(String, String)>>,
    /// Optional pid that receives throttled `{:exmpeg_progress, %{...}}`
    /// messages during the copy loop.
    pub(crate) progress: Option<LocalPid>,
}

/// Summary returned to Elixir. `packets_written` is what the muxer
/// accepted; `packets_dropped` is the count filtered out by the
/// start/duration window.
#[derive(Debug, NifMap)]
pub(crate) struct RemuxStats {
    pub(crate) packets_written: u64,
    pub(crate) packets_dropped: u64,
    pub(crate) streams_copied: u32,
}

pub(crate) fn remux<Q: AsRef<Path>>(
    env: Env<'_>,
    source: crate::input::InputSource,
    output_path: Q,
    opts: &RemuxOpts,
) -> Result<RemuxStats, NativeError> {
    let output_path = output_path.as_ref();
    let out_url = to_cstring(output_path)?;

    let mut input = source.open()?;
    let mut output = AVFormatContextOutput::create(&out_url)?;

    let drop_audio = opts.drop_audio.unwrap_or(false);
    let drop_video = opts.drop_video.unwrap_or(false);
    let drop_subtitles = opts.drop_subtitles.unwrap_or(false);

    // Map input stream index -> Some(output stream index) when kept,
    // None when filtered out. Packets from a `None` slot are dropped at
    // read time.
    let mut stream_map: Vec<Option<i32>> = Vec::with_capacity(input.streams().len());

    for in_stream in input.streams() {
        let in_codecpar = in_stream.codecpar();
        if should_drop(
            in_codecpar.codec_type,
            drop_audio,
            drop_video,
            drop_subtitles,
        ) {
            stream_map.push(None);
            continue;
        }

        let mut new_codecpar = AVCodecParameters::new();
        new_codecpar.copy(&in_codecpar);
        // Let the muxer choose the right fourCC for the output container
        // (mandatory when crossing container boundaries with the same
        // codec).
        ffi_helpers::clear_codec_tag(&mut new_codecpar);

        let mut out_stream = output.new_stream();
        out_stream.set_codecpar(new_codecpar);
        out_stream.set_time_base(in_stream.time_base);

        stream_map.push(Some(out_stream.index));
    }

    let streams_copied = stream_map.iter().filter(|s| s.is_some()).count() as u32;
    if streams_copied == 0 {
        return Err(NativeError::new(
            "invalid_request",
            "remux dropped every input stream; nothing left to write",
        ));
    }

    if let Some(tags) = opts.tags.as_ref() {
        ffi_helpers::set_format_metadata(&mut output, tags);
    }

    let mut header_opts = None;
    output.write_header(&mut header_opts)?;

    let start_s = opts.start_s.unwrap_or(0.0);
    let end_s = opts.duration_s.map(|d| start_s + d);

    let mut packets_written: u64 = 0;
    let mut packets_dropped: u64 = 0;
    // Largest pts (in seconds) of any packet actually written, so the
    // closing tick reports the real end position rather than 0.0.
    let mut last_written_pts_s = 0.0;
    let mut progress =
        ProgressEmitter::from_av_duration(env, opts.progress, "remux", input.duration);

    while let Some(mut packet) = input.read_packet()? {
        let in_index = packet.stream_index as usize;
        let Some(out_index) = stream_map.get(in_index).copied().flatten() else {
            // Stream was filtered out via drop_audio/video/subtitles.
            packets_dropped += 1;
            continue;
        };

        let in_stream = &input.streams()[in_index];
        let tb = in_stream.time_base;
        let pts_s = if packet.pts == ffi::AV_NOPTS_VALUE {
            None
        } else {
            Some(packet.pts as f64 * f64::from(tb.num) / f64::from(tb.den))
        };

        if let Some(pts) = pts_s {
            if pts < start_s {
                packets_dropped += 1;
                continue;
            }
            match end_s {
                Some(end) if pts >= end => {
                    packets_dropped += 1;
                    break;
                }
                _ => {}
            }
        }

        let out_tb = output.streams()[out_index as usize].time_base;
        packet.rescale_ts(tb, out_tb);
        packet.set_stream_index(out_index);

        output.interleaved_write_frame(&mut packet)?;
        packets_written += 1;
        if let Some(pts) = pts_s {
            if pts > last_written_pts_s {
                last_written_pts_s = pts;
            }
        }
        progress.tick(packets_written, pts_s.unwrap_or(0.0));
    }

    output.write_trailer()?;
    progress.finish(packets_written, last_written_pts_s);

    Ok(RemuxStats {
        packets_written,
        packets_dropped,
        streams_copied,
    })
}

fn to_cstring(path: &Path) -> Result<CString, NativeError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_err| {
        NativeError::new("invalid_request", "path contains NUL bytes")
            .with_detail("path", path.display().to_string())
    })
}

fn should_drop(codec_type: i32, drop_audio: bool, drop_video: bool, drop_subtitles: bool) -> bool {
    match codec_type {
        ffi::AVMEDIA_TYPE_AUDIO => drop_audio,
        ffi::AVMEDIA_TYPE_VIDEO => drop_video,
        ffi::AVMEDIA_TYPE_SUBTITLE => drop_subtitles,
        _ => false,
    }
}
