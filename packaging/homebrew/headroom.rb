cask "headroom" do
  version :latest
  sha256 :no_check

  url "https://github.com/gglucass/headroom-desktop/releases/latest/download/Headroom.dmg"
  name "Headroom"
  desc "Cut Claude Code and Codex token costs by ~50%"
  homepage "https://extraheadroom.com/"

  depends_on :macos
  depends_on arch: :arm64

  app "Headroom.app"

  zap trash: [
    "~/Library/Application Support/Headroom",
    "~/Library/Caches/com.extraheadroom.headroom",
    "~/Library/Preferences/com.extraheadroom.headroom.plist",
  ]
end
