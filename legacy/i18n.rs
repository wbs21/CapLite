use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct LocaleData {
    pub texts: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct I18n {
    current_lang: String,
    locales: HashMap<String, LocaleData>,
}

impl I18n {
    pub fn new() -> Self {
        let mut locales = HashMap::new();
        // 加载中文
        if let Ok(txt) = fs::read_to_string("locales/zh-CN.json") {
            if let Ok(data) = serde_json::from_str::<LocaleData>(&txt) {
                locales.insert("zh-CN".to_string(), data);
                log::info!("加载语言: zh-CN");
            }
        }
        // 加载英文
        if let Ok(txt) = fs::read_to_string("locales/en-US.json") {
            if let Ok(data) = serde_json::from_str::<LocaleData>(&txt) {
                locales.insert("en-US".to_string(), data);
                log::info!("加载语言: en-US");
            }
        }

        let current_lang = if locales.contains_key("zh-CN") {
            "zh-CN".to_string()
        } else {
            locales.keys().next().cloned().unwrap_or_default()
        };

        Self {
            current_lang,
            locales,
        }
    }

    pub fn set_language(&mut self, lang_id: &str) {
        if self.locales.contains_key(lang_id) {
            self.current_lang = lang_id.to_string();
            log::info!("切换语言至: {}", lang_id);
        }
    }

    pub fn current_language(&self) -> &str {
        &self.current_lang
    }

    pub fn available_languages(&self) -> Vec<String> {
        self.locales.keys().cloned().collect()
    }

    /// 获取文本，key不存在返回key原值
    pub fn t(&self, key: &str) -> &str {
        self.locales
            .get(&self.current_lang)
            .and_then(|loc| loc.texts.get(key))
            .map(|s| s.as_str())
            .unwrap_or(key)
    }
}
