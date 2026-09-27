fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("../../assets/flow.ico")
            .set("ProductName", "Flow Uninstaller")
            .set("FileDescription", "Flow Uninstaller")
            .compile()
            .expect("uninstaller icon");
    }
}
