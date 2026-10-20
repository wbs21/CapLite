//! 路径解析与"目录可用性探测"。
//!
//! 为什么单独抽一个模块：双击 exe 时**当前工作目录可能是 `C:\Windows\System32`**，
//! 而 `%APPDATA%` / `用户\视频` 这类目录也可能因为 ACL、漫游策略、被重定向到只读盘，
//! 或者**进程的可写范围被限制在工作区内**而写不进去（实测报 `os error 5 拒绝访问`）。
//! 这些都属于"环境问题"，必须①有确定的回退顺序、②失败时带上**具体路径 + 系统错误**。
//!
//! 因此这里只放**纯逻辑 + 探测**：
//! * [`choose_writable`] / [`choose_config_dir`] 是纯函数（探测行为用闭包注入），
//!   可以脱离真实文件系统单测；
//! * 所有拼接都用 [`PathBuf::join`]，绝不手工拼字符串（用户名 `Limit li` 带空格，
//!   手拼字符串迟早拼出 `C:\Users\Limit li\...` 被截断这类问题）；
//! * 所有相对路径都以 **exe 目录**为基准解析，不依赖 CWD。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 回退到 exe 同级时使用的子目录名（`<exe 目录>\cap-recordings`）。
pub(crate) const EXE_SUBDIR: &str = "cap-recordings";
/// 配置目录在"用户目录"下的子目录名（`用户\Documents\cap`）。
pub(crate) const USER_CONFIG_SUBDIR: &str = "cap";
/// 配置文件名（与 [`super::CONFIG_FILE`] 保持一致，这里给纯逻辑用）。
pub const CONFIG_FILE: &str = "config.json";

/// 一次"探测过的目录"记录：无论成败都留下来，用于拼出可操作的错误信息。
#[derive(Debug, Clone)]
pub(crate) struct ProbeAttempt {
    /// 探测过的目录
    pub dir: PathBuf,
    /// 探测过程中第一个失败的原因（`None` = 该目录可用）
    pub error: Option<String>,
}

impl ProbeAttempt {
    /// `目录（原因）`，用于错误信息；成功项只显示目录。
    pub fn render(&self) -> String {
        match &self.error {
            Some(e) => format!("{}（{e}）", self.dir.display()),
            None => self.dir.display().to_string(),
        }
    }
}

/// 目录选择的最终方案。
#[derive(Debug, Clone)]
pub struct DirPlan {
    /// 真正要用的目录；全部候选都不可用时为空
    pub chosen: PathBuf,
    /// 原定目录（用户/配置期望的那个）：`None` = 首选就可用，无需提示回退
    pub fell_back_from: Option<PathBuf>,
    /// 所有候选都不可用时的详细说明（含每个候选的具体错误）
    pub error: Option<String>,
}

impl DirPlan {
    /// 是否发生了回退（界面据此给"原目录不可写，已改用 X"的提示）
    pub fn fell_back(&self) -> bool {
        self.fell_back_from.is_some()
    }

    /// 选中的目录；全部候选都不可用时是 `None`（此时看 [`DirPlan::error`]）。
    ///
    /// 用方法而不是让调用方读字段，是为了避免"空 PathBuf 到底算不算没选中"这种歧义。
    pub fn chosen_path(&self) -> Option<&Path> {
        if self.chosen.as_os_str().is_empty() {
            None
        } else {
            Some(&self.chosen)
        }
    }
}

/// 探测一个目录能否"创建 + 写入 + 删除"。
///
/// 为什么必须真写一个文件而不是查 ACL：Windows 上 ACL 允许不代表能写
/// （只读属性、配额、被占用、被虚拟化重定向都会让写入失败），而"能写"才是我们真正需要的结论。
/// 临时文件用进程 id + 时间戳命名，避免多实例互相覆盖；无论成败都会尝试删掉自己。
pub fn probe_dir_writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!(".cap-write-probe-{}-{nanos}.tmp", std::process::id());
    let probe = dir.join(name);
    // create_new 而不是 write：探测文件也不能跟随预先放好的链接。
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(f) => f,
        Err(e) => return Err(format!("创建探测文件失败：{e}")),
    };
    use std::io::Write as _;
    if let Err(e) = f.write_all(b"cap-write-probe") {
        drop(f);
        let _ = std::fs::remove_file(&probe);
        return Err(format!("写入探测文件失败：{e}"));
    }
    drop(f);
    if let Err(e) = std::fs::remove_file(&probe) {
        // 删不掉说明目录权限有问题（能写不能删），同样视为不可用
        return Err(format!("删除探测文件失败：{e}"));
    }
    Ok(())
}

