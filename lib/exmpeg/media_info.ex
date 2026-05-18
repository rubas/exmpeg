defmodule Exmpeg.MediaInfo do
  @moduledoc """
  Top-level result of `Exmpeg.probe/1`.

  Mirrors what `ffprobe -show_format -show_streams` would print: a
  container-level summary plus a list of `Exmpeg.Stream` entries. Both
  shapes are stable across releases; new fields will only be added.
  """

  alias Exmpeg.Stream

  @type format :: %{
          name: String.t(),
          long_name: String.t() | nil,
          duration_s: float() | nil,
          bit_rate: integer(),
          start_time_s: float() | nil,
          nb_streams: non_neg_integer(),
          tags: %{optional(String.t()) => String.t()}
        }

  @type t :: %__MODULE__{
          format: format(),
          streams: [Stream.t()]
        }

  @enforce_keys [:format, :streams]
  defstruct [:format, :streams]

  @doc """
  Convenience: return the first stream matching `kind`, or `nil`.

      iex> Exmpeg.MediaInfo.first(info, :video)
      %Exmpeg.Stream{kind: :video, ...}
  """
  @spec first(t(), Stream.kind()) :: Stream.t() | nil
  def first(%__MODULE__{streams: streams}, kind) when is_atom(kind) do
    Enum.find(streams, &(&1.kind == kind))
  end

  @doc "Convenience: list every stream matching `kind`."
  @spec all(t(), Stream.kind()) :: [Stream.t()]
  def all(%__MODULE__{streams: streams}, kind) when is_atom(kind) do
    Enum.filter(streams, &(&1.kind == kind))
  end
end
