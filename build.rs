// Embeds the application icon and Windows version-info resource
// (product name, version, description) into the compiled .exe when
// targeting Windows. No-op on other platforms so `cargo build`/`cargo
// check` on Linux/macOS dev machines are unaffected.

fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("resources/icon.ico");
        res.set("ProductName", "HyperTerm");
        res.set("FileDescription", "HyperTerm - High-performance SSH terminal");
        res.set("LegalCopyright", "Copyright (c) Siro");
        res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, cargo_version_u64());
        if let Err(e) = res.compile() {
            // Don't fail the whole build over a missing rc.exe / resource
            // compiler on unusual toolchains -- log and continue so
            // contributors on GNU-toolchain Windows still get a working
            // (if unbranded) binary.
            println!("cargo:warning=failed to embed Windows resources: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn cargo_version_u64() -> u64 {
    let major: u64 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0);
    let minor: u64 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(1);
    let patch: u64 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0);
    (major << 48) | (minor << 32) | (patch << 16)
}
