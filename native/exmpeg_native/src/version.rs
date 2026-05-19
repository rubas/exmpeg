//! FFmpeg build info: version, configure flags, license. Returned as a
//! `NifMap` so the Elixir side can decide what to surface.
//!
//! Pure safe rsmpeg wrappers — no FFI / `unsafe` in this module.

use rsmpeg::{avcodec, avformat, avutil};
use rustler::NifMap;

#[derive(Debug, NifMap)]
pub(crate) struct VersionInfo {
    /// `libavformat` version as `major.minor.micro`.
    pub(crate) avformat: String,
    /// `libavcodec` version as `major.minor.micro`.
    pub(crate) avcodec: String,
    /// `libavutil` version as `major.minor.micro`.
    pub(crate) avutil: String,
    /// The license string baked into libavformat.
    pub(crate) license: String,
    /// The `./configure` flags the linked FFmpeg was built with.
    pub(crate) configuration: String,
}

pub(crate) fn version_info() -> VersionInfo {
    VersionInfo {
        avformat: avformat::version().to_string(),
        avcodec: avcodec::version().to_string(),
        avutil: avutil::version().to_string(),
        license: avformat::license().to_string_lossy().into_owned(),
        configuration: avformat::configuration().to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_info_strings_are_well_formed() {
        let v = version_info();
        // Every linked FFmpeg version is non-empty and dot-separated.
        for s in [&v.avformat, &v.avcodec, &v.avutil] {
            assert!(s.contains('.'), "expected dot-separated version, got: {s}");
        }
    }
}
