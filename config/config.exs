import Config

# When developing this library locally, always build the NIF from source
# rather than trying to download a precompiled artefact from GitHub releases.
# Consumers of the published Hex package get the precompiled artefact
# matching their target triple instead.
config :rustler_precompiled, :force_build, exmpeg: true
