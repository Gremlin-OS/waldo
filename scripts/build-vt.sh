#!/usr/bin/env bash
# Build the terminal emulator library, libghostty-vt, without Nix.
#
#   scripts/build-vt.sh [OUT]
#
# It builds the static library from the tarballs in vendor/libghostty-vt
# and puts libghostty-vt.a in OUT/lib (default: target/libghostty-vt, where
# build.rs looks when LIBGHOSTTY_VT_LIB_DIR is not set). It needs Zig 0.17,
# tar and sha256sum, and no network.
#
# The flags are the ones nix/libghostty-vt.nix uses; the reasons are there.
# Do not turn SIMD off: without it the 1.0.1 library replaces the program's
# calloc and free with stubs.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/target/libghostty-vt}
version=1.0.1
vendor=$root/vendor/libghostty-vt

command -v zig >/dev/null || { echo "build-vt.sh: zig is not on PATH; Zig 0.17 is needed (https://ziglang.org/download/)" >&2; exit 1; }
case $(zig version) in
  0.17.*) ;;
  *) echo "build-vt.sh: Zig 0.17 is needed, found $(zig version)" >&2; exit 1 ;;
esac

(cd "$vendor" && sha256sum --check --ignore-missing --quiet SHA256SUMS) ||
  { echo "build-vt.sh: the tarballs in $vendor do not match SHA256SUMS" >&2; exit 1; }

mkdir -p "$root/target" "$out/lib"
work=$(mktemp -d "$root/target/vt-build.XXXXXX")
tar xzf "$vendor/libghostty-vt-$version.tar.gz" -C "$work"
tar xzf "$vendor/libghostty-vt-$version-deps.tar.gz" -C "$work"

cd "$work/libghostty-vt-$version"
ZIG_GLOBAL_CACHE_DIR=$work/zig-cache zig build install \
  --prefix "$work/prefix" \
  --system "../libghostty-vt-$version-deps" \
  -Dcpu=baseline \
  -Doptimize=ReleaseSafe \
  -Dsimd=true

# Only the static library is wanted; the shared one stays behind so that it
# cannot be linked by accident.
cp "$work/prefix/lib/libghostty-vt.a" "$out/lib/"
echo "built $out/lib/libghostty-vt.a"
echo "the build directory $work can be deleted"
