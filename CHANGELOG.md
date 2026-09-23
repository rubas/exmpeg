# Changelog

## 0.6.0 - 2026-09-23

### Changed

- Breaking for source builds (`EXMPEG_BUILD=1`): the NIF now needs FFmpeg
  9 shared libraries and no longer builds against FFmpeg 8. Install FFmpeg
  9 and its dev packages before you compile. Precompiled-NIF consumers
  are unaffected.
- Source builds need access to GitHub again. `rsmpeg` comes from our fork
  `rubas/rsmpeg` at a pinned revision, not from crates.io, until an rsmpeg
  release on crates.io supports FFmpeg 9.
- The precompiled NIFs bundle FFmpeg 9.0.1 (was 8.1.3). The bundled
  libraries have the new FFmpeg 9 major versions, for example
  `libavcodec.so.63` and `libavutil.so.61`. `Exmpeg.version/0` reports
  FFmpeg 9.0.1.

## 0.5.0 - 2026-09-23

### Changed

- `concat/3` rejects inputs whose codec parameters differ. It returns
  `{:error, %Exmpeg.Error{reason: :invalid_request}}` when a stream's
  profile, sample rate, sample format, channel layout, size, pixel format,
  or H.264/HEVC parameter sets (avcC or hvcC) differ from the first input.
  Before, such a join succeeded and played wrong, for example PCM at
  44100 Hz joined with 48000 Hz. A parameter that the probe could not read
  is not compared. The error details name the `"field"`, `"expected"`, and
  `"got"`, and the `"stream"` for a per-stream field. A stream count or
  codec mismatch, which 0.4.1 already rejected, now also has the
  `"field"` detail (`"stream_count"` or `"codec_id"`). A codec mismatch
  gives codec names such as `"h264"` in `"expected"` and `"got"`, not
  numeric codec ids. Concat checks every input before it writes the
  output. To join such inputs, transcode them to the same parameters
  first.
- `transcode/3` output timestamps change. Video keeps the timestamps the
  source and the filter graph produce, and re-encoded audio starts at the
  timestamp of its first decoded frame. Before, video frames were
  restamped at a fixed cadence and audio started at 0. The packet times now
  match the `ffmpeg` CLI for the same command. A source with a negative
  start time now gives output that starts at zero.
- Source builds (`EXMPEG_BUILD=1`) require Rust 1.98 or newer (was 1.91).
  Precompiled-NIF consumers are unaffected.
- The precompiled NIFs bundle FFmpeg 8.1.3 (was 8.1). It includes upstream
  bounds and overflow fixes in decoders and demuxers.
- `remux/3` with `:duration_s` ends each stream on its decode timestamp,
  like `ffmpeg -t -c copy`. A B-frame video stream keeps its in-window
  frames and the frames they reference, so the cut can run a few frames
  past the window.
- `rsmpeg` resolves from crates.io (`=0.18.0`), not from a git revision. A
  source build no longer fetches from GitHub.
- CI uses Elixir 1.20.4, OTP 29.1.1, and Rust 1.98.1. `mix.exs` keeps
  `elixir: "~> 1.17"` as the minimum version.
- Dev and test dependencies (`ex_doc`, `ex_dna`, `ex_slop`) and the Cargo
  lockfile take compatible releases. The runtime deps do not move:
  `rustler_precompiled` stays 0.9.0 and `rustler` stays 0.38.0.
- The `:video_filter` option of `transcode/3` is documented.
- The README states that the open of an input, and so `probe/1`, cannot be
  cancelled. The FFmpeg defaults limit only the stream analysis, not the
  container header read.

### Fixed

- The precompiled macOS (Apple Silicon) NIF loads again. The archive now
  ships `libavdevice`, which the NIF loads. No bundled library loads
  `libX11`, and every bundled library finds its siblings through
  `@loader_path`.
- Audio re-encoding in `transcode/3` and `extract_audio/3` could abort the
  VM with `free(): invalid pointer`. A channel layout was copied into
  uninitialised memory.
- `extract_audio/3` progress messages report the number of muxed packets
  in `packets_written`, not the sample count.
- `concat/3` moves each input from its own start time to the end of the
  previous input. The output starts at zero and has no gap at a join,
  also for MPEG-TS segments and MP4 files with an edit-list offset.
