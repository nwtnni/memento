use std::path::PathBuf;
use std::process::Command;

fn build_ralloc() {
    let ralloc = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("Expected cargo to pass CARGO_MANIFEST_DIR")
        .join("ext/ralloc/test");

    let ralloc = ralloc
        .canonicalize()
        .unwrap_or_else(|_| panic!("Failed to find {ralloc:?}"));

    // Build Ralloc
    Command::new("make")
        .args(["clean"])
        .current_dir(&ralloc)
        .status()
        .expect("failed to make clean!");
    let args = {
        #[cfg(not(feature = "no_persist"))]
        {
            &["libralloc.a"]
        }
        #[cfg(feature = "no_persist")]
        {
            &["libralloc.a", "FEATURE=no_persist"]
        }
    };
    Command::new("make")
        .args(args)
        .current_dir(&ralloc)
        .status()
        .expect("failed to make!");

    // Link libralloc.a
    println!("cargo:rustc-link-search={}", ralloc.display());
    println!("cargo:rustc-link-lib=ralloc");
    println!("cargo:rustc-link-lib=numa");
}

fn main() {
    build_ralloc();
}
