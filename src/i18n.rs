//! Presentation-only localization. Download state and copied data stay canonical.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-CN")]
    Chinese,
    #[serde(rename = "en")]
    English,
}
static PREFERENCE: AtomicU8 = AtomicU8::new(0);
static ROOT: OnceLock<PathBuf> = OnceLock::new();
pub fn preference() -> Language {
    match PREFERENCE.load(Ordering::Relaxed) {
        1 => Language::Chinese,
        2 => Language::English,
        _ => Language::Auto,
    }
}
pub fn initialize(root: &Path) {
    let _ = ROOT.set(root.to_owned());
    let language = read_preference(root);
    PREFERENCE.store(language as u8, Ordering::Relaxed);
}
fn read_preference(root: &Path) -> Language {
    std::fs::read(root.join("data/ui-language.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
pub fn save_at(root: &Path, language: Language) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join("data"))?;
    let target = root.join("data/ui-language.json");
    let staged = root.join(format!("data/ui-language-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&staged, serde_json::to_vec(&language)?)?;
    if let Err(error) = std::fs::rename(&staged, &target) {
        let _ = std::fs::remove_file(staged);
        return Err(error.into());
    }
    Ok(())
}
pub fn select(language: Language) -> anyhow::Result<()> {
    save_at(
        ROOT.get()
            .ok_or_else(|| anyhow::anyhow!("Language preference directory unavailable"))?,
        language,
    )?;
    PREFERENCE.store(language as u8, Ordering::Relaxed);
    Ok(())
}
fn resolve(language: Language, system_chinese: bool) -> bool {
    match language {
        Language::English => true,
        Language::Chinese => false,
        Language::Auto => !system_chinese,
    }
}
pub fn english() -> bool {
    #[cfg(windows)]
    let chinese = unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        GetUserDefaultUILanguage() & 0x3ff == 4
    };
    #[cfg(not(windows))]
    let chinese = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_lowercase()
        .starts_with("zh");
    resolve(preference(), chinese)
}
struct Template {
    matcher: Regex,
    output: Vec<Part>,
}
#[derive(Debug)]
enum Part {
    Text(String),
    Slot(usize),
}
fn parts(input: &str) -> Vec<Part> {
    let mut result = Vec::new();
    let mut cursor = 0;
    let mut index = 0;
    while let Some(start) = input[cursor..].find('{') {
        let start = cursor + start;
        let Some(end) = input[start..].find('}') else {
            break;
        };
        let end = start + end;
        result.push(Part::Text(input[cursor..start].to_owned()));
        result.push(Part::Slot(index));
        index += 1;
        cursor = end + 1;
    }
    result.push(Part::Text(input[cursor..].to_owned()));
    result
}
struct Catalog {
    exact: BTreeMap<String, String>,
    templates: Vec<Template>,
}
fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut exact = BTreeMap::new();
        let mut templates = Vec::new();
        for line in include_str!("../assets/i18n-en.tsv")
            .lines()
            .filter(|line| !line.is_empty())
        {
            let (zh, en) = line.split_once('|').expect("valid translation row");
            let zh = zh.replace("\\n", "\n");
            let en = en.replace("\\n", "\n");
            if zh.contains('{') {
                let mut pattern = String::from("\\A");
                for part in parts(&zh) {
                    match part {
                        Part::Text(text) => pattern.push_str(&regex::escape(&text)),
                        Part::Slot(_) => pattern.push_str("(.*?)"),
                    }
                }
                pattern.push_str("\\z");
                templates.push(Template {
                    matcher: Regex::new(&format!("(?s){pattern}"))
                        .expect("valid translation template"),
                    output: parts(&en),
                });
            } else {
                assert!(exact.insert(zh, en).is_none(), "duplicate translation");
            }
        }
        Catalog { exact, templates }
    })
}
pub fn t(input: impl AsRef<str>) -> String {
    translate(input.as_ref(), english())
}
fn translate(input: &str, en: bool) -> String {
    if !en
        || !input
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    {
        return input.to_owned();
    }
    let catalog = catalog();
    if let Some(value) = catalog.exact.get(input) {
        return value.clone();
    }
    for template in &catalog.templates {
        if let Some(values) = template.matcher.captures(input) {
            return template
                .output
                .iter()
                .map(|part| match part {
                    Part::Text(text) => text.clone(),
                    // Translate only known static status values; user paths/names are
                    // opaque captures, never searched or replaced by substrings.
                    Part::Slot(i) => {
                        let value = values.get(i + 1).map_or("", |m| m.as_str());
                        catalog
                            .exact
                            .get(value)
                            .cloned()
                            .unwrap_or_else(|| value.to_owned())
                    }
                })
                .collect();
        }
    }
    input.to_owned()
}
pub fn selector(ui: &mut eframe::egui::Ui, id: &str) {
    let previous = preference();
    let mut selected = previous;
    eframe::egui::ComboBox::from_id_salt(id)
        .selected_text(match previous {
            Language::Auto => t("自动（跟随系统）"),
            Language::Chinese => "简体中文".into(),
            Language::English => "English".into(),
        })
        .width(130.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut selected, Language::Auto, t("自动（跟随系统）"));
            ui.selectable_value(&mut selected, Language::Chinese, "简体中文");
            ui.selectable_value(&mut selected, Language::English, "English");
        });
    if selected != previous {
        if let Err(error) = select(selected) {
            ui.colored_label(
                eframe::egui::Color32::RED,
                t(format!("语言设置保存失败：{error}")),
            );
        }
        ui.ctx().request_repaint();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_and_explicit_choices_resolve_correctly() {
        assert!(!resolve(Language::Auto, true));
        assert!(resolve(Language::Auto, false));
        assert!(resolve(Language::English, true));
        assert!(!resolve(Language::Chinese, false));
    }
    #[test]
    fn language_preference_survives_restart_without_changing_engine_settings() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("data")).unwrap();
        let path = root.path().join("data/settings.json");
        std::fs::write(&path, b"engine settings").unwrap();
        assert_eq!(read_preference(root.path()), Language::Auto);
        save_at(root.path(), Language::English).unwrap();
        assert_eq!(read_preference(root.path()), Language::English);
        save_at(root.path(), Language::Chinese).unwrap();
        assert_eq!(read_preference(root.path()), Language::Chinese);
        assert_eq!(std::fs::read(path).unwrap(), b"engine settings");
    }
    #[test]
    fn formatted_labels_and_diagnostics_preserve_values() {
        assert_eq!(translate("等待来源", true), "Waiting for sources");
        assert_eq!(
            translate("已选 4 / 8 个文件 · 12 MiB", true),
            "Selected 4 / 8 files · 12 MiB"
        );
        assert_eq!(
            translate("保存位置：D:\\中文下载\\movie.mp4", true),
            "Save location: D:\\中文下载\\movie.mp4"
        );
        assert_eq!(translate("Arcane.S02[中文]", true), "Arcane.S02[中文]");
        assert_eq!(
            translate(
                "已请求 5 个数据块，暂未收到数据；声明持有所缺分片的节点 2 个，其中 1 个当前未允许传输",
                true
            ),
            "Requested 5 blocks; no data received. 2 peers claim needed pieces; 1 do not permit transfer."
        );
        assert_eq!(translate("等待来源", false), "等待来源");
    }
    #[test]
    fn every_template_preserves_placeholder_order() {
        for line in include_str!("../assets/i18n-en.tsv")
            .lines()
            .filter(|line| !line.is_empty())
        {
            let (zh, en) = line.split_once('|').unwrap();
            let fields = |s: &str| {
                Regex::new(r"\{([^}:]*)(?::[^}]*)?\}")
                    .unwrap()
                    .captures_iter(s)
                    .map(|m| m[1].to_owned())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                fields(zh),
                fields(en),
                "Translation changed placeholders: {zh}"
            );
        }
        let _ = catalog();
    }
}
