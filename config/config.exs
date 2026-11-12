import Config

# Mirror EXMPEG_BUILD into the config: Mix recompiles Exmpeg.Native when a
# `compile_env` value changes, but not when an env var read at compile time
# changes. So a bare `mix compile`, which downloads the released NIF, never
# leaves a stale NIF behind for the next source build.
config :rustler_precompiled, :force_build, exmpeg: System.get_env("EXMPEG_BUILD") in ["1", "true"]
