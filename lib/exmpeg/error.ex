defmodule Exmpeg.Error do
  @moduledoc """
  Structured error returned from `Exmpeg` calls.

  `reason` is one of:

  - `:invalid_request` - bad arguments (missing path, malformed opts, non-numeric times).
  - `:io_error`        - libavformat could not open or read the input / output file.
  - `:decode_error`    - the decoder rejected a packet or could not drain a frame.
  - `:encode_error`    - the encoder rejected a frame or could not drain a packet.
  - `:unsupported`     - the build of FFmpeg this NIF is linked against has no muxer / demuxer / codec for the request.
  - `:runtime_error`   - internal NIF runtime fault (e.g. unexpected ffmpeg state).
  - `:nif_panic`       - the Rust side panicked; should never happen in practice.
  - `:native_error`    - fallback for unrecognised native error types.
  """

  @type reason ::
          :invalid_request
          | :io_error
          | :decode_error
          | :encode_error
          | :unsupported
          | :runtime_error
          | :nif_panic
          | :native_error

  @type t :: %__MODULE__{
          reason: reason(),
          message: String.t(),
          details: %{optional(String.t()) => String.t()}
        }

  defexception [:reason, :message, details: %{}]

  @spec new(reason(), String.t(), %{optional(String.t()) => String.t()}) :: t()
  def new(reason, message, details \\ %{}) do
    %__MODULE__{reason: reason, message: message, details: details}
  end

  @spec from_native(map()) :: t()
  def from_native(%{type: type, message: message} = payload) do
    new(to_reason(type), message, Map.get(payload, :details, %{}))
  end

  def from_native(other) do
    new(:native_error, "unexpected native error payload", %{"raw" => inspect(other)})
  end

  defp to_reason("invalid_request"), do: :invalid_request
  defp to_reason("io_error"), do: :io_error
  defp to_reason("decode_error"), do: :decode_error
  defp to_reason("encode_error"), do: :encode_error
  defp to_reason("unsupported"), do: :unsupported
  defp to_reason("runtime_error"), do: :runtime_error
  defp to_reason("nif_panic"), do: :nif_panic
  defp to_reason(_), do: :native_error

  @impl Exception
  def message(%__MODULE__{reason: reason, message: msg}), do: "#{reason}: #{msg}"
end
