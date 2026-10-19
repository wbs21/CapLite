//! 界面层（egui/eframe 0.36）。
//!
//! 页面分工：
//! * [`theme`]     —— **浅色清爽主题**：统一颜色/间距/圆角/字号 + 三页共用的小组件
//! * [`layout`]    —— **最小窗口尺寸**：三页必需高度的实测口径 + `min_inner_size` 的推导
//! * [`main_page`] —— 主页面：全屏/区域录制、三种内容模式、音源多选、计时与错误可见
//! * [`settings`]  —— 设置页：语言、目标码率、帧率、关键帧间隔、音源设备
//! * [`editor`]    —— 剪辑窗口：时间线缩放、关键帧吸附、copy 导出（不重新编码）
//!
//! 约定：各页面只依赖 `caplite::recorder` / `caplite::config` / `caplite::audio` / `caplite::video` / [`crate::edit`]，
//! 不直接碰 FFmpeg FFI。

pub mod editor;
pub mod layout;
pub mod main_page;
pub mod settings;
pub mod startup;
pub mod theme;

#[cfg(test)]
mod i18n_guard {
    //! **防回归：界面源码里不许出现硬编码中文**（P1）。
    //!
    //! 为什么需要它：`i18n::tests::builtin_locales_load_and_align` 只比对
    //! **两个 locale 文件的 key 集合**，它**看不见**"某处根本没走 i18n"——
    //! 那正是用户报的那个 bug：语言 = English 时，剪辑页空状态仍然是中文
    //! （`打开视频` / `重新载入` / `起点` / `终点` / `选区无效…` / `还没有打开视频…`），
    //! 而顶部 `Record / Trim / Settings` 是英文。
    //!
    //! 本测试把这条口径**钉在源码上**：扫描 `src/ui/` 下每个 `.rs` 的字符串字面量，
    //! 只要出现 CJK 字符就失败，并打印"文件:行号:字符串"。
    //!
    //! # 扫描口径（为什么这样切，而不是整仓扫）
    //!
    //! 刻意**跳过**这三类，否则会把大量非界面文案误报成违规：
    //!
    //! 1. **注释**（`//` / `///` / `/* */`）—— 本项目的注释全是中文，那是给人读的；
    //! 2. **`#[cfg(test)] mod tests { … }` 整块** —— 断言消息是中文，它们不上界面；
    //!    另外页面结构体里那些 `#[cfg(test)]` 字段也由这一条一并覆盖；
    //! 3. **日志宏**（`tracing::debug!/info!/warn!/error!/trace!`）—— 日志是**给开发者**
    //!    看的（落 `cap.log`），中英不敏感，且大量"技术性原因"（FFmpeg 错误等）都在这里。
    //!    日志与"用户可见文案"的边界就按这一条切：**宏名 `tracing::` 起、到字符串结束**。
    //!
    //! **白名单**（这几条文件允许有中文，且已经过人工确认"不上界面"）：
    //! * `theme.rs` —— 只有注释与 `RichText` 的入参转发，没有任何中文字面量（保留它是为了
    //!   将来加"无文案"的小组件时不必再改本测试）；
    //! * `layout.rs` / `startup.rs` —— 布局常量说明与启动阶段日志，**不渲染任何文字**；
    //! * `mod.rs` —— 本文件（模块文档 + 本测试自己）。
    //!
    //! 其余所有 `src/ui/*.rs`（`editor.rs` / `main_page.rs` / `settings.rs`）**必须零中文字面量**：
    //! 一切用户可见文字都走 `i18n.t("key")` / `i18n.tf("key", &[…])`，
    //! 新增 key 时 `zh-CN.json` 与 `en-US.json` 必须同时补齐（由 `i18n` 的 key 一致性测试守）。
    //!
    //! 说明：`src/**` 里还有若干**非界面**的中文诊断串（`edit/mod.rs` 的 `AppError` 文案、
    //! `config.rs` 的加载失败原因、`main.rs` 的 `[cap] …` 启动日志）。它们**不**在本测试
    //! 的范围内 —— 那是"内部诊断"层，界面展示它们时会在外层套上 i18n 键
    //! （例如 `err_open_video_failed`、`err_load_failed`）。要用 `cargo test` 覆盖那一层，
    //! 需要先把诊断串改成错误码/枚举，属于另一件事。

