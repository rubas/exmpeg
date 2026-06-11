//! Shared helpers for the audio re-encode paths (`extract_audio`,
//! `transcode`).

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
}
