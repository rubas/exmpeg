# exmpeg

A Rustler NIF over `rsmpeg` (FFmpeg 8) that replaces shelling out to the
`ffmpeg` / `ffprobe` CLIs. It ships on Hex with precompiled NIFs, so the
public API, the option validators, and the error taxonomy are a contract
with strangers.

Two files own what this one does not repeat: the `Exmpeg` moduledoc
documents the public functions and their options, `RELEASE.md` the
release flow.

## Gates

`task check` is the default gate: format check, compile with
`--warnings-as-errors`, credo strict, clippy with `-D warnings`, Elixir
unit tests, Rust tests, and zizmor over the workflows.

`task test:integration` is the expensive one. It builds fixtures with the
`ffmpeg` CLI and asserts packet timing with `ffprobe`, so both must be on
`PATH`; each test skips itself when one is missing. Run it after any
change to a demux, mux, or codec path. CI runs it on every pull request
and on every push to `main`.

The first `task compile` builds the NIF from source and takes several
minutes. devenv sets `EXMPEG_BUILD=1`, so a local build never pulls a
precompiled artefact.

## Layout

`native/exmpeg_native/src/` holds one file per operation plus shared
modules each operation pulls in as it needs them: `input.rs` (path,
`{:memory, _}`, and buffer sources), `atomic_output.rs` (partial file
plus rename), `cancel.rs` (caller-liveness checks), `progress.rs`
(throttled progress messages), `audio.rs` (resampling helpers), and
`ffi_helpers.rs`. A read-only operation uses few of them: `nif_probe`
takes none but `input.rs`, and `nif_version` and `nif_load_buffer` take
none at all.

`lib/exmpeg/native.ex` holds the `rustler_precompiled` stubs and stays
private to the library. Stub names match the Rust NIF symbols verbatim.

## House decisions

- `#![deny(unsafe_code)]` sits at the crate root and `ffi_helpers.rs` is
  the single audit surface: it re-enables `unsafe`, and every block there
  hides behind a safe function with a `SAFETY:` comment. An `unsafe`
  block anywhere else fails the build, which is the point.
- Every NIF entry point runs inside `run_with_panic_protection`, so a
  Rust panic returns `{:error, %{type: "nif_panic"}}` instead of taking
  down the VM.
- A native error uses a `type` string from a closed set:
  `invalid_request`, `io_error`, `decode_error`, `encode_error`,
  `unsupported`, `runtime_error`, `cancelled`, `nif_panic`.
  `Exmpeg.Error.from_native/1` maps it to an atom, and an unknown string
  falls through to `:native_error`.
- The `build_*` functions in `Exmpeg` match the NIF result map strictly
  in the function head, so a dropped field fails there instead of
  producing a half-filled struct. `test/exmpeg/nif_contract_test.exs`
  exercises them without a NIF call.
- Every `def` has a `@spec`, and credo enforces strict module layout.
  `.credo.exs` excludes `lib/exmpeg/native.ex` from the layout check
  because `use RustlerPrecompiled` needs its module attributes first.
- Every input opens with FFmpeg's `protocol_whitelist` pinned:
  `file,crypto,data` for a path, `crypto,data` for `{:memory, _}` and for
  a loaded buffer.
- The precompiled binaries link an LGPL FFmpeg, so `libx264` and
  `libx265` return `:unsupported` there. A source build against a
  GPL-enabled FFmpeg 8 gets them.

## Add an operation

1. Implement it in `native/exmpeg_native/src/<op>.rs`, returning
   `Result<T, NativeError>` with a `type` from the set above.
2. Add the `nif_<op>` entry point in `src/lib.rs`: `schedule = "DirtyIo"`
   for I/O-bound work, `"DirtyCpu"` for codec work.
3. Add the stub and its wrapper in `lib/exmpeg/native.ex`.
4. Build the typed public API in `lib/exmpeg.ex`: validate the options,
   call `Native`, map errors through `Error.from_native/1`.
5. Test three ways: option validation, the NIF map shape in
   `nif_contract_test.exs`, and a round trip against a synthetic fixture
   in `integration_test.exs`.

## Pitfalls

- Never return a raw `{:error, %{type: _}}` NIF map to a caller. Wrap it
  with `Error.from_native/1` so the caller matches on `Exmpeg.Error`.
- Never add a `type` string without its `to_reason/1` clause in
  `lib/exmpeg/error.ex` and a test. Without the clause the new type
  degrades to `:native_error` and nobody notices.
- Never skip an option validator in `lib/exmpeg.ex` for speed. It runs
  once per call and turns a typo into `:invalid_request` instead of an
  opaque native failure.
- Never probe an untrusted upload by path. A path input allows the `file`
  protocol, so a crafted on-disk HLS or DASH manifest points FFmpeg at
  other local files. Pass the bytes as `{:memory, binary}` or through
  `Exmpeg.load_buffer/1`.
- Never shell out from `lib/`. The `ffmpeg` and `ffprobe` CLIs belong to
  `test/support/fixtures.ex` and the integration assertions only.
