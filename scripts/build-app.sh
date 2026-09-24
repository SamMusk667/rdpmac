#!/bin/sh
# Builds rdpmac.app, the menu-bar app with rdpmacd inside, and optionally its installer.
#
#   sh scripts/build-app.sh          build/rdpmac.app
#   sh scripts/build-app.sh --pkg    also build/rdpmac-VERSION.pkg, which installs to /Applications
#
# Code is signed inside out with the "rdpmac Development" identity from scripts/sign-dev.sh.
# For distribution set RDPMAC_SIGN_IDENTITY to a "Developer ID Application" identity, which also
# turns on the hardened runtime and a secure timestamp, and RDPMAC_INSTALLER_IDENTITY to a
# "Developer ID Installer" identity for the package; then run scripts/notarize.sh.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/build"
app="$out/rdpmac.app"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)

die() {
    echo "build-app: $*" >&2
    exit 1
}

# Signs $1; a bare executable gets the identifier in $2.
sign() {
    if [ -n "${RDPMAC_SIGN_IDENTITY:-}" ]; then
        if [ -n "${2:-}" ]; then
            set -- "$1" --identifier "$2"
        else
            set -- "$1"
        fi
        target=$1
        shift
        codesign --force --options runtime --timestamp --sign "$RDPMAC_SIGN_IDENTITY" "$@" "$target"
    else
        sh "$root/scripts/sign-dev.sh" sign "$1" >/dev/null
    fi
}

[ -n "$version" ] || die "no version in Cargo.toml"
cargo build --release -p rdpmacd --manifest-path "$root/Cargo.toml"
(cd "$root/app" && swift build -c release)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
cp "$root/app/.build/release/rdpmac" "$app/Contents/MacOS/rdpmac"
cp "$root/target/release/rdpmacd" "$app/Contents/MacOS/rdpmacd"
sed "s/@VERSION@/$version/g" "$root/app/Bundle/Info.plist" >"$app/Contents/Info.plist"
cp "$root/app/Bundle/com.rdpmac.rdpmacd.plist" "$app/Contents/Library/LaunchAgents/"
plutil -lint -s "$app/Contents/Info.plist" "$app/Contents/Library/LaunchAgents/com.rdpmac.rdpmacd.plist"
# Build tools leave attributes such as com.apple.provenance; they do not belong in a release.
xattr -cr "$app"

# Nested code first: the app's signature seals the daemon's.
sign "$app/Contents/MacOS/rdpmacd" com.rdpmac.rdpmacd
sign "$app"
codesign --verify --deep --strict "$app"
echo "built $app ($version)"
codesign -d -r- "$app/Contents/MacOS/rdpmacd" 2>&1 | sed -n 's/^\(# \)\{0,1\}designated => /  rdpmacd: /p'
codesign -d -r- "$app" 2>&1 | sed -n 's/^\(# \)\{0,1\}designated => /  app: /p'

[ "${1:-}" = "--pkg" ] || exit 0

staging="$out/pkg-root"
rm -rf "$staging"
mkdir -p "$staging"
cp -R "$app" "$staging/"
# Install where the package says, even if an older copy was moved elsewhere.
pkgbuild --analyze --root "$staging" "$out/component.plist" >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO "$out/component.plist"
pkgbuild --root "$staging" --component-plist "$out/component.plist" --install-location /Applications \
    --scripts "$root/app/Installer" --identifier com.rdpmac.pkg --version "$version" \
    "$out/rdpmac-component.pkg" >/dev/null
pkg="$out/rdpmac-$version.pkg"
if [ -n "${RDPMAC_INSTALLER_IDENTITY:-}" ]; then
    productbuild --package "$out/rdpmac-component.pkg" --sign "$RDPMAC_INSTALLER_IDENTITY" "$pkg" >/dev/null
else
    productbuild --package "$out/rdpmac-component.pkg" "$pkg" >/dev/null
fi
rm -rf "$staging" "$out/component.plist" "$out/rdpmac-component.pkg"
echo "built $pkg"
