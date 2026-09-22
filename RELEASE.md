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
libexmpeg_native-vX.Y.Z-nif-2.17-<target>.so   # NIF
libavformat.so.62 / libavformat.62.dylib
libavcodec.so.62  / libavcodec.62.dylib
libavutil.so.60   / libavutil.60.dylib
libavfilter.so.11 / libavfilter.11.dylib
libswscale.so.9   / libswscale.9.dylib
libswresample.so.6 / libswresample.6.dylib
```

The FFmpeg libs resolve relative to the extracted tarball without
`LD_LIBRARY_PATH`. On Linux the NIF's RPATH is `$ORIGIN`. On macOS every
install name in the tarball is `@loader_path/...`: the ID of each image
and each load command that pointed at the FFmpeg prefix. The macOS link
passes `-dead_strip_dylibs`, so the NIF does not load the unused
`libavdevice` that `rusty_ffmpeg` links.

The job checks the bundle before it packs the tarball. On Linux, `ldd`
without `LD_LIBRARY_PATH` must resolve every library on the runner, so
a bundled FFmpeg library that is missing from the tarball fails with
`=> not found`. This check does not catch a library that the runner has
and the consumer does not. On macOS, every install name must be a
member of the tarball, a system path (`/usr/lib`, `/System/Library`), or
a Homebrew codec formula (`lame`, `opus`, `libvpx`, `webp`).

The bundled FFmpeg is built LGPL-only (no `--enable-gpl` /
`--enable-libx264`) so the tarballs ship under the package's MIT
license. It is also built with `--disable-xlib`, so no library loads
libX11. Codec libraries (libmp3lame / libopus / libvpx / libwebp) are
**not** bundled - consumers install them via their distro package
manager. See README.md's "Runtime requirements" section for the per-OS
install commands.

Add a new target by extending both `lib/exmpeg/native.ex` and the
`matrix.include` list in `.github/workflows/release.yml`. Bump the
`@version` in `mix.exs` to retrigger the release workflow.

## Full release flow

1. **Bump version**

   Edit `mix.exs` and set `@version "X.Y.Z"`. Update `CHANGELOG.md` so
   the new entry sits under the version heading. Open a PR and merge to
   `main`.

2. **CI builds the artefacts**

   On every push to `main`, `.github/workflows/release.yml` reads
   `@version` from `mix.exs`. When the `vX.Y.Z` tag does not exist, it
   builds each NIF target in a separate matrix job, creates the tag, and
   attaches every `*.tar.gz` plus `SHA256SUMS` to a fresh GitHub
   release. The tag marks the version as released. If a run fails
   before it creates the tag, the next push to `main` retries the
   release. If the run fails after it creates the tag, rebuild the tag
   by hand as described below. A push that lands during a release run
   waits for that run to finish, then sees the new tag and skips.

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

To rebuild the artefacts of an existing tag, trigger the workflow
manually:

```bash
gh workflow run release.yml -f tag=v0.1.1
```

The run builds the commit the tag points at, with the workflow file of
the branch it started from. It fails before any build when the tag does
not exist or when the tagged `mix.exs` has a different `@version`. It
replaces the existing release artefacts. Then follow steps 3 and 4
above from a branch at the rebuilt tag, not from `main`, because both
steps read the version from the checkout:

```bash
git switch -c release/v0.1.1 v0.1.1
```

A rebuild never reproduces the old tarballs byte for byte, so their
checksums change. Rebuild only a version that is not on Hex yet, for
example after a failed release run. Consumers of a published version
verify the tarballs against the checksum file in its Hex package.

## When something goes wrong

- **Checksum mismatch on download** — the GitHub release artefacts and
  the matrix builds drifted. Re-run the failed matrix job or
  re-trigger the whole workflow. The checksum command refuses to write
  out partial results.
- **The macOS build job fails with "which the archive does not
  bundle"** - the NIF or a bundled library loads a library the tarball
  does not ship. The error names the member and the load command. Most
  often the runner has a new Homebrew package that FFmpeg's configure
  detects. Disable that feature in the configure call and bump the
  FFmpeg cache key suffix.
- **Missing target after `checksum:download`** — confirm the target
  appears in both `lib/exmpeg/native.ex` `:targets` and the release
  matrix. If a matrix job failed, no tarball exists for that target.
- **`mix hex.publish` rejects the package** — most often a `files:`
  miss in `mix.exs` or a `:rustler_precompiled` version mismatch. The
  hex CLI prints the exact missing file; add it to the `files:` list
  and rerun.
