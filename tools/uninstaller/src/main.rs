#![cfg_attr(not(test), windows_subsystem = "windows")]
use std::{
    path::{Path, PathBuf},
    process::Command,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, flags: u32) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn WaitForSingleObject(handle: isize, timeout: u32) -> u32;
    fn CloseHandle(handle: isize) -> i32;
}
fn english(root: &Path) -> bool {
    match std::fs::read(root.join("data/ui-language.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<String>(&b).ok())
        .as_deref()
    {
        Some("en") => true,
        Some("zh-CN") => false,
        _ => unsafe { GetUserDefaultUILanguage() & 0x3ff != 4 },
    }
}
fn dialog(text: &str, flags: u32) -> i32 {
    let title = if text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
        "卸载 Flow"
    } else {
        "Flow Uninstaller"
    };
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(0, text.as_ptr(), title.as_ptr(), flags) }
}
fn validated_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() || root.parent().is_none() {
        return Err("Invalid installation directory".into());
    }
    let root = dunce::canonicalize(root)?;
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("flow-install.json"))?)?;
    if marker["product"] != "Flow" {
        return Err("Flow installation record not found".into());
    }
    Ok(root)
}
fn lock(root: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(root.join("data/engine.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}
fn remove_program_files(root: &Path) -> Result<()> {
    for name in ["Flow.exe", "Flow-Uninstall.exe", "flow-install.json"] {
        match std::fs::remove_file(root.join(name)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn hidden(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x08000000);
}
fn remove_registration(root: &Path) -> Result<()> {
    // Paths are passed as data. Remove only entries targeting this installation.
    let script = r#"
$ErrorActionPreference='Stop'; [Console]::OutputEncoding=New-Object System.Text.UTF8Encoding;
$root=$env:FLOW_UNINSTALL_ROOT; $shell=New-Object -ComObject WScript.Shell;
$entries=@(@{Path=[IO.Path]::Combine([Environment]::GetFolderPath('Programs'),'Flow.lnk');Exe='Flow.exe'},@{Path=[IO.Path]::Combine([Environment]::GetFolderPath('Programs'),'Uninstall Flow.lnk');Exe='Flow-Uninstall.exe'},@{Path=[IO.Path]::Combine([Environment]::GetFolderPath('Desktop'),'Flow.lnk');Exe='Flow.exe'});
foreach($entry in $entries){if(Test-Path -LiteralPath $entry.Path){$link=$shell.CreateShortcut($entry.Path);if($link.TargetPath -eq [IO.Path]::Combine($root,$entry.Exe)){Remove-Item -LiteralPath $entry.Path}}}
$key='Software\Microsoft\Windows\CurrentVersion\Uninstall\Flow'; $k=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($key);
if($k){$p=$k.GetValue('InstallLocation');$k.Dispose();if($p -eq $root){[Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($key,$false)}}
"#;
    let exe = PathBuf::from(std::env::var_os("SystemRoot").ok_or("Windows directory unavailable")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut cmd = Command::new(exe);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("FLOW_UNINSTALL_ROOT", root);
    hidden(&mut cmd);
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(())
}
fn wait_for_parent(pid: u32) -> Result<()> {
    unsafe {
        let handle = OpenProcess(0x00100000, 0, pid);
        if handle != 0 {
            let result = WaitForSingleObject(handle, 60000);
            CloseHandle(handle);
            if result != 0 {
                return Err("Timed out waiting for uninstaller to close".into());
            }
        }
    }
    Ok(())
}
fn finish(root: &Path, pid: u32) -> Result<()> {
    let root = validated_root(root)?;
    wait_for_parent(pid)?;
    let _lock = lock(&root)?;
    remove_registration(&root)?;
    remove_program_files(&root)?;
    Ok(())
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--finish-uninstall") {
        let root = Path::new(args.get(2).ok_or("Missing directory")?);
        let pid = args
            .get(3)
            .ok_or("Missing process")?
            .to_string_lossy()
            .parse()?;
        finish(root, pid)?;
        // Test helpers can run without opening a native dialog.
        if !args.get(4).is_some_and(|a| a == "--silent") {
            dialog(
                if english(root) {
                    "Flow removed. Downloads, settings and player runtime were kept."
                } else {
                    "Flow 已卸载，下载文件、个人配置和播放器环境已保留。"
                },
                0x40,
            );
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let root = validated_root(exe.parent().ok_or("Missing directory")?)?;
    let en = english(&root);
    if dialog(
        if en {
            "Uninstall Flow? Downloads, settings and player runtime will be kept. Exit Flow from the tray first."
        } else {
            "确定卸载 Flow？将保留下载文件、个人配置和播放器环境。请先从托盘完全退出 Flow。"
        },
        0x24,
    ) != 6
    {
        return Ok(());
    }
    let _lock = lock(&root).map_err(|_| {
        if en {
            "Flow is still running. Exit it from the tray and retry."
        } else {
            "Flow 仍在运行，请从托盘完全退出后重试。"
        }
    })?;
    let helper_dir = std::env::temp_dir().join(format!(
        "flow-uninstall-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&helper_dir)?;
    let helper = helper_dir.join("Flow-Uninstall.exe");
    std::fs::copy(exe, &helper)?;
    let mut cmd = Command::new(helper);
    cmd.arg("--finish-uninstall")
        .arg(root)
        .arg(std::process::id().to_string());
    hidden(&mut cmd);
    cmd.spawn()?;
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        dialog(&e.to_string(), 0x10);
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_program_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["data", "runtime", "downloads"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
            std::fs::write(dir.path().join(sub).join("keep"), b"keep").unwrap();
        }
        for name in ["Flow.exe", "Flow-Uninstall.exe"] {
            std::fs::write(dir.path().join(name), b"program").unwrap();
        }
        std::fs::write(
            dir.path().join("flow-install.json"),
            br#"{"product":"Flow"}"#,
        )
        .unwrap();
        let root = validated_root(dir.path()).unwrap();
        remove_program_files(&root).unwrap();
        for sub in ["data", "runtime", "downloads"] {
            assert!(root.join(sub).join("keep").is_file());
        }
        assert!(!root.join("Flow.exe").exists());
        assert!(!root.join("Flow-Uninstall.exe").exists());
        assert!(!root.join("flow-install.json").exists());
        assert!(validated_root(&root).is_err());
    }
    #[test]
    fn refuses_unmarked_and_wrong_product_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validated_root(dir.path()).is_err());
        std::fs::write(
            dir.path().join("flow-install.json"),
            br#"{"product":"Other"}"#,
        )
        .unwrap();
        assert!(validated_root(dir.path()).is_err());
        assert!(validated_root(Path::new("relative")).is_err());
    }
}