/// 带超时的可写探测：网络盘/休眠移动盘上的 `create_dir_all` 可能卡几十秒，
/// 启动路径不能无限等。探测线程超时后仍在后台，但主流程继续回落。
pub fn probe_dir_writable_timeout(dir: &Path) -> Result<(), String> {
    probe_dir_writable_timeout_with_budget(dir, Duration::from_millis(1500))
}

/// 带调用方预算版本：配置目录回退链共享一个总预算，避免每个候选都重新等 1.5 s。
pub fn probe_dir_writable_timeout_with_budget(
    dir: &Path,
    budget: Duration,
) -> Result<(), String> {
    if budget.is_zero() {
        return Err(format!("目录探测预算已耗尽：{}", dir.display()));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let p = dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("cfg-probe".into())
        .spawn(move || {
            let _ = tx.send(probe_dir_writable(&p));
        });
    if spawned.is_err() {
        return Err("探测线程创建失败".into());
    }
    match rx.recv_timeout(budget) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(format!(
                "目录探测超过 {} ms：{}",
                budget.as_millis(),
                dir.display()
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(format!("目录探测线程异常退出：{}", dir.display()))
        }
    }
}

/// 依次探测候选目录，返回第一个可用者的下标；全部不可用时返回**失败原因列表**。
///
/// `probe` 是注入的探测函数：真实调用传 [`probe_dir_writable`]，单测里传假实现，
/// 这样"回退链"本身可以脱离文件系统被验证。
pub(crate) fn choose_writable<F>(
    candidates: &[PathBuf],
    probe: F,
) -> Result<(usize, Vec<ProbeAttempt>), Vec<ProbeAttempt>>
where
    F: Fn(&Path) -> Result<(), String>,
{
    let mut tried: Vec<ProbeAttempt> = Vec::with_capacity(candidates.len());
    for (index, dir) in candidates.iter().enumerate() {
        match probe(dir) {
            Ok(()) => {
                tried.push(ProbeAttempt { dir: dir.clone(), error: None });
                return Ok((index, tried));
            }
            Err(e) => tried.push(ProbeAttempt { dir: dir.clone(), error: Some(e) }),
        }
    }
    Err(tried)
}

/// 常见回退目录（**输出目录**用，配置里的目录由调用方放在最前面）：
/// `用户\视频\cap` → `用户\文档\cap` → `<exe 目录>\cap-recordings` → 系统临时目录。
///
/// 每一项都用 `join` 拼；拿不到用户目录（例如剥掉环境变量的服务账号）时直接跳过，
/// 而不是塞一个"."进去——"." 是相对路径，双击 exe 时会指到 `System32`。
pub fn default_fallback_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(u) = directories::UserDirs::new() {
        let home = u.home_dir().to_path_buf();
        dirs.push(u.video_dir().unwrap_or(home.as_path()).join("cap"));
        dirs.push(u.document_dir().unwrap_or(home.as_path()).join("cap"));
    } else if let Some(profile) = std::env::var_os("USERPROFILE") {
        let home = PathBuf::from(profile);
        dirs.push(home.join("Videos").join("cap"));
        dirs.push(home.join("Documents").join("cap"));
    }
    if let Some(exe_dir) = exe_dir() {
        dirs.push(exe_dir.join(EXE_SUBDIR));
    }
    dirs.push(std::env::temp_dir().join(EXE_SUBDIR));
    dirs
}

/// **配置目录**的首选位置：`%USERPROFILE%\Documents\cap`。
///
/// 为什么不再首选 `%APPDATA%`：实测在受限环境（进程可写范围被限制在工作区）里
/// "创建 AppData 下的目录"直接被拒（`os error 5`），而用户目录下的目录同样常见、
/// 可见、可备份，且失败时更容易被用户理解和修改。
pub fn preferred_config_dir() -> Option<PathBuf> {
    if let Some(u) = directories::UserDirs::new() {
        let home = u.home_dir();
        return Some(u.document_dir().unwrap_or(home).join(USER_CONFIG_SUBDIR));
    }
    std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Documents").join(USER_CONFIG_SUBDIR))
}

