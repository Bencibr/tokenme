# Homebrew cask for the TokenMe menu-bar panel. The universal DMG is the
# Intel artifact (one download runs on every Mac); Apple Silicon gets the
# smaller arm64-only image. The app updates itself in place (the panel's
# in-app updater swaps the bundle), hence auto_updates.
cask "tokenme" do
  version "0.1.4"

  on_arm do
    url "https://github.com/Bencibr/tokenme/releases/download/v#{version}/tokenme_#{version}_arm64.dmg"
    sha256 "b2b26c762f3afb96ff8be1a39c75ed7ffc57b55dbf6524a82e4449d392215dad"
  end
  on_intel do
    url "https://github.com/Bencibr/tokenme/releases/download/v#{version}/tokenme_universal.dmg"
    sha256 "e48aa518d759ede358606af5c10ff0dc5088ca3ed10918f13ae3905176f8f727"
  end

  name "TokenMe"
  desc "Menu-bar usage and quota panel for AI coding tools"
  homepage "https://github.com/Bencibr/tokenme"

  livecheck do
    url :stable
    regex(/^v?(\d+(?:\.\d+)+)$/i)
    strategy :github_latest
  end

  auto_updates true

  app "TokenMe.app"
end
