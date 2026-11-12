# Releasing exmpeg

This package ships precompiled NIFs through `rustler_precompiled`, so a
release has three parts:

1. **Tag** + **GitHub Release** with one prebuilt NIF tarball per
   target. Built by `.github/workflows/release.yml`.
2. **Checksum file** (`checksum-Elixir.Exmpeg.Native.exs`) committed to
   the repo. Built **locally** by `task checksum:download`.
3. **Hex package** published by `.github/workflows/release.yml` when
   the checksum PR merges. The checksum file is included in the Hex
   tarball and is what consumers' builds verify against.

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

Each tarball contains the NIF and the seven FFmpeg shared libraries that
`rusty_ffmpeg` links, built from the FFmpeg release that `FFMPEG_VERSION`
in `.github/workflows/release.yml` names (9.0.2):

```
libexmpeg_native-vX.Y.Z-nif-2.17-<target>.so   # NIF
libavformat.so.63   / libavformat.63.dylib
libavcodec.so.63    / libavcodec.63.dylib
libavutil.so.61     / libavutil.61.dylib
libavfilter.so.12   / libavfilter.12.dylib
libswscale.so.10    / libswscale.10.dylib
libswresample.so.7  / libswresample.7.dylib
libavdevice.so.63   / libavdevice.63.dylib
```

The FFmpeg libs resolve relative to the extracted tarball without
`LD_LIBRARY_PATH`. On Linux the NIF's RPATH is `$ORIGIN`. On macOS every
install name in the tarball is `@loader_path/...`: the ID of each image
and each load command that pointed at the FFmpeg prefix. The Linux NIF
does not load `libavdevice`, because the linker drops it as unused. The
macOS NIF loads it.

The job checks the bundle before it packs the tarball. On Linux, `ldd`
without `LD_LIBRARY_PATH` must resolve every library on the runner, so
a bundled FFmpeg library that is missing from the tarball fails with
`=> not found`. This check does not catch a library that the runner has
and the consumer does not. On macOS, every install name must be a
member of the tarball, a system path (`/usr/lib`, `/System/Library`), or
a Homebrew codec formula (`lame`, `opus`, `libvpx`, `webp`). On both, the
job also binds every symbol of the NIF, as the BEAM does when it loads
it, and fails on a symbol that no library defines. Only the BEAM defines
`enif_*`.

The bundled FFmpeg is built LGPL-only (no `--enable-gpl` /
`--enable-libx264`) so the tarballs ship under the package's MIT
license. It is also built with `--disable-xlib`, so no library loads
libX11, and with `--disable-devices`, so `libavdevice` loads no capture
or playback library. Codec libraries (libmp3lame / libopus / libvpx /
libwebp) are **not** bundled - consumers install them via their distro
package manager. See README.md's "Runtime requirements" section for the per-OS
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

   It compiles `Exmpeg.Native` again first, because the download reads
   the version and the targets from the metadata of the last compile, in
   a user cache that every checkout shares. Then it runs `mix
   rustler_precompiled.download Exmpeg.Native --all --print`, which
   downloads every tarball
   referenced by the `base_url` in `lib/exmpeg/native.ex`, hashes
   them, and rewrites `checksum-Elixir.Exmpeg.Native.exs` in place.
   It does not read `SHA256SUMS`; the publish job compares the two.

   Commit the regenerated `checksum-Elixir.Exmpeg.Native.exs`, and
   open a pull request. The diff should contain only the checksum
   entries for the new version.

4. **Merge the checksum PR**

   The merge publishes. A push to `main` that changes the checksum
   file, for a version that has its tag, starts the `Publish to Hex`
   job of `.github/workflows/release.yml`. The job publishes the
   sources of the tag `vX.Y.Z`, which the tarballs were built from, with
   the checksum file of the merge:

   1. It compares the checksum file with the `SHA256SUMS` of the
      GitHub release, and stops when they differ.
   2. It installs the codec libraries from README.md, downloads the
      precompiled NIF of its runner, checks it against the checksum
      file, and loads it.
   3. It runs `mix hex.publish --yes`, which uploads the package and
      its docs, and fails when the log says `Publishing failed` or
      `Publishing docs failed`.

   The job stores no Hex key. It runs in the GitHub environment `hex`,
   which only `main` can use, and Hex exchanges the job's OIDC token
   for a token that can only publish `exmpeg`. For that, the package
   has a workload identity on hex.pm, in its tab "Workload
   identities": repository owner `rubas`, repository `exmpeg`,
   workflow `release.yml`, environment `hex`. Only a full owner of
   the package can add or change it.

   The package has staged publishing on. Hex holds each release from
   the job, and the job ends with `Package staged for approval at
   <url>`. A package owner approves it on hex.pm with their 2FA,
   within 7 days. The job does not wait for that approval: a GitHub
   job stops after 6 hours.

   When the job fails for a cause outside the repo, such as the
   workload identity, fix it and re-run the job: it publishes the same
   tag. A cause in the tag needs a new patch version. A re-run keeps
   the workflow file of its run, so after a fix of the job itself,
   release a new patch version, or a package owner runs `mix
   hex.publish` from the tag with the checksum file of the merge.
   When GitHub cancelled the run of that push, because newer pushes
   queued behind it, re-run the cancelled run.

   The `files:` list in `mix.exs` includes the checksum file, so the
   published Hex package contains the verified hashes for every
   precompiled target.

   A change that merges between the version bump and the checksum PR
   ships only with the next version. The tag
   `vX.Y.Z` points at the version bump, so its checksum file is still
   the one of the release before. A git dependency on the tag needs
   `EXMPEG_BUILD=1`.

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
above.

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
  hex CLI prints the exact missing file. Add it to the `files:` list,
  and release a new patch version.
- **The publish job fails with `access_denied`** - the workload
  identity on hex.pm does not match the repository, the workflow file,
  or the environment `hex`.
