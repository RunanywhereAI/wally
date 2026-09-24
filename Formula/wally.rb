# frozen_string_literal: true

class Wally < Formula
  desc "Run language, speech and image models on your own machine"
  homepage "https://github.com/RunanywhereAI/wally"
  version "0.6.1"
  license "MIT"

  # macOS arm64 ships the Swift MLX host
  # (llama.cpp + ONNX + Sherpa + MLX). Linux is not in this cut.
  on_macos do
    on_arm do
      url "https://github.com/RunanywhereAI/wally/releases/download/v0.6.1/wally-0.6.1-macos-arm64.tar.gz"
      # Placeholder -- the v0.6.1 release has not published a wally-named asset
      # yet. scripts/release/update-tap.sh re-stamps this from the real release
      # checksum; a stale value here fails brew install's own hash check
      # rather than installing something unverified.
      sha256 "0000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "bin/wally"
    bin.install "bin/mlx.metallib" if File.file?("bin/mlx.metallib")
    bin.install Dir["bin/*.bundle"] if Dir["bin/*.bundle"].any?
    lib.install Dir["lib/*"] if Dir["lib/*"].any?
  end

  def caveats
    <<~EOS
      Models are downloaded on demand and kept in
        ~/.local/share/runanywhere

      Getting started:
        wally models list             downloaded models
        wally models pull qwen3       download one
        wally run qwen3               talk to it
        wally run qwen3 "hi"          ask once and exit

      Also:
        wally account login           sign in
        wally backends                which engines this build linked
    EOS
  end

  test do
    assert_match "wally", shell_output("#{bin}/wally version")
    backends = shell_output("#{bin}/wally backends")
    assert_match(/llama/i, backends)
    assert_match(/onnx/i, backends)
    assert_match(/sherpa/i, backends)
    assert_match(/mlx/i, backends) if OS.mac? && Hardware::CPU.arm?
  end
end
