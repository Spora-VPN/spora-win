fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_transport(false)
        .compile_protos(
            &["../spora-wincore/proto/spora.proto"],
            &["../spora-wincore/proto"],
        )?;
    slint_build::compile("ui/app.slint")?;
    Ok(())
}
