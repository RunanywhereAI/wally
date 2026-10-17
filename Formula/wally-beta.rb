class WallyBeta < Formula
  desc "Run models on your machine or your RunAnywhere account (beta)"
  homepage "https://github.com/RunanywhereAI/wally"
  url "https://github.com/RunanywhereAI/wally/releases/download/v0.4.0-beta.1/wally-0.4.0-beta.1-Darwin-arm64.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  version "0.4.0-beta.1"

  depends_on :macos
  depends_on arch: :arm64

  # Installed as wally-beta so it can sit alongside a stable wally. Each
  # formula gets its own libexec, so the two Metal bundles do not collide.
  def install
    libexec.install "libexec/wally", "libexec/mlx-swift_Cmlx.bundle"
    bin.install_symlink libexec/"wally" => "wally-beta"
  end

  def caveats
    <<~EOS
      This is a prerelease. It installs as wally-beta and leaves a stable wally
      alone, so both can be on the machine at once.

      Models are downloaded on demand and kept in
        ~/.local/share/runanywhere

      Getting started:
        wally-beta list --all         every model in the catalog
        wally-beta pull qwen3-0.6b    download one
        wally-beta run qwen3-0.6b     talk to it, /? for commands

      To remove:
        brew uninstall wally-beta
    EOS
  end

  test do
    assert_match "wally", shell_output("#{bin}/wally-beta --version")
    # Proves the Metal bundle survived the install: without it MLX drops out
    # silently and the other five engines carry on.
    assert_match "mlx", shell_output("#{bin}/wally-beta backends")
  end
end
