{ pkgs, lib, ... }:
{
  imports = [
    ../dev/modules/elixir.nix
    ../dev/modules/rust.nix
  ];

  # rsmpeg drives bindgen against the system FFmpeg headers. Bindgen
  # needs libclang plus the libc include path; pkg-config (provided by
  # the elixir module) locates the FFmpeg .pc files.
  #
  # `pkgs.glibc` is undefined on Darwin, so the glibc.dev package and
  # its BINDGEN env entry are gated to Linux. On Darwin the system SDK
  # headers cover the same role and bindgen finds them automatically.
  packages = [
    pkgs.ffmpeg
    pkgs.libclang
  ]
  ++ lib.optional pkgs.stdenv.isLinux pkgs.glibc.dev;

  env = {
    LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
    # Force a from-source NIF build during development - precompiled
    # artefacts are only fetched by published-Hex consumers.
    EXMPEG_BUILD = "1";
  }
  // lib.optionalAttrs pkgs.stdenv.isLinux {
    BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.glibc.dev}/include";
  };
}
