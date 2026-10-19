//! 路径自检工具（非交互）：把"配置目录/输出目录"的真实选择过程与结果打到 stdout。
//!
//! 为什么需要它：`CapLite.exe` 是 GUI 程序，无头环境里既"看不到界面"、也只能证明"没有 panic"。
//! 这个工具直接调用**同一套**生产函数（[`ConfigStore`] 与 [`plan_output_dir`]），
//! 把每一步探测的目录、结果和回退结论如实打印出来，用于验收：
//!
//! ```text
//! cargo run --bin selfcheck_paths -- --config-dir "%TEMP%\cap-selfcheck"
//! ```
//!
//! 用法与主程序一致：`selfcheck_paths [--config-dir <目录>]`。

use caplite::config::{
    default_output_dir, plan_output_dir, probe_dir_writable, resolve_against_exe_dir, Config,
    ConfigStore,
};
use std::path::PathBuf;

fn config_dir_from_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--config-dir=") {
            return resolve_against_exe_dir(rest).ok();
        }
        if arg == "--config-dir" {
            let value = args.next()?;
            return resolve_against_exe_dir(&value).ok();
        }
    }
    None
}

fn main() {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<拿不到当前目录: {e}>"));
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<拿不到 exe 路径: {e}>"));
    println!("当前工作目录 = {cwd}");
    println!("exe 路径     = {exe}");
    println!();

    // ---- 1) 配置目录：与 CapLite.exe 启动时走的是同一个函数 ----
    let explicit = config_dir_from_args();
    println!(
        "显式指定的配置目录 = {}",
        match &explicit {
            Some(p) => p.display().to_string(),
            None => "（未指定）".to_string(),
        }
    );
    let store = ConfigStore::detect(explicit);
    println!("实际配置目录       = {}", store.path().display());
    println!("配置目录可写       = {}", store.writable());
    if let Some(n) = &store.notice {
        println!("配置目录提示       = {n}");
    }
    println!();

    // ---- 2) 配置读 / 写（真的落盘，再读回来核对） ----
    let mut configured = default_output_dir();
    match store.load() {
        Ok(cfg) => {
            configured = cfg.output_dir.clone();
            println!("读取配置成功：output_dir = {}", cfg.output_dir);
            match store.save(&cfg) {
                Ok(()) => println!("保存配置成功（写回 {}）", store.path().display()),
                Err(e) => println!("保存配置失败：{e}"),
            }
            match Config::load_from(store.path()) {
                Ok(back) => println!("回读校验：output_dir = {}", back.output_dir),
                Err(e) => println!("回读失败：{e}"),
            }
        }
        Err(e) => println!("读取配置失败（界面会提示并回落默认值）：{e}"),
    }
    println!();

    // ---- 3) 输出目录：配置值 → 回退链，逐个真写探测 ----
    println!("配置里的输出目录   = {configured}");
    let resolved = resolve_against_exe_dir(&configured);
    println!(
        "解析成绝对路径     = {}",
        match &resolved {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("<失败: {e}>（相对路径以 exe 目录为基准）"),
        }
    );
    let plan = plan_output_dir(&configured, &probe_dir_writable);
    // 重建与 plan_output_dir 完全一样的候选表，只为把每一步的探测结论打印出来
    let mut shown: Vec<PathBuf> = Vec::new();
    if let Ok(p) = &resolved {
        shown.push(p.clone());
    }
    for dir in caplite::config::default_fallback_dirs() {
        if !shown.contains(&dir) {
            shown.push(dir);
        }
    }
    println!("回退链探测结果：");
    for (i, dir) in shown.iter().enumerate() {
        let verdict = match probe_dir_writable(dir) {
            Ok(()) => "可写".to_string(),
            Err(e) => format!("不可写（{e}）"),
        };
        println!("  {}. {} → {verdict}", i + 1, dir.display());
    }
    println!(
        "最终选用的输出目录 = {}",
        match plan.chosen_path() {
            Some(p) => p.display().to_string(),
            None => "（没有任何可写的目录）".to_string(),
        }
    );
    if let Some(from) = &plan.fell_back_from {
        println!("回退来源           = {}（不可写）", from.display());
    }
    if let Some(err) = &plan.error {
        println!("输出目录错误       = {err}");
    }
    println!();

    // ---- 4) 结论 ----
    let cfg_ok = store.writable();
    let abs_ok = plan.chosen_path().map(|p| p.is_absolute()).unwrap_or(false);
    println!(
        "SELFCHECK config_dir={} output_dir={} output_is_absolute={}",
        if cfg_ok { "OK" } else { "FALLBACK-IN-MEMORY" },
        if abs_ok { "OK" } else { "NONE" },
        abs_ok
    );
    // 退出码：只要最终拿到一个**绝对路径**的输出目录就算通过
    // （配置目录可以退化成"内存模式"，但界面必须仍然可用）
    if abs_ok {
        std::process::exit(0);
    }
    std::process::exit(1);
}
