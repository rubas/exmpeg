# exmpeg usage rules

Rules for AI coding assistants and humans using `exmpeg`.

## Use the library, not the CLI

- Do NOT shell out to `ffmpeg` or `ffprobe` from application code.
- Use `Exmpeg.probe/1` instead of `System.cmd("ffprobe", ...)`.
- Use `Exmpeg.remux/3` for stream copy operations (container conversion,
  trim by time without re-encoding).
- The CLI is only acceptable inside `test/support/fixtures.ex` for
  generating synthetic test inputs - it never appears in `lib/`.

## Always destructure results

- Every public function returns `{:ok, value}` or `{:error, %Exmpeg.Error{}}`.
- Never `Map.get/2` against the error struct - pattern-match on
  `%Exmpeg.Error{reason: reason}` and branch on the reason atom.
- Reasons are stable across releases. Add a new clause when the library
  adds a new reason; do not assume an unknown reason is fatal.

## Stream metadata

- `t:Exmpeg.Stream.t/0` carries `:audio` or `:video` sub-maps populated
  only for the matching `:kind`. Pattern-match on the sub-map shape
  rather than calling `Map.get(stream.audio, :sample_rate)` - the
  latter raises `BadMapError` on a video stream because `stream.audio`
  is `nil`. The pattern-match makes the per-kind branch explicit at
  the call site.
- `:duration_s` is `nil` when ffmpeg cannot derive a duration. Treat
  `nil` as "unknown", not "zero".
- `:frame_rate` is `{0, 1}` (not `nil`) when unknown, matching FFmpeg's
  convention for `AVRational` defaults.

## Remux options

- `:start_s` does **not** seek to a keyframe. If you need precise cuts,
  pass a value that lands on or before a keyframe, or re-encode through
  `Exmpeg.transcode/3`.
- `:duration_s` is **relative to `:start_s`**, not absolute. Total
  output length is approximately `duration_s`.
- `:drop_audio` / `:drop_video` / `:drop_subtitles` skip those stream
  kinds entirely. They are independent — set as many as you need.
- The output container is inferred from the file extension. Mismatches
  (e.g. `.mp4` extension on Matroska content) surface as
  `:unsupported`.

## Errors are typed - act on the reason

```elixir
case Exmpeg.probe(path) do
  {:ok, info} ->
    handle(info)

  {:error, %Exmpeg.Error{reason: :io_error, message: msg}} ->
    Logger.warning("file unreadable: #{msg}")
    :skip

  {:error, %Exmpeg.Error{reason: :invalid_request}} ->
    {:error, :bad_argument}

  {:error, %Exmpeg.Error{} = err} ->
    # Catch-all so a new reason (added in a future release) never
    # raises `CaseClauseError` in production.
    Logger.warning("probe failed: #{Exception.message(err)}")
    :skip
end
```

Do not catch `:nif_panic` and continue - it indicates a Rust-side bug
that should crash the calling process and reach the supervisor.

## Don't add option fallbacks

The library validates options strictly:

```elixir
Exmpeg.remux(input, output, banana: 1)
#=> {:error, %Exmpeg.Error{reason: :invalid_request, message: "unknown option :banana"}}
```

If a feature flag is missing, file an issue or add it - do not swallow
the error and substitute a default.

## Transcode options

- `:video_codec` / `:audio_codec` take encoder short names. The
  precompiled (LGPL) binaries ship `"libvpx-vp9"`, `"aac"`, `"libopus"`,
  `"libmp3lame"`, and `"flac"`. The GPL H.264 / H.265 encoders
  (`"libx264"`, `"libx265"`) are not in the precompiled binaries and
  return `:unsupported` there; build from source (`EXMPEG_BUILD=1`)
  against a GPL-enabled FFmpeg 8 to use them. `"copy"` (or an omitted
  option) stream-copies that media type without re-encoding.
- `:video_filter` accepts an FFmpeg filter-graph spec
  (`"scale=720:-2,fps=30,crop=in_w:in_h-100:0:50"`). Setting it
  overrides the convenience options `:width` / `:height` / `:fps`.
- Audio resampling uses an internal `AVAudioFifo`, so codecs with a
  fixed `frame_size` (AAC, Opus, MP3) accept any input chunk size.
- Mismatched codec / container combinations surface as `:unsupported`.

## Audio extraction container support

`Exmpeg.extract_audio/3` writes the container that matches the output
extension and picks the matching encoder automatically:

| Extension          | Encoder         | Notes                          |
| ------------------ | --------------- | ------------------------------ |
| `.wav`             | `pcm_s16le`     | Default; rawest interop fit.   |
| `.mp3`             | `libmp3lame`    | Lossy, widest playback support.|
| `.m4a` / `.aac`    | `aac`           | AAC-LC in an MP4 audio container.|
| `.opus` / `.ogg`   | `libopus`       | Modern lossy.                  |
| `.flac`            | `flac`          | Lossless.                      |

The build of FFmpeg the NIF is linked against must include the
matching encoder. Missing encoders surface as `:unsupported`.

## Features intentionally not in v0.1

The library does not currently ship:

- Subtitle burn-in / extraction beyond stream-copy.
- HLS / DASH segment muxers.
- Hardware-accelerated device init (`-hwaccel`, `-hwaccel_device`).
  Hardware encoders (`h264_nvenc`, `h264_vaapi`) are still selectable
  by codec name through `:video_codec`, but only when the FFmpeg build
  initialises the device implicitly.

If you need one of these, file an issue with the use case rather than
shelling out to the CLI as a workaround.

## Memory inputs and progress

Every read-side op (`probe/1`, `extract_frame/3`, `extract_audio/3`,
`remux/3`, `concat/3`, `transcode/3`) accepts `{:memory, binary}` in
place of a path. `remux/3`, `extract_audio/3`, `concat/3`, and
`transcode/3` accept `:progress => pid()` and send throttled
`{:exmpeg_progress, %{...}}` messages to the pid. Wrap the call in
`Task.async/1` if you want to receive those messages while the NIF
runs.

## Reuse in-memory bytes with a buffer, not `{:memory, _}`

- `{:memory, binary}` copies the whole binary into the NIF on every
  call. Probing then transcoding the same bytes copies them twice; an
  N-input `concat/3` of memory inputs holds N copies at once.
- When the same bytes feed more than one call, `Exmpeg.load_buffer/1`
  copies once into a reusable `Exmpeg.Buffer` and every later call is a
  refcount bump:

  ```elixir
  {:ok, buf} = Exmpeg.load_buffer(File.read!("clip.mp4"))
  {:ok, info} = Exmpeg.probe(buf)
  {:ok, _} = Exmpeg.transcode(buf, "out.webm", video_codec: "libvpx-vp9")
  ```

- A buffer is accepted anywhere an input source is, including inside a
  `concat/3` list. For a single one-shot read, `{:memory, _}` or a path
  is fine; for large or reused media, prefer a path or a buffer.
