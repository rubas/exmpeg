defmodule Exmpeg.Stream do
  @moduledoc """
  Per-stream metadata returned by `Exmpeg.probe/1`.

  `kind` is one of `:video`, `:audio`, `:subtitle`, `:data`,
  `:attachment`, or `:unknown`. The codec-specific maps (`audio`,
  `video`) are populated only on streams of the matching kind so callers
  can match on shape without consulting `kind` first.
  """

  @type kind :: :video | :audio | :subtitle | :data | :attachment | :unknown

  @type audio_info :: %{
          sample_rate: integer(),
          channels: integer(),
          sample_format: String.t() | nil
        }

  @type video_info :: %{
          width: integer(),
          height: integer(),
          pixel_format: String.t() | nil,
          frame_rate: {integer(), integer()}
        }

  @type t :: %__MODULE__{
          index: non_neg_integer(),
          kind: kind(),
          codec: String.t(),
          codec_long_name: String.t() | nil,
          bit_rate: integer(),
          time_base: {integer(), integer()},
          duration_s: float() | nil,
          nb_frames: integer() | nil,
          audio: audio_info() | nil,
          video: video_info() | nil
        }

  @enforce_keys [:index, :kind, :codec, :bit_rate, :time_base]
  defstruct [
    :index,
    :kind,
    :codec,
    :codec_long_name,
    :bit_rate,
    :time_base,
    :duration_s,
    :nb_frames,
    :audio,
    :video
  ]
end
