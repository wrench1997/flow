//! Shell requests are queued locally, never evaluated as commands or auto-downloaded.
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

/// Register per-user shell handlers without modifying protected Windows UserChoice.
pub fn register_associations() -> Result<()> {
    let exe = std::env::current_exe()?;
    let command = format!("\"{}\" --open \"%1\"", exe.display());
    let icon = format!("\"{}\",0", exe.display());
    let values = [
        (r"Software\Classes\Flow.Torrent", "", "Flow Torrent"),
        (
            r"Software\Classes\Flow.Torrent\shell\open\command",
            "",
            command.as_str(),
        ),
        (
            r"Software\Classes\Flow.Torrent\DefaultIcon",
            "",
            icon.as_str(),
        ),
        (r"Software\Classes\.torrent", "", "Flow.Torrent"),
        (
            r"Software\Classes\.torrent\OpenWithProgids",
            "Flow.Torrent",
            "",
        ),
        (r"Software\Flow\Capabilities", "ApplicationName", "Flow"),
        (
            r"Software\Flow\Capabilities",
            "ApplicationDescription",
            "Flow torrent download manager",
        ),
        (
            r"Software\Flow\Capabilities\FileAssociations",
            ".torrent",
            "Flow.Torrent",
        ),
        (
            r"Software\RegisteredApplications",
            "Flow",
            r"Software\Flow\Capabilities",
        ),
    ];
    let reg = PathBuf::from(std::env::var_os("SystemRoot").context("找不到 Windows 系统目录")?)
        .join("System32/reg.exe");
    for (key, name, value) in values {
        let mut cmd = std::process::Command::new(&reg);
        cmd.args(["add", &format!("HKCU\\{key}")]);
        if name.is_empty() {
            cmd.arg("/ve");
        } else {
            cmd.args(["/v", name]);
        }
        cmd.args(["/t", "REG_SZ", "/d", value, "/f"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let output = cmd.output()?;
        ensure!(
            output.status.success(),
            "注册种子关联失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(windows)]
    {
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn SHChangeNotify(
                event: u32,
                flags: u32,
                a: *const std::ffi::c_void,
                b: *const std::ffi::c_void,
            );
        }
        unsafe {
            SHChangeNotify(0x08000000, 0, std::ptr::null(), std::ptr::null());
        }
    }
    Ok(())
}

pub fn clipboard_sequence() -> u32 {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetClipboardSequenceNumber() -> u32;
        }
        unsafe { GetClipboardSequenceNumber() }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Extract a magnet embedded in a copied sharing URL without visiting that site.
pub fn copied_magnet(text: &str) -> Option<String> {
    if text.len() > 32768 {
        return None;
    }
    let start = text.to_ascii_lowercase().find("magnet:?")?;
    let link = text[start..]
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>'))
        .next()?;
    let link = link.replace("&amp;", "&").replace("\\&", "&");
    let url = url::Url::parse(&link).ok()?;
    let hash = url.query_pairs().find(|(k, _)| k == "xt")?.1.into_owned();
    let value = hash.strip_prefix("urn:btih:")?;
    if !((value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        || (value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_alphabetic() || (b'2'..=b'7').contains(&b))))
    {
        return None;
    }
    Some(format!("magnet:?{}", url.query()?))
}

pub fn validate(source: &str) -> Result<String> {
    ensure!(source.len() <= 32768, "链接过长");
    if source
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("magnet:?"))
    {
        let url = url::Url::parse(source)?;
        ensure!(
            url.query_pairs().any(|(key, value)| key == "xt"
                && (value.starts_with("urn:btih:") || value.starts_with("urn:btmh:"))),
            "磁力链接缺少资源哈希"
        );
        return Ok(format!("magnet:?{}", url.query().unwrap_or_default()));
    }
    let path = Path::new(source);
    ensure!(
        path.extension()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.eq_ignore_ascii_case("torrent")),
        "仅支持磁力链接或 .torrent 文件"
    );
    ensure!(path.is_file(), "种子文件不存在");
    Ok(std::fs::canonicalize(path)?.display().to_string())
}

pub fn enqueue(root: &Path, source: &str) -> Result<()> {
    let source = validate(source)?;
    let directory = root.join("data/open-requests");
    std::fs::create_dir_all(&directory)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let temporary = directory.join(format!("{id}.tmp"));
    std::fs::write(&temporary, serde_json::to_vec(&source)?)?;
    std::fs::rename(&temporary, directory.join(format!("{id}.json")))?;
    Ok(())
}

pub fn take(root: &Path) -> Result<Option<String>> {
    let directory = root.join("data/open-requests");
    if !directory.exists() {
        return Ok(None);
    }
    let mut paths = std::fs::read_dir(directory)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|v| v == "json"))
        .collect::<Vec<PathBuf>>();
    paths.sort();
    let Some(path) = paths.first() else {
        return Ok(None);
    };
    let result = (|| {
        ensure!(std::fs::metadata(path)?.len() <= 65536, "打开请求过大");
        let source: String =
            serde_json::from_slice(&std::fs::read(path)?).context("打开请求无效")?;
        validate(&source)
    })();
    std::fs::remove_file(path)?;
    result.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_wrapper_extracts_magnet_without_fetching_webpage() {
        let wrapped = "https://example.invalid/share/magnet:?xt=urn:btih:F553895F3FFB43482406D77B0664C39A8681EA37&size=1876593086&biz=ktr";
        let expected =
            "magnet:?xt=urn:btih:F553895F3FFB43482406D77B0664C39A8681EA37&size=1876593086&biz=ktr";
        assert_eq!(copied_magnet(wrapped).as_deref(), Some(expected));
        assert!(copied_magnet("https://example.invalid/page").is_none());
        assert!(copied_magnet("magnet:?xt=urn:btih:broken").is_none());
    }
    #[test]
    fn shell_requests_preserve_links_and_spaces_without_execution() {
        let temp = tempfile::tempdir().unwrap();
        let link =
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789&dn=local%20fixture";
        enqueue(temp.path(), link).unwrap();
        assert_eq!(take(temp.path()).unwrap(), Some(link.into()));
        assert_eq!(take(temp.path()).unwrap(), None);
        let torrent = temp.path().join("fixture with spaces.torrent");
        std::fs::write(&torrent, b"fixture").unwrap();
        enqueue(temp.path(), torrent.to_str().unwrap()).unwrap();
        assert_eq!(
            take(temp.path()).unwrap(),
            Some(
                std::fs::canonicalize(torrent)
                    .unwrap()
                    .display()
                    .to_string()
            )
        );
        assert!(validate("https://example.invalid").is_err());
        assert!(validate("--check-backend").is_err());
        assert!(validate("magnet:?dn=nohash").is_err());
    }
}
