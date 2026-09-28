fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile ui/app.slint");

    // Name and version in the exe's file properties (the version comes from Cargo.toml).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "massier87shell");
        res.set("FileDescription", "massier87shell");
        res.set("LegalCopyright", "MIT license");
        if let Err(e) = res.compile() {
            println!("cargo:warning=exe version info skipped: {e}");
        }
    }
}
