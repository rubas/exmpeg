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
    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    assert format.duration_s < 1.5

    # Each kept stream ends on its own at the window edge; the audio is
    # not truncated early because the video happened to reach the end
    # first. Both land near the requested 1 s.
    [video] = Enum.filter(streams, &(&1.kind == :video))
    [audio] = Enum.filter(streams, &(&1.kind == :audio))
    assert_in_delta video.duration_s, 1.0, 0.4
    assert_in_delta audio.duration_s, 1.0, 0.4
    assert_in_delta video.duration_s, audio.duration_s, 0.3
  end

  test "remux with duration_s cuts correctly when a stream ends before the window" do
    # The audio track (1 s) ends well before the 2 s cut window, so it
    # never crosses the boundary. The loop must still terminate and keep
    # the full audio plus the cut video, rather than dropping audio or
    # hanging on a never-completing stream.
    src = Path.join(System.tmp_dir!(), "exmpeg_shortaud_#{System.unique_integer([:positive])}.mp4")
    out = Path.join(System.tmp_dir!(), "exmpeg_shortaud_out_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=64x48:r=10:d=4) ++
          ~w(-f lavfi -i sine=frequency=440:duration=1 -c:v libx264 -c:a aac #{src}),
        env: %{}
      )

    assert {:ok, _stats} = Exmpeg.remux(src, out, duration_s: 2.0)

    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    # Video is cut to ~2 s; the short audio is kept whole (~1 s).
    assert_in_delta format.duration_s, 2.0, 0.4
    [video] = Enum.filter(streams, &(&1.kind == :video))
    [audio] = Enum.filter(streams, &(&1.kind == :audio))
    assert_in_delta video.duration_s, 2.0, 0.4
    assert_in_delta audio.duration_s, 1.0, 0.4
  end

  test "remux to an unknown output extension returns :unsupported", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_bad_#{System.unique_integer([:positive])}.xyz")
    on_exit(fn -> File.rm(out) end)

    assert {:error, %Exmpeg.Error{reason: :unsupported}} = Exmpeg.remux(clip, out)
  end

  test "remux of a video into a codec-incompatible container returns :unsupported", %{clip: clip} do
    # .wav has a muxer, but it cannot hold an h264 video stream; the
    # failure surfaces from write_header as :unsupported, not :io_error.
    out = Path.join(System.tmp_dir!(), "exmpeg_badcodec_#{System.unique_integer([:positive])}.wav")
    on_exit(fn -> File.rm(out) end)

    assert {:error, %Exmpeg.Error{reason: :unsupported}} = Exmpeg.remux(clip, out)
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

  test "extract_audio passes a PCM source through unchanged when no resample is needed" do
    # A pcm_s16le source extracted to .wav with no rate/channel override
    # needs no resampling and the pcm encoder takes arbitrary chunk sizes,
    # so the fast path skips the resampler and FIFO. The output must still
    # be a correct, complete file.
    src = Path.join(System.tmp_dir!(), "exmpeg_pcm_#{System.unique_integer([:positive])}.wav")
    out = Path.join(System.tmp_dir!(), "exmpeg_pcm_out_#{System.unique_integer([:positive])}.wav")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i sine=frequency=440:duration=2:sample_rate=44100) ++
          ~w(-ac 2 -c:a pcm_s16le #{src}),
        env: %{}
      )

    assert {:ok, stats} = Exmpeg.extract_audio(src, out)
    assert stats.codec == "pcm_s16le"
    assert stats.sample_rate == 44_100
    assert stats.channels == 2
    assert_in_delta stats.duration_s, 2.0, 0.1

    assert {:ok, %MediaInfo{format: format, streams: [audio]}} = Exmpeg.probe(out)
    assert_in_delta format.duration_s, 2.0, 0.1
    assert %Stream{codec: "pcm_s16le", audio: %{sample_rate: 44_100, channels: 2}} = audio
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

  test "concat of duration-less inputs reports and writes the summed duration" do
    # A webm muxed to a pipe (a non-seekable sink, like MediaRecorder or
    # an interrupted capture) carries no container duration. The concat
    # offset is then advanced from the tracked per-stream end instead of
    # the (unknown) container duration; without that the second input
    # overlapped the first, duration_s came back 0.0, and every packet of
    # the second half was retimed to a single filled gap.
    a = make_pipe_webm(2)
    b = make_pipe_webm(2)
    out = Path.join(System.tmp_dir!(), "exmpeg_vfrcat_#{System.unique_integer([:positive])}.webm")
    on_exit(fn -> Enum.each([a, b, out], &File.rm/1) end)

    # Confirm the fixtures really lack a container duration, so the test
    # exercises the unknown-duration path rather than the known one.
    assert {:ok, %MediaInfo{format: %{duration_s: nil}}} = Exmpeg.probe(a)

    assert {:ok, stats} = Exmpeg.concat([a, b], out)
    assert stats.inputs_joined == 2
    assert_in_delta stats.duration_s, 4.0, 0.3

    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    assert_in_delta format.duration_s, 4.0, 0.3

    # Both streams survive the two-input join: the unknown-duration path
    # advances every stream to one shared segment end, so the multi-stream
    # boundary is handled (a webm container carries no per-stream duration
    # to assert finer A/V alignment from).
    assert Enum.any?(streams, &(&1.kind == :video))
    assert Enum.any?(streams, &(&1.kind == :audio))
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

  test "transcode normalises a nonzero source start to a zero origin" do
    # A source whose streams start at ~1.4 s (MPEG-TS capture / edit-list
    # offset). With a copy + re-encode mix the copied stream kept its 1.4 s
    # offset while the re-encoded stream started at 0, a constant A/V
    # desync. Normalising the output to a zero origin collapses the bogus
    # offset: the ~2 s of media spans ~0..2 s instead of ~0..3.4 s.
    src = Path.join(System.tmp_dir!(), "exmpeg_offset_#{System.unique_integer([:positive])}.ts")
    out = Path.join(System.tmp_dir!(), "exmpeg_offset_out_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=160x120:r=10:d=2 -f lavfi -i sine=frequency=440:duration=2) ++
          ~w(-c:v libx264 -c:a aac -output_ts_offset 1.4 -muxpreload 0 -muxdelay 0 #{src}),
        env: %{}
      )

    # Sanity: the source really starts well after zero.
    assert {:ok, %MediaInfo{format: %{start_time_s: src_start}}} = Exmpeg.probe(src)
    assert src_start > 1.0

    assert {:ok, _} = Exmpeg.transcode(src, out, video_codec: "copy", audio_codec: "aac")

    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    # The output starts at zero and spans only the ~2 s of media; the
    # pre-fix desync would push the container duration past 3 s.
    assert format.duration_s < 2.5
    assert format.start_time_s == nil or format.start_time_s < 0.1
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

    assert {:ok, %MediaInfo{format: format, streams: streams}} = Exmpeg.probe(out)
    [video] = Enum.filter(streams, &(&1.kind == :video))
    assert video.video.width == 80
    # The crop removed 20 lines; scale preserved aspect → height < 60.
    assert video.video.height < 60

    # A custom :video_filter chain with no fps filter keeps the input
    # stream time_base on the buffersink. Stepping pts by a bare 1 there
    # collapses the output to a few microseconds; stepping by one frame
    # interval keeps the real ~2 s duration.
    assert format.duration_s > 1.5 and format.duration_s < 2.5
  end

  test "transcode :video_filter ignores an overridden :fps for pts timing", %{clip: clip} do
    # `:video_filter` overrides `:fps`, so the pts step must come from the
    # source cadence, not the ignored `:fps`. With the bug, a high `:fps`
    # stamped frames too close together and compressed the duration.
    out = Path.join(System.tmp_dir!(), "exmpeg_xc6_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, _} =
             Exmpeg.transcode(clip, out,
               video_codec: "libx264",
               video_filter: "crop=iw:ih-20:0:10",
               fps: {120, 1}
             )

    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    assert format.duration_s > 1.5 and format.duration_s < 2.5
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

    on_exit(fn ->
      File.rm(out)
      out |> partials_for() |> Enum.each(&File.rm/1)
    end)

    assert {:error, %Exmpeg.Error{reason: :invalid_request, message: msg}} =
             Exmpeg.extract_frame(audio, out)

    assert msg =~ "no video stream"
    refute File.exists?(out)
    assert partials_for(out) == []
  end

  test "concurrent writes to the same output resolve to a complete file", %{clip: clip} do
    # Each call writes to a unique `<stem>.partial.<nonce>.<ext>`, so
    # racing writes to one destination never share a partial. Both
    # complete, the renames are atomic, and the destination is always a
    # whole file (last-complete-rename-wins) - never a half-written mix.
    out = Path.join(System.tmp_dir!(), "exmpeg_race_#{System.unique_integer([:positive])}.mp4")

    on_exit(fn ->
      File.rm(out)
      out |> partials_for() |> Enum.each(&File.rm/1)
    end)

    results =
      1..3
      |> Enum.map(fn _ ->
        Task.async(fn ->
          Exmpeg.transcode(clip, out, video_codec: "libx264", audio_codec: "aac", width: 80)
        end)
      end)
      |> Task.await_many(60_000)

    assert Enum.all?(results, &match?({:ok, _}, &1))

    # The destination probes as a complete, valid file and no partial
    # sibling is left behind.
    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    assert Enum.any?(streams, &(&1.kind == :video and &1.codec == "h264"))
    assert partials_for(out) == []
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

  test "extract_audio to opus/ogg at a non-48 kHz rate keeps the right duration", %{clip: clip} do
    # The Ogg muxer pins Opus streams to a 1/48000 stream time_base
    # regardless of the encoder's 1/sample_rate. Without rescaling each
    # packet into the muxer's time_base, a 16 kHz extraction reports its
    # container duration 3x short (~0.68 s for a 2 s source) while still
    # returning {:ok, _}. The source clip is ~2 s.
    for ext <- ["opus", "ogg"] do
      out = Path.join(System.tmp_dir!(), "exmpeg_audio16_#{System.unique_integer([:positive])}.#{ext}")
      on_exit(fn -> File.rm(out) end)

      assert {:ok, _stats} = Exmpeg.extract_audio(clip, out, sample_rate: 16_000)
      assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
      assert_in_delta format.duration_s, 2.0, 0.2
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

    last = List.last(msgs)
    assert last.op == "remux"
    # The closing tick reports the real end position, not 0.0, so a
    # subscriber rendering current_pts_s / total_duration_s sees ~100% at
    # completion. The clip is ~2 s.
    assert last.current_pts_s > 1.5
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

  # A `seconds`-long VP8 webm muxed to a pipe. Writing to a non-seekable
  # sink leaves the container duration unset, which is the case this
  # exercises (MediaRecorder / interrupted captures behave the same way).
  defp make_pipe_webm(seconds) do
    path = Path.join(System.tmp_dir!(), "exmpeg_pipe_#{System.unique_integer([:positive])}.webm")

    # Both a video and an audio stream, so the concat boundary exercises
    # advancing every stream to one shared segment end.
    {bytes, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -f lavfi -i testsrc2=s=128x96:r=10:d=#{seconds}) ++
          ~w(-f lavfi -i sine=frequency=440:duration=#{seconds}) ++
          ~w(-c:v libvpx -c:a libvorbis -f webm pipe:1),
        env: %{}
      )

    File.write!(path, bytes)
    path
  end

  defp drain_progress(acc) do
    receive do
      {:exmpeg_progress, msg} -> drain_progress([msg | acc])
    after
      50 -> Enum.reverse(acc)
    end
  end

  # All `<stem>.partial*` siblings of an output path. The partial name
  # carries a per-call nonce, so glob the infix rather than predict it.
  defp partials_for(out) do
    root = Path.rootname(out)
    Path.wildcard(root <> ".partial*")
  end
end
