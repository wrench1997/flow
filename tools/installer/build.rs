fn main() {
    for name in ["FLOW_MAIN_PAYLOAD", "FLOW_UNINSTALLER_PATH"] {
        println!("cargo:rerun-if-env-changed={name}");
        let path =
            std::env::var(name).expect("Build the main app and uninstaller before the installer");
        println!("cargo:rerun-if-changed={path}");
        println!("cargo:rustc-env={name}={path}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("../../assets/flow.ico")
            .set("ProductName", "Flow Setup")
            .set("FileDescription", "Flow Installer")
            .compile()
            .expect("installer icon");
    }
}
