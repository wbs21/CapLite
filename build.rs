//! 构建脚本。
//!
//! 负责五件事：
//! 1. 链接本地 **libyuv** 静态库（`native/yuv.lib`，含 `ARGBToI420`）
//! 2. 编译 **CRT 链接桩**（`native/crt_stubs.c`）——`yuv.lib` 的 `cpu_id.obj` 遗留
//!    `__imp_fopen`/`__imp_fgets` 两个未解析导入符号，不补桩则链接失败
//! 3. 把 **FFmpeg 的 5 个运行期 DLL 改为延迟加载**（启动优化第 1 项，见下）
//! 4. 把**运行期 DLL** 复制到 `target\<profile>\runtime\`（发布版布局）；
//!    测试可执行文件在 `deps\` 下，因此同一组 DLL 也复制一份到 `deps\`（不进发布包）：
//!    * `openh264.dll`（同时以 `openh264-7.dll` 落地 —— FFmpeg 的 libopenh264 封装按这个
//!      名字动态加载，而我们的原生模块用 `openh264.dll`）
//!    * FFmpeg 的 LGPL 库（`avcodec-*` / `avformat-*` / `avutil-*` / `swresample-*` /
//!      `swscale-*`），取自环境变量 `FFMPEG_DIR/bin`
//! 5. 把 **`assets/caplite.ico` 与版本信息（`VERSIONINFO`）嵌进 exe**（资源节，
//!    见 [§5](#5-图标--版本资源用-msvc-的-rcexe-嵌进-exe)）
//!
//! FFmpeg 的头文件与导入库由 `ffmpeg-sys-next` 通过 `FFMPEG_DIR` 自行处理（bindgen + link），
//! 本脚本只负责"把隐式依赖改成延迟加载"。
//!
//! # 3) FFmpeg DLL 延迟加载（为什么、怎么做、怎么验证）
//!
//! **为什么**：`avcodec-63.dll` 在本机是 **87 MB**。隐式链接意味着**进程一启动**就要把它
//! 映射进来（还要被 Defender 扫一遍），而"只录屏/只看界面"这条路径根本不碰它。
//! 延迟加载（`/DELAYLOAD`）把映射推迟到**第一次真正调用**其中的函数时。
//!
//! **怎么做**：
//! * DLL 名字**从 `$FFMPEG_DIR/bin` 的实际文件名推导**（`avcodec-63.dll` 这种带版本号的名字
//!   写死会随 FFmpeg 升级失效，而"名字对不上"时延迟加载会**静默失效** —— 照样隐式加载）；
//! * 名字的基线来自 `Cargo.toml` 里 `ffmpeg-next` 打开的那几个 feature
//!   （codec/format/software-resampling/software-scaling），这些正是**可能被链接进来**的导入库；
//! * `/DELAYLOAD:<dll>` 只对"真的出现在导入表里的 DLL"有效（没被链接的会被链接器忽略），
//!   所以多写几个名字是安全的；
//! * 还需要链接 `delayimp.lib`（提供 `__delayLoadHelper2`）。它**不在 Windows SDK 里**
//!   （网上常见的说法是 `um/x64`，本机实测不存在），而在 **MSVC 工具集**的
//!   `VC\Tools\MSVC\<ver>\lib\x64` —— 恰好是 `LIB` 环境变量里的一项，
//!   所以这里先按 `LIB`/`VCToolsInstallDir` 找一遍，找到就显式 `link-search`，
//!   找不到就退回 `/DEFAULTLIB:delayimp` 让链接器自己在 `LIB` 里找。
//!
//! **怎么验证**（别只看启动耗时）：
//! * 静态：`dumpbin /imports CapLite.exe` 里这些 DLL 应从 "Image has the following imports"
//!   移到 "Image has the following delay load imports"；
//! * 运行期：`(Get-Process -Id $p.Id).Modules` 里**启动后不应出现** `av*`/`sw*`，
//!   而触发一次录制/剪辑（真正用到 FFmpeg）之后**必须出现**。
//!   两步都写在 `scripts/opts-verify.ps1` 里，可复现。
//!
//! 设 `CAP_NO_DELAYLOAD=1` 可**关掉**延迟加载（重建后即"改前"的隐式链接版本），
//! 用于在**同一份代码**上做 A/B 对比 —— 这是"改动前基线"唯一干净的取法。
//!
//! # 3.1) DLL 名**只有一份**（构建期与运行期共用）
//!
//! 名字被两个消费者使用，**必须**一致，否则"延迟加载/预加载"会静默失效：
//! * 构建期：`/DELAYLOAD:<name>`；
//! * 运行期：`src/ui/startup.rs::preload_ffmpeg_dlls_once`（枚举完成后主动 `LoadLibraryW`）。
//!
//! 做法：`main()` 里**只调一次** [`ffmpeg_dll_names`]，把结果同时
//! ① 写成 `OUT_DIR/ffmpeg_dll_names.rs`（运行期 `include!` 进去，见 [`emit_shared_dll_names`]）
//! ② 逐个发 `/DELAYLOAD`。所以"改版本号"永远只改 `$FFMPEG_DIR/bin` 里的文件名这一处，
//! 不存在"两处各写一份、升级时漏改一处"的可能。`src/ui/startup.rs` 的测试会读本文件的
//! `FFMPEG_LIB_STEMS` 与 `$FFMPEG_DIR/bin` 反查一遍，防止这层关系被改坏。
//!
//! # 5) 图标 + 版本资源（用 MSVC 的 `rc.exe` 嵌进 exe）
//!
//! **为什么不用 `winres`/`embed-resource` 这类 crate**：本仓库的构建路径已经全部依赖
//! MSVC 工具链（`link.exe`/`lib.exe`/Windows SDK 都在 `env.ps1` 里导入），而 `rc.exe`
//! 就在 Windows SDK 的 `bin\<ver>\x64` 下 —— 为一件"编一个 `.res` 再丢给链接器"的事
//! 新增一个 crate，收益为零、还多一份供应链。
//!
//! **做法**（三步，全部落在 `OUT_DIR`，不污染仓库）：
//! 1. 写一个 `.rc`（见 [`rc_body`]），内容两块：
//!    * `1 ICON "<ico 的绝对路径>"` —— 应用图标。**序号用 `1`**，不用 `IDI_ICON1`
//!      （rc 不预处理宏，会把标识符原样存成字符串名，取图标的地方按序号查就查不到）；
//!      路径用**正斜杠的绝对路径**并加引号，避开 rc 的工作目录与 `\` 转义两个坑；
//!    * `VS_VERSION_INFO VERSIONINFO` —— "文件属性"那一页：`FileDescription` /
//!      `ProductName` / `FileVersion` / `ProductVersion` / `OriginalFilename` /
//!      `LegalCopyright`（`Get-Item CapLite.exe | % VersionInfo` 读的就是它）。
//!      常量**内联**在 `.rc` 里，不 `#include <winver.h>`（那样会依赖 rc 的 INCLUDE 路径）；
//! 2. `rc /nologo /fo <out>\caplite.res <out>\caplite.rc`；
//! 3. `cargo:rustc-link-arg=<out>\caplite.res` —— 链接器把资源节并进 exe。
//!
//! **失败的降级口径**（重要）：找不到 `rc.exe`、或 `rc.exe` 非零退出，都**只 warn**，
//! 不 `panic`、不让构建失败 —— 图标与版本属性是"好看"，不是"能跑"。此时 exe 没有图标，
//! 但 `assets/caplite.ico` 与 `assets/caplite-256.png` 已经生成好（`scripts/make-icon.py`），
//! 可以手工用别的工具嵌。
//!
//! **清单（manifest）不会丢**：`#[cfg]`/Cargo 默认给 Windows 目标塞的是"默认清单"，
//! 我们自己的 `app.manifest`（DPI aware）原本并没有被引用。既然这一轮**真的**会调
//! `link.exe`，就顺手用 `/MANIFEST:EMBED /MANIFESTINPUT:<app.manifest>` 把它一起嵌进去
//! （`/MANIFEST:NO` 会**关掉**清单生成，那样反而丢 DPI 声明 —— 所以不用它）。
//! 这两条 `/MANIFEST*` **只在 `.res` 真的参与链接时才发**：嵌入失败时保持原状，
//! 不引入"没图标还赔上一个清单"的回归。

