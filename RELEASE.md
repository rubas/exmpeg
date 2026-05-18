# Releasing exmpeg

This package ships precompiled NIFs through `rustler_precompiled`, so a
release has three parts:

1. **Tag** + **GitHub Release** with one prebuilt NIF tarball per
   target. Built by `.github/workflows/release.yml`.
2. **Checksum file** (`checksum-Elixir.Exmpeg.Native.exs`) committed to
   the repo. Built **locally** by `task checksum:download`.
3. **Hex package** uploaded with `task release:publish`. The checksum
   file is included in the Hex tarball and is what consumers' builds
   verify against.

## Targets

The release workflow builds tarballs named:

```
libexmpeg_native-vX.Y.Z-nif-2.17-<target>.so.tar.gz
```

for every `<target>` listed in `lib/exmpeg/native.ex`'s
`use RustlerPrecompiled` block:

- `aarch64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`

### Tarball contents

Each tarball contains the NIF and the six FFmpeg shared libraries it
loads at runtime:

```
libexmpeg_native-vX.Y.Z-nif-2.17-<target>.so   # NIF (RPATH=$ORIGIN / @loader_path)
libavformat.so.62 / libavformat.62.dylib
libavcodec.so.62  / libavcodec.62.dylib
libavutil.so.60   / libavutil.60.dylib
libavfilter.so.11 / libavfilter.11.dylib
libswscale.so.9   / libswscale.9.dylib
libswresample.so.6 / libswresample.6.dylib
```

The NIF's RPATH is patched to `$ORIGIN` (Linux) or `@loader_path`
(macOS) so the FFmpeg libs resolve relative to the extracted tarball
without `LD_LIBRARY_PATH`. Codec libraries (libx264 / libmp3lame /
libopus / libvpx) are **not** bundled — consumers install them via
their distro package manager. See README.md's "Runtime requirements"
section for the per-OS install commands.

Add a new target by extending both `lib/exmpeg/native.ex` and the
`matrix.include` list in `.github/workflows/release.yml`. Bump the
`@version` in `mix.exs` to retrigger the release workflow.

## Full release flow

1. **Bump version**

   Edit `mix.exs` and set `@version "X.Y.Z"`. Update `CHANGELOG.md` so
   the new entry sits under the version heading. Open a PR and merge to
   `main`.

2. **CI builds the artefacts**

   On push to `main`, `.github/workflows/release.yml` detects the
   version change, builds each NIF target in a separate matrix job,
   creates the `vX.Y.Z` tag, and attaches every `*.tar.gz` plus
   `SHA256SUMS` to a fresh GitHub release.

   Wait for the workflow to finish. Confirm the tarballs are on the
   release page (`https://github.com/rubas/exmpeg/releases/tag/vX.Y.Z`).

3. **Refresh the checksum file locally**

   Pull `main` so your working copy is at the tagged commit, then:

   ```bash
   task checksum:download
   ```

   This is a thin wrapper around `mix rustler_precompiled.download
   Exmpeg.Native --all --print`, which downloads every tarball
   referenced by the `base_url` in `lib/exmpeg/native.ex`, verifies
   them against `SHA256SUMS`, and rewrites
   `checksum-Elixir.Exmpeg.Native.exs` in place.

   Commit the regenerated `checksum-Elixir.Exmpeg.Native.exs`. The
   diff should contain only the checksum entries for the new version.

4. **Publish to Hex**

   ```bash
   task release:publish
   ```

   This runs `mix deps.get`, `mix hex.build` (a final sanity check
   that the package compiles cleanly), then `mix hex.publish`. The
   command will prompt for confirmation and your Hex API key.

   The `files:` list in `mix.exs` includes the checksum file, so the
   published Hex package contains the verified hashes for every
   precompiled target.

## Manual / out-of-band release

To rebuild artefacts without bumping `@version`, trigger the workflow
manually:

```bash
gh workflow run release.yml -f tag=v0.1.1
```

It will build for the tag-derived version, replace the existing release
artefacts (if any), and re-tag if the tag doesn't already exist. Then
follow steps 3 and 4 above.

## When something goes wrong

- **Checksum mismatch on download** — the GitHub release artefacts and
  the matrix builds drifted. Re-run the failed matrix job or
  re-trigger the whole workflow. The checksum command refuses to write
  out partial results.
- **Missing target after `checksum:download`** — confirm the target
  appears in both `lib/exmpeg/native.ex` `:targets` and the release
  matrix. If a matrix job failed, no tarball exists for that target.
- **`mix hex.publish` rejects the package** — most often a `files:`
  miss in `mix.exs` or a `:rustler_precompiled` version mismatch. The
  hex CLI prints the exact missing file; add it to the `files:` list
  and rerun.
