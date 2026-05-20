defmodule Exmpeg.NifContractTest do
  @moduledoc """
  Tests for the build_* helpers - pin the atom keys and shape the NIF
  emits without requiring a real ffmpeg call.

  These tests fail loudly if a refactor accidentally drops a field from
  the NIF payload, which used to silently turn into `nil` everywhere.
  """

  use ExUnit.Case, async: true

  alias Exmpeg.{MediaInfo, Stream}

  describe "build_stream/1" do
    test "maps a complete video stream payload" do
      payload = %{
        index: 0,
        kind: "video",
        codec: "h264",
        codec_long_name: "H.264 / AVC",
        bit_rate: 1_500_000,
        time_base: {1, 90_000},
        duration_s: 12.5,
        nb_frames: 125,
        audio: nil,
        video: %{width: 1920, height: 1080, pixel_format: "yuv420p", frame_rate: {25, 1}}
      }

      stream = Exmpeg.build_stream(payload)

      assert stream == %Stream{
               index: 0,
               kind: :video,
               codec: "h264",
               codec_long_name: "H.264 / AVC",
               bit_rate: 1_500_000,
               time_base: {1, 90_000},
               duration_s: 12.5,
               nb_frames: 125,
               audio: nil,
               video: %{
                 width: 1920,
                 height: 1080,
                 pixel_format: "yuv420p",
                 frame_rate: {25, 1}
               }
             }
    end

    test "maps an audio stream payload" do
      payload = %{
        index: 1,
        kind: "audio",
        codec: "aac",
        codec_long_name: "AAC (Advanced Audio Coding)",
        bit_rate: 128_000,
        time_base: {1, 44_100},
        duration_s: 12.5,
        nb_frames: nil,
        audio: %{sample_rate: 44_100, channels: 2, sample_format: "fltp"},
        video: nil
      }

      stream = Exmpeg.build_stream(payload)
      assert stream.kind == :audio
      assert stream.audio.sample_rate == 44_100
      assert stream.video == nil
    end

    test "decode_kind/1 falls back to :unknown for unrecognised strings" do
      payload = %{
        index: 9,
        kind: "freshly_invented",
        codec: "?",
        codec_long_name: nil,
        bit_rate: 0,
        time_base: {0, 1},
        duration_s: nil,
        nb_frames: nil,
        audio: nil,
        video: nil
      }

      assert Exmpeg.build_stream(payload).kind == :unknown
    end
  end

  describe "build_format/1" do
    test "tags get normalised into a map" do
      format =
        Exmpeg.build_format(%{
          name: "mp4",
          long_name: "MPEG-4",
          duration_s: 12.5,
          bit_rate: 1_600_000,
          start_time_s: 0.0,
          nb_streams: 2,
          tags: [{"title", "demo"}, {"encoder", "libx264"}]
        })

      assert format.tags == %{"title" => "demo", "encoder" => "libx264"}
    end
  end

  describe "build_media_info/1" do
    test "assembles the top-level struct" do
      payload = %{
        format: %{
          name: "mp4",
          long_name: "MPEG-4",
          duration_s: 12.5,
          bit_rate: 1_600_000,
          start_time_s: 0.0,
          nb_streams: 0,
          tags: []
        },
        streams: []
      }

      assert %MediaInfo{streams: [], format: %{name: "mp4"}} =
               Exmpeg.build_media_info(payload)
    end
  end
end
