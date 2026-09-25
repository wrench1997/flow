fn main() {
    println!("cargo:rerun-if-changed=assets/flow.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/flow.ico")
            .set("ProductName", "Flow 下载工作台")
            .set("FileDescription", "Flow · Rust BitTorrent Download Manager")
            .compile()
            .expect("compile Windows icon resource");
    }
}