- `transcode/3` checks for a dead caller while the video filter graph
  drains, so a long `tpad` or `reverse` stops within about 100 ms.
- `transcode/3` re-encodes into Matroska and other global-header
  containers with out-of-band codec headers. libx264 into `.mkv` no longer
  fails with `:io_error`.
- `transcode/3` keeps a source frame rate that only the container carries
  (FFV1, VP8, VP9, MJPEG, ProRes) instead of converting to 25 fps.
- `transcode/3` and `extract_frame/3` keep the sample aspect ratio, also
  one that only the container carries. A resize with both `:width` and
  `:height` in another proportion keeps the display aspect ratio, as the
  `ffmpeg` `scale` filter does.
- `transcode/3` keeps the timestamps a custom `:video_filter` produces
  (`setpts`, `select`, `tpad`, variable frame rate). A frame whose
  timestamp does not advance is dropped, as in the `ffmpeg` CLI.
- `transcode/3` re-encodes a raw H.264 stream that carries no timestamps.
- `transcode/3` keeps the offset of a re-encoded audio or video stream
  that starts later than the other streams, and keeps a gap inside
  re-encoded audio in line with the video.

### Removed

- The Hex package no longer ships `usage-rules.md`. The `Exmpeg`
  moduledoc documents the public API.

## 0.4.1 - 2026-06-17

### Changed

- Build and test against FFmpeg 8.1 (was 8.0.1): the CI verification
  build, the distributed precompiled-NIF release artefacts, and the local
  `devenv` toolchain now all target FFmpeg 8.1. The pinned rsmpeg
  revision compiles unchanged against 8.1's libavformat 62.x, so there is
  no source or API change. Consumers building from source
  (`EXMPEG_BUILD=1`) should link against an FFmpeg 8.x install;
  precompiled-NIF consumers are unaffected.

## 0.4.0 - 2026-06-11

### Added

- `Exmpeg.load_buffer/1` returns an opaque, reusable `Exmpeg.Buffer` for
  in-memory input. Unlike `{:memory, binary}`, which copies the whole
  binary into the NIF on every call, a buffer copies the bytes once into
  a refcounted native resource; passing it to later operations (or as
  repeated `concat/3` inputs) is then a refcount bump, not another copy.
  A buffer is accepted anywhere an input source is.

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

### Security

- Output paths that look like a protocol URL (`http://`, `ftp://`,
  `rtmp://`, `tcp://`, ...) are now rejected with `:invalid_request`
  before any work, closing a write-side SSRF where libavformat would open
  a network write from a caller-controlled destination. Local paths
  (including Windows drive letters) are unaffected.
- Every input is now opened with FFmpeg's `protocol_whitelist` pinned.
  `{:memory, binary}` inputs are restricted to `crypto,data` (no
  filesystem, no network) and filesystem-path inputs to
  `file,crypto,data`. This closes an SSRF / local-file-disclosure vector
  where a crafted manifest demuxer (HLS, DASH, `concat`) could drive
  libavformat into opening attacker-controlled URLs from untrusted media.

### Performance

- `extract_audio/3` skips the resampler and the sample FIFO when the
  decoded audio already matches the target format, rate, and channel
  layout and the encoder accepts arbitrary chunk sizes (the PCM case),
  sending each decoded frame straight to the encoder. Output is
  unchanged; the conversion path is unaffected.

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
- `remux/3` and `concat/3` now return `{:error, %Exmpeg.Error{reason:
  :unsupported}}` for an unknown output extension or a codec the chosen
  container cannot hold (e.g. h264 into `.wav`), matching the documented
  contract, instead of the generic `:io_error` they returned before.
- `remux/3` with `:duration_s` no longer truncates other streams when one
  stream reaches the window edge first. Each kept stream now ends
  individually; the copy loop stops only once every kept stream has
  passed the window (or the input hits EOF), so interleaved audio is not
  cut short relative to video. Packets with no pts are bounded by their
  dts instead of bypassing the window check.
- Long-running operations (`remux/3`, `extract_frame/3`,
  `extract_audio/3`, `concat/3`, `transcode/3`) now stop when the calling
  process dies. They check caller liveness on a ~100 ms throttle and, on
  a dead caller, remove the partial output and return
  `{:error, %Exmpeg.Error{reason: :cancelled}}` instead of running the
  dirty-scheduler job to completion. Adds the new `:cancelled` error
  reason.

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
