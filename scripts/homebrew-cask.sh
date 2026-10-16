#!/bin/sh
# Prints the Homebrew cask for a published DataQuery release, with the DMG
# checksums GitHub recorded for it. Used by the release workflow to update the
# tap (github.com/dev-manik-mia/homebrew-dataquery); also runnable by hand:
#   sh scripts/homebrew-cask.sh 0.1.1 > ../homebrew-dataquery/Casks/dataquery.rb
set -eu
version=${1:?usage: homebrew-cask.sh <version>}
repo=${GITHUB_REPOSITORY:-dev-manik-mia/DataQuery}

digest() {
  sha=$(gh api "repos/$repo/releases/tags/v$version" \
    --jq ".assets[] | select(.name == \"DataQuery_${version}_$1.dmg\") | .digest" | sed 's/^sha256://')
  [ -n "$sha" ] || { echo "DataQuery_${version}_$1.dmg is not attached to v$version" >&2; exit 1; }
  echo "$sha"
}
arm=$(digest aarch64)
intel=$(digest x64)

cat <<CASK
cask "dataquery" do
  arch arm: "aarch64", intel: "x64"

  version "$version"
  sha256 arm:   "$arm",
         intel: "$intel"

  url "https://github.com/$repo/releases/download/v#{version}/DataQuery_#{version}_#{arch}.dmg"
  name "DataQuery"
  desc "Fast, safe desktop database client"
  homepage "https://dataquery.manikmia.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  # DataQuery updates itself (Settings → About → Updates).
  auto_updates true
  depends_on :macos

  app "DataQuery.app"

  # The app is not notarized yet; without this macOS reports it as damaged.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/DataQuery.app"]
  end

  zap trash: [
    "~/Library/Application Support/dev.dataquery.app",
    "~/Library/Caches/dev.dataquery.app",
    "~/Library/Preferences/dev.dataquery.app.plist",
    "~/Library/Saved Application State/dev.dataquery.app.savedState",
    "~/Library/WebKit/dev.dataquery.app",
  ]
end
CASK
