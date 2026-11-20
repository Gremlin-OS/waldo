# libghostty-vt, built as a static library from the tarballs in
# vendor/libghostty-vt. See DESIGN.md section 9.2.
#
# ReleaseSafe keeps the library's safety checks: a bug in it stops the
# session with a message instead of corrupting memory.
#
# SIMD stays on, although speed does not matter here. With simd=false the
# 1.0.1 library is built without libc and exports weak stub `calloc`
# (returns null) and `free` (does nothing) from pkg/wuffs/src/main.zig. In a
# static link against a shared glibc those stubs win, and the whole program
# loses its allocator. With SIMD on, the library links libc and has no
# stubs. Zig compiles the bundled simdutf and Highway itself, in their
# no-libc++ mode, so nothing is added to the build or to run time.
{
  lib,
  stdenv,
  zig_0_17,
}:
let
  version = "1.0.1";
  dir = ../vendor/libghostty-vt;
in
stdenv.mkDerivation {
  pname = "libghostty-vt-static";
  inherit version;

  srcs = [
    (dir + "/libghostty-vt-${version}.tar.gz")
    (dir + "/libghostty-vt-${version}-deps.tar.gz")
  ];
  sourceRoot = "libghostty-vt-${version}";

  nativeBuildInputs = [ zig_0_17 ];

  dontSetZigDefaultFlags = true;
  zigBuildFlags = [
    "--system"
    "../libghostty-vt-${version}-deps"
    "-Dcpu=baseline"
    "-Doptimize=ReleaseSafe"
    "-Dsimd=true"
  ];

  # Only the static library and the headers are wanted. Removing the shared
  # library makes it impossible to link it by accident.
  postInstall = ''
    rm -f $out/lib/libghostty-vt.so*
    rm -f $out/lib/pkgconfig/libghostty-vt.pc
    rm -rf $out/lib/cmake
  '';

  meta = {
    homepage = "https://ghostty.org";
    license = lib.licenses.mit;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
