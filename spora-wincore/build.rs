fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_client(false)
        .compile_protos(&["proto/spora.proto"], &["proto"])?;

    // Version resource (FileVersion/ProductVersion from the workspace
    // version) so Explorer's Details tab and Task Manager name the service
    // binary and its release. The gnu target needs
    // `x86_64-w64-mingw32-windres` on PATH.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "Spora")
            .set("FileDescription", "Spora VPN Service")
            .set("LegalCopyright", "Copyright (C) 2026 The Spora authors. GPL-3.0.")
            .compile()?;
    }
    Ok(())
}
