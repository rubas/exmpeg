defmodule Exmpeg.TestFixtures do
  @moduledoc """
  Test-side helpers for generating media files on disk.

  Synthesises sample clips with `ffmpeg`'s `lavfi` virtual demuxers
  (sine + testsrc) so the integration suite never depends on the network
  or a checked-in binary. The CLI is only used during fixture
  generation; the library under test never shells out.
  """

  @fixture_dir Path.expand("../fixtures", __DIR__)

  @doc "Absolute path to the test/fixtures directory."
  @spec dir() :: String.t()
  def dir, do: @fixture_dir

  @doc """
  Returns the path to a short MP4 with one video stream and one audio
  stream. Generated on first call; cached for subsequent calls in the
  same run.
  """
  @spec ensure_av_clip!() :: String.t()
  def ensure_av_clip! do
    path = Path.join(@fixture_dir, "av_clip.mp4")

    unless File.exists?(path) do
      File.mkdir_p!(@fixture_dir)
      generate_av_clip!(path)
    end

    path
  end

  @doc "Returns the path to a short video-only MP4 (no audio stream)."
  @spec ensure_video_only_clip!() :: String.t()
  def ensure_video_only_clip! do
    path = Path.join(@fixture_dir, "video_only.mp4")

    unless File.exists?(path) do
      File.mkdir_p!(@fixture_dir)

      run_ffmpeg!([
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=80x60:rate=10:duration=1",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        path
      ])
    end

    path
  end

  @doc "Returns the path to a short audio-only WAV (no video stream)."
  @spec ensure_audio_only_clip!() :: String.t()
  def ensure_audio_only_clip! do
    path = Path.join(@fixture_dir, "audio_only.wav")

    unless File.exists?(path) do
      File.mkdir_p!(@fixture_dir)

      run_ffmpeg!([
        "-y",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100:duration=1",
        "-c:a",
        "pcm_s16le",
        path
      ])
    end

    path
  end

  defp generate_av_clip!(path) do
    run_ffmpeg!([
      "-y",
      "-f",
      "lavfi",
      "-i",
      "testsrc=size=160x120:rate=10:duration=2",
      "-f",
      "lavfi",
      "-i",
      "sine=frequency=440:sample_rate=44100:duration=2",
      "-c:v",
      "libx264",
      "-preset",
      "ultrafast",
      "-pix_fmt",
      "yuv420p",
      "-c:a",
      "aac",
      "-shortest",
      path
    ])
  end

  # Clear inherited env to avoid leaking secrets into the ffmpeg child;
  # the binary only needs PATH-resolved libs and our explicit argv.
  defp run_ffmpeg!(args) do
    case System.cmd("ffmpeg", args, stderr_to_stdout: true, env: %{}) do
      {_out, 0} -> :ok
      {out, status} -> raise "ffmpeg fixture generation failed (status #{status}):\n#{out}"
    end
  end
end
