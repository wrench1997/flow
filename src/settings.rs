use anyhow::Result;
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "background_default")]
    pub background_on_close: bool,
    pub download_dir: String,
    pub download_kib: u32,
    pub upload_kib: u32,
    pub peer_limit: u16,
}
impl Settings {
    pub fn defaults(root: &std::path::Path) -> Self {
        Self {
            background_on_close: true,
            download_dir: root.join("downloads").display().to_string(),
            download_kib: 0,
            upload_kib: 512,
            peer_limit: 150,
        }
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            std::path::Path::new(&self.download_dir).is_absolute(),
            "下载目录必须是绝对路径"
        );
        anyhow::ensure!((10..=2000).contains(&self.peer_limit), "连接数应为 10–2000");
        anyhow::ensure!(
            self.download_kib <= 4_000_000 && self.upload_kib <= 4_000_000,
            "限速超出支持范围"
        );
        Ok(())
    }
}
fn background_default() -> bool {
    true
}
