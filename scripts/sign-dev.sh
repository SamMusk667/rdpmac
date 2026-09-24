#!/bin/sh
# Code signing for rdpmacd during development.
#
# macOS pins a command-line tool's privacy grants (Screen Recording, Accessibility) to its code
# signature. The linker's ad-hoc signature is a hash of the binary, so every build loses the
# grants. Signing every build with the same certificate gives the binary a stable designated
# requirement, "identifier com.rdpmac.rdpmacd and certificate root = <hash>", and the grants stay.
#
#   sh scripts/sign-dev.sh setup          create the "rdpmac Development" identity, once per Mac
#   sh scripts/sign-dev.sh sign [PATH]    sign PATH, default target/release/rdpmacd; an app bundle
#                                         keeps the identifier from its Info.plist
#   sh scripts/sign-dev.sh status [PATH]  show the identity and PATH's signature
#
# The identity lives in its own keychain, unlocked with a password stored next to it and readable
# only by you, so signing also works over SSH, where the login keychain is locked. The keychain is
# on your search list only while codesign runs. Set RDPMAC_SIGN_IDENTITY to sign with another
# identity from your keychains instead, for example an Apple Development certificate.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
name="rdpmac Development"
identifier="com.rdpmac.rdpmacd"
state="$HOME/Library/Application Support/rdpmac/signing"
keychain="$state/rdpmac-dev.keychain-db"
password_file="$state/keychain-password"
# LibreSSL writes PKCS#12 files in the format the macOS keychain imports.
openssl=/usr/bin/openssl

die() {
    echo "sign-dev: $*" >&2
    exit 1
}

random_password() {
    head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 32
}

setup() {
    if [ -f "$keychain" ]; then
        echo "the '$name' identity already exists in $keychain"
        return
    fi
    mkdir -p "$state"
    chmod 700 "$state"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    cat >"$tmp/request.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
O = rdpmac
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
EOF
    "$openssl" req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$tmp/request.cnf" \
        -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null ||
        die "creating the certificate failed"
    p12_password=$(random_password)
    "$openssl" pkcs12 -export -name "$name" -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
        -out "$tmp/identity.p12" -passout "pass:$p12_password" ||
        die "packaging the identity failed"

    keychain_password=$(random_password)
    (umask 077 && printf '%s' "$keychain_password" >"$password_file")

    # create-keychain adds the new keychain to your search list; put the list back as it was,
    # since signing names this keychain explicitly.
    security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"$//' >"$tmp/search-list"
    security create-keychain -p "$keychain_password" "$keychain"
    set --
    while IFS= read -r k; do
        [ -n "$k" ] && set -- "$@" "$k"
    done <"$tmp/search-list"
    # Never write back an empty list: that would hide the login keychain from every app.
    if [ "$#" -gt 0 ]; then
        security list-keychains -d user -s "$@"
    else
        echo "sign-dev: could not read your keychain search list; left it as create-keychain set it" >&2
    fi

    # No auto-lock timeout: an agent restart at night should not need a password.
    security set-keychain-settings "$keychain"
    security unlock-keychain -p "$keychain_password" "$keychain"
    security import "$tmp/identity.p12" -k "$keychain" -P "$p12_password" -f pkcs12 \
        -T /usr/bin/codesign >/dev/null
    # Lets codesign use the key without a confirmation dialog, which SSH could not show.
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" \
        "$keychain" >/dev/null
    echo "created the '$name' identity in $keychain"
    echo "certificate SHA-1: $(fingerprint)"
}

fingerprint() {
    security find-certificate -c "$name" -Z "$keychain" 2>/dev/null | sed -n 's/^SHA-1 hash: //p'
}

search_list() {
    security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"$//'
}

# Never writes an empty list: that would hide the login keychain from every app.
set_search_list() {
    if [ "$#" -eq 0 ]; then
        echo "sign-dev: not emptying your keychain search list" >&2
        return 1
    fi
    security list-keychains -d user -s "$@"
}

list_keychain() {
    set -- "$keychain"
    while IFS= read -r k; do
        if [ -n "$k" ]; then
            set -- "$@" "$k"
        fi
    done <<EOF
$(search_list)
EOF
    # Only our keychain would be left: the list could not be read.
    [ "$#" -gt 1 ] || die "could not read your keychain search list"
    set_search_list "$@"
}

# Takes out only our keychain, so changes other programs made meanwhile survive.
unlist_keychain() {
    set --
    while IFS= read -r k; do
        if [ -n "$k" ] && [ "$k" != "$keychain" ]; then
            set -- "$@" "$k"
        fi
    done <<EOF
$(search_list)
EOF
    set_search_list "$@"
}

sign() {
    bin="${1:-$root/target/release/rdpmacd}"
    [ -e "$bin" ] || die "no such file: $bin (build it first)"
    # A bare executable gets rdpmacd's identifier; a bundle's comes from its Info.plist.
    if [ -d "$bin" ]; then
        set -- --timestamp=none
    else
        set -- --identifier "$identifier" --timestamp=none
    fi
    if [ -n "${RDPMAC_SIGN_IDENTITY:-}" ]; then
        codesign --force --sign "$RDPMAC_SIGN_IDENTITY" "$@" "$bin"
    else
        [ -f "$keychain" ] || die "no signing identity yet; run: sh scripts/sign-dev.sh setup"
        hash=$(fingerprint)
        [ -n "$hash" ] || die "no '$name' certificate in $keychain"
        security unlock-keychain -p "$(cat "$password_file")" "$keychain"
        # codesign finds identities only in keychains on the search list, --keychain or not.
        # Listing ours for good would make apps that search every keychain ask to unlock it.
        if ! search_list | grep -qxF "$keychain"; then
            trap unlist_keychain EXIT
            trap 'exit 130' INT HUP TERM
            list_keychain
        fi
        codesign --force --keychain "$keychain" --sign "$hash" "$@" "$bin"
    fi
    codesign --verify --strict "$bin"
    echo "signed $bin"
    requirement "$bin"
}

requirement() {
    codesign -d -r- "$1" 2>&1 | sed -n 's/^\(# \)\{0,1\}designated => /designated requirement: /p'
}

status() {
    if [ -f "$keychain" ]; then
        echo "identity: '$name' in $keychain"
        echo "certificate SHA-1: $(fingerprint)"
    else
        echo "identity: none, run: sh scripts/sign-dev.sh setup"
    fi
    bin="${1:-$root/target/release/rdpmacd}"
    if [ -e "$bin" ]; then
        codesign -dv --verbose=2 "$bin" 2>&1 | grep -E '^(Identifier|Authority|Signature)='
        requirement "$bin"
    fi
}

case "${1:-}" in
setup) setup ;;
sign)
    shift
    sign "$@"
    ;;
status)
    shift
    status "$@"
    ;;
*)
    sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
