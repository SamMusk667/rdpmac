#!/bin/sh
# Signs target/<profile>/rdpmacd so macOS privacy permissions survive rebuilds.
#
# TCC identifies a bare executable by its code signature. Ad-hoc signatures change on every build,
# so a granted permission is lost the next time the daemon is rebuilt. Signing with a certificate
# (a Developer ID, an Apple Development certificate, or a self-made code-signing certificate
# trusted in the login keychain) keeps the designated requirement stable.
#
#   RDPMAC_SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" sh scripts/sign-dev.sh [release]
#
# Without RDPMAC_SIGN_IDENTITY the script falls back to an ad-hoc signature, which is enough to
# get the prompts but not to keep the grant across builds.
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
profile="${1:-debug}"
bin="$root/target/$profile/rdpmacd"
identity="${RDPMAC_SIGN_IDENTITY:--}"
[ -x "$bin" ] || { echo "build first: cargo build ${1:+--$1}"; exit 1; }
codesign --force --sign "$identity" --identifier com.rdpmac.rdpmacd \
  --options runtime --timestamp=none "$bin"
codesign --display --verbose=2 "$bin" 2>&1 | grep -E 'Identifier|Authority|Signature' || true
