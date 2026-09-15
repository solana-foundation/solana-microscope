use std::{env, path::PathBuf};

fn main() {
    let cpi_event =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets the manifest dir"))
            .join("../program-decoder/src/instructions/cpi_event.rs");

    println!("cargo::rerun-if-changed={}", cpi_event.display());
    println!("cargo::rustc-check-cfg=cfg(program_events)");
    if cpi_event.is_file() {
        println!("cargo::rustc-cfg=program_events");
    }
}
