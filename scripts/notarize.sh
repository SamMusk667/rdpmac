#!/bin/sh
# Notarizes and staples build/rdpmac-VERSION.pkg. The package and the app in it must be signed
# with Developer ID identities (see scripts/build-app.sh). Store your notary credentials once:
#
#   xcrun notarytool store-credentials rdpmac --apple-id you@example.com --team-id TEAMID
#   RDPMAC_NOTARY_PROFILE=rdpmac sh scripts/notarize.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
pkg="$root/build/rdpmac-$version.pkg"

die() {
    echo "notarize: $*" >&2
    exit 1
}

[ -f "$pkg" ] || die "no $pkg; build it: sh scripts/build-app.sh --pkg"
[ -n "${RDPMAC_NOTARY_PROFILE:-}" ] || die "set RDPMAC_NOTARY_PROFILE to a notarytool keychain profile"
pkgutil --check-signature "$pkg" | grep -q "Developer ID Installer" ||
    die "the package is not signed with a Developer ID Installer identity"
xcrun notarytool submit "$pkg" --keychain-profile "$RDPMAC_NOTARY_PROFILE" --wait
xcrun stapler staple "$pkg"
spctl --assess --type install -vv "$pkg"
