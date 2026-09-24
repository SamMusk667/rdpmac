#!/bin/sh
# Builds libscreenio and links the C example against the shared library.
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cargo build -p screenio --manifest-path "$root/Cargo.toml"
cc -std=c11 -Wall -I"$root/crates/screenio/include" "$here/screenshot.c" \
   -L"$root/target/debug" -lscreenio -Wl,-rpath,"$root/target/debug" -o "$here/screenshot"
echo "built $here/screenshot"