/// **配置目录**的兜底位置：`<exe 目录>\cap-recordings` → 系统临时目录。
///
/// 之所以先放 exe 同级：便携（绿色）用法下用户期望配置跟着程序走；
/// 临时目录放最后，因为重启后可能被清理，只当作"总比完全存不了强"。
pub fn fallback_config_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(exe_dir) = exe_dir() {
        dirs.push(exe_dir.join(EXE_SUBDIR));
    }
    dirs.push(std::env::temp_dir().join(EXE_SUBDIR));
    dirs
}

/// 选择配置目录：`显式指定的目录` → `首选目录（用户\Documents\cap）` → 兜底目录。
///
/// 返回的 [`DirPlan`] 里 `fell_back_from` 只记录**首选目录**（用户真正期望的那个），
/// 这样界面上的一句提示就能说清"原本想用哪、为什么没用成、现在用哪"。
pub fn choose_config_dir(
    explicit: Option<&Path>,
    preferred: Option<PathBuf>,
    fallbacks: Vec<PathBuf>,
    probe: &dyn Fn(&Path) -> Result<(), String>,
) -> DirPlan {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 显式指定优先，且它是绝对路径（解析在调用方做），不参与"回退提示"
    let explicit_owned = explicit.map(|p| p.to_path_buf());
    if let Some(p) = &explicit_owned {
        candidates.push(p.clone());
    }
    if let Some(p) = &preferred {
        if !candidates.contains(p) {
            candidates.push(p.clone());
        }
    }
    for dir in fallbacks {
        if !candidates.contains(&dir) {
            candidates.push(dir);
        }
    }

    match choose_writable(&candidates, probe) {
        Ok((index, _tried)) => {
            // 选中的就是"显式指定"或"首选"时不提示；否则说明首选被跳过了
            let picked = &candidates[index];
            let is_explicit = explicit_owned.as_ref().is_some_and(|p| p == picked);
            let is_preferred = preferred.as_ref().is_some_and(|p| p == picked);
            let fell_back_from = if is_explicit || is_preferred { None } else { preferred };
            DirPlan { chosen: picked.clone(), fell_back_from, error: None }
        }
        Err(tried) => DirPlan {
            chosen: PathBuf::new(),
            fell_back_from: preferred,
            error: Some(format!(
                "没有任何可写的配置目录，已尝试：{}",
                render_attempts(&tried)
            )),
        },
    }
}

/// 运行中 exe 所在目录。这是"不依赖工作目录"的基准点。
pub fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(|p| p.to_path_buf())
}

/// 把用户填的目录解析成**绝对路径**。
///
/// * 空 → 错误（调用方应回退到回退链）；
/// * 相对路径 → 以 **exe 目录**为基准解析（双击 exe 时 CWD 可能是 `System32`，
///   用 CWD 当基准会把 `cap` 解析到 `C:\Windows\System32\cap`）。
pub fn resolve_against_exe_dir(value: &str) -> Result<PathBuf, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("目录为空".to_string());
    }
    let raw = PathBuf::from(trimmed);
    if raw.is_absolute() {
        return Ok(raw);
    }
    let base = exe_dir().or_else(|| std::env::current_dir().ok()).ok_or_else(|| {
        format!("相对路径 {trimmed} 无法解析：既拿不到 exe 所在目录，也拿不到当前目录")
    })?;
    Ok(base.join(raw))
}