    /// 本测试**会扫**的文件（相对 `CARGO_MANIFEST_DIR`）；`theme.rs` / `layout.rs` /
    /// `startup.rs` / `mod.rs` 在白名单里，见模块文档。
    const SCANNED: &[&str] = &[
        "src/ui/editor.rs",
        "src/ui/main_page.rs",
        "src/ui/settings.rs",
    ];

    /// **行内白名单（单记号）**：整行含这些记号时跳过该行。
    ///
    /// 为什么要"按行"而不是"按整仓类型"：这些记号所在的行都是**纯诊断**，
    /// 它们的产物要么落 `cap.log`、要么与系统错误原文拼在一起交给上层——
    /// **都不该、也不能**走 i18n（翻译系统错误文本没有意义）。逐行列出是为了
    /// 让"哪一行被豁免、为什么"在源码里一眼可见，而不是藏在正则里。
    ///
    /// 两处：
    /// * `startup_marks::` —— `main_page.rs` 的启动阶段耗时打点（借 `startup_marks::note`
    ///   写日志，字符串里带中文说明）；
    /// * `[cap] ` —— 同类的启动/运行日志行（写进 `cap.log` 的 `[cap] …` 行）。
    ///
    /// 改这些文案时**不需要**同步语言包（它们不上界面）。
    const LINE_ALLOW: &[&str] = &["startup_marks::", "[cap] "];

    /// **行内白名单（成对记号）**：整行**同时**含两项时跳过。
    ///
    /// 目前只有 `(".expect(", …)`：那是"到这里按上面的 `return` 已不可能再走到"的
    /// **开发者断言**，触发即 panic（消息只进 panic backtrace / 崩溃日志），
    /// **不上界面**、也不该走 i18n —— 翻译一句"上面已判空"没有意义。
    /// 写成"成对"而不是把中文原文本身加进白名单，是为了**收窄**豁免面：
    /// 只有挂在 `.expect(` 上的中文才被放过，普通字符串里出现同样几个字照拦不误。
    const LINE_ALLOW_PAIRS: &[(&str, &str)] = &[(".expect(", "上面已判空")];

    /// `c` 是不是"中文/日文界面会用到的那种字符"。
    ///
    /// 覆盖 CJK 统一表意文字（含扩展 A）、CJK 标点（`、` `。` `「` `」` `（` `）` `：`）、
    /// 全角形式（`！` `～` 等）与中文注音符号。**不含**普通半角标点与 `×` `→` 这类
    /// 中西通用符号（它们在英文界面里也照用）。
    fn is_cjk(c: char) -> bool {
        matches!(c as u32,
            0x3000..=0x303F   // CJK 标点（、。「」等）
            | 0x3040..=0x30FF // 假名（本项目不用，但同属"日文/中文界面"范畴）
            | 0x3100..=0x312F // 注音
            | 0x3400..=0x4DBF // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 统一表意文字
            | 0xF900..=0xFAFF // CJK 兼容表意文字
            | 0xFE30..=0xFE4F // CJK 兼容形式（竖排标点等）
            | 0xFF00..=0xFFEF // 全角形式
            | 0x20000..=0x2FA1F // CJK 扩展 B 及以后
        )
    }

