defmodule Exmpeg.ErrorTest do
  @moduledoc "Tests for Exmpeg.Error - native payload mapping and exception message."

  use ExUnit.Case, async: true

  alias Exmpeg.Error

  test "new/3 builds a struct with default empty details" do
    err = Error.new(:invalid_request, "boom")
    assert err.reason == :invalid_request
    assert err.message == "boom"
    assert err.details == %{}
  end

  test "from_native/1 maps every known type string" do
    for {type, atom} <- [
          {"invalid_request", :invalid_request},
          {"io_error", :io_error},
          {"decode_error", :decode_error},
          {"encode_error", :encode_error},
          {"unsupported", :unsupported},
          {"runtime_error", :runtime_error},
          {"nif_panic", :nif_panic}
        ] do
      err = Error.from_native(%{type: type, message: "x", details: %{"k" => "v"}})
      assert err.reason == atom
      assert err.message == "x"
      assert err.details == %{"k" => "v"}
    end
  end

  test "from_native/1 falls back to :native_error for unknown type" do
    err = Error.from_native(%{type: "freshly_invented_kind", message: "?"})
    assert err.reason == :native_error
  end

  test "from_native/1 handles non-map payloads" do
    err = Error.from_native({:weird, :tuple})
    assert err.reason == :native_error
    assert err.details["raw"] =~ "weird"
  end

  test "Exception.message/1 prefixes the reason" do
    err = Error.new(:io_error, "no such file")
    assert Exception.message(err) == "io_error: no such file"
  end
end
