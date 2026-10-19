//! 界面多语言。
//!
//! 语言包在编译期内嵌（`include_str!`），因此 exe 自带中英文、不依赖工作目录；
//! 同时支持从 exe 同级 `locales/` 目录加载同名文件覆盖内嵌内容，方便改文案。

use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
struct LocaleData {
    texts: HashMap<String, String>,
}

/// 外部语言包大小上限：正常 10 KB 左右；超过说明文件损坏或被恶意放大。
const MAX_LOCALE_BYTES: u64 = 256 * 1024;

fn read_locale_file(path: &Path) -> Option<String> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_LOCALE_BYTES {
            tracing::warn!(
                path = %path.display(),
                bytes = meta.len(),
                "外部语言包过大，已忽略"
            );
            return None;
        }
    }
    std::fs::read_to_string(path).ok()
}

/// 内嵌语言包：(语言 id, JSON 内容)
const BUILTIN: &[(&str, &str)] = &[
    ("zh-CN", include_str!("../locales/zh-CN.json")),
    ("en-US", include_str!("../locales/en-US.json")),
];

#[derive(Debug, Clone)]
pub struct I18n {
    lang: String,
    locales: BTreeMap<String, HashMap<String, String>>,
}

impl I18n {
    pub fn new(lang: &str) -> Self {
        let mut locales = BTreeMap::new();
        for (id, json) in BUILTIN {
            match serde_json::from_str::<LocaleData>(json) {
                Ok(data) => {
                    locales.insert((*id).to_string(), data.texts);
                }
                Err(e) => {
                    tracing::error!(locale = id, error = %e, "内嵌语言包解析失败");
                }
            }
        }
        let mut me = Self { lang: String::new(), locales };
        me.set_language(lang);
        me
    }

    /// 用磁盘上的 locales/*.json 覆盖内嵌语言包（可选）。
    pub fn overlay_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let Some(text) = read_locale_file(&path) else { continue };
            match serde_json::from_str::<LocaleData>(&text) {
                Ok(data) => {
                    tracing::info!(locale = id, path = %path.display(), "加载外部语言包");
                    self.locales.insert(id.to_string(), data.texts);
                }
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "外部语言包解析失败"),
            }
        }
    }

    /// 切换语言；不存在的语言 id 会被忽略，返回是否切换成功。
    pub fn set_language(&mut self, lang: &str) -> bool {
        if self.locales.contains_key(lang) {
            self.lang = lang.to_string();
            true
        } else {
            if self.lang.is_empty() {
                // 首次初始化：回落到内置语言包里的第一个（BTreeMap 有序 → en-US 在前）。
                // 这个值必须与 `config::DEFAULT_LANGUAGE` 一致；测试
                // `default_language_is_builtin_and_first` 会挡住两侧漂移。
                self.lang = self
                    .locales
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "en-US".into());
            }
            false
        }
    }

    pub fn language(&self) -> &str {
        &self.lang
    }

    pub fn available(&self) -> Vec<String> {
        self.locales.keys().cloned().collect()
    }

    /// 取文案；缺失时回退到其它语言，再回退到 key 本身（便于发现漏翻）。
    pub fn t<'a>(&'a self, key: &'a str) -> &'a str {
        if let Some(map) = self.locales.get(&self.lang) {
            if let Some(v) = map.get(key) {
                return v.as_str();
            }
        }
        for map in self.locales.values() {
            if let Some(v) = map.get(key) {
                return v.as_str();
            }
        }
        key
    }

    /// 取带占位符的文案并顺序替换 `{}`。
    pub fn tf(&self, key: &str, args: &[&dyn std::fmt::Display]) -> String {
        let mut out = String::with_capacity(64);
        let template = self.t(key).to_string();
        let mut rest: &str = &template;
        for arg in args {
            match rest.split_once("{}") {
                Some((head, tail)) => {
                    out.push_str(head);
                    out.push_str(&arg.to_string());
                    rest = tail;
                }
                None => break,
            }
        }
        out.push_str(rest);
        out
    }
}

