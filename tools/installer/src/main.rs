#![windows_subsystem = "windows"]
#[path = "../../../src/i18n.rs"]
mod i18n;
#[path = "../../../src/installer.rs"]
mod installer;
mod updater {
    pub fn wait_for_process(pid: u32) -> anyhow::Result<()> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn WaitForSingleObject(handle: isize, timeout: u32) -> u32;
            fn CloseHandle(handle: isize) -> i32;
        }
        unsafe {
            let handle = OpenProcess(0x00100000, 0, pid);
            if handle != 0 {
                let result = WaitForSingleObject(handle, 60000);
                CloseHandle(handle);
                anyhow::ensure!(result == 0, "Timed out waiting for process");
            }
        }
        Ok(())
    }
}
fn main() -> eframe::Result {
    if let Some(root) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_owned()))
    {
        i18n::initialize(&root);
    }
    installer::run(vec!["--install".into()])
}
