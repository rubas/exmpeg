defmodule Exmpeg.MixProject do
  use Mix.Project

  @version "0.3.0"
  @source_url "https://github.com/rubas/exmpeg"

  @spec project() :: keyword()
  def project do
    [
      app: :exmpeg,
      version: @version,
      elixir: "~> 1.17",
      start_permanent: Mix.env() == :prod,
      elixirc_paths: elixirc_paths(Mix.env()),
      description: description(),
      package: package(),
      docs: docs(),
      deps: deps()
    ]
  end

  @spec application() :: keyword()
  def application do
    [extra_applications: [:logger]]
  end

  defp elixirc_paths(:test), do: ["lib", "test/support"]
  defp elixirc_paths(_), do: ["lib"]

  @spec docs() :: keyword()
  defp docs do
    [
      main: "readme",
      extras: ["README.md", "CHANGELOG.md", "usage-rules.md", "LICENSE"],
      source_url: @source_url,
      source_ref: "v#{@version}",
      homepage_url: @source_url
    ]
  end

  @spec description() :: String.t()
  defp description do
    "Native Elixir bindings for FFmpeg via the rsmpeg Rust crate. " <>
      "Replaces shelling out to the ffmpeg/ffprobe CLI with an in-process NIF."
  end

  @spec package() :: keyword()
  defp package do
    [
      licenses: ["MIT"],
      links: %{
        "GitHub" => @source_url,
        "rsmpeg" => "https://crates.io/crates/rsmpeg",
        "FFmpeg" => "https://ffmpeg.org/"
      },
      files:
        ~w(lib native/exmpeg_native/src native/exmpeg_native/Cargo.toml native/exmpeg_native/Cargo.lock checksum-*.exs mix.exs README.md CHANGELOG.md LICENSE* usage-rules.md)
    ]
  end

  @spec deps() :: [tuple()]
  defp deps do
    [
      # `rustler_precompiled` selects a prebuilt NIF artefact at install time
      # from the GitHub release matching the package version. `rustler` is
      # only needed for source builds (`EXMPEG_BUILD=1`) and during release
      # CI, so it is marked optional.
      {:rustler_precompiled, "~> 0.8"},
      {:rustler, "~> 0.37.3", optional: true},
      {:credo, "~> 1.7.18", only: [:dev, :test], runtime: false},
      {:ex_slop, "~> 0.4", only: [:dev, :test], runtime: false},
      {:ex_dna, "~> 1.5", only: [:dev, :test], runtime: false},
      {:ex_doc, "~> 0.40", only: :dev, runtime: false}
    ]
  end
end
