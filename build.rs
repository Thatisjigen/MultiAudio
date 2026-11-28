// Embeds the icon and version info in the Windows executable.
fn main() {
    println!("cargo:rerun-if-changed=assets/multiaudio.ico");
    // The tests are not in the repository: each module takes them from the
    // folder src/local_tests, where there is one (see the end of the files
    // in src/). Watching src is what makes the folder noticed when it comes
    // or goes.
    println!("cargo:rustc-check-cfg=cfg(local_tests)");
    println!("cargo:rerun-if-changed=src");
    if std::path::Path::new("src/local_tests").is_dir() {
        println!("cargo:rustc-cfg=local_tests");
    }
    // Only when building for Windows on Windows (needs the Windows SDK's rc.exe).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") && cfg!(windows) {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/multiaudio.ico");
        res.set("FileDescription", "MultiAudio");
        res.set("ProductName", "MultiAudio");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icona Windows non incorporata: {e}");
        }
    }
}