use std::path::{Path, PathBuf};

/// 可能被链接进来的 FFmpeg 库（去掉版本号的前缀）。
///
/// 与 `Cargo.toml` 的 `ffmpeg-next` feature 一一对应：
/// `codec`→avcodec、`format`→avformat、`software-resampling`→swresample、
/// `software-scaling`→swscale；avutil 永远在。**没被链接的那些会被链接器忽略**。
const FFMPEG_LIB_STEMS: &[&str] = &["avcodec", "avformat", "avutil", "swresample", "swscale"];

/// 生成给**运行期**用的 DLL 名清单文件名（落在 `OUT_DIR` 下，见 [`emit_shared_dll_names`]）。
///
/// 名字本身也是"单一来源"的一部分：`src/ui/startup.rs` 用 `include!` 引入同一个文件名，
/// 两侧都写这一个常量（改名会同时编译失败，不会静默对不上）。
const DLL_NAMES_MODULE: &str = "ffmpeg_dll_names.rs";

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let native = manifest.join("native");

    // ---- 1) libyuv ----
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rustc-link-lib=static=yuv");
    println!("cargo:rerun-if-changed={}", native.display());

    // ---- 2) CRT 桩（为 yuv.lib(cpu_id.obj) 的 __imp_fopen/__imp_fgets）----
    let stub = native.join("crt_stubs.c");
    if stub.exists() {
        cc::Build::new()
            .file(&stub)
            .warnings(false)
            .compile("cap_crt_stubs");
        println!("cargo:rerun-if-changed={}", stub.display());
    } else {
        println!(
            "cargo:warning=缺少 {}：一旦调用 libyuv 的 ARGBToI420，链接会因 \
             __imp_fopen/__imp_fgets 未解析而失败",
            stub.display()
        );
    }

    // ---- 3) FFmpeg DLL 延迟加载（含 MSVC 版导入库，见 emit_delayload）----
    //
    // DLL 名**只推导一次**，两个消费者看的是同一份 `Vec`：
    //   * 构建期 `/DELAYLOAD:<name>`（把隐式导入改成延迟导入）；
    //   * 运行期"枚举完成后主动预加载"（生成文件被 `src/ui/startup.rs` 用 `include!` 引入，见 §3.1）。
    let dll_names = ffmpeg_dll_names();
    emit_shared_dll_names(&dll_names);
    emit_delayload(&dll_names);

    // ---- 5) 图标资源（assets/caplite.ico → .res → 链接进 exe）----
    //
    // 放在这里（而不是 main 末尾）：它和 `profile_dir()` 无关，也不该被
    // "推导不出输出目录" 提前 return 掉。
    embed_icon(&manifest);

    // ---- 4) 运行期 DLL ----
    let Some(profile_dir) = profile_dir() else {
        println!("cargo:warning=无法推导输出目录，跳过 DLL 复制");
        return;
    };

    // 发布版要求：exe 同级只放 CapLite.exe / README.md，DLL 统一进 runtime\。
    // 测试可执行文件在 target\<profile>\deps\ 下，没有 main 的 AddDllDirectory，
    // 因此也把同一组 DLL 复制到 deps\（仅构建产物，不进入发布包）。
    let runtime = profile_dir.join("runtime");
    let deps = profile_dir.join("deps");
    if let Err(e) = std::fs::create_dir_all(&runtime) {
        println!("cargo:warning=创建 {} 失败: {e}", runtime.display());
    }
    let _ = std::fs::create_dir_all(&deps);
    let targets = [&runtime, &deps];

    // 4a) openh264：两个文件名都放
    let oh264 = native.join("openh264.dll");
    if oh264.exists() {
        for dir in targets {
            copy(&oh264, &dir.join("openh264.dll"));
            copy(&oh264, &dir.join("openh264-7.dll"));
        }
    } else {
        println!("cargo:warning=缺少 {}", oh264.display());
    }

    // 4b) FFmpeg 运行期库
    match std::env::var("FFMPEG_DIR") {
        Ok(dir) => {
            let bin = PathBuf::from(&dir).join("bin");
            let mut copied = 0usize;
            if let Ok(entries) = std::fs::read_dir(&bin) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) != Some("dll") {
                        continue;
                    }
                    let stem = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    // 名字形如 avcodec-63 / avformat-63 / avutil-61 / swresample-7
                    if FFMPEG_LIB_STEMS
                        .iter()
                        .any(|w| stem.starts_with(w))
                    {
                        if let Some(name) = p.file_name() {
                            for dir in targets {
                                copy(&p, &dir.join(name));
                            }
                            copied += 1;
                        }
                    }
                }
            }
            if copied == 0 {
                println!(
                    "cargo:warning=FFMPEG_DIR={dir} 下没找到运行期 DLL（avcodec/avformat/avutil/swresample），\
                     程序启动会因缺 DLL 失败"
                );
            }
        }
        Err(_) => println!(
            "cargo:warning=未设置 FFMPEG_DIR：ffmpeg-sys-next 无法 bindgen，且运行期 DLL 不会被复制"
        ),
    }
}