impl Default for I18n {
    /// 与 `config::default_language()` 同一个口径：**英文**。
    fn default() -> Self {
        Self::new("en-US")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_external_locale_is_ignored() {
        let dir = std::env::temp_dir().join(format!("cap-locale-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("xx-XX.json");
        std::fs::write(&path, vec![b'x'; (MAX_LOCALE_BYTES + 1) as usize]).unwrap();
        assert!(read_locale_file(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_locales_load_and_align() {
        let zh = I18n::new("zh-CN");
        let en = I18n::new("en-US");
        assert_eq!(zh.language(), "zh-CN");
        assert_eq!(en.language(), "en-US");
        for key in [
            "app_title",
            "btn_start",
            "btn_stop",
            "tab_editor",
            "err_invalid_range",
            // P40 新增的三个键（深色模式开关 + 剪辑页那句拖拽提示）
            "dark_mode",
            "dark_mode_tip",
            "segment_drag_hint",
        ] {
            assert!(!zh.t(key).is_empty(), "zh 缺 {key}");
            assert!(!en.t(key).is_empty(), "en 缺 {key}");
        }
        // P40 删掉的两个分区标题必须真没了（留着没人用的键 = 翻译漂移）
        for gone in ["sec_general", "sec_video"] {
            assert_eq!(zh.t(gone), gone, "zh 应已删除 {gone}");
            assert_eq!(en.t(gone), gone, "en 应已删除 {gone}");
        }
        // 两种语言的 key 集合必须一致（漏翻会被这条测出来）
        let mut zh_keys: Vec<_> = zh.locales["zh-CN"].keys().cloned().collect();
        let mut en_keys: Vec<_> = en.locales["en-US"].keys().cloned().collect();
        zh_keys.sort();
        en_keys.sort();
        assert_eq!(zh_keys, en_keys, "中英文 key 集合不一致");
    }

    /// **默认语言 = 英文**，且这个默认值必须真的内置（否则界面会一个字都翻不出来）。
    ///
    /// 同时挡住"`config::DEFAULT_LANGUAGE` 与内置语言包漂移"：两边必须一致。
    #[test]
    fn default_language_is_builtin() {
        let i = I18n::default();
        assert_eq!(i.language(), crate::config::default_language());
        assert_eq!(i.language(), "en-US");
        assert!(
            i.available().iter().any(|l| l == crate::config::default_language()),
            "默认语言必须是内置语言包之一，实际内置：{:?}",
            i.available()
        );
        // 未知语言 id 也不能把界面打成空白：应回落到默认语言
        let mut j = I18n::new("fr-FR");
        assert_eq!(j.language(), "en-US");
        assert!(j.set_language("zh-CN"), "中文仍应可切换");
        assert_eq!(j.language(), "zh-CN");
    }

    #[test]
    fn unknown_key_and_language_are_safe() {
        let mut i = I18n::new("zh-CN");
        assert_eq!(i.t("no_such_key"), "no_such_key");
        assert!(!i.set_language("fr-FR"));
        assert_eq!(i.language(), "zh-CN");
        assert!(i.set_language("en-US"));
        assert_eq!(i.language(), "en-US");
    }

    #[test]
    fn template_substitution() {
        let i = I18n::new("en-US");
        let s = i.tf("pick_size", &[&1920, &1080]);
        assert!(s.contains("1920") && s.contains("1080"), "占位符未替换: {s}");
        assert!(!s.contains("{}"));
    }

    /// 把 `src/` 下所有 `.rs` 拼成一份文本。
    ///
    /// 两条源码级守门单测（正向 key 扫描 / `tf` 实参数）共用这一份 ——
    /// 它们都只问"某个字面量在源码里出现过没有"，与文件顺序无关。
    fn source_text() -> String {
        fn collect(dir: &Path, out: &mut String) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        out.push_str(&text);
                    }
                }
            }
        }

        let mut src = String::new();
        collect(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut src);
        assert!(!src.is_empty(), "没读到源码：判据会假通过");
        src
    }

    /// 去掉 `//` 与 `/* */` 注释，**字符串字面量原样保留**。
    ///
    /// 这一步不能省：文档注释里写着 `t("key")` 这类示例，不剥掉就会被当成真调用，
    /// 于是守门单测对着自己的说明文字报"key 不存在"（P51 第一次跑就是这样翻车的）。
    /// 只剥注释、不剥字符串：`"t("` 这种**测试数据**仍会留下，由 `looks_like_key` 过滤。
    fn strip_comments(text: &str) -> String {
        let b = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0usize;
        while i < b.len() {
            match b[i] {
                b'"' => {
                    let start = i;
                    i += 1;
                    while i < b.len() {
                        if b[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if b[i] == b'"' {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    out.push_str(&text[start..i.min(b.len())]);
                }
                b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                    let mut depth = 1usize;
                    i += 2;
                    while i < b.len() && depth > 0 {
                        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                            depth += 1;
                            i += 2;
                        } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                _ => {
                    let ch = text[i..].chars().next().expect("i 落在字符边界上");
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        out
    }

    /// 真 key 的形态：小写字母 / 数字 / 下划线。
    ///
    /// 用来挡掉扫描器的自伤 —— `for name in ["t(", "tf("]` 里那个字符串字面量
    /// 会让 `find("t(")` 命中，随后被解析成 key `", "`；它不是合法 key 形态，直接跳过。
    fn looks_like_key(k: &str) -> bool {
        !k.is_empty()
            && k.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    /// 在 `text` 里找 `name`，要求**前一个字符不是标识符字符**。
    ///
    /// 少了这个边界判断，`find("t(")` 会把 `format!("…")` 也当成一次 `t("…")` 调用
    ///（`forma**t(**` 里就含 `t(`），于是扫出一堆不存在的 key。
    fn find_call(text: &str, from: usize, name: &str) -> Option<usize> {
        let mut idx = from;
        while let Some(rel) = text[idx..].find(name) {
            let at = idx + rel;
            let boundary_ok = at == 0 || {
                let c = text.as_bytes()[at - 1];
                !(c.is_ascii_alphanumeric() || c == b'_')
            };
            if boundary_ok {
                return Some(at);
            }
            idx = at + name.len();
        }
        None
    }

    /// 扫出所有 `name("key")` 形态的调用，返回 `(key, key 结束引号之后的文本)`。
    fn scan_calls<'a>(text: &'a str, name: &str) -> Vec<(&'a str, &'a str)> {
        let mut out = Vec::new();
        let mut from = 0usize;
        while let Some(at) = find_call(text, from, name) {
            let tail = text[at + name.len()..].trim_start();
            if let Some(stripped) = tail.strip_prefix('"') {
                if let Some(endq) = stripped.find('"') {
                    out.push((&stripped[..endq], &stripped[endq + 1..]));
                }
            }
            from = at + name.len();
        }
        out
    }

    /// `s` 从 `&[` 之后开始；返回顶层元素个数（数到对应的 `]` 为止）。
    /// 括号/方括号/花括号内的逗号不计数；字符串字面量整体跳过。
    fn count_slice_items(s: &str) -> Option<usize> {
        let b = s.as_bytes();
        let (mut depth, mut commas, mut saw_item, mut after_comma, mut i) = (0usize, 0usize, false, false, 0usize);
        while i < b.len() {
            match b[i] {
                b'"' => {
                    i += 1;
                    while i < b.len() {
                        if b[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if b[i] == b'"' {
                            break;
                        }
                        i += 1;
                    }
                    saw_item = true;
                    after_comma = false;
                }
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    saw_item = true;
                    after_comma = false;
                }
                b')' | b'}' => depth = depth.saturating_sub(1),
                b']' => {
                    if depth == 0 {
                        return Some(if !saw_item {
                            0
                        } else if after_comma {
                            commas
                        } else {
                            commas + 1
                        });
                    }
                    depth -= 1;
                }
                b',' if depth == 0 => {
                    commas += 1;
                    after_comma = true;
                }
                b' ' | b'\n' | b'\r' | b'\t' => {}
                _ => {
                    saw_item = true;
                    after_comma = false;
                }
            }
            i += 1;
        }
        None
    }

    /// **语言包里不许有"永远不会显示"的 key**（P48）。
    ///
    /// 判据很硬：这个 key 的字面量必须在 `src/` 的某个 `.rs` 里出现过。
    /// 不出现 = 代码里没有任何一条路径能取到它 = 死翻译，只会让中英两边
    /// 越漂越远（P48 一次性删掉了 55 个这种 key）。
    ///
    /// **动态拼出来的 key 走前缀白名单**：目前只有
    /// `t(&format!("bad_{}", kind.key()))`（`edit::health::BadKind`），
    /// 所以 `bad_*` 放行。以后再有 `format!` 拼 key，必须同时加进这张表 ——
    /// 这不是"测试太严"，而是逼着人把"这个 key 从哪儿来"写下来。
    #[test]
    fn every_locale_key_is_referenced_in_source() {
        const DYNAMIC_PREFIXES: &[&str] = &["bad_"];

        let src = source_text();

        let i = I18n::new("zh-CN");
        let mut dead: Vec<&str> = i.locales["zh-CN"]
            .keys()
            .filter(|k| !DYNAMIC_PREFIXES.iter().any(|p| k.starts_with(p)))
            .filter(|k| !src.contains(&format!("\"{k}\"")))
            .map(String::as_str)
            .collect();
        dead.sort_unstable();
        assert!(
            dead.is_empty(),
            "这些 key 在源码里从未被引用（要么用起来、要么从两个语言包里删掉）：{dead:?}"
        );
    }

    /// **代码里引用的 key 必须真的存在**（P51，P48 那条的反向）。
    ///
    /// P48 那条查的是"语言包里有、代码里没人用"（死翻译）；这条查相反方向 ——
    /// "代码里用了、语言包里没有"。后者更隐蔽：`I18n::t` 找不到就**回退成 key 原文**，
    /// 于是界面上会直接出现 `stat_frames` 这种字符串，而编译、测试、走查全都不出声
    ///（`unknown_key_and_language_are_safe` 甚至把这条回退行为固化成了"预期"）。
    #[test]
    fn every_key_used_in_source_exists_in_locales() {
        // 唯一的例外：`unknown_key_and_language_are_safe` 故意用一个不存在的 key
        // 来断言"回退成 key 原文"。它是**测试夹具**，不是漏翻。
        const INTENTIONALLY_MISSING: &[&str] = &["no_such_key"];

        let src = strip_comments(&source_text());
        let i = I18n::new("en-US");

        let mut missing: Vec<String> = Vec::new();
        for name in ["t(", "tf("] {
            for (key, _) in scan_calls(&src, name) {
                if !looks_like_key(key) || INTENTIONALLY_MISSING.contains(&key) {
                    continue;
                }
                if !i.locales["en-US"].contains_key(key) || !i.locales["zh-CN"].contains_key(key) {
                    missing.push(format!("{name}\"{key}\")"));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "这些 key 在源码里被引用，但语言包里没有（界面会直接显示 key 原文）：{missing:?}"
        );
    }

    /// **`tf(key, &[..])` 的实参数必须等于模板里 `{}` 的个数**（P51）。
    ///
    /// `I18n::tf` 对两种错配都**不报错**：
    /// * 多传 —— `rest` 里再也找不到 `{}` 就 `break`，多出来的参数**静默丢弃**；
    /// * 少传 —— 剩下的 `{}` 原样留在界面上，用户看到字面 `{}`。
    ///
    /// 真实案例（P51 抓到的）：`close_cache_hint` 的模板只有 1 个 `{}`，
    /// 调用却传了 `&[&APP_NAME, &dir]` —— 路径参数从没被用上（路径实际由紧邻的
    /// `mono_muted(&dir)` 那一行显示），但读代码的人会以为模板消费了它。
    ///
    /// 只对**字面量切片**生效：第二参数不是 `&[` 开头的（例如传一个变量）一律跳过 ——
    /// 静态看不出来，硬猜只会造成假失败。
    #[test]
    fn every_tf_call_matches_its_placeholder_count() {
        let src = strip_comments(&source_text());
        let i = I18n::new("en-US");

        let mut checked = 0usize;
        let mut bad: Vec<String> = Vec::new();
        for (key, tail) in scan_calls(&src, "tf(") {
            if !looks_like_key(key) {
                continue;
            }
            let Some(tmpl) = i.locales["en-US"].get(key) else {
                continue; // 缺 key 由上面那条单测负责报
            };
            let Some(rest) = tail.trim_start().strip_prefix(',') else { continue };
            let Some(slice) = rest.trim_start().strip_prefix("&[") else { continue };
            let Some(n_args) = count_slice_items(slice) else { continue };
            let n_ph = tmpl.matches("{}").count();
            checked += 1;
            if n_args != n_ph {
                bad.push(format!("tf(\"{key}\") 占位符 {n_ph} / 实参 {n_args}"));
            }
        }
        assert!(checked >= 40, "只解析到 {checked} 个 tf 调用，扫描逻辑可能已失效");
        bad.sort();
        assert!(
            bad.is_empty(),
            "这些 tf 调用的实参数与模板占位符数不一致（多传的会被静默丢弃、少传的会留下字面 {{}}）：{bad:?}"
        );
    }
}
