defmodule Exmpeg do
  @moduledoc """
  Native Elixir bindings for FFmpeg via the `rsmpeg` Rust crate.

  Replaces shelling out to the `ffmpeg` / `ffprobe` CLIs with an
  in-process NIF: every call runs against the FFmpeg shared libraries
  this NIF was linked at compile time, and structured results come back
  as plain Elixir structs and maps.

  ## Quickstart

      {:ok, info} = Exmpeg.probe("input.mkv")
      info.format.duration_s
      #=> 12.345
      Exmpeg.MediaInfo.first(info, :video).codec
      #=> "h264"

      {:ok, %{packets_written: n}} =
        Exmpeg.remux("input.mkv", "output.mp4")

  ## Scope

  This release covers:

  - `version/0` - linked FFmpeg version info.
  - `probe/1` - container + per-stream metadata (`ffprobe`).
  - `remux/3` - stream copy between containers, optionally trimmed by a
    start/duration window (`ffmpeg -i ... -c copy ...`).
  - `extract_frame/3` - single image at a timestamp (`.jpg`, `.png`,
    `.bmp`, `.webp`).
  - `extract_audio/3` - audio stream to `.wav`, `.mp3`, `.m4a`/`.aac`,
    `.opus`/`.ogg`, or `.flac`.
  - `concat/3` - stream-copy concatenation of multiple inputs that
    share the same stream layout.
  - `transcode/3` - per-stream re-encode with codec, bitrate, scale,
    fps and filter selection.

  ## Output atomicity

  Operations that write to disk (`remux/3`, `extract_frame/3`,
  `extract_audio/3`, `concat/3`, `transcode/3`) write to a sibling
  `<stem>.partial.<ext>` file and rename onto the final path only
  after the muxer trailer has been written successfully. A failure
  mid-encode removes the partial file so the destination is never
  left half-written.
  """

  alias Exmpeg.{Error, MediaInfo, Native, Stream}

  @typedoc "Options accepted by `remux/3`."
  @type remux_opt ::
          {:start_s, number()}
          | {:duration_s, number()}
          | {:drop_audio, boolean()}
          | {:drop_video, boolean()}
          | {:drop_subtitles, boolean()}
          | {:tags, [{String.t(), String.t()}] | %{optional(String.t()) => String.t()}}
          | {:progress, pid()}

  @typedoc "Stats returned by `remux/3`."
  @type remux_stats :: %{
          packets_written: non_neg_integer(),
          packets_dropped: non_neg_integer(),
          streams_copied: non_neg_integer()
        }

  @typedoc "Options accepted by `extract_frame/3`."
  @type extract_frame_opt ::
          {:timestamp_s, number()}
          | {:width, pos_integer()}
          | {:height, pos_integer()}

  @typedoc "Stats returned by `extract_frame/3`."
  @type extract_frame_stats :: %{
          width: pos_integer(),
          height: pos_integer(),
          timestamp_s: float(),
          pts_known: boolean(),
          codec: String.t()
        }

  @typedoc "Options accepted by `extract_audio/3`."
  @type extract_audio_opt ::
          {:sample_rate, pos_integer()}
          | {:channels, 1..2}
          | {:bitrate, pos_integer()}
          | {:progress, pid()}

  @typedoc "Stats returned by `extract_audio/3`."
  @type extract_audio_stats :: %{
          sample_rate: pos_integer(),
          channels: 1..2,
          samples_written: non_neg_integer(),
          duration_s: float(),
          codec: String.t()
        }

  @typedoc "Stats returned by `concat/2`."
  @type concat_stats :: %{
          packets_written: non_neg_integer(),
          inputs_joined: non_neg_integer(),
          streams_copied: non_neg_integer(),
          duration_s: float()
        }

  @typedoc """
  Options accepted by `transcode/3`.

  Codec selection uses encoder short names (`"libx264"`, `"libx265"`,
  `"libvpx-vp9"`, `"aac"`, `"libopus"`, `"libmp3lame"`, `"flac"`). Pass
  `"copy"` (or omit) to stream-copy that media type.
  """
  @type transcode_opt ::
          {:video_codec, String.t()}
          | {:audio_codec, String.t()}
          | {:video_bitrate, pos_integer()}
          | {:audio_bitrate, pos_integer()}
          | {:width, pos_integer()}
          | {:height, pos_integer()}
          | {:fps, {pos_integer(), pos_integer()}}
          | {:sample_rate, pos_integer()}
          | {:channels, 1..2}
          | {:video_filter, String.t()}
          | {:drop_audio, boolean()}
          | {:drop_video, boolean()}
          | {:drop_subtitles, boolean()}
          | {:tags, [{String.t(), String.t()}] | %{optional(String.t()) => String.t()}}
          | {:progress, pid()}

  @typedoc "Stats returned by `transcode/3`."
  @type transcode_stats :: %{
          streams_copied: non_neg_integer(),
          streams_reencoded: non_neg_integer(),
          packets_written: non_neg_integer(),
          duration_s: float()
        }

  @doc """
  Returns the version of every FFmpeg sub-library this NIF is linked
  against, plus the `./configure` flags used to build them.

      iex> {:ok, %{avformat: avformat}} = Exmpeg.version()
      iex> String.match?(avformat, ~r/^\\d+\\.\\d+\\.\\d+$/)
      true
  """
  @spec version() ::
          {:ok,
           %{
             avformat: String.t(),
             avcodec: String.t(),
             avutil: String.t(),
             license: String.t(),
             configuration: String.t()
           }}
          | {:error, Error.t()}
  def version do
    case Native.version() do
      {:ok, info} -> {:ok, info}
      {:error, payload} -> {:error, Error.from_native(payload)}
    end
  end

  @doc """
  Probes `path` and returns container / stream metadata.

  Reads the file with `avformat_open_input` + `avformat_find_stream_info`,
  so the result reflects what the FFmpeg demuxer actually sees - not what
  the file extension suggests.
  """
  @typedoc """
  Input source. Either a filesystem path (`String.t()`) or
  `{:memory, binary}` to read the entire input from an in-memory
  buffer through a custom AVIOContext.
  """
  @type input_source :: Path.t() | {:memory, binary()}

  @spec probe(input_source()) :: {:ok, MediaInfo.t()} | {:error, Error.t()}
  def probe(source) do
    with :ok <- validate_input(source, :input),
         {:ok, payload} <- native_call(Native.probe(source)) do
      {:ok, build_media_info(payload)}
    end
  end

  @doc """
  Stream-copies `input` to `output` without re-encoding.

  Every input stream is added to the output container with codec
  parameters preserved verbatim. The output container is inferred from
  the file extension (`.mp4`, `.mkv`, `.mov`, ...). A muxer / codec
  combination that the FFmpeg build does not support returns
  `{:error, %Error{reason: :unsupported}}`.

  ## Options

  - `:start_s` - drop packets whose pts is earlier than this offset (in
    seconds). The result is not keyframe-aligned: video that does not
    start on a keyframe will be unplayable until the next keyframe.
  - `:duration_s` - stop after this many seconds past `:start_s`.

  ## Returns

  A stats map of what the muxer accepted:

      %{packets_written: 1234, packets_dropped: 0, streams_copied: 2}
  """
  @spec remux(input_source(), Path.t(), [remux_opt()]) ::
          {:ok, remux_stats()} | {:error, Error.t()}
  def remux(input, output, opts \\ [])

  def remux(input, output, opts) when is_binary(output) and is_list(opts) do
    with :ok <- validate_input(input, :input),
         :ok <- validate_non_empty_string(output, :output),
         :ok <- validate_options(opts, remux_validators()) do
      native_call(Native.remux(input, output, build_remux_opts(opts)))
    end
  end

  def remux(_input, _output, _opts) do
    {:error, Error.new(:invalid_request, "output must be a string and opts a keyword list")}
  end

  @doc """
  Decodes one video frame from `input` at `:timestamp_s` (default `0.0`)
  and writes it as an image at `output`.

  The output codec is inferred from the extension:

  - `.jpg` / `.jpeg` -> MJPEG
  - `.png`           -> PNG
  - `.bmp`           -> BMP
  - `.webp`          -> WebP

  ## Options

  - `:timestamp_s` - capture point in seconds (default `0.0`). The
    decoder seeks to the preceding keyframe and decodes forward, so the
    actually-returned timestamp may be a few hundred milliseconds early
    or late depending on the GOP structure. The exact pts of the
    returned frame is reported in the result map.
  - `:width` / `:height` - resize to this size in pixels. When only one
    dimension is given the other is computed to preserve the source
    aspect ratio. Both are rounded down to the nearest even value so the
    encoder's pixel format requirements are met.

  ## Returns

      %{width: 1280, height: 720, timestamp_s: 1.501, pts_known: true, codec: "mjpeg"}
  """
  @spec extract_frame(input_source(), Path.t(), [extract_frame_opt()]) ::
          {:ok, extract_frame_stats()} | {:error, Error.t()}
  def extract_frame(input, output, opts \\ [])

  def extract_frame(input, output, opts) when is_binary(output) and is_list(opts) do
    with :ok <- validate_input(input, :input),
         :ok <- validate_non_empty_string(output, :output),
         :ok <- validate_options(opts, extract_frame_validators()) do
      native_call(Native.extract_frame(input, output, build_extract_frame_opts(opts)))
    end
  end

  def extract_frame(_input, _output, _opts) do
    {:error, Error.new(:invalid_request, "output must be a string and opts a keyword list")}
  end

  @doc """
  Decodes the best audio stream of `input` and writes it to `output`.

  The encoder is picked from the output extension:

  | Extension          | Encoder         |
  | ------------------ | --------------- |
  | `.wav`             | `pcm_s16le`     |
  | `.mp3`             | `libmp3lame`    |
  | `.m4a` / `.aac`    | `aac`           |
  | `.opus` / `.ogg`   | `libopus`       |
  | `.flac`            | `flac`          |

  ## Options

  - `:sample_rate` - target sample rate in Hz (default: source). For
    codecs that only accept a fixed list of rates (libopus snaps to
    `[8000, 12000, 16000, 24000, 48000]`), the closest supported rate
    is used.
  - `:channels` - `1` for mono or `2` for stereo. Defaults to the
    source layout when the source is mono or stereo; sources with
    more channels (5.1, 7.1, ...) require an explicit value and
    otherwise return `:invalid_request`.
  - `:bitrate` - target bitrate in bps. Ignored by lossless codecs
    (`pcm_s16le`, `flac`); used as a quality hint for the lossy
    codecs.

  ## Returns

      %{
        sample_rate: 16_000,
        channels: 1,
        samples_written: 32_322,
        duration_s: 2.020125,
        codec: "pcm_s16le"
      }
  """
  @spec extract_audio(input_source(), Path.t(), [extract_audio_opt()]) ::
          {:ok, extract_audio_stats()} | {:error, Error.t()}
  def extract_audio(input, output, opts \\ [])

  def extract_audio(input, output, opts) when is_binary(output) and is_list(opts) do
    with :ok <- validate_input(input, :input),
         :ok <- validate_non_empty_string(output, :output),
         :ok <- validate_options(opts, extract_audio_validators()) do
      native_call(Native.extract_audio(input, output, build_extract_audio_opts(opts)))
    end
  end

  def extract_audio(_input, _output, _opts) do
    {:error, Error.new(:invalid_request, "output must be a string and opts a keyword list")}
  end

  @doc """
  Joins `inputs` into a single `output` without re-encoding.

  Every input must share the same stream layout (same number of streams
  and same codec id per stream index). Mismatches return
  `{:error, %Error{reason: :invalid_request}}`.

  PTS / DTS values are shifted by the cumulative duration of preceding
  inputs so the resulting timeline is monotonic.

  ## Returns

      %{packets_written: 3456, inputs_joined: 3, streams_copied: 2, duration_s: 6.04}
  """
  @typedoc "Options accepted by `concat/3`."
  @type concat_opt :: {:progress, pid()}

  @spec concat([input_source()], Path.t(), [concat_opt()]) ::
          {:ok, concat_stats()} | {:error, Error.t()}
  def concat(inputs, output, opts \\ [])

  def concat(inputs, output, opts) when is_list(inputs) and is_binary(output) and is_list(opts) do
    with :ok <- validate_non_empty_string(output, :output),
         :ok <- validate_concat_inputs(inputs),
         :ok <- validate_options(opts, concat_validators()) do
      native_call(Native.concat(inputs, output, build_concat_opts(opts)))
    end
  end

  def concat(_inputs, _output, _opts) do
    {:error, Error.new(:invalid_request, "inputs must be a list and output a string")}
  end

  @doc """
  Re-encodes `input` to `output` with per-stream codec selection.

  Each stream is either copied or re-encoded based on the corresponding
  `:video_codec` / `:audio_codec` option. `"copy"` (or an omitted option)
  preserves the source codec; any other value is resolved through
  `avcodec_find_encoder_by_name` - if FFmpeg wasn't built with that
  encoder, the call returns `{:error, %Error{reason: :unsupported}}`.

  ## Options

  - `:video_codec` / `:audio_codec` - encoder short name (default
    `"copy"`).
  - `:video_bitrate` / `:audio_bitrate` - target bitrate in bps.
  - `:width` / `:height` - output video size in pixels. Specifying one
    derives the other from the source aspect ratio. Always rounded down
    to the nearest even value.
  - `:fps` - target framerate as `{num, den}`. Defaults to the source.
  - `:sample_rate` - target audio sample rate in Hz.
  - `:channels` - `1` (mono) or `2` (stereo).

  ## Returns

      %{
        streams_copied: 0,
        streams_reencoded: 2,
        packets_written: 312,
        duration_s: 2.04
      }
  """
  @spec transcode(input_source(), Path.t(), [transcode_opt()]) ::
          {:ok, transcode_stats()} | {:error, Error.t()}
  def transcode(input, output, opts \\ [])

  def transcode(input, output, opts) when is_binary(output) and is_list(opts) do
    with :ok <- validate_input(input, :input),
         :ok <- validate_non_empty_string(output, :output),
         :ok <- validate_options(opts, transcode_validators()) do
      native_call(Native.transcode(input, output, build_transcode_opts(opts)))
    end
  end

  def transcode(_input, _output, _opts) do
    {:error, Error.new(:invalid_request, "output must be a string and opts a keyword list")}
  end

  # The build_* functions pattern-match the NIF map shape strictly in
  # the function head. That makes the shape a static contract: Elixir's
  # typechecker proves any caller that passes a map it can't show fits
  # the head will not match. Exposed as `@doc false def` so the contract
  # tests in `nif_contract_test.exs` can exercise them without a real
  # NIF call.

  @doc false
  @spec build_media_info(map()) :: MediaInfo.t()
  def build_media_info(%{format: format, streams: raw_streams}) do
    %MediaInfo{
      format: build_format(format),
      streams: Enum.map(raw_streams, &build_stream/1)
    }
  end

  @doc false
  @spec build_format(map()) :: MediaInfo.format()
  def build_format(%{
        name: name,
        long_name: long_name,
        duration_s: duration_s,
        bit_rate: bit_rate,
        start_time_s: start_time_s,
        nb_streams: nb_streams,
        tags: tags
      }) do
    %{
      name: name,
      long_name: long_name,
      duration_s: duration_s,
      bit_rate: bit_rate,
      start_time_s: start_time_s,
      nb_streams: nb_streams,
      tags: Map.new(tags)
    }
  end

  @doc false
  @spec build_stream(map()) :: Stream.t()
  def build_stream(%{
        index: index,
        kind: kind,
        codec: codec,
        codec_long_name: codec_long_name,
        bit_rate: bit_rate,
        time_base: time_base,
        duration_s: duration_s,
        nb_frames: nb_frames,
        audio: audio,
        video: video
      }) do
    %Stream{
      index: index,
      kind: decode_kind(kind),
      codec: codec,
      codec_long_name: codec_long_name,
      bit_rate: bit_rate,
      time_base: time_base,
      duration_s: duration_s,
      nb_frames: nb_frames,
      audio: audio,
      video: video
    }
  end

  defp build_remux_opts(opts) do
    %{
      start_s: Keyword.get(opts, :start_s),
      duration_s: Keyword.get(opts, :duration_s),
      drop_audio: Keyword.get(opts, :drop_audio),
      drop_video: Keyword.get(opts, :drop_video),
      drop_subtitles: Keyword.get(opts, :drop_subtitles),
      tags: opts |> Keyword.get(:tags) |> normalize_tags(),
      progress: Keyword.get(opts, :progress)
    }
  end

  defp normalize_tags(nil), do: nil
  defp normalize_tags(tags) when is_map(tags), do: Enum.map(tags, fn {k, v} -> {to_string(k), to_string(v)} end)
  defp normalize_tags(tags) when is_list(tags), do: Enum.map(tags, fn {k, v} -> {to_string(k), to_string(v)} end)

  defp build_extract_frame_opts(opts) do
    %{
      timestamp_s: Keyword.get(opts, :timestamp_s),
      width: Keyword.get(opts, :width),
      height: Keyword.get(opts, :height)
    }
  end

  defp build_concat_opts(opts) do
    %{progress: Keyword.get(opts, :progress)}
  end

  defp build_extract_audio_opts(opts) do
    %{
      sample_rate: Keyword.get(opts, :sample_rate),
      channels: Keyword.get(opts, :channels),
      bitrate: Keyword.get(opts, :bitrate),
      progress: Keyword.get(opts, :progress)
    }
  end

  defp build_transcode_opts(opts) do
    %{
      video_codec: Keyword.get(opts, :video_codec),
      audio_codec: Keyword.get(opts, :audio_codec),
      video_bitrate: Keyword.get(opts, :video_bitrate),
      audio_bitrate: Keyword.get(opts, :audio_bitrate),
      width: Keyword.get(opts, :width),
      height: Keyword.get(opts, :height),
      fps: Keyword.get(opts, :fps),
      sample_rate: Keyword.get(opts, :sample_rate),
      channels: Keyword.get(opts, :channels),
      video_filter: Keyword.get(opts, :video_filter),
      drop_audio: Keyword.get(opts, :drop_audio),
      drop_video: Keyword.get(opts, :drop_video),
      drop_subtitles: Keyword.get(opts, :drop_subtitles),
      tags: opts |> Keyword.get(:tags) |> normalize_tags(),
      progress: Keyword.get(opts, :progress)
    }
  end

  defp native_call({:ok, _} = ok), do: ok
  defp native_call({:error, payload}), do: {:error, Error.from_native(payload)}

  @kinds ~w(video audio subtitle data attachment unknown)
  @kind_atoms Map.new(@kinds, fn k -> {k, String.to_atom(k)} end)

  defp decode_kind(kind) when kind in @kinds, do: Map.fetch!(@kind_atoms, kind)
  defp decode_kind(_), do: :unknown

  @spec validate_non_empty_string(String.t(), atom()) :: :ok | {:error, Error.t()}
  defp validate_non_empty_string(value, name) when is_binary(value) do
    if String.trim(value) == "" do
      {:error, Error.new(:invalid_request, "#{name} must be a non-empty string")}
    else
      :ok
    end
  end

  @spec validate_input(any(), atom()) :: :ok | {:error, Error.t()}
  defp validate_input(path, name) when is_binary(path), do: validate_non_empty_string(path, name)

  defp validate_input({:memory, bytes}, name) when is_binary(bytes) do
    if byte_size(bytes) == 0 do
      {:error, Error.new(:invalid_request, "#{name} {:memory, _} binary is empty")}
    else
      :ok
    end
  end

  defp validate_input(_other, name) do
    {:error,
     Error.new(
       :invalid_request,
       "#{name} must be a path string or {:memory, binary}"
     )}
  end

  defp remux_validators do
    %{
      start_s: &non_neg_number?/1,
      duration_s: &positive_number?/1,
      drop_audio: &is_boolean/1,
      drop_video: &is_boolean/1,
      drop_subtitles: &is_boolean/1,
      tags: &tags?/1,
      progress: &is_pid/1
    }
  end

  defp concat_validators do
    %{progress: &is_pid/1}
  end

  defp extract_frame_validators do
    %{
      timestamp_s: &non_neg_number?/1,
      width: &positive_integer?/1,
      height: &positive_integer?/1
    }
  end

  defp extract_audio_validators do
    %{
      sample_rate: &positive_integer?/1,
      channels: &channel_count?/1,
      bitrate: &positive_integer?/1,
      progress: &is_pid/1
    }
  end

  defp transcode_validators do
    %{
      video_codec: &non_empty_string?/1,
      audio_codec: &non_empty_string?/1,
      video_bitrate: &positive_integer?/1,
      audio_bitrate: &positive_integer?/1,
      width: &positive_integer?/1,
      height: &positive_integer?/1,
      fps: &fps_tuple?/1,
      sample_rate: &positive_integer?/1,
      channels: &channel_count?/1,
      video_filter: &non_empty_string?/1,
      drop_audio: &is_boolean/1,
      drop_video: &is_boolean/1,
      drop_subtitles: &is_boolean/1,
      tags: &tags?/1,
      progress: &is_pid/1
    }
  end

  defp non_empty_string?(v), do: is_binary(v) and String.trim(v) != ""

  defp fps_tuple?({num, den}), do: is_integer(num) and is_integer(den) and num > 0 and den > 0
  defp fps_tuple?(_), do: false

  defp tags?(tags) when is_map(tags), do: Enum.all?(tags, &valid_tag_pair?/1)
  defp tags?(tags) when is_list(tags), do: Enum.all?(tags, &valid_tag_pair?/1)
  defp tags?(_), do: false

  defp valid_tag_pair?({k, v}), do: is_binary(k) and is_binary(v)
  defp valid_tag_pair?(_), do: false

  @spec validate_concat_inputs([input_source()]) :: :ok | {:error, Error.t()}
  defp validate_concat_inputs([]) do
    {:error, Error.new(:invalid_request, "inputs list must not be empty")}
  end

  defp validate_concat_inputs(inputs) do
    Enum.reduce_while(inputs, :ok, fn input, :ok ->
      case validate_input(input, :input) do
        :ok -> {:cont, :ok}
        err -> {:halt, err}
      end
    end)
  end

  defp positive_integer?(v), do: is_integer(v) and v > 0
  defp channel_count?(v), do: is_integer(v) and v in 1..2

  @spec validate_options(keyword(), map()) :: :ok | {:error, Error.t()}
  defp validate_options(opts, validators) do
    Enum.reduce_while(opts, :ok, fn pair, :ok -> check_option(pair, validators) end)
  end

  defp check_option({key, value}, validators) do
    case Map.fetch(validators, key) do
      :error ->
        {:halt, {:error, Error.new(:invalid_request, "unknown option #{inspect(key)}")}}

      {:ok, validator} ->
        if validator.(value) do
          {:cont, :ok}
        else
          {:halt,
           {:error,
            Error.new(
              :invalid_request,
              "invalid value for option #{inspect(key)}: #{inspect(value)}"
            )}}
        end
    end
  end

  defp number?(v), do: is_integer(v) or is_float(v)
  defp positive_number?(v), do: number?(v) and v > 0
  defp non_neg_number?(v), do: number?(v) and v >= 0
end
