#!/bin/sh
# Copies the licence and patent files of the C libraries built into incre-image-compression-server (they come
# inside the -sys crates, so cargo-about doesn't see them) into DIR.
#   tools/c-licenses.sh DIR
set -eu
out=$1
mkdir -p "$out"
dir() {
    cargo metadata --format-version 1 --locked |
        jq -r --arg n "$1" '.packages[] | select(.name == $n) | .manifest_path' |
        head -n 1 | xargs dirname
}
copy() { # NAME FILE...
    name=$1; shift
    for f in "$@"; do
        [ -f "$f" ] || { echo "c-licenses: missing $f" >&2; exit 1; }
        cp "$f" "$out/$name-$(basename "$f")"
    done
}
avif=$(dir libavif-sys); aom=$(dir libaom-sys); dav1d=$(dir libdav1d-sys); jxl=$(dir jpegxl-src)/libjxl
copy libavif "$avif/libavif/LICENSE"
copy libaom "$aom/vendor/LICENSE" "$aom/vendor/PATENTS"
copy dav1d "$dav1d/vendor/COPYING"
copy libjxl "$jxl/LICENSE" "$jxl/PATENTS"
copy highway "$jxl/third_party/highway/LICENSE" "$jxl/third_party/highway/LICENSE-BSD3"
copy brotli "$jxl/third_party/brotli/LICENSE"
copy skcms "$jxl/third_party/skcms/LICENSE"
