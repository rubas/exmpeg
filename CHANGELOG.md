# Changelog

## Unreleased

### Changed

- `transcode/3` no longer silently downmixes a surround (>2-channel)
  source to stereo when `:channels` is omitted and the audio is
  re-encoded. It now returns `{:error, %Exmpeg.Error{reason:
  :invalid_request}}` and requires an explicit `:channels` (1 or 2),
  matching `extract_audio/3`. Mono/stereo sources are unaffected.
- Updated `rustler` to 0.38. Source builds (`EXMPEG_BUILD=1`) now require
  Rust 1.91 or newer (rustler 0.38 raised its MSRV). Precompiled-NIF
  consumers are unaffected.
- Widened the `rustler_precompiled` requirement to `~> 0.9` (already
  resolved to 0.9.0).
- Bumped dev/test dependencies (`credo`, `ex_doc`, `ex_dna`, `ex_slop`)
  and refreshed transitive Rust crate versions and GitHub Actions pins.

### Fixed

- `remux/3`'s closing progress message now reports the real final
  `current_pts_s` (the largest written packet pts) instead of `0.0` when
  no `:duration_s` was given, so a subscriber rendering
  `current_pts_s / total_duration_s` sees ~100% at completion rather than
  0%.
- A Rust panic mid-write no longer leaks the `.partial` file on disk. The
  output write now arms an RAII guard that removes the partial on any
  early exit, including a panic unwind (which `catch_unwind` only catches
  one frame up), and is disarmed only after the rename onto the
  destination succeeds.
- `extract_audio/3` to `.opus` / `.ogg` at a sample rate other than
  48 kHz no longer produces a file whose container duration is wrong. The
  Ogg muxer pins Opus streams to a `1/48000` time_base, so encoder
  packets are now rescaled into the muxer's chosen stream time_base
  before writing (previously a 16 kHz extraction reported ~3x short).
  WAV/MP3/M4A/FLAC are unaffected (the rescale is an identity there).
- `transcode/3` with a mix of copied and re-encoded streams no longer
  desyncs them when the source does not start at timestamp 0 (MPEG-TS
  captures, edit-list offsets). Re-encoded streams use zero-based
  counters; copied packets now have the container start time subtracted
  too, so both share a zero origin while keeping their true inter-stream
  offset.
- Disk writes now use a per-call unique partial path
  (`<stem>.partial.<nonce>.<ext>`) instead of a deterministic one. Two
  concurrent writes to the same destination (duplicate jobs, a retry
  racing a slow first attempt, two nodes on shared storage) no longer
  share a partial file, so one call can no longer unlink or rename
  another's in-progress output. The guarantee is now
  last-complete-rename-wins: every observable destination state is a
  complete file.
- Several inputs that passed option validation but then raised inside the
  NIF decode now return `{:error, %Exmpeg.Error{reason: :invalid_request}}`
  as the contract promises: an `:fps` component past the i32 range, a
  bitrate past the i64 range, a non-UTF-8 path/output binary, and an opts
  list containing a non-`{key, value}` element.
- `transcode/3` with a custom `:video_filter` that has no `fps` filter no
  longer corrupts output timing. Filtered frames are now stepped by one
  frame interval in the encoder time_base instead of by a bare `1`, so a
  chain like `crop=...` keeps the real duration instead of collapsing the
  stream to a few microseconds. The default filter chain is unaffected.
- `concat/3` no longer corrupts timing when an input's container
  duration is unknown (mkv/webm from a non-seekable sink such as
  MediaRecorder or an interrupted capture). The per-stream offset is now
  advanced from the tracked end of the written packets instead of being
  left in place, so the following input no longer overlaps and trips the
  monotonic-dts ratchet (which flattened its real inter-frame gaps).
  `duration_s` now reports the summed input durations instead of `0.0`.

### Security

- Output paths that look like a protocol URL (`http://`, `ftp://`,
  `rtmp://`, `tcp://`, ...) are now rejected with `:invalid_request`
  before any work, closing a write-side SSRF where libavformat would open
  a network write from a caller-controlled destination. Local paths
  (including Windows drive letters) are unaffected.

