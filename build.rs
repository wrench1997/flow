fn main() {
    println!("cargo:rerun-if-changed=tools/uninstaller");
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("uninstaller");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        std::fs::create_dir_all(&output).unwrap();
        let placeholder = output.join("windows-only");
        std::fs::write(&placeholder, []).unwrap();
        println!(
            "cargo:rustc-env=FLOW_UNINSTALLER_PATH={}",
            placeholder.display()
        );
        return;
    }
    let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
        .args(["build", "--release", "--locked", "--manifest-path"])
        .arg(root.join("tools/uninstaller/Cargo.toml"))
        .arg("--target-dir")
        .arg(&output)
        .status()
        .expect("build independent uninstaller");
    assert!(status.success(), "independent uninstaller build failed");
    let exe = output.join("release").join(if cfg!(windows) {
        "flow-uninstaller.exe"
    } else {
        "flow-uninstaller"
    });
    println!("cargo:rustc-env=FLOW_UNINSTALLER_PATH={}", exe.display());
    println!("cargo:rerun-if-changed=assets/flow.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/flow.ico")
            .set("ProductName", "Flow 下载工作台")
            .set(
                "FileDescription",
                "Flow · Download Manager and Media Player",
            )
            .compile()
            .expect("compile Windows icon resource");
    }
}
