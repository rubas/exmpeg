//! Shared helpers for the audio re-encode paths (`extract_audio`,
//! `transcode`).

use rsmpeg::avcodec::AVCodecRef;
use rsmpeg::avutil::{AVChannelLayout, AVFrame};

use crate::errors::NativeError;

/// Resolve the target channel count for an audio re-encode.
///
/// When the caller hasn't asked for a specific layout we only carry
/// mono / stereo sources through unchanged. A source with more channels
/// (5.1, 7.1, ...) would otherwise be silently downmixed, which hides
/// the layout change from the caller and violates the project's
/// no-hidden-fallbacks rule. Force the caller to opt in to mono or
/// stereo explicitly via `:channels`.
pub(crate) fn resolve_channels(requested: Option<i32>, src: i32) -> Result<i32, NativeError> {
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

pub(crate) fn pick_sample_fmt(codec: &AVCodecRef<'static>, src: i32) -> i32 {
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

pub(crate) fn alloc_resample_frame(
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

pub(crate) fn empty_resample_frame(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_mono_or_stereo_source_when_unspecified() {
        assert_eq!(resolve_channels(None, 1).unwrap(), 1);
        assert_eq!(resolve_channels(None, 2).unwrap(), 2);
    }

    #[test]
    fn rejects_surround_source_without_explicit_channels() {
        assert_eq!(
            resolve_channels(None, 6).unwrap_err().r#type,
            "invalid_request"
        );
    }

    #[test]
    fn honours_explicit_request_and_rejects_out_of_range() {
        assert_eq!(resolve_channels(Some(1), 6).unwrap(), 1);
        assert_eq!(resolve_channels(Some(2), 6).unwrap(), 2);
        assert_eq!(
            resolve_channels(Some(3), 6).unwrap_err().r#type,
            "invalid_request"
        );
    }

    #[test]
    fn resample_capacity_scales_by_rate_and_clamps_corrupt_input() {
        assert_eq!(compute_resample_capacity(1024, 48_000, 16_000), 597);
        assert_eq!(compute_resample_capacity(0, 44_100, 16_000), 4096);
        assert_eq!(compute_resample_capacity(1024, 1, 192_000), 1 << 20);
    }
}
