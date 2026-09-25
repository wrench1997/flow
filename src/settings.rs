use anyhow::Result;
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub seed_after_download: bool,
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
            seed_after_download: false,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_settings_default_to_no_seeding() {
        let root = std::env::temp_dir();
        let defaults = Settings::defaults(&root);
        assert!(!defaults.seed_after_download);
        let mut value = serde_json::to_value(defaults).unwrap();
        value.as_object_mut().unwrap().remove("seed_after_download");
        assert!(
            !serde_json::from_value::<Settings>(value)
                .unwrap()
                .seed_after_download
        );
    }
}
