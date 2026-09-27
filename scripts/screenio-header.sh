#!/bin/sh
# Regenerates crates/screenio/include/screenio.h from the C ABI in crates/screenio/src/lib.rs.
# Needs cbindgen (cargo install cbindgen). Pass --verify to fail when the header was not current;
# cbindgen rewrites it even then.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
cbindgen --config crates/screenio/cbindgen.toml --crate screenio \
    --output crates/screenio/include/screenio.h "$@"
