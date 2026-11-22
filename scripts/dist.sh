#!/usr/bin/env bash
# Pack the release binaries into a tarball.
#
#   scripts/dist.sh [OUTDIR]
#
# Run it after scripts/build-vt.sh and 'cargo build --release --locked'. It
# writes waldo-VERSION-ARCH-linux.tar.gz and a .sha256 file next to it in
# OUTDIR (default: target/dist). The tarball holds the two binaries, the
# README, PROTOCOL.md, and the licences of everything linked into them.
#
# The binaries need the C library they were built against or a newer one,
# so build on the oldest system you mean to support.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
outdir=${1:-$root/target/dist}
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
name=waldo-$version-$(uname -m)-linux
vt=1.0.1
vendor=$root/vendor/libghostty-vt

for b in waldo-wrap waldo-term; do
  [ -x "$root/target/release/$b" ] || { echo "dist.sh: target/release/$b is missing; run 'cargo build --release --locked' first" >&2; exit 1; }
done
[ "$("$root/target/release/waldo-term" --version | awk '{print $NF}')" = "$version" ] ||
  { echo "dist.sh: target/release/waldo-term is not version $version; rebuild" >&2; exit 1; }

mkdir -p "$root/target" "$outdir"
work=$(mktemp -d "$root/target/dist-build.XXXXXX")
d=$work/$name
mkdir -p "$d/bin" "$d/licenses"
cp "$root/target/release/waldo-wrap" "$root/target/release/waldo-term" "$d/bin/"
cp "$root/README.md" "$root/PROTOCOL.md" "$d/"
cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$d/"

# The emulator library and the packages its build takes in.
mkdir -p "$d/licenses/libghostty-vt"
tar xzf "$vendor/libghostty-vt-$vt.tar.gz" -C "$work" "libghostty-vt-$vt/LICENSE"
cp "$work/libghostty-vt-$vt/LICENSE" "$d/licenses/libghostty-vt/"
tar xzf "$vendor/libghostty-vt-$vt-deps.tar.gz" -C "$work" --wildcards '*/LICENSE*'
for dir in "$work/libghostty-vt-$vt-deps"/*/; do
  dep=$(basename "$dir")
  mkdir -p "$d/licenses/libghostty-vt/$dep"
  cp "$dir"LICENSE* "$d/licenses/libghostty-vt/$dep/"
done

# The Rust crates in the binaries: the ones cargo tree lists, not every
# crate the lock file names.
linked=$(cd "$root" && cargo tree --locked --prefix none -e normal | awk '{print $1 "-" substr($2, 2)}' | sort -u)
(cd "$root" && cargo metadata --format-version 1 --locked) |
  grep -o '"manifest_path":"[^"]*"' | cut -d'"' -f4 | sort -u |
  while read -r manifest; do
    src=$(dirname "$manifest")
    [ "$src" != "$root" ] || continue
    crate=$(basename "$src")
    echo "$linked" | grep -qxF "$crate" || continue
    mkdir -p "$d/licenses/$crate"
    found=no
    for f in "$src"/LICENSE* "$src"/LICENCE* "$src"/COPYING* "$src"/NOTICE*; do
      [ -f "$f" ] && { cp "$f" "$d/licenses/$crate/"; found=yes; }
    done
    [ $found = yes ] || { echo "dist.sh: no licence file found for $crate" >&2; exit 1; }
  done

tar czf "$outdir/$name.tar.gz" -C "$work" --owner=0 --group=0 --numeric-owner "$name"
(cd "$outdir" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "wrote $outdir/$name.tar.gz"
echo "the build directory $work can be deleted"
