defmodule Exmpeg.IntegrationTest do
  @moduledoc """
  End-to-end tests for Exmpeg's public media operations against
  synthetic clips generated on demand via `ffmpeg -f lavfi`.

  Covers successful probe, remux, frame extraction, audio extraction,
  concat, transcode, memory input, progress, metadata, stream-drop, and
  output-cleanup contracts. It does not compare encoded bytes against
  the `ffmpeg` CLI; assertions stay at the public API and container
  metadata boundary.

  Excluded from the default `mix test` run because they require the
  `ffmpeg` and `ffprobe` binaries on `PATH` (fixture generation and
  packet-timing assertions). Run with:

      mix test --include integration
  """

  use ExUnit.Case, async: false

  alias Exmpeg.{MediaInfo, Stream, TestFixtures}

  @moduletag :integration
  @moduletag timeout: 120_000

  setup_all do
    cond do
      System.find_executable("ffmpeg") == nil ->
        {:skip, "ffmpeg binary not on PATH; cannot generate fixtures"}

      System.find_executable("ffprobe") == nil ->
        {:skip, "ffprobe binary not on PATH; required for packet-timing assertions"}

      true ->
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

  test "remux with duration_s keeps the in-window B-frames and their references like ffmpeg -t -c copy" do
    # Video decode order is pts 0.0 0.3 0.1 0.2 0.6 0.4 0.5 0.9 0.7 0.8.
    # A cut at 0.8 s that ended the video on the first out-of-window pts
    # lost the 0.7 frame (video only), or kept 0.7 without the 0.9
    # P-frame it references (with audio still in the window).
    src = Path.join(System.tmp_dir!(), "exmpeg_bframes_#{System.unique_integer([:positive])}.mp4")
    out = Path.join(System.tmp_dir!(), "exmpeg_bframes_out_#{System.unique_integer([:positive])}.mp4")
    cli = Path.join(System.tmp_dir!(), "exmpeg_bframes_cli_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out, cli], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=64x48:r=10:d=2 -f lavfi -i sine=frequency=440:duration=2) ++
          ~w(-c:v libx264 -x264-params bframes=2:b-adapt=0:keyint=30:scenecut=0 -c:a aac #{src}),
        env: %{}
      )

    {_, 0} = System.cmd("ffmpeg", ~w(-v error -y -i #{src} -t 0.8 -c copy #{cli}), env: %{})
    expected = video_packet_pts_times(cli)
    in_window = src |> video_packet_pts_times() |> Enum.filter(&(&1 < 0.8))
    assert in_window -- expected == []

    for opts <- [[duration_s: 0.8], [duration_s: 0.8, drop_audio: true]] do
      assert {:ok, _stats} = Exmpeg.remux(src, out, opts)
      assert video_packet_pts_times(out) == expected
    end
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

  test "extract_frame keeps an anamorphic source's display aspect ratio" do
    src = make_anamorphic_clip()
    jpg = Path.join(System.tmp_dir!(), "exmpeg_sar_frame_#{System.unique_integer([:positive])}.jpg")
    png = Path.join(System.tmp_dir!(), "exmpeg_sar_frame_#{System.unique_integer([:positive])}.png")
    on_exit(fn -> Enum.each([src, jpg, png], &File.rm/1) end)

    assert {:ok, %{width: 720, height: 576}} = Exmpeg.extract_frame(src, jpg)
    assert video_aspect(jpg) == {"720x576", "64:45", "16:9"}

    # A non-proportional resize folds the pixel shape change into the SAR.
    assert {:ok, %{width: 360, height: 360}} = Exmpeg.extract_frame(src, png, width: 360, height: 360)
    assert video_aspect(png) == {"360x360", "16:9", "16:9"}
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

    assert {:ok, stats} = Exmpeg.extract_audio(src, out, progress: self())
    assert stats.codec == "pcm_s16le"
    assert stats.sample_rate == 44_100
    assert stats.channels == 2
    assert_in_delta stats.duration_s, 2.0, 0.1

    # The WAV demuxer re-chunks on read, so ffprobe cannot count the muxed
    # packets. A packet count sits between zero and the sample count.
    last = [] |> drain_progress() |> List.last()
    assert last.packets_written > 0
    assert last.packets_written < stats.samples_written

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
    # boundary is handled.
    assert Enum.any?(streams, &(&1.kind == :video))
    assert Enum.any?(streams, &(&1.kind == :audio))

    # Finer than the container duration: the inputs are 10 fps CFR, so every
    # output video packet must sit one ~0.1 s frame apart across the whole
    # join, and the second input's packets must be offset into the 2-4 s
    # window. The original bug advanced no offset for a duration-less input,
    # so the second input either overlapped the first (max pts near 2 s) or
    # was retimed onto a single filled gap (compressed spacing); both show up
    # in the packet timestamps below where the container duration alone hides
    # them.
    pts = video_packet_pts_times(out)
    assert length(pts) >= 36 and length(pts) <= 44
    assert Enum.max(pts) > 3.5

    gaps = pts |> Enum.chunk_every(2, 1, :discard) |> Enum.map(fn [a, b] -> b - a end)
    assert Enum.all?(gaps, &(&1 > 0.08 and &1 < 0.13))
  end

  test "concat of MPEG-TS segments with non-zero start times joins them from zero like ffmpeg -f concat" do
    # `-f segment` MPEG-TS segments keep the running source timestamps:
    # the first starts near 1.6 s and the second near 3.4 s. Kept as is,
    # those starts became a lead-in and a hole of about 2 s at the join.
    dir = Path.join(System.tmp_dir!(), "exmpeg_tscat_#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    on_exit(fn -> File.rm_rf(dir) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=64x48:r=10:d=4 -f lavfi -i sine=frequency=440:duration=4) ++
          ~w(-c:v libx264 -g 20 -c:a aac -f segment -segment_time 2 -segment_format mpegts #{dir}/seg%d.ts),
        env: %{}
      )

    segments = [seg0, _seg1] = [Path.join(dir, "seg0.ts"), Path.join(dir, "seg1.ts")]
    assert {:ok, %MediaInfo{format: %{start_time_s: start}}} = Exmpeg.probe(seg0)
    assert start > 1.0

    list = Path.join(dir, "list.txt")
    File.write!(list, Enum.map_join(segments, &"file '#{&1}'\n"))
    cli = Path.join(dir, "cli.mp4")
    {_, 0} = System.cmd("ffmpeg", ~w(-v error -y -f concat -safe 0 -i #{list} -c copy #{cli}), env: %{})

    out = Path.join(dir, "joined.mp4")
    assert {:ok, stats} = Exmpeg.concat(segments, out)
    assert {:ok, %MediaInfo{format: expected}} = Exmpeg.probe(cli)
    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    assert_in_delta stats.duration_s, expected.duration_s, 0.05
    assert_in_delta format.duration_s, expected.duration_s, 0.05

    pts = video_packet_pts_times(out)
    expected_pts = video_packet_pts_times(cli)
    assert length(pts) == length(expected_pts)
    assert hd(pts) < 0.1

    for {got, want} <- Enum.zip(pts, expected_pts) do
      assert_in_delta got, want, 0.01
    end
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

  test "transcode re-encodes H.264 + AAC into Matroska with out-of-band codec headers", %{clip: clip} do
    # Matroska stores the SPS/PPS in CodecPrivate. libx264 writes them to
    # extradata only under AV_CODEC_FLAG_GLOBAL_HEADER; without the flag
    # the muxer rejects the track at write_header.
    out = Path.join(System.tmp_dir!(), "exmpeg_xc_mkv_#{System.unique_integer([:positive])}.mkv")
    on_exit(fn -> File.rm(out) end)

    assert {:ok, %{streams_reencoded: 2}} =
             Exmpeg.transcode(clip, out, video_codec: "libx264", audio_codec: "aac", width: 80)

    assert {:ok, %MediaInfo{streams: streams}} = Exmpeg.probe(out)
    assert Enum.any?(streams, &(&1.kind == :video and &1.codec == "h264"))
    assert Enum.any?(streams, &(&1.kind == :audio and &1.codec == "aac"))
    assert {"", 0} = System.cmd("ffmpeg", ~w(-v error -i #{out} -f null -), stderr_to_stdout: true, env: %{})
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

  test "transcode keeps a re-encoded audio stream's 1 s delay against the video" do
    # The audio starts ~1 s after the video (0.978 s after AAC priming).
    # The re-encoded audio clock started at 0 and dropped that offset.
    src = Path.join(System.tmp_dir!(), "exmpeg_delay_#{System.unique_integer([:positive])}.mp4")

    outs =
      for ext <- ~w(mp4 mp4 webm),
          do: Path.join(System.tmp_dir!(), "exmpeg_delay_out_#{System.unique_integer([:positive])}.#{ext}")

    on_exit(fn -> Enum.each([src | outs], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=64x48:r=10:d=3 -itsoffset 1) ++
          ~w(-f lavfi -i sine=frequency=440:duration=2:sample_rate=48000 -c:v libx264 -c:a aac #{src}),
        env: %{}
      )

    assert %{"video" => +0.0, "audio" => src_audio} = stream_start_times(src)
    assert_in_delta src_audio, 0.978, 0.01

    runs = [
      [audio_codec: "aac"],
      [video_codec: "libx264", audio_codec: "aac"],
      [video_codec: "libvpx-vp9", audio_codec: "libopus"]
    ]

    for {out, opts} <- Enum.zip(outs, runs) do
      assert {:ok, _} = Exmpeg.transcode(src, out, opts)
      assert %{"video" => video, "audio" => audio} = stream_start_times(out)
      assert video < 0.05
      # The ffmpeg CLI gives 0.956 for AAC (a second priming) and 0.979 for Opus.
      assert audio > 0.9 and audio < 1.0
    end
  end

  test "transcode keeps a gap inside re-encoded audio in line with the video" do
    # Both streams skip the second between 1 s and 2 s. The audio clock
    # packed the samples after the gap straight onto the ones before it,
    # so the audio ended a second early and ran ahead of the video.
    src = Path.join(System.tmp_dir!(), "exmpeg_gap_#{System.unique_integer([:positive])}.mp4")
    out = Path.join(System.tmp_dir!(), "exmpeg_gap_out_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=80x60:r=10:d=4) ++
          ~w(-f lavfi -i sine=frequency=440:duration=4:sample_rate=48000) ++
          ["-vf", "select='not(between(t,1,1.999))'", "-af", "aselect='not(between(t,1,1.999))'"] ++
          ~w(-fps_mode passthrough -c:v libx264 -c:a aac #{src}),
        env: %{}
      )

    assert {:ok, _} =
             Exmpeg.transcode(src, out, video_codec: "libx264", audio_codec: "aac", video_filter: "null")

    # The ffmpeg CLI gives the same packet times as the source.
    audio = audio_packet_pts_times(out)
    assert_in_delta List.last(audio), 3.989, 0.002

    assert [[gap_start, gap_end]] =
             audio |> Enum.chunk_every(2, 1, :discard) |> Enum.filter(fn [a, b] -> b - a > 0.5 end)

    assert_in_delta gap_start, 0.981, 0.002
    assert_in_delta gap_end, 2.005, 0.002
    assert_in_delta out |> video_packet_pts_times() |> List.last(), 3.9, 0.002
  end

  test "transcode moves a negative container start to zero without cutting re-encoded audio" do
    # The audio starts at -0.52 s. Its timestamps stayed negative, and the
    # MP4 muxer cut the half second before zero.
    src = Path.join(System.tmp_dir!(), "exmpeg_neg_#{System.unique_integer([:positive])}.mkv")
    out = Path.join(System.tmp_dir!(), "exmpeg_neg_out_#{System.unique_integer([:positive])}.m4a")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i sine=frequency=440:duration=2:sample_rate=48000) ++
          ~w(-c:a aac -output_ts_offset -0.5 -avoid_negative_ts disabled #{src}),
        env: %{}
      )

    assert %{"audio" => src_start} = stream_start_times(src)
    assert src_start < -0.5

    assert {:ok, _} = Exmpeg.transcode(src, out, audio_codec: "aac")
    assert %{"audio" => +0.0} = stream_start_times(out)
    # The ffmpeg CLI gives 2.026 s.
    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out)
    assert_in_delta format.duration_s, 2.026, 0.01
  end

  test "transcode starts re-encoded Opus audio with the copied video" do
    # The Opus decoder trims its priming samples but moved the frame
    # timestamp only when it knew the packet time_base. The audio then
    # started 7 ms early and the muxer shifted the video to 0.007 s.
    src = Path.join(System.tmp_dir!(), "exmpeg_opus_#{System.unique_integer([:positive])}.webm")
    out = Path.join(System.tmp_dir!(), "exmpeg_opus_out_#{System.unique_integer([:positive])}.mkv")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=80x60:r=30:d=2) ++
          ~w(-f lavfi -i sine=frequency=440:duration=2:sample_rate=48000 -c:v libvpx-vp9 -c:a libopus #{src}),
        env: %{}
      )

    assert {:ok, %{streams_copied: 1}} = Exmpeg.transcode(src, out, audio_codec: "pcm_s16le")
    # The ffmpeg CLI starts both streams at 0 too.
    assert %{"video" => +0.0, "audio" => +0.0} = stream_start_times(out)
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
    # stream time_base on the buffersink, and the frames keep their ~2 s
    # of timestamps.
    assert format.duration_s > 1.5 and format.duration_s < 2.5
  end

  test "transcode :video_filter ignores an overridden :fps for pts timing", %{clip: clip} do
    # `:video_filter` overrides `:fps`, so the ignored `:fps` must not
    # change the timing. A high `:fps` once stamped frames too close
    # together and compressed the duration.
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

  test "transcode :video_filter keeps the timestamps the filter graph produces", %{clip: clip} do
    # The frames were restamped at a fixed cadence, which undid setpts and
    # flattened the gaps of a variable-frame-rate source.
    vfr = Path.join(System.tmp_dir!(), "exmpeg_vfr_#{System.unique_integer([:positive])}.mp4")
    outs = for _ <- 1..2, do: Path.join(System.tmp_dir!(), "exmpeg_vf_ts_#{System.unique_integer([:positive])}.mp4")
    [slow, cropped] = outs
    on_exit(fn -> Enum.each([vfr | outs], &File.rm/1) end)

    assert {:ok, _} = Exmpeg.transcode(clip, slow, video_codec: "libx264", video_filter: "setpts=2*PTS")
    assert_in_delta slow |> video_packet_pts_times() |> List.last(), 3.8, 0.01

    # Ten frames 50 ms apart, then ten frames 150 ms apart.
    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=80x60:r=10:d=2 -vf) ++
          ["settb=1/1000,setpts='if(lt(N,10),N*0.05,0.5+(N-10)*0.15)/TB'"] ++
          ~w(-fps_mode passthrough -enc_time_base 1/1000 -c:v libx264 -bf 0 #{vfr}),
        env: %{}
      )

    assert {:ok, _} = Exmpeg.transcode(vfr, cropped, video_codec: "libx264", video_filter: "crop=iw:ih-20:0:10")

    src_pts = video_packet_pts_times(vfr)
    out_pts = video_packet_pts_times(cropped)
    assert length(src_pts) == 20 and length(out_pts) == 20
    assert_in_delta List.last(src_pts), 1.85, 0.002
    Enum.zip_with(src_pts, out_pts, &assert_in_delta(&1, &2, 0.002))
  end

  test "transcode :video_filter drops a frame whose timestamp does not advance" do
    # Every frame shares its pts with the next one, as in many phone and
    # screen recordings. The muxer rejected the second frame of each pair.
    src = Path.join(System.tmp_dir!(), "exmpeg_dup_pts_#{System.unique_integer([:positive])}.mkv")
    out = Path.join(System.tmp_dir!(), "exmpeg_dup_pts_out_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out], &File.rm/1) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=80x60:r=10:d=2 -vf) ++
          ["setpts='floor(N/2)*2/10/TB'"] ++ ~w(-fps_mode passthrough -c:v ffv1 #{src}),
        env: %{}
      )

    src_pts = video_packet_pts_times(src)
    assert length(src_pts) == 20

    assert {:ok, _} = Exmpeg.transcode(src, out, video_codec: "libx264", video_filter: "crop=iw:ih-8:0:4")
    assert video_packet_pts_times(out) == Enum.dedup(src_pts)
  end

  test "transcode stamps raw H.264 frames that carry no timestamps" do
    # An Annex B stream has no timestamps at all. The frames entered the
    # graph without a pts, so the muxer rejected them or the fps filter
    # dropped them.
    src = Path.join(System.tmp_dir!(), "exmpeg_raw_#{System.unique_integer([:positive])}.h264")

    outs =
      for _ <- 1..2, do: Path.join(System.tmp_dir!(), "exmpeg_raw_out_#{System.unique_integer([:positive])}.mp4")

    on_exit(fn -> Enum.each([src | outs], &File.rm/1) end)

    {_, 0} =
      System.cmd("ffmpeg", ~w(-v error -y -f lavfi -i testsrc2=s=80x60:r=10:d=2 -c:v libx264 #{src}), env: %{})

    for {out, opts} <- Enum.zip(outs, [[], [video_filter: "null"]]) do
      assert {:ok, _} = Exmpeg.transcode(src, out, [video_codec: "libx264"] ++ opts)
      pts = video_packet_pts_times(out)
      assert length(pts) == 20
      assert_in_delta List.last(pts), 1.9, 0.01
    end
  end

  test "transcode keeps a 10 fps rate that only the container carries" do
    # FFV1 has no timing in its bitstream, so the decoder reports no frame
    # rate; only Matroska knows the source is 10 fps. Falling back to
    # 25 fps duplicated frames on the default chain and squeezed a custom
    # :video_filter to 0.8 s.
    src = Path.join(System.tmp_dir!(), "exmpeg_ffv1_#{System.unique_integer([:positive])}.mkv")
    out = Path.join(System.tmp_dir!(), "exmpeg_ffv1_out_#{System.unique_integer([:positive])}.mp4")
    out_crop = Path.join(System.tmp_dir!(), "exmpeg_ffv1_crop_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, out, out_crop], &File.rm/1) end)

    {_, 0} =
      System.cmd("ffmpeg", ~w(-v error -y -f lavfi -i testsrc2=s=64x48:r=10:d=2 -c:v ffv1 #{src}), env: %{})

    assert {:ok, _} = Exmpeg.transcode(src, out, video_codec: "libx264")
    pts = video_packet_pts_times(out)
    assert length(pts) == 20
    assert_in_delta List.last(pts), 1.9, 0.01

    assert {:ok, _} = Exmpeg.transcode(src, out_crop, video_codec: "libx264", video_filter: "crop=iw:ih-8:0:4")
    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(out_crop)
    assert format.duration_s > 1.5 and format.duration_s < 2.5
  end

  test "transcode keeps the sample aspect ratio the filter graph produces" do
    # 720x576 at SAR 64:45 displays as 16:9. The encoder never got the SAR,
    # so every re-encode came out as square pixels (5:4).
    src = make_anamorphic_clip()

    outs =
      for name <- ~w(same.mkv square.mp4 setsar.mp4),
          do: Path.join(System.tmp_dir!(), "exmpeg_sar_#{System.unique_integer([:positive])}_#{name}")

    [same, square, setsar] = outs
    plain = Path.join(System.tmp_dir!(), "exmpeg_sar_plain_#{System.unique_integer([:positive])}.mp4")
    container = Path.join(System.tmp_dir!(), "exmpeg_sar_container_#{System.unique_integer([:positive])}.mkv")
    container_out = Path.join(System.tmp_dir!(), "exmpeg_sar_container_out_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> Enum.each([src, plain, container, container_out | outs], &File.rm/1) end)

    assert {:ok, _} = Exmpeg.transcode(src, same, video_codec: "libx264")
    assert video_aspect(same) == {"720x576", "64:45", "16:9"}

    # A remux with -aspect sets the SAR only in the container, and the
    # container wins over the bitstream's square pixels.
    {_, 0} =
      System.cmd("ffmpeg", ~w(-v error -y -f lavfi -i testsrc2=s=320x240:r=10:d=1 -c:v libx264 #{plain}), env: %{})

    {_, 0} = System.cmd("ffmpeg", ~w(-v error -y -i #{plain} -c copy -aspect 16:9 #{container}), env: %{})
    assert {:ok, _} = Exmpeg.transcode(container, container_out, video_codec: "libx264")
    assert video_aspect(container_out) == {"320x240", "4:3", "16:9"}

    # A non-proportional resize keeps the display aspect ratio through
    # the SAR, as the `scale` filter and the ffmpeg CLI do.
    assert {:ok, _} = Exmpeg.transcode(src, square, video_codec: "libx264", width: 360, height: 360)
    assert video_aspect(square) == {"360x360", "16:9", "16:9"}

    assert {:ok, _} = Exmpeg.transcode(src, setsar, video_codec: "libx264", video_filter: "setsar=2")
    assert video_aspect(setsar) == {"720x576", "2:1", "5:2"}
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

    assert {:error, %Exmpeg.Error{reason: :invalid_request, message: msg, details: %{"field" => "stream_count"}}} =
             Exmpeg.concat([clip, video_only], out)

    assert msg =~ "stream"
  end

  test "concat rejects inputs whose codec parameters differ and names the stream and field" do
    dir = Path.join(System.tmp_dir!(), "exmpeg_concat_params_#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    on_exit(fn -> File.rm_rf(dir) end)

    fixture = fn name, args ->
      path = Path.join(dir, name)
      {_, 0} = System.cmd("ffmpeg", ~w(-v error -y) ++ args ++ ~w(#{path}), env: %{})
      path
    end

    sine = &~w(-f lavfi -i sine=frequency=440:duration=1:sample_rate=#{&1} -ac #{&2} -c:a pcm_s16le)
    video = &~w(-f lavfi -i testsrc2=s=64x48:r=10:d=1 -c:v libx264 -profile:v #{&1} -pix_fmt yuv420p)
    mono_44k = fixture.("mono_44k.wav", sine.(44_100, 1))
    mono_48k = fixture.("mono_48k.wav", sine.(48_000, 1))
    stereo_48k = fixture.("stereo_48k.wav", sine.(48_000, 2))
    high = fixture.("high.mp4", video.("high"))
    baseline = fixture.("baseline.mp4", video.("baseline"))
    high_cavlc = fixture.("high_cavlc.mp4", video.("high") ++ ~w(-x264-params cabac=0))

    out = Path.join(dir, "joined.wav")
    File.write!(out, "existing")

    for {inputs, out, field} <- [
          {[mono_44k, mono_48k], out, "sample_rate"},
          {[mono_48k, stereo_48k], out, "channel_layout"},
          {[high, baseline], Path.join(dir, "joined.mp4"), "profile"},
          {[high, high_cavlc], Path.join(dir, "joined.mp4"), "extradata"}
        ] do
      assert {:error, %Exmpeg.Error{reason: :invalid_request, details: %{"stream" => "0", "field" => ^field}}} =
               Exmpeg.concat(inputs, out)
    end

    assert File.read!(out) == "existing"
  end

  test "concat joins inputs whose layout order or unprobed parameters are the only difference" do
    dir = Path.join(System.tmp_dir!(), "exmpeg_concat_unknown_#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    on_exit(fn -> File.rm_rf(dir) end)

    fixture = fn name, args ->
      path = Path.join(dir, name)
      {_, 0} = System.cmd("ffmpeg", ~w(-v error -y) ++ args ++ ~w(#{path}), env: %{})
      path
    end

    # A plain WAV header carries only a channel count; the MOV `chan`
    # atom names the stereo layout.
    sine = ~w(-f lavfi -i sine=frequency=440:duration=1:sample_rate=48000 -ac 2 -c:a pcm_s16le)
    unspecified = fixture.("unspecified.wav", sine)
    stereo = fixture.("stereo.mov", sine)

    # A TS cut inside a 10 s GOP leaves no SPS in the second part's probe
    # window, so its profile, size, and pixel format stay unset.
    long = fixture.("long.ts", ~w(-f lavfi -i testsrc2=s=64x48:r=10:d=10 -c:v libx264 -g 100))
    bytes = File.read!(long)
    cut = 188 * div(byte_size(bytes), 376)
    part_a = Path.join(dir, "part_a.ts")
    part_b = Path.join(dir, "part_b.ts")
    File.write!(part_a, binary_part(bytes, 0, cut))
    File.write!(part_b, binary_part(bytes, cut, byte_size(bytes) - cut))

    for {inputs, out} <- [
          {[unspecified, stereo], Path.join(dir, "joined.wav")},
          {[part_a, part_b], Path.join(dir, "joined.mp4")}
        ] do
      assert {:ok, %{inputs_joined: 2}} = Exmpeg.concat(inputs, out)
    end
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

  test "memory input cannot read a local file via a crafted manifest demuxer" do
    # A DASH manifest whose only segment points at an on-disk media file.
    # The DASH demuxer auto-probes raw bytes (no extension/MIME gate on a
    # custom AVIO), so without a protocol whitelist FFmpeg would open the
    # referenced file and probe its streams - local file disclosure driven
    # by attacker-controlled input. With the whitelist pinned to
    # `crypto,data`, the nested `file:` open is refused and the call fails
    # instead of leaking the sentinel's streams.
    sentinel =
      Path.join(System.tmp_dir!(), "exmpeg_sentinel_#{System.unique_integer([:positive])}.mp4")

    on_exit(fn -> File.rm(sentinel) end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-y -f lavfi -i color=c=red:s=128x96:d=1 -c:v libx264 -pix_fmt yuv420p #{sentinel}),
        stderr_to_stdout: true,
        env: %{}
      )

    mpd = """
    <?xml version="1.0"?>
    <MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT1S" minBufferTime="PT1S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
      <Period>
        <AdaptationSet mimeType="video/mp4" segmentAlignment="true">
          <Representation id="1" bandwidth="100000" codecs="avc1.42c01e" width="128" height="96">
            <BaseURL>file://#{sentinel}</BaseURL>
            <SegmentBase indexRange="0-9999"><Initialization range="0-9999"/></SegmentBase>
          </Representation>
        </AdaptationSet>
      </Period>
    </MPD>
    """

    assert {:error, %Exmpeg.Error{}} = Exmpeg.probe({:memory, mpd})
  end

  test "a loaded buffer is reusable across operations", %{clip: clip} do
    out = Path.join(System.tmp_dir!(), "exmpeg_buf_#{System.unique_integer([:positive])}.opus")
    cat = Path.join(System.tmp_dir!(), "exmpeg_bufcat_#{System.unique_integer([:positive])}.mp4")
    on_exit(fn -> File.rm(out) end)
    on_exit(fn -> File.rm(cat) end)

    bytes = File.read!(clip)
    assert {:ok, %Exmpeg.Buffer{byte_size: size} = buf} = Exmpeg.load_buffer(bytes)
    assert size == byte_size(bytes)

    # The same buffer feeds several operations without re-copying.
    assert {:ok, %MediaInfo{format: format}} = Exmpeg.probe(buf)
    assert format.nb_streams == 2

    assert {:ok, audio} = Exmpeg.extract_audio(buf, out, sample_rate: 16_000)
    assert audio.codec == "libopus"

    assert {:ok, concat} = Exmpeg.concat([buf, buf], cat)
    assert concat.inputs_joined == 2
    assert {:ok, %MediaInfo{format: cat_format}} = Exmpeg.probe(cat)
    assert cat_format.duration_s > 3.5
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

  test "extract_audio progress counts muxed packets, not samples", %{clip: clip} do
    # m4a keeps one sample entry per muxed packet, so ffprobe reads back
    # the exact count. The WAV demuxer re-chunks on read and would not.
    out = Path.join(System.tmp_dir!(), "exmpeg_audio_progress_#{System.unique_integer([:positive])}.m4a")
    on_exit(fn -> File.rm(out) end)

    parent = self()

    task =
      Task.async(fn ->
        Exmpeg.extract_audio(clip, out, progress: parent)
      end)

    {:ok, stats} = Task.await(task, 60_000)

    msgs = drain_progress([])
    assert msgs != [], "expected at least one progress message"
    last = List.last(msgs)
    assert last.op == "extract_audio"
    assert last.total_duration_s > 1.5
    assert last.packets_written == audio_packet_count(out)
    assert last.packets_written < stats.samples_written
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

  # A 1 s 720x576 clip with SAR 64:45, so it displays as 16:9.
  defp make_anamorphic_clip do
    path = Path.join(System.tmp_dir!(), "exmpeg_anamorphic_#{System.unique_integer([:positive])}.mp4")

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-v error -y -f lavfi -i testsrc2=s=720x576:r=10:d=1 -vf setsar=64/45 -c:v libx264 #{path}),
        env: %{}
      )

    path
  end

  # `{"WxH", sar, dar}` of the first video stream, read via ffprobe; the
  # probe API does not expose the sample aspect ratio.
  defp video_aspect(path) do
    {out, 0} =
      System.cmd(
        "ffprobe",
        ~w(-v error -select_streams v:0 -show_entries stream=width,height,sample_aspect_ratio,display_aspect_ratio) ++
          ["-of", "csv=p=0", path],
        env: %{}
      )

    [w, h, sar, dar] = out |> String.trim() |> String.split(",")
    {"#{w}x#{h}", sar, dar}
  end

  # `%{"video" => start_s, "audio" => start_s}` of a file's streams, read
  # via ffprobe; the probe API reports only the container start time.
  defp stream_start_times(path) do
    {out, 0} =
      System.cmd(
        "ffprobe",
        ~w(-v error -show_entries stream=codec_type,start_time -of csv=p=0 #{path}),
        env: %{}
      )

    for line <- String.split(out, "\n", trim: true), into: %{} do
      [kind, start] = String.split(line, ",")
      {kind, String.to_float(start)}
    end
  end

  # Sorted presentation timestamps (seconds) of a file's video or audio
  # packets, read via ffprobe. The probe API exposes stream/format metadata but not
  # per-packet timing, so concat boundary timing is asserted through this.
  defp video_packet_pts_times(path), do: packet_pts_times(path, "v:0")

  defp audio_packet_pts_times(path), do: packet_pts_times(path, "a:0")

  defp packet_pts_times(path, stream) do
    {out, 0} =
      System.cmd(
        "ffprobe",
        ["-v", "error", "-select_streams", stream, "-show_entries", "packet=pts_time", "-of", "csv=p=0", path],
        env: %{}
      )

    out
    |> String.split("\n", trim: true)
    |> Enum.flat_map(fn line ->
      case Float.parse(line) do
        {f, _} -> [f]
        :error -> []
      end
    end)
    |> Enum.sort()
  end

  # Number of audio packets in a file, read via ffprobe.
  defp audio_packet_count(path) do
    {out, 0} =
      System.cmd(
        "ffprobe",
        [
          "-v",
          "error",
          "-count_packets",
          "-select_streams",
          "a:0",
          "-show_entries",
          "stream=nb_read_packets",
          "-of",
          "csv=p=0",
          path
        ],
        env: %{}
      )

    out |> String.trim() |> String.to_integer()
  end

  test "killing the caller mid-transcode cancels the NIF and removes the partial" do
    # A long, high-resolution source so the encode is unmistakably still
    # running when we kill the calling process - small clips finish before
    # a kill can land.
    src = Path.join(System.tmp_dir!(), "exmpeg_cancel_src_#{System.unique_integer([:positive])}.mp4")
    out = Path.join(System.tmp_dir!(), "exmpeg_cancel_out_#{System.unique_integer([:positive])}.mp4")

    on_exit(fn ->
      File.rm(src)
      File.rm(out)
      out |> partials_for() |> Enum.each(&File.rm/1)
    end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-y -f lavfi -i testsrc2=s=1280x720:r=30:d=60 -c:v libx264 -preset ultrafast -pix_fmt yuv420p #{src}),
        stderr_to_stdout: true,
        env: %{}
      )

    parent = self()

    # Unlinked spawn: a linked Task would propagate the :kill exit to the
    # test process. We only need the pid to kill, not the result.
    pid =
      spawn(fn ->
        send(parent, {:started, self()})
        Exmpeg.transcode(src, out, video_codec: "libx264", width: 1280)
      end)

    assert_receive {:started, ^pid}, 5_000

    # Wait until the muxer has actually opened the partial file, i.e. the
    # encode loop is running, before pulling the rug out.
    assert eventually(fn -> partials_for(out) != [] end, 10_000)

    Process.exit(pid, :kill)

    # Within ~100 ms the NIF observes the dead caller, returns the
    # `cancelled` error, and atomic_output removes the partial. No final
    # output is ever produced.
    assert eventually(fn -> partials_for(out) == [] and not File.exists?(out) end, 10_000)
  end

  test "killing the caller while the filter graph drains cancels the NIF and removes the partial" do
    # Six input frames, so the packet loop ends almost at once. `tpad`
    # emits its 30 000 padded frames only after EOF, so the kill lands in
    # the filter drain. Without a liveness check there, the NIF keeps
    # encoding for minutes after the caller is gone.
    src = Path.join(System.tmp_dir!(), "exmpeg_cancel_drain_src_#{System.unique_integer([:positive])}.mp4")
    out = Path.join(System.tmp_dir!(), "exmpeg_cancel_drain_out_#{System.unique_integer([:positive])}.mp4")

    on_exit(fn ->
      File.rm(src)
      File.rm(out)
      out |> partials_for() |> Enum.each(&File.rm/1)
    end)

    {_, 0} =
      System.cmd(
        "ffmpeg",
        ~w(-y -f lavfi -i testsrc2=s=1280x720:r=30:d=0.2 -c:v libx264 -preset ultrafast -pix_fmt yuv420p #{src}),
        stderr_to_stdout: true,
        env: %{}
      )

    parent = self()

    pid =
      spawn(fn ->
        send(parent, {:started, self()})
        Exmpeg.transcode(src, out, video_codec: "libx264", video_filter: "tpad=stop=30000:stop_mode=clone")
      end)

    assert_receive {:started, ^pid}, 5_000
    assert eventually(fn -> partials_for(out) != [] end, 10_000)

    # Let the packet loop finish so the NIF is inside the drain.
    Process.sleep(1_000)
    Process.exit(pid, :kill)

    assert eventually(fn -> partials_for(out) == [] and not File.exists?(out) end, 2_000)
  end

  defp drain_progress(acc) do
    receive do
      {:exmpeg_progress, msg} -> drain_progress([msg | acc])
    after
      50 -> Enum.reverse(acc)
    end
  end

  # All `<stem>.partial*` siblings of an output path. Globbing the infix
  # keeps the assertion stable regardless of the exact partial-name shape.
  defp partials_for(out) do
    root = Path.rootname(out)
    Path.wildcard(root <> ".partial*")
  end

  defp eventually(fun, timeout_ms, waited_ms \\ 0) do
    cond do
      fun.() ->
        true

      waited_ms >= timeout_ms ->
        false

      true ->
        Process.sleep(50)
        eventually(fun, timeout_ms, waited_ms + 50)
    end
  end
end
