fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/clip-ai.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/clip-ai.ico");
        res.set_manifest_file("app.manifest");
        res.set("ProductName", "clip-ai");
        res.set(
            "FileDescription",
            "clip-ai - Headless Windows Clipboard & Selection AI Assistant",
        );
        res.set("LegalCopyright", "Copyright (c) 2026 AsukaVoV");
        res.compile().expect("failed to compile Windows resources");
    }
}
