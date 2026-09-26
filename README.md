# exmpeg

Elixir bindings for FFmpeg. A [Rustler](https://github.com/rusterlium/rustler)
NIF on the [`rsmpeg`](https://crates.io/crates/rsmpeg) crate runs FFmpeg in
the BEAM process, so you do not shell out to `ffmpeg` or `ffprobe`. Every
call returns plain Elixir structs and maps.

## Installation

```elixir
def deps do
  [
    {:exmpeg, "~> 0.6"}
  ]
end
```

The Hex package ships precompiled NIFs for `aarch64-apple-darwin`,
`x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`. You need no
Rust toolchain and no FFmpeg install to use them. See
[Runtime requirements](#runtime-requirements) for the system libraries the
host must have.

## What it covers

| Function                 | Replaces                                                    |
| ------------------------ | ----------------------------------------------------------- |
| `Exmpeg.probe/1`         | `ffprobe -show_format -show_streams`                        |
| `Exmpeg.remux/3`         | `ffmpeg -i in -c copy out`, with an optional `-ss` / `-t` cut |
| `Exmpeg.extract_frame/3` | `ffmpeg -ss T -i in -frames:v 1 out.jpg`                    |
| `Exmpeg.extract_audio/3` | `ffmpeg -i in -vn -acodec pcm_s16le out.wav`                |
| `Exmpeg.concat/3`        | `ffmpeg -f concat -i list.txt -c copy out`                  |
| `Exmpeg.transcode/3`     | `ffmpeg -i in -c:v libvpx-vp9 -c:a libopus out`, and others |

`Exmpeg.load_buffer/1` turns a binary into an in-memory input that you can
use many times. `Exmpeg.version/0` returns the FFmpeg version the NIF links.

## Quickstart

```elixir
# Probe (ffprobe)
{:ok, info} = Exmpeg.probe("input.mkv")
info.format.duration_s
#=> 12.345

# Remux: change the container, with an optional cut
{:ok, _} = Exmpeg.remux("input.mkv", "output.mp4")
{:ok, _} = Exmpeg.remux("input.mp4", "clip.mp4", start_s: 5.0, duration_s: 2.0)

# Thumbnail at a timestamp, optionally resized
{:ok, _} = Exmpeg.extract_frame("input.mp4", "thumb.jpg", timestamp_s: 1.5, width: 320)

# Audio to WAV with a sample rate and channel count
{:ok, _} = Exmpeg.extract_audio("input.mp4", "audio.wav", sample_rate: 16_000, channels: 1)

# Concat three clips with the same codecs
{:ok, _} = Exmpeg.concat(["a.mp4", "b.mp4", "c.mp4"], "joined.mp4")

# Re-encode to VP9 and Opus at a smaller width
{:ok, _} =
  Exmpeg.transcode("input.mov", "output.webm",
    video_codec: "libvpx-vp9", audio_codec: "libopus",
    width: 1280, sample_rate: 48_000
  )

# Read the same bytes more than once without a new copy
{:ok, buffer} = Exmpeg.load_buffer(File.read!("input.mp4"))
{:ok, info} = Exmpeg.probe(buffer)
{:ok, _} = Exmpeg.extract_frame(buffer, "thumb.jpg", timestamp_s: 1.5)
```

## Runtime requirements

Each precompiled tarball bundles the seven FFmpeg 9.0.1 shared libraries
(`libavformat`, `libavcodec`, `libavutil`, `libavfilter`, `libswscale`,
`libswresample`, `libavdevice`) next to the NIF. The NIF finds them through
`$ORIGIN` or `@loader_path`, so you need no `LD_LIBRARY_PATH`.

The bundled FFmpeg is LGPL only (`--enable-libmp3lame --enable-libopus
--enable-libvpx --enable-libwebp`, no `--enable-gpl`), so the MIT package can
redistribute it. It has no `libx264` or `libx265`. `transcode/3` with
`video_codec: "libx264"` or `"libx265"` returns
`{:error, %Exmpeg.Error{reason: :unsupported}}`. To use them, build from
source against your own GPL FFmpeg 9.

The host must supply the rest:

- glibc with `libm`, `libdl`, and `libpthread`. x86_64 needs glibc 2.35 or
  newer (Ubuntu 22.04, Debian 12). aarch64 needs glibc 2.38 or newer
  (Ubuntu 24.04, Debian 13). An older host must build from source. Check
  with `ldd --version`.
- The codec libraries that libavcodec loads: `libmp3lame`, `libopus`,
  `libvpx` (9 or newer), and `libwebp` (7 or newer, for `.webp` frames).
  The distro packages pull in their own dependencies.

```bash
# Debian / Ubuntu
sudo apt install -y libmp3lame0 libopus0 libvpx9 libwebp7

# macOS (Homebrew)
brew install lame opus libvpx webp
```

## Build from source

Set `EXMPEG_BUILD=1` before you compile. A source build links the FFmpeg on
the host and needs:

- FFmpeg 9.x with its dev headers (`libavcodec-dev` and the others).
  `rsmpeg` finds them through `pkg-config`. Set `FFMPEG_PKG_CONFIG_PATH`
  for an install outside the default path.
- Rust 1.98 or newer, and libclang for `bindgen`. Set `LIBCLANG_PATH` when
  libclang is outside the default library path.
- Access to GitHub. The NIF uses
  [our `rsmpeg` fork](https://github.com/rubas/rsmpeg) until an `rsmpeg`
  release on crates.io supports FFmpeg 9.
- Elixir 1.17 or newer and OTP 26 or newer (NIF version 2.17).

## Errors and cancellation

Every call returns `{:ok, value}` or `{:error, %Exmpeg.Error{}}`.
`t:Exmpeg.Error.reason/0` lists the reasons: `:invalid_request`,
`:io_error`, `:decode_error`, `:encode_error`, `:unsupported`,
`:runtime_error`, `:cancelled`, `:nif_panic`, `:native_error`.

`remux/3`, `extract_frame/3`, `extract_audio/3`, `concat/3`, and
`transcode/3` check about every 100 ms that the calling process is alive.
When the caller dies (a `Task` timeout, a supervisor shutdown, a
disconnect), the work stops at the next check, the NIF removes the partial
output, and the call returns `{:error, %Exmpeg.Error{reason: :cancelled}}`.

The checks run in the packet loops, so you cannot cancel the open of an
input. The open reads the container header (`avformat_open_input`), then
analyzes the streams (`avformat_find_stream_info`). FFmpeg limits only the
analysis: about 5 MB of packets (`probesize`) and 5 to 90 s of media,
by format (`analyzeduration`). Nothing limits the header read. An mp4
`moov` index grows with the sample count, so a long mp4 can read tens of MB
before the analysis starts. `probe/1` is only this open, so you cannot
cancel it. A read that blocks in the kernel, for example on a stalled
network mount, holds the dirty scheduler thread until it returns.

## Untrusted input

Every input opens with FFmpeg's `protocol_whitelist` set. A crafted file
cannot make libavformat open a URL of the attacker's choice, as HLS, DASH,
and the `concat` protocol can through nested opens. The limit depends on
the input kind:

- `{:memory, binary}` and a buffer from `Exmpeg.load_buffer/1` allow only
  `crypto,data`. They reach no file and no network. Use them for uploads
  and for any media you did not create.
- A file path allows `file,crypto,data`. The network is blocked, but
  `file` must stay, because it opens the path itself and the segment files
  of a local HLS or DASH playlist. The whitelist applies to every open, so
  a crafted manifest on disk can still point FFmpeg at other local files.
  Do not write an upload to a temp file and probe it by path.

Single-file demuxers such as mp4 and mkv do no nested opens, so the
whitelist does not affect them.

## Safety

The crate root has `#![deny(unsafe_code)]`.
`native/exmpeg_native/src/ffi_helpers.rs` is the only module with
`unsafe`. It wraps the few raw FFmpeg calls that `rsmpeg` has no safe
wrapper for. Each block has a `SAFETY:` comment, and unit tests in the same
module cover them.

Every NIF entry point runs in `run_with_panic_protection`. A Rust panic
returns `{:error, %Exmpeg.Error{reason: :nif_panic}}` and does not stop the
BEAM.

## Development

`task check` runs the format check, compile, lint, the Elixir and Rust unit
tests, and `zizmor` on the workflows. `task --list` shows every task. The first `task compile` builds
the NIF and takes several minutes.

`task test:integration` generates a small MP4 with the `ffmpeg` CLI and
checks packet timing with `ffprobe`, so both must be on `PATH`.

## License

MIT. See [LICENSE](LICENSE).