/// 为 Windows MSVC 目标 emit `/DELAYLOAD` 链接参数（见文件头注释的 3）。
///
/// 顺序很关键：**先**把 MSVC 版导入库的目录塞进 link-search（`FFMPEG_DIR/lib` 之前），
/// **再**发 `/DELAYLOAD`。用 GNU 版导入库时 `/DELAYLOAD` 会被链接器判成 LNK4199 静默失效。
///
/// `names` 由 `main()` 传入（= [`ffmpeg_dll_names`] 的结果，与运行期预加载清单**同一份**）。
///
/// 非 Windows / 非 MSVC 目标（本机不会用到，但让脚本保持可移植）直接跳过。
fn emit_delayload(names: &[String]) {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }
    if std::env::var("CAP_NO_DELAYLOAD").is_ok_and(|v| v == "1") {
        // 这是**构建期**的一条提示（不是告警）：A/B 对比时用它重建"改前"的隐式链接版本
        //
        // 必须声明 rerun-if-env-changed：否则 cargo 会复用上一次（开了延迟加载的）构建脚本输出，
        // 这个开关看起来"没生效" —— 实测踩过一次：设了 CAP_NO_DELAYLOAD=1 重新 build，
        // 新的 CapLite.exe 仍然带 Delay Import Directory（=160 字节）。
        println!("cargo:rerun-if-env-changed=CAP_NO_DELAYLOAD");
        println!(
            "cargo:warning=CAP_NO_DELAYLOAD=1：已关闭 FFmpeg DLL 延迟加载（A/B 对比基线）"
        );
        return;
    }
    println!("cargo:rerun-if-env-changed=CAP_NO_DELAYLOAD");

    if names.is_empty() {
        println!(
            "cargo:warning=没能推导出 FFmpeg 的 DLL 名（FFMPEG_DIR 未设置或 bin 目录读不到），\
             本次不启用延迟加载：启动仍会隐式映射 avcodec-*.dll"
        );
        return;
    }

    // 3a) MSVC 版导入库（GNU 版会让 /DELAYLOAD 静默失效，见 `make_msvc_import_libs`）
    //
    // 注意：**5 个 DLL 全部发 `/DELAYLOAD`**，不做"哪些真的被链接"的预判 ——
    // 那需要在链接前知道导入表，做不到。对本机没被链接的 `swresample-7.dll`
    // 链接器会发 LNK4199；它**无害**（swresample 本来就没映射），但会污染构建输出，
    // 所以下面第 3b 步同时挂上 `/IGNORE:4199`（"零告警"是本任务的硬基线）。
    match make_msvc_import_libs() {
        Ok(dir) => println!("cargo:rustc-link-search=native={}", dir.display()),
        Err(reason) => println!(
            "cargo:warning=未能生成 MSVC 版导入库（{reason}）；\
             链接器很可能忽略 /DELAYLOAD（LNK4199），延迟加载会静默失效"
        ),
    }

    // 3b) 逐个 DLL 发 /DELAYLOAD
    for name in names {
        println!("cargo:rustc-link-arg=/DELAYLOAD:{name}");
    }
    // 防御性忽略 LNK4199（"该 DLL 没有导入，已忽略 /DELAYLOAD"）：
    // 只在"某个 DLL 确实没被链接进来"时出现（本机是 swresample），语义上无需处理。
    println!("cargo:rustc-link-arg=/IGNORE:4199");
    // 延迟加载的运行时辅助函数（`__delayLoadHelper2`），必须显式链进来。
    match find_delayimp_lib() {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=delayimp");
        }
        None => {
            // `LIB` 环境变量里通常已经有它（MSVC 工具集的 lib\x64），让链接器自己找
            println!("cargo:rustc-link-arg=/DEFAULTLIB:delayimp");
        }
    }
    println!(
        "cargo:warning=已启用 FFmpeg DLL 延迟加载（{}）：首次真正调用时才映射这些 DLL",
        names.join(", ")
    );
}

