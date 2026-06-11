defmodule Exmpeg.Buffer do
  @moduledoc """
  An opaque, pre-loaded in-memory input created by `Exmpeg.load_buffer/1`.

  A `{:memory, binary}` input copies the whole binary into the Rust heap
  on every call, so probing then transcoding the same bytes copies them
  twice and an N-input `concat` holds N copies at once. A buffer copies
  the bytes once into a refcounted native resource; passing the buffer to
  later operations is then a refcount bump, not another copy.

  Accept it anywhere an input source is expected:

      {:ok, buf} = Exmpeg.load_buffer(File.read!("clip.mp4"))
      {:ok, info} = Exmpeg.probe(buf)
      {:ok, _} = Exmpeg.transcode(buf, "out.webm", video_codec: "libvpx-vp9")

  The struct is opaque: `ref` holds the native resource and is not meant
  to be inspected. The resource (and its bytes) is freed when the buffer
  is garbage-collected.
  """

  @enforce_keys [:ref, :byte_size]
  defstruct [:ref, :byte_size]

  @type t :: %__MODULE__{ref: reference(), byte_size: non_neg_integer()}
end
