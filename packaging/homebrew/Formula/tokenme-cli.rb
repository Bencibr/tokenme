# Homebrew formula for the tokenme CLI (cask tokenme is the panel; this is the CLI).: installs the prebuilt binary from the
# GitHub release — no compile step, the workspace's debug/test targets never
# touch the end user. The menu-bar panel is the tokenme cask.
class TokenmeCli < Formula
  desc "Cross-tool AI token usage and cost from the tools' own logs"
  homepage "https://github.com/Bencibr/tokenme"
  version "0.1.4"
  license "MIT"

  livecheck do
    url :stable
    regex(/^v?(\d+(?:\.\d+)+)$/i)
    strategy :github_latest
  end

  # macOS-only on purpose: the releases carry Apple and Windows binaries, and
  # a formula that compiles would force a Rust toolchain onto every user.
  depends_on :macos

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Bencibr/tokenme/releases/download/v#{version}/tokenme-cli-aarch64-apple-darwin.tar.gz"
      sha256 "ac226d502da8c5eaafaab573d747c8b034b6521bfd3f3baab183edaea2effc66"
    else
      url "https://github.com/Bencibr/tokenme/releases/download/v#{version}/tokenme-cli-x86_64-apple-darwin.tar.gz"
      sha256 "602673ae439183b369bafa051dc63ea8ee54f8bfae6fb62bd3f7d00729bce9ce"
    end
  end

  def install
    bin.install "tokenme"
  end

  def caveats
    <<~EOS
      The menu-bar panel (quota gauges, usage pages) ships as a cask:
        brew install --cask Bencibr/tokenme/tokenme
    EOS
  end

  test do
    # The CLI stamps its own Cargo version, which trailed the panel's until the
    # workspace alignment; match the shape, not this release's literal.
    assert_match %r{tokenme \d+\.\d+\.\d+}, shell_output("#{bin}/tokenme --version")
  end
end
