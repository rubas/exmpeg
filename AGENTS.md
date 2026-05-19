# AGENTS.md

Project-specific guidance for AI coding assistants working in this repo.
General rules in `~/.claude/CLAUDE.md` still apply.

## What this library is

A Rustler NIF over `rsmpeg` (Rust FFmpeg bindings, FFmpeg 8). Replaces
shelling out to the `ffmpeg` / `ffprobe` CLI with in-process calls.

`v0.1` exposes:

- `Exmpeg.version/0`         - linked FFmpeg version info.
- `Exmpeg.probe/1`           - container + per-stream metadata.
- `Exmpeg.remux/3`           - stream copy between containers with
                               optional time window and stream filters.
- `Exmpeg.extract_frame/3`   - single image at a timestamp.
- `Exmpeg.extract_audio/3`   - audio stream to WAV / MP3 / Opus / FLAC /
                               M4A.
- `Exmpeg.concat/3`          - stream-copy concatenation of multiple
                               inputs.
- `Exmpeg.transcode/3`       - per-stream re-encode with codec / bitrate
                               / scale / fps / filter selection.

## Layout

```
lib/exmpeg.ex                  # public API + option validation
lib/exmpeg/native.ex           # rustler_precompiled stubs (do not call directly)
lib/exmpeg/error.ex            # typed error struct
lib/exmpeg/media_info.ex       # probe result top-level struct
lib/exmpeg/stream.ex           # per-stream metadata struct
native/exmpeg_native/          # Rust crate
  src/lib.rs                   # NIF init + dispatch
  src/probe.rs                 # probe implementation
  src/remux.rs                 # stream-copy implementation
  src/extract_frame.rs         # decode -> swscale -> image2 writer
  src/extract_audio.rs         # decode -> swresample -> WAV/MP3/Opus/...
  src/concat.rs                # multi-input stream-copy
  src/transcode.rs             # decoder + (swscale|filtergraph|swr) + encoder
  src/version.rs               # version reporter
  src/errors.rs                # NIF error mapping
  src/ffi_helpers.rs           # the only `unsafe` in the crate
```

## Working rules

- The Elixir public API is the contract. Native maps are private; always
  go through the `build_*` helpers in `Exmpeg`.
- Every `def` gets a `@spec`. Strict module layout is enforced via
  credo, except for `lib/exmpeg/native.ex` (RustlerPrecompiled requires
  module attributes before `use`).
- Rust crate has `#![deny(unsafe_code)]` at the root. Any `unsafe`
  block must live in `src/ffi_helpers.rs` (the single audit surface)
  and be wrapped in a safe public function with a `SAFETY:` comment.
- Every NIF entry point goes through `run_with_panic_protection` so a
  Rust panic surfaces as `{:error, %{type: "nif_panic"}}` rather than
  crashing the VM.

## Quality gates

```
task fmt:check
task compile      # mix compile --warnings-as-errors (builds NIF)
task lint         # mix credo --strict + cargo clippy -D warnings
task test         # mix test (fast unit tests)
task test:rust    # cargo test
task check        # full gate
```

The integration suite (`mix test --include integration`) requires the
`ffmpeg` CLI on `PATH` for fixture generation. The library itself never
shells out.

## Releasing

See `RELEASE.md` for the full flow. Short version:

1. Bump `@version` in `mix.exs`, update `CHANGELOG.md`, merge to `main`.
2. `.github/workflows/release.yml` builds one tarball per target listed
   in `lib/exmpeg/native.ex` `:targets`, creates the `vX.Y.Z` tag, and
   attaches the tarballs plus `SHA256SUMS` to a GitHub release.
3. Locally: `task checksum:download` to refresh
   `checksum-Elixir.Exmpeg.Native.exs` from the release artefacts. Commit it.
4. `task release:publish` to push to Hex.

## Adding a new operation

1. Add the implementation to `native/exmpeg_native/src/<op>.rs`. Return
   `Result<T, NativeError>`. Categorize errors with the existing
   `"invalid_request" | "io_error" | "decode_error" | "encode_error" |
   "unsupported" | "runtime_error"` taxonomy.
2. Wire a `nif_<op>` entry point in `src/lib.rs` (use `schedule =
   "DirtyIo"` for I/O-bound work, `"DirtyCpu"` for codec work).
3. Add the stub + thin wrapper in `lib/exmpeg/native.ex`.
4. Build the typed public API in `lib/exmpeg.ex`: validate options,
   call into Native, map errors via `Error.from_native/1`.
5. Pin the NIF map shape via a `@doc false def build_*` so refactors
   that drop a field fail compile-time.
6. Test: unit test option validation, NIF contract test the
   `build_*` shape, integration test the round-trip against a synthetic
   fixture.

## Don't

- Don't widen `Exmpeg.Error.reason/0` without adding a `to_reason/1`
  clause _and_ matching tests.
- Don't surface raw `{:error, %{type: _}}` tuples from the NIF to the
  caller - always wrap with `Error.from_native/1`.
- Don't bypass option validators in `lib/exmpeg.ex` "for performance".
  Validation runs once per call and prevents silent NIF errors.