### Performance

- `extract_audio/3` skips the resampler and the sample FIFO when the
  decoded audio already matches the target format, rate, and channel
  layout and the encoder accepts arbitrary chunk sizes (the PCM case),
  sending each decoded frame straight to the encoder. Output is
  unchanged; the conversion path is unaffected.
## 0.3.0 - 2026-05-20

Initial public release. Native Elixir bindings for FFmpeg 8 via the
`rsmpeg` Rust crate, packaged as a Rustler NIF - in-process calls
instead of shelling out to the `ffmpeg` / `ffprobe` CLIs.

### Operations

- `Exmpeg.version/0` - linked FFmpeg sub-library versions and configure
  flags.
- `Exmpeg.probe/1` - container + per-stream metadata (`ffprobe`).
- `Exmpeg.remux/3` - stream copy between containers with optional
  `start_s` / `duration_s` window, `:drop_audio` / `:drop_video` /
  `:drop_subtitles`, and a `:tags` keyword/map for container metadata.
- `Exmpeg.extract_frame/3` - single-frame thumbnail at a timestamp,
  written as `.jpg` / `.png` / `.bmp` / `.webp`, with optional
  aspect-preserving resize.
- `Exmpeg.extract_audio/3` - decoded audio to `.wav`, `.mp3`,
  `.m4a` / `.aac`, `.opus` / `.ogg`, or `.flac`, with optional
  sample-rate / channel-count / bitrate selection. Sample rate snaps to
  the encoder's supported list when needed (e.g. libopus
  8/12/16/24/48 kHz).
- `Exmpeg.concat/3` - join multiple inputs sharing the same stream
  layout into a single output without re-encoding.
- `Exmpeg.transcode/3` - per-stream re-encode with codec / bitrate /
  scale / fps / sample-rate selection driven by an `AVFilterGraph`, plus
  a raw `:video_filter` filter-graph spec. Streams marked `"copy"` are
  stream-copied.

### Inputs and progress

- **Memory inputs** - every read-side operation accepts
  `{:memory, binary}` in place of a filesystem path, backed by a custom
  `AVIOContextCustom` with read + seek callbacks so demuxers that seek
  (`mp4` `moov`, `matroska` cues) work without a temp file.
- **Progress callbacks** - `remux/3`, `extract_audio/3`, `concat/3`, and
  `transcode/3` accept `progress: pid()` and send throttled
  `{:exmpeg_progress, %{...}}` messages plus a final tick after
  `write_trailer`.

### Packaging

- Precompiled NIFs for `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`; other
  targets build from source with `EXMPEG_BUILD=1`.
- The precompiled tarballs bundle an **LGPL-only** FFmpeg 8 (libmp3lame,
  libopus, libvpx; no `--enable-gpl`), so they redistribute cleanly
  under this package's MIT license. H.264 / H.265 software encoding
  (`libx264` / `libx265`, GPL) is not in the precompiled binaries and
  returns `:unsupported`; build from source against a GPL-enabled
  FFmpeg 8 to use them. H.264/H.265 decoding is unaffected.

### Safety

- The Rust crate is built on rsmpeg's safe wrappers with
  `#![deny(unsafe_code)]` at the root. All `unsafe` is confined to
  `ffi_helpers.rs`, each block carrying a `SAFETY:` comment.
- Every NIF entry point is wrapped in `run_with_panic_protection`, so a
  Rust panic surfaces as `{:error, %{type: "nif_panic"}}` instead of
  crashing the BEAM.
- Size options (`:width` / `:height` / `:sample_rate`) are bounded at
  the API boundary so an absurd value cannot trigger an out-of-memory
  allocation inside the NIF.
- Disk writes are atomic: operations write a `<stem>.partial.<ext>`
  sibling and rename onto the destination only after the muxer trailer
  is written; a mid-encode failure removes the partial.
