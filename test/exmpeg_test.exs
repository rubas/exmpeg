defmodule ExmpegTest do
  @moduledoc "Tests for Exmpeg public API - argument validation and option checking."

  use ExUnit.Case, async: true

  alias Exmpeg.Error

  describe "probe/1" do
    test "rejects a non-string path" do
      assert {:error, %Error{reason: :invalid_request}} = Exmpeg.probe(42)
    end

    test "rejects an empty path" do
      assert {:error, %Error{reason: :invalid_request}} = Exmpeg.probe("   ")
    end

    test "rejects a missing path with :io_error from libavformat" do
      # The NIF returns :invalid_request for a non-existent path before
      # libavformat even sees it. Either kind is acceptable; what matters
      # is we surface a typed error rather than a tuple from the NIF.
      assert {:error, %Error{reason: reason}} = Exmpeg.probe("/no/such/file.mp4")
      assert reason in [:invalid_request, :io_error]
    end

    test "rejects a non-UTF-8 path without raising" do
      # Legal on Linux (e.g. from File.ls/1) but not decodable as a Rust
      # String; must surface as an error tuple, not a NIF decode raise.
      assert {:error, %Error{reason: :invalid_request, message: msg}} =
               Exmpeg.probe(<<0xFF, 0xFE, "x.mp4">>)

      assert msg =~ "UTF-8"
    end
  end

  describe "remux/3" do
    test "rejects non-string input/output" do
      assert {:error, %Error{reason: :invalid_request}} = Exmpeg.remux(nil, "out.mp4")
      assert {:error, %Error{reason: :invalid_request}} = Exmpeg.remux("in.mp4", nil)
    end

    test "rejects unknown options" do
      assert {:error, %Error{reason: :invalid_request, message: msg}} =
               Exmpeg.remux("in.mp4", "out.mp4", banana: 1)

      assert msg =~ "unknown option"
    end

    test "rejects a URL output (write-side SSRF guard)" do
      for url <- ["http://host/out.mp4", "ftp://host/out.mp4", "rtmp://host/live"] do
        assert {:error, %Error{reason: :invalid_request, message: msg}} =
                 Exmpeg.remux("/no/such/file.mp4", url)

        assert msg =~ "local path"
      end
    end

    test "rejects negative start_s" do
      assert {:error, %Error{reason: :invalid_request, message: msg}} =
               Exmpeg.remux("in.mp4", "out.mp4", start_s: -1)

      assert msg =~ "start_s"
    end

    test "rejects zero or negative duration_s" do
      for bad <- [0, -1, -0.5] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.remux("in.mp4", "out.mp4", duration_s: bad)
      end
    end

    test "accepts numeric start_s and duration_s shapes without erroring at the validator" do
      # We don't have a real input file here; libavformat will reject
      # the open. We only care that the validator passes and the error
      # comes from the NIF rather than option validation.
      assert {:error, %Error{reason: reason}} =
               Exmpeg.remux("/no/such/file.mp4", "/tmp/exmpeg_invalid_out.mp4",
                 start_s: 0.5,
                 duration_s: 1
               )

      assert reason in [:invalid_request, :io_error]
    end
  end

  describe "version/0" do
    test "returns the linked FFmpeg version triplet" do
      assert {:ok, info} = Exmpeg.version()
      assert Regex.match?(~r/^\d+\.\d+\.\d+$/, info.avformat)
      assert Regex.match?(~r/^\d+\.\d+\.\d+$/, info.avcodec)
      assert Regex.match?(~r/^\d+\.\d+\.\d+$/, info.avutil)
      assert is_binary(info.license)
      assert is_binary(info.configuration)
    end
  end

  describe "extract_frame/3" do
    test "rejects unknown options" do
      assert {:error, %Error{reason: :invalid_request, message: msg}} =
               Exmpeg.extract_frame("in.mp4", "out.jpg", banana: 1)

      assert msg =~ "unknown option"
    end

    test "rejects non-positive width / height" do
      for opt <- [width: 0, height: 0, width: -10] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.extract_frame("in.mp4", "out.jpg", [opt])
      end
    end

    test "rejects width / height past the dimension cap" do
      for opt <- [width: 16_385, height: 1_000_000, width: 2_000_000_000] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.extract_frame("in.mp4", "out.jpg", [opt])
      end
    end

    test "rejects negative timestamp" do
      assert {:error, %Error{reason: :invalid_request}} =
               Exmpeg.extract_frame("in.mp4", "out.jpg", timestamp_s: -1)
    end
  end

  describe "extract_audio/3" do
    test "rejects non-positive sample_rate" do
      assert {:error, %Error{reason: :invalid_request}} =
               Exmpeg.extract_audio("in.mp4", "out.wav", sample_rate: 0)
    end

    test "rejects sample_rate past the cap" do
      assert {:error, %Error{reason: :invalid_request}} =
               Exmpeg.extract_audio("in.mp4", "out.wav", sample_rate: 768_001)
    end

    test "rejects channels other than 1 or 2" do
      for c <- [0, 3, 8] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.extract_audio("in.mp4", "out.wav", channels: c)
      end
    end
  end

  describe "concat/2" do
    test "rejects an empty input list" do
      assert {:error, %Error{reason: :invalid_request}} = Exmpeg.concat([], "out.mp4")
    end

    test "rejects non-binary entries in the input list" do
      assert {:error, %Error{reason: :invalid_request}} =
               Exmpeg.concat(["a.mp4", :not_a_path], "out.mp4")
    end

    test "rejects non-string output" do
      assert {:error, %Error{reason: :invalid_request}} = Exmpeg.concat(["a.mp4"], nil)
    end
  end

  describe "transcode/3" do
    test "rejects unknown options" do
      assert {:error, %Error{reason: :invalid_request, message: msg}} =
               Exmpeg.transcode("in.mp4", "out.mp4", banana: 1)

      assert msg =~ "unknown option"
    end

    test "rejects fps that isn't a positive {num, den}" do
      for bad <- [{0, 1}, {30, 0}, {-1, 1}, "30"] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.transcode("in.mp4", "out.mp4", fps: bad)
      end
    end

    test "rejects an fps component past the i32 range without raising" do
      assert {:error, %Error{reason: :invalid_request}} =
               Exmpeg.transcode("in.mp4", "out.mp4", fps: {2_147_483_648, 1})
    end

    test "rejects a bitrate past the supported range without raising" do
      for opt <- [video_bitrate: 10 ** 30, audio_bitrate: 10 ** 30] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.transcode("in.mp4", "out.mp4", [opt])
      end
    end

    test "rejects a non-UTF-8 string option without raising" do
      for opt <- [video_codec: <<0xFF, 0xFE>>, audio_codec: <<0xFF>>, video_filter: <<0xFF, "x">>] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.transcode("in.mp4", "out.mp4", [opt])
      end
    end

    test "rejects a non-UTF-8 tag key or value without raising" do
      for tags <- [[{<<0xFF>>, "v"}], [{"k", <<0xFF>>}], %{<<0xFF>> => "v"}] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.transcode("in.mp4", "out.mp4", tags: tags)
      end
    end

    test "rejects a non-{key, value} option entry without raising" do
      assert {:error, %Error{reason: :invalid_request, message: msg}} =
               Exmpeg.transcode("in.mp4", "out.mp4", [:fast])

      assert msg =~ "key, value"
    end

    test "rejects empty codec names" do
      assert {:error, %Error{reason: :invalid_request}} =
               Exmpeg.transcode("in.mp4", "out.mp4", video_codec: "")
    end

    test "rejects width / height / sample_rate past their caps" do
      for opt <- [width: 16_385, height: 2_000_000_000, sample_rate: 768_001] do
        assert {:error, %Error{reason: :invalid_request}} =
                 Exmpeg.transcode("in.mp4", "out.mp4", [opt])
      end
    end
  end
end
