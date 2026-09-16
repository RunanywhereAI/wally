class Wally < Formula
  desc "RunAnywhere CLI: local and cloud LLM inference, on-device GGUF and MLX"
  homepage "https://github.com/RunanywhereAI/wally"
  version "0.0.0"
  license "Apache-2.0"

  # Homebrew ships the production macOS arm64 bottle. The dev bottle is not
  # tapped. Version and sha256 are stamped by scripts/release/stamp-formula.py
  # from the released archive, so this file is committed with placeholders.
  on_macos do
    on_arm do
      url "https://github.com/RunanywhereAI/wally/releases/download/v#{version}/wally-#{version}-macos-arm64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    # wally-mlx and the Metal shader bundles must sit beside the binary, so the
    # whole bin/ tree goes to libexec and only wally is linked onto PATH. Go's
    # os.Executable resolves the symlink to libexec, where the helper lives.
    libexec.install Dir["bin/*"]
    bin.install_symlink libexec/"wally"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/wally --version")
  end
end
