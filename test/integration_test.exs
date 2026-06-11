defmodule Exmpeg.IntegrationTest do
  @moduledoc """
  End-to-end tests for Exmpeg's public media operations against
  synthetic clips generated on demand via `ffmpeg -f lavfi`.

  Covers successful probe, remux, frame extraction, audio extraction,
  concat, transcode, memory input, progress, metadata, stream-drop, and
  output-cleanup contracts. It does not compare encoded bytes against
  the `ffmpeg` CLI; assertions stay at the public API and container
  metadata boundary.

  Excluded from the default `mix test` run because they require an
  `ffmpeg` binary on `PATH` for fixture generation. Run with:

      mix test --include integration
  """

  use ExUnit.Case, async: false

  alias Exmpeg.{MediaInfo, Stream, TestFixtures}

  @moduletag :integration
  @moduletag timeout: 120_000

  setup_all do
    if System.find_executable("ffmpeg") == nil do
      {:skip, "ffmpeg binary not on PATH; cannot generate fixtures"}
    else
      {:ok, clip: TestFixtures.ensure_av_clip!()}
    end
  end

  test "probes a 2 s mp4 and reports one video + one audio stream", %{clip: clip} do
    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(clip)

    assert format.nb_streams == 2
    assert format.duration_s > 1.5
    assert format.duration_s < 3.0
    assert format.name =~ "mp4"

    [video] = Enum.filter(streams, &(&1.kind == :video))
    [audio] = Enum.filter(streams, &(&1.kind == :audio))

    assert %Stream{codec: "h264", video: %{width: 160, height: 120}} = video
    assert %Stream{codec: "aac", audio: %{sample_rate: 44_100}} = audio
  end

  test "remuxes mp4 -> mkv and the result probes the same streams", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_remux_#{System.unique_integer([:positive])}.mkv")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.remux(clip, out)
    assert stats.streams_copied == 2
    assert stats.packets_written > 0

    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    assert format.name =~ "matroska"
    assert Enum.any?(streams, &(&1.kind == :video and &1.codec == "h264"))
    assert Enum.any?(streams, &(&1.kind == :audio and &1.codec == "aac"))
  end

  test "remux with duration_s drops packets past the window", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_cut_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.remux(clip, out, start_s: 0.0, duration_s: 1.0)
    assert stats.packets_written > 0
    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    assert format.duration_s < 1.5
  end

  test "extract_frame at a timestamp writes a jpeg of the requested size", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_frame_#{System.unique_integer([:positive])}.jpg")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.extract_frame(clip, out, timestamp_s: 1.0, width: 80)
    assert stats.codec == "mjpeg"
    assert stats.width == 80
    assert stats.height == 60
    assert stats.pts_known == true
    assert stats.timestamp_s >= 1.0
    assert File.stat!(out).size > 0
    # Minimal sanity: starts with the JPEG SOI marker.
    assert <<0xFF, 0xD8, _::binary>> = File.read!(out)
  end

  test "extract_frame writes a png with default dimensions", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_frame_#{System.unique_integer([:positive])}.png")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.extract_frame(clip, out)
    assert stats.codec == "png"
    assert stats.width == 160 and stats.height == 120
    assert <<0x89, "PNG", _::binary>> = File.read!(out)
  end

  test "extract_frame writes bmp and webp outputs", %{clip: clip} do
    for {ext, expected_codecs, magic} <- [
          {"bmp", ["bmp"], "BM"},
          {"webp", ["webp", "libwebp", "libwebp_anim"], "RIFF"}
        ] do
      out = Path.join(System.tmp_dir!(), "exmpeg_frame_#{System.unique_integer([:positive])}.#{ext}")
      on_exit(fn -> File.rm(out) end)

      assert {:ok, stats} = Exmpeg.extract_frame(clip, out, width: 80)
      assert stats.codec in expected_codecs
      assert stats.width == 80
      assert binary_part(File.read!(out), 0, byte_size(magic)) == magic
    end
  end

  test "extract_audio writes a WAV with the requested rate and channel count", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_audio_#{System.unique_integer([:positive])}.wav")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.extract_audio(clip, out, sample_rate: 16_000, channels: 1)
    assert stats.codec == "pcm_s16le"
    assert stats.sample_rate == 16_000
    assert stats.channels == 1
    assert stats.duration_s > 1.5 and stats.duration_s < 2.5

    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    [audio] = streams
    assert %Stream{codec: "pcm_s16le", audio: %{sample_rate: 16_000, channels: 1}} = audio
  end

  test "concat joins three copies into a 6 s output", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_concat_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.concat([clip, clip, clip], out)
    assert stats.inputs_joined == 3
    assert stats.streams_copied == 2

    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    assert format.duration_s > 5.5 and format.duration_s < 6.5
  end

  test "transcode re-encodes both streams with libx264 + aac", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} =
             Exmpeg.transcode(clip, out,
               video_codec: "libx264",
               audio_codec: "aac",
               width: 80,
               sample_rate: 22_050,
               channels: 1
             )

    assert stats.streams_reencoded == 2
    assert stats.streams_copied == 0

    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    [video] = Enum.filter(streams, &(&1.kind == :video))
    [audio] = Enum.filter(streams, &(&1.kind == :audio))
    assert %Stream{codec: "h264", video: %{width: 80}} = video
    assert %Stream{codec: "aac", audio: %{sample_rate: 22_050, channels: 1}} = audio
  end

  test "transcode copies the video and re-encodes only audio", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc2_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} =
             Exmpeg.transcode(clip, out, video_codec: "copy", audio_codec: "aac", sample_rate: 22_050)

    assert stats.streams_copied >= 1
    assert stats.streams_reencoded >= 1

    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    assert Enum.any?(streams, &(&1.kind == :video and &1.codec == "h264"))
    assert Enum.any?(streams, &(&1.kind == :audio and &1.codec == "aac"))
  end

  test "transcode copies audio while re-encoding video", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc3_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} =
             Exmpeg.transcode(clip, out, video_codec: "libx264", audio_codec: "copy", width: 80)

    assert stats.streams_copied >= 1
    assert stats.streams_reencoded >= 1

    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    assert Enum.any?(streams, &(&1.kind == :video and &1.video.width == 80))
    assert Enum.any?(streams, &(&1.kind == :audio and &1.codec == "aac"))
  end

  test "transcode of a surround source requires an explicit :channels" do
    # 5.1 source. Re-encoding the audio without :channels would silently
    # downmix to stereo; instead it must return :invalid_request, matching
    # extract_audio. An explicit value transcodes fine.
    src = Path.join(System.tmp_dir!(), "exmpeg_surround_#{System.unique_integer([:positive])}.mp4")
    out = Path.join(System.tmp_dir!(), "exmpeg_surround_out_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=64x48:d=1) ++
          ["-f", "lavfi", "-i", "aevalsrc=0|0|0|0|0|0:c=5.1:d=1"] ++
          ~w(-c:v libx264 -c:a aac -shortest #{src}),
        env: %{}
      )

    assert {:error, %Exmpeg.Error{reason: :invalid_request, message: msg}} =
             Exmpeg.transcode(src, out, audio_codec: "aac")

    assert msg =~ "channels"

    assert {:ok, stats} = Exmpeg.transcode(src, out, audio_codec: "aac", channels: 2)
    assert stats.streams_reencoded >= 1
  end

  test "transcode mp4 -> webm with vp9 + opus", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc4_#{System.unique_integer([:positive])}.webm")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} =
             Exmpeg.transcode(clip, out,
               video_codec: "libvpx-vp9",
               audio_codec: "libopus",
               video_bitrate: 200_000,
               sample_rate: 48_000
             )

    assert stats.streams_reencoded == 2

    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    assert format.name =~ "webm" or format.name =~ "matroska"
    assert Enum.any?(streams, &(&1.kind == :video and &1.codec == "vp9"))
    assert Enum.any?(streams, &(&1.kind == :audio and &1.codec == "opus"))
  end

  test "transcode honors :video_filter for crop + scale", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc5_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, _} =
             Exmpeg.transcode(clip, out,
               video_codec: "libx264",
               audio_codec: "aac",
               video_filter: "crop=iw:ih-20:0:10,scale=80:-2"
             )

    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    [video] = Enum.filter(streams, &(&1.kind == :video))
    assert video.video.width == 80
    # The crop removed 20 lines; scale preserved aspect → height < 60.
    assert video.video.height < 60
  end

  test "transcode drop options and metadata tags are reflected in the output", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc_tags_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} =
             Exmpeg.transcode(clip, out,
               video_codec: "libx264",
               drop_audio: true,
               tags: [{"title", "video-only-transcode"}]
             )

    assert stats.streams_reencoded >= 1

    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    assert format.tags["title"] == "video-only-transcode"
    assert Enum.any?(streams, &(&1.kind == :video))
    assert Enum.all?(streams, &(&1.kind != :audio))
  end

  test "transcode rejects unknown encoder name", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_xc_bad_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:error, %Exmpeg.Error{reason: :unsupported}} =
             Exmpeg.transcode(clip, out, video_codec: "definitely_not_a_codec")
  end

  test "remux drop_audio produces a video-only file", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_drop_a_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, _} = Exmpeg.remux(clip, out, drop_audio: true)
    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    assert Enum.all?(streams, &(&1.kind != :audio))
    assert Enum.any?(streams, &(&1.kind == :video))
  end

  test "remux drop_video produces an audio-only file", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_drop_v_#{System.unique_integer([:positive])}.m4a")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, _} = Exmpeg.remux(clip, out, drop_video: true)
    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    assert Enum.all?(streams, &(&1.kind != :video))
    assert Enum.any?(streams, &(&1.kind == :audio))
  end

  test "remux dropping every kind returns :invalid_request", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_drop_all_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:error, %Exmpeg.Error{reason: :invalid_request}} =
             Exmpeg.remux(clip, out, drop_audio: true, drop_video: true, drop_subtitles: true)
  end

  test "remux writes container metadata tags", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_tags_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, _} =
             Exmpeg.remux(clip, out, tags: %{"title" => "tagged-clip", "comment" => "exmpeg test"})

    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    assert format.tags["title"] == "tagged-clip"
    assert format.tags["comment"] == "exmpeg test"
  end

  test "extract_frame rejects an audio-only input" do
    audio = TestFixtures.ensure_audio_only_clip!()
    out = Path.join(System.tmp_dir!(), "exmpeg_noframe_#{System.unique_integer([:positive])}.jpg")
    partial = partial_output_path(out)

    on_exit(fn ->
      File.rm(out)
      File.rm(partial)
    end)

    assert {:error, %Exmpeg.Error{reason: :invalid_request, message: msg}} =
             Exmpeg.extract_frame(audio, out)

    assert msg =~ "no video stream"
    refute File.exists?(out)
    refute File.exists?(partial)
  end

  test "extract_audio rejects a video-only input" do
    video = TestFixtures.ensure_video_only_clip!()
    out = Path.join(System.tmp_dir!(), "exmpeg_noaudio_#{System.unique_integer([:positive])}.wav")
    on_exit(fn -> File.rm(out) end)

    assert {:error, %Exmpeg.Error{reason: :invalid_request, message: msg}} =
             Exmpeg.extract_audio(video, out)

    assert msg =~ "no audio stream"
  end

  test "extract_audio writes mp3 and reports the encoder name", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_mp3_#{System.unique_integer([:positive])}.mp3")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.extract_audio(clip, out, sample_rate: 22_050, channels: 1)
    assert stats.codec == "libmp3lame"
    assert File.stat!(out).size > 0
  end

  test "extract_audio writes flac (lossless)", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_flac_#{System.unique_integer([:positive])}.flac")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} = Exmpeg.extract_audio(clip, out)
    assert stats.codec == "flac"
    assert {:ok, %MediaInfo{streams: [audio]}} = Exmpeg.probe(out)
    assert audio.codec == "flac"
  end

  test "extract_audio writes m4a and opus outputs", %{clip: clip} do
    for {ext, reported_codec, probed_codec} <- [
          {"m4a", "aac", "aac"},
          {"opus", "libopus", "opus"}
        ] do
      out = Path.join(System.tmp_dir!(), "exmpeg_audio_#{System.unique_integer([:positive])}.#{ext}")
      on_exit(fn -> File.rm(out) end)

      assert {:ok, stats} = Exmpeg.extract_audio(clip, out, sample_rate: 48_000, channels: 1)
      assert stats.codec == reported_codec
      assert {:ok, %MediaInfo{streams: [audio]}} = Exmpeg.probe(out)
      assert audio.codec == probed_codec
      assert audio.audio.channels == 1
    end
  end

  test "concat rejects inputs with mismatched stream layouts", %{clip: clip} do
    video_only = TestFixtures.ensure_video_only_clip!()
    out = Path.join(System.tmp_dir!(), "exmpeg_concat_bad_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:error, %Exmpeg.Error{reason: :invalid_request, message: msg}} =
             Exmpeg.concat([clip, video_only], out)

    assert msg =~ "stream"
  end

  test "probe accepts {:memory, binary} input", %{clip: clip} do
    bytes = File.read!(clip)
    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe({:memory, bytes})
    assert format.nb_streams == 2
    assert Enum.any?(streams, &(&1.kind == :video))
    assert Enum.any?(streams, &(&1.kind == :audio))
  end

  test "probe rejects an empty memory input" do
    assert {:error, %Exmpeg.Error{reason: :invalid_request}} = Exmpeg.probe({:memory, <<>>})
  end

  test "extract_frame works from a {:memory, binary} source", %{clip: clip} do
    bytes = File.read!(clip)
    out = Path.join(System.tmp_dir!(), "exmpeg_memframe_#{System.unique_integer([:positive])}.jpg")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, stats} =
             Exmpeg.extract_frame({:memory, bytes}, out, timestamp_s: 1.0, width: 80)

    assert stats.width == 80
    assert <<0xFF, 0xD8, _::binary>> = File.read!(out)
  end

  test "transcode from memory + progress messages", %{clip: clip} do
    bytes = File.read!(clip)
    out = Path.join(System.tmp_dir!(), "exmpeg_memxc_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    parent = self()

    task =
      Task.async(fn ->
        Exmpeg.transcode({:memory, bytes}, out,
          video_codec: "libx264",
          audio_codec: "aac",
          width: 80,
          progress: parent
        )
      end)

    {:ok, _stats} = Task.await(task, 120_000)

    # Drain whatever progress messages arrived before / during the call.
    msgs = drain_progress([])
    assert msgs != [], "expected at least one progress message"
    last = List.last(msgs)
    assert last.op == "transcode"
    assert last.total_duration_s > 1.5
    # The closing tick is always sent and reports the full duration.
    assert last.current_pts_s >= last.total_duration_s - 0.5
  end

  test "remux emits progress messages", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_remux_progress_#{System.unique_integer([:positive])}.mkv")
    on_exit(fn -> File.rm(out) end)

    parent = self()

    task =
      Task.async(fn ->
        Exmpeg.remux(clip, out, progress: parent)
      end)

    {:ok, _stats} = Task.await(task, 60_000)

    msgs = drain_progress([])
    assert msgs != [], "expected at least one progress message"
    assert List.last(msgs).op == "remux"
  end

  test "extract_audio emits progress messages", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_audio_progress_#{System.unique_integer([:positive])}.wav")
    on_exit(fn -> File.rm(out) end)

    parent = self()

    task =
      Task.async(fn ->
        Exmpeg.extract_audio(clip, out, progress: parent)
      end)

    {:ok, _stats} = Task.await(task, 60_000)

    msgs = drain_progress([])
    assert msgs != [], "expected at least one progress message"
    last = List.last(msgs)
    assert last.op == "extract_audio"
    assert last.total_duration_s > 1.5
  end

  test "concat accepts memory inputs and emits progress", %{clip: clip} do
    bytes = File.read!(clip)
    out = Path.join(System.tmp_dir!(), "exmpeg_memcat_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    parent = self()

    task =
      Task.async(fn ->
        Exmpeg.concat([{:memory, bytes}, {:memory, bytes}], out, progress: parent)
      end)

    {:ok, stats} = Task.await(task, 60_000)
    assert stats.inputs_joined == 2

    msgs = drain_progress([])
    assert msgs != []
    assert List.last(msgs).op == "concat"
  end

  defp drain_progress(acc) do
    receive do
      {:exmpeg_progress, msg} -> drain_progress([msg | acc])
    after
      50 -> Enum.reverse(acc)
    end
  end

  defp partial_output_path(path) do
    root = Path.rootname(path)
    ext = Path.extname(path)

    if ext == "" do
      root <> ".partial"
    else
      root <> ".partial" <> ext
    end
  end
end
