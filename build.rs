//! Tells cargo where the static emulator library is.
//!
//! The flake sets LIBGHOSTTY_VT_LIB_DIR for both the package build and the
//! development shell. See nix/libghostty-vt.nix and DESIGN.md section 9.2.

fn main() {
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_LIB_DIR");
    match std::env::var("LIBGHOSTTY_VT_LIB_DIR") {
        Ok(dir) => println!("cargo:rustc-link-search=native={dir}"),
        Err(_) => {
            eprintln!(
                "LIBGHOSTTY_VT_LIB_DIR is not set. It must name the directory holding \
                 libghostty-vt.a. Enter the development shell with 'nix develop', which sets it."
            );
            std::process::exit(1);
        }
    }
}
