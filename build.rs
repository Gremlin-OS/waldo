//! Tells cargo where the static emulator library is.
//!
//! LIBGHOSTTY_VT_LIB_DIR names the directory holding libghostty-vt.a. The
//! flake sets it for the package build and the development shell (see
//! nix/libghostty-vt.nix). Without it, the place scripts/build-vt.sh puts
//! the library is used. See DESIGN.md section 9.2.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_LIB_DIR");
    let dir = match std::env::var("LIBGHOSTTY_VT_LIB_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("target/libghostty-vt/lib"),
    };
    let lib = dir.join("libghostty-vt.a");
    println!("cargo:rerun-if-changed={}", lib.display());
    if !lib.exists() {
        eprintln!(
            "{} does not exist. Build it with 'scripts/build-vt.sh' (needs Zig 0.17), \
             or enter the Nix shell with 'nix develop', or set LIBGHOSTTY_VT_LIB_DIR to \
             the directory that holds libghostty-vt.a.",
            lib.display()
        );
        std::process::exit(1);
    }
    println!("cargo:rustc-link-search=native={}", dir.display());
}
