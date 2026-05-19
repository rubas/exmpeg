# exmpeg

Native Elixir bindings for FFmpeg via the [`rsmpeg`](https://crates.io/crates/rsmpeg) Rust crate.

This library replaces shelling out to the `ffmpeg` / `ffprobe` CLIs with an
in-process [Rustler](https://github.com/rusterlium/rustler) NIF. Every call
runs against the FFmpeg shared libraries the NIF was linked at compile time
and returns structured results as plain Elixir structs / maps.

## Status

`v0.1` covers the operations needed to fully replace the `ffmpeg` /
`ffprobe` CLI for the common cases:

| Operation                  | Replaces                                                          |
| -------------------------- | ----------------------------------------------------------------- |
| `Exmpeg.probe/1`           | `ffprobe -show_format -show_streams`                              |
| `Exmpeg.remux/3`           | `ffmpeg -i in -c copy out` (with optional `-ss` / `-t` cut)       |
| `Exmpeg.extract_frame/3`   | `ffmpeg -ss T -i in -frames:v 1 out.jpg`                          |
| `Exmpeg.extract_audio/3`   | `ffmpeg -i in -vn -acodec pcm_s16le out.wav`                      |
| `Exmpeg.concat/2`          | `ffmpeg -f concat -i list.txt -c copy out`                        |
| `Exmpeg.transcode/3`       | `ffmpeg -i in -c:v libx264 -c:a aac out` (and friends)            |

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

# Re-encode to libx264 + aac at a smaller width / lower audio rate
{:ok, _} =
  Exmpeg.transcode("input.mov", "output.mp4",
    video_codec: "libx264", audio_codec: "aac",
    width: 1280, sample_rate: 48_000
  )
```

## Safety

The Rust crate is built on rsmpeg's safe wrappers with
`#![deny(unsafe_code)]` at the root. The three things rsmpeg does not
yet expose safely (`AVCodecParameters.codec_tag` write, `AVAudioFifo`
read / write) live in a single `ffi_helpers.rs` module - about 15 lines
of `unsafe` behind safe wrappers. Every other module is `unsafe`-free.

## Installation

```elixir
def deps do
  [
    {:exmpeg, "~> 0.1"}
  ]
end
```

The published Hex package ships precompiled NIFs for common targets
(`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`); consumers do not need a Rust toolchain to
use them.

To build the NIF from source, install Rust 1.85 or newer and set
`EXMPEG_BUILD=1` before compiling.

## Build requirements

- FFmpeg 8.x shared libraries on the linker / loader path. `rsmpeg`
  discovers them via `pkg-config`; set `FFMPEG_PKG_CONFIG_PATH` when
  building against a non-default install.
- Rust 1.85+ for source builds.
- Elixir 1.17+ / OTP 26+.

## Runtime requirements (precompiled NIF consumers)

The published Hex package ships precompiled NIF tarballs that **bundle
the six FFmpeg shared libraries** (`libavformat`, `libavcodec`,
`libavutil`, `libavfilter`, `libswscale`, `libswresample`) next to the
NIF and use `$ORIGIN` / `@loader_path` so the loader finds them without
`LD_LIBRARY_PATH` gymnastics. Consumers therefore do **not** need to
install FFmpeg 8 separately.

What is **not** bundled and must be on the host:

- `libc` 2.35+ (Ubuntu 24.04 / Debian 12) with `libm`, `libdl`,
  `libpthread`. Standard on any modern Linux.
- The codec system libraries that libavcodec dlopens at decode/encode
  time:
  - `libx264` (Debian / Ubuntu: `libx264-164` or newer)
  - `libmp3lame` (`libmp3lame0`)
  - `libopus` (`libopus0`)
  - `libvpx` (`libvpx9` or newer)
- Their transitive system deps (`libgsm`, `libnuma`, ...) which the
  distro packages above pull in automatically.

For Debian / Ubuntu:

```bash
sudo apt install -y libx264-164 libmp3lame0 libopus0 libvpx9
```

For macOS (Apple Silicon, via Homebrew):

```bash
brew install x264 lame opus libvpx
```

Source builds (`EXMPEG_BUILD=1`) link directly against the system's
FFmpeg 8 install and so behave like a normal `pkg-config` consumer:
they need the dev packages (`libavcodec-dev` & friends) at build time
and the matching runtime libs at load time.

## Errors

Every call returns either `{:ok, value}` or `{:error, %Exmpeg.Error{}}`.
`Exmpeg.Error.reason/0` enumerates the categories: `:invalid_request`,
`:io_error`, `:decode_error`, `:encode_error`, `:unsupported`,
`:runtime_error`, `:nif_panic`, `:native_error`.

## Development

```bash
task setup        # mix deps.get
task compile      # build the NIF (first run takes several minutes)
task test         # fast Elixir unit tests
task test:rust    # cargo test
task lint         # mix credo --strict + cargo clippy -D warnings
task check        # full local gate
```

Run integration tests (synthesises a small MP4 via the `ffmpeg` CLI) with:

```bash
task test:integration
```

## License

MIT. See [LICENSE](LICENSE).
