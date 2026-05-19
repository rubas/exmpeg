# Changelog

## Unreleased

### Added

- **Memory inputs** — every read-side operation
  (`probe/1`, `extract_frame/3`, `extract_audio/3`, `remux/3`,
  `concat/3`, `transcode/3`) now accepts `{:memory, binary}` in place
  of a filesystem path. A custom `AVIOContextCustom` with read + seek
  callbacks lets demuxers that jump around the file (`mp4` looking for
  `moov`, `matroska` reading cues) work without a temp file write.
- **Progress callbacks** — `remux/3`, `extract_audio/3`, `concat/3`,
  and `transcode/3` accept `progress: pid()`. The NIF sends throttled
  (≤ 1 every 100 ms) `{:exmpeg_progress, %{op, packets_written,
  current_pts_s, total_duration_s}}` messages to the pid for the
  duration of the call, plus an unconditional final tick after
  `write_trailer`. The call must be wrapped in `Task.async/1` (or
  similar) so the calling process can receive while the NIF runs.

### Earlier in this branch

- `Exmpeg.extract_audio/3` now writes `.wav`, `.mp3`, `.m4a` / `.aac`,
  `.opus` / `.ogg`, and `.flac`. New `:bitrate` option is forwarded to
  the lossy codecs. Sample rate is snapped to the encoder's supported
  list when needed (e.g. libopus 8/12/16/24/48 kHz).
- `Exmpeg.remux/3` and `Exmpeg.transcode/3` accept
  `:drop_audio` / `:drop_video` / `:drop_subtitles` booleans (replacing
  `ffmpeg -an` / `-vn` / `-sn`) and a `:tags` keyword/map for
  container-level metadata (`title`, `artist`, `encoder`, `comment`,
  ...).
- `Exmpeg.transcode/3` accepts a `:video_filter` string forwarded to
  the FFmpeg filter-graph parser (e.g.
  `"crop=iw:ih-20:0:10,scale=720:-2,fps=30"`). The convenience options
  `:width` / `:height` / `:fps` remain as sugar when no filter spec is
  given; in either case the encoder is configured from the buffersink's
  post-filter dimensions / pix_fmt / time_base so user filters drive
  the final output shape.
- 13 new integration tests covering each of the above plus
  cross-container vp9+opus, audio-copy, missing-stream errors, mp3 /
  flac extraction, and concat layout-mismatch errors.

### Changed

- The video transcode pipeline switched from a one-shot `swscale` call
  to a full `AVFilterGraph`. The graph drives pix_fmt conversion,
  resize, and fps in one step and unlocks the `:video_filter` option.
- Internal: `ffi_helpers` now also exposes a safe `set_format_metadata`
  helper. All `unsafe` in the crate remains confined to this module.

### Deferred (not yet shipped)

- Subtitle burn-in / dedicated subtitle extraction.
- HLS / DASH segment muxers.
- Hardware device init (`-hwaccel`, `-hwaccel_device`). Hardware
  encoders selectable by codec name still work when the FFmpeg build
  initialises the device implicitly.

## 0.1.0

Initial release. Native Elixir bindings for FFmpeg 8 via the `rsmpeg` Rust
crate, packaged as a Rustler NIF.

### Operations

Full coverage of the common `ffmpeg` / `ffprobe` CLI replacements:

- `Exmpeg.version/0` - reports linked FFmpeg sub-library versions and
  configure flags.
- `Exmpeg.probe/1` - container + per-stream metadata (`ffprobe`).
- `Exmpeg.remux/3` - stream copy between containers with optional
  `start_s` / `duration_s` window (`ffmpeg -i in -c copy out`).
- `Exmpeg.extract_frame/3` - single-frame thumbnail at a timestamp,
  written as `.jpg` / `.png` / `.bmp` / `.webp` with optional resize
  (`ffmpeg -ss T -i in -frames:v 1 out.jpg`).
- `Exmpeg.extract_audio/3` - decoded audio stream as a 16-bit PCM WAV,
  with optional sample-rate / channel-count change
  (`ffmpeg -i in -vn -acodec pcm_s16le out.wav`).
- `Exmpeg.concat/3` - join multiple inputs sharing the same stream
  layout into a single output without re-encoding (`ffmpeg -f concat`).
- `Exmpeg.transcode/3` - per-stream re-encode with codec / bitrate /
  scale / fps / sample-rate selection. Streams marked `"copy"` are
  stream-copied; others go through decoder + swscale|swresample +
  encoder.

### Errors

`%Exmpeg.Error{}` returned from every call. Reasons:
`:invalid_request`, `:io_error`, `:decode_error`, `:encode_error`,
`:unsupported`, `:runtime_error`, `:nif_panic`, `:native_error`.

### Safety

The Rust crate is built on rsmpeg's safe wrappers with
`#![deny(unsafe_code)]` at the crate root. The three operations
rsmpeg does not yet expose safely (`AVCodecParameters.codec_tag`
write, `AVAudioFifo` read/write) are isolated to a single
`ffi_helpers.rs` module - about 15 lines of audited `unsafe` behind
safe wrappers. Every other module is `unsafe`-free.
