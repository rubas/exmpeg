defmodule Exmpeg.Native do
  @moduledoc """
  Low-level Rustler bindings to the `rsmpeg` Rust crate.

  This module is private to the library. Use `Exmpeg` for the public API.
  Stub names must match the Rust NIF symbols verbatim (Rustler verifies
  arity at module load time); friendlier wrappers live below them.
  """

  @version Mix.Project.config()[:version]

  use RustlerPrecompiled,
    otp_app: :exmpeg,
    crate: "exmpeg_native",
    base_url: "https://github.com/rubas/exmpeg/releases/download/v#{@version}",
    version: @version,
    # Opt-in source build; unconditional in this repo via `config/config.exs`.
    force_build:
      System.get_env("EXMPEG_BUILD") in ["1", "true"] or
        Application.compile_env(:rustler_precompiled, [:force_build, :exmpeg], false),
    nif_versions: ["2.17"],
    targets: ~w(
      aarch64-apple-darwin
      x86_64-unknown-linux-gnu
      aarch64-unknown-linux-gnu
    )

  @typedoc """
  Input source as seen by the NIF: a filesystem path string,
  `{:memory, binary}`, or a loaded buffer resource reference.
  """
  @type input_source :: String.t() | {:memory, binary()} | reference()

  @doc "Reports the FFmpeg version + configure flags this NIF is linked against."
  @spec version() :: {:ok, map()} | {:error, map()}
  def version, do: nif_version()

  @doc "Copies a binary into a refcounted resource and returns its reference."
  @spec load_buffer(binary()) :: {:ok, reference()} | {:error, map()}
  def load_buffer(binary), do: nif_load_buffer(binary)

  @doc "Probes a media file and returns format + per-stream metadata."
  @spec probe(input_source()) :: {:ok, map()} | {:error, map()}
  def probe(source), do: nif_probe(source)

  @doc "Stream-copies an input container to an output container."
  @spec remux(input_source(), String.t(), map()) :: {:ok, map()} | {:error, map()}
  def remux(input, output, opts), do: nif_remux(input, output, opts)

  @doc "Decodes a single video frame at a timestamp and writes it as an image."
  @spec extract_frame(input_source(), String.t(), map()) :: {:ok, map()} | {:error, map()}
  def extract_frame(input, output, opts), do: nif_extract_frame(input, output, opts)

  @doc "Decodes the best audio stream and writes it as a 16-bit PCM WAV."
  @spec extract_audio(input_source(), String.t(), map()) :: {:ok, map()} | {:error, map()}
  def extract_audio(input, output, opts), do: nif_extract_audio(input, output, opts)

  @doc "Stream-copy concatenation of multiple inputs into a single output."
  @spec concat([input_source()], String.t(), map()) :: {:ok, map()} | {:error, map()}
  def concat(inputs, output, opts), do: nif_concat(inputs, output, opts)

  @doc "Per-stream re-encode with codec / bitrate / scale / fps selection."
  @spec transcode(input_source(), String.t(), map()) :: {:ok, map()} | {:error, map()}
  def transcode(input, output, opts), do: nif_transcode(input, output, opts)

  defp nif_version, do: :erlang.nif_error(:nif_not_loaded)
  defp nif_load_buffer(_binary), do: :erlang.nif_error(:nif_not_loaded)
  defp nif_probe(_source), do: :erlang.nif_error(:nif_not_loaded)
  defp nif_remux(_source, _output, _opts), do: :erlang.nif_error(:nif_not_loaded)
  defp nif_extract_frame(_source, _output, _opts), do: :erlang.nif_error(:nif_not_loaded)
  defp nif_extract_audio(_source, _output, _opts), do: :erlang.nif_error(:nif_not_loaded)
  defp nif_concat(_sources, _output, _opts), do: :erlang.nif_error(:nif_not_loaded)
  defp nif_transcode(_source, _output, _opts), do: :erlang.nif_error(:nif_not_loaded)
end
