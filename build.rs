use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let manifest_path: PathBuf = PathBuf::from(manifest_dir).join("app.manifest");
        if manifest_path.exists() {
            println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
                manifest_path.display()
            );
        }
    }
}
