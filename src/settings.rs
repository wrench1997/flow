use anyhow::Result;
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "background_default")]
    pub clipboard_watch: bool,
    #[serde(default)]
    pub seed_after_download: bool,
    #[serde(default = "background_default")]
    pub background_on_close: bool,
    pub download_dir: String,
    pub download_kib: u32,
    pub upload_kib: u32,
    pub peer_limit: u16,
    #[serde(default = "background_default")]
    pub ipv6_enabled: bool,
    #[serde(default = "background_default")]
    pub tcp_enabled: bool,
    #[serde(default = "background_default")]
    pub utp_enabled: bool,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default)]
    pub upnp_enabled: bool,
}
impl Settings {
    pub fn defaults(root: &std::path::Path) -> Self {
        Self {
            clipboard_watch: true,
            seed_after_download: false,
            background_on_close: true,
            download_dir: root.join("downloads").display().to_string(),
            download_kib: 0,
            upload_kib: 512,
            peer_limit: 150,
            ipv6_enabled: true,
            tcp_enabled: true,
            utp_enabled: true,
            listen_port: default_listen_port(),
            upnp_enabled: false,
        }
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            std::path::Path::new(&self.download_dir).is_absolute(),
            "下载目录必须是绝对路径"
        );
        anyhow::ensure!(self.tcp_enabled || self.utp_enabled, "至少启用 TCP 或 uTP");
        anyhow::ensure!((10..=2000).contains(&self.peer_limit), "连接数应为 10–2000");
        anyhow::ensure!(
            self.download_kib <= 4_000_000 && self.upload_kib <= 4_000_000,
            "限速超出支持范围"
        );
        Ok(())
    }
}
fn default_listen_port() -> u16 { 51413 }
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
    #[test]
    fn old_settings_enable_dualstack_transports_without_losing_preferences() {
        let mut value=serde_json::to_value(Settings::defaults(&std::env::temp_dir())).unwrap();
        for key in ["ipv6_enabled","tcp_enabled","utp_enabled","listen_port","upnp_enabled"] {value.as_object_mut().unwrap().remove(key);}
        value["clipboard_watch"]=serde_json::json!(false);
        let mut settings:Settings=serde_json::from_value(value).unwrap();
        assert!(settings.ipv6_enabled && settings.tcp_enabled && settings.utp_enabled);
        assert_eq!(settings.listen_port,51413);
        assert!(!settings.upnp_enabled && !settings.clipboard_watch);
        settings.tcp_enabled=false;settings.utp_enabled=false;
        assert!(settings.validate().is_err());
    }

}