    /// 找到 `at` 处（`#[cfg(test)]` 的 `[`）那个单元测试模块，返回它**开括号的字节下标**。
    ///
    /// 判据（这是**必须**写成"容忍空白与换行风格"的原因）：
    /// `#[cfg(test)]` 与 `mod tests {` 之间可能有换行/缩进，而且本仓的
    /// `editor.rs` / `main_page.rs` / `settings.rs` 是 **CRLF** 换行，`mod.rs` / `startup.rs`
    /// 是 **LF** —— 旧实现把中间那个换行字面写成 `"\n"`，于是在这三个文件上
    /// **一次都没匹配上**：整块测试代码被当成"界面源码"扫，
    /// 既把断言里的中文报成硬编码（误报），又因为测试代码里有 `contains('中')`
    /// 这种"字节游标落在多字节字符中间"的地方而**直接 panic**（本次修的 bug）。
    ///
    /// 所以判据是：从 `mod` 起按 **Rust 标识符**读段（`mod`、`tests`），
    /// 两者之间只允许空白/换行，随后必须紧跟 `{`。
    /// 返回 `mod … {` 那个 **`{` 的字节下标**（相对整个 `text`）。
    ///
    /// P52 修了一个真 bug：旧实现用 `char_indices()` 走，返回的是**字符下标**，
    /// 而调用方 [`blank_test_blocks`] 拿它去索引 `body.as_bytes()` —— **字符下标被当成字节下标**。
    /// 本仓源码中文注释很多，`startup.rs` 上两者相差 **7,123 字节**：扫描提前 7 KB 起步，
    /// 一路碰到无关的 `{`/`}`，`mod tests` 只被挖掉开头三十来行就提前收尾，
    /// **剩下的测试代码一直被当成生产代码扫**（中文守卫的假阳性来源、panic 守卫的漏报来源）。
    ///
    /// 顺带把写死的模块名 `tests` 泛化成任意标识符：`#[cfg(test)] mod i18n_guard { … }`
    /// 同样是测试代码，没理由只认 `mod tests`。
    ///
    /// 全程按**字节**走是安全的：要匹配的 `mod`、模块名、`{` 都是 ASCII，
    /// 而"跳过空白"用 `is_ascii_whitespace` 也覆盖 `\r\n`。
    fn skip_test_mod(text: &str, at: usize) -> Option<usize> {
        let rest = text.get(at..)?;
        let b = rest.as_bytes();
        let mut i = 0usize;
        // ① 关键字 `mod`（前面允许空白，含 `\r\n`）
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if !rest[i..].starts_with("mod") {
            return None;
        }
        i += 3;
        // ② 不能是更长标识符的前缀（`module` 不算 `mod` + 空白）
        if i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            return None;
        }
        // ③ `mod` 与模块名之间可以有空白
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        // ④ 模块名：任意标识符（旧实现写死成 `tests`，见上面 P52 的说明）
        let name_start = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        if i == name_start {
            return None; // 没有模块名
        }
        // ⑤ 模块名与 `{` 之间可以有空白
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'{' {
            return None;
        }
        Some(at + i)
    }

    /// 把源码里**每个** `#[cfg(test)] mod tests { … }` 整块替换成**等长空白**
    /// （保留 `\n`，所以行号仍是原文件行号）。
    ///
    /// 两个细节都是踩过的坑：
    /// * **换行风格**：判据必须容忍 `\r\n`（见 [`skip_test_mod`]）；
    /// * **多次出现**：旧实现用 `find` 只处理**第一处**；本仓一个文件目前只有一个 tests 模块，
    ///   但"只挖第一处"是隐形的地雷 —— 这里循环挖到没有为止。
    fn blank_test_blocks(text: &str) -> String {
        let mut body = text.to_string();
        loop {
            let Some(at) = body.find("#[cfg(test)]") else {
                return body;
            };
            let Some(open) = skip_test_mod(&body, at + "#[cfg(test)]".len()) else {
                // 不是本测试要跳的形态（例如 `#[cfg(test)] #[derive(..)] struct TestRects`）：
                // 只把这**一个属性标记本身**挖掉，保证游标一定前进（不会死循环），
                // 但**留着**它后面的代码（那是正常源码，必须照扫）。
                let end = at + "#[cfg(test)]".len();
                let mut blanked = String::with_capacity(end - at);
                for ch in body[at..end].chars() {
                    blanked.push(if ch == '\n' { '\n' } else { ' ' });
                }
                body = format!("{}{}{}", &body[..at], blanked, &body[end..]);
                continue;
            };
            let bytes = body.as_bytes();
            let mut depth = 0i32;
            let mut i = open;
            let mut end: Option<usize> = None;
            let mut in_str = false;
            let mut in_line_comment = false;
            let mut in_block_comment = false;
            // `Some(n)` = 正在 `r` + n 个 `#` 开头的原始串里
            let mut raw_hashes: Option<usize> = None;
            while i < bytes.len() {
                let c = bytes[i];
                if in_line_comment {
                    if c == b'\n' {
                        in_line_comment = false;
                    }
                } else if in_block_comment {
                    // P52：块注释此前**完全没处理** —— `/* } */` 会被当成真花括号。
                    if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        in_block_comment = false;
                        i += 1;
                    }
                } else if raw_hashes.is_some() {
                    // P52：原始串 `r"…"` / `r#"…"#`（`#` 个数任意）。此前不认，
                    // 遇到测试里的 `r#"…{…}…"#` 就会把串内的花括号算进配平、提前收尾。
                    let n = raw_hashes.expect("上面刚判过 Some");
                    if c == b'"' {
                        // 后面要跟 n 个 `#` 才算结束
                        let mut ok = true;
                        for k in 1..=n {
                            if bytes.get(i + k) != Some(&b'#') {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            raw_hashes = None;
                            i += n;
                        }
                    }
                } else if in_str {
                    if c == b'\\' {
                        i += 2;
                        continue;
                    }
                    if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    in_line_comment = true;
                    i += 1;
                } else if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    in_block_comment = true;
                    i += 1;
                } else if c == b'r'
                    && matches!(bytes.get(i + 1), Some(b'"') | Some(b'#'))
                    && !bytes
                        .get(i.wrapping_sub(1))
                        .is_some_and(|p| p.is_ascii_alphanumeric() || *p == b'_')
                {
                    // 原始串开头：`r"` 或 `r#`…（左侧不能是标识符，避免把 `bar#` 之类误判）
                    let mut n = 0usize;
                    let mut j = i + 1;
                    while bytes.get(j) == Some(&b'#') {
                        n += 1;
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'"') {
                        raw_hashes = Some(n);
                        i = j;
                    }
                } else if c == b'\'' {
                    // P52：字符字面量 `b'{'` / `'\''` —— 里面的花括号不是结构
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                i += 1;
            }
            let Some(e) = end else {
                // 配平失败（源码被截断之类）：别把剩余部分全挖掉，直接收工。
                return body;
            };
            // **等长替换成空白**（保留换行）：这样下面按字符扫出来的行号
            // 仍然是**原文件**的行号，报错信息才能直接对上源码。
            let mut blanked = String::with_capacity(e - at);
            for ch in body[at..e].chars() {
                blanked.push(if ch == '\n' { '\n' } else { ' ' });
            }
            body = format!("{}{}{}", &body[..at], blanked, &body[e..]);
        }
    }

    /// **纯文本扫描**：返回 `(行号, 字符串内容)` 列表。
    ///
    /// 实现要求（缺一就会误报/漏报，别改坏）：
    /// * 注释：整行 `//` 直接跳过；`/* */` 按块跳过（本项目的块注释很短，逐字符处理足够）；
    /// * 字符串：常规 `"…"`（含 `\"` 转义）、字节串 `b"…"`、原始串 `r"…"` / `r#"…"#`；
    /// * 日志宏：**跳过整条宏调用**（从 `tracing::` 起做括号配平到平衡为止），
    ///   因为宏里同时有中文**格式串**和多个参数，逐字符"看前后 400 字节有没有 `tracing::`"
    ///   那种启发式会在长宏上漏判；
    /// * `#[cfg(test)] mod tests {`：按花括号配平整块跳过。
    fn scan(text: &str) -> Vec<(usize, String)> {
        // ---- 第 1 步：把 `#[cfg(test)] mod tests { … }` 整块挖空（花括号配平）----
        let body = blank_test_blocks(text);

        // ---- 第 2 步：逐字符找"真字符串字面量 + 行号"，跳过注释与 tracing 宏 ----
        let b = body.as_bytes();
        let mut out = Vec::new();
        let mut line = 1usize;
        let mut i = 0usize;
        let mut block_comment = false;
        while i < b.len() {
            // **字符边界兜底**（这个扫描器的"不 panic"底线）。
            //
            // 正常情况下游标永远落在字符边界上；但只要分词器有任何一个状态机 bug
            //（上一版就有一个：CRLF 让"跳过测试模块"失效，于是测试代码里的 `contains('中')`
            // 把游标推进了多字节字符中间），`&body[i..]` 就会
            // `byte index … is not a char boundary` **直接 panic** —— 扫描器一崩，
            // 整个 i18n 防回归就等于不存在。所以这里宁可"跳过这一字节、继续扫"，
            // 也不让扫描器自己炸掉；`CAPLITE_SCAN_DEBUG=1` 时把现场打出来便于定位分词器 bug。
            if !body.is_char_boundary(i) {
                if std::env::var_os("CAPLITE_SCAN_DEBUG").is_some() {
                    let lo = (0..=i)
                        .rev()
                        .find(|k| body.is_char_boundary(*k))
                        .unwrap_or(0);
                    eprintln!(
                        "[scan] 游标落在字符中间（分词器有 bug）：i={i} line={line} 上文={:?}",
                        &body[lo.saturating_sub(60)..lo]
                    );
                }
                i += 1;
                continue;
            }
            let c = b[i];
            if c == b'\n' {
                line += 1;
                i += 1;
                continue;
            }
            // 块注释
            if block_comment {
                if c == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    block_comment = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if c == b'/' && i + 1 < b.len() {
                if b[i + 1] == b'/' {
                    // 行注释：跳到行尾
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if b[i + 1] == b'*' {
                    block_comment = true;
                    i += 2;
                    continue;
                }
            }
            // 字符字面量 `'{'` 之类：跳过，免得它被当成块注释/字符串的一部分。
            //
            // ⚠️ 必须区分**生命周期**（`'static`、`&'a str`）与**字符字面量**（`'{'`、`'\''`）：
            // 两者都以 `'` 开头，只看"下一个 `'` 在哪"会把 `'static` 一直吞到后面某个 `'`，
            // 途中真正的字符串（含中文）就被整段跳过了 —— 那是**漏报**，比误报更危险。
            // 判据：Rust 的字符字面量最多 4 字节（`'a'` / `'\''` / `'\u{7f}'` 是 8 字节，
            // 但那只在源码里写成 `'\u{…}'`，本仓没有）。所以"闭引号出现在 8 字节内"就按
            // 字符字面量跳，否则按生命周期只前进 1 字节。
            if c == b'\'' {
                let mut j = i + 1;
                if j < b.len() && b[j] == b'\\' {
                    j += 2;
                }
                let closes_within = j + 1 < b.len() && b[j + 1] == b'\'' && j - i <= 8;
                if closes_within {
                    i = j + 2;
                } else {
                    i += 1;
                }
                continue;
            }
            // logging 宏：整条跳过（从 `tracing::` 到括号配平结束）
            if !block_comment
                && body[i..].starts_with("tracing::")
                && ["debug!", "info!", "warn!", "error!", "trace!"]
                    .iter()
                    .any(|m| body[i + "tracing::".len()..].starts_with(m))
            {
                // 找到宏名后的第一个 '('，再配平到它的 ')'
                let mut j = i;
                while j < b.len() && b[j] != b'(' {
                    j += 1;
                }
                let mut depth = 0i32;
                let mut in_str = false;
                while j < b.len() {
                    let d = b[j];
                    if d == b'\n' {
                        line += 1;
                    } else if in_str {
                        if d == b'\\' {
                            j += 2;
                            continue;
                        }
                        if d == b'"' {
                            in_str = false;
                        }
                    } else if d == b'"' {
                        in_str = true;
                    } else if d == b'(' {
                        depth += 1;
                    } else if d == b')' {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // 字节串 / 原始串前缀：跳过前缀字母，落到 `"` 上由下面统一处理
            if (c == b'b' || c == b'r') && i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'r')
            {
                // 只处理 `b"` 与 `r"` / `r#"`；`br"` 由下一轮循环落到 `r`
                if c == b'b' && b[i + 1] == b'"' {
                    i += 1;
                    continue;
                }
            }
            if c == b'r' && i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'#') {
                // 原始字符串：`r"…"` 或 `r#"…"#`（`#` 的个数任意）
                let mut hashes = 0usize;
                let mut j = i + 1;
                while j < b.len() && b[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if j < b.len() && b[j] == b'"' {
                    j += 1;
                    let start = j;
                    let mut end = None;
                    while j < b.len() {
                        if b[j] == b'"' {
                            let mut k = j + 1;
                            let mut seen = 0usize;
                            while k < b.len() && b[k] == b'#' && seen < hashes {
                                seen += 1;
                                k += 1;
                            }
                            if seen == hashes {
                                end = Some(j);
                                j = k;
                                break;
                            }
                        } else if b[j] == b'\n' {
                            line += 1;
                        }
                        j += 1;
                    }
                    if let Some(e) = end {
                        out.push((line, body[start..e].to_string()));
                    }
                    i = j;
                    continue;
                }
            }
            // 常规字符串
            if c == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < b.len() {
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' {
                        break;
                    }
                    if b[j] == b'\n' {
                        line += 1;
                    }
                    j += 1;
                }
                let end = j.min(b.len());
                out.push((line, body[start..end].to_string()));
                i = end + 1;
                continue;
            }
            i += 1;
        }
        out
    }

    /// **界面源码里不许有硬编码中文**（用户报过的 bug 的防回归）。
    #[test]
    fn no_hardcoded_chinese_in_ui_sources() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut bad: Vec<String> = Vec::new();
        for rel in SCANNED {
            let path = root.join(rel);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("读不到 {}：{e}", path.display()));
            // 行内容快照：用来判"这一行是否在 `LINE_ALLOW` 里"
            let src_lines: Vec<&str> = text.lines().collect();
            for (line, lit) in scan(&text) {
                if !lit.chars().any(is_cjk) {
                    continue;
                }
                let here = src_lines.get(line.saturating_sub(1)).copied().unwrap_or("");
                if LINE_ALLOW.iter().any(|mark| here.contains(mark)) {
                    continue;
                }
                if LINE_ALLOW_PAIRS
                    .iter()
                    .any(|(a, b)| here.contains(a) && here.contains(b))
                {
                    continue;
                }
                bad.push(format!("{rel}:{line}: \"{lit}\""));
            }
        }
        assert!(
            bad.is_empty(),
            "界面源码里出现了硬编码中文（请改成 i18n 键，并在 zh-CN.json / en-US.json 同步补齐）：\n{}",
            bad.join("\n")
        );
    }

    /// **界面层不许出现 panic 型调用**（P52 新增）。
    ///
    /// 为什么单独盯 `src/ui/`：这里的输入几乎全是**用户数据**（选区、文件、设备列表、
    /// 用户拖出来的分段顺序……），"这个不可能失败"的假设最不可靠；而一旦
    /// `unwrap()` / `expect()` 真的在绘制路径上触发，崩的是**整个进程** ——
    /// 用户正在录的东西跟着一起没。生产代码该走 `AppError`（`src/mux`、`src/edit`
    /// 里"不该发生的状态也返回 AppError"就是既有写法）。
    ///
    /// 两类例外：
    /// * 测试块 —— `blank_test_blocks` 会整块挖成空白，测试里 panic 正是想要的行为；
    /// * 注释行，以及本文件里那张 lint 记号表（`".expect("` 在那儿只是个**字符串**）。
    #[test]
    fn ui_layer_has_no_panic_calls() {
        const TOKENS: &[&str] = &[
            ".unwrap()",
            ".expect(",
            "panic!(",
            "unreachable!(",
            "todo!(",
            "unimplemented!(",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = root.join("src/ui");
        let mut bad: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("读不到 src/ui：{e}")) {
            let path = entry.expect("读目录项失败").path();
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .display()
                .to_string()
                .replace('\\', "/");
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读不到 {rel}：{e}"));
            checked += 1;
            // `#[cfg(test)] mod …` 整块挖成空白（**保留换行** → 行号仍对得上原文件）
            let blanked = blank_test_blocks(&text);
            for (i, line) in blanked.lines().enumerate() {
                let t = line.trim();
                if t.starts_with("//") || t.starts_with('*') {
                    continue;
                }
                // 本文件里的 lint 记号表自己含 `.expect(` 字样（是字符串，不是调用）
                if line.contains("LINE_ALLOW_PAIRS") {
                    continue;
                }
                if let Some(tok) = TOKENS.iter().find(|tok| line.contains(**tok)) {
                    bad.push(format!("{rel}:{}: 含 `{tok}` → {t}", i + 1));
                }
            }
        }
        assert!(
            checked >= 5,
            "只扫到 {checked} 个文件，`src/ui` 目录结构可能已变（扫描逻辑失效会假装通过）"
        );
        assert!(
            bad.is_empty(),
            "界面层（src/ui/）出现了 panic 型调用 —— 在这里崩掉会带走整个进程，\
             请改成返回 AppError 或用 `if let` 接住：\n{}",
            bad.join("\n")
        );
    }

    /// **字符下标 ≠ 字节下标** —— P52 修的那个真 bug 的防回归。
    ///
    /// 旧实现从 `char_indices()` 拿到的是**字符下标**，却被 [`blank_test_blocks`]
    /// 当成**字节下标**去索引 `as_bytes()`。源码里中文越多偏得越多
    /// （`src/ui/startup.rs` 上实测差 **7,123 字节**），于是扫描提前 7 KB 起步、
    /// 撞上无关的 `{`/`}` 就提前收尾：`mod tests` 只被挖掉开头三十来行，
    /// 剩下的测试代码一直被当成生产代码扫。
    #[test]
    fn blank_test_blocks_is_byte_accurate_with_multibyte_text() {
        // 前面塞满多字节字符，故意拉开"字符下标"与"字节下标"的差距
        let mut sample = String::new();
        for _ in 0..200 {
            // 注释里带花括号是常态（`{x}` 这类文档示例），正是它们让"下标偏了"变成真错
            sample.push_str("// 中文注释占位 {x}：拉开字符下标与字节下标的差距\n");
        }
        sample.push_str("#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { let _ = \"PROBE\"; }\n}\n");
        let blanked = blank_test_blocks(&sample);
        let lines: Vec<&str> = blanked.lines().collect();
        assert!(
            !blanked.contains("PROBE"),
            "多字节文本下 `#[cfg(test)] mod` 没被整块挖空（字符下标被当成字节下标了）"
        );
        // 反过来：模块**外**的正常源码必须原样留着，不能误挖
        assert!(
            lines[0].contains("中文注释占位"),
            "挖空时把模块外的正常源码一起挖掉了：{:?}",
            lines[0]
        );
    }

    /// **本仓真实换行风格**下的两条防回归（这就是上一版扫描器翻车的地方）。
    ///
    /// `editor.rs` / `main_page.rs` / `settings.rs` 是 **CRLF**（Windows 检出），而旧实现把
    /// "`#[cfg(test)]` 与 `mod tests {` 之间的换行"写死成 `"\n"` → 三个文件上**一次都没跳过**
    /// 测试模块。后果有两条，本条测试各钉一个：
    /// 1. **误报**：测试断言里的中文被当成"界面硬编码中文"；
    /// 2. **崩溃**：测试代码里的 `contains('…')` 让字节游标落在多字节字符中间，
    ///    旧实现在那里做 `&body[i..]` → `byte index … is not a char boundary` 直接 panic。
    #[test]
    fn scanner_handles_crlf_sources_without_panicking_or_false_positives() {
        // ① CRLF + 缩进：整块测试模块必须被挖空（断言里的中文不算界面文案）
        let crlf = "fn ui() {\r\n    let s = \"界面文案\";\r\n}\r\n\r\n#[cfg(test)]\r\nmod tests {\r\n    #[test]\r\n    fn t() {\r\n        assert!(x.contains('中'), \"断言里的中文\");\r\n    }\r\n}\r\n";
        let found = scan(crlf);
        let all: Vec<&str> = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            all.contains(&"界面文案"),
            "CRLF 源码里正常字符串没被抓到：{all:?}"
        );
        assert!(
            !all.iter().any(|s| s.contains("断言里的中文")),
            "CRLF 源码里的 #[cfg(test)] mod tests 块没被跳过：{all:?}"
        );

        // ② 与被扫文件同款：CRLF + 缩进的 `#[cfg(test)]`，后面还有正常源码 ——
        //    游标不得落在多字节字符中间（旧的 `&body[i..]` 在这里 panic）
        let crlf2 = "fn f() -> String {\r\n    let msg = \"提示\";\r\n    msg.to_string()\r\n}\r\n#[cfg(test)]\r\nmod tests {\r\n    #[test]\r\n    fn t() {\r\n        let e = \"x\";\r\n        assert!(e.contains('中'), \"x\");\r\n    }\r\n}\r\n";
        let found2 = scan(crlf2);
        let all2: Vec<&str> = found2.iter().map(|(_, s)| s.as_str()).collect();
        assert!(all2.contains(&"提示"), "CRLF 第二个样本漏抓：{all2:?}");

        // ③ 多个 tests 模块：**每一个**都要跳过（旧实现只挖第一处）
        let two = "#[cfg(test)]\nmod tests { fn a() { let _ = \"甲\"; } }\nfn real() { let s = \"界面\"; }\n#[cfg(test)]\nmod tests { fn b() { let _ = \"乙\"; } }\n";
        let found3 = scan(two);
        let all3: Vec<&str> = found3.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            all3.contains(&"界面") && !all3.iter().any(|s| *s == "甲" || *s == "乙"),
            "多个 tests 模块没有全部跳过：{all3:?}"
        );

        // ④ `#[cfg(test)]` 用在别的条目上（不是 mod tests）时，**不能**把它后面的正常源码吞掉
        let attr = "#[cfg(test)]\n#[derive(Debug)]\nstruct T;\nfn real() { let s = \"界面\"; }\n";
        let found4 = scan(attr);
        assert!(
            found4.iter().any(|(_, s)| s == "界面"),
            "`#[cfg(test)]` 用在非 mod 条目上时把后面的正常源码吞掉了：{found4:?}"
        );

        // ⑤ 挖空必须**等长**（行号要能直接对上源码）
        assert_eq!(
            blank_test_blocks(crlf2).lines().count(),
            crlf2.lines().count(),
            "挖空测试模块后行数变了，报错行号就对不上源码了"
        );
    }

    /// 扫描器自身的**最小可信度**检查：它必须真的能抓到中文、也真的会跳过注释/日志/测试。
    ///
    /// 没有这条，"扫描器写坏了 → 永远返回空 → 测试永远绿"这种最糟的失效模式就没人发现。
    #[test]
    fn scanner_actually_catches_and_skips() {
        // ⚠️ 样本里**故意**放了一个生命周期（`&'static str`）：它后面的字符串必须仍被抓到，
        //    否则"把生命周期误当字符字面量、一路吞掉后面的字符串"这种漏报就测不出来。
        let sample = r#"
// 注释里的中文不该被抓到
/// 文档注释里的中文也不该
/* 块注释
   里的中文 */
fn f() -> &'static str {
    let a = "硬编码中文";           // 必须被抓到
    let tail: &'static str = "生命周期之后的硬编码中文";
    tracing::warn!("日志里的中文没问题");
    tracing::info!(
        error = %e,
        "多行日志宏里的中文也没问题：{}",
        x
    );
    let b = r"原始串里的中文";
    let c = b"byte string ascii";
    let d = a.len() + tail.len();
    let _ = (b, c, d);
    "函数尾部的硬编码中文"
}
#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert!(true, "测试断言里的中文不算"); }
}
"#;
        let found: Vec<String> = scan(sample).into_iter().map(|(_, s)| s).collect();
        assert!(
            found.iter().any(|s| s == "硬编码中文"),
            "扫描器漏掉了普通字符串里的中文：{found:?}"
        );
        assert!(
            found.iter().any(|s| s.contains("生命周期之后的硬编码中文")),
            "扫描器被生命周期骗了（漏掉了它后面的字符串）：{found:?}"
        );
        assert!(
            found.iter().any(|s| s.contains("函数尾部的硬编码中文")),
            "扫描器漏掉了函数尾部/测试模块之前的字符串：{found:?}"
        );
        assert!(
            found.iter().any(|s| s.contains("原始串里的中文")),
            "扫描器漏掉了原始字符串：{found:?}"
        );
        assert!(
            !found.iter().any(|s| s.contains("注释")),
            "扫描器把注释当成了字符串：{found:?}"
        );
        assert!(
            !found.iter().any(|s| s.contains("日志")),
            "扫描器把 tracing 宏里的中文当成了硬编码：{found:?}"
        );
        assert!(
            !found.iter().any(|s| s.contains("测试断言")),
            "扫描器没有跳过 #[cfg(test)] mod tests 块：{found:?}"
        );
    }
}
