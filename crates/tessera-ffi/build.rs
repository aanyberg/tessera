//! Generates `TesseraCore.h` from the public `extern "C"` surface.

fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out = std::path::Path::new(&crate_dir).join("../../target/include/TesseraCore.h");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create include dir");
    }
    match cbindgen::generate(&crate_dir) {
        Ok(bindings) => {
            bindings.write_to_file(&out);
        }
        Err(cbindgen::Error::ParseSyntaxError { .. }) => {
            // rust-analyzer / partial builds; let rustc report the real error.
        }
        Err(e) => panic!("cbindgen failed: {e}"),
    }
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