/// 把推导出的 DLL 名写成 `OUT_DIR/ffmpeg_dll_names.rs`：**运行期预加载的唯一来源**。
///
/// 为什么"生成一个 Rust 文件"而不是在 `src` 里再写一份常量：这些名字带版本号
/// （`avcodec-63.dll`），**只能**从 `$FFMPEG_DIR/bin` 的真实文件名推导；再抄一份就等于
/// 给未来留一个"升级 FFmpeg 后漏改一处"的坑 —— 而名字对不上时延迟加载/预加载都是
/// **静默失效**（不报错、只是照样在别处映射）。生成物被 `src/ui/startup.rs` 用
/// `include!` 引入，与 `/DELAYLOAD` 用的是**同一个** `Vec<String>`。
///
/// 清单为空（`FFMPEG_DIR` 没设/bin 读不到）时也要写出一个**空数组**：那样 `include!`
/// 侧照旧编译通过，只是预加载退化成"什么都不做"，不会把构建搞坏。
fn emit_shared_dll_names(names: &[String]) {
    let Ok(out) = std::env::var("OUT_DIR") else {
        println!("cargo:warning=无 OUT_DIR：运行期清单无法生成（预加载会退化成空清单）");
        return;
    };
    let path = Path::new(&out).join(DLL_NAMES_MODULE);
    let mut src = String::new();
    // 用普通注释（`//`）而不是内部文档注释（`//!`）：本文件是被 `include!` 进一个
    // **模块内部**的（`pub mod ffmpeg_dlls { include!(..) }`），`//!` 在那个位置不合法。
    src.push_str("// 本文件由 build.rs 生成，请勿手改。\n");
    src.push_str("//\n");
    src.push_str("// 唯一来源：`$FFMPEG_DIR/bin` 里的实际文件名（带版本号，例如 avcodec-63.dll）。\n");
    src.push_str("// 同一份清单在构建期还逐个喂给了 `/DELAYLOAD`（见 build.rs::emit_delayload），\n");
    src.push_str("// 所以「哪些 DLL 被延迟加载」与「运行期预加载哪些」永远一致。\n\n");
    src.push_str("/// 运行期 FFmpeg DLL 名（构建期从 `$FFMPEG_DIR/bin` 推导）。\n");
    src.push_str("pub const FFMPEG_RUNTIME_DLLS: &[&str] = &[\n");
    for name in names {
        src.push_str("    ");
        src.push_str(&format!("{name:?}"));
        src.push_str(",\n");
    }
    src.push_str("];\n");
    if let Err(e) = std::fs::write(&path, src) {
        println!(
            "cargo:warning=写 {} 失败（{e}）：运行期不会预加载 FFmpeg DLL（不影响构建与录制）",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// 图标资源（见文件头 §5）
// ---------------------------------------------------------------------------

/// 图标文件相对仓库根的位置（`scripts/make-icon.py` 的默认输出）。
const ICON_REL: &str = "assets/caplite.ico";
/// 自己的清单文件（DPI aware）；存在就一起嵌，不存在就跳过。
const MANIFEST_REL: &str = "app.manifest";
/// 图标的**资源序号**：`1`。
///
/// 为什么是数字而不是 `IDI_ICON1` 这个名字：实测 `rc.exe` 会把标识符**原样**存成字符串名
/// （`EnumResourceNames(RT_GROUP_ICON)` 看到的是 `NAME:IDI_ICON1`），而 `IDI_ICON1` 只是
/// `windows.h` 里的一个宏 —— rc 不做预处理就认不出它。名字形式的组图标多数取图标的地方
/// （Explorer / `LoadIcon(NULL, IDI_APPLICATION)` 之类的数字查找）**取不到**；
/// 用序号 1 则处处可查（"exe 的第一个图标"就是这个约定）。
/// 这条由 `scripts/extract-icon.ps1` 按序号 1 回读验证。
const ICON_RES_ID: &str = "1";

// ---------------------------------------------------------------------------
// VERSIONINFO（"文件属性"里那一页）
// ---------------------------------------------------------------------------

/// **产品版本**（展示串）。与 `Cargo.toml` 的 `version` 保持一致；
/// 四段数字形式由 [`version_quad`] 从它推导（`1.0.2` → `1,0,2,0`）。
const PRODUCT_VERSION_STR: &str = "1.0.3";
/// 产品名：与 `theme::APP_NAME` 同值（窗口标题与文件属性不该各写一个）。
const PRODUCT_NAME: &str = "CapLite";
/// `FileDescription`：资源管理器里"说明"那一栏。
const FILE_DESCRIPTION: &str = "CapLite screen recorder";
/// `OriginalFilename`：链接器不会自动填，必须自己给（很多工具靠它判断"这原本叫什么"）。
const ORIGINAL_FILENAME: &str = "CapLite.exe";
/// `LegalCopyright`。
const LEGAL_COPYRIGHT: &str = "Copyright (c) 2026 CapLite";
/// `CompanyName`；留空则不写这一项。
const COMPANY_NAME: &str = "CapLite";

/// 把 `"1.0.0"` 这样的展示串转成 `VERSIONINFO` 要的四段数字，缺的段补 0。
///
/// 非法段（空 / 非数字 / > 65535）会让构建**直接失败**而不是悄悄填 0 —— 版本号填错是
/// "文件属性里显示 0.0.0.0"这种没人会立刻发现的错误，宁可构建时就炸。
fn version_quad(v: &str) -> [u16; 4] {
    let mut out = [0u16; 4];
    for (i, part) in v.split('.').take(4).enumerate() {
        // 预发布后缀（`1.0.0-beta`）在这里没意义：FILEVERSION 只接受数字，直接取前缀数字段
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        out[i] = digits.parse::<u16>().unwrap_or_else(|_| {
            panic!("PRODUCT_VERSION_STR={v:?} 的第 {} 段 {part:?} 不是合法数字", i + 1)
        });
    }
    out
}

/// 生成 `.rc` 的**正文**：图标 + `VERSIONINFO`。
///
/// `VERSIONINFO` 的两半都不能少：
/// * `FILEVERSION` / `PRODUCTVERSION`（固定块）= "数字"，资源管理器的"文件版本 / 产品版本"
///   和很多安装器读的是它们；
/// * `StringFileInfo` 里的字符串表 = 属性页上看到的"文件说明 / 产品名称 / 版权"，
///   以及 `Get-Item … | % VersionInfo` 读到的 `FileDescription` / `ProductName` /
///   `FileVersion` / `ProductVersion` / `OriginalFilename` / `LegalCopyright`。
fn rc_body(ico_abs_slash: &str) -> String {
    let [a, b, c, d] = version_quad(PRODUCT_VERSION_STR);
    let mut s = String::new();
    s.push_str("// 由 build.rs 生成，请勿手改。\n");
    s.push_str(&format!("{ICON_RES_ID} ICON \"{ico_abs_slash}\"\n\n"));
    // **不 `#include <winver.h>`**：那要 rc.exe 能找到 SDK 的 include 目录（`INCLUDE` 环境变量
    // 由 vcvars 设置，但不保证每个构建环境都有）。这里把用到的那几个常量按 winver.h 的定义
    // 内联进来，`.rc` 就完全自足、不依赖任何头文件。
    s.push_str("// ---- winver.h 里的常量（内联，避免依赖 rc 的 include 路径）----\n");
    s.push_str("#define VS_FFI_FILEFLAGSMASK 0x3fL\n");
    s.push_str("#define VS_FF_DEBUG         0x1L\n");
    s.push_str("#define VS_FF_PRERELEASE    0x2L\n");
    s.push_str("#define VOS_NT_WINDOWS32    0x00040004L\n");
    s.push_str("#define VFT_APP             0x00000001L\n");
    s.push_str("#define VFT2_UNKNOWN        0x00000000L\n\n");
    // **资源名用 `1`，不用 `VS_VERSION_INFO`**：实测用字面量 `VS_VERSION_INFO` 作为资源名时，
    // `rc` 会把它当成**字符串名**存进资源目录，于是 `GetFileVersionInfoSizeW` 直接返回 0
    // （`ERROR_RESOURCE_TYPE_NOT_FOUND` 1813）—— 资源在文件里、字节完全正确，但 version.dll
    // 按"序号 1 的 VERSION 资源"这个惯例去找，找不到。（`notepad.exe` 用的就是序号 1。）
    // 所以这里写 `1 VERSIONINFO`，与 Windows 自己的可执行文件保持同一形态。
    s.push_str("1 VERSIONINFO\n");
    s.push_str(&format!(" FILEVERSION {a},{b},{c},{d}\n"));
    s.push_str(&format!(" PRODUCTVERSION {a},{b},{c},{d}\n"));
    s.push_str(" FILEFLAGSMASK VS_FFI_FILEFLAGSMASK\n");
    s.push_str(" FILEFLAGS 0x0L\n");
    s.push_str(" FILEOS VOS_NT_WINDOWS32\n");
    s.push_str(" FILETYPE VFT_APP\n");
    s.push_str(" FILESUBTYPE VFT2_UNKNOWN\n");
    s.push_str("BEGIN\n");
    s.push_str("    BLOCK \"StringFileInfo\"\n");
    s.push_str("    BEGIN\n");
    // 语言/代码页：0409 = en-US，04e4 = 1252（Latin-1）。**不要用 04b0（Unicode/1200）**：
    // 实测 `[System.Diagnostics.FileVersionInfo]::GetVersionInfo()`（PowerShell 的
    // `(Get-Item x.exe).VersionInfo` 就是它）读 **04b0** 的块会全部返回空串 —— 同一个 exe
    // 用 04e4 就能读出 `FileVersion`/`ProductName`/`FileDescription`。我们的字符串全是 ASCII，
    // 用 1252 没有任何损失。
    s.push_str("        BLOCK \"040904e4\"\n");
    s.push_str("        BEGIN\n");
    s.push_str(&format!("            VALUE \"FileDescription\", \"{FILE_DESCRIPTION}\"\n"));
    s.push_str(&format!("            VALUE \"FileVersion\", \"{PRODUCT_VERSION_STR}\"\n"));
    s.push_str(&format!("            VALUE \"ProductName\", \"{PRODUCT_NAME}\"\n"));
    s.push_str(&format!("            VALUE \"ProductVersion\", \"{PRODUCT_VERSION_STR}\"\n"));
    s.push_str(&format!("            VALUE \"OriginalFilename\", \"{ORIGINAL_FILENAME}\"\n"));
    s.push_str(&format!("            VALUE \"LegalCopyright\", \"{LEGAL_COPYRIGHT}\"\n"));
    if !COMPANY_NAME.is_empty() {
        s.push_str(&format!("            VALUE \"CompanyName\", \"{COMPANY_NAME}\"\n"));
    }
    s.push_str("        END\n");
    s.push_str("    END\n");
    // 固定块里的 VarFileInfo（翻译表），与上面的 040904e4 对应
    s.push_str("    BLOCK \"VarFileInfo\"\n");
    s.push_str("    BEGIN\n");
    s.push_str("        VALUE \"Translation\", 0x409, 1252\n");
    s.push_str("    END\n");
    s.push_str("END\n");
    s
}

/// 把 `assets/caplite.ico` 编成 `.res` 并让链接器带进 exe。**任何失败都只 warn**。
fn embed_icon(manifest_dir: &Path) {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return; // 非 Windows 目标没有资源节这回事
    }
    let ico = manifest_dir.join(ICON_REL);
    // 无论成败都盯着它：改了图标必须重跑，否则 exe 里还是旧图标（这种"改了没生效"最难查）
    println!("cargo:rerun-if-changed={}", ico.display());
    if !ico.is_file() {
        println!(
            "cargo:warning=没有图标文件 {}（跑 `python scripts/make-icon.py` 生成）；\
             本次 exe 不带图标资源",
            ico.display()
        );
        return;
    }

    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        println!("cargo:warning=无 OUT_DIR：跳过图标嵌入");
        return;
    };
    let out_dir = PathBuf::from(out_dir);
    let rc_path = out_dir.join("caplite.rc");
    let res_path = out_dir.join("caplite.res");

    // rc 语法：`<名> ICON "<文件>"`。用**正斜杠**的绝对路径（同时避开 `\` 与 rc 的
    // 转义/续行规则），并用 `"` 包住（含空格的工作区路径）。
    let ico_for_rc = ico.display().to_string().replace('\\', "/");
    let rc_text = rc_body(&ico_for_rc);
    if let Err(e) = std::fs::write(&rc_path, rc_text) {
        println!(
            "cargo:warning=写 {} 失败（{e}）：跳过图标嵌入",
            rc_path.display()
        );
        return;
    }
    println!("cargo:rerun-if-changed={}", rc_path.display());

    let Some(rc_exe) = find_rc_exe() else {
        println!(
            "cargo:warning=找不到 MSVC 的 rc.exe（Windows SDK 的 bin\\<版本>\\<架构>\\rc.exe）；\
             跳过图标嵌入 —— assets/caplite.ico 已生成，可用其它工具手工嵌"
        );
        return;
    };

    if let Some(parent) = res_path.parent() {
        if !parent.is_dir() && std::fs::create_dir_all(parent).is_err() {
            println!(
                "cargo:warning=建不出目录 {}：跳过图标嵌入",
                parent.display()
            );
            return;
        }
    }

    let output = std::process::Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .output();
    match output {
        Ok(o) if o.status.success() && res_path.is_file() => {
            // `.res` 参与链接 → 链接器自己也会想要"生成默认清单"，于是同时明确要求
            // 嵌我们自己的 app.manifest（否则会丢 DPI 声明）。见文件头 §5 的降级口径。
            let manifest = manifest_dir.join(MANIFEST_REL);
            if manifest.is_file() {
                println!("cargo:rerun-if-changed={}", manifest.display());
                println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
                println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
            }
            println!("cargo:rustc-link-arg={}", res_path.display());
            // 读回 `.res` 大小当"真的编出来了"的旁证（写进构建日志，便于事后核对）
            let size = std::fs::metadata(&res_path).map(|m| m.len()).unwrap_or(0);
            println!(
                "cargo:warning=已嵌入图标资源（{} → {}，{size} 字节，rc.exe={}）",
                ico.display(),
                res_path.display(),
                rc_exe.display()
            );
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            println!(
                "cargo:warning=rc.exe 编译图标失败（退出码 {}）：{}；跳过图标嵌入",
                o.status,
                err.trim().replace('\n', " ")
            );
        }
        Err(e) => println!("cargo:warning=启动 {} 失败（{e}）：跳过图标嵌入", rc_exe.display()),
    }
}

/// 找 MSVC 的 `rc.exe`。
///
/// 顺序（都能失败 → 返回 `None`，调用方降级）：
/// 1. `PATH`：`env.ps1` 导入 vcvars 后**不一定**含 SDK 的 bin（实测本机 PATH 里没有），
///    但别的机器/别的导入方式可能有，先认；
/// 2. `WindowsSDKVersion`（vcvars 一定会设，形如 `10.0.28000.0\`）+
///    `WindowsSdkDir` 或默认的 `C:\Program Files (x86)\Windows Kits\10\`；
///    版本目录对不上时退一步取 `bin\` 下字典序最大的版本目录。
///
/// **不用短路径**：实测本机的 `rc.exe` 能正确处理含空格的绝对路径
/// （工作区在 `C:\Users\Limit li\...`），所以 `.rc` 里用正斜杠绝对路径就够了。
///
/// 目标架构只认 `x64`（本项目的唯一目标）。
fn find_rc_exe() -> Option<PathBuf> {
    // 1) PATH
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';').filter(|d| !d.trim().is_empty()) {
            let cand = Path::new(dir).join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }

    // 2) Windows SDK 目录 + 版本
    let version = std::env::var("WindowsSDKVersion")
        .ok()
        .map(|v| v.trim_end_matches('\\').to_string())
        .filter(|v| !v.is_empty());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("WindowsSdkDir") {
        if !dir.trim().is_empty() {
            roots.push(PathBuf::from(dir));
        }
    }
    roots.push(PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\"));
    roots.push(PathBuf::from(r"C:\Program Files\Windows Kits\10\"));

    for root in roots {
        let bin = root.join("bin");
        if let Some(v) = &version {
            let cand = bin.join(v).join("x64").join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
        // 版本没拿到（或对不上）时：取 bin 下**字典序最大的版本目录**，够用且不用递归
        if let Ok(entries) = std::fs::read_dir(&bin) {
            let mut versions: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            versions.sort();
            for v in versions.into_iter().rev() {
                let cand = v.join("x64").join("rc.exe");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// 从 `$FFMPEG_DIR/bin` 的实际文件名推导 DLL 名（`avcodec-63.dll` 这种）。
///
/// 为什么必须推导：延迟加载按**文件名字符串**匹配导入表，名字错一个字符就静默失效。
/// 读不到 `FFMPEG_DIR` 时返回空 —— 宁可不启用，也不要写出一个"看起来开了、其实没开"的配置。
fn ffmpeg_dll_names() -> Vec<String> {
    let Ok(dir) = std::env::var("FFMPEG_DIR") else {
        return Vec::new();
    };
    let bin = PathBuf::from(dir).join("bin");
    let Ok(entries) = std::fs::read_dir(&bin) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let is_dll = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("dll"))
            .unwrap_or(false);
        if !is_dll {
            continue;
        }
        let Some(file_name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if FFMPEG_LIB_STEMS.iter().any(|w| stem.starts_with(w)) {
            names.push(file_name.to_ascii_lowercase());
        }
    }
    names.sort();
    names.dedup();
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-changed={}", bin.display());
    names
}

/// 找 `delayimp.lib`：MSVC 工具集的 `lib\x64`（`LIB` 环境变量里的一项）。///
/// 为什么不用 Windows SDK 的路径：本机 Windows SDK 10.0.28000.0 的 `um/x64`、`ucrt/x64`
/// 里**都没有** `delayimp.lib`（试过），它随 MSVC 工具集发布。
fn find_delayimp_lib() -> Option<PathBuf> {
    // 1) 直接问 `LIB`：链接器最终也是在这些目录里找
    if let Ok(lib) = std::env::var("LIB") {
        for dir in lib.split(';').filter(|d| !d.trim().is_empty()) {
            let candidate = Path::new(dir).join("delayimp.lib");
            if candidate.is_file() {
                return candidate.parent().map(Path::to_path_buf);
            }
        }
    }
    // 2) 退一步：按 VCToolsInstallDir 拼（`LIB` 缺失时也能工作）
    if let Ok(vc) = std::env::var("VCToolsInstallDir") {
        for sub in ["lib\\x64", "lib\\x86", "lib\\arm64"] {
            let candidate = Path::new(&vc).join(sub).join("delayimp.lib");
            if candidate.is_file() {
                return candidate.parent().map(Path::to_path_buf);
            }
        }
    }
    None
}

/// 生成 **MSVC 格式**的 FFmpeg 导入库，返回放它们的目录。
///
/// # 为什么非要这一步（本机实测的坑）
/// FFmpeg 分发包里带的 `*.lib` 是 **GNU/MinGW 风格**的导入库。用它们链接出的 exe 能跑，
/// 但 `link.exe` **认不出"这些导入属于哪个 DLL"**：
/// ```
/// LINK : warning LNK4199: 已忽略 /DELAYLOAD:avcodec-63.dll；未找到来自 avcodec-63.dll 的导入
/// ```
/// 结果是 **Delay Import Directory 为空 → 延迟加载静默失效**（照样隐式映射 87 MB 的
/// `avcodec-63.dll`）。同一条 `/DELAYLOAD`，链接 MSVC 自己生成的导入库时就完全正常 ——
/// 最小复现见 `_rust/tmp/rustdl`（`probe.lib` 成功、GNU 版 `avcodec.lib` 失败）。
///
/// # 做法
/// 调 `scripts/make-msvc-import-libs.py`：`dumpbin /exports <dll>` → `.def`
/// → `lib.exe /def:` 生成 MSVC 导入库。放进 `<profile>/msvc-implib/`，
/// 再用 `rustc-link-search` 把它排在 `FFMPEG_DIR/lib` **前面**（先发者优先）。
///
/// 失败时返回原因字符串；调用方只 warn、不中断构建（宁可慢，也不能构建失败）。
fn make_msvc_import_libs() -> Result<PathBuf, String> {
    let ffmpeg = std::env::var("FFMPEG_DIR").map_err(|_| "未设置 FFMPEG_DIR".to_string())?;
    let manifest = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| "无 CARGO_MANIFEST_DIR")?;
    let script = Path::new(&manifest)
        .join("scripts")
        .join("make-msvc-import-libs.py");
    let out = profile_dir()
        .ok_or_else(|| "推导不出 target/<profile> 目录".to_string())?
        .join("msvc-implib");

    // 找一个能用的 Python：优先环境变量，其次本机 DSH 运行时自带的
    let python = std::env::var("CAP_PYTHON")
        .ok()
        .filter(|p| Path::new(p).is_file())
        .or_else(|| {
            let bundled = PathBuf::from(
                r"C:\Users\Limit li\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe",
            );
            bundled.is_file().then_some(bundled.display().to_string())
        })
        .ok_or_else(|| "找不到 Python（设 CAP_PYTHON 指定）".to_string())?;

    if !script.is_file() {
        return Err(format!("缺少脚本 {}", script.display()));
    }
    if !out.is_dir() && std::fs::create_dir_all(&out).is_err() {
        return Err(format!("建不出目录 {}", out.display()));
    }

    let status = std::process::Command::new(&python)
        .arg(&script)
        .arg("--ffmpeg-dir")
        .arg(&ffmpeg)
        .arg("--out")
        .arg(&out)
        .status()
        .map_err(|e| format!("启动 {python} 失败: {e}"))?;
    if !status.success() {
        return Err(format!("脚本退出码 {status}"));
    }
    // 至少要有一个导入库才算成功
    let ok = std::fs::read_dir(&out)
        .map(|d| {
            d.flatten()
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("lib"))
        })
        .unwrap_or(false);
    if !ok {
        return Err(format!("{} 里没有生成任何 .lib", out.display()));
    }
    println!("cargo:rerun-if-changed={}", script.display());
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    Ok(out)
}

/// `OUT_DIR = target/<profile>/build/<pkg>-<hash>/out` → 上溯 3 级得到 `target/<profile>`
fn profile_dir() -> Option<PathBuf> {
    let out = std::env::var("OUT_DIR").ok()?;
    PathBuf::from(out).ancestors().nth(3).map(Path::to_path_buf)
}

fn copy(from: &Path, to: &Path) {
    if let Err(e) = std::fs::copy(from, to) {
        println!("cargo:warning=复制 {} → {} 失败: {e}", from.display(), to.display());
    }
}
