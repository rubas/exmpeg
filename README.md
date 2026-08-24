# exmpeg

Native Elixir bindings for FFmpeg via the [`rsmpeg`](https://crates.io/crates/rsmpeg) Rust crate.

This library replaces shelling out to the `ffmpeg` / `ffprobe` CLIs with an
in-process [Rustler](https://github.com/rusterlium/rustler) NIF. Every call
runs against the FFmpeg shared libraries the NIF was linked at compile time
and returns structured results as plain Elixir structs / maps.

## What it covers

| Operation                | Replaces                                                     |
| ------------------------ | ------------------------------------------------------------ |
| `Exmpeg.probe/1`         | `ffprobe -show_format -show_streams`                          |
| `Exmpeg.remux/3`         | `ffmpeg -i in -c copy out` (with optional `-ss` / `-t` cut)   |
| `Exmpeg.extract_frame/3` | `ffmpeg -ss T -i in -frames:v 1 out.jpg`                      |
| `Exmpeg.extract_audio/3` | `ffmpeg -i in -vn -acodec pcm_s16le out.wav`                  |
| `Exmpeg.concat/3`        | `ffmpeg -f concat -i list.txt -c copy out`                    |
| `Exmpeg.transcode/3`     | `ffmpeg -i in -c:v libvpx-vp9 -c:a libopus out` (and friends) |

`Exmpeg.load_buffer/1` turns a binary into a reusable in-memory input, and
`Exmpeg.version/0` reports the FFmpeg version the NIF is linked against.

## Quickstart

```elixir
# Probe (ffprobe)
{:ok, info} = Exmpeg.probe("input.mkv")
info.format.duration_s
#=> 12.345

# Remux: container change, optional cut window
{:ok, _} = Exmpeg.remux("input.mkv", "output.mp4")
{:ok, _} = Exmpeg.remux("input.mp4", "clip.mp4", start_s: 5.0, duration_s: 2.0)

# Thumbnail at a timestamp, optionally resized
{:ok, _} = Exmpeg.extract_frame("input.mp4", "thumb.jpg", timestamp_s: 1.5, width: 320)

# Audio to WAV with explicit sample rate + channels
{:ok, _} = Exmpeg.extract_audio("input.mp4", "audio.wav", sample_rate: 16_000, channels: 1)

# Concat three same-codec clips
{:ok, _} = Exmpeg.concat(["a.mp4", "b.mp4", "c.mp4"], "joined.mp4")

# Re-encode to VP9 + Opus at a smaller width / lower audio rate
{:ok, _} =
  Exmpeg.transcode("input.mov", "output.webm",
    video_codec: "libvpx-vp9", audio_codec: "libopus",
    width: 1280, sample_rate: 48_000
  )

# Read the same bytes more than once without copying them again
{:ok, buffer} = Exmpeg.load_buffer(File.read!("input.mp4"))
{:ok, info} = Exmpeg.probe(buffer)
{:ok, _} = Exmpeg.extract_frame(buffer, "thumb.jpg", timestamp_s: 1.5)
```

## Safety

The Rust crate is built on rsmpeg's safe wrappers with
`#![deny(unsafe_code)]` at the root.
`native/exmpeg_native/src/ffi_helpers.rs` is the only module that
contains `unsafe`; everything else, including the progress emitter that
reconstructs an `Env<'_>` through those helpers, stays outside it. The
quarantined operations are the ones rsmpeg does not yet expose safely:

- clearing `AVCodecParameters.codec_tag` (a single primitive store on a
  unique `&mut` borrow),
- `AVAudioFifo::write` / `AVAudioFifo::read` against a frame's
  `extended_data` per-channel pointer array,
- assigning a freshly-built `AVDictionary` into
  `AVFormatContextOutput.metadata` (libavformat takes ownership),
- comparing two raw `AVChannelLayout`s through
  `av_channel_layout_compare`, which reads both and keeps no pointer, to
  decide whether audio extraction can skip resampling,
- rebuilding an `Env<'_>` from the raw `NIF_ENV` captured at the entry
  point, so a long-running operation can emit throttled
  `{:exmpeg_progress, ...}` messages without an `OwnedEnv` (which panics
  on dirty-scheduler threads).

Every `unsafe` block names its invariant in a `SAFETY:` comment, and unit
tests in the same module exercise the round-trips.

Every NIF entry point is wrapped in `run_with_panic_protection`, so a
Rust panic surfaces as `{:error, %{type: "nif_panic", ...}}` instead
of taking down the BEAM VM.

### Untrusted input

Every input is opened with FFmpeg's `protocol_whitelist` pinned, so a
crafted file cannot drive libavformat into opening attacker-controlled
URLs (the SSRF / local-file-disclosure vector that HLS, DASH, and the
`concat` protocol expose through nested segment opens). The guarantee
differs by input kind:

- **In-memory input is the path for untrusted media.** Both
  `{:memory, binary}` and a buffer from `Exmpeg.load_buffer/1` are
  restricted to `crypto,data`: no filesystem and no network reach, so a
  crafted upload can reach neither the network nor any local file. Buffer
  untrusted uploads (and anything you did not author) through this path.
- **Filesystem-path inputs trust the local filesystem.** They allow
  `file,crypto,data`: the network is blocked, but `file` is required. It
  is the protocol that opens the path itself, and it also lets a local
  HLS/DASH playlist read its sibling segment files. `protocol_whitelist`
  applies uniformly to every open libavformat performs, so there is no
  way to keep the top-level file open while forbidding the nested ones,
  and a crafted *on-disk* manifest can therefore still point FFmpeg at
  other local files via a `file:` reference. So do not write an untrusted
  upload to a temp file and probe it by path; hand the bytes to
  `{:memory, _}` or a buffer instead.

Single-file demuxers (mp4, mkv, ...) perform no nested opens, so the
whitelist is invisible to them; it only constrains the reference
demuxers, which is exactly where the risk lives.

## Installation

```elixir
def deps do
  [
    {:exmpeg, "~> 0.4"}
  ]
end
```

The published Hex package ships precompiled NIFs for common targets
(`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`); consumers do not need a Rust toolchain to
use them.

To build the NIF from source, install Rust 1.91 or newer and set
`EXMPEG_BUILD=1` before compiling.

## Build requirements

- FFmpeg 8.x shared libraries on the linker / loader path. `rsmpeg`
  discovers them via `pkg-config`; set `FFMPEG_PKG_CONFIG_PATH` when
  building against a non-default install.
- Rust 1.91+ for source builds.
- Elixir 1.17+ / OTP 26+ (the NIF targets Erlang NIF version 2.17).

## Runtime requirements (precompiled NIF consumers)

The published Hex package ships precompiled NIF tarballs that **bundle
the six FFmpeg shared libraries** (`libavformat`, `libavcodec`,
`libavutil`, `libavfilter`, `libswscale`, `libswresample`) next to the
NIF and use `$ORIGIN` / `@loader_path` so the loader finds them without
`LD_LIBRARY_PATH` gymnastics. Consumers therefore do **not** need to
install FFmpeg 8 separately.

The bundled FFmpeg is built **LGPL-only** (`--enable-libmp3lame
--enable-libopus --enable-libvpx --enable-libwebp`, no `--enable-gpl`), so
the precompiled binaries can be redistributed under this package's MIT
license. H.264 / H.265 software encoding via `libx264` / `libx265` is GPL
and is **not** in the precompiled binaries; calling `transcode/3` with
`video_codec: "libx264"` (or `"libx265"`) on a precompiled install
returns `{:error, %Error{reason: :unsupported}}`. To use them, build
from source (`EXMPEG_BUILD=1`) against your own GPL-enabled FFmpeg 8.

What is **not** bundled and must be on the host:

- glibc, with `libm`, `libdl`, and `libpthread`. The floor differs per
  architecture, because the two builds reference different versioned math
  symbols: **x86_64 needs glibc 2.35 or newer**, which Ubuntu 22.04 and
  Debian 12 clear; **aarch64 needs glibc 2.38 or newer**, which Ubuntu
  24.04 and Debian 13 clear. An older host has to build from source.
  Check a host with `ldd --version`.
- The codec system libraries that libavcodec dlopens at decode/encode
  time:
  - `libmp3lame` (`libmp3lame0`)
  - `libopus` (`libopus0`)
  - `libvpx` (`libvpx9` or newer)
  - `libwebp` (`libwebp7` or newer), for `.webp` frame output
- Their transitive system deps (`libgsm`, `libnuma`, ...) which the
  distro packages above pull in automatically.

For Debian / Ubuntu:

```bash
sudo apt install -y libmp3lame0 libopus0 libvpx9 libwebp7
```

For macOS (Apple Silicon, via Homebrew):

```bash
brew install lame opus libvpx webp
```

Source builds (`EXMPEG_BUILD=1`) link directly against the system's
FFmpeg 8 install and so behave like a normal `pkg-config` consumer:
they need the dev packages (`libavcodec-dev` & friends) at build time
and the matching runtime libs at load time.

## Errors

Every call returns either `{:ok, value}` or `{:error, %Exmpeg.Error{}}`.
`t:Exmpeg.Error.reason/0` enumerates the categories: `:invalid_request`,
`:io_error`, `:decode_error`, `:encode_error`, `:unsupported`,
`:runtime_error`, `:cancelled`, `:nif_panic`, `:native_error`.

A long-running operation (`remux/3`, `extract_frame/3`, `extract_audio/3`,
`concat/3`, `transcode/3`) checks whether the calling process is still
alive roughly every 100 ms. If the caller dies mid-operation (a `Task`
timeout, a supervised shutdown, a disconnect) the native work stops at
the next check, the partial output is removed, and the call resolves to
`{:error, %Exmpeg.Error{reason: :cancelled}}` (which the dead caller
never observes). The operation is uninterruptible between checks;
`probe/1` is not cancellable.

## Development

```bash
task setup             # mix deps.get
task compile           # build the NIF (first run takes several minutes)
task test              # fast Elixir unit tests
task test:rust         # cargo test
task lint              # mix credo --strict + cargo clippy -D warnings
task check             # full local gate
task test:integration  # end-to-end tests against a generated clip
```

`task test:integration` synthesises a small MP4 with the `ffmpeg` CLI and
checks packet timing with `ffprobe`, so both must be on `PATH`.

## License

MIT. See [LICENSE](LICENSE).