/// 输入：配置里写的输出目录（可能是空、相对路径或绝对路径）。
/// 输出：最终使用的目录 + 回退信息（或"全部不可用"的详细错误）。
///
/// 顺序严格按规格：`配置里的 output_dir` → `用户\视频\cap` → `用户\文档\cap`
/// → `<exe 目录>\cap-recordings` → 系统临时目录。
pub fn plan_output_dir(configured: &str, probe: &dyn Fn(&Path) -> Result<(), String>) -> DirPlan {
    // 候选表：第 0 项是配置值（解析后的绝对路径），后面是回退链。
    let configured_resolved: Option<PathBuf> = resolve_against_exe_dir(configured).ok();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = &configured_resolved {
        candidates.push(p.clone());
    }
    for dir in default_fallback_dirs() {
        if !candidates.contains(&dir) {
            candidates.push(dir);
        }
    }

    match choose_writable(&candidates, probe) {
        Ok((index, _tried)) => {
            // index==0 表示配置里的值本身可用；否则回退到了第 index 个候选
            let fell_back_from =
                if index == 0 { None } else { configured_resolved.clone() };
            DirPlan { chosen: candidates[index].clone(), fell_back_from, error: None }
        }
        Err(tried) => DirPlan {
            chosen: PathBuf::new(),
            fell_back_from: configured_resolved,
            error: Some(format!(
                "没有任何可写的输出目录，已尝试：{}",
                render_attempts(&tried)
            )),
        },
    }
}

