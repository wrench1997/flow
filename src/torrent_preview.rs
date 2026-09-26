use anyhow::{Result, ensure};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Torrent {
    info: Info,
}
#[derive(Deserialize)]
struct Info {
    name: String,
    length: Option<u64>,
    files: Option<Vec<File>>,
}
#[derive(Deserialize)]
struct File {
    length: u64,
    path: Vec<String>,
}
pub fn read(path: &Path) -> Result<Vec<(String, u64)>> {
    ensure!(
        std::fs::metadata(path)?.len() <= 20_000_000,
        "种子文件不能超过 20 MB"
    );
    parse(&std::fs::read(path)?)
}
fn parse(bytes: &[u8]) -> Result<Vec<(String, u64)>> {
    let torrent: Torrent = serde_bencode::from_bytes(bytes)?;
    let info = torrent.info;
    let rows = if let Some(files) = info.files {
        files
            .into_iter()
            .map(|f| (format!("{}/{}", info.name, f.path.join("/")), f.length))
            .collect()
    } else {
        vec![(
            info.name,
            info.length
                .ok_or_else(|| anyhow::anyhow!("种子缺少文件大小"))?,
        )]
    };
    ensure!(!rows.is_empty(), "种子没有文件");
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_keeps_file_order_and_sizes() {
        let rows = parse(b"d4:infod5:filesld6:lengthi2e4:pathl5:a.txteed6:lengthi3e4:pathl5:b.txteee4:name4:rootee").unwrap();
        assert_eq!(
            rows,
            vec![("root/a.txt".into(), 2), ("root/b.txt".into(), 3)]
        );
        assert!(parse(b"invalid").is_err());
    }
}
