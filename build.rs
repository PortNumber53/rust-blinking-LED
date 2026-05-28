//! Build script: places `memory.x` (the RP2350 memory layout + boot blocks)
//! on the linker search path and passes the required linker arguments.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    // Put `memory.x` in the output directory and add it to the linker search path.
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");

    // Linker arguments for cortex-m-rt + the RP2350 boot blocks defined in memory.x.
    // (No -Tdefmt.x here: this project uses panic-halt, not defmt.)
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
}
