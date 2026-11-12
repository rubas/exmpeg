# exmpeg

The README describes the library, its requirements, and the untrusted-input
rules. The `Exmpeg` moduledoc documents the public functions and their
options, and `RELEASE.md` the release flow. exmpeg ships on Hex, so the
public API, the option validators, and the error reasons are a contract
with strangers.

## Checks

- Run `task test:integration` after any change to a demux, mux, or codec
  path. It needs `ffmpeg` and `ffprobe` on `PATH`; each test skips itself
  when one is missing. CI runs it on every pull request and every push to
  `main`.
- The Taskfile and CI set `EXMPEG_BUILD=1`, so their builds compile the
  NIF from source. A bare `mix compile` downloads the precompiled NIF of
  the GitHub release and ignores your Rust changes. After a switch between
  the two, Mix stops with a `compile_env` error; run `mix compile --force`.
- The toolchain comes from the host: the Elixir, OTP, and Rust versions in
  `.github/workflows/ci.yml`, plus FFmpeg 9 with its headers, `pkg-config`,
  and libclang. On Arch that is `pacman -S ffmpeg clang`. A distro with an
  older FFmpeg builds 9 from source as `.github/actions/setup/action.yml`
  does.

## Rules

- `unsafe` lives only in `native/exmpeg_native/src/ffi_helpers.rs`. Put
  each block behind a safe function with a `SAFETY:` comment. The crate
  root has `#![deny(unsafe_code)]`, so `unsafe` anywhere else fails the
  build.
- A native error has a `type` string from a closed set: `invalid_request`,
  `io_error`, `decode_error`, `encode_error`, `unsupported`,
  `runtime_error`, `cancelled`, `nif_panic`. `Exmpeg.Error.from_native/1`
  maps it to an atom. A new `type` needs its `to_reason/1` clause in
  `lib/exmpeg/error.ex` and a test, or it silently becomes `:native_error`.
- Wrap every NIF error with `Error.from_native/1`. Never return a raw
  `{:error, %{type: _}}` map to a caller.
- The `build_*` functions in `Exmpeg` match the NIF result map strictly in
  the function head, so a missing field fails there and does not produce a
  half-filled struct. `test/exmpeg/nif_contract_test.exs` tests them
  without a NIF call.
- Keep every option validator in `lib/exmpeg.ex`. It turns a typo into
  `:invalid_request` instead of an unclear native failure.
- Every `def` has a `@spec`. Credo enforces strict module layout;
  `.credo.exs` excludes `lib/exmpeg/native.ex`, because
  `use RustlerPrecompiled` needs its module attributes first.
- `lib/exmpeg/native.ex` holds the `rustler_precompiled` stubs and is
  private to the library. Stub names match the Rust NIF symbols exactly.
- Never shell out from `lib/`. Only `test/support/fixtures.ex` and the
  integration assertions use the `ffmpeg` and `ffprobe` CLIs.
- A merge to `main` that bumps `@version` in `mix.exs` tags the version
  and builds the release tarballs. A merge that changes
  `checksum-Elixir.Exmpeg.Native.exs` publishes the version to Hex, and
  Hex versions are immutable. Change each only in its release PR;
  `RELEASE.md` has the flow.
- `rsmpeg` comes from our fork `rubas/rsmpeg` at a pinned `rev`. The
  `TODO(revert: ...)` in `native/exmpeg_native/Cargo.toml` names when to go
  back to crates.io. `task upgrade` skips git dependencies, so move the
  `rev` by hand.

## Add an operation

1. Implement it in `native/exmpeg_native/src/<op>.rs`. Return
   `Result<T, NativeError>` with a `type` from the set above.
2. Add the `nif_<op>` entry point in `src/lib.rs` inside
   `run_with_panic_protection`. Use `schedule = "DirtyIo"` for I/O work and
   `"DirtyCpu"` for codec work.
3. Add the stub and its wrapper in `lib/exmpeg/native.ex`.
4. Add the typed public function in `lib/exmpeg.ex`: validate the options,
   call `Native`, and map errors through `Error.from_native/1`.
5. Test three ways: option validation, the NIF map shape in
   `nif_contract_test.exs`, and a round trip on a synthetic fixture in
   `integration_test.exs`.
