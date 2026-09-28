#!/bin/sh
# Builds rdpmac.app, the menu-bar app with rdpmacd inside, and optionally its installer.
#
#   sh scripts/build-app.sh          build/rdpmac.app
#   sh scripts/build-app.sh --pkg    also build/rdpmac-VERSION.pkg, which installs to /Applications
#
# VERSION is the one rdpmacd reports, such as 0.4.0-dev55 between releases.
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
# The version rdpmacd was built as, 0.4.0-dev55 between releases, and the number of commits it is
# built from, which is the bundle version: every later build counts higher.
full=$("$root/target/release/rdpmacd" --version | sed 's/^rdpmacd //')
build=$(git -C "$root" rev-list --count HEAD 2>/dev/null) || build=$version
(cd "$root/app" && swift build -c release)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Library/LaunchAgents"
cp "$root/app/.build/release/rdpmac" "$app/Contents/MacOS/rdpmac"
# The app icon and the menu-bar templates come from the designs in app/Icons.
icons="$out/icons"
rm -rf "$icons"
swift "$root/scripts/icons.swift" "$root/app/Icons" "$icons"
iconutil -c icns "$icons/AppIcon.iconset" -o "$app/Contents/Resources/AppIcon.icns"
cp "$icons"/*Template.pdf "$app/Contents/Resources/"
rm -rf "$icons"
cp "$root/target/release/rdpmacd" "$app/Contents/MacOS/rdpmacd"
sed -e "s/@VERSION@/$version/g" -e "s/@BUILD@/$build/g" -e "s/@FULL_VERSION@/$full/g" \
    "$root/app/Bundle/Info.plist" >"$app/Contents/Info.plist"
cp "$root/app/Bundle/com.rdpmac.rdpmacd.plist" "$app/Contents/Library/LaunchAgents/"
plutil -lint -s "$app/Contents/Info.plist" "$app/Contents/Library/LaunchAgents/com.rdpmac.rdpmacd.plist"
# Build tools leave attributes such as com.apple.provenance; they do not belong in a release.
xattr -cr "$app"

# Nested code first: the app's signature seals the daemon's.
sign "$app/Contents/MacOS/rdpmacd" com.rdpmac.rdpmacd
sign "$app"
codesign --verify --deep --strict "$app"
echo "built $app ($full)"
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
pkg="$out/rdpmac-$full.pkg"
if [ -n "${RDPMAC_INSTALLER_IDENTITY:-}" ]; then
    productbuild --package "$out/rdpmac-component.pkg" --sign "$RDPMAC_INSTALLER_IDENTITY" "$pkg" >/dev/null
else
    productbuild --package "$out/rdpmac-component.pkg" "$pkg" >/dev/null
fi
rm -rf "$staging" "$out/component.plist" "$out/rdpmac-component.pkg"
echo "built $pkg"
