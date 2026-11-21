#!/bin/sh
# Regenerates crates/tuner/src/it9175/data_*.rs and crates/tuner/data/fw_*.bin from an OpenDTV checkout.
# Usage: tools/gen-tables/gen.sh [path/to/OpenDTV]   (default: ../OpenDTV next to this repo). Needs a JDK (11+).
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
opendtv=${1:-$root/../OpenDTV}
src=$opendtv/opendtv/app/src/main/java
out=$(mktemp -d)
javac -d "$out" -sourcepath "$src" \
  "$src/ph/opendtv/source/It9175Profile.java" \
  "$here/ph/opendtv/source/GenRustTables.java"
java -cp "$out" ph.opendtv.source.GenRustTables "$root/crates/tuner/src/it9175" "$root/crates/tuner/data"
rm -rf "$out"
echo "generated from $opendtv ($(git -C "$opendtv" rev-parse --short HEAD 2>/dev/null || echo 'no git'))"
