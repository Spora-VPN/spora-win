fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_transport(false)
        .compile_protos(
            &["../spora-wincore/proto/spora.proto"],
            &["../spora-wincore/proto"],
        )?;
    slint_build::compile("ui/app.slint")?;

    // The application icon (the spore mark, square, centered on its circle)
    // for Explorer, the taskbar and shortcuts, plus the version resource
    // winresource fills from the workspace version. Only meaningful in a
    // Windows build; the gnu target needs `x86_64-w64-mingw32-windres` on PATH.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=../assets/spora.ico");
        winresource::WindowsResource::new()
            .set_icon("../assets/spora.ico")
            .set("ProductName", "Spora")
            .set("FileDescription", "Spora")
            .compile()?;
    }
    Ok(())
}