/// 把探测记录拼成"一路试过哪些目录、各自为什么不行"的单行摘要。
pub(crate) fn render_attempts(attempts: &[ProbeAttempt]) -> String {
    if attempts.is_empty() {
        return "（没有候选目录）".to_string();
    }
    attempts.iter().map(|a| a.render()).collect::<Vec<_>>().join("；")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// 正常情况：第一个候选可用就选它，不做任何多余探测。
    #[test]
    fn picks_first_writable_candidate() {
        let cands = vec![pb(r"C:\a"), pb(r"C:\b"), pb(r"C:\c")];
        let probe = |p: &Path| {
            if p == Path::new(r"C:\a") {
                Ok(())
            } else {
                Err("不该被探测".to_string())
            }
        };
        let (index, tried) = choose_writable(&cands, probe).expect("第一个候选可用时必须成功");
        assert_eq!(index, 0);
        assert_eq!(tried.len(), 1, "第一个就成功，不应继续探测后面的候选");
        assert!(tried[0].error.is_none());
    }

    /// 第一个不可写 → 回退到下一个，并保留失败原因供界面展示。
    #[test]
    fn falls_back_to_next_when_first_is_not_writable() {
        let cands = vec![pb(r"C:\a"), pb(r"C:\b"), pb(r"C:\c")];
        let probe = |p: &Path| {
            if p == Path::new(r"C:\b") {
                Ok(())
            } else {
                Err(format!("拒绝访问：{}", p.display()))
            }
        };
        let (index, tried) = choose_writable(&cands, probe).expect("第二个候选可用时应成功");
        assert_eq!(index, 1);
        assert_eq!(tried.len(), 2, "应停在第一个可用者，不再探测 C:\\c");
        assert_eq!(tried[0].error.as_deref(), Some(r"拒绝访问：C:\a"));
        assert!(tried[1].error.is_none());
    }

    /// 全部不可写 → 返回错误，且错误里必须能看到每一个候选目录与各自的原因。
    #[test]
    fn returns_detailed_error_when_all_candidates_fail() {
        let cands = vec![pb(r"C:\a"), pb(r"C:\b")];
        let probe = |p: &Path| Err(format!("创建目录失败：拒绝访问 {}", p.display()));
        let tried = choose_writable(&cands, probe).expect_err("全部不可写时必须是 Err");
        assert_eq!(tried.len(), 2);
        let text = render_attempts(&tried);
        assert!(text.contains(r"C:\a"), "错误信息缺少第一个候选：{text}");
        assert!(text.contains(r"C:\b"), "错误信息缺少第二个候选：{text}");
        assert!(text.contains("拒绝访问"), "错误信息缺少系统原因：{text}");
    }

    /// 空字符串与相对路径：都必须先变成绝对路径，绝不留相对路径给运行期。
    #[test]
    fn empty_and_relative_output_dir_are_resolved_to_absolute() {
        assert!(resolve_against_exe_dir("").is_err(), "空目录必须报错，交给回退链");
        assert!(resolve_against_exe_dir("   ").is_err(), "全空白同样视为空");

        let abs =
            resolve_against_exe_dir(r"C:\Users\Limit li\Videos\cap").expect("绝对路径直接返回");
        assert_eq!(abs, pb(r"C:\Users\Limit li\Videos\cap"));

        // 相对路径以 exe 目录为基准（不依赖 CWD），且带空格的目录名不被截断
        let rel = resolve_against_exe_dir(r"sub dir\cap").expect("相对路径应能解析成绝对路径");
        assert!(rel.is_absolute(), "相对路径必须被转成绝对路径：{}", rel.display());
        assert!(rel.ends_with(r"sub dir\cap"), "相对部分不应被改写：{}", rel.display());
        if let Some(exe_dir) = exe_dir() {
            assert!(rel.starts_with(&exe_dir), "相对路径应以 exe 目录为基准：{}", rel.display());
        }
    }

    /// 输出目录回退链：配置值不可写 → 回退到链上的下一项，并说明"从哪回退"。
    #[test]
    fn output_plan_falls_back_with_explanation() {
        // 全部候选都不可用 → 必须给出错误，而不是悄悄用一个不存在的目录
        let plan = plan_output_dir(r"C:\nope\out", &|_p: &Path| Err("拒绝访问".to_string()));
        assert!(plan.error.is_some(), "全部不可用时必须带错误说明");
        assert!(plan.chosen.as_os_str().is_empty());

        // 只让"配置值"失败，其余放行 → 应回退到回退链第一项且带上"原目录"
        let plan = plan_output_dir(r"C:\nope\out", &|p: &Path| {
            if p == Path::new(r"C:\nope\out") {
                Err("拒绝访问".to_string())
            } else {
                Ok(())
            }
        });
        assert!(plan.fell_back(), "必须标记发生了回退");
        assert_eq!(plan.fell_back_from.as_deref(), Some(Path::new(r"C:\nope\out")));
        assert!(plan.error.is_none(), "有可用候选时不应报错");
        assert!(!plan.chosen.as_os_str().is_empty());
        assert_ne!(plan.chosen, pb(r"C:\nope\out"));
    }

    /// 配置目录回退链：显式指定优先；首选不可写时退到兜底，并记录"原定目录"。
    #[test]
    fn config_dir_prefers_explicit_then_preferred_then_fallback() {
        let explicit = pb(r"C:\explicit\cfg");
        let preferred = pb(r"C:\Users\Limit li\Documents\cap");
        let fallback = vec![pb(r"C:\exe\cap-recordings"), pb(r"C:\Temp\cap")];

        // 1) 显式指定的目录可用 → 用它，且不该提示"回退"
        let ok_all = |_p: &Path| Ok(());
        let plan = choose_config_dir(Some(&explicit), Some(preferred.clone()), fallback.clone(), &ok_all);
        assert_eq!(plan.chosen, explicit);
        assert!(!plan.fell_back(), "显式指定可用时不需要回退提示");

        // 2) 显式 + 首选都不可写 → 退到兜底第一项，原定目录=首选
        let plan = choose_config_dir(
            Some(&explicit),
            Some(preferred.clone()),
            fallback.clone(),
            &|p: &Path| {
                if p == Path::new(r"C:\exe\cap-recordings") {
                    Ok(())
                } else {
                    Err("拒绝访问 (os error 5)".to_string())
                }
            },
        );
        assert_eq!(plan.chosen, pb(r"C:\exe\cap-recordings"));
        assert_eq!(plan.fell_back_from.as_deref(), Some(preferred.as_path()));
        assert!(plan.error.is_none());

        // 3) 只给了首选目录且不可写 → 退到临时目录
        let plan = choose_config_dir(None, Some(preferred.clone()), fallback, &|p: &Path| {
            if p == Path::new(r"C:\Temp\cap") {
                Ok(())
            } else {
                Err("拒绝访问 (os error 5)".to_string())
            }
        });
        assert_eq!(plan.chosen, pb(r"C:\Temp\cap"));
        assert_eq!(plan.fell_back_from.as_deref(), Some(preferred.as_path()));

        // 4) 全不可写 → 错误信息里必须出现每个候选与系统原因
        let plan = choose_config_dir(None, Some(preferred.clone()), vec![pb(r"C:\Temp\cap")], &|_p: &Path| {
            Err("创建目录失败：拒绝访问。 (os error 5)".to_string())
        });
        assert!(plan.chosen.as_os_str().is_empty());
        let text = plan.error.expect("全不可写必须报错");
        assert!(text.contains(r"C:\Users\Limit li\Documents\cap"), "{text}");
        assert!(text.contains(r"C:\Temp\cap"), "{text}");
        assert!(text.contains("os error 5"), "{text}");
    }
}
