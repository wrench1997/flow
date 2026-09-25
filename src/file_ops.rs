//! Delete only explicitly enumerated task files; never recursively delete a directory.
use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};
fn linked(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
pub fn validate_files(root: &Path, files: &[PathBuf]) -> Result<()> {
    anyhow::ensure!(root.is_absolute(), "删除路径必须为绝对路径");
    for path in files {
        let relative = path.strip_prefix(root).context("任务文件超出保存目录")?;
        anyhow::ensure!(
            relative
                .components()
                .all(|p| matches!(p, Component::Normal(_)))
                && !relative.as_os_str().is_empty(),
            "文件路径包含不安全分量"
        );
        // Validate ancestors too: a junction in an ancestor must not redirect deletion.
        for ancestor in path.ancestors() {
            match std::fs::symlink_metadata(ancestor) {
                Ok(m) => anyhow::ensure!(
                    !linked(&m),
                    "拒绝操作符号链接或联接点：{}",
                    ancestor.display()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        if path.exists() {
            anyhow::ensure!(path.is_file(), "目标不是普通文件：{}", path.display());
        }
    }
    Ok(())
}
pub fn remove_files(root: &Path, files: &[PathBuf]) -> Result<()> {
    validate_files(root, files)?;
    for path in files {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("无法删除 {}；任务保留以便重试", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_deletion_preserves_neighbors_and_rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let own = temp.path().join("own.part");
        let other = temp.path().join("other.bin");
        std::fs::write(&own, b"partial").unwrap();
        std::fs::write(&other, b"keep").unwrap();
        assert!(remove_files(temp.path(), &[temp.path().join("../escape")]).is_err());
        remove_files(temp.path(), &[own.clone()]).unwrap();
        assert!(!own.exists());
        assert!(other.exists());
        assert!(remove_files(temp.path(), &[temp.path().to_path_buf()]).is_err());
    }
}
