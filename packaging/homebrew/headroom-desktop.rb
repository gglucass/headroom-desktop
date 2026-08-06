cask "headroom-desktop" do
  version "0.7.4"
  sha256 "ca58f2b017395c352a6961bb43a53d375827f5e43694b542d60576b86be05734"

  url "https://github.com/gglucass/headroom-desktop/releases/download/v#{version}/Headroom_#{version}_mac.dmg",
      verified: "github.com/gglucass/headroom-desktop/"
  name "Headroom Desktop"
  desc "Reduce token usage for Claude Code and Codex"
  homepage "https://extraheadroom.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  depends_on macos: :sonoma
  depends_on arch: :arm64

  app "Headroom.app"

  uninstall launchctl: "com.extraheadroom.headroom",
            quit:      "com.extraheadroom.headroom"

  zap trash: [
    "~/.headroom",
    "~/Library/Application Support/Headroom",
    "~/Library/Caches/com.extraheadroom.headroom",
    "~/Library/LaunchAgents/com.extraheadroom.headroom.plist",
    "~/Library/LaunchAgents/Headroom.plist",
    "~/Library/Preferences/com.extraheadroom.headroom.plist",
    "~/Library/Saved Application State/com.extraheadroom.headroom.savedState",
  ]
end
