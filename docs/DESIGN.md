# CapLite 方案选型与已有资产核查（设计定稿）

> 结论先行：**16 个环节的确定方案 + 已有资产可用性逐项判定 + 3 处需要你拍板的取舍**。
> 所有"实测"结论都附命令/数据；标"未验证"的会写清怎么验证。
>
> **索引（按主题）**：环境/约束 → §1；风险与对策 → §2；资产核查 → §3；许可与已拍板决定 → §4；
> 实施顺序 → §5；MP4 打包选型（三轮）→ §6–§8；FFmpeg 两条路线与最终选择 → §9–§10；
> 编码参数与实测基准 → §10.2–§10.7；**实现进度与踩坑 → §11.1–§11.8**。
>
> **近几轮的界面与生命周期设计（本文件末尾）**：
> **§11.9** 目录与文件生命周期（Cache：不删、不统计、关闭时提醒一次）｜
> **§11.10** 区域框选（三态交互 + 「确定」按钮 + 两个根因）｜
> **§11.11** 录制页（显示器与录制范围、启动强制全屏、状态卡片）｜
> **§11.12** 剪辑页（版式、三色标尺、预览口径、常驻操作指南）｜
> **§11.13** 设置页与外壳（只剩三项、调整即保存、底部信息栏）｜
> **§11.14** 开发签名与信任（摘要 + 指向 [`docs/DEV-SIGNING.md`](DEV-SIGNING.md)）｜
> **§11.15** **变更记录（近几轮）** ｜ **§11.16** 本轮验证记录。
>
> 编号说明：本文档的章节编号是**按撰写轮次追加**的，历史上有过 §6 排在 §5 之前、两个 §9 之类的顺序错位；为避免改动既有编号导致代码注释里 `§10.x` / `§11.x` 的引用失配，新增内容一律作为 **§11.9–§11.16** 追加在末尾，**不重排既有编号、也不出现重复号**。

---

## 0. 决定表（各环节最优方案）

| # | 环节 | 决定方案 | 关键依据（实测） | 备选/回退 |
|---|---|---|---|---|
| 1 | 视频采集 | **windows-capture 2.0.1 的 DXGI Desktop Duplication**（`DxgiDuplicationApi`） | WGC 显示器捕获在本机失败（`ItemConvertFailed`）；DXGI 实测 **61.6 fps、0 超时 0 错误、1920×1080、`row_pitch=7680`（无 padding）、`Bgra8`**，画面正确 | WGC + 交互式 `GraphicsCapturePicker`（含指针但需人工选屏）；GDI `BitBlt`（最慢，兜底） |
| 2 | 鼠标指针 | **自绘合成**：GDI `GetCursorInfo` + `DrawIconEx` 画到 32 位 DIB，再叠加到采集帧 | DXGI 不合成指针；crate 未暴露 `PointerPosition`/`GetFramePointerShape`；WGC 的 `WithCursor` 在本机不可用（见 §2.2） | 设置项"不显示指针"（一行代码，先可跑通） |
| 3 | 区域录制 | **在同一帧上裁剪**：优先 `Frame::buffer_crop()`/`DxgiDuplicationFrame::buffer_crop()`，否则用 `row_pitch` + 指针偏移零拷贝交给 libyuv | crate 两个采集后端都原生提供 `buffer_crop`；`row_pitch == width*4` 时偏移计算无坑 | 抓全屏后 CPU 裁剪（多一次拷贝） |
| 4 | 区域拾取（UI） | **GDI 冻结帧 + 不透明全屏遮罩**：进入框选先 `GetDC(NULL)`+`BitBlt`+`GetDIBits` 抓整张虚拟桌面，遮罩窗口（`with_monitor(rank)` 无边框置顶）1:1 铺上这张图，**只把选区之外压暗 50%**，选区保持原亮度 | 选区坐标由"显示器矩形 + 窗口内逻辑点 × 显示器物理/逻辑比例"直接算出，**物理像素**且全程 `i64`，绕开 DPI/多屏负坐标坑；实测 `capture_desktop()` 在本机抓到 `origin=(0,0) 3600x1080`（单测 `desktop_frame_captures_and_decodes`） | 透明分层窗口（`WS_EX_LAYERED`+`SetLayeredWindowAttributes`）直接透出真桌面——**已实测不可靠**：eframe immediate viewport 拿不到逐像素透明，用户看到整屏被遮挡，故弃用；只在冻结帧抓取失败时退化成"中性底色 + 压暗"并显示原因 |
| 5 | BGRA→I420 | **本地 libyuv 静态库**（`native/yuv.lib`，含 `ARGBToI420`/`ABGRToI420`，x64） | 已解析归档成员：44 成员 / 41 个 x64 COFF 对象 / 两个符号都在；采集格式恰好是 `Bgra8` | 纯 Rust 手写转换（慢 5–10 倍）；FFmpeg `swscale`（引入 118 MB DLL） |
| 6 | 视频编码 | **随包 openh264 2.6.0 DLL + 手写 `ISVCEncoder` vtable FFI + `LoadLibraryW`/`GetProcAddress`**（**已实现并交付**，见 §8.4） | ABI 已用 MSVC 实测（`SEncParamBase`=24、`SEncParamExt`=924、`SSpatialLayerConfig`=200、`SSourcePicture`=80、`SFrameBSInfo`=7192，vtable 10 槽）；**绝不链接 `openh264.lib`**（它是 `openh264-7.dll` 的导入库且缺 `WelsEncodeFrame`/`WelsSetEncoderParam`）；实测 32 项测试全绿、**ffmpeg 独立解码 30 帧干净通过且关键帧位置与 `SFrameBSInfo.eFrameType` 完全一致** | crates 上 `openh264` 0.9.8（源码编译，需 cc+nasm，构建慢）；把 DLL 复制成 `openh264-7.dll` 后走导入库（只能拿到 6 个 C 函数） |
| 7 | 音频采集 | **`wasapi` 0.26.0 crate**：`Direction::Render` 环回（系统声）+ `Direction::Capture`（麦克风） | crate 源码含 `examples/loopback.rs`；有 `initialize_mta`/`get_default_device`/`get_mixformat`/`initialize_client`/`set_get_eventhandle`/`get_audiocaptureclient` 全套；**COM 单元模型必须是"可接受的 HRESULT 集合"**（`S_OK`/`S_FALSE`/`RPC_E_CHANGED_MODE`），否则 eframe 的 STA 主线程上打不开音源 | `windows` crate 手写 WASAPI（COM+事件+格式转换全自己写，多 400+ 行） |
| 8 | 混音/重采样 | **自研**：统一到 48 kHz / 2ch / f32；单声道复制；采样率不同时线性重采样；求和后限幅 | 环回与麦克风格式可能不同，必须归一；不引入额外依赖 | 只支持同格式（遇不同格式报错） |
| 9 | 音频编码 | **Media Foundation AAC MFT**（AAC-LC 192 kbps） | `C:\Windows\System32\mfAACEnc.dll`（1.6 MB）确认存在；`mfplat`/`mfreadwrite` 均在；免依赖、免许可证争议、可直接取 `MF_MT_USER_DATA` 里的 AudioSpecificConfig 填 `esds` | `fdk-aac` 0.8.0（音质更好，但许可证非自由 + 需编 C）；FFmpeg `aac`（要带 118 MB DLL） |
| 10 | 录制中的流式文件（fMP4，崩溃可播） | **自研极简 fMP4 写入器**（视频+音频两轨；只写不解析：`ftyp` + `moov(mvhd/trak/mvex/trex)` + 每片 `moof(tfhd/tfdt/trun)+mdat`） | **唯一没有许可干净的现成实现的一环**：`muxide` 的 `FragmentedMuxer` 官方注明 "Only video configuration is supported for fragmented MP4"（`FragmentConfig` 无音频字段）；`mp4` crate 只能写渐进式；Bento4 无增量写且是 GPL | 仅画面模式可直接用 `muxide::FragmentedMuxer`（MIT/Apache）；或放弃崩溃可播（不做） |
| 11 | 停止后导出普通 MP4 + 剪辑 copy 导出 | **读 → `mp4box` 0.14.0（MIT）`track_samples_from_path()`**（给出 `SampleInfo{file_offset,size,is_sync,pts,dts}`）；**写 → `muxide` 0.2.5（MIT/Apache）`Muxer`**（`write_video(pts, Annex-B, is_keyframe)` + `write_audio(pts, ADTS-AAC)`） | 两者都是 MIT/Apache；`mp4box` 暴露**文件偏移**，copy 导出只读需要的字节、不重编码；`mp4box` 还能解码 `avcC`/`esds` 供重建容器；`muxide` 吃的正是我们的格式（openh264=Annex-B、MF AAC 裸帧补 7 字节 ADTS） | 全自研读写（子代理已有，作为交叉验证/回退） |
| 12 | 剪辑读取 + 关键帧索引 | **`mp4box` 0.14.0（MIT）**：`get_boxes` 盒树、`track_samples_from_path` 样本表（渐进+分片）、`get_boxes_tolerant` 容忍损坏文件、`edit::Editor` 非破坏性编辑（自动 size/chunk-offset 修正 + faststart） | MIT；依赖仅 `anyhow`+`serde`；**同时覆盖渐进式与 fMP4/CMAF**；`file_offset` 让 copy 导出不必把样本读进内存 | `mp4` 0.14 / `re_mp4` 0.5 / `mp4san` 0.5（都 MIT，均能解析 moof）做交叉验证；自研 reader 作第三方互验 |
| 13 | MP4 独立验证（多方互证） | ① **`mp4box` 自带 `mp4dump`/`mp4info`/`mp4samples`（MIT，可本地编出）** ② `mp4` crate 读回断言 ③ **Media Foundation Source Reader 解码**（容器+码流双验证） | `mp4box` 的 CLI 是 Bento4 那套工具的 MIT 等价物 → **不再需要 GPL 的 Bento4**；`mfreadwrite.dll` 在，可作在环解码验证 | Bento4 工具（GPL）仅在需要第三方 oracle 时于开发机上独立运行，不分发 |
| 14 | UI | **eframe/egui 0.36.2**，三页签（录制/设置/剪辑）+ 自绘时间线 | 已解析并编译通过；**注意 0.36 API 已变**（见 §2.6） | 降到旧版 egui（不推荐：丢修复） |
| 15 | 配置/多语言 | 内嵌 JSON（`include_str!`）+ serde；zh-CN / en-US | 已就绪：`i18n` 有"中英 key 集合一致"单测、`config` 有 4 个单测 | 增量加语言 |
| 16 | 录制会话编排 | 4 条线程：DXGI 采集 / WASAPI 音频 / 编码（openh264+MF AAC）/ fMP4 封装；**有界队列 + 显式丢帧策略** | 旧代码的忙等、假线程安全（`Mutex<()>`）、跨线程 `&mut self` 全部要避开 | 单线程串行（简单但掉帧） |
| 17 | 异常恢复 | 录制中文件本身就是**可播放的 fMP4**；正常停止→再生成普通 MP4；崩溃/断电后 fMP4 仍可用 | 规格要求；DXGI `AccessLost` 用 recreate 恢复 | — |

---

## 1. 环境与约束（全部实测）

| 项 | 事实 | 影响 |
|---|---|---|
| 网络 | **HTTPS 全面握手失败**（`schannel: SEC_E_NO_CREDENTIALS`），**HTTP(80) 正常**（`static.rust-lang.org`、清华/中科大/rsproxy、`index.crates.io` 均 200） | 一切依赖必须走 HTTP：rustup 走清华 dist 镜像，crates 走本地索引代理（`_rust/crates-shim.py`，把索引 `config.json` 的 `dl` 改写成 USTC 的 HTTP 端点） |
| Rust | rustc/cargo **1.99.0**，装在 `_rust/`（RUSTUP_HOME/CARGO_HOME 都在工作区内） | 每个新 shell 先 `. _rust/env.ps1` |
| C/C++ | **MSVC 14.51**（VS18 Community）+ Win10 SDK 10.0.28000；**meson / ninja / cmake / nasm / git 全在** | 能编 C/C++ 探针、能重建 openh264、能编 Bento4 工具 |
| Python | 系统 `C:\Python314` + 捆绑 Python（含 Pillow 12.3.0） | 测试素材生成（tone WAV）、抓帧目视核对 |
| 机器 | Windows 10 Pro，DisplayVersion **26H2**，build **26300**，console 会话，用户 `Limit li`（真实交互桌面） | WGC/ DXGI 都能跑；非 RDP |
| 显示器 | 主：DELL **SE2717H/HX 1920×1080@60**；副：**SMS22A330BW 1680×1050@60** | 必须支持多屏与负坐标 |
| 已固化依赖 | eframe/egui **0.36.2**、rfd 0.17.2、windows **0.62.2**、windows-capture **2.0.1**、wasapi **0.26.0**、serde 1.0.229、tracing 0.1.44、tracing-subscriber 0.3.23、directories 6.0.0 | 版本已锁定，避免"猜版本"；`tracing-appender` 已在 P49 删除（全仓零引用） |

**已通过的端到端验证**：`cargo new`→编译→运行 exe；`cargo add`+`cargo build` 拉取并编译 serde/serde_json（HTTP 代理）；windows-capture 2.0.1 全量编译；DXGI 实拍 60 帧并落盘、转 PNG 目视正确。

---

## 2. 关键风险与对策（标注验证状态）

### 2.1 已解决
| 风险 | 结论 |
|---|---|
| WGC 显示器捕获不可用 | 实测报 `ItemConvertFailed`；**改用 DXGI Desktop Duplication** 后实测稳定 61.6 fps。WGC 仍可用于"抓窗口"（后续可选） |
| 依赖拉取（离线沙箱） | HTTP 索引代理 + 清华 dist 镜像，已验证 |
| openh264 导入库坏（名称不符 + 缺符号） | 改为运行时 `LoadLibraryW`+`GetProcAddress`；`build.rs` 同时把 DLL 复制成 `openh264.dll` 与 `openh264-7.dll` 两个名字 |

### 2.2 待实测（有明确对策）
| 风险 | 对策 | 验证方式 |
|---|---|---|
| **DXGI 画面不含鼠标指针** | GDI 合成：`GetCursorPos` + `GetCursorInfo`/`GetIconInfo` + `DrawIconEx(DI_NORMAL)` 画进 32 位 DIB，再叠加到帧；设置项可关 | 探针：移动鼠标前后各抓一帧，比对指针区域像素差 |
| DXGI `AccessLost`（切分辨率/独占全屏/UAC 桌面切换） | 捕获 `Error::AccessLost` → `dup.recreate()`；分辨率变化 → 重建编码器与 Muxer | 探针：录制中改分辨率，观察恢复且文件可播 |
| `Error::Timeout`（画面无变化） | 固定帧率输出：超时就**重复上一帧**（CFR），不阻塞 | 探针：静止画面下统计帧数是否仍达标的 fps |
| 全屏 8.29 MB/帧（60 Hz ≈ 510 MB/s） | 只按目标 fps 取帧；裁剪零拷贝；有界队列满则丢帧并计数 | 压测：1920×1080@30 录制 60 s，看 CPU 占用/丢帧计数 |
| MF AAC 编码器 priming（首帧延迟/静音预热） | 先送 2 帧静音再开始计时，或按实测丢弃前 N 个输出帧 | 探针：编码 1 kHz 正弦，解码校验时长/相位 |
| openh264 首帧是否带 SPS/PPS | 用 `EncodeFrame` 后遍历 `sLayerInfo[*].pNalLengthInByte`/`pBsBuf` 找 nalu_type 7/8；必要时用 `EncodeParameterSets` 显式取 | 子代理 C（OpenH264 FFI）正在做，要求断言"首帧含 SPS+PPS+IDR" |
| fMP4 中途崩溃仍可播 | 每个 moof 写完 flush；`ftyp+moov(mvex)` 在开头 | 用 MF Source Reader 打开"录制中被 kill"的文件 |
| egui 0.36 API 变更 | 以**本地随包源码**为准（`registry/src/.../eframe-0.36.2`）：`App` 必实现 `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)`，另有可选 `fn logic(&mut self, ctx, frame)`；`CentralPanel::show(self, ui: &mut Ui, …)` | 已确认签名（编译错误 + 源码双重确认） |
| 高 DPI 下选区偏移 | 选区直接在**物理像素位图**上做（§0 #4），不做逻辑↔物理换算 | 在 100%（本机）与 150% 缩放下各测一次 |

### 2.3 明确的非目标（本轮不做）
- 摄像头画中画、鼠标点击高亮、实时预览窗、多轨/分轨导出、直播推流。
- 逐帧精确剪辑（规格只要关键帧吸附 + copy 导出）。

---

## 3. 已有资产核查（逐项：可用 / 不可用 + 证据）

### 3.1 CapLite 内随包二进制
| 资产 | 核查结论 | 证据 |
|---|---|---|
| `native/openh264.dll`（804 KB） | ✅ **可用**（动态加载），且与 `openh264/openh264/build_x64/openh264.dll` **SHA256 完全相同** → 就是本机用 meson 构建的 **openh264 2.6.0** | `EB0B38B8…0DC7B9` 两侧一致；导出表仅 6 个函数（`WelsCreateSVCEncoder`/`WelsDestroySVCEncoder`/`WelsCreateDecoder`/`WelsDestroyDecoder`/`WelsGetCodecVersion`/`WelsGetCodecVersionEx`） |
| `native/openh264.lib`（2.9 KB） | ⚠️ **不能直接用**：是 `openh264-7.dll` 的导入库（实测含字符串 `openh264-7.dll`，不含 `openh264.dll`），且**缺** `WelsEncodeFrame`/`WelsSetEncoderParam` | 符号检索 + 归档成员名 `openh264-7.dll/` |
| `native/yuv.lib`（1.28 MB） | ✅ **可用**：静态库，44 成员 / 41 个 x64 COFF 对象，含 `ARGBToI420` 与 `ABGRToI420` | COFF 归档解析 + 符号检索 |
| `native/libyuv.dll`（18 KB） | ❌ **不可用**：导出表 **0 个函数**（空壳） | PE 导出表解析 |
| `native/ap4.lib`（5.19 MB） | ⚠️ 可用但未采用：Bento4 静态库（136 个 x64 对象）；未验证 CRT 匹配（`/MD` vs `/MT`） | COFF 归档解析 |
| `openh264/openh264/`（源码 2.6.0 + `build_x64` 完整 ninja 树） | ✅ **可用**：meson/ninja/nasm 都在，需要改 soname/重编随时 `ninja` | `meson.build: version '2.6.0'`、`build.ninja`、`compile_commands.json`、`openh264.dll`+`openh264.lib` |
| `openh264` 头文件（`codec_api.h`/`codec_app_def.h`/`codec_def.h`） | ✅ **已用于**生成 ABI 地面真值（`_rust/abi_probe.c`），继续作为 FFI 唯一依据 | vtable 10 槽 + 全部结构体 offset |
| `libyuv/libyuv/`（源码） | ✅ 可用（需要时重建 `yuv.lib`） | `include/libyuv/convert_argb.h`、`convert_from_argb.h` |
| `Bento4/Bento4/`（源码 + Apps） | ⚠️ **禁止链接进产品（GPL-2.0）**，仅作**开发期独立验证工具**：`Mp4Dump`/`Mp4Info`/`Mp4Fragment`/`Mp4Edit`/`Mp4IframeIndex`/`Mp4AudioClip`/`Aac2Mp4`/`AvcInfo`（CMake+Ninja 已可构建，见 §6）。技术上它有 `AP4_SyntheticSampleTable::AddSample`（可引用源文件偏移）+`GetNearestSyncSampleIndex`+`AP4_FileWriter`+`AP4_LinearReader`，能力完全够，但**没有增量写分片的类**，且 GPL 会传染整个产品 | Apps 目录清单 + `Ap4.h`（C++ 类，无 `extern "C"`）+ 许可头声明 |
| `legacy/*.rs`（旧 11 个文件） | ❌ **基本不可用**（38 项缺陷：不可能编译、假线程安全、FFI 调不存在的符号、负坐标越界…）；已归档不参与编译。仅"libyuv 调用方式"与"SPS/PPS 提取意图"可参考 | `BUG_REPORT.md` |
| `locales/*.json` | ✅ 已重写为覆盖新 UI 的中英双语，key 对齐由单测保证 | `i18n.rs` 的 `builtin_locales_load_and_align` |
| `src/{error,config,i18n}.rs`、`main.rs` 骨架、`build.rs`、`Cargo.toml` | ✅ 已就绪（依赖已解析并编译过）；`build.rs` 已修正库搜索路径并自动复制 DLL | `cargo build` 输出 |

### 3.2 工作区前序会话资产
| 资产 | 核查结论 |
|---|---|
| `_diag/ffinc/*.h`（avcodec/avformat/frame/packet/pixfmt… 共 9 个头） | ⚠️ **可用但本轮不用**：与随包 FFmpeg 8.x 匹配。仅当改走 FFmpeg 路线时才需要（`avformat` 封装 / `aac` 编码） |
| `_diag/ffenc/Program.cs`（17.5 KB） | ✅ 经验可复用：`NativeLibrary.Load` + 手工委托；视频编码器候选 `h264_mf`/`libx264`/`mpeg4`/`mjpeg`；`avformat_alloc_output_context2`+`av_interleaved_write_frame`+`av_write_trailer` 全流程；也印证"**前序会话已在 .NET 侧打通 FFmpeg 直调**" |
| `_diag/fflibprobe/Program.cs` | ✅ 经验可复用：按路径加载 + `av_guess_format` 探封装器、`avcodec_find_encoder_by_name` 探编码器 |
| `_diag/wasapitest/Program.cs` | ✅ 经验可复用：WASAPI 事件式采集 + 混音 provider + 设备枚举（C# 侧）；对应 Rust 侧我们用 `wasapi` crate |
| `_diag/out-h264.mp4`（61 KB，`isom`）、`out-n901PySide6.mp4`（272 KB，`isom`） | ✅ **可用作剪辑读取器/关键帧吸附的测试素材**（真实 FFmpeg 产物） |
| `_diag/frame15.png`（512 KB） | ✅ 抓帧参考图 |
| `Captura-8.0.0-new/`（.NET 10 WPF 完整产品 + `ffmpeg-libs` 5 个 DLL） | ⚠️ **不进本项目**，但有两点价值：① 其 `HANDOVER.md`/`NEXT-SESSION.md` 记录的音视频同轨/分轨缺陷、离线构建方法可借鉴；② `avcodec-63.dll` 实测含 **libopenh264** 与 `aac` 编码器，是我们"若改走 FFmpeg 路线"的现成资源 |
| `_verify/*`（.NET 容器/主题/崩溃取证） | ❌ 与本项目无关 |

### 3.3 系统侧可用能力
`mfAACEnc.dll`/`mfplat.dll`/`mfreadwrite.dll`（AAC 编码 + MP4 读取验证）、MSVC/meson/ninja/cmake/nasm、Pillow（素材/目视核对）。

---

## 4. 已拍板的决定（含许可结论）

| # | 决定 | 结论 | 影响 |
|---|---|---|---|
| 4.1 | MP4 打包 | **产品保持闭源 → 不链接 Bento4（GPL-2.0）**。最终组合：**`mp4box` 0.14（MIT，读） + `muxide` 0.2.5（MIT/Apache，写） + 自研音视频双轨 fMP4 直播写入器**；验证用 `mp4box` 自带 CLI（MIT）+ `mp4` crate + MF 解码；Bento4 可完全不用 | 无许可风险；`mp4`/`muxide`/`mp4box` 都能从本机 HTTP 镜像取到并编译；只剩约 400 行 fMP4 写入器需自研 |
| 4.2 | 鼠标指针 | **自研 GDI 合成**（`GetCursorInfo`+`DrawIconEx` 画进 32 位 DIB 再叠加），设置项可关 | +100~150 行 unsafe win32；需实测（移动鼠标前后比对指针区域像素差） |
| 4.3 | 剪辑窗口预览 | **加缩略图 / 逐帧预览** | 需要解码路径：用 Media Foundation Source Reader（顺带当验证器）或随包 openh264 的 `WelsCreateDecoder`；预算 +1~2 天 |

### 4.4 许可尽调结论（重要）
| 组件 | 许可 | 能否进闭源产品 |
|---|---|---|
| OpenH264 2.6.0（随包 DLL，BSD） | BSD-2-Clause | ✅ |
| libyuv（`yuv.lib`，BSD） | BSD-3-Clause | ✅ |
| `muxide` 0.2.5 | MIT OR Apache-2.0 | ✅ |
| `mp4` 0.14.0 | MIT | ✅ |
| `windows-capture` / `wasapi` / `windows` | MIT / Apache-2.0 / MIT | ✅ |
| **Bento4（`ap4.lib`/工具）** | **GPL-2.0 或商业授权** | ❌ 链接进产品会传染；**仅开发期以独立 exe 做验证** |
| 随包 FFmpeg（`Captura-8.0.0-new/ffmpeg-libs`） | **LGPLv3**（实测配置串：`--enable-version3 --enable-libopenh264 --disable-libx264 --disable-libfdk-aac`，**无** `--enable-gpl`/`--enable-nonfree`） | ⚠️ 动态链接 + 提供库可替换性/源码即合规，但**本项目不需要它**（我们的路线是 openh264 直调 + muxide） |
| Media Foundation AAC（系统组件） | 操作系统组件 | ✅ |

> 注：随包 FFmpeg 是 LGPLv3 而非 GPL，这点与原 `D:\cap` 的旧代码注释（"OpenH264: MIT License"）一致的许可意识相符；但它含 118 MB DLL，故不采用。

---

## 6. MP4 打包方案调研明细（"替代 Bento4"的候选对比）

| 候选 | 许可 | 读渐进式 | 读分片 | 写渐进式 | 写分片（含音频） | 关键帧信息 | 结论 |
|---|---|---|---|---|---|---|---|
| **`muxide` 0.2.5** | MIT/Apache | ✗（无 reader） | ✗ | ✅ `Muxer`（H.264 Annex-B + ADTS AAC，`finish()`/`finish_in_place()`） | ⚠️ `FragmentedMuxer` **仅视频**（源码注释明确 "Only video configuration is supported for fragmented MP4"） | 由调用方给 `is_keyframe` | **采用（写渐进式）** |
| **`mp4` 0.14.0** | MIT | ✅ `Mp4Reader` | ✅ 解析 `moof/traf/trun` 更新轨道，`is_fragmented()` | ✅ 但需预知样本尺寸（`write_start/add_track/write_sample/write_end`） | ✗ 无分片写 | ✅ `Mp4Sample.is_sync` / `start_time` / `duration` / `bytes` | **采用（读取+交叉验证）** |
| **Bento4** | **GPL-2.0** | ✅ 最强 | ✅ `AP4_LinearReader` | ✅ `AP4_SyntheticSampleTable`+`AP4_FileWriter` | ✗ 无增量写类（`mp4fragment` 是整文件处理） | ✅ `GetNearestSyncSampleIndex` | **仅开发期验证工具** |
| FFmpeg `avformat`（随包） | LGPLv3 | ✅ | ✅ | ✅ `movflags frag_keyframe+empty_moov` | ✅（含音频） | ✅ | 备选：功能最全但 118 MB + 巨型结构体 FFI；**仅当自研 fMP4 写入器翻车时启用** |
| 自研 ISO-BMFF | 自有 | ✅（子代理实现中） | ✅ | ✅ | ✅（**含音频**） | ✅ | **采用（只保留"fMP4 直播写入器"这一必要子集；reader/trim 作为交叉验证与回退）** |

**最终 MP4 分工**
```
录制中   : 自研 fMP4 writer  →  <目标名>.streaming.mp4   （可边录边播、崩溃可播）
停止时   : mp4 crate 读样本 → muxide::Muxer 写        →  <目标名>.mp4（普通 MP4，fast_start）
剪辑窗口 : mp4 crate 读任意 MP4（渐进/分片）→ 关键帧吸附 → muxide::Muxer 重写容器（NAL 载荷逐字节不变）
预览     : Media Foundation Source Reader 解码出缩略图/逐帧
验证     : mp4 crate 读回断言 + MF 解码 + Bento4 mp4dump/mp4info（开发期 oracle）
```


---

## 5. 下一步实施顺序（确认后按此执行）

1. **视频闭环最小可跑**：DXGI 采集（含 `AccessLost`/`Timeout` 处理、`buffer_crop`、GDI 指针合成）→ libyuv → openh264 → 自研 fMP4 写入器，产出**能被 `mp4` crate 读回、能被 MF 解码、能被 Bento4 mp4info 解析**的 mp4。
2. **音频闭环**：`wasapi` 环回 + 麦克风 → 混音/重采样 → MF AAC-LC 192k → 与视频同轨封装；实测"系统声音有能量"（用 tone WAV 播放 + 采集 RMS 断言）。
3. **流模式与导出**：分片 fMP4 写入 + 停止时重建普通 MP4 + **kill 进程后文件仍可播**的回归。
4. **主页面 UI**：全屏/区域、画面+声音/仅画面/仅声音、音源多选、开始/停止/计时、错误可见。
5. **设置页**：语言、存储位置、编码速度（`iComplexityMode`）、编码方式（`RC_BITRATE_MODE`/`RC_QUALITY_MODE`）、码率、关键帧间隔、是否允许跳帧（`bEnableFrameSkip`）。**"熵编码 CABAC"选项取消**——实测在本 API 上不可用（见 §8.3）。
6. **剪辑窗口**：`mp4` crate 打开视频（渐进/分片）、时间线缩放 + 缩略图（MF 解码）、起点吸附后一关键帧/终点吸附前一关键帧、`muxide::Muxer` copy 导出（不重编码、不改分辨率）。
7. **回归脚本**：录制/崩溃恢复/剪辑导出三段自动化 + 产物验证（MF + Bento4 + `h264-reader`）。

> 当前并行进行中的两个子代理（独立 crate，产出后并入第 1、6 步）：
> - **MP4/ISO-BMFF 模块**（分片写、普通写、读取+关键帧索引、copy 导出 + 单测）
> - **OpenH264 FFI 模块**（vtable 调用、动态加载、实测编出含 SPS/PPS/IDR 的码流）

---

## 7. 第二轮选型：你提的三个候选 + 新发现（本节结论覆盖 §6 的读取/验证部分）

### 7.1 `mp4` crate 0.14.0（MIT）—— 部分合适
| 能力 | 实测结论 |
|---|---|
| 读渐进式 | ✅ `Mp4Reader::read_header` |
| 读分片 | ✅ reader 解析 `moof/traf/trun`，有 `is_fragmented()` |
| 样本信息 | ⚠️ `Mp4Sample{start_time, duration, rendering_offset, is_sync, bytes}` —— **样本数据进内存、无文件偏移** |
| 写渐进式 | ✅ `write_start/add_track/write_sample/write_end`，但需**预知样本尺寸** |
| 写分片 | ❌ 无 moof 写出 |

→ **定位：交叉验证器（读回断言）**；不作主读写实现。

### 7.2 `rust-ffmpeg`（现名 `ffmpeg-next` 9.0.0 / 底层 `ffmpeg-sys-next` 9.0.1，WTFPL）—— **本机不可构建 ❌**
三个硬依赖全缺（均实测）：
1. **libclang**：其 build.rs 首行 `extern crate bindgen;` → bindgen 必须有 libclang。全盘搜索（`Program Files`、`Program Files (x86)`、`LOCALAPPDATA`、`APPDATA`、`C:\Python314`、`D:\`、`.dsh`，深度 4）**未找到任何 `libclang*.dll` / `clang.exe` / `llvm-config`**；VS18 只装了 MSVC 工具集，无 LLVM 组件。
2. **完整 FFmpeg 8.x 头文件**：本机只有 `_diag/ffinc` 的 9 个散装头。实测 `avutil.h`、`libavutil/frame.h`、`libavutil/pixfmt.h`、`libavutil/channel_layout.h`、`libavutil/version.h`、`avformat/avio.h`、`swresample.h`、`swscale.h` **全部缺失** → 连预处理都过不了。
3. **导入库**：只有 5 个 DLL（`avcodec-63`/`avformat-63`/`avutil-61`/`swresample-7`/`swscale-10`），**没有任何 `.lib`/`.pc`**（Captura 的 `build-deps` 里也没有 FFmpeg 开发包，只有 .NET DLL）。

另外其 build.rs 还支持 `git clone https://github.com/FFmpeg/FFmpeg` 自行构建 FFmpeg（需 HTTPS，本沙箱不通）。
→ 要启用必须补齐：① libclang（可由 PyPI 镜像的 `libclang` 轮子经 HTTP 取得）② HTTP 可下的完整 FFmpeg 8.x dev 包（头+导入库）③ 版本与 `avcodec-63` 严格对齐。**成本与 ABI 风险都高于收益。**

### 7.3 `ac-ffmpeg` 0.19.0（MIT / 底层 `ffmpeg-sys-the-third` 6.0.0）—— **本机不可构建 ❌**
build.rs 用 `ac_ffmpeg_build::ffmpeg_include_dirs(true)` / `ffmpeg_lib_dirs(true)` 找**系统 FFmpeg 的 include/lib 目录**并链接 `avfilter/avcodec/avformat/avutil/swresample/swscale`；底层同样走 bindgen（需 libclang），另可选自行构建 FFmpeg（需下载源码）。→ 与 7.2 同缺口，且多一个 `avfilter` 依赖。

### 7.4 新发现：**`mp4box` 0.14.0（MIT）才是"替代 Bento4"的正解 ✅**
我另外扫描了 `mp4san` / `mp4box` / `mp4_muxer` / `re_mp4` / `minimp4` / `mp4parse`，`mp4box` 最对口（crate 自述："A minimal, dependency-light MP4/ISOBMFF parser and editor"，依赖仅 `anyhow` + `serde`）：

| 我们需要的 | mp4box 提供 |
|---|---|
| 读任意 MP4（渐进 + 分片/CMAF） | `track_samples_from_path()` / `track_samples_from_reader()`，完整支持 `moof/trun/tfhd/tfdt` |
| 关键帧索引 | `SampleInfo.is_sync`（渐进取 `stss`；分片取 `sample_flags` bit16） |
| **copy 导出必需的文件偏移** | `SampleInfo.file_offset`（`stsc`+`stco/co64`；分片按 base-is-moof 规则计算）→ **不必把样本读进内存** |
| 崩溃残留文件可解析 | `get_boxes_tolerant()`（部分盒树 + 定位到的问题） |
| 重建容器所需编解码参数 | `registry` 结构化解码 `avcC` / `esds`(AudioSpecificConfig) / `hvcC` |
| 编辑/修正 | `edit::Editor` 非破坏性编辑：自动 size 与 chunk-offset 修正、faststart |
| 验证工具 | **自带 `mp4dump` / `mp4info` / `mp4samples` / `mp4edit` 四个 CLI（MIT）** → 取代 GPL 的 Bento4 工具 |
| 从样本创建新 MP4 | ❌ 它是 parser+editor，不是 muxer → 由 `muxide::Muxer` 承担 |

其他候选裁决：`mp4_muxer` 0.1.0（MIT）自述 **"No fragmented MP4 (fMP4/DASH/HLS) output"** 且只做 AV1 → ✗；`re_mp4` 0.5.1（MIT）是 rerun 的**只读** fork（仅 reader）→ 可作交叉验证；`mp4san` 0.5.4（MIT）是解析/净化器（支持分片）→ 可作交叉验证；`minimp4-sys` 0.1.1（MPL-2.0）无分片写 → ✗。

### 7.5 修正后的 MP4 分工（同一套实现同时服务"停止导出"与"剪辑导出"）
```
录制中   : 自研 fMP4 写入器（视频+音频双轨）→ <名>.streaming.mp4   ← 唯一自研环节（约 400 行，只写不解析）
停止时   : mp4box 读样本表(file_offset/is_sync) → muxide::Muxer 写出 → <名>.mp4（普通 MP4）
剪辑窗口 : mp4box 读任意 MP4 → 关键帧吸附 → muxide::Muxer 重写容器（NAL 载荷逐字节不变；不重编码、不改分辨率）
预览     : Media Foundation Source Reader 解码缩略图 / 逐帧
验证     : mp4box CLI(mp4dump/mp4info/mp4samples) + mp4 crate 读回 + MF 解码（三方互证）
```
> 兜底方案（若自研 fMP4 写入器出问题）：仅画面模式直接用 `muxide::FragmentedMuxer`；"画面+声音"时录制中只落"视频 fMP4 + 独立 ADTS 音频临时文件"，停止时合并——代价是**录制中的文件没有声音**，故仅作降级。

---

## 8. 第三轮选型：你问的三个 crate 的最终裁决（最终结论，覆盖 §7）

### 8.1 结论表
| 候选 | crate 许可 | 本机可行性（实测） | 裁决 |
|---|---|---|---|
| `mp4` 0.14.0 | MIT | ✅ 已从 HTTP 镜像拉到并编译通过 | **可用**（读取 + 交叉验证）；❌ 不能写分片；写渐进式需预知样本尺寸；样本进内存、无文件偏移 |
| `rust-ffmpeg`（`ffmpeg-next` 9.0.0 / `ffmpeg-sys-next` 9.0.1） | WTFPL | ⚠️ 卡点已全部定位：libclang **可以拿到**；但完整 dev 包只有 **GPL** 版本 | **闭源前提下不可用** |
| `ac-ffmpeg` 0.19.0 | MIT | ⚠️ 同上，且额外需要 `avfilter` | **不可用（无优势）** |

### 8.2 卡点实测明细（证据）
1. **libclang：可拿到 ✅（不再是卡点）**
   - 全盘无 LLVM/libclang，但 HTTP PyPI 镜像可用：从 `http://mirrors.aliyun.com/pypi/simple/libclang/` 直下 `libclang-18.1.1-py2.py3-none-win_amd64.whl`（**25.2 MB**），内含 **80.1 MB `libclang.dll`** → 设 `LIBCLANG_PATH` 即可满足 bindgen。
   - 注：`pip download` 会因沙箱对临时目录的限制报 `PermissionError`（即使把 TEMP 指到工作区）；**直接 curl 轮子 URL 即可**（URL 从 simple 索引页正则解析）。
2. **完整 FFmpeg 头文件 + 导入库：HTTP 可达的只有 GPL 版本 ❌**
   - `git ls-remote https://github.com/FFmpeg/FFmpeg` → **schannel 同样失败**（git 的 HTTPS 也不通）
   - `http://ffmpeg.org/releases/*.tar.xz` → **301 跳 HTTPS**（源码 tarball 拿不到）
   - MSYS2 镜像（HTTP 200）有 `mingw-w64-x86_64-ffmpeg 9.0.2-1`，但其 `%LICENSE% = spdx:GPL-3.0-or-later`，依赖含 `libx264`/`x265`/`xvidcore`/`rtmpdump` → **GPLv3 构建**
   - 我们手上**许可可行**的是 **LGPLv3 的 FFmpeg 8.x**（`avcodec-63`/`avformat-63`/`avutil-61`/`swresample-7`/`swscale-10`，Captura 随包），但**没有头文件**，且与 crate 的 **9.x** 主版本绑定不匹配
   - 本机 `C:\ffmpeg\bin`：gyan.dev 2024-10-24 essentials，`--enable-gpl --enable-libx264`，libavcodec **61**（FFmpeg 7.x），**只有 3 个 exe，无 `include/`、无 `lib/`**
   → **保持闭源 ⇒ ffmpeg-next / ac-ffmpeg 在本机无法构建。**
3. **唯一现实启用路径**：由你在有外网的机器上取一份 **LGPL 的 FFmpeg 8.x shared dev 包**（如 BtbN 的 `ffmpeg-n8.0-*-win64-lgpl-shared-*.zip`，含 `include/`、`lib/`、`bin/`），拷进工作区；我再配 `LIBCLANG_PATH`（轮子已就位）即可编译 `ffmpeg-next`，届时 AAC 编码 / fMP4 / copy remux / 解码缩略图可一次性由 FFmpeg 提供。

### 8.3 OpenH264 交付带来的、必须落到设计的修正
1. **CABAC 不可用（实测）**：`ParamValidationExt` 只接受 `CAMERA_VIDEO_REAL_TIME(0)` / `SCREEN_CONTENT_REAL_TIME(1)`，而 CABAC 仅对 non-real-time 类型生效 —— 正是被拒的那两个。→ 设置页**取消"熵编码 CABAC/CAVLC"选项**（§0 #15、§5 第 5 步已改）。
2. **`yuv.lib` 是 `/MT` 静态 CRT**，且 `cpu_id.obj` 遗留 `__imp_fopen`/`__imp_fgets` 未解析 → 产品 `build.rs` 必须带 **CRT 链接桩**（`_rust/proto-oh264/cprobe/crt_stubs.c` 已可用），否则 `cargo:rustc-link-lib=static=yuv` 链接失败。
3. **`ENCODER_OPTION_BITRATE` 的参数是 `SBitrateInfo*`（`{i32 iLayer; i32 iBitrate}`）而非 `int*`**；且 `set_bitrate(目标 > 当前 maximum)` 会被拒（`ENC_RETURN_UNSUPPORTED_PARA`，"改 8 Mbps 失败但改 900 kbps 成功"）→ 复用子代理 wrapper（自动先抬高 ceiling）。
4. `bEnableFrameSkip=false` 时 OpenH264 压不住低码率（会自己警告 `bitrate can't be controlled ... without enabling skip frame`）；需要硬压码率才置 `true`（默认关，绝不静默丢帧）。

### 8.4 资产更新：OpenH264 模块已交付 ✅（`_rust/proto-oh264/`）
纯 std、零依赖；**32 项测试全绿（debug + release）、零编译警告**。公开面：`Encoder/EncoderConfig`（RAII，Drop 里 `Uninitialize`+`WelsDestroySVCEncoder`）、`encode_i420` / `encode_i420_ex`（含 `is_keyframe`）/ `encode_i420_into`（免分配）、`set_bitrate`/`set_max_bitrate`/`set_complexity`/`force_intra_frame`/`encode_parameter_sets`/`get_statistics`、`Nal` + `parse_annex_b`、`OpenH264Lib::load()`（多路径探测 + 清晰错误）。
产物 `out/*.264` 已被 **ffmpeg 逐帧独立解码验证**（关键帧位置与我们上报的 0/10/20 完全一致）。
另交付 **libyuv 语义实测**：`ARGBToI420` 的 `ARGB` 内存序 = **B,G,R,A**（故 `ARGBToI420` 处理 BGRA 数据正确）、stride 单位=字节、BT.601 limited range、目标 stride 更宽时填充列不写、`libyuv.dll` 确为 0 导出空壳。

### 8.5 开发期验证 oracle：本机 ffmpeg CLI（仅验证，不进产品）
`C:\ffmpeg\bin\{ffmpeg,ffprobe,ffplay}.exe`（GPL、FFmpeg 7.x、85 MB）可用于：
- `ffprobe -show_frames` 取帧级/关键帧信息，交叉核对我们的 fMP4/普通 MP4 样本表
- `ffmpeg -i our.mp4 -f null -` 全解码校验码流合法性（已在 OpenH264 模块中用过）
- `ffmpeg -c copy` 做参考 remux，与我们的 `muxide` 输出对照
**不链接、不随产品分发。**

---

## 9. 第四轮：用户提供的 FFmpeg 开发包 —— **实测可用 ✅**（本轮覆盖 §7.2/§8.1 中"不可用"的结论）

用户在工作区放入 `CapLite/ffmpeg-master-latest-win64-lgpl-shared/`（BtbN 的 **LGPL shared** 构建）。我做了三层实测，**结论：可用**。

### 9.1 包的资质（实测）
| 项 | 值 |
|---|---|
| 版本 | FFmpeg **master N-127367**（2026-10-11），`libavcodec 63.18.101`、`libavformat 63.8.100`、`libavutil 61.13.100`、`libswresample 7`、`libswscale 10` |
| **许可** | 运行时自证配置串：`--enable-version3 --enable-shared --enable-libopenh264`、`--disable-libx264 --disable-libx265 --disable-libxvid --disable-libfdk-aac`、**无 `--enable-gpl` / `--enable-nonfree`** → **LGPLv3，可进闭源产品**（含 `LICENSE.txt`） |
| 开发文件 | `include/`（七套头文件）、`lib/*.lib`（**MSVC 导入库**，另附 `.def`/`.dll.a`）、`lib/pkgconfig/*.pc`、`bin/*.dll` + **`ffmpeg.exe`/`ffprobe.exe`/`ffplay.exe`（同为 LGPL）**、`presets/`、`doc/` |
| 体积 | `bin/` 约 172 MB（`avcodec-63` 87.7 MB、`avfilter-12` 30.2、`avformat-63` 24.5、`avutil-61` 2.9、`swscale-10` 2.3、`swresample-7` 0.7、`avdevice-63` 4.7） |

### 9.2 构建配方（4 个条件，已全部跑通并写进 `_rust/env.ps1`）
```powershell
$env:FFMPEG_DIR      = "...\CapLite\ffmpeg-master-latest-win64-lgpl-shared"   # 走 sys crate 的 FFMPEG_DIR 分支：只用 include/ + lib/，无需 pkg-config，不校验版本
$env:LIBCLANG_PATH   = "...\_rust\libclang\libclang-18.1.1.data\platlib\clang\native"  # bindgen 必需（该轮子从 aliyun PyPI HTTP 镜像直下）
$env:BINDGEN_EXTRA_CLANG_ARGS = '-include C:/Users/LIMITL~1/.../CapLite/native/BINDGE~1/CUDA_S~1.H'
```
**坑 1（必须处理）**：`ffmpeg-sys-next` 的 `hwcontext_wrapper.h` 只补了 `CUcontext`/`CUstream`，但这版 FFmpeg 新增了 `AV_PIX_FMT_CUARRAY` → `hwcontext_cuda.h` 还需要 **`CUarray` 与 `CUDA_ARRAY3D_DESCRIPTOR`**，否则 bindgen 报 `unknown type name 'CUarray'`。
→ 已建桩头文件 `CapLite/native/bindgen-stubs/cuda_stub.h` 并用 `BINDGEN_EXTRA_CLANG_ARGS` 注入。
**坑 2**：该环境变量走 **shlex 式分词**——反斜杠会被当转义吃掉、空格会切分。所以必须**正斜杠 + 无空格路径**（用 8.3 短路径 `C:/Users/LIMITL~1/...`）。两次失败的真实报错：`'C:UsersLIMITL~1...' file not found`。
**坑 3**：运行期 DLL 必须能被找到（把 `bin/*.dll` 复制到 exe 旁，或把 `bin` 加进 PATH）。

### 9.3 实测输出（`_rust/probe-ff/`，依赖 `ffmpeg-sys-next 9.0.1`）
```
[probe] avcodec  = 63.18.101        [probe] avformat = 63.8.100
[probe] 许可自检通过：LGPLv3 构建、无 GPL 组件
[probe] 编码器 aac 可用 / libopenh264 可用 / h264_mf 可用 / libx264 不可用（正确）
[probe] mp4 输出上下文创建成功：mp4 (MPEG-4 Part 14)     [probe] mp4 支持 movflags 选项: 是
[probe] 全部通过 ✅
```
再用**我们 openh264 模块的真实码流**（`proto-oh264/out/smoke_1280x720.264`）做端到端：
- `-c:v copy -movflags frag_keyframe+empty_moov` → **分片 MP4 110.1 KB**，含 `ftyp/moov/mvex/trex/moof/traf/trun/mdat`
- `-c:v copy -movflags +faststart` → 普通 MP4 109.8 KB
- `ffprobe`：`h264 1280x720 30fps duration=0.99999`（30 帧@30fps 精确）；分片文件 `-f null -` 全解码 exit 0

### 9.4 ⚠️ 重要取舍：**用底层 `ffmpeg-sys-next`，不要用高层 `ffmpeg-next`**
高层 `ffmpeg-next 9.0.0` 对着这份 **master** 头文件编译会失败（穷举 `match` 认不全新枚举）：
```
E0004: AV_SAMPLE_FMT_DSD 未覆盖
E0004: AV_PIX_FMT_CUARRAY / AV_PIX_FMT_XYZP12LE / AV_PIX_FMT_XYZP12BE 未覆盖
E0004: AV_FRAME_DATA_DOWNMIX_MATRIX / AV_FRAME_DATA_GAIN_MAP_PARAMS 未覆盖
E0004: AV_CODEC_ID_ASTC / AV_CODEC_ID_RW2 / AV_CODEC_ID_PCM_DVDA 等未覆盖
```
两条路：
- **(推荐) 只用 `ffmpeg-sys-next` + 自写薄封装**：bindgen 用**本包头文件**生成布局 → 与包内 DLL 天然一致，且不受高层 crate 的版本漂移影响。我们只需要十来组 API（avformat 输出/输入、avcodec 编码/解码、packet 拷贝），约 400~600 行。
- (备选) 换一份**与 crate 发行版匹配**的 FFmpeg（如 BtbN 的 `n9.0` 资产），让 `ffmpeg-next 9.0.0` 原样编译。缺点：被钉死在某个 FFmpeg 版本上。

### 9.5 它能替代设计里的哪些环节（省掉的工作量）
| 原计划 | 改用 FFmpeg 后 |
|---|---|
| Media Foundation AAC MFT 编码（含 priming 调试） | ✅ `aac` 编码器（libavcodec），**整块删掉** |
| 自研 fMP4 直播写入器（约 400 行，唯一自研 MP4 代码） | ✅ `movflags=frag_keyframe+empty_moov`，**整块删掉** |
| 停止时导出普通 MP4 | ✅ 同一套 avformat（`+faststart`） |
| `muxide` 写普通 MP4 | ✅ 可删（保留仅作交叉验证） |
| `mp4box`/`mp4` crate 读样本表 + copy 导出 | ✅ avformat packet 级 `-c copy` + `av_seek_frame` 关键帧吸附（**不重新编码、不改分辨率**） |
| 剪辑窗口缩略图/预览的解码路径 | ✅ avcodec H.264 解码 |
| libyuv BGRA→I420 + `yuv.lib` 的 CRT 桩 | ⚠️ 可选改用 `swscale`（但我们已有 libyuv，体积更小，建议保留） |
| Bento4 / mp4box / muxide / 自研 writer 的多方验证 | ✅ 包内自带 **LGPL `ffprobe`/`ffmpeg`** 可直接当验证器（**可随产品分发**，与 GPL 的 `C:\ffmpeg` 不同） |

**代价**：随包体积约 **116~172 MB**（至少需 `avcodec/avformat/avutil/swresample` 4 个 DLL）+ **LGPLv3 合规**：必须动态链接（满足）、随包 `LICENSE.txt` 与库原文、允许用户替换这些 DLL、不得静态链接。视频编码仍可按规格走 openh264（`libopenh264` 编码器也在这份 FFmpeg 里，或继续用我们已交付的原生 openh264 模块）。

### 9.6 下一步待验证（尚未做）
用 `ffmpeg-sys-next` 写**产品自己的薄封装**并实测：① AAC-LC 192k 编码 + ② 分片 MP4 边录边写（含音频轨）+ ③ 停止时 remux 成普通 MP4 + ④ 按关键帧吸附的 packet 级 copy 导出 + ⑤ 解码取缩略图。

> 注：§9.6 的"自写薄封装"已被 §9.7 取代 —— 选对包之后高层 crate 可直接用。

---

## 9.7 【最终】两个 FFmpeg 包的实测对比 → **选 n9.0 发行分支**

工作区里现有两份 LGPLv3 shared 包（均为 BtbN，均含 `libopenh264`、同带 LGPL `ffmpeg/ffprobe`）：

| 项 | `ffmpeg-n9.0-...-lgpl-shared-9.0` ✅ | `ffmpeg-master-...-lgpl-shared` |
|---|---|---|
| 版本 | `n9.0.2-25-g67b60c310b`（release/9.0 分支），libavutil 61.**1**.102 | master `N-127367`，libavutil 61.**13**.100 |
| **高层 `ffmpeg-next 9.0.0`** | ✅ **编译 + 运行通过**（clean target 复现：`Finished in 50.33s`；运行输出 avcodec 63.1.102 / avformat 63.1.102、LGPL 自检通过、libopenh264+aac 可用、mp4 输出上下文 OK） | ❌ **4 个 E0004**（穷举 match 认不全新枚举） |
| 结论 | **采用**：高层安全 API 可用（且 crate 自带 `examples/{remux,transcode-x264,transcode-audio}.rs` 可借鉴）→ **代码最少** | 只能用底层 `ffmpeg-sys-next` + 自写 400~600 行封装 |
| 体积 | avcodec 86.9 / avformat 21.7 MB | avcodec 87.7 / avformat 24.5 MB |
| 许可 | LGPLv3（无 GPL 组件） | 同 |

**踩坑记录（我亲自踩的）**：`env.ps1` 原先默认指向 **master** 包，导致同一份 `probe-ff9` 一次"编译通过"、一次"4 个 E0004"——**用错包就会误判**。现已把 `env.ps1` 改成 **n9.0 优先、master 备选**，并在注释里写明原因。
> 另外：master 分支比 crate 新这一事实本身说明"高层 crate 有版本耦合"；若将来升级 FFmpeg，需要同步升级 `ffmpeg-next` 或退回 sys-only 方案。

### 9.8 FFmpeg 的 `libopenh264` 封装暴露了哪些参数（决定设置页能否保住）
```
-loopfilter <int>            启用环路滤波
-profile  {constrained_baseline|main|high}
-coder    {default|cavlc|cabac|vlc|ac}        ← 但 OpenH264 实时档位下 CABAC 实际不可用（见 §8.3）
-rc_mode  {off|quality|bitrate|buffer|timestamp}
-allow_skip_frames <bool>    为压码率允许跳帧
-max_nal_size <int>
（通用） -b:v 码率 / -g 关键帧间隔 / -threads
```
**关键缺口：没有 `complexity`（复杂度/编码速度档位）** → 规格里"设置页 → 编码速度（快/中/慢）"在 FFmpeg 路径下**无法实现**。
而我们**已交付的原生 openh264 模块**（§8.4）暴露了 `iComplexityMode`（LOW/MEDIUM/HIGH，实测三种档位输出不同）、`iRCMode`、`bEnableFrameSkip`、线程数、level/profile → **控制力最全，且集成成本≈0**。

### 9.9 两条路线的工作量对比（决定"代码最少"）
| 环节 | 原计划（自研 + MIT 组合） | 全程 FFmpeg（n9.0 + `ffmpeg-next`） |
|---|---|---|
| 视频编码 | 原生 openh264 模块（**已交付**，集成 ~50 行） | `libopenh264`（~30 行）**但丢 complexity 档位** |
| 音频编码 | Media Foundation AAC MFT（~400 行 + priming 调试） | `aac` 编码器（~60 行） |
| 录制中分片 MP4 | **自研 fMP4 写入器 ~400 行（最高危）** | `avformat` + `movflags=frag_keyframe+empty_moov`（~50 行） |
| 停止导出普通 MP4 | `muxide` 集成（~120 行） | 同一套 avformat remux（~80 行） |
| 剪辑 copy 导出 | `mp4box` 读 + `muxide` 写（~300 行） | `av_seek_frame` + packet `-c copy`（~150 行） |
| 缩略图/预览解码 | MF Source Reader（~250 行） | `ffmpeg-next` 解码（~120 行） |
| **合计新增代码** | **≈1500 行**（含 ~800 行自研高危 MP4/AAC） | **≈440–490 行**（全部走成熟库） |

**结论**：FFmpeg 路线约**省 1000 行代码**，并彻底删掉两块自研高危代码（fMP4 写入器、MF AAC）。代价是随包 **≈112 MB**（`avcodec 86.9 + avformat 21.7 + avutil 2.9 + swresample 0.7`，至少这 4 个）与 LGPLv3 合规（动态链接 / 随包 LICENSE / 允许替换 DLL）。

---

## 10. 【已确认的最终方案】+ 编码器基准实测

用户已确认采用下述方案（"原生 openh264 编码 + 其余全程 FFmpeg"）。

### 10.1 最终技术栈
| 环节 | 方案 |
|---|---|
| 视频采集 | `windows-capture` 2.0.1 的 **DXGI Desktop Duplication**（实测 61.6 fps / `Bgra8` / `row_pitch=7680` 无 padding）；WGC 显示器路径本机不可用 |
| 鼠标指针 | 自研 **GDI 合成**（`GetCursorPos`+`GetCursorInfo`+`DrawIconEx` 画进 32 位 DIB 再叠加），设置项可关 |
| 区域录制 | 同一帧上裁剪（`row_pitch` + 指针偏移零拷贝交给 libyuv） |
| 色彩转换 | 本地 **libyuv** 静态库 `yuv.lib`（`ARGBToI420`，语义已实测：BGRA 字节序 / stride 字节 / BT.601 limited） |
| **视频编码** | **原生 openh264 2.6.0 模块**（已交付，32 测试全绿）→ 保住"编码速度(complexity)/编码方式(RC)/码率/跳帧/线程数"设置项 |
| 音频采集 | `wasapi` 0.26（环回 + 麦克风）+ 自研混音/重采样（48 kHz/2ch/f32） |
| **音频编码** | **FFmpeg `aac`**（AAC-LC 192 kbps） |
| **录制中封装** | **FFmpeg 分片 MP4**（`movflags=frag_keyframe+empty_moov`，可边录边播、崩溃可播） |
| **停止导出** | **FFmpeg remux → 普通 MP4（`+faststart`）** |
| **剪辑** | **FFmpeg `av_seek_frame` + packet 级 copy**：起点吸附后一关键帧 / 终点吸附前一关键帧；不重编码、不改分辨率 |
| 预览/缩略图 | FFmpeg 解码（avcodec H.264） |
| UI / 设置 / i18n | eframe/egui 0.36.2（注意 `App::ui(&mut Ui)` 新 API）+ 内嵌 JSON（已就绪） |
| 验证 | 包内 LGPL `ffprobe`/`ffmpeg` + openh264 模块已有测试 + `mp4box`/`mp4` crate 交叉验证 |

### 10.2 编码器基准（真实屏幕内容 1080p，90 帧，目标 8 Mbps CBR，GOP 60）
参考序列：真实桌面截图 + 滚动/移动方块运动（`_rust/bench/ref_1080p.yuv`，267 MB，帧间平均绝对差 16.8）。
脚本：`_rust/bench-encoders.ps1`（v2，已修正度量方法）+ `_rust/bench-enc`（直接用库 API 测 slice/线程）。

| 配置 | 编码速度 | 实际码率 | PSNR-Y | SSIM |
|---|---|---|---|---|
| screen_content + 单 slice + 1 线程 | **27.5 fps** | 1157 kbps | 44.91 | 0.99633 |
| screen_content + 16 slice + 20 线程 | 36.5 fps | 1241 kbps | 45.65 | 0.99715 |
| camera_rt + 单 slice + 1 线程 | 60.5 fps | 2465 kbps | **53.46** | 0.99896 |
| **camera_rt + 16 slice + 20 线程** | **129.1 fps** | 2530 kbps | 53.30 | 0.99890 |
| camera_rt + auto + **Low** 复杂度 | **156.4 fps** | 2561 kbps | 53.28 | 0.99887 |
| FFmpeg 封装 `libopenh264` | 284.5 fps | 2424 kbps | **53.48** | 0.99893 |
| FFmpeg `h264_mf`（Media Foundation） | 350.5 fps | 1919 kbps | 50.86 | 0.99853 |

**三条硬结论**：
1. **决定速度的是 usage type，不是线程**：`SCREEN_CONTENT_REAL_TIME` 会启用调色板/intra 等昂贵工具，比 `CAMERA_VIDEO_REAL_TIME` **慢 2.2 倍**，且码率只用掉 1130/8000 kbps → PSNR 掉 8.5 dB。**默认改用 `camera_rt`**。
2. **线程必须配合 slice 才生效**：单 slice 时 `t=1/auto/8/20` 结果**逐字节相同**（27.5 fps）；改成 `FixedSlcNumSlice` 后 60.5 → **129.1 fps**（OpenH264 会把 1080p 的 `uiSliceNum` 自动夹到 16；`RasterSlice` 会因 `uiSliceNum 68 > MAX 35` 直接初始化失败，**不要用**）。
3. **我们的模块与 FFmpeg 封装在同一配置下质量几乎相同**（53.46 vs 53.48 dB，体积 2408 vs 2424 kbps）→ 证实 FFmpeg 硬编码 `camera_rt`；两条路不存在"谁画质更好"，差别只在**参数控制力**（我们能设 complexity，它不能）。

**产品默认编码参数**（据上表确定）：
```
usage_type        = CameraVideoRealTime        // 见结论 1
complexity        = Medium（设置项：快=Low ≈156fps / 中=Medium ≈129fps / 慢=High）
rc_mode           = Bitrate (CBR)
threads           = 0 (auto)
slice_mode        = FixedSlcNumSlice
slice_num         = 0 (auto，会被夹到 16)
enable_frame_skip = 默认 false（用户可开；开才可能真正压住码率）
intra_period      = 关键帧间隔秒 × fps
```
**实时性余量**：1080p30 需 ≥30 fps，实测 129–156 fps → **4~5 倍余量**；4K（4 倍像素）约 32–39 fps，对 4K30 偏紧（需降复杂度或改用硬件编码，但本机硬件编码器全部不可用，见 §10.3）。

### 10.3 本机硬件编码器可用性（实测，全部不可用）
`h264_nvenc`：`Cannot load cuMemAllocAsync`（驱动/GPU 不支持）；`h264_qsv`：MFX session -9；`h264_amf`：缺 `amfrt64.dll`；`h264_d3d12va`/`h264_vulkan`：像素格式协商失败。
→ 只能走软件编码（openh264 / h264_mf）。**`h264_mf` 是可选备选**（350 fps、PSNR 50.86@1919kbps），但规格要求 openh264，故仅作为将来的"编码器"下拉项候选。

### 10.5 【2025 修订】编码参数收敛：ABR + 平衡 + 上限 20 Mbps + CABAC（全部内部固定）

用户要求"外部只留目标码率与帧率，其余内部定"，于是设置页只剩 **目标码率（5–20 Mbps 滑尺，默认 5）/ 帧率（30 或 60 下拉）/ 关键帧间隔**，
其余参数在 `src/video/encode.rs::build_encoder_config()` 里写死：

| 参数 | 取值 | 依据 |
|---|---|---|
| `usage_type` | `CameraVideoRealTime` | §10.2 结论 1：比 screen_rt 快 2.2 倍且 PSNR 高 8.5 dB |
| `iRCMode` | **ABR**（`RcMode::Bitrate` = `RC_BITRATE_MODE` = 1） | 屏幕内容要稳定体积；外部不再给 CBR/VBR 选项 |
| `iComplexityMode` | **Medium（平衡）** | 129 fps vs Low 156 fps / High 更慢；60 fps 目标下有 2 倍余量 |
| `iTargetBitrate` | `video_bitrate_kbps × 1000`，夹到 5–20 Mbps | 设置页滑尺 |
| `iMaxBitrate` / 每层 `iMaxSpatialBitrate` | **20 Mbps（`MAX_BITRATE_BPS`，写死）** | 与目标码率上限对齐，保证"目标 ≤ 上限"，`InitializeExt` 不会因码率被拒 |
| `iMultipleThreadIdc` | 0（auto） | §10.2 结论 2；slice = `FixedSlcNumSlice`、`uiSliceNum` = 0 |
| `bEntropyCodingModeFlag` | **1（CABAC）** | 见下 |

**CABAC 实测（推翻本仓库早先"CABAC 不可用"的说法）** —— `cargo test --lib probe_cabac_and_max_bitrate -- --nocapture`，
1920×1080、30 帧合成屏幕内容、ABR 目标 5 Mbps、上限 20 Mbps、Medium：

| 配置 | 产物字节 | 编码速度 | PPS 里的 `entropy_coding_mode_flag` | SPS profile | 解码 PSNR |
|---|---|---|---|---|---|
| `CameraVideoRealTime` + CAVLC | 299 294 B | 83.7 fps | 0 | 66（Baseline） | 52.73 dB |
| `CameraVideoRealTime` + **CABAC** | **158 688 B（−47%）** | 83.2 fps | **1** | 100（High） | **52.73 dB** |
| `CameraVideoNonRealTime` + CABAC | — | — | — | — | **初始化失败**：`ParamValidationExt(),Invalid usage type = 2` → `cmInitParaError` |
| `ScreenContentNonRealTime` + CABAC | — | — | — | — | 同上（`Invalid usage type = 3`） |

结论：**CABAC 在实时 usage type 下真的生效**（PPS 标志位 + High profile + 体积减半 + PSNR 完全不变 + 速度不变），
而**非实时 usage type 在这个 openh264 构建里根本不被接受**。所以"优先 CABAC"是**零代价**拿到的：
不需要牺牲帧率，也不需要碰非实时模式。`Config::prefer_cabac`（默认 `true`）是**非 UI 开关**，
写进配置文件，万一将来某台机器上 CABAC 得不偿失可以改一行配置回退 CAVLC，不必改代码。

目标码率上下边界也实测过：`target=5 Mbps` 与 `target=20 Mbps`（**正好等于内部上限**）都能初始化并编出全部 30 帧，
实测码率分别是 4.79 / 5.30 Mbps（合成内容吃不满目标值，属正常）。

### 10.6 【2025 修订】录制帧率上限 60、抽帧语义，以及"编码慢于目标帧率时不让时间轴失真"

* **硬上限 60**：`recorder::MAX_RECORD_FPS = 60`，`Recorder::start()` 用 `effective_fps()` 再夹一次
  （配置侧虽然只有 30/60 两档，但编码路径不依赖 UI 的约束）。
* **抽帧语义 = 取当前最新一帧，丢掉中间被超越的帧**：`windows-capture` 的 `acquire_next_frame()`
  就是 `IDXGIOutputDuplication::AcquireNextFrame`，并把 `DXGI_OUTDUPL_FRAME_INFO` 原样透出
  （`DxgiDuplicationFrame::frame_info()`）。其中的 `AccumulatedFrames` 是"操作系统自上次取帧以来
  **累计并合并**的帧数（含本次返回的这一帧）"，所以 DXGI 永远只给**当前最新**的桌面图像 ——
  不存在"按刷新率取到排队历史帧"的可能，中间帧由系统合并。
  多出来的部分（`AccumulatedFrames − 1`）计入 `CaptureStats::superseded_frames` →
  `RecorderStats::superseded_frames`，这就是"先舍弃多余帧"的**可观测证据**。
* **节拍重同步**：`advance_deadline()`（纯函数，有单测）在落后 ≥ 5 拍时丢弃落后额度、对齐到"现在"，
  并计入 `RecorderStats::pacing_resyncs`（不再静默跳帧）。
* **视频 pts 用真实采集时刻**（`PtsSource::RealTime`，单调时钟 − 录制开始），不用"帧序号 × 1000/fps"。
  原因：编码慢于目标帧率时（一秒墙钟只产出 N < fps 帧），按帧序号计时会让视频**比真实时间短**、
  与按墙钟走的音轨错位（播放表现为快放）。两种口径由 `video_pts_ms()` 提供，单测里做了对照。

**本机实测（1920×1080，4 秒，GOP 2 s，音频为系统环回）**：

| 场景 | 墙钟 | 编码帧数 | 实测 fps | 视频时长 | 音频时长 | 被舍弃(合并)帧 | 节拍重同步 |
|---|---|---|---|---|---|---|---|
| `--fps 30` | 3.934 s | 118（期望 118，−0.02%） | 29.99 | 3941 ms | 3926 ms | **123** | 0 |
| `--fps 60` | 3.937 s | 234（期望 236，−0.93%） | 59.44 | 3947 ms | 3946 ms | 7 | 0 |
| `--fps 60` + 24 个 CPU 烧机进程 | 3.124 s | **125（期望 187，−33%）** | **40.02** | **3127 ms** | **3135 ms** | 107 | 18 |

第三行就是"编码跟不上目标帧率"的真实场景：**帧数少了 33%，但视频时长 3127 ms ≈ 墙钟 3124 ms ≈ 音频 3135 ms**（差 ≤ 11 ms）。
若还用帧序号打戳，同样的 125 帧会被当成 60 fps 的内容播成 2083 ms —— 快放 33%。

**代价（如实记录）**：GOP 是**帧数**（现在是固定的 2 秒 → `2 × fps` 帧），编码变慢时关键帧在**时间上**会变稀：
GOP = 120 帧在 40 fps 下最后一个关键帧落在 **2.94 s** 而不是 2.0 s（帧率正常时是 2.004 s / 2.014 s，正好整秒）。
同一条链意味着"崩溃最多丢失时长"也会随之变长（§11.8 的结论按帧率正常时成立）。

### 10.7 【2025 修订】"自动"关键帧的实测落地、导出成功后删流文件、界面只留必需项

**关键帧间隔：内部自动。** 设置页已删掉"关键帧间隔"和它下面那两行说明（都属于绑在已删设置项上的残留文案），
`Config::keyframe_interval_sec` 语义固定为 `0 = 自动`（`sanitized()` 把任何输入归一为 0；
只有测试用 `recorder_smoke --gop N` 在 `sanitized()` **之后**直接覆盖字段）。实测"自动"到底意味着什么：

| `uiIntraPeriod` | 30 fps 录 6 s | 60 fps 录 6 s | 结论 |
|---|---|---|---|
| **0（OpenH264 自动）** | 178 帧 / 关键帧 **1** 个（0.000 s） | 355 帧 / 关键帧 **1** 个（0.000 s） | **只有首帧 IDR，没有周期性关键帧** → 不能直接用（崩溃会丢整段、剪辑无法吸附） |
| `2 × fps`（60 / 120 帧） | 关键帧 0、2.000 s | 关键帧 0、2.000 s | 周期性 IDR，间隔正好 2 秒 |

数字来自 `recorder_smoke 6 out.mp4 --gop 0 --fps 30|60` + `ffprobe -v error -select_streams v:0
-show_entries frame=key_frame -of csv=p=0`（第 0 行是 0.000 s）。因此**产品的"自动"落地成固定 2 秒**
（`encode::AUTO_KEYFRAME_SECS = 2` → `2 × fps` 帧）：既是用户熟悉的默认值，又保证周期性 IDR。
单测 `video::encode::tests::auto_keyframe_policy_keeps_periodic_idr` 把这张表固化下来
（0 → `[0]`；120 → `[0, 120]`；并断言 `intra_period_auto(cfg, 30) = 60`、`(cfg, 60) = 120`）。

**连带影响的因果关系（留在文档，不再上界面）**：关键帧间隔同时决定 ① 崩溃时最多丢失的时长
（分片 MP4 只在关键帧处落盘，见 §11.8）与 ② 剪辑吸附精度（起点吸附到后一个关键帧、终点到前一个）。
固定 2 秒 ⇒ 崩溃最多丢约 2 秒、吸附粒度约 2 秒；目标帧率越高，同样秒数对应的**帧数**越多
（30 fps → 60 帧 / 60 fps → 120 帧）。回归脚本第 3 步因此改成**先 ffprobe 读出真实关键帧位置再选点**，
不再硬编码 `[1.2s, 3.4s]`（那依赖"1 秒 GOP"这种只存在于 `--gop 1` 覆盖下的巧合）；
实测关键帧 `0, 1.023, 2.013, 3.201, 4.191` → 选点 `[0.512s, 3.696s]` → 吸附成 `1023..3201 ms`。

**其它"外部不显示、内部固定"的参数**（实测见 §10.5）：RC = ABR、复杂度 = 平衡、最大码率 = 20 Mbps、
线程 = auto、熵编码 = CABAC、音频码率 = **192 kbps**（`config::FIXED_AUDIO_BITRATE_KBPS`，
`sanitized()` 把任何输入归一为 192，单测覆盖 `0/32/64/192/320/512/9999 → 192`）。
192 kbps 对屏幕录制（人声/系统声）足够透明又不至于让文件无谓变大；注意**静音环回时实测码率只有
~2.3 kbps**（AAC 对静音几乎不产生字节，实测音频包合计 1401 B / 4.927 s），所以"码率"要看配置值，
不能拿静音录像的实测值去比。

**停止录制 → 导出普通 MP4，Cache 里的流文件保留（不删除）**：`mux_thread` 在 `remux_to_plain` 成功后**什么都不删** ——
导出产物落到**用户设置的输出目录**，与流文件**同名同基名**（分处两个目录），重名时追加 `-1`/`-2`…（`recorder::export_path_for`，纯函数 + 单测：
同名直接用；已存在→`-1`→`-2`；导出路径必须 ≠ 流文件路径），Cache 里那条流文件**原样保留**，作为"导出失败/忘了另存"时的找回手段。
`RecordingResult { streaming, exported, .. }` 把两条路径**同时**如实告诉界面：导出成功 → 结果区优先显示导出产物 + 说明临时副本在 Cache；
导出失败 → 琥珀提示"导出失败，已保留分片文件（可直接播放）" + 显示 Cache 里那条。
（**曾经的**"导出成功后删除流文件 / 送回收站 / 7 天自动清理"三条规则**全部取消**，见 §11.9.3；`should_remove_streaming` 与 `streaming_removed` 也一并删掉了。）
崩溃路径（`--crash`、进程被 abort、从未导出）**永远保留**流文件 —— 回归脚本第 1 步断言
"导出目录里有普通 MP4（非空、h264+aac 双流）**且 Cache 里流文件仍在**"，第 2 步断言"Cache 里流文件在 + 导出目录里没有同名 MP4"，两种情况分开写。

**视频 pts 吸附到帧网格**：真实采集时刻（单调时钟）带抖动，相邻两帧可能落进容器时间基的同一个格子，
ffmpeg 复用时报 `Application provided invalid, non monotonically increasing dts to muxer`。
`recorder::video_pts_on_grid()` 把真实时刻**吸附到 `1000/fps` 网格**（半步四舍五入）再做"严格递增"兜底：
既消掉抖动（回归脚本第 2 步的"解码干净"因此重新成立），又保持"视频时长 = 墙钟时长"
（掉帧只表现为画面略卡，不会快放）。

**界面文案分级（info/warn/error）**：目录**自动回退**属于预期行为，用中性 info 横幅
（可关闭、同类只显示一次、不堆叠）；只有"一个能写的目录都没有 / 配置读不出来 / 保存失败"才是红色 error，
且**不自动消失**。录制页**不再有"打开所在文件夹"按钮**（用户要求删掉，`open_folder` 辅助函数与
`btn_open_folder` key 一并删除），输出目录只保留路径文本 + "选择保存位置"。

### 10.4 度量陷阱记录（避免重复踩坑）
- 用 `-r 30` 把编码后的**裸 H.264** 喂给 `psnr`/`ssim` 滤镜会因时间戳/帧率对不齐而给出**完全错误**的数值（18.9 dB vs 真实 46.1 dB）。正确做法：**先解码成 yuv420p 裸流，再与参考裸流 raw→raw 比对**。
- `encode_demo` 的 `--threads` 默认是 **1**，且不带 slice 设置 → 直接用它测速度会严重低估（24 fps vs 129 fps）。
- PowerShell 里对含空格的路径做 `.Replace(' ','')` 会把 `Limit li` 也改掉 → 路径失效（我踩过）。

---

## 11. 实现进度与踩坑（随开发更新）

### 11.1 已完成并通过验证
| 环节 | 状态 | 验证方式 |
|---|---|---|
| 环境（Rust 1.99 + HTTP crates 代理 + MSVC + FFmpeg n9.0 LGPL） | ✅ | cargo build/test 全通 |
| 原生 OpenH264 模块并入产品树 | ✅ | 21+ 项单测（ABI 编译期断言内含）+ 实测编码 |
| libyuv BGRA→I420（零拷贝裁剪） | ✅ | 4 项单测，断言值来自 MSVC **C 探针**（红 Y=82/U=90/V=239、黑 16/128/128、白 235/128/128） |
| 鼠标指针合成（GDI + 自写 alpha 混合） | ✅ | 4 项单测 + **录制产物目视确认指针存在** |
| DXGI 采集（区域换算/会话重建/固定帧率） | ✅ | 6 项单测（含真机显示器枚举）+ 实测 180 帧 0 掉帧 |
| FFmpeg 分片 MP4 写入 + 重封装（copy） | ✅ | 见 §11.2 端到端结果 |
| 视频闭环烟雾测试 `src/bin/record_smoke.rs` | ✅ | ffprobe 独立校验：180 帧、关键帧 0/60/120、全解码 exit 0 |

### 11.2 视频闭环实测结果（1920×1080@30fps，录 6 秒）
```
采集：180 新帧，0 无变化，0 会话重建      编码：camera_rt + FixedSlcNumSlice + auto 线程
关键帧：0 / 60 / 120（GOP 60）            码率：1474 kbps
分片流文件：1082 KB，录制中可播（每秒 flush）
重封装：1082 KB 普通 MP4，16.2 ms（180 packets 原样 copy，+faststart）
时长：分片 5999 ms / 普通 5999 ms（期望 6000 ms）
ffprobe：两文件均 180 帧、Constrained Baseline 1920x1080 30/1、全解码 exit 0
```

### 11.3 本轮踩坑（写 FFmpeg mux 必看）
1. **`avformat_write_header` 会改写流的 `time_base`**。我按毫秒（1/1000）写 pts，结果 6 秒录像只报 **406 ms**（时间轴被压缩）。必须用 `av_rescale_q(pts_ms, {1,1000}, (*stream).time_base)` 在**写头之后**按实际 `time_base` 换算。
2. **MP4 的 extradata 必须是 avcC**（`AVCDecoderConfigurationRecord`），不是裸 SPS/PPS；且必须用 `av_malloc` 分配并留 padding（FFmpeg 会用 `av_freep` 释放）。
3. **OpenH264 的 `uiSliceNum` 有上限**：1080p 显式设 20 会被夹到 16（有 warning），设 68 直接 `cmInitParaError` 初始化失败；`RasterSlice` 模式在 1080p 上也会因此失败。用 `slice_num = 0`（auto）最稳。
4. **`ffmpeg-next` 的 feature 名不是 `avcodec`**，而是 `codec`/`format`/`software-resampling`/`software-scaling`（分别对应 sys crate 的 avcodec/avformat/swresample/swscale）。
5. **本机无可用硬件编码器**（nvenc/qsv/amf/d3d12va/vulkan 全部失败），软件编码只有 openh264 与 h264_mf。
6. `av_interleaved_write_frame` **接管 packet 所有权**（内部 unref），不要重复释放；写入失败路径也要注意 packet 已由它处理。

### 11.4 下一步
音频（wasapi 环回+麦克风 → 混音 → FFmpeg AAC-LC）、录制会话编排（四条线程 + 有界队列 + 丢帧统计）、主页面/设置界面、剪辑窗口（关键帧吸附 copy 导出）、端到端回归脚本。

### 11.5 音频链路实测结果（本轮完成）
| 项 | 实测 |
|---|---|
| 设备 | 渲染 1 个（Realtek 扬声器，默认）、采集 1 个（Realtek 麦克风，默认） |
| 系统声音环回 | 播 0.6 幅度 1 kHz 正弦 → 抓到 RMS **0.4248**（理论 0.6/√2=0.4243）；**FFT 主频 999.0 Hz**（分辨率 2.93 Hz，即精确命中）；144000 帧/3.00s = **48001 Hz**，0 超时 |
| 麦克风 | 144000 帧/3.00s = **48001 Hz**，环境底噪 RMS 0.00068 |
| 混音 | 系统 0.0200 + 麦克风 0.00068 → 混音 0.0232 ≈ √(a²+b²)，正确 |
| AAC-LC | 1 秒 48 kHz 正弦 → **48 帧 / 24342 字节**；AudioSpecificConfig = `11 90 38 E5 00`（**AAC-LC / 48 kHz / 立体声**）|
| 画面+声音 | 2 个流（h264 1920×1080 + aac 48 kHz 2ch）；视频 **4999 ms** vs 音频 **5014 ms**；全解码 exit 0 |
| 仅声音 | 1 个 aac 流，4010 ms（录 4 秒），全解码 exit 0 |
| 重封装（copy） | 188 packets 3.7 ms（纯音频）/ 293 packets 188 ms（音视频），均 `+faststart` |

### 11.6 音频踩坑（写 WASAPI 采集必看）
1. **WASAPI 环回在系统静音时不产生数据包**。实测：不播任何声音录 4 秒只拿到 **2.67 秒**音频（而播放声音时是满速 48029 Hz）。→ **音频时间轴不能依赖环回推进**，必须按墙上时钟补静音；否则音轨比视频短（8.4 节修复后：静音场景音轨 4010 ms vs 视频 3999 ms ✓）。
2. **事件只用于避免忙等，缓冲区才是真相**。早期版本 `wait_for_event` 未触发就直接返回、不排空缓冲，导致环形缓冲被覆盖，又丢了约 1/3 音频（3306 ms vs 4999 ms）。现在无论事件是否触发都尝试排空。
3. **AAC 编码器有 priming 延迟**：首帧 `pts` 常为负（-1024），且输出帧数比"输入样本/1024"多一帧。录制时**用自己的帧计数当时间戳**（从 0 开始），避免负时间戳进 MP4。
4. **wasapi 需要先初始化 COM，但 `RPC_E_CHANGED_MODE`（0x80010106）不是错误**。设备枚举/打开都要 COM，已封装成 `ensure_com()`；它把 `S_OK`/`S_FALSE`/`RPC_E_CHANGED_MODE` 都当成功——eframe/winit 的主线程是 **STA**，在那里调 `CoInitializeEx(MTA)` 必然拿到 0x80010106。早期版本把它当错误，直接导致"仅声音模式"启动录制失败（回归测试 `audio::devices::tests::open_audio_capture_succeeds_on_sta_thread` 复现）。
5. `WaveFormat::new` 的 `channels` 参数是 **usize**（不是 u16）；环回要"打开渲染设备 + `initialize_client(Direction::Capture)`"，crate 会自动加 `AUDCLNT_STREAMFLAGS_LOOPBACK`。
6. `autoconvert: true` + 显式请求 32bit float/48 kHz/2ch 可让 WASAPI 自己完成格式与采样率转换 —— **省掉全部手写重采样代码**，实测采样率精确（48001 Hz）。
7. **音频打不开时必须让用户看见**：`Recorder::start()` 对音频失败历来是"警告后继续"，于是"画面+声音"模式会悄悄录成只有画面的文件。现在打开失败的原文存在 `Recorder::audio_warnings()` 里，主页面用"本次录制没有声音：原因"明确显示。

### 11.7 录制会话编排（四线程）实测
正式路径在 `src/recorder.rs`：视频线程（采集+编码）、音频线程（采集+补静音+混音+AAC）、封装线程（按 pts 归并写入）、停止时 remux。
```
cargo run --bin recorder_smoke -- 6 out.mp4        # 正常录制 → 停止导出普通 MP4
cargo run --bin recorder_smoke -- 6 out.mp4 --crash # 到点 abort，验证分片文件仍可播
```
单次 6 秒实测：视频 **178 帧 / 丢帧 0 / 关键帧 3**，音频 **276 帧**；视频 5933 ms vs 音频 5951 ms（**差 18 ms**）；停止时 457 packets 重封装成普通 MP4（1233696 字节）；录制中句柄非阻塞（每秒可读状态与统计）。

线程安全上补了两处必须的 `unsafe impl Send`：
* `Encoder`（openh264）：内含 `ISVCEncoder*` 裸指针，改为 `Arc<OpenH264Lib>` 并声明"实例只被一个线程拥有"。
* `AudioCapture`（WASAPI）：内含 `*mut c_void`，MTA 对象可跨线程移交，且音频线程进入时会再次 `ensure_com()`。

### 11.8 崩溃可恢复性：丢失窗口实测（可证明）
分片 MP4 的完整分片**只在关键帧处生成**，生成后还要等一次 `avio_flush` 才落盘。因此：
* 刷盘间隔 `FLUSH_INTERVAL = 250ms`（早期 1 秒 → 实测丢 2 秒）。
* **结论：崩溃最多丢失一个 GOP**。录 6 秒后 `abort()`（退出码 -1073740791）：

| GOP | 存活时长 | 丢失 | 视频帧 | 全解码 |
|---|---|---|---|---|
| 1 s | 5013 ms | 0.99 s | 150 | exit 0 |
| 2 s | 4010 ms | 1.99 s | 120 | exit 0 |

崩溃后正确**没有**生成导出产物（没机会收尾），但 `crash.mp4` 仍能被 ffprobe 解析并完整解码。关键帧间隔同时决定"崩溃最多丢多久"与"剪辑吸附精度"，已在设置里暴露给用户。

> ⚠️ **本节正文里的 `.plain.mp4` 措辞是旧约定**：`§11.9` 起，导出产物与流文件**同名同基名**（都叫 `rec-NNN.mp4`，只是分处两个目录），产品不再产生 `*.plain.mp4`。本节表格里"GOP / 存活时长 / 丢失 / 视频帧 / 全解码"的实测结论**仍然成立**（它只与关键帧间隔和刷盘间隔有关）。

---

## 11.9 目录与文件生命周期：Cache（**不删、不统计、只在关闭时提醒一次**）

### 11.9.1 两个落点、同名同基名

| 产物 | 落点 | 形态 |
|---|---|---|
| **流文件**（录制过程中持续落盘） | **`<exe 目录>\Cache\`**（`cache::CACHE_DIR_NAME = "Cache"`） | 分片 MP4（`frag_keyframe+empty_moov`），**崩溃也能播** |
| **导出产物**（停止时 remux 出来） | **用户设置的输出目录**（`cfg.output_dir`） | 普通 MP4（`+faststart`，packet 级 copy、不重编码） |

* **两边同名同基名**：`rec-YYYYmmdd-HHMMSS.mp4`（`main_page::record_paths`，时间戳取 **UTC** —— 只求"不重名 + 能排序"，本机 eframe 侧没开 windows 的 Time feature 拿不到本地时区）。
* **`.plain` 后缀约定已取消**：旧版导出叫 `<名>.plain.mp4`，现在导出就叫 `<名>.mp4`。原因是"两边本来就在不同目录里，同名更直观"，用户一眼能对上"哪条流对应哪个导出"（`recorder::export_path_for` 的文档注释）。**产品代码已不再生成任何 `*.plain.mp4`**；只是 `ui::editor::newest_recording` 仍**兼容**读取遗留的 `*.plain.mp4`（见 §11.12）。
* **导出目录重名不覆盖**：`export_path_for()` 逐个试探，同名就追加 `-1`、`-2`…（上限 10000 次，仍全占用则退回第一个）。
  单测 `recorder::tests::export_goes_to_the_output_dir_and_never_overwrites`：`rec-20261012-060115.mp4` → 已存在 → `…-1.mp4` → 再存在 → `…-2.mp4`，并断言导出路径 ≠ 流文件路径。为什么必须避让：用户可能**在同一秒内录两次**。

### 11.9.2 Cache 目录本身不可写时往哪退

`cache::resolve_cache_dir(exe_dir, config_dir, probe)` 的**回退链**（`probe` 注入的是 `config::probe_dir_writable`，即"真建目录 + 真写一个探测文件 + 真删掉"）：

1. **`<exe 目录>\Cache`**（首选：便携、用户可见、与程序同处一地）；
2. **`<配置目录>\Cache`**（配置目录本身已经是启动时探测过的可用位置）；
3. **`%TEMP%\CapLite\Cache`**（`std::env::temp_dir().join("CapLite").join(CACHE_DIR_NAME)`）。

单测 `cache::tests::cache_dir_prefers_exe_dir_then_config_dir` 用假探测函数固定这条顺序。
启动时 `main.rs` 会把结论打进日志：`[cap] 临时文件目录(Cache): …`（**只进日志，不上界面**）。

### 11.9.3 **不自动删除、不统计占用空间**（这是最终决定，不是没做）

界面上**没有**任何"清理缓存 / 缓存占用 XX MB"的字样，代码里也**没有**任何删除流文件的路径。演变过程（如实记录）：

| 版本 | 做法 | 为什么被否 |
|---|---|---|
| ① "7 天自动清理" | 按文件时间删掉 7 天前的流文件 | 用户要**自己掌控**数据：机器上放了多久不该由软件替他决定 |
| ② "停止导出成功后删除流文件" / "送回收站" | 导出成功即删；更早还想过 `Recycle` | 导出成功 ≠ 用户已经把产物收好（还可能想对比、想找回）；送回收站仍然是一次"软件擅自动数据" |
| ③ **最终：删都不删**：只保留 + **关闭时提醒一次** | 三者都不做 | 流文件是"导出失败/忘了另存"时的**唯一找回手段**；用户明确要求"程序不做任何删除、也不统计占用空间" |

* 现在 `Recorder::start` 之后**流文件永不删除**，`RecordingResult { streaming, exported, .. }` 同时把两条路径如实交给界面：
  导出成功 → 结果区优先显示导出产物 + 说明临时副本在 Cache；导出失败 → 琥珀提示 + 显示 Cache 里那条（可直接播放/剪辑）。
* 连"占用多少空间"也不统计：`cache::cache_has_files()` **只看第一个目录项就返回**（`read_dir().next().is_some()`），不遍历、不求和。注释里写明了理由：用户最终版要求不统计占用空间。
* ✅ **口径残留已清理**（本轮）：`recorder.rs` 里"界面在底部栏提示 Cache 占用了多少空间"的注释已改成现状（不统计、只在关闭时提醒），`scripts/regression.ps1` 第 4 步的"分片已按设计删除"提示已改成"Cache 流文件同名同基名且保留（不删除）"，漏参的 `Add-SummaryRow` 已补全参数（不再报 `Cannot bind argument to parameter 'LiteralPath'`）。**实际行为以本节为准**。

### 11.9.4 关闭软件时提醒一次

实现（`main.rs` 的 `App::ui` 开头）：

| 环节 | 做法 |
|---|---|
| 触发条件 | `close_requested && !close_confirmed` → 取 `cache::cache_has_files(&self.cache_dir)` → `cache::should_warn_cache(nonempty, self.cache_warned)` |
| 拦截关闭 | `egui::ViewportCommand::CancelClose`（**不使用** `rfd` 同步弹窗：`rfd` 会阻塞渲染线程，Windows 上还会卡住消息循环） |
| 提醒形态 | 模态化的 `egui::Window::new(theme::APP_NAME)`，`collapsible(false)` / `resizable(false)` / 居中锚定，**一个**「知道了」按钮（`theme::primary_button`，宽 120） |
| 文案 | `close_cache_hint` = "{} —— 请定期删除 Cache 内缓存，以免占用太多空间。"（`{}` = `theme::APP_NAME`，并在下面单独一行用等宽字体列出 Cache **完整路径**）；按钮 = `btn_got_it`"知道了" |
| 放行 | 点「知道了」→ `show_cache_close_dialog = false`、`close_confirmed = true`、**再发一次** `ViewportCommand::Close`（这一次不再拦截） |
| 只提醒一次 | `cache_warned: bool` 是**内存标志**，不写配置文件（用户没要求持久化，只要同一次运行里别反复弹） |

纯函数与单测（`src/cache.rs`）：

* `cache::tests::cache_has_files_only_checks_presence`：空目录 / 不存在的目录 → `false`（不报错）；有一个文件 → `true`。
* `cache::tests::should_warn_cache_only_when_nonempty_and_not_yet_warned`：`(true,false)→提醒`、`(false,false)→直接关`、`(true,true)→放行`、`(false,true)→放行`。

**取舍**：判"有没有文件"只花一次 `read_dir().next()`，所以关闭路径上**不会**出现"关窗口时卡一下"（这是"不遍历求和"换来的直接好处）。

### 11.9.5 崩溃可恢复语义不变（保留 Cache 的主要价值）

进程被强杀（`recorder_smoke --crash` 走 `std::process::abort()`）后：

* **`<exe>\Cache` 里的分片文件仍可直接播放**（ffprobe 可解析 + `ffmpeg -f null -` 全解码干净）；
* 崩溃时**不会**在导出目录留下同名文件（没机会收尾）；
* 本轮回归实测（2026-10-12）：第 2 步 `PASS`，退出码 `-1073740791`；崩溃前每秒落盘进度打印为 `1 KB → 210 KB → 395 KB → 616 KB`（`FLUSH_INTERVAL = 250 ms`）。
  这正是"流文件必须留在 Cache 里、不许自动删"的**根本理由**。

---

## 11.10 区域框选：三态交互 + 「确定」按钮

### 11.10.1 状态机（`ui::main_page::PickState`）

| 状态 | 何时 | 画面处理 |
|---|---|---|
| **Idle** | 还没选区（`anchor == None`） | **整屏原亮度、完全不压暗**（`dim_alpha_for(None) = 0`） |
| **Dragging** | 按下左键之后（`dragging == true`） | **把"选中的矩形"本身画成半透明深色**（`SELECT_DIM_ALPHA = 96`，≈ **38% 黑**），**框外保持正常亮度**；2px 白边框 + 1px 蓝描边 |
| **Adjusting** | **松开之后不返回**（`anchor.is_some() && !dragging`） | 同上填充，**额外画出 8 个手柄**（10×10，四角 + 四边中点）提示"这里能拖" |
| **确认** | 点「确定」 | 换算成物理像素矩形 → `PickerEnd::Picked(region)` → 写回 `cfg.region` 并关闭遮罩窗口 |

> 与旧设计（§0 #4）的差别：旧版是"**只把选区之外压暗 50%**"，现在是"**只把选区之内压暗 ≈38%**"——用户要求"鼠标开始选取时，把选取的部分变深"。Idle 时**一个压暗像素都不画**。

### 11.10.2 交互与控件

* **8 个手柄**缩放（`resize_by_handle`）、**框内拖动**整体移动（`move_selection`，两者都夹紧在屏幕内）；
* **框外按下 = 重新点选**（丢弃旧矩形，从新锚点开始）；
* **`Esc` / 右键 = 取消**（`PickerEnd::Cancelled`，**保留原选区**）；右上角还有一行"取消"提示（`pick_cancel_hint`）；
* **「确定」按钮**（`CONFIRM_SIZE = 72×30`，用户要求高度 28–32）：
  * 位置 = **选区正下方、水平居中**（`confirm_button_rect`，先试 `sel.max.y + gap`，再把 x 夹进屏幕）；
  * **下方空间不够**（贴着屏幕底边）→ 移到**选区内侧底部居中**（`sel.max.y - 高 - gap`，且不小于 `area.min.y`）；
  * 选区**小于 8px**（`CONFIRM_MIN_SIDE`）不显示按钮（避免误点一个 1×1 的选区）；
* **尺寸信息 `宽 × 高`**（`pick_size` = "选区 {} × {}"）画在**选区正中央**，带一层半透明黑底衬（`rgba(0,0,0,150)`，圆角 5），下面 22px 处再补一行左上角物理坐标；
* 十字辅助线 + 交点小方框：先把指针换算成**物理像素**再换算回来，所以线就落在最终会写进 `cfg.region` 的那条边界上（`{px}, {py}` 标注）；
* 命中优先级（`picker_hit`，纯函数 + 单测 `picker_hit_priority_is_confirm_then_handle_then_inside_then_outside`）：**确定 → 手柄 → 矩形内部（移动）→ 矩形外部（重选）**。

### 11.10.3 踩坑：「点确定无反应」的两个根因（务必保留）

用户报的 bug：框好区域后点「确定」**没反应、遮罩不返回**。根因有**两条**，缺一不可：

1. **按钮以前只是 `painter` 画出来的、没注册成 egui 交互控件** → 点击被整屏的 `Sense::click_and_drag()` 吃掉（事件先被整屏 response 收走）。
   **修法**：`ui.interact(btn, ui.id().with("pick-confirm"), egui::Sense::click())` 注册一个**真控件**；并且**指针在按钮上时（`pointer_on_confirm`）跳过全部拖拽/选区处理**（按下不记锚点、拖动不更新当前点）。
2. **确认判断挂在 `drag_started_by` / `dragged_by` 上** → **egui 只在指针移动之后才触发 drag 事件**，所以"按下 → 松开、中间不移动"这种**标准点击永远不会成立**（调试帧证据：`down_on=true drag_started=false`）。
   **修法**：改用 `ui.input(|i| i.pointer.primary_pressed() / primary_released())` —— **按下那一刻**就定锚点/记标记，松开时按 `press_on_confirm && pointer_on_confirm` **成对判定**（按下与松开都落在按钮里才算点中）。

**修前失败的证据 + 回归测试**：`ui::main_page::tests::headless_click_on_confirm_finishes_the_picker`
（headless egui，完整走一遍事件序列：`PointerMoved(A)` → 按下 A → 移到 B → 松开 → 移到按钮 → 按下 → 松开）。断言三件事：① 松开 B 时**必须**返回 `PickerEnd::Keep`（"松开不能立刻返回"）；② `confirm_button_rect` 的结果**水平居中于选区且 `btn.min.y > sel.max.y`**，且 `picker_hit(btn.center(), …)` 必须认出 `Confirm`；③ 点确认后返回 `PickerEnd::Picked`，且 `region` 就是选区换算值（800×600 显示器 1:1 映射下 `(100,80)` + `400×320`）、`pk.finished == true`。
该测试的注释里写明了**怎么复现旧 bug**：把「确定」改回"纯绘制 + 靠 `cursor` 判断松开点"的老逻辑后，这一条会**失败在 ③**（返回 `Keep`、`finished == false`）。

### 11.10.4 截图能力不足（如实说明）

本机**合成鼠标事件无法稳定送进独立遮罩窗口**（遮罩是 `with_monitor(rank)` 的独立 viewport），所以"活动选区的实时截图"抓不到。这块交互**由 headless 测试覆盖**（`picker_frame` 用 `ctx.run_ui(...)` + 手工注入 `Event::PointerMoved` / `Event::PointerButton`，`.drop_without_applying_deltas()`），而不是靠截图。

---

## 11.11 录制页：显示器与录制范围

### 11.11.1 显示器下拉在**两种模式下都可用**（修掉"区域模式锁死显示器"）

* 用户报的 bug：区域模式下显示器被锁死，必须先切到全屏模式才能选屏。现在下拉框在**任何模式下都在、都能改**（只有一个显示器时里面就一项）。
* **区域模式下切换显示器** → 旧选区按新屏坐标系**搬移**（按两台显示器原点的 `dx/dy` 平移）再**夹紧**（`clamp_region_to_monitor`，顺带宽高取偶）；**装不下就清空**（`width = 0; height = 0` → 界面显示"未选择"）。
  → 底部栏给一条 **amber** 提示 `region_reselect_hint` = "已切换显示器，请点「重新框选」重新选择区域"。
  纯函数 `region_after_monitor_switch` + 单测 `monitor_switch_moves_or_clears_the_region`（同尺寸屏搬得动；800×600 小屏装不下 → `None`）。
* **框选始终开在最新选中的显示器上**：`Picker.rank` 取自当前 `cfg.monitor_index`，`ViewportBuilder::with_monitor(rank)`。
* 框选写回时还会**按"选区落在哪块屏"重新对齐 `monitor_index`**（`apply_picked_region`）：遮罩可能被系统放到别的屏幕上，对齐后才能保证 `region` 与 `monitor_index` 指的是同一块屏（否则录制会报"选区在显示器之外"）。

### 11.11.2 「刷新」按钮已删，原位改成**信息文字**

* **没有「刷新」按钮**了（历史上有一个）。显示器那一行右侧现在是一段**只读的次要色小字**（`region_info_text`，纯函数 + 单测 `region_info_text_covers_full_region_and_unset`）：

| 模式 | 显示 | 实测（单测固化） |
|---|---|---|
| 全屏 | `全屏录制：{所选显示器分辨率}` | `全屏录制：1920x1080`；显示器未知时 `全屏录制：0x0`（不留空洞） |
| 区域 + 有效选区 | `区域录制：{宽}x{高}`（**宽高取偶**） | `region = 801×601` → `区域录制：800x600` |
| 区域 + 还没框选 | `区域录制：未选择` | `0×0` 与 `8×8`（小于 `MIN_REGION_SIDE = 16`）都算"未选择" |

* **显示器列表自动刷新**（结果缓存，**绝不每帧枚举**）：
  * 进入录制页枚举一次（`monitors_loaded`，首帧 `refresh_monitors()`）；
  * 之后**约每 5 秒**重新枚举一次（`MONITOR_REFRESH = Duration::from_secs(5)`）——拔插显示器最多 5 秒后反映出来；
  * 下拉里点中一项 → 立刻**再枚举一次**（保证列表与选择一致）；
  * ✅ **已修复：打开下拉时真的重枚举**。判据是 `ComboBox::show_ui` 的**内层闭包被调用**（egui 只在展开时执行它）→ 置 `combo_open = true`，闭包外立刻 `refresh_monitors()`；**阈值沿用 `MONITOR_REFRESH = 5s`**（`combo_open && last_monitor_refresh.elapsed() >= MONITOR_REFRESH` 才枚举）。所以：拔插显示器后**打开下拉就能看到最新列表**（不必等满 5 秒），而"展开着不动"或"每帧都要画"都不会反复枚举（仍然受 5 秒节流）。
    * 为什么把枚举放在闭包**外**：闭包里还借着 `self.monitors` 做迭代，闭包内不能再可变借用 `self`；因此闭包只登记标记。
    * **失败处理**：`refresh_monitors()` 枚举失败时**保留上一次的列表**（`self.monitors = before`），只记 warning（`monitors_err` → 页面那行琥珀小字 + 一条 `tracing::warn!`），**不清空列表、不 panic**。
  * ⚠️ **未实测项（如实记录）**：本会话**无法**为它做端到端验证 —— 没有真实多显示器热插拔事件，也没有可用的 GUI 会话。
    * 曾尝试无窗口驱动 egui 来数枚举次数：本机 egui 0.36 的 headless 环境里，合成的指针点击**能**让 `ComboBox` 的 `Response::clicked()` 为真，但**下拉的展开状态始终没能被观察到**（`ComboBox::is_open` 恒为 false，`open_popup` 等内存 API 在 0.36 是 `pub(crate)`，外部无法直接置位），所以这条计数实验**没有结论，已按要求改写下面第 1 项验证**，临时代码也已删除（单测数保持 117，与基线一致）。
    * **替代验证（已做）**：`refresh_monitors()` 里加了一条 `tracing::debug!("枚举显示器（进页面 / ≥5s 周期 / 打开下拉 / 点选后）")`。在**真实界面**上用 `RUST_LOG=cap=debug` 跑一次、反复点开/关上显示器下拉，即可直接观察频率：打开下拉**只多一条**、连续帧不再增加、5 秒内再次打开也不会多（"不是每帧枚举"由此可证）。

### 11.11.3 启动**强制全屏录制**（产品决策）

`main.rs` 的 `App::new` 第 ⑤ 步：`cfg.capture_mode = CaptureMode::FullScreen`（**每次启动都覆盖**）。
理由（写在代码注释里）：区域框选是一次性的定位动作，用户下次打开十有八九只是想整屏录。
**区域坐标仍完整保留在配置里**（`cfg.region` 不动）→ 切到"区域录制"时可直接复用上次的框。

### 11.11.4 状态卡片：就绪 + 大号时间码在左，小按钮右对齐

* 卡片第一行：`● 就绪`（`state_idle`，状态点 + 状态文字）+ **大号等宽时间码**（`theme::mono_big`，无录制时显示 `00:00`）+ **右对齐**的「开始录制 / 停止录制」按钮。
* 按钮尺寸 **132×34**（`theme::START_BTN_W/H`，圆角 6、字号 13.5）；**录制中换成危险色「停止录制」时位置与尺寸完全不变**（不会跳动）—— 这一点是用户明确提的"按钮不要跳"。
  **本轮"压低这一行"也没有动它**（见 §11.27）。
* 录制中卡片下方追加统计网格（`stats_grid`），并 `request_repaint_after(200 ms)` 保持刷新。
* 旧版的"页面底部固定操作条"（`Panel::bottom("cap_record_bar")`）**已删除**，因此卡片下方不会再有那条空框。
* **那一行的高度由行内最高的部件决定**：实测（§11.27）改前是**时间码**（34 号字号 → 行高 40），
  改后（29 号）是 **34 = 按钮高度**，两者齐平。状态文字是 `theme::FS_STATUS`（14）。

---

## 11.12 剪辑页：当前形态与预览口径

### 11.12.1 版式顺序（自上而下）

| # | 内容 | 在哪里 |
|---|---|---|
| ① | `打开视频` / `重新载入` + **同一行右侧** `起点`（绿）/ `终点`（红）可拖可输的时间码 + `选区时长` | `ScrollArea` |
| ② | **四个彩色按钮一行**：`设入点`（绿实心）/ `设出点`（红实心）/ `添加片段`（蓝实心）/ `删除片段`（橙实心）+ `片段 {cur}/{n}`（未选点时前两个置灰，悬停说明"请先在时间线上点击选点"） | `ScrollArea` |
| ③ | **片段芯片行**（有片段才出现）：`片段 {n}`，点击选中该段、**按住左右拖动改合成顺序** | `ScrollArea` |
| ④ | **时间线**（三色标尺 + 关键帧刻度 + 整体缩略图条） | `ScrollArea` |
| ⑤ | **当前帧预览（流式，见 §11.12.6）+ 右侧一列三件：时间码 / `播放`（绿）/ `导出`（蓝）**（整列与预览垂直居中；窄窗口整列换行到预览下方、顺序不变；见 §11.12.8） | `ScrollArea` 的最后一屏 |
| ⑥ | **文件信息**一行 | `Panel::bottom("cap_editor_fileinfo")` |
| ⑦ | **常驻操作指南**一行 | `Panel::bottom("cap_editor_help")`（最下） |

* ⑥⑦ 是**两个底部面板**，声明顺序 = 版面顺序：help 先占最下、文件信息紧接着占它**正上方** —— 两行**构造上紧邻**（实测两行之间缝隙 0px），而且都**先占掉高度**，所以 `ScrollArea` 拿到的 `available_height()` 已经是"扣掉这两行之后"的剩余空间，**预览因此自动充满**（见 §11.12.6）。
* **P36 精简（2026-10-18）**：② 那一行原本是「`当前标尺 {时间码}` + 设为起点/终点 + 吸附提示」，
  ③ 原本是「`适应全片` + 平移滑条 + 波形开关」。用户要求"恢复原来的单行布局、只做小改动"后，
  这些**全部删掉**：当前帧信息时间线上本来就有、吸附规则固定不需要每帧念一遍、
  适应全片与"缩放回铺满比例"重复、波形改常开。省下来的高度换成了 ③ 的片段芯片行（见 §11.34），
  **最小窗口反而从 687 降到 671**。
* ⑥ 的一行摘要：文件名 / 时长 / `宽×高` / `{fps} fps` / 视频编码 / 音频编码+采样率+声道 / `关键帧 N`（同一事实只出现一次）。窗口过窄时**单行裁掉右端**（`ui.horizontal` 不换行），与 ⑦ 的 `truncate` 同一个取舍：**宁可省略，不撑高**。
* ⑦ 常驻一行用 `Panel::bottom("cap_editor_help")` 钉在剪辑页底部（在 App 的底部状态栏**之上**），文案是 i18n key **`editor_help`**：
  > 拖拽时间线或滚轮缩放 ｜ 点击时间线放置当前标尺（蓝色）｜ [ 设为起点 ｜ ] 设为终点 ｜ ←/→ 微调（Shift 按关键帧、Ctrl 10ms）｜ Home/End 到片头片尾 ｜ 预览显示当前标尺（蓝色）所在的关键帧
* **「打开最近录制」按钮已删；自动载入也一并删了（P46）**：剪辑页**只接受显式打开** —— 录制页底部的「在剪辑页打开」、本页的「打开视频」/「重新载入」。改造前有两条自动载入（构造时按 `cfg.last_video_path`、`ui()` 里"clip 为空就载入录制目录最新一条"），见 §11.46。没有素材时页面显示 `no_video_hint`（"点上面的「打开视频」，或在录制页点「在剪辑页打开」"）。
* 导出完成后的**汇总卡片挪到预览之前**（时间线与预览之间）：预览吃掉了"时间线以下"的全部剩余高度，卡片放在它后面会被推到底部面板之下、必须滚动才看得见。

### 11.12.2 三色标尺

| 标尺 | 字段 | 颜色 | 怎么移动 |
|---|---|---|---|
| 起点 | `sel_start_ms` | **绿**（`OK_FG`） | 拖绿色把手 / 在 ② 的时间码里输 |
| 终点 | `sel_end_ms` | **红**（`DANGER`） | 拖红色把手 / 在 ② 的时间码里输 |
| **当前标尺** | `cursor_ms`（初值 = 时长的一半） | **蓝**（`theme::CURSOR = #1D4ED8`） | **只由"点击时间线"或"拖动它的把手"移动** |

选中的有效区间用半透明色高亮；关键帧刻度是金色（`KEYFRAME`），过密时按 `KEYFRAME_MIN_GAP_PX` 抽稀（单测 `keyframe_ticks_are_thinned`）。
`cursor_committed`：`cursor_ms` 的初值"整片中间"是**中性占位**，不是用户选的点 —— 所以在用户第一次点击时间线之前，② 显示"未选点"、`设为起点/终点` 置灰，键盘的 `[` / `]` 也不生效。**这条路径上没有任何 hover 回退**（单测 `headless_mark_buttons_read_the_committed_ruler_not_the_last_hover`）。

### 11.12.3 预览口径（最终版）：**只看蓝色当前标尺**

`EditorPage::preview_ms()`：**预览 = `cursor_ms` 所在位置最近的关键帧**；没有关键帧时**退回标尺位置本身**（`edit::nearest_keyframe(clip, cursor).unwrap_or(cursor)`），保证**预览始终有值、不闪空**。
单测：`preview_ms_needs_a_clip_and_falls_back_without_keyframes`、`headless_preview_shows_the_keyframe_at_the_blue_ruler`（含"点击时间线改变 `cursor_ms` 后预览随之变化"与"悬停不改 `cursor_ms`/预览"的断言）。

**演变与原因（一句话）**：先做过**悬停即当前位置 → 预览跟随悬停**，又改成**拖动预览**，最终定为**只看蓝色标尺** —— 因为"悬停即当前位置"时，用户从时间线移向按钮的过程中位置一直在变，**"去点按钮时位置已经移动了，定位不到自己想要的位置"**（`editor.rs` 文件头注释原文）。

**既成的拖动语义**：在**轨道空白处拖动 = 拉着蓝色标尺走**（`resp.drag_started()` 那条路径会写 `cursor_ms`），**预览随之变化** —— 这是**特意保留**的：它与"点击时间线"同属"用户主动定位标尺"。真正**不影响预览**的是**起点/终点把手与其它控件**（悬停、去点按钮都不会移动蓝色标尺）。

### 11.12.4 底部常驻操作指南 + 删掉悬停弹层

* **为什么从"悬停弹层"改成常驻一行**：悬停弹层的**渲染发虚（半透明叠加）且会与时间线/预览重叠**；改成 `Panel::bottom` 后它**先占掉这块高度**，内容区自动收窄，**不随内容滚动、不覆盖时间线与预览**；窗口过窄时**单行省略（`truncate`）而不是换行撑高**。
* 配合的取舍：**剪辑页全部内容放进 `ScrollArea::vertical()`** —— 内容再长（罕见的小窗口 / 导出汇总卡片出现）也只滚动内容，**帮助行永远可见**。
  **本轮更新（见 §11.23）**：滚动条设成 `ScrollBarVisibility::AlwaysHidden` + `auto_shrink([false, false])`，
  并且删掉了预览块之后那句多余的 `add_space(GAP_CARD)` —— 它会让内容**永远**比视口高 12px、
  从而**一直**挂着一条滚动条。现在"窗口够高 ⇒ 内容高度恰等于视口高度 ⇒ 不滚动"，
  且窗口不允许小于实测出来的 `min_inner_size`（760×625）。
* 高度口径（本轮收紧）：**上下一共只留 2px 内边距**（`theme::compact_plain_frame()` = 左右 8 / 上下 2），**不再有上下各 4px 的 `add_space`**；字号一点没变小（`FS_SMALL = 12`），文字上下各留 2px，不会被裁。

### 11.12.5 默认载入的是**导出产物**，不是 Cache 里的流文件

* 交接路径：录制停止 → `main.rs::open_in_editor(path, streaming_fallback)`，`path` **优先取导出产物**；只有导出失败（`exported` 为空或文件不存在）才回退到 Cache 里的流文件，并用 `warn_editor_streaming_fallback` 明确提示。
* 剪辑页自己找"最近录制"时（`newest_recording`）：**不递归**、只认目录里的普通文件、扩展名 ASCII 不区分大小写、候选之间**修改时间最新者胜**。
* ⚠️ **兼容层的口径**：`newest_recording` 仍实现"`X.plain.mp4` 优先于 `X.mp4`"（存在同名 `.plain.mp4` 就丢掉 `X.mp4`；时间相同时 `.plain.mp4` 优先）—— 它是给**遗留产物**用的；产品路径下两边**同名同基名但分处两个目录**，剪辑页搜索的是**导出目录**，所以拿到的本来就是导出产物。
  相关单测名保留了旧词汇：`editor_opens_the_exported_file_not_the_streaming_one`、`newest_recording_prefers_the_exported_plain_file`、`newest_recording_only_looks_at_direct_mp4_files`、`set_output_dir_invalidates_recent_cache`。
* ✅ **口径残留已清理**（本轮）：`editor.rs` 文件头/字段注释与 `main_page.rs` 的 `MainOutcome` 注释里原先写的"**黄色**当前标尺"已全部改成"**蓝色**"（`theme::CURSOR = #1D4ED8`），原先把 `<name>.plain.mp4` 当作**现行**命名的措辞已改成"现行=导出产物与流文件**同名同基名**、分处输出目录与 Cache；`.plain.mp4` 只是**遗留兼容**"（兼容代码与保留旧词汇的单测名**不动**）。
* 仍然存在（**不属于本轮范围**）：剪辑页 ①~⑤ 的部分静态文案是**硬编码中文**（如 `打开视频`、`重新载入`、`起点`、`终点`、`设为起点`、`设为终点`、`适应全片`、`平移`、`导出`），并未全部走 i18n。

### 11.12.6 预览是**流式**的（以高度为准、下限 200、保持源比例、充满剩余空间）

**口径（唯一实现在 `editor.rs::fluid_preview_size`，纯函数、有单测）**：

1. **以高度为准**：`preview_h = 可用高度`，`preview_w = preview_h × 源宽/源高`。画框比例**恒等于源比例**，所以既不拉伸、也不裁剪、更不留黑边。源尺寸未知（没有视频流 / 宽高为 0）时按 **16:9** 兜底（`PREVIEW_FALLBACK_ASPECT`）。
2. **高度下限 200px**（`PREVIEW_MIN_H`）：可用高度不足时取 200，**绝不把画面压成一条**。
   这条下限就是"程序最小窗口高度"的锚点：`min_inner_size` 的高度 = 外壳 126 + 剪辑页
   必需高度 499（其中就含这 200，见 §11.23）。窗口不允许缩到那个尺寸以下，
   所以**下限永远不需要靠滚动条来兜底**。
3. **宽度受限时按宽度收敛**：`preview_w > 可用宽度` 时先试 `preview_w = 可用宽度`、`preview_h = 可用宽度 × 源高/源宽`；**只有收敛后的高度仍 ≥ 200 才接受**，否则**保持"以高度为准"的结果**（宽度超出可用宽度，横向裁切/滚动）。两条路径都严格保持比例。
4. 尺寸取整到整数像素（避免半像素发虚）；取整是单调的，所以"窗口更高 → 预览更高"成立。

**可用高度怎么算**：

```
可用高度 = ui.available_height() − 预览下方固定内容
预览下方固定内容 = 预览自己那行时间码（+4px 间距） [+ 按钮换行到下方时：9px + 按钮 44px]
```

* `ui.available_height()` 是**滚动区里"时间线以下"的剩余高度**，它**已经**不含文件信息行与操作指南行 —— 那两行是两个 `Panel::bottom`，在滚动区**之前**就把高度占掉了。所以这里**不再**扣它们（再扣一次反而会凭空在两行之间留出空白）。
* 按钮换行到下方的情形：可用宽度 < 240 + 20 + 200 时（`PREVIEW_SIDE_BTN_MIN_W`），导出按钮换到预览**下方**，此时必须再给按钮留出 9 + 44px，否则按钮会被挤出可视区。
  **本轮补充（§11.23.5）**：还要按**算出来的预览宽度**再判一次 —— 极宽画幅（21:9 / 32:9）
  保高度下限时 `preview_w = 200 × 源比例` 可能超过可用宽度（规则 3），
  右侧那一列放不下就必须换行，否则按钮会落到窗口右边界之外。

**"充满"是怎么保证的**：预览那一整行（预览画框 + 它下面的时间码）的矩形是**自己算好**的（`allocate_exact_size(page_w, preview_h + caption_h)` + `new_child`，位置全部用矩形给定），所以
`预览块底部 == 文件信息行顶部`（实测：1040×740 时 627 == 627，1600×990 时 877 == 877）。**不依赖 egui 的子 Ui 布局继承** —— 早期版本用 `ui.horizontal` + `ui.allocate_ui` 时，`add_space(垂直居中补白)` 会**横向**推按钮，预览一变高按钮就被推出窗口（本轮修掉的就是这个坑）。

**实测数字**（debug 版 `_rust\target-cap\debug\CapLite.exe`，素材 `reg1.mp4` 1920×1080 = 16:9）：

| 窗口 | 预览 | 比例 | 导出按钮 | 按钮-预览间距 / 垂直居中误差 | ⑥⑦ 两行 |
|---|---|---|---|---|---|
| 1040×740 | **490×276** @ (165,333) | 1.7754（16:9 误差 0.13%） | 200×44 | 22px / **0.0px** | 24px + 30px，缝隙 **0px** |
| 1600×990 | **935×526** @ (222,333) | 1.7776（误差 0.01%） | 200×44 | 22px / **0.0px** | 24px + 30px，缝隙 **0px** |
| 旧版（同 1040×740） | 320×180（固定宽） | 1.7778 | 200×44 | — | 51px + ~39px，缝隙 ~22px |

截图证据：`_rust/ui-shots/after-preview-fluid-small.png`（1040×740）、`_rust/ui-shots/after-preview-fluid-large.png`（1600×990）。

**解码尺寸跟着预览变**（`thumb_bucket_w` / `decode_size`）：

* 缓存 key 从"时间点"变成 **`(时间点, 解码尺寸档)`**；档位 = 目标宽度按 **64px 向上取整**（`THUMB_BUCKET_STEP`）后夹到 `[320, 1920]`，并**不超过源宽**。
* 于是**拖动窗口 1px 不会跨档 → 缓存命中 → 不重新解码**（单测 `headless_one_pixel_resize_keeps_the_cached_thumb`、`thumb_bucket_quantizes_the_decode_size`）；真的跨档时才按新尺寸解一张，且"同一时刻只允许一张在途"，连续拖动只解**最近**那一张。
* 精确档位还没解好时，先用**同一关键帧的其它档位**顶着显示（换窗口大小时不闪"解码中…"）。

**预览口径不变**（§11.12.3）：仍然只显示**蓝色当前标尺**所在位置最近的关键帧，不跟悬停、不跟拖动。

新增单测：`headless_fluid_preview_follows_window_height`、`headless_preview_respects_the_200px_floor`、`headless_preview_converges_on_width_when_needed`、`fluid_preview_size_rules`、`preview_avail_h_subtracts_the_fixed_rows`、`thumb_bucket_quantizes_the_decode_size`、`headless_one_pixel_resize_keeps_the_cached_thumb`、`headless_file_info_and_help_are_adjacent_below_the_preview`、`headless_export_button_stays_visible_beside_the_preview`、`headless_fluid_preview_still_shows_the_ruler_keyframe`。

### 11.12.7 ⑥⑦ 两行压到"刚好一行文字"

| 行 | 形态 | 旧高度 | 新高度 |
|---|---|---|---|
| ⑥ 文件信息 | `Panel::bottom("cap_editor_fileinfo")` + `theme::compact_card_frame()`（白底 + 发丝描边 + 圆角 8，**内边距左右 8 / 上下 2**） | 51px（`theme::card` 的 14px 内边距 + `item_spacing`） | **24px** |
| ⑦ 操作指南 | `Panel::bottom("cap_editor_help")` + `theme::compact_plain_frame()`（无底色，**内边距左右 8 / 上下 2**） | ~39px（上下各 4px 的 `add_space` + 默认面板内边距） | **30px**（= 2 + 文字的整行高 25 + 2，已是该字号的极限） |

* ⑥ 里额外把 **`item_spacing.y = 0`** 与 **`interact_size.y = 0`** 也收掉：egui 的 `ui.horizontal` 默认把行高撑到 `spacing.interact_size.y`（主题里 30），而这一行只有静态文字 —— 不收紧的话行高会被抬到 30px，白白从预览身上抠掉 ~6px。
* 字号**一点没变小**（⑥ 用 `FS_BODY`/等宽 `FS_BODY`，⑦ 用 `FS_SMALL`），文字上下各留 2px、不裁切；⑦ 仍然 `truncate()` 单行省略，⑥ 用不换行的 `ui.horizontal`（窄窗口裁掉右端，不撑高）。
* 省下来的高度**全部还给预览**：同 1040×740 窗口下预览从 469×264（收紧前实测）涨到 490×276。

### 11.12.8 播放（P28）：时间码 / 播放 / 导出**三件一列**

| 项 | 形态 |
|---|---|
| 时间码 | `fmt_time(cursor_clamped())`，蓝色等宽，在播放按钮**正上方**（`caption_h` 那一行） |
| 播放 | 绿色实心（`theme::OK_SOLID`，hover `OK_SOLID_HOVER`）；图标用 `Painter` **画矢量**：三角 = 播放、方块 = 停止 |
| 导出 | 蓝色（`theme::PRIMARY`），高 `theme::H_PRIMARY` |

* 三件**左对齐同一列**、整列（含间距）与预览**垂直居中**：
  `col_h = code_h + gap + btn_h + gap + btn_h`，`col_top = (frame.center().y - col_h/2).max(frame.top())`。
* 窄窗口（`btn_below`）整列换行到预览**下方**：`fixed_below += caption_h + gap + col_h`。
  因为只是**复用**预览右侧那一列的位置，`ui::layout::EDITOR_REQ_H` **仍是 557**（新增两个控件没有新增高度）。
* **图标不能放进 locale**：字体子集里没有 `▶`(U+25B6) / `■`(U+25A0)，会变豆腐块；
  `font-subset` 的覆盖测试也会直接失败。所以 `theme::play_button_in()` 用 `Shape::convex_polygon`
  与 `rect_filled` 画 10×11 px 的图标，再与文字整体居中。
* **控件 id 必须固定**（`ui.id().with("cap_editor_play_btn")`）：剪辑页前两帧布局才落定，
  若 id 由矩形位置推导，按下与松开之间矩形挪几像素 ⇒ **两个不同的控件 id** ⇒ 点击被丢弃
  （症状：`hovered == true` 但 `clicked() == false`，按钮"点不动"）。
* 空格：`text_edit_focused()` 时**让给输入框**；否则 `consume_key(NONE, Space)`（**消费掉**，
  避免同一次按键又触发某个聚焦按钮）。
* 翻页：`page_view_start(cursor_ms, vs, visible_ms, max_view)` 是**纯函数**，只有
  `cursor >= vs + visible_ms` 才翻，新 `vs = cursor`（夹到 `[0, max_view]`）。
  **P30**：当前帧落到视口**左边**时同样翻到它（缩放不再把播放头拉回视野，见 §11.12.9）。
* **点时间线 / 拖蓝色把手 / Home-End-方向键都不停播**（P28 第二轮口径）：
  一律 `reanchor_play()` —— 时钟重锚 + 清帧队列 + `PLAY_SEEK_MIN_MS = 120` 的 seek 节流。
  **P30 四次修订**：缩放与位置行不再是定位动作，因此**不** `reanchor_play()`、也不改
  `cursor_ms`；它们改为**直接停播**（`stop_play()`，标尺停在停播那一刻）。
* 解码侧见 `src/edit/player.rs`（常驻线程 + `sync_channel(8)` + `LEAD_MS = 200` 墙钟节流 +
  落后就丢帧 + EOF 后阻塞 `recv()`）。离开剪辑页 → `main.rs` 调 `editor.stop_playback()`。

### 11.12.9 当前帧策略（P30）：只有"点影片"能动标尺

用户口径（2026-10-17 三次修订，**覆盖** P14/P17 的"点/拖位置行 = 定位当前帧 + 居中跟随"）：

> 「改变当前帧策略，只有当在影片上点击鼠标时，才能改变当前帧的位置，缩放影片、拖动滚动条，
> 都不要改变当前帧的位置，当前帧取消保持在显示区域的中间位置。」

四次修订（同日，紧接上一条）：

> 「1 现在拖动滚动条的逻辑是反的，浅蓝色进度条是在往鼠标拖动的反方向移动。
> 2 还是改为：当缩放影片及拖动滚动条时，停止播放。」

| 操作 | 改 `cursor_ms` | 改视口 | 播放 | 说明 |
|---|---|---|---|---|
| 点影片（胶片条 / 波形区 / 时间码行） | **是** | 否 | 不停播（跳过去继续播） | 唯一入口（`resp.clicked()`）；拖蓝色把手 = 按住影片，同样算 |
| 滚轮缩放 | 否 | 是（锚点 = 鼠标） | **停播** | `zoom_anchored` 只写 `px_per_ms` / `view_start_ms` |
| 拖位置行 | 否 | 是（相对位移，与指针**同向**） | **停播** | `drag_delta.x → 时刻`，视口跟着指针走 |
| 点位置行 | 否 | 是（以点击处为中心） | **停播** | `pos_row_t_of` → `centered_view_start` |
| 键盘 Home/End/←/→ | 是 | 否 | 不停播（重锚） | 明确的定位动作，保留 |

* **拖动的方向**：`d = +drag_delta.x / pos_w × duration`（**正号**）。`vs` 变大 ⇒ 淡蓝色带在
  位置行上右移 ⇒ "拖着色带走"。符号写反过一次（色带朝反方向跑），单测因此断言的是
  **拖动中途**的位移，而不是松手后的终值（松手那一帧还有"点一下 = 跳到点击处"的语义）。
* **松手不跳**：`pos_resp.clicked() && !pos_resp.drag_stopped()` —— 一次真拖动在松手那一帧
  也可能被 egui 记成 click，不挡掉就会"一松手跳成以松手处为中心"。
* **播放中"动视口 = 停播"**：缩放 / 点拖位置行都调 `stop_play()`（不改 `cursor_ms`）。
  整片铺满（`max_view == 0`）时位置行没有可滚余量，碰它不停播。

* **取消居中跟随**：`centered_view_start` 只剩一个调用点（点位置行），
  语义从"把当前帧居中"变成"把点击处居中"。点影片**不动视口** —— 标尺到点处，画面不动。
* **删掉"缩放后把当前帧拉回视野"**：那条钳制（`centered_view_start(cur, …)`）正是
  "缩放会挪当前帧"的观感来源。删掉之后，播放头仍可能落到视口**左侧**
  （`Home` / `←` 这类键盘定位，或缩放前就已经被甩出去），所以
  `page_view_start` 增加**左边界分支**：当前帧在视口左侧时同样翻到它（落在左边界，**不是居中**），
  否则影片条会整屏冻住到播放头自己追上右边界。
* **`fit_mode` 不被滚动破坏**：`max_view == 0`（全片都在眼前）时拖/点位置行都是空操作，
  也不退出铺满模式（否则窗口改宽时不再自动铺满）。
* 位置行**仍是全片映射**（`pos_row_x_of`）：蓝条 = 全片，淡色带 = 当前可见区间；
  拖动用**相对位移**而不是"指针位置 → 时刻"，避免"视口一动、同一指针位置算出另一个时刻"的自激回路。


---

## 11.13 设置页与外壳

### 11.13.1 设置页最终只剩三项

| 项 | 形态 | 取值 |
|---|---|---|
| **界面语言** | 下拉（`lang_label`） | `i18n.available()` 的 id 列表 |
| **目标码率** | 滑尺（无十进制值显示）+ 同排数值框 | **5–20 Mbps**（`VIDEO_BITRATE_MIN_KBPS..=VIDEO_BITRATE_MAX_KBPS`，存储是 kbps），**默认 5**；滑尺 `step_by(100)`；数值框显示 `{:.1} Mbps`，手输 `7.5` 按 Mbps、`7500` 按 kbps |
| **帧率** | 滑尺两档 + 同排数值框 | **30 / 60**，**默认 30**；`Slider::new(&mut cfg.fps, 30..=60).step_by(30.0)` 让拖动/点击/方向键**只可能停在 30 与 60** |

两行用同一个 `Grid`（左列固定宽 104 的标签、右列同宽同高的滑尺 + 数值框），所以两行**左边缘严格对齐**。

**被移除的整块内容与理由**（去冗余）：

| 被移除 | 理由 |
|---|---|
| **音频整块** | 音源（系统声音 / 麦克风）与麦克风设备都在**录制页**选，那才是实际生效的地方；音频码率内部固定 192 kbps |
| **存储位置** | 录制页已经有"保存位置"的选择与可用性提示，设置页再来一份属于重复 |
| **编码速度 / 编码方式 / 熵编码 / 线程数** | 全部内部固定（复杂度 = 平衡、RC = ABR、熵编码 = CABAC、线程 = auto，见 §10.5）；`Config` 里保留字段只为旧配置还能反序列化，`sanitized()` 一律归一 |
| **关键帧间隔** | 语义固定为"自动"（`0`，落地成 2 秒，见 §10.7） |
| **恢复默认 / 保存按钮** | **调整即保存**，没有"保存"这个动作，也就不需要"已保存"提示 |
| **页面标题「设置」/「改动会立即保存」/ 只读的「当前配置目录」** | 与标签栏重复；配置目录已在底部状态栏常驻显示 |

### 11.13.2 调整即保存 + 录制中置灰

* **调整即保存**：任何改动 → `cfg = cfg.clone().sanitized()` → `store.save(cfg)`（**临时文件 + rename** 的原子写法）。保存**失败**仍然是可见错误，但不再画在页面里 —— 通过 `SettingsOutcome::error` 交给外壳的**底部状态栏红条**（`err_save_failed`），全程序只有一条消息通道。
* **录制进行中整页控件置灰**并提示 **`recording_locked`** = "录制中不可调整，停止后可修改"：
  * 设置页：`ui.add_enabled_ui(!locked, …)` 一条指令禁用**全部**子控件（新加控件也不会漏）；
  * 录制页：状态卡片**之外**的三个卡片（采集/内容/输出）包在 `add_enabled_ui(!recording, …)` 里 —— 停止按钮在状态卡片里，所以**始终可用**。
* **内核在启动时快照本次会话参数**（所以"改了不生效"是设计而不是 bug）：`Recorder::start(cfg, output)` 在返回前就把参数定死并建好采集与编码器 ——
  * `effective_fps(cfg.fps)` 的结果（`MAX_RECORD_FPS = 60` 再夹一次）在启动时算出，中途改 `cfg.fps` 不影响本次；
  * **导出目录在启动时快照**：`let export_dir = resolve_against_exe_dir(&cfg.output_dir)`（注释原文："录制中途改配置不影响本次"）；
  * 视频编码器由 `VideoEncoder::new(cfg, …)` 在启动时一次性建好。
  * ✅ **注释已修正为与实现一致**（原先写的是"快照好了（`SessionParams`）"，但代码里**没有** `SessionParams` 这个类型，也从来没有过；已按事实改写）：参数快照就是上面那几样 —— `Recorder::start` 内的**局部变量**（`effective_fps(...)` 的结果、启动时解析好的 `export_dir`）加上**已经建好的编码器/封装器**（它们持有参数值），此外没有别的快照载体。
* 单测（覆盖"参数归一 / 不会被非法值带进录制线程"）：`config::tests::video_bitrate_is_clamped_to_five_twenty_mbps`、`config::tests::fps_is_snapped_to_the_two_supported_choices`、`config::tests::audio_bitrate_and_keyframe_interval_are_normalized`、`config::tests::sanitize_clamps_and_fixes`、`ui::settings::tests::sanitize_is_applied_before_save_pipeline`。

### 11.13.3 标签顺序与应用名

* 标签顺序 **录制 | 剪辑 | 设置**（`Tab::Home / Tab::Editor / Tab::Settings`，设置放最后），**默认进录制页**（`tab: Tab::Home`）。标签做成"胶囊"：选中项 `ACCENT_SOFT` 浅底 + `ACCENT` 文字，尺寸 76×30。
* **应用名单一来源**：`theme::APP_NAME = "CapLite"` —— 窗口标题（`ViewportBuilder::with_title`）与关闭提醒窗口标题都用它；**内容区不重复显示应用名**（三个页面都不再有内容区标题）。
  注意（代码注释已写明）：**包名/二进制名仍是 `CapLite` / `CapLite.exe`**（`Cargo.toml`、脚本、目录都不动）。

### 11.13.4 底部信息栏

`Panel::bottom("cap_status_bar")`（**先占高度**、不是悬浮层，所以永远不会和内容重叠）：

| 位置 | 内容 |
|---|---|
| 左 | 状态点 + 状态文字（`state_idle`="就绪" / `state_recording`="录制中" + `MM:SS` 或 `HH:MM:SS` / `state_stopping`="正在停止…" / `state_finalizing`="正在导出普通 MP4…" / `rec_finished`="录制完成" / `state_failed`="录制失败" + `：{原因}`）+ **实际生效的输出目录**（标签 `output_dir_label`="输出目录"，只读、等宽、中段省略、悬停看全文） |
| 右 | **消息区**，一次显示**一条** |

消息区规则（`main.rs::pick_notice`，纯函数）：

* 按严重度 **Error > Warn > Info**，同级取**最新**那条；排队中的其它消息在右边显示 `+N`；
* **error**：红底 + 加粗 + **不自动消失**（必须手动点「知道了」）；**warn/info**：各自的浅底 + 可「知道了」；
* **栏高固定**：单行 + `truncate()` 省略，不会因为来了消息把内容顶得跳动；
* 「知道了」= `btn_got_it`，记进 `dismissed` 列表 → **同类不再出现**（本会话持久）；
* 录制页/设置页报上来的错误与警告都走**这一条**通道（`page_error` / `page_warning`），不再各画一套横幅。

**目录自动回退提示的口径**（易误读，写清）：

* **启动时**输出目录的自动回退（`plan_output_dir` 选中了别的目录）**全程不在界面出现**，只打日志：`[cap] 输出目录不可写（X），已自动改用 Y`；界面只如实显示"实际生效的目录"。
* 但**录制页「保存位置」那一行还有一道独立的探测**（按目录值缓存，不是每帧）：用户**显式选了一个不可写目录**时会出现两条真提示 —— `warn_fallback_to`="原输出目录不可写，已自动改用：" + 实际目录（琥珀）或"没有任何可写的输出目录，请手动选择一个有写入权限的文件夹"的详细错误（`err_no_output_dir`）。
* 真错误（红条、不自动消失）只有三类：**一个能写的配置目录都没有**（`err_config_memory_only`，纯内存运行）、**用户显式指定的配置目录不可写**（`err_config_dir_explicit_fallback`）、**配置加载失败**（`err_load_failed`）。

---

## 11.14 开发签名与信任（详见 `docs/DEV-SIGNING.md`）

本节只留**结论与引用**，全文（含手工清标记、撤销证书、常见问题表）见同目录的 [DEV-SIGNING.md](DEV-SIGNING.md)，这里不重复粘贴。

* **两个弹框、两种原因**：①「无法验证发布者。你确定要运行此软件吗？」= 走 **ShellExecuteEx** 路径时的 **Attachment Manager** 安全警告（需要：走 Shell 启动 + 文件带 **Mark-of-the-Web**（`Zone.Identifier` 备用数据流）+ 没有受信任的发布者签名）；② 定义上属于 **Microsoft Defender SmartScreen** 的"已保护你的电脑"（带 MotW 的**低信誉**文件，新编译的未签名文件信誉≈0）。**两者是不同的弹框**，修一个不等于修另一个。
* **两条脚本**：
  * [`scripts/dev-sign.ps1`](../scripts/dev-sign.ps1)：自签证书（`CN=CapLite dev signing`，复用已有）→ 公钥装 `Cert:\CurrentUser\TrustedPublisher`（必要时**再补装 `Root`** 并复测，打印 `TRUST-PROBE` 结论）→ **清 `Zone.Identifier`**（文件 + 各级父目录，清完复查）→ `Set-AuthenticodeSignature -HashAlgorithm SHA256` → **回读 `Status`**；幂等，退出码 `0/1/2/3/4` 各有明确含义。
  * [`scripts/run-dev.ps1`](../scripts/run-dev.ps1)：构建 → 签名 → **启动不走 ShellExecute**（`ProcessStartInfo(UseShellExecute = $false)`，即直接 CreateProcess）。加 `-UseShellExecute` 才回到 `Start-Process` 老路（对比排障用）。
* **实测数据（说明弹框只长在 ShellExecute 路径上）**：同样"未签名 + 带 MotW"的文件，`Start-Process`（ShellExecute）**调用返回 9415 ms**，而 `UseShellExecute=$false` **仅 114 ms**；真实的未签名 `CapLite.exe`（无 MotW）也是 `Start-Process` 2637 ms vs 直接启动 **19 ms**。那几秒就是 Shell 在做区域/附件/签名评估 —— 模态框就在这段评估里等用户点击。完整的 7 行对照表在 DEV-SIGNING.md §2.3。
* **沙箱限制（必须如实记录）**：在**受限写入**的沙箱里**无法写用户证书库** —— 实测报 `CertEnroll::CX509Enrollment::_CreateRequest: 拒绝访问。0x80090010 (NTE_PERM)` 与 `certutil: -addstore 失败: 0x80070005 (ERROR_ACCESS_DENIED)`。因此沙箱里能证明的是"**签名确实写进了 exe**（回读签名者 = `CN=CapLite dev signing`、`SignatureType=Authenticode`）+ **MotW 检测与清除确实有效** + **启动方式差异确实存在**"；**`Status=Valid` 必须在能写当前用户证书存储的普通 PowerShell 里跑一次才能得到**（沙箱里回读只会是 `UnknownError`，`StatusMessage` 明确写"证书链在不受信任的根证书中终止"，即**信任锚缺失，不是签名没写上**）。

---

## 11.15 变更记录（近几轮）

按时间顺序（越靠下越新）：

| # | 项 | 曾经的做法 | 最终做法 / 现在的状态 | 为什么 |
|---|---|---|---|---|
| 1 | 流文件清理 | 计划 **7 天自动清理** | 取消 | 用户要自己掌控数据，软件不替他决定放多久 |
| 2 | 流文件清理 | 改为"导出成功后**删除**流文件"（更早还提过**送回收站**） | **都不做**：永不删除 | 导出成功 ≠ 用户已收好产物；流文件是导出失败/忘另存时的唯一找回手段 |
| 3 | 缓存占用 | 底部栏提示"Cache 占用了多少空间" | **不统计占用空间**（`cache_has_files` 只看第一个条目） | 用户明确要求；顺带让关闭路径零开销 |
| 4 | 关闭提醒 | 无 / 每次关闭都提示 | **只在 Cache 非空时弹一次**（内存标志，不写配置） | 空 Cache 不打扰；同一次运行不反复弹 |
| 5 | 导出产物命名 | `<名>.plain.mp4`（与流文件区分后缀） | **与流文件同名同基名**（`rec-NNN.mp4`，分处两个目录）；重名追加 `-1`/`-2` | 用户要求去掉该约定；两边目录不同，同名更直观 |
| 6 | 框选压暗方向 | **只把选区之外压暗 50%** | **只把选区之内压暗 ≈38%**（`SELECT_DIM_ALPHA = 96`）；Idle 时完全不压暗 | 用户："鼠标开始选取时，把选取的部分变深" |
| 7 | 框选「确定」按钮 | 纯 `painter` 绘制 + 靠 `drag_started_by` 判断 | `ui.interact(...)` **真控件** + 指针在按钮上时跳过拖拽 + `primary_pressed/released` **成对判定** | 修掉"点确定无反应"的两个根因（§11.10.3） |
| 8 | 区域模式显示器 | 锁死，必须先切全屏才能选屏 | **两种模式都能选**；换屏时选区搬移/夹紧/装不下就清空 + amber 提示 | 修 bug；绝不出现"切了屏但框选还在旧屏" |
| 9 | 显示器「刷新」按钮 | 有一个手动刷新按钮 | **删除**，原位改成信息文字（全屏/区域/未选择三种文案） | 手动刷新是多余动作：列表已自动刷新（进页面 + 约 5 秒 + 点选时） |
| 10 | 启动采集模式 | 沿用上次保存的模式 | **每次启动强制全屏录制**（区域坐标仍保留在配置里可复用） | 产品决策：区域框选是一次性定位动作 |
| 11 | 状态卡片按钮 | 底部固定操作条 / 大按钮 | 卡片第一行**右对齐的小按钮 132×34**，录制中位置尺寸不变 | 用户要求"按钮放到时间码右侧、别跳" |
| 12 | 剪辑预览 | **悬停即当前位置**（预览跟随悬停） | 先改成**拖动预览**，最终 **只看蓝色当前标尺**（不跟随悬停、不跟随拖动；无关键帧时退回标尺位置） | 悬停时"移向按钮的过程中位置一直在变，定位不到想要的位置" |
| 13 | 剪辑操作指南 | **悬停弹层**（渲染发虚、与时间线/预览重叠） | `Panel::bottom` 的**常驻一行**（`editor_help`），内容整体放进 `ScrollArea` | 常驻不遮挡、不随内容滚动；窄窗口单行省略 |
| 14 | 帧率设置 | 下拉菜单 | **两档滑尺（30/60）**，`step_by(30)`；`sanitized()` 把 24/45/100000 吸附到最近档（平局取 30） | 中间值无真实收益，还让码率与关键帧间隔的整算变模糊 |
| 15 | 设置页栏目 | 语言 + 音频 + 存储位置 + 编码速度/方式/熵编码/线程 + 码率 + 关键帧间隔 + 恢复默认 + 保存 | **只剩 界面语言 + 目标码率滑尺（5–20 Mbps，默认 5）+ 帧率两档滑尺（30/60，默认 30）**；**调整即保存**，录制中整页置灰 | 去冗余：音源在录制页，其余参数内部固定 |
| 16 | 关键帧间隔 | 设置页可调 | **内部自动**（`0` → 落地成固定 2 秒的周期性 IDR） | 实测 `uiIntraPeriod = 0` 在 OpenH264 里只有首帧 IDR，不能直接用（§10.7） |
| 17 | 消息提示 | 顶部横幅 / 每页各画一套 | **底部状态栏单一通道**（Error > Warn > Info，一次一条，栏高固定，`+N` 排队） | 顶部横幅打扰；单一通道行为一致 |
| 18 | 目录自动回退提示 | 每次都提示 | **启动时只进日志**，界面只显示实际生效的目录 | 用户连续两次反馈"这条提示还在，太打扰" |
| 19 | 应用名 | 窗口标题与内容区各写一个 | **单一来源 `theme::APP_NAME = "CapLite"`**，内容区不重复显示 | 避免"窗口标题写一个、界面标题写另一个"；暂定名，改只动一处 |
| 20 | 标签顺序 | 录制/设置/剪辑 | **录制 \| 剪辑 \| 设置**，默认录制页 | 用户要求设置放最后 |
| 21 | 启动弹框治理 | 只处理签名（或只处理 MotW） | **签名 + 清 MotW + 不走 ShellExecute** 三件事一起做 | 两个弹框成因不同，缺一条就还会弹（§11.14） |
| 22 | 显示器下拉重枚举（**本轮小清理**） | 打开下拉时只重置 `last_monitor_refresh` 计时器，注释却写"顺手重枚举一次"（§11.11.2 记为"注释与实现不符"） | **真的枚举**：`show_ui` 内层闭包（只有展开才执行）置 `combo_open` → 闭包外按 `combo_open && elapsed >= MONITOR_REFRESH(5s)` 调 `refresh_monitors()`；枚举失败**保留旧列表** + warning，不清空、不 panic、不每帧枚举；`refresh_monitors()` 另加一条 `tracing::debug!`，便于在真实界面用 `RUST_LOG=cap=debug` 观察频率 | 让"打开下拉就能看到最新列表"这条真的成立 |
| 23 | 一致性清理（**本轮小清理**） | 多处注释/文案与实现不符 | `main_page.rs` 去掉不存在的类型名 `SessionParams`（按实现改写）；`editor.rs` 与 `main_page.rs` 里"黄色当前标尺"→**蓝色**（`theme::CURSOR = #1D4ED8`）；`editor.rs` 里把 `.plain.mp4` 当**现行**命名的措辞改成"现行=导出与流文件同名同基名分处两目录、`.plain.mp4` 只是**遗留兼容**"（兼容代码与旧测试名保留）；`recorder.rs` 去掉"界面在底部栏提示 Cache 占用多少空间"（现状=不统计、只在关闭时提醒） | 注释/文案要能当依据用；行为一律不动 |
| 24 | 回归脚本第 4 步（**本轮小清理**） | 打印"分片 reg1.mp4 已按设计在导出成功后删除"（旧语义），且 `Add-SummaryRow` 漏参报 `Cannot bind argument to parameter 'LiteralPath'` | 文案改成"Cache 流文件**同名同基名且保留（不删除）**"；四处调用补全参数（`$export1`/`$cache1`/`$cache2`/`$out3`），跑完**无任何绑定报错**；另：脚本补回 **UTF-8 BOM**（Windows PowerShell 5.1 按 ANSI 解析无 BOM 的中文脚本会直接解析失败） | 不改 8 步判定与 PASS 条件，只让输出与现状一致、无报错 |
| 25 | release 版的控制台窗口（**本轮**） | bin 默认是**控制台子系统**，release 双击 exe 会多一个黑窗（里面是 `[cap] 配置文件目录/输出目录/临时文件目录(Cache)` 那几行） | `src/main.rs` 顶部加 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`：**release = Windows 子系统（无控制台）**，**debug 保持控制台**；原来打 stderr 的启动行改为走新的 `StartupLog`：**release 落盘 `<配置目录>\cap.log`、debug 只打控制台**（§11.17） | GUI 程序不该带控制台；但不能因此丢掉诊断信息，也不能让开发期每跑一次就多一个文件 |
| 26 | 发布包里的 FFmpeg（**本轮收尾**） | 随包带 BtbN **全功能** LGPL 共享包（5 个 DLL 共 **114.43 MB**） | 换成**项目自建的最小配置** LGPLv3 共享版（5 个 DLL 共 **4.20 MB**）；configure 参数、工具链、7 个坑与许可合规见 **§11.19**；发布包解压后 131.10 MB → **20.89 MB**（ZIP 9.24 MB，因 `CapLite.exe` 占大头而基本持平） | 录屏只用到 `h264` 解码 + `aac` 编码 + `mp4` 封装 + `file` 协议，其余 96% 的代码从不执行却要用户下载 |
| 27 | `FFMPEG_DIR` 的指向（**本轮收尾**） | 交接文档建议把 `CapLite\ffmpeg-min` 放到 `FfmpegCandidates` **首位** | **不换**：`$FFMPEG_DIR` 同时供着同目录的 `ffprobe.exe`/`ffmpeg.exe`，而回归脚本要真调它们约 11 次（含 `-af volumedetect`）→ 换首位必挂（实测 exit `-1073741515` / 补 DLL 后 `-1073741511`）。改为新增独立变量 **`$env:FFMPEG_TOOLS_DIR`**，`regression.ps1` 优先用它（§11.19.5） | 构建用哪个 FFmpeg、交叉校验用哪个 CLI，本来就该是两件事；"一行改动"那条路的代价（12/12 PASS 变红）不值得付 |
| 28 | startup A/B 开关（**本轮发现并修**） | `build.rs` 读 `CAP_NO_DELAYLOAD=1` 来重建"改前"基线，但**没声明** `cargo:rerun-if-env-changed` | 补上该声明。原状态下设了开关重新 build，cargo 会复用上一次的构建脚本输出 → 开关**静默失效**（实测：设了 `CAP_NO_DELAYLOAD=1`，新 exe 仍带 `DelayImport size=160`） | 没有这行，A/B 基线的可信度为零；这只动构建配置，不动任何运行期行为 |
| 29 | 启动耗时对比脚本（**本轮新增**） | 交接文档里的 `opt-measure.ps1` 从未跑过 | 新增 `scripts/startup-compare.ps1`：外部 5 ms 轮询窗口 + `av*/sw*` 模块 + 解析 `cap.log` 打点，输出 4 个口径与冷热 min/中位/max，并落 JSON；实测数字见 §11.19.4 | 要让"启动变快了"变成**可复核的数字**，而不是感觉 |

---

## 11.16 本轮（文档补齐 + 小清理）的验证记录

| 命令 | 结果 |
|---|---|
| `cargo build --all-targets` | **成功，零编译警告**（`Finished dev profile`） |
| `cargo test`（`CARGO_TARGET_DIR=_rust\target-cap`） | **lib 117 passed / 0 failed**；`tests\e2e.rs` **7 passed / 0 failed**；doc-tests **1 passed**；各 `src/bin/*` 为 0 项 —— 与改造前基线**完全一致** |
| `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/regression.ps1` | **8 步全部 PASS，退出码 0**，输出里**没有任何** `Add-SummaryRow` / `Cannot bind argument to parameter 'LiteralPath'` 报错。关键实测数（**代码清理完成后的最终一轮**）：第 1 步墙钟 4.947 s / 编码 106 帧 / 关键帧 4 个 / 音频 228 帧 / **Cache 流文件保留=True**；第 2 步崩溃退出码 `-1073740791`、Cache 里流文件 `ffmpeg -f null -` 干净、导出目录无同名文件；第 3 步按实测关键帧 `0, 1.452, 2.838, 4.257` 选点 → 吸附 `1452 → 2838 ms`、产物 1408 ms、分辨率 1920x1080 一致、位流拷贝；第 4 步汇总 **4 个产物全部存在且可解析**（含 Cache 里保留的 `reg1.mp4`）；第 7 步 `mean_volume = -7.5 dBFS`（≈0.4217 线性） |
| 未验证项 | **显示器"打开下拉即重枚举"没有做端到端验证**：本会话既无法合成多显示器热插拔，也无法在 headless egui 里观察到下拉的展开状态（详见 §11.11.2 的如实记录）。其余改动（注释/文案、脚本参数与 BOM、文档同步）均被上面三条命令覆盖 |
| `Select-String -Path docs/DESIGN.md -Pattern '^## '` | 见交付报告（§12 不存在：为不改动既有编号、避免代码里 `§10.x`/`§11.x` 引用失配，新内容全部作为 **§11.9–§11.16** 追加） |

---

## 11.17 release 版不带控制台窗口 + 启动日志落盘（本轮）

### 11.17.1 现象与机制

**现象**：双击 release 版 `CapLite.exe`，除了主界面还会多一个控制台窗口，里面是

```
[cap] 配置文件目录: …
[cap] 输出目录: …
[cap] 临时文件目录(Cache): …
```

**机制**：不是"我们创建了控制台"，而是 Rust 的 bin 默认是 **控制台子系统**（PE 可选头 `Subsystem = 3 / IMAGE_SUBSYSTEM_WINDOWS_CUI`）。Windows 为控制台子系统进程**自动分配**一个控制台；从 Explorer（自己**没有**控制台）启动时就是**新开一个控制台窗口**。实测（本轮改造前，PE `Subsystem = 3`）：

| 目标 | PE Subsystem | 启动后新增的顶层窗口 |
|---|---|---|
| 改造前 `release\CapLite.exe` | **3（CUI）** | **`ConsoleWindowClass\|visible\|…\release\CapLite.exe`**（1 个）+ 主窗口 `Window Class\|CapLite` |
| `powershell.exe`（对照，CUI） | 3 | 1 个 `ConsoleWindowClass` |
| `notepad.exe`（对照，GUI） | 2 | **0 个** |

（测量方法：`ProcessStartInfo` + `UseShellExecute = $false` 直接 `CreateProcess`；探测进程先 `FreeConsole()`，复现"父进程没有控制台 = Explorer 启动"的条件，否则 CUI 子进程会**继承**父进程的控制台、看起来没有新窗口 —— 这是一个会**误判为已修好**的坑。）

### 11.17.2 修法

* `src/main.rs` **第一行**（在任何 `use`/`fn`/item 之前，与 crate 文档注释同为内部属性，顺序无所谓）：

  ```rust
  #![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
  ```

  只对 release 生效 → PE `Subsystem = 2 / IMAGE_SUBSYSTEM_WINDOWS_GUI`。
* **不丢诊断信息**：原来那 6 处 `eprintln!("[cap] …")`（配置目录 / 输出目录 / Cache 目录 / 配置加载失败 / 输出目录回退 / 中文字体缺失）改为走新增的 `StartupLog::line()`。

### 11.17.3 日志落盘策略（含取舍）

| 项 | 决定 | 为什么 |
|---|---|---|
| 落盘时机 | **只有 release 写文件**；debug **只打控制台，不落盘** | release 没有控制台可看，必须落盘；debug 有控制台，信息本来就看得见，开发期不想每跑一次就多一个文件 |
| 日志位置 | `<配置目录>\cap.log` —— **沿用 `ConfigStore` 的回退链结果**（`--config-dir` → 用户\Documents\cap → exe 同级\cap-recordings → 临时目录），那一步已经"真写一个探测文件"验证过可写 | 不把日志写死在 exe 同级：Program Files 下的 exe 目录通常**不可写**，写死了就等于没有日志 |
| 兜底 | 配置目录一个都写不了 → `%TEMP%\CapLite\cap.log`（与 `cache` 的临时兜底同一处） | 总比完全没有日志强；这一处**只有**在配置全链路失败时才会用到 |
| 不无限增长 | 启动时 `len > 1 MB` → 删掉重写（等价于截断） | 一行判断就够，不需要复杂轮转；日志只记"启动那几行 + panic"，1 MB 够存很多次运行 |
| 写失败 | **只 `eprintln!` 一条 warning，绝不 panic、绝不影响启动** | 磁盘满/目录只读/文件被占用都不该让程序起不来 |
| panic | `std::panic::set_hook`：先往同一个日志文件追加 `[cap] PANIC: …`，**再调用原 hook** | release 下 panic 默认只写 stderr = **静默消失、无从排查**；调用原 hook 保证 debug 的控制台输出与 `RUST_BACKTRACE` 行为**完全不变** |
| `run_native` 返回 `Err` | 同样往日志文件补一行 `[cap] 启动失败（…）: …` | 创建窗口/GL 上下文失败时默认也只写 stderr，release 下同样看不见 |
| 时间戳 | 自己算 UTC（`YYYY-MM-DD HH:MM:SSZ`，Howard Hinnant 的 civil_from_days） | 只为日志里一行时间不值得引入 chrono/time（**无新依赖**是硬要求） |

**"debug 保留控制台"的取舍（如实说明）**：

* **收益**：开发时 `[cap] …` 直接可见、panic 回溯直接打在终端里；不需要"先找日志文件再打开"。
* **代价**：**debug 与 release 的子系统不同，所以"窗口行为"这类问题只能在 release 上复现**（本轮验证也正是这么做的：release 断言无控制台，debug 断言**仍有**控制台）。发布前必须跑一次 release 版，不能只测 debug。
* 另一条备选是"debug 也落盘"（开发时把控制台和文件都写上）。放弃它的原因：开发期会不断产生/增长日志文件，而本轮的日志内容（启动那几行）在控制台上已经完整可见，收益为零。

### 11.17.4 本轮验证记录

| 命令 / 方法 | 结果 |
|---|---|
| `cargo build --all-targets` | **成功，零编译警告** |
| `cargo build --release --bin CapLite` | **成功**（PE `Subsystem` 由 3 变 **2**） |
| `cargo test` | **lib 118 passed**；`tests\e2e.rs` **7 passed**；doc-tests **1 passed** —— 与基线一致 |
| `scripts/regression.ps1` | **12 条全部 PASS，退出码 0**（回归跑的是 **debug** 产物，理应完全不受影响） |
| 窗口探测（release，先 `FreeConsole()` 复现 Explorer 条件） | 该进程顶层窗口 **10 个**：可见的只有主窗口 `Window Class\|visible\|CapLite` 与 winit 的辅助窗口 `Winit Thread Event Target\|visible\|`（标题为空），其余是 `NVOpenGLPbuffer`(×2)/`wgpu Device Class`/`IME`(×4)/`MSCTFIME UI` 等**隐藏**窗口；**全系统新增 `ConsoleWindowClass` = 0**；`Get-Process … MainWindowTitle` = **`CapLite`** |
| 窗口探测（debug，同法） | PE `Subsystem = 3`；**新增 1 个 `ConsoleWindowClass\|visible`**，`MainWindowTitle` = debug exe 的完整路径 —— **控制台如期保留**；debug 配置目录下**没有** `cap.log` |
| 日志内容 | `…\target-cap\release\cap-recordings\cap.log`：启动横幅 + 日志文件自身路径 + §11.17.1 的三行路径信息。**另一个实例还实测到了回退分支**：`[cap] 输出目录不可写（C:\Users\…\Videos），已自动改用 …\cap-recordings` —— 正是过去只打在 stderr、release 下会丢的那类信息 |
| 1 MB 截断 | 先把 `cap.log` 写成 **1 141 790 字节**（1100 行 `OLD-LINE-*`），启动一次 release 后变 **609 字节**，`OLD-LINE` 一条不剩 |
| panic 落盘 | 临时插入一段**由 `CAP_VERIFY_PANIC` 环境变量触发**的 `panic!`（验证后已删除并**重新构建**发布产物）：`cap.log` 里出现 `[cap] PANIC: panicked at src\main.rs:975:17:` + panic 文案，进程退出码 **101**；同一次运行 stderr 里仍有默认 hook 的输出（含 `stack backtrace`）→ 说明**默认行为没被顶掉**。发布用的 exe 里没有这个触发器（`Select-String CAP_VERIFY_PANIC` 空） |
| 发布包 | `dist\CapLite-0.1.0\`（8 个运行时文件 + 2 个许可证 + `THIRD-PARTY-NOTICES.txt` + `README.txt`）→ `dist\CapLite-0.1.0-win64.zip`，**59 327 221 字节**，SHA256 `A1E0488855C10ED39F4C827B470BEA34CA84A5EF0C349412B6F4B01CCC378040`；解压后 12 个文件逐字节（SHA256）与发布目录一致；`dist\CapLite.exe`（sha256 `D92C41D1…`）实测 `Subsystem = 2` + `Status=Valid`（`scripts\dev-sign.ps1`），**并对"从 ZIP 解出来的那份 exe"重跑了窗口断言**（`ConsoleWindowClass` 新增 0、`MainWindowTitle=CapLite`）。包内 5 个 FFmpeg DLL 与 `ffmpeg-n9.0-latest-win64-lgpl-shared-9.0\bin` 逐字节同源、`openh264.dll` 与 `native\openh264.dll` 同源 |
| 未验证项 | ① `run_native` 返回 `Err` 的那条日志**没有真实触发过**（本机没有能让 eframe 创建窗口/GL 上下文失败的稳定手段），代码路径与 panic 那一行同源、同样只 append 一行；② 真实 Explorer 双击（而非 `FreeConsole()` 等价复现）需要人坐在桌面前操作，本会话用等价条件替代；③ 窗口断言是在 `_rust\target-cap\release\CapLite.exe` 与**从 ZIP 解出来的同一个 exe** 上做的；`dist\` 里的那份没有再启动（启动会在发布目录里留下 `Cache\`），它与 release 产物签名前后都是同一份代码、`Subsystem=2` 已单独核对 |

---

## 11.18 FFmpeg DLL：**枚举完成后主动预加载**（延迟加载的收尾）

### 11.18.1 问题：延迟加载把 110 MB 的映射推到了"按下开始录制"那一刻

5 个 FFmpeg 运行期 DLL（`avcodec-63` / `avformat-63` / `avutil-61` / `swresample-7` /
`swscale-10`，合计约 **118 MB**）已经改成 `/DELAYLOAD`（见 `build.rs` 文件头注释）：
好处是**进程启动瞬间不映射**它们（启动路径上少一大块 IO + Defender 扫描），
代价是**第一次真正调用**其中的函数时才映射 —— 而那个时刻正是用户按下「开始录制」的瞬间，
按钮会顿一下（本机实测要映射的就是那个 91 MB 的 `avcodec-63.dll`）。

### 11.18.2 修法：首帧之后、**显示器与音频枚举完成**那一刻，后台线程预加载

| 项 | 决定 | 为什么 |
|---|---|---|
| 时机 | `MainPage::ui()` 末尾：`defer_ticks == 0`（本帧稍前已完成显示器枚举 + 目录探测）**且**音频设备已"尝试过枚举"（`mic_combo` 在同一帧完成） | 用户明确要求"显示器、音频枚举完成之后就加载"；这两个事件都在**首帧之后**，所以"启动瞬间不映射"这条红利一点没丢 |
| 方式 | `startup::preload_ffmpeg_dlls_once()` → `std::thread::spawn` 里逐个 `LoadLibraryW` | 不占 UI 线程、不阻塞枚举本身（`ui()` 不等线程结束）；`LoadLibraryW` 只加引用计数，与延迟加载的 `__delayLoadHelper2` 走同一把加载器锁，天然安全 |
| 一次性 | `AtomicBool`（`claim_once`） | `ui()` 每帧都会走到这段判断：没有一次性标志就会反复起线程 |
| 释放 | **故意不 `FreeLibrary`** | 预加载的意义就是"让它常驻"；释放等于白做，还会把延迟加载拉回按需映射 |
| 失败 | **只写一行 warning 到 `cap.log`**（`[cap] FFmpeg 预加载警告：3/5 成功…`），绝不 panic、绝不影响录制 | 预加载是"提前做的好事"，不是正确性前提；真正的调用点仍由延迟加载兜底 |
| 成功 | 写一行 + 一条阶段打点 `phase=ffmpeg_preload_done`（带 `t=` / `dur=` / `5/5 成功`） | 让"预加载到底发生在启动后多久"变成**可解析的时间轴证据**，而不是靠肉眼计时 |

### 11.18.3 DLL 名只有一份（构建期与运行期共用）

名字带版本号（`avcodec-63.dll`），**只能**从 `$FFMPEG_DIR/bin` 的实际文件名推导；名字对不上时
延迟加载与预加载都是**静默失效**（不报错、只是行为退回"启动即映射"或"按录制才映射"）。
所以 `build.rs::main()` 里**只调一次** `ffmpeg_dll_names()`，把同一个 `Vec<String>`：

1. 写成 `OUT_DIR/ffmpeg_dll_names.rs`（`pub const FFMPEG_RUNTIME_DLLS: &[&str] = &[...]`），
   由 `src/ui/startup.rs` 的 `pub mod ffmpeg_dlls { include!(..) }` 引入给运行期预加载；
2. 逐个发给链接器做 `/DELAYLOAD:<name>`。

**两侧不许各写一份**这条由测试兜底（`cargo test --lib`）：

* `runtime_dll_names_equal_build_rs_rule_applied_to_ffmpeg_dir` —— 运行期常量必须等于
  "用 `build.rs` 的 `FFMPEG_LIB_STEMS` 规则在 `$FFMPEG_DIR/bin` 上**现算一遍**"的结果（防生成物过期）；
* `runtime_dll_names_are_dll_files_matching_build_stems` —— 每项都是命中该前缀的 `.dll`；
* `build_rs_does_not_hardcode_versioned_dll_names` —— `build.rs` 里不许出现带引号的
  `"avcodec-63.dll"` 这种写死名字（那正是"升级 FFmpeg 漏改一处"的来源；注释里提及是允许的）。

`windows` crate 只为此新增了一个 feature：`Win32_System_LibraryLoader`（`LoadLibraryW`）。

### 11.18.4 与既有行为的关系（都没动）

> **启动优化的实测数字（本节三项 + 预加载时机）统一记录在 [§11.19.4](#11194-启动耗时改前--改后实测冷热对比)**：
> ① 延迟加载（首个 `av*/sw*` 模块从 65→71 ms 推到 360→365 ms）、
> ② 枚举推迟到首帧之后（首帧 233→236 ms，持平）、
> ③ 中文子集字体内嵌（520 KiB，`font_subset_installed` 在 t≈243 ms 完成）、
> ④ 预加载时机（`enum_audio_done t=309ms` → `ffmpeg_preload_done t=330ms dur=19.8ms`）。

* **延迟加载仍然生效**：`build.rs` 照旧发 `/DELAYLOAD` + 生成 MSVC 导入库；预加载只是
  "提前按需映射"，不是退回隐式链接 —— 静态证据（Delay Import Directory 非空）与运行期
  证据（启动瞬间 `Modules` 里没有 `av*`/`sw*`）都仍然成立；
* **枚举/目录探测仍推迟到首帧之后**（`DEFER_INIT_FRAMES = 2`、`flush_deferred_init` 未改语义）；
* **字体子集策略不变**（子集内嵌 + 首帧后追加系统完整字体，见 §11.16）；
* 录制/编码/封装/剪辑逻辑一行未改（预加载只多映射几个 DLL，没有任何调用点被改动）。

### 11.18.5 本轮验证记录

| 命令 / 方法 | 结果 |
|---|---|
| `cargo build --all-targets` | **成功，零 rustc 告警**（唯一的 `warning: cap@0.1.0: 已启用 FFmpeg DLL 延迟加载…` 是 `build.rs` 的**构建期提示**，改动前就在） |
| `cargo test` | **lib 132 passed**（基线 126 + 本轮新增 6 条：清单同源 3 条 + 一次性判定 1 条 + 失败只记日志/空清单空转 2 条）；`tests\e2e.rs` **7 passed**；doc-tests **1 passed（1 ignored，与基线同）** |
| `scripts/regression.ps1` | **12 条全部 PASS，退出码 0**（跑的是 debug 产物，与改动前逐条一致） |
| 运行期时序（release，`ProcessStartInfo` 启动，时间为**相对真实进程创建**） | 未按任何按钮：`t=+44ms` / `t=+259ms` 模块列表**空**；`t=+354ms` 起 5 个 DLL 全部出现；`t=+2916ms` 仍 5 个。进程内时间轴（`cap.log`）：`ui_first_frame_done t=233ms` → `deferred_enum_done t=284ms`（显示器 2 台）→ `enum_audio_done t=288ms`（麦克风 1 个）→ `ffmpeg_preload_done t=295ms dur=6.2ms extra="5/5 成功"` —— **加载确实发生在两处枚举之后**（不是启动即映射，也不是等录制） |
| 冷缓存对照（首次运行 / 把 exe 与 DLL 复制到新目录后运行） | 首次运行：`ffmpeg_preload_done t=1267ms dur=967ms`，外部采样在 `+1109/1207/1210ms` **仍为空**、`+2002ms` 出现 5 个；新目录副本：`t=+257ms` 空 → `+352ms` 只出现 `avcodec-63` → `+399ms` 3 个 → `+634ms` 4 个 → `+682ms` 5 个（逐条 `LoadLibraryW` 的**渐进**过程），同期 `ffmpeg_preload_done dur=307.7ms` |
| 按「开始录制」前后（release，键盘导航 `Tab×4 → Space`，见下） | `[A] 点击前` / `[B] 开始后` / `[C] 停止后` 三次快照**都是同样 5 个模块**（`avcodec-63`/`avformat-63`/`avutil-61`/`swresample-7`/`swscale-10`），**新增 0 个**；产物：导出 `rec-*.mp4` 325 514 字节 + Cache 里流文件 324 220 字节，`ffprobe` 读出 `h264 1920x1080` + `aac`，时长 3.04 s |
| 本会话**无法注入鼠标**（如实说明） | `SetCursorPos()` 直接返回 **False**（`GetCursorPos` 不变）、`mouse_event` 无效：本会话的 pwsh 运行在 **Low 完整性级别**，而前台窗口是中等完整性进程；`PostMessage(WM_MOUSEMOVE/WM_LBUTTONDOWN)` 虽然返回 True（`WM_CLOSE` 能关掉进程、`WM_SIZE` 能触发重绘），但**不会**进入 egui 的指针状态（按钮 hover 色不变）。最终改用**键盘导航**完成"按录制"：实测 Tab 顺序 = `#1 录制页签 / #2 剪辑页签 / #3 设置页签 / #4 开始录制按钮`，在第 4 站按 Space 即开始录制（同一站再按 Space 停止）。这条限制只影响"自动化点按钮"的手段，不影响被测代码 |

---

## 11.19 最小 FFmpeg 构建（**交付落地**）

本节把"发布包为什么能瘦下 110 MB"讲清楚：FFmpeg 由**本项目自建的最小配置 LGPLv3 共享版**提供，
不再是 BtbN 全功能包。上一节（§11.18）讲的是"什么时候加载 DLL"，本节讲"装进发布包的是哪些 DLL"。

### 11.19.1 为什么：FFmpeg DLL 114.43 MB → 4.20 MB（−96.3%）

| 项 | 旧（BtbN 全功能 LGPL 包） | 新（自建最小配置） | 变化 |
|---|---:|---:|---:|
| FFmpeg 运行期 DLL 合计 | 119 989 248 B（**114.43 MB**） | 4 407 808 B（**4.20 MB**） | **−110.2 MB（−96.3%）** |
| `dist\CapLite-0.1.0\`（12 个文件） | 137 467 828 B = **131.10 MB**（按旧 ZIP 条目逐项相加） | 21 907 491 B = **20.89 MB**（实测） | **−110.21 MB（−80.2%）** |
| `dist\CapLite-0.1.0-win64.zip` | 9 685 479 B（旧包，全功能 DLL） | **9 689 826 B = 9.24 MB** | ZIP 体积**几乎不变**（见下） |

> ZIP 体积为什么没跟着变小：ZIP 里的大头是 **15.1 MB 的 `CapLite.exe`**（Rust/egui 静态链接，
> 其中大部分是不可压缩的代码与内嵌字体子集），压缩后约 5 MB；旧包里那 114 MB 的 DLL
> 压缩率极高（~92%），压完只剩约 4.2 MB。所以**解压后**少了 110 MB，**下载体积**基本持平。
> 这正是"最小 FFmpeg"的价值所在：**安装后占用的磁盘与内存**（以及首次映射/扫描的 IO）大幅下降。


逐个 DLL（实测字节数）：

| DLL | 旧 | 新 | 压缩比 |
|---|---:|---:|---:|
| `avcodec-63.dll` | 91 136 000 B (86.9 MB) | 2 087 424 B (2.0 MB) | −97.7% |
| `avformat-63.dll` | 22 769 664 B (21.7 MB) | 607 232 B (0.6 MB) | −97.3% |
| `avutil-61.dll` | 3 016 704 B (2.9 MB) | 686 080 B (0.65 MB) | −77.3% |
| `swresample-7.dll` | 734 720 B (0.7 MB) | 106 496 B (0.1 MB) | −85.5% |
| `swscale-10.dll` | 2 332 160 B (2.2 MB) | 920 576 B (0.9 MB) | −60.5% |

**CapLite.exe 本身不变**（15 831 552 B / 15.10 MB）：我们只换了它**动态链接**的运行库，没动代码。
省下来的空间全在 DLL 上 —— 原来那 5 个 DLL 里 99% 的代码（avfilter/avdevice/各硬件加速器/
上百个与录屏无关的编解码器）从来不会被这个程序调用。

### 11.19.2 configure 参数（**原文，复现用**）

工具链：`C:\mingw64`（GCC 14.2.0 + binutils）+ `mingw32-make` 4.4.1 + `nasm` 3.02，
POSIX shell 用 **busybox-w32 1.33**（原因见 11.19.3 第 2 条）。

```powershell
$bb = "$short\_rust\tools\bbbin\busybox.exe"
$a = @(
 'configure',
 "--prefix=$short\CapLite\ffmpeg-min",
 '--arch=x86_64','--target-os=mingw32',
 '--enable-shared','--disable-static','--enable-version3',
 '--disable-everything','--disable-programs','--disable-doc','--disable-debug','--disable-network',
 '--disable-avdevice','--disable-avfilter','--disable-autodetect',
 '--enable-avcodec','--enable-avformat','--enable-avutil','--enable-swresample','--enable-swscale',
 '--enable-encoder=aac','--enable-decoder=h264','--enable-parser=h264',
 '--enable-muxer=mp4','--enable-muxer=mov','--enable-demuxer=mov','--enable-protocol=file',
 '--enable-small','--extra-version=capmin','--extra-ldflags=-static-libgcc'
)
Push-Location $srcShort
& $bb sh @a                       # 约 2 分钟，exit 0
```

`configure` 成功后的关键输出（存档）：

```
Libraries:   avcodec  avformat  avutil  swresample  swscale
Enabled decoders: h264          Enabled encoders: aac
Enabled parsers:  ac3  h264     Enabled demuxers: mov
Enabled muxers:   mov  mp4      Enabled protocols: file
Enabled filters:  （空）        Enabled bsfs: aac_adtstoasc  vp9_superframe
License: LGPL version 3 or later
```

`make` / `make install`（三个覆盖**必须**带上，原因见 11.19.3）：

```powershell
$mk = "$short\_rust\tools\bbbin\make.exe"
Push-Location $srcShort
& $mk -j8     AR_CMD=C:/mingw64/bin/ar.exe NM_CMD="C:/mingw64/bin/nm.exe -g" LN_S="cp -f"
& $mk install AR_CMD=C:/mingw64/bin/ar.exe NM_CMD="C:/mingw64/bin/nm.exe -g" LN_S="cp -f"
```

全量编译约 30 秒（`-j8`）+ configure 约 2 分钟 = **118.7 s**。

**产物即一个完整可用的 `FFMPEG_DIR`**：`CapLite\ffmpeg-min\` 里 `include\`（bindgen 头文件）、
`lib\*.lib`（MSVC 导入库）+ `lib\*.dll.a`（MinGW）、`bin\*.dll` 三样齐全，所以它能**从零重编整个
crate 图**（`ffmpeg-sys-next` → `ffmpeg-next` → `CapLite`）而不需要 BtbN 开发包 —— 这一点已实测。

### 11.19.3 踩过的坑（都是实测，**不要再踩**）

| # | 坑 | 现象 | 对策 |
|---|---|---|---|
| 1 | `--disable-postproc` 在 **FFmpeg 9.0 已不存在** | `Unknown option "--disable-postproc"` 直接退出 | 参数表里**不要**写它 |
| 2 | 沙箱封了 Windows `Global\` 内核对象命名空间 | MSYS2 / Cygwin 运行时（Git for Windows 自带的 `bash`/`sh`/`sed`/`grep`/`awk`）**全部起不来**：`couldn't create signal pipe, Win32 error 5`，因为它们的共享内存正好用 `Global\` | 用 **busybox-w32**（原生 Win32 二进制，只依赖 KERNEL32/USER32/ADVAPI32/msvcrt/WS2_32），内置 ash + 156 个 applet，够 configure 用 |
| 3 | 路径里的空格（工作区是 `C:\Users\Limit li\...`） | configure 拆开路径：`configure: eval: line 1: li/Documents/.../test.c: not found` → `C compiler test failed.` | 全程用 **8.3 短路径**（`cmd /c for %I in ("<长路径>") do @echo %~sI`），并且**连 `TMP`/`TEMP`/`TMPDIR` 一起**指到短路径下（只改 `--prefix` 不够，configure 的临时目录同样会炸） |
| 4 | busybox 的 ash 启用了 **standalone applet 派发**（先在自己内部找 applet，**不看 PATH**） | 即使 `C:\mingw64\bin` 排在 PATH 最前，`ar` 仍命中 busybox 自带的 `ar` applet → `sh: unknown option -- s` / `Could not create temporary library.` | 覆盖 **`AR_CMD` / `NM_CMD`**（注意不是 `AR`：FFmpeg 的 `SLIB_CREATE_DEF_CMD` 用的是 `$(AR_CMD)`/`$(NM_CMD)`，`ffbuild/config.mak` 里写死为 `ar` / `nm -g`）。**别指望改 PATH 顺序能解决 applet 遮蔽** |
| 5 | Windows 建不了符号链接 | `LN_S` 默认 `ln -s -f` 报 `ln: avutil.dll: Function not implemented`，卡在 `libavutil/avutil.dll` | 覆盖 **`LN_S="cp -f"`**（`cd ./libavutil && $(LN_S) avutil-61.dll avutil.dll`，用复制等价） |
| 6 | `swscale-10.dll` 默认依赖 `libgcc_s_seh-1.dll`（发布包里没有！） | 运行期缺 DLL | configure 必须带 **`--extra-ldflags=-static-libgcc`** |
| 7 | BtbN 随包的 `ffprobe.exe`/`ffmpeg.exe` 与最小版 **不兼容** | 把 5 个最小 DLL 放到 `ffprobe.exe` 旁边 → exit `-1073741515`（0xC0000135 = STATUS_DLL_NOT_FOUND，缺 `avfilter-12`/`avdevice-63`）。补上那两个 → exit `-1073741511`（0xC0000139 = STATUS_ENTRYPOINT_NOT_FOUND：`avfilter` 需要最小版 `avcodec/avutil` 没编进去的 `avpriv_elbg_*` / `av_amf_*` 等 8 个符号） | **不要**把 `CapLite\ffmpeg-min` 放进 `env.ps1` 的 `FfmpegCandidates` 首位；CLI 目录独立成 `$env:FFMPEG_TOOLS_DIR`（见 11.19.5） |

### 11.19.4 启动耗时：改前 / 改后实测（冷热对比）

**"改前"基线怎么来的（不是估算）**：`build.rs` 本来就留了 A/B 开关 `CAP_NO_DELAYLOAD=1`。
本次先 `CAP_NO_DELAYLOAD=1 cargo build --release --bin CapLite` 得到一个**隐式映射**（DLL 在进程启动时
就被加载）的 `CapLite.exe`，另存到 `_rust\target-cap\release-nodelayload\`，再把 7 个 DLL 复制到它旁边，
用同一个脚本以相同方式测量。两个 exe 的静态差别可复核：

| 构建 | `DelayImport` 数据目录 | CapLite.exe 字节数 |
|---|---|---|
| 改前（`CAP_NO_DELAYLOAD=1`） | `(RVA=0x0, size=0)` —— 无延迟导入 | 15 827 968 |
| 改后（当前 release） | `(RVA=0xEC4B80, size=160)` —— 5 个 DLL 全部延迟 | 15 831 552 |

> **顺手修掉的一个真问题**：`build.rs` 原来没有 `cargo:rerun-if-env-changed=CAP_NO_DELAYLOAD`，
> 所以设了 `CAP_NO_DELAYLOAD=1` 重新 build 时 **cargo 会复用上一次的构建脚本输出**，
> 这个开关**看起来没生效**（实测：新 exe 仍然带 `DelayImport size=160`）。
> 已在 `build.rs` 里补上这行声明（只影响构建配置，不影响任何运行期行为）。

测量方式：`scripts\startup-compare.ps1`（新增，见 11.19.5）——外部 `ProcessStartInfo` 启动、
5 ms 轮询窗口与 `av*/sw*` 模块、解析 `cap.log` 的 `phase=` 打点。
**热启动 = 连跑 3 次取后 2 次的 min/中位/max**（第 1 次单独列，它带首次读盘/Defender 扫描）。

| 口径 | 改前（隐式映射，热） | 改后（延迟 + 预加载，热） | 差值 |
|---|---:|---:|---:|
| 进程创建 → **首个 `av*/sw*` 模块加载** | **64.9 / 68 / 71.1 ms** | **360.5 / 362.8 / 365 ms** | +292 ms（**这就是省下的启动时间**） |
| 进程创建 → 主窗口可见（外部墙钟，5 ms 分辨率） | 60.6 / 74.4 / 88.1 ms | 54.6 / 56.5 / 58.4 ms | −18 ms |
| 进程创建 → 首帧（`t_z + ui_first_frame_done`） | 233 / 233.5 / 234 ms | 233 / 236 / 239 ms | ≈ 持平 |
| 首帧 → 显示器/音频枚举完成 | 49 / 49.5 / 50 ms | 51 / 51.5 / 52 ms | ≈ 持平 |
| 枚举完成 → FFmpeg 预加载完成 | 6 / 6.5 / 7 ms | 12 / 12.5 / 13 ms | +6 ms（后台线程） |
| 进程创建 → FFmpeg 预加载完成 | 288 / 289.5 / 291 ms | 297 / 300 / 303 ms | +10 ms（**都在后台线程里**） |
| 预加载自身耗时（热，DLL 已被系统缓存） | — | **6.6 ms**（3 次完全一致） | — |
| 峰值工作集 | 219.9 / 220 / 220.1 MB | 219.4 / 219.6 / 219.7 MB | 无变化 |

**首个 `av*/sw*` 模块从 65→71 ms 推到 360→365 ms**：改前那 87 MB 的 `avcodec-63.dll`
是**进程启动路径**上的隐式映射（在 `main()` 之前）；改后进程启动时**一个都不映射**，
直到"首帧之后 + 显示器/音频枚举完成"，由后台线程逐个 `LoadLibraryW`。
**用户可感知的那一段（窗口出现、首帧、枚举）完全没变慢**，多出来的 10 ms 全在后台线程里。

**冷缓存首跑（如实标注：不是真正的冷启动）**：本机无法在不重启系统的前提下清空文件缓存，
所以只能给"本次会话第一次运行"：

| 场景 | 改前 | 改后 |
|---|---:|---:|
| 第一次运行：预加载自身耗时 | — | **973.8 ms**（DLL 首次从磁盘读 + 扫描） |
| 第一次运行：进程创建 → 预加载完成 | 288–303 ms（同一会话内已缓存） | **1282 ms** |

即：**最坏情况（DLL 完全不在系统缓存里）下，预加载要把 `avcodec-63.dll` 从磁盘读进来，
约 0.97 s —— 但它发生在后台线程里，界面早已可用**；这正是"延迟加载 + 枚举后预加载"这个组合的
设计目的：不阻塞启动，也不把代价留到用户按下「开始录制」那一刻（§11.18.1）。
如实说明局限：这两行是**同一台机器、同一次会话**的对照，不是"重启后的真冷启动"。

### 11.19.5 与 `FFMPEG_DIR` 的关系（**为什么不直接换掉它**）

`$env:FFMPEG_DIR` 同时被三处消费：`ffmpeg-sys-next` 的 bindgen/链接、`build.rs` 复制运行期 DLL
（`$FFMPEG_DIR\bin\av*.dll`）、以及**同一个目录里的 `ffmpeg.exe`/`ffprobe.exe`**。
按 11.19.3 第 7 条，把最小包设成 `FFMPEG_DIR` 会让同目录的 CLI 起不来，而
`scripts\regression.ps1` 会真调这两个 CLI **约 11 次**（含 `-af volumedetect`，需要 avfilter）
→ 12/12 PASS 必然变红。

所以本次**没有**改 `FfmpegCandidates` 的顺序，而是把两件事分开：

| 变量 | 指向 | 谁用 |
|---|---|---|
| `FFMPEG_DIR` | 完整开发包（`CapLite\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0`） | bindgen / 链接 / `build.rs` 复制运行期 DLL / 交叉校验 CLI |
| `FFMPEG_TOOLS_DIR`（新增） | 上述开发包的 `bin\` | 只给 `scripts\regression.ps1` 用（`-ToolsDir` > `$env:FFMPEG_TOOLS_DIR` > 回落 `<FfmpegDir>\bin`） |

发布包与这两个变量**都无关**：`dist\` 里放的是 `CapLite\ffmpeg-min\bin` 的 5 个 DLL。

**顺带修掉 `_rust\env.ps1` 的一个既有缺陷（UTF-8 BOM）**：该文件本来就是"UTF-8 **无 BOM**
且含中文"，所以 **Windows PowerShell 5.1 按 ANSI 解析会失败**——`scripts\regression.ps1`
早就记录了这个事实，因此它**不直接点源**，而是显式按 UTF-8 读文本再 `Invoke-Expression`。
精确重建"编辑前"版本对比：ANSI 模式下本来就有 **5 个 ParserError**，我新增的中文注释使其变成
**7 个**（缺陷本来就在，但被放大了 2 个）。处理：给该文件加 BOM。
现在 PS 5.1 与 PS 7 的 `ParseFile` 都是 **ParserErrors=0**，新会话点源导入正常
（实测 `FFMPEG_DIR` / `FFMPEG_TOOLS_DIR` 都正确设置），且现有消费者行为不变
（`regression.ps1` 按 UTF-8 读并 `TrimStart` BOM）。

### 11.19.6 许可合规（LGPLv3）

* **许可**：`--enable-shared --enable-version3`，**没有** `--enable-gpl` / `--enable-nonfree`；
  `configure` 自报 `License: LGPL version 3 or later`（构建脚本对这个字符串做**硬校验**：
  不含 `LGPL` 就直接失败退出）。
* **链接方式**：动态链接（`--enable-shared --disable-static`），发布包内 5 个 DLL 与 `CapLite.exe` 同目录。
* **用户可替换**：DLL 可被同名文件直接覆盖生效；程序**不校验**这些 DLL 的签名或哈希（LGPLv3 第 4 条要求）。
* **源码获取方式（可复现）**：
  * FFmpeg 9.0.2 上游源码：`http://mirrors.tuna.tsinghua.edu.cn/debian/pool/main/f/ffmpeg/ffmpeg_9.0.2.orig.tar.xz`
    （Debian pool 里的 `.orig.tar.xz` 就是上游 pristine tarball），
    SHA256 `8C3850283EB25FA026482078A04051E0BE17347B09EF81A0849BEC15A96E002E`；
  * **构建脚本**：`scripts/build-ffmpeg-min.ps1`（本项目提供，随仓库分发）——
    它记录完整 configure 参数、必需的 make 覆盖、依赖自检，能**从头复现**发布包里的二进制；
  * 发布包内的许可全文：`LICENSE-FFmpeg.txt`；组件声明：`THIRD-PARTY-NOTICES.txt`。
* **分工**：**开发包留 `ffprobe.exe`/`ffmpeg.exe`（验证 oracle，不进产品），发布包只带最小 5 个 DLL**
  （不含 `avfilter`/`avdevice`，也不含任何 CLI 程序）。

### 11.19.7 本轮验证记录

| 命令 / 方法 | 结果 |
|---|---|
| `cargo build --all-targets` | **成功，零 rustc 告警** |
| `cargo test` | **lib 142 passed / 0 failed**；`tests\e2e.rs` **7 passed**；doc-tests **1 passed（1 ignored）** |
| `scripts\regression.ps1` | **12 条全部 PASS，退出码 0** |
| release 运行期（`ProcessStartInfo` 启动，独立配置目录） | `t=0 log_ready` → `t=253ms ui_first_frame_done` → `t=304ms deferred_enum_done`（显示器 2 台）→ `t=309ms enum_audio_done`（麦克风 1 个）→ `t=330ms ffmpeg_preload_done dur=19.8ms extra="5/5 成功"`；同一进程模块列表 = 5 个 `av*`/`sw*`（**加载发生在两处枚举之后**）；峰值工作集 220 MB |
| 发布包体积 / 签名 / SHA256 | 见 §11.20 |

---

## 11.20 最终签名与打包（0.1.0）

`dist\CapLite-0.1.0\`（**恰好 12 个文件**，21 904 932 B = **20.89 MB**）：

| # | 文件 | 字节 | 来源 |
|---|---|---:|---|
| 1 | `CapLite.exe` | 15 832 976（**已签名后**） | `_rust\target-cap\release\CapLite.exe`（本轮重建 + `dev-sign.ps1`） |
| 2–6 | `avcodec-63.dll` / `avformat-63.dll` / `avutil-61.dll` / `swresample-7.dll` / `swscale-10.dll` | 2 087 424 / 607 232 / 686 080 / 106 496 / 920 576 | `CapLite\ffmpeg-min\bin`（自建最小配置，11.19） |
| 7–8 | `openh264.dll` / `openh264-7.dll` | 823 296 ×2 | `CapLite\native`（内容一致，两个名字都要） |
| 9–11 | `LICENSE-FFmpeg.txt` / `LICENSE-OpenH264.txt` / `THIRD-PARTY-NOTICES.txt` | 7 651 / 1 317 / 6 150 | 随包许可与组件声明 |
| 12 | `README.txt` | 4 997 | 随包说明 |

（`openh264.dll` 与 `openh264-7.dll` 的 SHA256 相同 = `EB0B38B8…`，即同一份文件的两个名字。）

* **签名**：`scripts\dev-sign.ps1`（自签证书 `CN=CapLite dev signing`，thumb `22DACD13…`）→
  `Status=Valid`、`StatusMessage=Signature verified.`、`SignatureType=Authenticode`、
  `MotW=无`（`Zone.Identifier` 原有 0 个、清除后残留 0 个）；
* **打包**：`dist\CapLite-0.1.0-win64.zip` = **9 689 826 B（9.24 MB）**，
  12 个条目（根目录名 `CapLite-0.1.0/`）；
  解压到临时目录后**逐个核对 SHA256 与源目录一致：12/12 OK，0 不一致**（核对完已清理临时目录）；
  解压出的 `CapLite.exe` 回读签名仍是 `Status=Valid`（签名在 ZIP 里保持不变）；
* **数字**：`SHA256(ZIP)` = `305F4494…E96C65`；`SHA256(CapLite.exe)`（dist 内已签名版本）= `23A3BF92…C34DB86B`；
  未签名的 release 产物 = `A4C18383…2D89B11`（签名会改写文件，所以两者不同）；
* `dist\` 里**只有**这一个 ZIP 与这一个解压目录（没有多余文件）。

## 11.21 改名 CapLite + 蓝色图标 + 默认英文（本轮）

### 11.21.1 改名：目录 / 包名 / 二进制名 / 显示名**四处同名**

| 处 | 改前 | 改后 |
|---|---|---|
| 目录 | `CapLite\` | `CapLite\` |
| `Cargo.toml` 包名 | `cap` | `caplite`（新增显式 `[lib] name = "caplite"`） |
| 二进制 | `cap.exe`（隐式） | **`CapLite.exe`**（新增显式 `[[bin]] name = "CapLite" path = "src/main.rs"`；`src/bin/*.rs` 仍自动发现） |
| 显示名 | `theme::APP_NAME = "CaputraPro"` | `theme::APP_NAME = "CapLite"`（窗口标题 + 关闭提醒窗口共用同一个常量） |
| 版本号 | `0.1.0` | **`1.0.0`**（`Cargo.toml` 的 `version`；对外展示即 **1.0**。`docs` 里 §11.17–§11.20 是**上一轮 0.1.0 的历史记录**，其中的 `CapLite-0.1.0` 字样刻意保留不改，避免篡改历史证据） |

* `use cap::…` → `use caplite::…`：**111 处 / 11 个文件**（`src/main.rs` 85、`src/bin/*` 17、
  `src/ui/*` 6、`tests/e2e.rs` 2、`src/video/openh264/encoder.rs` 1）。
* 全仓路径字符串 `CapLite` → `CapLite`：**71 处 / 14 个文件**（`_rust\env.ps1` 5、
  `scripts\*.ps1` 28、`docs\*.md` 24、`src\video\openh264\encoder.rs` 6、`BUG_REPORT.md` 3、
  `native\bindgen-stubs\cuda_stub.h` 1、`dist\*\THIRD-PARTY-NOTICES.txt` 2 …）；
  `cap.exe` 字样 → `CapLite.exe`：**46 处 / 4 个文件**。
* 残留的 `cap` 是**有意保留**的内部标识（都不影响用户可见名称，改它们只会增加回归风险）：
  日志前缀 `[cap]`、日志文件名 `cap.log`、线程名 `cap-mux`/`cap-video`/`cap-audio`、
  临时目录前缀 `cap-probe-*`/`cap-cache-*`/`cap-export-*`/`cap-e2e`、配置目录
  `Documents\cap` 与 `cap-recordings`（后者是**已有用户的数据目录**，改名会让老用户的
  配置与缓存"凭空消失"）。日志前缀与文件名若要改，需要同步改动 `regression.ps1`
  的多条日志断言 —— 属于"改名之外"的一轮，本轮按"不动录制/编码/封装/剪辑算法与脚本断言"处理。
* `_rust\target-cap`（构建产物目录）**保持原名**，避免全量重编。

### 11.21.2 图标：`scripts/make-icon.py` → `assets/caplite.ico` → `build.rs` 用 `rc.exe` 嵌入

* **生成**（可重复、离线、无新依赖）：本机 Python 3.12 + Pillow 12.3.0。
  设计：1024² 主图（圆角半径 21.5%），垂直渐变 `#3B82F6 → #2563EB`（下端 = 界面强调色
  `theme::ACCENT`），白色粗体 **"Cap"**（Segoe UI Semibold，墨迹宽度 = 画布 80%，故左右
  各留 10%），右下角**半透明 "Lite" 只在 ≥64 px 的尺寸**绘制（16 px 上多一行小字只会糊）。
  导出多尺寸 ICO **16/24/32/48/64/128/256** + `caplite-256.png`（预览）。
  再生成方法：`python scripts\make-icon.py`（已存在则跳过，`--force` 强制重写；
  脚本 docstring 写明了全部设计参数）。
* **嵌入**：`build.rs` 新增 `embed_icon()`（文件头 §5）：写 `OUT_DIR/caplite.rc`
  （`1 ICON "<ico 绝对路径，正斜杠>"`）→ Windows SDK 的 `rc.exe`
  （`bin\<WindowsSDKVersion>\x64\rc.exe`，`rc` 不在 PATH 里时按 `WindowsSdkDir`/默认根目录
  探测）→ `cargo:rustc-link-arg=<OUT_DIR>/caplite.res`。
  `.res` 参与链接时同时发 `/MANIFEST:EMBED /MANIFESTINPUT:app.manifest`，
  这样**图标与 DPI 声明同时在**（`/MANIFEST:NO` 会丢掉 DPI 声明，故不用）。
  `rc.exe` 找不到 / 非零退出 → **只 warn，不中断构建**（图标是好看，不是能跑）。
* **资源序号用 `1` 而不是 `IDI_ICON1`**：实测 `rc.exe` 把标识符原样存成**字符串名**
  （`EnumResourceNames(RT_GROUP_ICON)` → `NAME:IDI_ICON1`），而 Windows 惯例是取序号 1 的组图标。
* **回读验证**：`scripts/extract-icon.ps1` 从 exe 资源节枚举
  `RT_GROUP_ICON`(14) → 解析 `GRPICONDIR` → 按 `RT_ICON`(3) 逐尺寸
  `CreateIconFromResourceEx` 还原 → 存 PNG。实测 **7/7 个尺寸全取回**
  （见 §11.21.4 的真数字）。

### 11.21.3 默认语言改英文

* `config::DEFAULT_LANGUAGE = "en-US"`（新增常量），`default_language()` 直接返回它；
  **不再看 `LANG` 环境变量** —— 否则"同一份 exe 在不同机器上默认语言不同"，与"新装默认英文"冲突。
* `I18n::default()` 与 `I18n::set_language()` 的兜底语言同步改成英文（未知语言 id 不再把界面
  打成空白）。
* **已有配置文件里的选择照旧生效**：`Config::sanitized()` 只在 `language` 为空时回填，
  写了 `zh-CN` 的配置原样保留（两条新单测钉住这两件事：
  `config::tests::default_language_is_english_and_user_choice_wins`、
  `i18n::tests::default_language_is_builtin`）。
* `locales\en-US.json` 与 `zh-CN.json` 的 key 集合一致性测试（原有）继续通过。

### 11.21.4 版本资源（`VERSIONINFO`）——两个**实测坑**

`build.rs` 的 `rc_body()` 除了 `1 ICON …` 还写一段 `VS_VERSIONINFO`，让"文件属性"有内容：
`FileDescription` / `FileVersion` / `ProductName` / `ProductVersion` / `OriginalFilename` /
`LegalCopyright` / `CompanyName`。两个坑都是实测撞出来的：

1. **资源名必须用序号 `1`，不能用字面量 `VS_VERSION_INFO`。**
   写 `VS_VERSION_INFO VERSIONINFO` 时 `rc` 会把 `VS_VERSION_INFO` 当成**字符串名**存进资源目录；
   结果 `GetFileVersionInfoSizeW` 直接返回 0（`ERROR_RESOURCE_TYPE_NOT_FOUND` 1813）——
   资源在文件里、字节完全正确，但 version.dll 按"**序号 1 的 VERSION 资源**"这个惯例去找
   （`notepad.exe` 用的就是序号 1）。改成 `1 VERSIONINFO` 后，立刻能读到。
2. **代码页用 `04e4`（1252），不要用 `04b0`（Unicode/1200）。**
   实测 `[System.Diagnostics.FileVersionInfo]::GetVersionInfo()`（PowerShell 的
   `(Get-Item x.exe).VersionInfo`）读 `040904b0` 的块会**全部返回空串**，换成 `040904e4` 就正常。
   反正我们的字符串全是 ASCII，用 1252 没有损失。

`PRODUCT_VERSION_STR = "1.0.0"` 是这一块的**单一来源**：`version_quad()` 把它转成
`FILEVERSION 1,0,0,0`（四段数字），非法段会让构建**直接失败**而不是悄悄填 0。

### 11.21.5 拿 PowerShell 5.1 跑脚本：`.ps1` 必须带 UTF-8 BOM

`regression.ps1` 在 `powershell.exe`（5.1）里报一堆 `Array index expression is missing` ——
根因是**改写脚本时把 UTF-8 BOM 丢了**：没有 BOM 时 Windows PowerShell 按系统 ANSI 代码页
（本机 GBK）解码 `.ps1`，中文字符串被解成乱码并破坏引号配对。已给
`scripts\*.ps1` 与 `_rust\env.ps1` **统一补回 `EF BB BF`**（`.rs` / `.md` / `.toml` / `.py`
不需要 BOM，保持无 BOM 的 UTF-8）。教训：**动 `.ps1` 之后要检查前 3 个字节**。

### 11.21.6 本轮验证记录

见本文件末尾「§11.22」的实测数字表（改名/图标/语言/版本/打包，与 §11.20 的 0.1.0
历史数字分开列，避免混淆）。

## 11.22 CapLite 1.0.0 验证记录（本轮实测）

> 与 §11.20 的关系：§11.20 记的是 **0.1.0** 那一轮的打包（12 文件 / 20.89 MB / ZIP 9.24 MB），
> 它**没有被改写**，作为历史证据保留。本节是改名 + 图标 + 默认英文 + 版本号改 1.0.0 之后的实测。

### 11.22.1 构建与测试

| 项 | 结果 |
|---|---|
| `cargo build --all-targets` | **成功，0 条 rustc 告警**（唯一的 `warning: caplite@1.0.0: …` 是 `build.rs` 的构建期提示） |
| `cargo test` | lib **144 passed / 0 failed**（基线 142，本轮新增 2 条：`config::tests::default_language_is_english_and_user_choice_wins`、`i18n::tests::default_language_is_builtin`）、e2e **7 passed**、doc **1 passed + 1 ignored** |
| `scripts\regression.ps1` | **12 条 PASS，exit 0**（第 1 次运行在"3. copy 导出"上 FAIL —— 那一次录制只有 3 个关键帧且时长 3465 ms，脚本的选点法退化成空区间；**第 2 次运行关键帧 5 个、时长正常，12/12 全过**。这个"关键帧太少"的偶发在历史记录里出现过两次：`_rust\ver-dense-1.txt`、`ver-dense-2.txt`） |
| 产物名 | `_rust\target-cap\{debug,release}\CapLite.exe`（旧的 `cap.exe` 是改名前的残留文件，不再生成） |

### 11.22.2 图标证据（从**发布包里的 exe** 资源节回读）

| 尺寸 | RT_ICON 序号 | 资源字节 | 还原出的 PNG |
|---|---:|---:|---:|
| 256×256 | 7 | 20 934 | 21 606 B |
| 128×128 | 6 | 9 624 | 10 144 B |
| 64×64 | 5 | 4 267 | 4 508 B |
| 48×48 | 4 | 2 666 | 2 852 B |
| 32×32 | 3 | 1 635 | 1 718 B |
| 24×24 | 2 | 1 143 | 1 260 B |
| 16×16 | 1 | 704 | 819 B |

* `RT_GROUP_ICON #1` = 7 个条目（104 字节），`scripts\extract-icon.ps1` **7/7 全部还原成功**；
* `dumpbin /headers`：`Resource Directory` = `F13000 [A790]`，`subsystem = 2 (Windows GUI)`；
* `assets\caplite.ico`（41 091 B）SHA256 = `70B1FED86C420E86CABA4C098B14D34CAEFF33859EC2AA827F4443BEDABD2A2A`。

### 11.22.3 版本资源（`Get-Item CapLite.exe | % VersionInfo`）

```
FileVersion      = 1.0.0
ProductVersion   = 1.0.0
ProductName      = CapLite
FileDescription  = CapLite screen recorder
OriginalFilename = CapLite.exe
LegalCopyright   = Copyright (c) 2026 CapLite
```

### 11.22.4 发布包（13 个文件 —— 本轮比 0.1.0 多一个项目自身的 `LICENSE`）

`dist\CapLite-1.0.0\` = **21 952 227 B（20.94 MB）**：

| # | 文件 | 字节 | 来源 |
|---|---|---:|---|
| 1 | `CapLite.exe` | 15 874 960（**已签名**） | `_rust\target-cap\release\CapLite.exe` + `dev-sign.ps1` |
| 2–6 | `avcodec-63.dll` / `avformat-63.dll` / `avutil-61.dll` / `swresample-7.dll` / `swscale-10.dll` | 2 087 424 / 607 232 / 686 080 / 106 496 / 920 576 | `CapLite\ffmpeg-min\bin` |
| 7–8 | `openh264.dll` / `openh264-7.dll` | 823 296 ×2 | `CapLite\native`（SHA256 相同 = `EB0B38B8…`） |
| 9 | `LICENSE` | 1 077 | 仓库根 `LICENSE`（**MIT**） |
| 10–11 | `LICENSE-FFmpeg.txt` / `LICENSE-OpenH264.txt` | 7 651 / 1 317 | 随包许可全文 |
| 12 | `THIRD-PARTY-NOTICES.txt` | 7 044 | 第三方组件声明（含"本项目自身许可见同目录 LICENSE（MIT）"） |
| 13 | `README.txt` | 5 608 | 英文使用说明（与仓库根 `README.md` 同内容） |

* `dist\CaputraPro-0.1.0\` 与 `dist\CaputraPro-0.1.0-win64.zip`（上一轮的旧包）**已删除**；
  `dist\` 下现在只有 `CapLite-1.0.0\` 与 `CapLite-1.0.0-win64.zip`；
* **ZIP**：`dist\CapLite-1.0.0-win64.zip` = **9 731 201 B（9.28 MB）**，13 个条目，根目录名 `CapLite-1.0.0/`；
* **逐文件核对**：解压到临时目录后与发布目录逐个比 SHA256 —— **13/13 一致，0 不一致**；
  解压出的 `CapLite.exe` 回读签名仍 `Status=Valid`、`VersionInfo` 仍是 `1.0.0 / CapLite`、
  备用数据流只有 `:$DATA`（无 MotW）。核对完临时目录已删除；
* **SHA256**：
  * `SHA256(CapLite-1.0.0-win64.zip)` = `C5F95253495782FCB513488A22B583438387868108AAE9D538A536F2058B332C`
  * `SHA256(CapLite.exe)`（发布目录内已签名版本）= `77EF33C7096759C09080E163907FCDEC686BCF4D2A1DB58E15D0F28D069D7F53`

### 11.22.5 签名（`scripts\dev-sign.ps1`）

* `Get-AuthenticodeSignature` → `Status=Valid`、`StatusMessage=已验证签名。`/`Signature verified.`、
  `SignatureType=Authenticode`、`Signer=CN=CapLite dev signing`（thumb `22DACD13…`，有效期至 2031-10-12）；
* MotW：原有标记 0 个、清除后残留 0 个；`TRUST-PROBE` 结论 = 只装 `TrustedPublisher` 就够（无需装 Root）；
* **签名者主体名是旧名字，原因如实记录**：本会话的进程**不能写用户证书库**
  （`New-SelfSignedCertificate` → `NTE_PERM 0x80090010`），因此无法新建 `CN=CapLite dev signing`。
  为此在 `dev-sign.ps1` 里加了一个**显式的兼容分支**：当"新主体一张可用证书都没有"时，
  复用本机已有、且**已经装进 TrustedPublisher** 的那张开发证书（`CN=CapLite dev signing`），
  并打印 WARN 说明。签名链因此仍然可信（`Status=Valid`），只有签名者显示的那行字是旧主体名；
  在有权限的普通 PowerShell 里跑一次本脚本即可换成新主体（那时不会走这条分支）。

### 11.22.6 干净启动（默认英文 + 窗口标题 + 无控制台）

`ProcessStartInfo{ UseShellExecute=$false }` 启动**发布包里的** `CapLite.exe`（`--config-dir` 指向空目录）：

* `MainWindowTitle` = **`CapLite`**，`Responding=True`，进程存活，**无控制台窗口**（`subsystem=2`，已核对）；
* 新配置目录下自动生成 `config.json`，其中 **`"language": "en-US"`** —— 新装即英文；
* 日志正常落盘（`cap.log` 2 224 B）：`phase=log_ready → eframe_backend_ready(253ms) → font_subset_installed
  → config_loaded → output_dir_checked → i18n_loaded → ui_first_frame_done(265ms) → full_font_deferred_loaded
  → deferred_enum_done（显示器 2 台）→ enum_audio_done（麦克风 1 个）→ **ffmpeg_preload_done 5/5（79 ms）**`；
* 注意：`--output-dir` **不是**程序支持的参数（只支持 `--config-dir`），所以那次启动按回退链
  自动挑了可写目录（`…\CapLite-1.0.0\cap-recordings`）—— 这是正确行为，不是缺陷；
* 结束后进程已 `Stop-Process` 结束，发布目录里的 `Cache\` 与 `cap-recordings\` 已清理（dist 恢复成
  13 个文件）。

### 11.22.7 未验证 / 已知残留

* **签名者主体名**仍是旧名（原因见 §11.22.5，需要一次有权限的普通 PowerShell）。
* `_rust\cargo\…`、`_rust\target-cap\…`、`_selfcheck\CapLite\…`、`_rust\tmp\dist-text\…`、
  `ffmpeg-gpl\README.txt` 里仍是旧名/旧路径 —— 这些是**构建缓存与历史证据**，按边界要求不动。
* 程序内部仍有一批 `cap` 标识（`[cap]` 日志前缀、`cap.log`、线程名、`cap-probe-*`/`cap-cache-*`
  临时目录、`Documents\cap` 与 `cap-recordings` 目录名）—— 见 §11.21.1 的取舍说明。

## 11.23 最小窗口尺寸：按"预览最小 200px"反推 + 取消上下滚动条（本轮）

### 11.23.1 需求与结论

用户原话："根据剪辑界面预览最小竖向尺寸，计算程序 ui 的最小显示界面，要求界面内容充满，
无滚动条，最小时，界面内容不能遮挡。录制界面，竖向方面紧凑一些……总之，取消上下滚动条。"

**最终 `min_inner_size = 760 × 625`**（逻辑点），常量集中在
`caplite::ui::layout`（`MIN_INNER_W` / `MIN_INNER_H`），`src/main.rs` 的
`ViewportBuilder::with_min_inner_size` 直接引用它们，不再有第二处魔法数。

### 11.23.2 怎么量出来的（全部 headless 实测，不是估）

`ui::layout` 的模块注释里写着完整口径，这里给结论数字（单位 = egui 逻辑点）：

| 量 | 实测 | 量它的单测 |
|---|---|---|
| 外壳（窗口高 − 页面可用高） | **126.0** | `src/main.rs::tests::headless_min_window_fits_every_page` |
| 剪辑页必需页面高度 | **499.0**（滚动区内容 455 + 页内底部两行 44） | `ui::editor::tests::headless_editor_required_height_fits_the_min_window` |
| 录制页必需页面高度 | **402.0**（本轮压到 **396.0**，见 §11.27） | `ui::main_page::tests::headless_record_required_height_fits_the_min_window` |
| 三页"不换行"的最小宽度 | **719**（剪辑页那一行最长；718 时 475→493） | 同上两条（宽度扫描） |

* **必需高度**：把窗口压到"剪辑页预览已经触底（200px）"的高度（340 / 400 各量一次，
  两次结果必须一致），读 `ScrollAreaOutput::content_size.y`；外壳算 `窗口高 − 页面可用高`
  （含中央面板 8+8 内边距）。
* **`min_h` = 外壳 + max(剪辑页, 录制页) = 126 + max(499, 402) = 625**。
  录制页压缩后已经**比剪辑页矮**，所以最小高度由剪辑页（预览 ≥ 200px）决定 ——
  这正是用户要求的方向（"尽量能在最小界面完全显示；如果不行，那么程序最小界面按录制页设置"）。
* **`min_w` = 760**：实测"不换行阈值"是 719，取 760 留 40px 给字体版本差异。
  720 以下剪辑页会有一行换行（高度 +18），所以 760 是**量出来的**而不是拍出来的。
* 数字必须与常量一致：三种单测都会**重新量一遍并断言等于常量**，常量过期立刻变红。

### 11.23.3 取消上下滚动条（三页都做）

* 三页的 `ScrollArea::vertical()` 统一改成
  `.scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden).auto_shrink([false, false])`：
  **滚动条一个像素都不画**，横向滚动本来就没开；
  设置页（`ui/settings.rs`）也一并改了 —— 它本来就装得下（两张卡片 ≈ 250px），
  改它只是为了"三页同一种口径"；
* 更关键的一处修复：剪辑页原本在预览块之后还有一句 `ui.add_space(GAP_CARD)`。
  预览块的高度**正好等于滚动区剩余高度**，再补 12px 间距就等于让内容**必然**比视口高出 12px
  —— 于是**即便窗口很大，剪辑页也一直挂着一条纵向滚动条**（用户看到的"上下滚动条"根源之一）。
  现在这一句删掉了：`content_size.y == inner_rect.height()`（单测断言），
  窗口够高时**永远不需要滚动**；
* 保留滚动区本体只为**罕见溢出**兜底（导出汇总卡片出现、录制中多出实时统计），
  那时滚轮仍可看到全部内容，而滚动条始终不可见；
* 顺带修掉一处"文字被裁切"：时间刻度尺最右那一格的时间码原本画在刻度右边、会**冲出时间线矩形**
  （最小宽度下看得最清楚）。现在右边界放不下就改成**右对齐贴住边界**，
  再放不下（会与前一格重叠）就**不画那一格的时间码**（刻度线仍然画）。

### 11.23.4 录制页竖向紧凑化（488 → 402）

> **后续**：本轮（§11.27）再把状态卡片那一行压了 6px → **402 → 396**，
> 手段是**只降计时器字号**（34 → 29）。下表是本节当时的改动，仍然是 488 → 402 的原因。

| 改动 | 省下 |
|---|---|
| 卡片内边距 14 → **10**（`theme::PAD_CARD_TIGHT` + `theme::card_tight`） | 4×2×4 = 32px |
| 卡片间距 12 → **8**（`theme::GAP_CARD_TIGHT`） | 3×4 = 12px |
| 「录制鼠标指针」并入「录制内容」那一行（只录声音时置灰、位置不变） | ~38px |
| 合计 | **86px（488 → 402）** |

「音源（可多选）」这一行**故意不并**进 `field_row` 的 88px 标签框：英文语言包里它是
`Audio sources (multi-select)`，塞进去会被裁掉/换行 —— "文字不被裁切"是硬要求。

### 11.23.5 顺带修掉的"极宽画幅把导出按钮挤出视野"

`btn_below`（导出按钮是否换行到预览下方）原先只看**可用宽度**够不够。
32:9 素材在最小宽度下保 200px 高度下限时，预览宽度 = 200 × 3.56 = 711px，
加上"间距 20 + 按钮 200"= 931 > 760 →按钮真的落到了窗口右边界之外。
现在多了一条判据：**用算出来的预览宽度判**（`size.x + side_col_w > page_w` → 换行到下方），
21:9 / 32:9 两种极宽画幅都有单测断言"按钮完整落在页宽内"。

### 11.23.6 本轮验证记录

* `cargo build --all-targets` → **0 个 rustc 告警**（只有 build.rs 原有的两条说明性 warning）；
* `cargo test` → lib **148 passed**（原 144：+`ui::layout` 2 条 +剪辑页/录制页实测各 1 条）、
  `src/main.rs` **1 passed**（新增整窗验证）、e2e **7 passed**、doc **1 passed / 1 ignored**；
* 整窗实测（`--nocapture`）：`外壳=126.0`；`760×625` 下录制页 `内容 402 / 视口 499 / 可用 499`，
  剪辑页 `内容 455 / 视口 455 / 可用 499`（**刚好放下、不需要滚动**）；
* 截图证据：`_rust/ui-shots/min-size-record.png`、`min-size-editor.png`、`min-size-settings.png`
  （`SetWindowPos` 把**客户区**调到 760×625 后整窗抓图，`shot-min.ps1`）；
* `scripts\regression.ps1` → **12 条全 PASS，exit 0**（`_rust\reg-min-ui-run6.txt`，未改脚本）。

**⚠️ 关于回归的一次如实记录**：本轮先后跑了 6 次回归，其中 4 次在第 3/4 步 FAIL，
原因**不是本次改动**，而是**录制侧一个既有的"尾部片段丢失"抖动**：
`reg1.mp4`（5 秒、`--gop 1`）有时只写出 759 ms / 2013 ms / 2772 ms / 3564 ms 的媒体，
于是整条片子只有 1~3 个关键帧（关键帧间隔 1.4 s 时 5 秒本该有 4~5 个），
而回归脚本第 3 步的选点算法在"关键帧 < 4"时会退化成空区间
（`[k1, k_{n-2}]` 在 n=3 时就是 `[k1, k1]`）→ `CLIP_RESULT=FAIL`。

依据（都记在 `_rust\reg-min-ui-run*.txt`）：
* **失败路径与本次改动无关**：第 3/4 步只跑 `recorder_smoke.exe` / `clip_smoke.exe` / ffprobe，
  **从不启动 GUI**；本次改动只动 `src/ui/**`、`src/main.rs` 与文档，录制/编码/封装/剪辑算法一行未改；
* 失败时刻的**编码器计数是正常的**（例：run3 在 t=4.2s 已编 90 帧、3 个关键帧），
  但落盘的分片只有 759 ms —— 是**写盘/刷盘尾部丢失**，不是编码失败；
* 直接连续跑 3 次 `recorder_smoke.exe 5 x.mp4 --gop 1`（前台、机器空闲）→ 媒体时长
  **4983 / 4983 / 4950 ms**，全部健康；
* 同一份**未改动的脚本**在第 4 次（`-SkipBuild`）与第 6 次（普通调用）都是 **12/12 PASS / exit 0**。

> ❌ **上面这段"写盘/刷盘尾部丢失"的根因判定是错的，已在 §11.24 用逐轮实测推翻**：
> 短的那几十轮里**流文件与导出产物时长完全相等**（差 0 ms），而且**媒体时长贴着录制窗口
> （末帧 pts 一起贴着）**——尾部一个字节都没丢。真正丢的是**头部**：
> `Recorder::start()` 的会话就绪耗时（机器很忙时实测 0.31~6.0 s）被算进了"录 5 秒"的预算。
> 下面这些观测仍然成立，只是解释要按 §11.24 改。

结论（**已被 §11.24 取代，保留原文以留痕**）：这是本机 DXGI 采集/分片刷盘在"机器刚跑过 cargo build、负载未落定"时的既有抖动，
本次 UI 改动既未引入也无法修（修它要动录制的写盘路径，属于明确禁区）。

---

## 11.24 "录制产物偶发严重缩短"的根因定位与修复（本轮）

### 11.24.1 现象与入口

`recorder_smoke 5 <out>.mp4 --gop 1`（回归第 1 步）在**机器很忙**时（后台编译 FFmpeg、磁盘打满）
会产出 **759 / 2013 / 2772 / 3564 ms** 的媒体、关键帧只有 1~3 个；前台空闲时连续 3 次是
**4983 / 4983 / 4950 ms**（健康）。失败时编码器计数"看起来正常"（t=4.2 s 已编约 90 帧 ≈ 30 fps）。

### 11.24.2 判定方法（先量再改）

要区分**捕获侧丢尾**与**重封装静默截断**，必须把三个量并排：

| 量 | 来源 | 含义 |
|---|---|---|
| `REC_STREAM_MS` | `mux::probe`(Cache 流文件) | 分片 MP4 自己的时长（捕获+封装的产物） |
| `REC_EXPORT_MS` | `mux::probe`(导出产物) | 重封装后的时长 |
| `REC_WALL_MS` | `recorder_smoke` 的墙钟 | **真实录制窗口**（会话就绪 → 停止就绪） |

再加两个**方向性**证据：`REC_START_MS`（会话就绪耗时，头部空档）与
`REC_LAST_PTS_MS` / `REC_TAIL_GAP_MS`（末帧 pts 与录制窗口的差，尾部空档）。
`remux_range` 的拷贝循环也补了逐包对账：`packets_read/written`、`first/last_pts_ms`、
`read_errors`/`read_retries`/`write_errors`/`exited_early`/`exit_reason`
（`mux::RemuxReport`，`remux_to_plain_verified` 会把它带回 `RecordingResult::export`）。

### 11.24.3 实测：受控负载（18 个 CPU 燃烧进程 + 1 路 400 MB 覆盖写，逻辑核 20）

**负载下 20 轮（旧行为，只加诊断、不改计时起点）** —— 明细 `_rust\tmp\repro-before2\summary.csv`：

| 轮次 | 会话就绪 ms | 录制窗口 ms | 流文件 ms | 产物 ms | 差 ms | 末帧 pts | 关键帧 | 帧数 | 退出码 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | **2053** | 2996 | 3037 | 3037 | 0 | 3003 | 3 | 90 | 0 |
| 2 | 520 | 4568 | 4621 | 4621 | 0 | 4587 | 5 | 138 | 0 |
| 3 | 321 | 4680 | 4720 | 4720 | 0 | 4686 | 5 | 141 | 0 |
| 4 | 437 | 4648 | 4687 | 4687 | 0 | 4653 | 5 | 140 | 0 |
| 5 | 313 | 4783 | 4852 | 4852 | 0 | 4818 | 5 | 144 | 0 |
| … | 313~520 | 4568~4783 | 4621~4852 | 同左 | **0** | ≈窗口 | 5 | 138~144 | 0 |

**同一负载下再跑 20 轮（更重的第一次对照，`_rust\tmp\repro-before\summary.csv`）**：
20 轮里 6 轮**根本没有产物**（`recorder_smoke` 退出码 1），14 轮成功但产物
2278~4687 ms（最短 2278 ms），**这 14 轮的"流文件 vs 产物"差全部是 0 ms**。
那 6 轮的日志给出了头部空档的直接读法：`采集会话已建立` → `视频编码器就绪` 用了
**5.25 s / 6.0 s**，而 `t0` 起 5 秒的预算已经用完 → 录制窗口为 0 → 0 帧 → 明确报错。

### 11.24.4 根因判定（有证据）

1. **不是重封装静默截断**：全部成功轮次里 `产物时长 == 流文件时长`（差 **0 ms**），
   `packets_read == packets_written`、`video_read == video_written`、`exit_reason=eof`、
   `read_errors=write_errors=0`。重封装是逐包 copy，忠实到 0 ms。
2. **不是捕获侧丢尾**：`末帧 pts ≈ 录制窗口`（如窗口 2996 / 末帧 3003；窗口 4783 / 末帧 4818），
   `REC_TAIL_GAP_MS` 只有 **+29~+62 ms**（正的、且只有一帧多的量）。尾部没有空档。
3. **真正丢的是头部**：`媒体时长 ≈ 5000 − 会话就绪耗时`。第 1 轮就绪 2053 ms → 媒体 3037 ms；
   空闲时就绪 ~185 ms → 媒体 5049 ms（§11.24.5 的同一台机器实测）。
   `Recorder::start()` 里的 **OpenH264 初始化**是最大的一块（实测 0.46 s ~ 6.0 s），
   这段时间**一帧画面都没有**，而 `recorder_smoke` 的循环**按 `t0`（准备开始的那一刻）计时**，
   于是"录 5 秒"被启动耗时吃掉了。

> 所以现象里的 759 / 2013 / 2772 / 3564 ms 对应的是"会话就绪耗时 4.2 / 3.0 / 2.2 / 1.4 s"——
> 数值上完全对得上，也和"t=4.2 s 已编 ~90 帧"（= 会话就绪约 1.2 s 后开始录，到 4.2 s 是 3.0 s ≈ 90 帧 @30fps）
> 这条观测自洽：**计数器从来没撒谎，被压缩的是它覆盖的那段窗口**。
> 回归第 3 步的 FAIL 是**脚本选点**放大了它（关键帧 3 个时选点退化成空区间），不是真截断。

### 11.24.5 修了什么

| 位置 | 改动 | 目的 |
|---|---|---|
| `src/bin/recorder_smoke.rs` | 循环预算**按录制窗口计时**（`t_loop` 起算，不再按 `t0`）；`--crash` 的 abort 同理 | "录 5 秒"就该真的录 5 秒。旧口径把会话就绪也算进预算，才让同一句命令产出 759 ms |
| `src/bin/recorder_smoke.rs` | 新增 `REC_START_MS` / `REC_LAST_PTS_MS` / `REC_STREAM_MS` / `REC_EXPORT_MS` / `REC_EXPORT_DELTA_MS` / `REC_TAIL_GAP_MS` / `REC_EXPORT_*`（13 个包级字段）/ `REC_EXPORT_OK` / `REC_EXPORT_ISSUE` | 让"捕获侧 vs 重封装侧"变成可判定；每轮都能对账 |
| `src/bin/recorder_smoke.rs` | **硬判定**：产物缺失、`\|产物−流文件\| > 300 ms`、或媒体比录制窗口短超过阈值（视频 300 ms / 纯音频 500 ms）→ **退出码 4 + 醒目报错** | **不允许"成功但明显短"** |
| `src/mux/mod.rs` | `remux_core`：读包失败**重试 3 次（20/100/250 ms 退避）**，仍失败 → `Err`；写包失败 → `Err`（不再 `warn` 后继续）；新增 `RemuxReport` 逐包对账 | 堵死"静默截断"的两个入口 |
| `src/mux/mod.rs` | 新增 `remux_to_plain_verified` + `verify_full_copy`（纯函数）：源时长/产物时长/包数三样对账，超过 `EXPORT_DURATION_TOLERANCE_MS = 300 ms`、或视频包写少了、或循环提前结束 → `Err` | 导出**要么完整，要么报错** |
| `src/recorder.rs` | 导出改走 `remux_to_plain_verified`；失败/不完整 → 保留 Cache 流文件 + `RecordingResult::export_issue` | 用户看得见"导出不完整，已保留 Cache 流文件" |
| `src/recorder.rs` | 收尾 drain 的写失败不再 `let _ =` 吞掉（逐条计数 → 判 `Failed`）；"写入器未就绪时被丢弃的包"计数告警 | 尾部少包从"无人知晓"变成"明确失败" |
| `src/recorder.rs` | 新增 `RecorderStats::last_video_pts_ms`、`RecordingResult::{export, export_issue, start_latency_ms, wall_ms}`、`Recorder::start_latency_ms()`；会话就绪 ≥ `START_LATENCY_WARN_MS = 1500 ms` 时 `tracing::warn` | 头部空档在**产品日志**里也留痕（`video_init_ms` / `audio_init_ms` / `start_latency_ms`） |
| `src/ui/main_page.rs` + `locales/*.json` | 导出不完整时在结果卡片上显示 `export_incomplete`（"导出不完整（已保留临时文件）：{原因}"） | 界面上"绝不静默" |

**没有改**：采集/编码算法、分片写盘核心（`FragmentedMp4Writer` 的写入路径）、`+faststart` 与 copy 语义、分辨率。

### 11.24.6 修复后的实测（同一台机器、同一负载）

受控负载下 20 轮（`_rust\tmp\repro-after\summary.csv`，18 个 CPU 燃烧 + 磁盘写）：

| 轮次 | 会话就绪 ms | 录制窗口 ms | 流文件 ms | 产物 ms | 差 ms | 末帧 pts | 关键帧 | 帧数 | 退出码 | 告警 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1015 | 5047 | 5083 | 5083 | 0 | 5049 | 5 | 137 | 0 | 无 |
| 2 | 303 | 5087 | 5182 | 5182 | 0 | 5148 | 5 | 140 | 0 | 无 |
| 3 | 270 | 5029 | 5116 | 5116 | 0 | 5082 | 5 | 148 | 0 | 无 |
| … | 270~1015 | 5023~5102 | 5050~5182 | 同左 | 0~6 | ≈窗口 | 5~6 | 137~154 | 全部 0 | **全部无** |
| 20 | 326 | 5062 | 5083 | 5083 | 0 | 5049 | 6 | 152 | 0 | 无 |

* 20/20 轮：**产物与流文件差 0~6 ms**（阈值 300 ms）、**媒体 5050~5182 ms**、**无告警**、退出码 0。
* 会话就绪耗时依旧会波动（270~1652 ms），**但它不再吃掉录制窗口**：负载下的回归第 1 步
  就绪 1652 ms，产物仍是 **5115 ms**；空闲那一次（紧跟 `cargo build --bins` 之后）就绪 **2290 ms**，
  产物仍是 **5049 ms** —— 同一个 2290 ms 在**旧口径**下会得到 `5049 − 2290 ≈ 2759 ms`，
  正是历史记录里的 **2772 ms**。这一条就是"头部空档"最直观的前后对照。
* `cargo build --all-targets` → 0 个 rustc 告警；`cargo test` → lib **151 passed**（原 148：新增
  `verify_full_copy_accepts_consistent_report`、`verify_full_copy_rejects_every_truncation_shape`、
  `remux_to_plain_verified_reports_consistent_copy`）、`src/main.rs` 1、e2e 7、doc 1/1 ignored。
* `scripts\regression.ps1`：**空闲 12/12 PASS exit 0**（`_rust\tmp\reg-idle.txt`）、
  **受控负载 12/12 PASS exit 0**（`_rust\tmp\reg-load.txt`，含第 0 步 `cargo build --bins`）。
  `reg1.mp4` 两次都是 **5050 / 5115 ms、6 个关键帧**（此前偶发 3465 ms / 3 个关键帧）。

### 11.24.7 未验证 / 残留

* **产品界面里"导出不完整"这条告警没有真人点过**：只在单元层面与烟雾测试层面验证了
  失败会走 `export_issue` 分支（`verify_full_copy` 的截断单测 + 该文案的 i18n key 对齐测试）；
  没有构造出"真实录制里导出被截断"的场景去截图（本轮的失败模式是头部空档，不是截断）。
* **`recorder_smoke 5` 现在固定录满 5 秒**，所以单次回归的墙钟会比以前长
  （= 会话就绪 + 5 s + 收尾）；这是"按窗口计时"的必然代价，不是退化。
* 会话就绪耗时**本身没有优化**（OpenH264 初始化 0.46~6.0 s 仍在）：本轮只让它**可见**
  （日志 + `start_latency_ms`）并**不再吃掉录制预算**。要缩短它需要改编码器初始化路径（禁区）。
  **→ 这一条已在 §11.25（下一轮）兑现：编码器初始化改成"启动后台预热 + 录制时取用"，
  按开始录制后不再等它。**

---


## 11.25 录制启动耗时拆解 + OpenH264 编码器预热（本轮）

§11.24 把"启动耗时不再吃掉录制窗口"解决了（计时口径 + `start_latency_ms` 可见），但
**启动耗时本身仍是用户能感觉到的等待**：按了「开始录制」，要过几百毫秒到几秒才真的开录。
本轮做两件事：**① 先把这段耗时按阶段拆开量**（先知道谁是大头，别凭直觉优化）；
**② 把大头搬到"用户还没按按钮"的时候去做**（后台预热 + 按缓存键取用）。

### 11.25.1 第 1 步：打点位置与测量方法

`Recorder::start` 里按**顺序**打点，落进新的公开结构 `recorder::StartPhases`
（`capture_open_ms` / `encoder_init_ms` / `audio_open_ms` / `aac_init_ms` / `total_ms` /
`encoder_prewarmed` / `first_frame_ms`），同时用 `tracing::info!` 打在**同一行**日志里
（`recorder_smoke` 的 INFO 订阅者会直接把它打到 stdout），另外
`Recorder::start_phases()` / `Recorder::first_frame_ms()` 可随读随新：

| 打点 | 边界（就是这几行） |
|---|---|
| `capture_open_ms` | `video::capture::Capture::open`（`resolve_monitor` + `region_to_crop` + `IDXGIOutputDuplication`） |
| `encoder_init_ms` | `VideoEncoder::new`（`WelsCreateSVCEncoder` + `GetDefaultParams` + `InitializeExt` + `SetOption(DATAFORMAT)`）；命中预热时≈0 |
| `audio_open_ms` | 音源解析（`devices::resolve_*`）+ `AudioCapture::open`（WASAPI） |
| `aac_init_ms` | `AacEncoder::new`（第一次会触发 FFmpeg 5 个 DLL 的延迟加载映射） |
| `total_ms` | `Recorder::start` 从**进入**到**返回句柄** = `start_latency_ms`（= `recorder_smoke` 的 `REC_START_MS`） |
| `first_frame_ms` | 从 `Recorder::start` 入口到**第一帧被编码完成**（视频线程写 `AtomicU64`）|

* **测量者**：`src/recorder.rs` 的 `#[ignore]` 真机探针 `start_latency_phase_breakdown`
  （`cargo test --lib start_latency_phase_breakdown -- --ignored --nocapture`）。
  它真的开 DXGI 会话、真的编帧、真的导出，每轮只录 ~0.2 s，测完即删临时目录。
* 每个条件**各 5 轮**，前 5 轮"预热前"（缓存空、每次同步 `InitializeExt`），
  后 5 轮"预热后"（每轮开始前先后台预热并等它就绪，再按开始）。
* 测表**不含**第一轮"付一次性成本"的数据（首次 FFmpeg DLL 延迟加载映射 0.76~2.47 s，
  见下面的注）；那一轮单独打印出来。配置：1920×1080 @30fps 8000 kbps，画面+系统声音。

### 11.25.2 实测表（全部毫秒；每格是 5 轮平均）

**条件 A：空闲**

| 阶段 | 采集会话 | **OpenH264** | 音源 | AAC | **total(=REC_START_MS)** | first_frame |
|---|---:|---:|---:|---:|---:|---:|
| 预热前 | 38 | **77** | 19 | 0 | **137** | 157 |
| 预热后 | 38 | **0** | 17 | 0 | **57** | 78 |

**条件 B：受控负载 = 18 个 CPU 燃烧进程（逻辑核 20；实测 17.6~17.8/20 核在忙）**

| 阶段 | 采集会话 | **OpenH264** | 音源 | AAC | **total** | first_frame |
|---|---:|---:|---:|---:|---:|---:|
| 预热前 | 72 | **82** | 53 | 0 | **210** | 252 |
| 预热后 | 69 | **0** | 45 | 0 | **117** | 157 |

**条件 C：重负载 = 40 个 CPU 燃烧进程（2× 超配）+ 2 路 400 MB 覆盖写（19.5/20 核在忙）**

| 阶段 | 采集会话 | **OpenH264** | 音源 | AAC | **total** | first_frame |
|---|---:|---:|---:|---:|---:|---:|
| 预热前 | 105 | **240**（单轮最高 **668**） | 141 | 0 | **488**（最高 828） | 576 |
| 预热后 | 102 | **0** | 224（最高 549） | 0 | **328**（最高 652） | 471 |

明细（条件 C，"预热前" 5 轮）：`encoder = 159 / 668 / 121 / 108 / 145`，
`total = 485 / 828 / 394 / 237 / 500`；"预热后" 5 轮：`total = 304 / 150 / 652 / 249 / 287`
（后者的抖动全部来自 `audio_open_ms`，与编码器无关）。

**不计入测表的第一轮（付掉一次性成本）**：

| 条件 | 采集会话 | OpenH264 | 音源 | **AAC（=FFmpeg DLL 首次映射）** | total |
|---|---:|---:|---:|---:|---:|
| 空闲 | 68 | 96 | 24 | **760** | 950 |
| 18 燃烧 | 140 | 208 | 59 | **1804** | 2213 |
| 40 燃烧+2 写 | 176 | 808 | 214 | **1316** | 2516 |

### 11.25.3 结论：谁是大头

1. **`AAC`（其实是 FFmpeg 5 个 DLL 的首次延迟加载映射）是一次性成本，但它是新进程里最大的单项**
   （0.76~2.47 s，占首轮 total 的 52%~82%）。它**已经**被 `ui::startup::preload_ffmpeg_dlls_once`
   搬到"枚举完成、首帧之后"的后台线程（§11.19.4），所以产品路径上不该再看到它。
   → 本轮的探针因此把它单独列出并**排除在对比之外**（否则"预热前"那一组会被它带偏，
   看起来像"预热省了 1.4 秒"，那是假的）。
2. **在"每次录制都要付"的阶段里，OpenH264 初始化是最大的那一块**：
   空闲 77 ms（占 total 的 56%）、18 燃烧 82 ms（39%）、重负载 240 ms（49%，单轮最高 668 ms）。
   采集会话建立 38~105 ms 次之，音源打开 17~224 ms 波动最大（取决于 WASAPI 当时的状态）。
   → **所以第 2 步的重点是编码器**，与历史记录里"OpenH264 初始化 0.46~6.0 s"的归因一致。
3. 采集会话（DXGI）确实也不小（38~105 ms），但**没有预热它**（理由见 §11.25.6）。

> **如实说明**：本轮用 18 个纯 CPU 燃烧进程，以及 40 进程 + 2 路磁盘覆盖写，
> **都没有复现出历史日志里的 5.25 s / 6.0 s**（探针里 OpenH264 最高 808 ms）。
> 历史那两个数字出现在"后台全量编译 FFmpeg + 磁盘打满"的机器上（§11.24.3），
> 属于更极端的内存/IO 压力组合。本轮的结论不依赖那个绝对量级：
> **预热把这一段的耗时在 15 个"预热后"轮次里全部变成 0 ms**，
> 所以它在更忙的机器上是多少，就从"按开始录制"里省掉多少。

### 11.25.4 预热方案（实现）

位置：`src/video/encode.rs` 的"编码器预热"一节 + `src/ui/startup.rs::prewarm_encoder_once`
+ `src/ui/main_page.rs` 的**同一个时机**（显示器/音频枚举完成、FFmpeg DLL 预加载那一帧）
+ `src/recorder.rs` 的取用点。

| 关注点 | 做法 |
|---|---|
| **缓存键** | `EncoderKey` = **整份 `EncoderConfig`**（逐字段结构相等）+ 录制侧整数帧率。`EncoderConfig` 里装着所有会改变码流/编码器状态的参数（宽高、帧率、目标与上限码率、usage、复杂度、RC、IDR 周期、CABAC、线程、slice 模式与片数、空间/时间层数、profile/level、帧跳过、prefix NAL、simulcast），所以改分辨率/帧率/码率/CABAC/关键帧间隔**必然不命中** |
| **尺寸从哪来** | `planned_encoder_dims(cfg)`：用 `capture::{monitor_rect, region_to_crop}`（与 `Capture::open` **同一套纯函数**）算出"采集会话会给出的裁剪尺寸"，**不创建任何 DXGI 会话** |
| **触发时机** | ① 枚举完成 + 首帧之后那一帧（与 FFmpeg 预加载同处）；② 用户改了配置（`cfg_changed`）且不在录制中；③ **每次录制停止之后**（`stop_recording` 末尾）—— 录制会把实例**取走**，不补一次的话"紧接着录第二段"又会回到同步初始化；④ 同键重复调用是**空操作**（一次互斥锁 + 一次 `GetMonitorInfoW`） |
| **不阻塞 UI** | 预热跑在后台线程 `oh264-prewarm`；UI 只做"算键 + 认领一次"（微秒级） |
| **实例独占** | `take_prewarmed_encoder` 命中即 `Option::take()`——**取走即从缓存移除**，同一个编码器实例绝不被两次录制共用 |
| **键不匹配 / 已过期** | 取用时键不等（或闲置 > `PREWARM_TTL = 300 s`）→ 丢弃旧实例并返回 `None` → `Recorder::start` 走**原来的同步 `VideoEncoder::new`**（即预热前的行为） |
| **失败回退** | DLL 缺失/参数非法/显示器几何算不出来/线程起不来/同一个键失败过：**只记 warning（+`cap.log` 一行）**，绝不 panic、绝不阻止录制、也不每帧重试（失败的键记住不重试，换配置即重试） |
| **并发** | 请求带 `generation` 代数：只有**最新一代**的预热结果会被写进缓存，配置连续改动时不会留下旧实例 |
| **参数零差异** | `VideoEncoder::new` 与 `VideoEncoder::from_key` 走**同一个** `initialize(EncoderConfig, w, h, fps)`；预热实例与同步实例参数逐字段相同 |
| **为什么需要 TTL** | 编码器实例在 1080p 下占用可观内存与内部线程；闲置 5 分钟后丢弃重建，避免"挂着不用" |

### 11.25.5 预热前后对比（产品口径）

| 条件 | `start_latency_ms`（= `REC_START_MS` 同一区间） | "按开始录制 → 第一帧被编码" |
|---|---|---|
| 空闲 预热前 → 预热后 | 137 → **57 ms**（−80） | 157 → **78 ms**（−79） |
| 18 燃烧 预热前 → 预热后 | 210 → **117 ms**（−93） | 252 → **157 ms**（−95） |
| 40 燃烧 + 2 写 预热前 → 预热后 | 488 → **328 ms**（−160） | 576 → **471 ms**（−105） |

* `encoder_init_ms`：**15/15 个"预热后"轮次全部是 0 ms**（预热前 77 / 82 / 240 ms）。
* 预热本身的成本（后台线程，不占 UI 帧）：空闲 69 ms、18 燃烧 75 ms、重负载 103 ms
  （重负载下有一轮是 808 ms —— 那也发生在后台、且发生在用户按按钮之前）。
* 预热后剩下的 `total` 就是**采集会话 + 音源打开**，两者本轮都不优化（见 §11.25.6）。

### 11.25.6 为什么**没有**预热 DXGI 采集会话

`Capture::open` 建立的 `IDXGIOutputDuplication` 是**有生命周期的活动会话**：系统对同一输出的
并发复制会话有限额，而且切分辨率、独占全屏、UAC 桌面切换都会让它 `AccessLost` 需要
`recreate()`。把它长时间挂在缓存里，等于让一个"随时可能失效、还占着系统额度"的会话跨越
用户的待机期——风险明显高于收益。反过来，OpenH264 的编码器实例是**纯内存对象**，
没有系统级额度、不持有外部资源，挂多久都安全。所以本轮**只预热编码器**；
采集会话仍在 `Recorder::start` 里按现状同步建立（也正因为如此，预热后才剩下它的耗时）。

### 11.25.7 新增单测（纯逻辑，不需要 `openh264.dll`）

放在 `src/video/encode.rs` 的 `mod tests`：

| 测试名 | 断言什么 |
|---|---|
| `prewarm_key_hits_only_when_every_parameter_matches` | 同参数**命中**；改 fps / 码率 / 分辨率 / CABAC / 关键帧间隔**都不命中**（并校验键里读出的宽高/帧率/码率/IDR 周期） |
| `prewarm_cache_take_is_exclusive_and_drops_stale_entries` | **取走即移除**（同一键第二次取是 `None`）；键不匹配时丢弃旧实例；闲置超过 `PREWARM_TTL` 判过期；**旧一代结果不得覆盖最新一代** |
| `prewarm_failure_falls_back_without_panic` | 算不出尺寸（选区在显示器外）与纯音频 → 只 `Skipped`；空缓存取实例 → `None`（回退同步创建）；**失败过的键不重试**（`FailedBefore`） |
| `planned_dims_follow_the_same_pure_geometry_as_capture` | 预热用的宽高与 `capture::region_to_crop` 的换算**同源**（全屏=整屏、区域=取偶后的选区）—— 失配就会导致"永远不命中" |

缓存本体写成泛型 `PrewarmSlot<T>` / `PrewarmState<T>`，所以上面 3 条缓存测试可以用 `T = ()`
/ `T = &str` 跑**纯逻辑**，不需要加载 DLL、不需要显示器。

### 11.25.8 正确性：产物与预热前等价

* **参数来源唯一**：预热与同步创建都走 `VideoEncoder::initialize(build_encoder_config(cfg, w, h), …)`，
  传进 `InitializeExt` 的 `SEncParamExt` 是同一份 → 分辨率 / 帧率 / 码率 / 关键帧间隔 /
  编解码器（OpenH264, CABAC, `CameraVideoRealTime` + `FixedSlcNumSlice` + auto 线程）**完全一致**；
* **实例状态等价**：预热实例 `frame_index = 0`、`sps_pps = None`、`total_bytes = 0`，
  与刚 `new` 出来的实例逐字段相同（首帧的 `ts_ms` 都是 0）；
* **端到端**：`cargo test`（lib 155 passed + 1 ignored）、`scripts\regression.ps1`
  空闲 **12/12** + 受控负载 **12/12** 全 PASS（见 §11.25.9），其中第 1/3 步会 ffprobe 回读
  分辨率、时长、关键帧数、h264+aac 双流，剪辑步还断言 **bitstream copy、分辨率一致**。

### 11.25.9 本轮验证记录

**构建 / 测试**

| 命令 | 结果 |
|---|---|
| `cargo build --all-targets` | 成功，**零 rustc 告警**（仅有两条 `build.rs` 的 `cargo:warning` 提示，与基线一致） |
| `cargo test` | lib **155 passed / 0 failed / 1 ignored**（共 156 条；基线 151 → 新增 4 条纯逻辑单测 + 1 条 `#[ignore]` 真机探针）、`src/main.rs` **1 passed**、`tests\e2e.rs` **7 passed**、doc-tests **1 passed / 1 ignored** |
| 真机探针 | `cargo test --lib start_latency_phase_breakdown -- --ignored --nocapture` 三种条件各 5+5 轮全过（数据见 §11.25.2 / §11.25.5） |

**两套端到端回归**（`scripts\regression.ps1`，用 `powershell` 5.1 跑）

| 条件 | 结果 | 记录 |
|---|---|---|
| 空闲 | **12/12 PASS，exit 0** | `_rust\tmp\reg-idle.txt` |
| 受控负载（18 个 CPU 燃烧进程，实测 17.9/20 核在忙） | **12/12 PASS，exit 0** | `_rust\tmp\reg-load.txt` |

两次都覆盖：导出 MP4 就绪 + Cache 流文件保留 + h264/aac 双流（第 1 步）、崩溃后分片可解（第 2 步）、
copy 剪辑分辨率一致/时长差 11~27 ms/解码干净（第 3 步）、麦克风 48 kHz 时长对齐（第 5 步）、
系统声音 RMS 0.42（第 7 步）、音源全关时归一为系统声音（第 8 步）。
**录制路径的产物与预热前逐项一致**（分辨率 1920×1080、时长对齐录制窗口、关键帧数、双流、bitstream copy）。

**产品路径（GUI）实证**（发布目录里的 `CapLite.exe`，`--config-dir` 指向空目录，release 无控制台）

`cap.log` 里的启动时间轴（三次启动都一样，下面是最后一次）：

```text
[cap][t=266ms] phase=ui_first_frame_done scope=frame dur=4.7ms
[cap][t=326ms] phase=deferred_enum_done scope=app dur=1.2ms extra="显示器 2 台（0ms）/ 本次目录探测 已执行"
[cap][t=331ms] phase=enum_audio_done scope=app dur=3.5ms extra="麦克风 1 个"
[cap] OpenH264 编码器预热已启动（后台线程；按下开始录制时直接复用）
[cap][t=416ms] phase=ffmpeg_preload_done scope=app dur=83.9ms extra="5/5 成功"
[cap] OpenH264 编码器预热完成：1920x1080 @30fps 5000kbps，后台耗时 148ms（按下开始录制时直接复用）
```

* 预热**确实发生在"枚举完成 + 首帧之后"**（t≈331 ms），与 FFmpeg DLL 预加载同一时机；
  148 ms 后（后台线程）实例就绪 —— 此时用户还在看界面，**远早于任何一次"按下开始录制"**；
  （另两次启动同样如此：t≈323 ms 触发 / 139 ms 完成、t≈418 ms 完成，各次差异就是当时的机器忙闲。）
* 子进程 stderr 里能看到 OpenH264 在 `InitializeExt` 期间输出的两条库警告
  （`SliceArgumentValidationFixedSliceMode` / `bEnableFrameSkip`）—— 这就是"编码器真在后台被
  创建过"的旁证（启动阶段只有预热线程会创建编码器）；
* 新增的 per-recording 日志行（每次录制都写 `cap.log`，GUI 里也能看见）：
  `[cap] 录制已启动：会话就绪 Nms（DXGI 采集 Ams / OpenH264 Bms，命中预热 / 音源 Cms / AAC Dms）；第一帧被编码 Ems`。
  加它的原因：**产品（GUI）根本没有安装 tracing 订阅者**，`tracing::info!/warn!` 在用户机器上
  全部丢弃 —— 所以预热的"完成/失败"与每次录制的阶段耗时都改成走启动日志落点（`cap.log`），
  否则"预热到底成没成"在用户机器上完全看不见。**这一条是本次顺带修掉的观测缺口，不是新功能。**

**发布包（13 个文件）**

| # | 文件 | 字节 | 来源 |
|---|---|---:|---|
| 1 | `CapLite.exe`（已签名） | 15 916 432 | `cargo build --release --bin CapLite` + `scripts\dev-sign.ps1` |
| 2–6 | `avcodec-63.dll` / `avformat-63.dll` / `avutil-61.dll` / `swresample-7.dll` / `swscale-10.dll` | 2 087 424 / 607 232 / 686 080 / 106 496 / 920 576 | `CapLite\ffmpeg-min\bin` |
| 7–8 | `openh264.dll` / `openh264-7.dll` | 823 296 ×2 | 构建产物目录（SHA256 相同 = `EB0B38B8…`，与 `native\openh264.dll` 一致） |
| 9 | `LICENSE` | 1 077 | 仓库根 `LICENSE`（MIT） |
| 10–11 | `LICENSE-FFmpeg.txt` / `LICENSE-OpenH264.txt` | 7 651 / 1 317 | FFmpeg 9.0.2 的 LGPLv3 全文 / OpenH264 的 BSD-2 全文 |
| 12 | `THIRD-PARTY-NOTICES.txt` | 4 062 | 第三方组件声明 |
| 13 | `README.txt` | 7 758 | 英文使用说明（仓库根 `README.md` 同内容） |

* `dist\CapLite-1.0.0\` = **21 992 697 B（20.97 MB）**，**恰好 13 个文件**（无多余文件）；
* `dist\CapLite-1.0.0-win64.zip` = **9 748 079 B（9.30 MB）**，13 个条目，根目录名 `CapLite-1.0.0/`；
* 解压到临时目录后逐文件核对 SHA256：**13/13 一致、0 不一致、0 缺失**；解压出的 `CapLite.exe`
  回读签名仍 `Status=Valid`、`VersionInfo` 仍 `1.0.0 / CapLite`、备用数据流只有 `:$DATA`（无 MotW）；
  核对完临时目录已删除；
* `SHA256(CapLite-1.0.0-win64.zip)` = `396B6FEDD2CC3FC4E1A6FC30EF9D4CAC4595437766B5361692708B08E9251D17`；
* `SHA256(CapLite.exe)`（发布目录内已签名版本）= `1EF0778892094870C76306CEBA182D7E055041A3A01B9ECBD2A66DFF260EEF99`；
* 签名：`Status=Valid`、`SignatureType=Authenticode`、`MotW=无`；
  签名者仍是本机既有的 `CN=cap-rust dev signing`（thumb `22DACD13…`）——原因见 §11.22.5
  （会话进程不能写用户证书库，无法新建 `CN=CapLite dev signing`；`dev-sign.ps1` 走的是
  显式兼容分支，链仍可信）；
* `VersionInfo`：`FileVersion=1.0.0`、`ProductVersion=1.0.0`、`ProductName=CapLite`、
  `FileDescription=CapLite screen recorder`、`OriginalFilename=CapLite.exe`、
  `LegalCopyright=Copyright (c) 2026 CapLite`。

### 11.25.10 未验证 / 残留

* **"按开始录制 → 首帧"没有真人点过按钮**：预热命中路径是在探针里按"后台预热完成 → 再
  `Recorder::start`"复现的（`encoder_init_ms = 0` 15/15 轮），GUI 侧只验证到"预热在启动 323 ms
  时触发、139 ms 后完成并写进 `cap.log`"。**UI 里点一次"开始录制"并肉眼确认不再等待**属于人工步骤。
* `REC_START_MS`（`recorder_smoke` 自报的那个数）**本身不会因为预热而变化**：`recorder_smoke`
  不经过 UI，也就没有任何时机去预热（它一次进程只录一次）。所以本轮的对比用探针里**同一口径**
  的 `t0 → Recorder::start 返回`（§11.25.5 的 `rec_start` 列，与 `REC_START_MS` 数值逐轮相同，
  差 ≤1 ms）。**没有改 `src/bin/recorder_smoke.rs`**：它不在本轮的允许改动边界内。
* **没有复现历史 5.25 s / 6.0 s 的编码器初始化**（18 燃烧最高 87 ms；40 燃烧 + 2 路覆盖写最高
  808 ms）。历史数字来自"后台全量编译 FFmpeg + 磁盘打满"的极端组合（§11.24.3），本轮没有构造
  同等级的内存压力。结论不依赖绝对量级：预热把该阶段在 15/15 轮里变成 0 ms。
* **预热实例闲置 5 分钟（`PREWARM_TTL`）后被丢弃**这条只由单测覆盖（构造过期时刻），
  没有在真机上"挂着 5 分钟再录"跑过。
* 预热后仍剩 `capture_open_ms`（38~105 ms）与 `audio_open_ms`（17~224 ms）：前者按 §11.25.6
  的理由**故意不预热**，后者是 WASAPI 打开设备，本轮未优化。

## 11.26 窗口图标（标题栏左上角 + 任务栏）——**PE 资源图标 ≠ 窗口图标**（本轮）

### 11.26.1 症状与根因

§11.21/§11.22 之后 exe 的**文件图标**（资源管理器 / 文件属性里那个）已经是蓝色 "Cap"：
`build.rs` 把 `assets/caplite.ico` 交给 `rc.exe` 编成 `.res` 再用 `cargo:rustc-link-arg` 链接进
**PE 资源节**（回读 7/7 尺寸全部在，见 §11.22.2）。但**运行起来之后**，窗口标题栏左上角与
任务栏按钮上仍是**默认图标**。

根因不是资源没嵌进去，而是**这是两个完全不同的东西**：

| | PE 资源图标（文件图标） | 窗口图标（标题栏 / 任务栏） |
|---|---|---|
| 谁画 | shell（资源管理器、文件属性） | DWM，按窗口的 `ICON_SMALL`/`ICON_BIG` 属性 |
| 从哪来 | 读 exe 的 `RT_GROUP_ICON`（序号 1） | 进程运行时用 `WM_SETICON` 设到 HWND 上 |
| 不设会怎样 | —— | 显示**窗口库的默认图标**（eframe 是它自带的 egui logo） |

`src/main.rs` 在构造 `NativeOptions` 时**没有**调用 `ViewportBuilder::with_icon(...)`，
于是 eframe 走了它的兜底分支：

```rust
// eframe-0.36.2/src/native/epi_integration.rs:205
let icon = native_options.viewport.icon.clone()
    .unwrap_or_else(|| std::sync::Arc::new(load_default_egui_icon()));   // data/icon.png
```

`data/icon.png` 就是 egui 的 logo —— 这就是"窗口/任务栏是默认图标"的由来。
**顺手核对的一件事**：这次改动**没有**碰 `build.rs` 的图标路径，release exe 的资源节回读
仍然是 7/7 尺寸、字节数与 §11.22.2 完全一致（见 §11.26.5 的对照表）。

### 11.26.2 做法：`scripts/make-icon.py` 导出裸 RGBA → `include_bytes!` → `with_icon`

**为什么导出裸 RGBA 而不是复用 PNG**：`egui::IconData` 的字段就是裸字节，PNG 还得走解码器
（要 `image` crate 的 PNG feature，且解码结果与本次构建之间多一层不确定性）。裸 RGBA 没有
解码步骤、也没有新依赖。

* `scripts/make-icon.py` 新增输出（**幂等**）：
  * `assets/caplite-256.rgba` = **262 144 B**（256×256×4，逐行 RGBA8、**非预乘**）
  * `assets/caplite-64.rgba` = 16 384 B（备选小尺寸，见下）
  脚本里由 `RGBA_SIZES = [256, 64]` + `write_rgba()` 产出；口径与 `egui` 的 `IconData.rgba`
  一致（`egui-0.36.2/src/viewport.rs`：`RGBA pixels, with separate/unmultiplied alpha`），
  实现就是 `img.convert("RGBA").tobytes()`（Pillow 对 RGBA 不做预乘）。
  **可重复运行**：ICO/PNG 已存在时主流程照旧跳过，但会检查 `*.rgba` 是否齐全，缺哪个补哪个
  （删掉 `caplite-64.rgba` 再跑一次会把它补回来，且 ICO/PNG 的 mtime 不变）。
  `--force` 重跑：`caplite.ico` 与 `caplite-256.png` 的 SHA256 **逐字节不变**
  （`70B1FED8…` / `BF6532D3…`，与 §11.22.2 相同）。
* `src/main.rs`：
  * 文件顶部的常量（`include_bytes!` 的相对路径是相对**当前源文件**，所以是 `../assets/…`）
    ```rust
    const WINDOW_ICON_RGBA: &[u8] = include_bytes!("../assets/caplite-256.rgba");
    const WINDOW_ICON_PX: u32 = 256;
    const _: () = assert!(WINDOW_ICON_RGBA.len() == (WINDOW_ICON_PX * WINDOW_ICON_PX * 4) as usize, …);
    ```
    那条 `const _: () = assert!(…)` 是**编译期**钉子：egui 不校验 `rgba.len()`，尺寸写错会
    静默画出错位的图，所以宁可让构建失败。
  * `NativeOptions` 的 viewport 末尾：
    ```rust
    .with_title(caplite::ui::theme::APP_NAME)
    .with_icon(eframe::egui::IconData {
        rgba: WINDOW_ICON_RGBA.to_vec(),
        width: WINDOW_ICON_PX,
        height: WINDOW_ICON_PX,
    }),
    ```
* **`IconData` 的实际用法（本机 API 实测，不是猜的）**：
  * `pub fn with_icon(mut self, icon: impl Into<Arc<IconData>>) -> Self`（`viewport.rs:434`）——
    签名收 `impl Into<Arc<IconData>>`，但 `From<IconData> for Arc<IconData>` 就是标准库的
    那个 reflexive `From<T> for Arc<T>`，所以**直接传 `IconData` 值即可，不需要自己包 `Arc`**。
  * 字段是 `rgba: Vec<u8>` / `width: u32` / `height: u32`（`viewport.rs:183-192`），
    `rgba` **非预乘**，宽高文档建议 4 的倍数（256 ✔）。
  * Windows 上 eframe **不**用 winit 的 `set_window_icon`（它只能设 `ICON_SMALL`），而是自己走
    `native/app_icon.rs`：把 `IconData` 缩到 `SM_CXICON`(32) / `SM_CXSMICON`(16) → 编码成 PNG →
    `CreateIconFromResourceEx` → **先 `ICON_BIG` 再 `ICON_SMALL`** 两次 `WM_SETICON`
    （注释里写明了顺序原因：反过来小图标会被"大图标缩放"糊掉）。

### 11.26.3 尺寸取舍：**最终选 256×256**

* 系统每次要的是 `SM_CXICON`=32 / `SM_CXSMICON`=16（本机 `GetSystemMetrics` 实测，100% DPI），
  任务栏高亮区 32 px、里面的图标实测 **24×24** —— **任何源尺寸都会被 eframe 重采样一次**，
  所以"给 64 就不会被缩放"并不成立。
* 256 的优点：与 `caplite-256.png` / ICO 的最大帧**同源同尺寸**，一眼能对上；缩小到 32/24
  （Lanczos3）时源里的"Lite"细节仍在，24×24 的任务栏图标能看清 "Cap" 与淡淡的 "Lite"。
* 64 只在"16×16 标题栏图标"上可能略有优势（256→16 是 16 倍缩小，Pillow 的 Lanczos 会把
  白色笔画拉成一团浅蓝）。**本轮没有采用**，但 `assets/caplite-64.rgba` 已经生成好，
  换它只需把 `WINDOW_ICON_PX` 改成 64、`include_bytes!` 换成 `caplite-64.rgba`。
* 客观话说清楚：16×16 的标题栏图标**本身就是糊的**（"Cap" 三个字母在 16 px 上只有约 11×6 px
  的墨迹），这是尺寸的物理下限，不是设置没生效。见 §11.26.4 的像素级对照。

### 11.26.4 运行时图标证据（截图 + 像素对照）

**采集脚本已经进仓库**：`scripts\icon-shots.ps1`（只写 `-OutDir`，不碰其它文件）：

```
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\icon-shots.ps1 `
    -Exe dist\CapLite-1.0.0\CapLite.exe -OutDir <目录>
```

它做的事（全部只用 user32/gdi32 + .NET，不装东西）：
`ProcessStartInfo` + `UseShellExecute=$false` 启动 exe（`--config-dir` 指向临时空目录）→
按 PID `EnumWindows` 找**可见且带非客户区**的顶层窗口（`MainWindowHandle` 会给回 winit 的
消息窗口 `Winit Thread Event Target`，不能用）→ 摆到 (40,40) →
`PrintWindow(..., PW_RENDERFULLCONTENT=2)` 抓整窗（**不需要抢前台**：本会话是 Low 完整性，
`SetForegroundWindow` 有时被拒，`CopyFromScreen` 就会抓到盖在上面的别的窗口，实测抓到过一次
PowerShell 自己）→ 在标题栏找**最右边那段 ≥6 px 宽的蓝色列区间**定位图标（最左那段可能属于
紧挨着我们的**邻居窗口**）→ 裁 16×16；
任务栏用 `Shell_TrayWnd` 的 rect 抓"启动前 / 启动后"两条，**差集按列切成列段后取最高的一段**
（差集里不止有我们的按钮：时钟跳分钟、通知区图标闪动都会变，所以不能直接用整块包围盒），
再裁按钮。
一次成功运行（已签名 release exe）的输出：标题栏蓝色列区间 `16-31`（宽 16 = `SM_CXSMICON`）、
图标窗口内偏移 `(16,6)`；任务栏差集 **580 px**、列段 `x[989..1012]`（宽 24、最高 27 px）——
与下面表格里的数字一致。

**看图结论（`_rust\ui-shots\`，都是真实像素的最近邻放大）**：

| 文件 | 内容 | 观察 |
|---|---|---|
| `icon-evidence-overview.png` | A 标题栏 16×16（12.5×）、B 任务栏 24×24（10×）、C 整条标题栏左端（3×） | 三张都清楚看到**蓝色圆角方块 + 白色 "Cap"**；C 里它就在 "CapLite" 文字左边；**没有任何 egui logo 的痕迹** |
| `icon-titlebar.png` / `-x10.png` | 标题栏图标 16×16 原图 / 10× | 蓝色底、白色 "Cap" 可辨（16 px 下的抗锯齿让笔画连成一片，属正常） |
| `icon-taskbar-aligned.png` / `-x10.png` | 任务栏按钮里的图标 24×24 / 10× | 与设计稿几乎一致：**"Cap" 笔画清晰，右下角有淡淡的 "Lite"** |
| `icon-titlebar-release.png` / `-x10.png` | **已签名 release exe** 的标题栏图标 16×16 / 10×（`PrintWindow` 抓的） | 同一张图：蓝色圆角方块 + 白色 "Cap" |
| `icon-titlebar-compare.png` / `icon-taskbar-compare.png` | 左=实拍、右=用 `caplite-256.png` 按 LANCZOS 重现的"应该长什么样" | 两张并排看不出实质差别 |

**机器可判的对照**（在候选图标里做穷举位移搜索，取最小平均绝对差 MAD，越小越像）：

| 比较对象 | 标题栏 16×16（debug） | 标题栏 16×16（**已签名 release**） | 任务栏 24×24（debug） |
|---|---:|---:|---:|
| **我们的 RGBA 源**（`caplite-256.rgba` 缩到该尺寸） | **8.37**/255 | **1.69**/255 | **4.72**/255 |
| PE 资源里的 ICO 帧（同美术、不同重采样路径） | 16.98/255 | 11.12/255 | 9.89/255 |
| eframe 的默认图标（egui logo，=`with_icon` 不调用时的兜底） | **116.96**/255 | **84.55**/255 | **75.19**/255 |

* 实拍像素与我们给的 RGBA 源几乎逐像素相同（release 那一路 MAD 只有 **1.69/255**，即平均每个
  通道差 1.7 个色阶），与 egui 默认图标差一个数量级 → **窗口/任务栏画的确实是新图标**。
* **release 那一路的抓法**（不需要抢前台）：本会话是 Low 完整性，`SetForegroundWindow` 有时
  会被系统拒绝，`CopyFromScreen` 就会抓到盖在上面的别的窗口（实测抓到过一次 PowerShell 自己）。
  改用 `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT=2)` 直接把窗口画进我们的 DC —— 这样不依赖
  前台状态，得到的正是"窗口自己的标题栏"（`PrintWindow` 参数 0 那次反而把桌面透出来了，故用 2）。
  生成物：`_rust\ui-shots\icon-titlebar-release.png`（+`-x10`）。
* 补充证据：`WM_GETICON`(`0x007F`) 在运行中的窗口上返回
  `ICON_SMALL=非 0`、`ICON_BIG=非 0`（debug 一轮实测 `0x198091D`/`0x13A90A8F`，
  已签名 release 一轮 `0x10B0811`/`0x1F10C47`；每次运行句柄不同、**非 0 才是判据**）
  —— 说明 `WM_SETICON` 真的被调用过。**它不能替代看图**（句柄非 0 也可能是别的图标），
  所以上面的图与像素对照才是主证据。
* **任务栏按钮的可见性**：本会话的 `explorer.exe` 不在本用户会话里，键盘注入写不进去
  （Low 完整性），所以"任务栏按钮"是靠**启动前后像素差**定位的：差集只出现在
  `x[989..1012] y[1044..1067]`（= 新按钮），进程退出后该差集**逐像素消失**（580 px 同样数量）
  → 这块像素确实属于 CapLite 的按钮，而不是别的常驻图标。

### 11.26.5 本轮验证记录

| 项 | 结果 |
|---|---|
| `cargo build --all-targets` | **成功，0 条 rustc 告警**（两条 `warning: caplite@1.0.0: …` 仍是 `build.rs` 的构建期提示：延迟加载 + 已嵌入图标资源） |
| `cargo test` | lib **155 passed / 1 ignored**（与基线一致，本轮**没有**新增/删除测试）、bin **1 passed**、e2e **7 passed**、doc **1 passed + 1 ignored** |
| `scripts\regression.ps1`（空闲） | **12/12 PASS，exit 0**（第 0b/0c/0d 步 + 第 1~8 步全 PASS；第 3 步 copy 导出的关键帧这次有 5 个，没撞上历史的"关键帧太少"偶发） |
| `cargo build --release --bin CapLite` | 成功、0 告警；未签名 `CapLite.exe` = 16 178 176 B（比改动前 15 916 432 B **+261 744 B** ≈ 内嵌的 262 144 B RGBA，符合预期） |
| PE 资源图标仍在 | `scripts\extract-icon.ps1` 回读 release exe：`RT_GROUP_ICON #1` 7 条目、**7/7 还原成功**，`BytesInRes` 与 §11.22.2 **逐项相同**（20934 / 9624 / 4267 / 2666 / 1635 / 1143 / 704），还原出的 PNG 与 debug 版**逐文件 SHA256 相同** |
| `assets\caplite.ico` | 41 091 B、SHA256 `70B1FED8…`（重跑脚本后**未变**） |
| 没有新增依赖 | `Cargo.toml` / `Cargo.lock` 未改动（`IconData` 来自已有的 `eframe`/`egui`） |

### 11.26.6 重打包（`dist\CapLite-1.0.0`，13 个文件不变）

| # | 文件 | 字节 | 来源 |
|---|---|---:|---|
| 1 | `CapLite.exe`（已签名，**含新窗口图标**） | 16 179 600 | `cargo build --release --bin CapLite` + `scripts\dev-sign.ps1` |
| 2–6 | `avcodec-63` / `avformat-63` / `avutil-61` / `swresample-7` / `swscale-10` | 2 087 424 / 607 232 / 686 080 / 106 496 / 920 576 | `CapLite\ffmpeg-min\bin` |
| 7–8 | `openh264.dll` / `openh264-7.dll` | 823 296 ×2 | `_rust\target-cap\release` |
| 9–13 | `LICENSE` / `LICENSE-FFmpeg.txt` / `LICENSE-OpenH264.txt` / `THIRD-PARTY-NOTICES.txt` / `README.txt` | 1 077 / 7 651 / 1 317 / 4 062 / 7 758 | 仓库根 `LICENSE`、FFmpeg LGPLv3 全文、OpenH264 BSD-2 全文、第三方声明、`README.md`（`README.txt` 与 `README.md` SHA256 相同） |

* `dist\CapLite-1.0.0\` = **22 255 865 B（21.22 MB）**，**恰好 13 个文件**（`dist\` 下清理后只有
  该目录与 zip）；
* `dist\CapLite-1.0.0-win64.zip` = **9 770 951 B（9.32 MB）**，**13 个条目**，根目录名 `CapLite-1.0.0/`；
* 解压到临时目录逐文件核对 SHA256：**13/13 一致、0 不一致、0 缺失、0 多余**；解压出的
  `CapLite.exe` 回读签名仍 `Status=Valid`、`VersionInfo` 仍 `1.0.0 / CapLite`、备用数据流只有
  `:$DATA`（无 MotW）；核对完临时目录已删除；
* `SHA256(CapLite-1.0.0-win64.zip)` = `701B718A0D7850A631161571D0AD4C87071DED3DB434239F2EB1F156B8EEBE6A`；
* `SHA256(CapLite.exe)`（发布目录内已签名版本）= `60CDBDE637159E7F38C128EAB794F6B8AD8926B5C35CC2DE912C786F661CEBBC`；
* 签名：`Status=Valid`、`SignatureType=Authenticode`、`MotW=无`（原因同 §11.22.5：会话进程写不了
  用户密钥库，`dev-sign.ps1` 复用本机既有 `CN=cap-rust dev signing`，thumb `22DACD13…`）；
* 证据图（都在 `_rust\ui-shots\`）：`icon-evidence-overview.png`、`icon-titlebar.png`
  （+`-x10/-x20`）、`icon-taskbar-aligned.png`（+`-x10`）、`icon-titlebar-compare.png`、
  `icon-taskbar-compare.png`。

**未验证项（本轮）**：没有在**深色主题**/高 DPI（125%/150%）下重复截图 —— 本机是 100% DPI +
系统浅色主题，`SM_CXICON`/`SM_CXSMICON` = 32/16 是实测值，高 DPI 下 eframe 会拿到更大的目标
尺寸（图标会更清楚，不会更糊），但没有实拍证据。任务栏"固定到任务栏"后的图标、
以及 Alt+Tab / 任务视图里的大图标同样没有单独截图（它们走同一套 `WM_SETICON` 属性）。

---

## 11.27 录制页状态行压低 + 计时器字号一档 + **逐部件高度表**（本轮）

### 11.27.1 需求

用户原话："录制页顶部「状态卡片」里那一行（"就绪" + 大号 `00:00` 时间码 + 右侧「开始录制」按钮）
压低一点，并把 `00:00` 计时器字号调小一档"；顺带要**录制页逐部件高度表**。
硬约束：**「开始录制」按钮尺寸不变**（主操作）；**时间码的等宽字体与对齐方式不变**
（避免录制中数字跳动）；状态文字与按钮**仍完整可见不裁切**；最小窗口下**不出现滚动条**、页面仍"充满"。

### 11.27.2 第 1 步：先量再改（全部 headless 真布局实测）

新增两条**一次性探针**（都是 `#[ignore]`，**不参与常规测试**，用
`cargo test --lib -- --ignored --nocapture` 跑）：

| 探针 | 作用 |
|---|---|
| `ui::main_page::tests::dump_record_page_heights` | 打印**逐部件高度表**（中英各一份），数据源是 `MainPage` 每帧实记的 `RecordProbe`（`#[cfg(test)]` 字段，发布构建里不存在） |
| `ui::main_page::tests::probe_timer_font_heights` | 扫描候选字号 → 实测行高/宽度（选字号靠它，不靠估） |
| `ui::main_page::tests::calibrate_scroll_content_size` | 标定 `ScrollArea::content_size.y` 的口径（解释"合计"的差额从哪来） |

**改前逐部件高度表**（视口 760×499 = 最小窗口 760×625 扣掉外壳 126；中英**完全相同**）：

| 部件 | 改前 | 改后 | 差 |
|---|---|---|---|
| ① 状态卡片（整卡） | 62.0 | **56.0** | −6.0 |
| ↳ 「就绪 + 时间码」那一行 | **40.0** | **34.0** | **−6.0** |
| ↳ 状态点 / 状态文字 | 10.0 / 18.0 | 10.0 / 18.0 | 0 |
| ↳ **时间码文本自身** | **40.0** | **34.0** | −6.0 |
| ↳ 右侧「开始录制」按钮 | 34.0 高 × 132.0 宽 | 34.0 高 × 132.0 宽 | **0（未动）** |
| ↳ 行内撑高余量（行高 − 最高部件） | 0.0 | 0.0 | 0 |
| ② 采集卡片（整卡） | 99.0 | 99.0 | 0 |
| ↳ 「录制范围」行 / 行距 / 「显示器 + 信息」行 | 30.0 / 9.0 / 30.0 | 同 | 0 |
| ③ 录制内容卡片（整卡） | 125.0 | 125.0 | 0 |
| ↳ 「录制内容」行 / 行距 / 「音源」小标题 / 小标题行距 / 复选框行 | 30.0 / 9.0 / 16.0 / 2.0 / 30.0 | 同 | 0 |
| ④ 存储位置卡片（整卡） / ↳ 存储位置行 | 52.0 / 30.0 | 52.0 / 30.0 | 0 |
| 卡片间距 ×3（`GAP_CARD_TIGHT` = 8.0） | 24.0 | 24.0 | 0 |
| 卡片内边距（`PAD_CARD_TIGHT` = 10.0，4 张 ×2） | 80.0 | 80.0 | 0 |
| 页面首留白 / 尾留白 | 0.0 / 97.0 | 0.0 / **103.0** | +6.0 |
| 滚动区口径差额（见下） | 40.0 | 40.0 | 0 |
| **合计（= `RECORD_REQ_H`）** | **402.0** | **396.0** | **−6.0** |

**这一行的高度由什么决定（判定，不是猜）**：行高 40 = **时间码自己的行高**，
而右侧按钮只有 34 —— 也就是说 34 号字号把行撑高了 6px、按钮被"架空"。
所以"压低这一行"的**唯一**手段就是降时间码字号（按钮不动、状态文字 18 已在按钮之下、
内边距属于卡片不属于这一行）。这**不是**按比例估的，是上面那条字号扫描探针量的：

| 字号 | 时间码行高 | 那一行的高度 | 结论 |
|---|---|---|---|
| 34.0（改前） | 40 | 40 | 时间码撑高，按钮被架空 |
| 31.0 / 30.0 | 36 / 35 | 36 / 35 | 还是时间码说了算 |
| **29.0（改后）** | **34** | **34** | = 按钮高度，**刚好齐平** |
| 28.0 及以下 | 33… | 34 | 按钮成为下限，行高不再降 |

→ **29.0 是"行高压到最低 + 字号只降一档"的那个点**。顺便实测：`00:00` / `00:01` / `09:59` /
`59:59` 在 29.0 下宽度**完全相同**（87.3px）→ 录制中数字**不会跳动**（等宽字体族与对齐方式没动）。

### 11.27.3 第 2 步：改了什么（只改字号，一行代码）

* `theme::FS_TIMER` **34.0 → 29.0**（**单一来源**：`mono_big` 是时间码唯一入口，
  原来那个 `34.0` 是写在 `mono_big` 里的字面量，本轮提成常量并调档）；
* 顺带把两处散落字面量收进 `theme`（同一个"字号/尺寸只有一处"的原则）：
  `FS_STATUS = 14.0`（状态文字，"就绪"原来是硬编码 14.0）、
  `START_BTN_W/H = 132.0/34.0`（主按钮，原来硬编码在 `ui.add_sized([132.0, 34.0], …)`）、
  `FIELD_LABEL_W = 88.0`（四行的标签列宽，原来四个 `88.0` 字面量）。
  **这些只是"搬家"，数值一个都没变**（`layout.rs` 的单测断言按钮仍是 132×34）；
* 时间码的取文案也顺手合并成**一个 `ui.label` 出口**（原来 `Some(d)`/`None` 两个分支各写一次），
  但显示的字符串与字体族完全一致；
* **没有动**：按钮尺寸、状态文字字号数值、卡片内边距/间距、另外三张卡片的任何尺寸、
  录制/编码/封装/剪辑算法、预览流式规则与 200px 下限、最小宽度推导。

### 11.27.4 常量与最小窗口

* `ui::layout::RECORD_REQ_H`：**402.0 → 396.0**（实测 396.0，中英相同）；
* `ui::layout::MIN_INNER_H`：**625.0，不变**（= 126 + max(499, 396)，仍由**剪辑页的 499** 决定）；
  `src/main.rs` 的 `min_inner_size(760, 625)` 因此**没有改动**；
* `layout.rs` 文档里的实测表格已同步，并补了一张**录制页逐部件高度表**；
* 新增单测 `ui::layout::tests::record_page_compression_accounts_for_exactly_the_status_row`：
  把"状态卡片压 6px（40 → 34）→ 整页 402 → 396、按钮仍 132×34"钉成常量关系。

**"滚动区口径差额 40"是什么**（不解释清楚就对不上账）：`ScrollArea` 报的 `content_size`
是内容层 `Ui::min_size()`，而 `Ui::add_space` **既推进游标、又把 `min_rect` 撑大** →
滚动区把行距**重复计了一次**（标定：同一份卡片只换最后一张高度，差额恒为间距之和）。
所以"必需高度"必须用 `content_size.y` 这个口径定义（`no_vertical_scroll` 判的就是它），
逐部件表把差额单列（实测 40.0）= 40 + 332（四卡）+ 24（三间距）→ **396.0，与 `content_size.y` 逐像素一致**。

### 11.27.5 第 3 步：验证

* `cargo build --all-targets` → **0 个 rustc 告警**（只剩 build.rs 原有的两条说明性输出）；
* `cargo test --all-targets` → lib **156 passed / 3 ignored**（原 155/1：+1 条常量守门单测，
  +2 条 `#[ignore]` 探针）、`src/main.rs` **1 passed**、e2e **7 passed**；`cargo test --doc` → **1 passed**；
* `scripts\regression.ps1` → **12/12 PASS，exit 0**；
* 截图（`SetWindowPos` 把**客户区**调到指定尺寸后整窗抓图；本会话不能注入鼠标，
  标签页用 `Tab`+`Space` 键盘切换）：
  * 最小窗口 **760×625**：`_rust\ui-shots\record-row-before-min.png` /
    `record-row-after-min.png`（**无滚动条**、页面充满、按钮与"就绪"完整）；
  * 默认窗口 **1020×720**（实际客户区 1036×759）：`record-row-before-default.png` /
    `record-row-after-default.png`，以及裁到状态卡片那一行的
    `record-row-before-statuscard.png` / `record-row-after-statuscard.png`（肉眼可比的对照）；
  * `before` 两图是**临时**把 `FS_TIMER` 改回 34.0 重新构建后拍的，拍完即恢复（`theme.rs` 里现在
    只有 29.0，`git diff` 看不到 34.0 的残留；`before` 用的是 debug 构建、`after` 用的是 release 构建）。

### 11.27.6 重编 + 签名 + 打包（CapLite 1.0.0）

* `dist\CapLite-1.0.0\` = **22 262 009 B（21.23 MB）**，**恰好 13 个文件**
  （比 §11.22.4 的 22 255 865 多 4 720 B —— `CapLite.exe` 变大 + 签名）；
* `dist\CapLite-1.0.0-win64.zip` = **9 772 430 B（9.32 MB）**，**13 个条目**，根目录 `CapLite-1.0.0/`；
* 解压到临时目录逐文件核对 SHA256：**13/13 一致、0 不一致、0 缺失、0 多余**；解压出的
  `CapLite.exe` 回读签名仍 `Status=Valid / Authenticode`、`VersionInfo` 仍 `1.0.0 / CapLite`、
  无 MotW（核对完临时目录已删除）；
* `SHA256(CapLite-1.0.0-win64.zip)` = `054D6217195822C6A0341BEF26EA31DC86F245C12FC932D6F84CC53DD6D173D3`；
* `SHA256(CapLite.exe)`（发布目录内已签名版本）= `97D61C1BD6673900BB8621E70D2B66959FBF32C3180283B6C7797D78B5B70804`；
* 签名：`Status=Valid`、`SignatureType=Authenticode`、`MotW=无`（同 §11.22.5：会话进程写不了
  用户密钥库，`dev-sign.ps1` 复用本机既有 `CN=cap-rust dev signing`，thumb `22DACD13…`）。

**未验证项（本轮）**：
* 截图只在**本机 100% DPI + 浅色主题**下拍过，没有 125%/150% DPI 与深色系统下的实拍；
* 录制中/暂停两态的**状态行同高**只由代码结构保证（状态文字 18、时间码 34、按钮 34 三者不随
  状态变化、字符数相同 → 行高不变），**没有**录一段真视频去拍"录制中"的实图；
* 最小窗口的"无滚动条"是 headless 实测（`content_size.y 396 ≤ 视口 499`）+ 截图确认，
  没有用像素级工具去数截图上有没有滚动条（滚动条配置是 `AlwaysHidden`，本来也不画）。

**发现一处既有文档陈旧（不在本轮允许改动范围内，如实记录）**：`src/main.rs`
`ViewportBuilder::with_min_inner_size` 上方那段推导注释仍是**更早一轮**的数字
（写着"剪辑页 = 475 / 录制页 = 402 / 外壳 61 / min_h 536 / 760×536"），
与 §11.23 起的实测值（剪辑页 499、录制页 402→396、外壳 126、`min_h` 625、760×625）不一致
—— 代码引用的常量是对的（`MIN_INNER_W`/`MIN_INNER_H`），**只有那段注释过期**。
本轮边界是"允许改 `main_page.rs` / `theme.rs` / `layout.rs` / `docs/DESIGN.md` / `dist\**`"，
`src/main.rs` 不在其中，所以**没有动它**；建议下一轮顺手把那 6 行注释按
`ui::layout` 的表格改过来。

---

## 11.28 托盘图标（通知区域）+ 全局快捷键（同一键开始/停止，三选一）（本轮）

需求两条：① 录制开始时把窗口**隐藏到托盘**（替代"最小化"）；② 加**全局快捷键**，
**同一个键**在"开始录制 / 停止录制"之间切换。硬约束：**不加任何新 crate** ——
`windows` crate 已在依赖里，只补两个 feature（`Win32_UI_Shell`、`Win32_UI_Input_KeyboardAndMouse`），
全部手写 Win32（新文件 `src/tray.rs`，约 1 600 行含注释与单测）。

### 11.28.1 托盘实现：隐藏顶层窗口 + `Shell_NotifyIconW` + 自建菜单

| 部件 | 做法 | 为什么 |
|---|---|---|
| 消息窗口 | **隐藏的顶层窗口**：`CreateWindowExW(WS_EX_TOOLWINDOW, class="CapLiteTrayMessageWindow", title="CapLiteTray", WS_POPUP, 0,0,0,0, parent=NULL)`，**永不 `ShowWindow`** | 需求允许"message-only 窗口**或**隐藏窗口"。先按 message-only（`HWND_MESSAGE`）实现，**实测不行**：另一个进程 `FindWindowExW(HWND_MESSAGE, 0, 类名, NULL)` / `EnumChildWindows(HWND_MESSAGE)` 都**找不到**它（`child count = 0`），于是"用 `PostMessage(WM_HOTKEY)` 打到自己的消息窗口"这条硬性验证路直接断掉。改成隐藏顶层窗口后 `EnumWindows` 能列出它（`class='CapLiteTrayMessageWindow' title='CapLiteTray' visible=False`），验证可做；`Shell_NotifyIcon` 的生态实践（`tray-icon` 等）用的也是普通窗口。`WS_EX_TOOLWINDOW` 让它不进 Alt+Tab，`WS_POPUP` + 不显示 → 不在任务栏 |
| 窗口建在**主线程** | `App::new` 里装（eframe 的事件循环线程） | Win32 的消息**只被创建窗口的那条线程**的消息循环取走。eframe/winit 用 `PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE)` 取**本线程所有窗口**的消息再 `DispatchMessageW`（winit 0.30 `event_loop.rs::dispatch_peeked_messages`），所以**不需要、也不该**自己起第二个消息循环 —— 自己起循环会与 winit 抢消息，那才是"托盘 + eframe 冲突"的真正来源 |
| 图标 | `assets/caplite-256.rgba`（与运行时窗口图标同一份字节）→ 按 alpha 加权的盒式降采样到 32×32 → `rgba_to_bgra` → 32bpp **自上而下** DIB（`biHeight = -32`，否则图标上下翻转）→ `CreateIconIndirect`（32bpp 彩色位图 + 1bpp 掩码）→ `Shell_NotifyIconW(NIM_ADD, NIF_ICON\|NIF_MESSAGE\|NIF_TIP)` | 复用已有字节，不引图标库；降采样的 RGB 按 alpha 加权，避免透明像素把边缘染灰 |
| 菜单 | 右键**或左键**点图标 → `CreatePopupMenu` + `AppendMenuW` + `TrackPopupMenu(TPM_LEFTALIGN\|TPM_BOTTOMALIGN\|TPM_RIGHTBUTTON\|TPM_RETURNCMD\|TPM_NONOTIFY)`，**三项**：显示窗口 / 停止录制 / 退出。前后配 `SetForegroundWindow(hwnd)` + `PostMessage(WM_NULL)`（MSDN 的标准写法，不做菜单点外面不消失） | `TPM_RETURNCMD` 让选中的命令直接由返回值给出，不用 `WM_COMMAND`；菜单**在 WndProc 里弹**（不在 egui 帧里弹），避免"绘制过程中再进一层模态消息循环" |
| 菜单文案 | 界面帧把 i18n 文案推给 Win32 侧的全局表（语言一变就重推） | WndProc 那条路径拿不到 `I18n`，也不该为了菜单把语言状态搬进全局 |
| 唤醒界面 | WndProc 入队命令后顺手 `egui::Context::request_repaint()`（句柄在 `install` 时存进全局） | `WM_HOTKEY`/菜单回调发生在 egui 帧之外，不唤醒的话命令会一直躺在队列里 |

**是否落在溢出区由系统决定，程序不能强制**：Win11 默认把新出现的通知区域图标收进
**溢出菜单**（任务栏上那个 `^`）。本程序不做任何"强制固定到任务栏"的动作（也没有 API），
用户可以在「任务栏设置 → 其他系统托盘图标」里自己固定。

**托盘 API 失败的降级**：`Shell_NotifyIconW` 失败**不再**否决整个托盘 ——
消息窗口留着，**全局快捷键照常注册**，`icon_ok()` 为 false（详见 11.28.5 的实测原因）。
只有"连消息窗口都建不出来"才整体失败，那时也只记一行 `cap.log`。

### 11.28.2 隐藏 / 恢复规则（"谁隐藏谁恢复"）

配置键 `hide_to_tray_on_record_start`（**默认 `false`**，老配置缺键也回落 false；
隐私优先，避免热键启动时静默隐藏窗口。历史章节里“默认 true”的口径已过时）。
判定集中在 `tray::HideState::poll(recording, hide_to_tray)`（纯逻辑，5 条单测），
三条入口（界面按钮 / 托盘菜单 / 全局快捷键）共用同一个判据：

| 状态 | 动作 |
|---|---|
| 录制**真正开始**（`Recorder::start` 成功 → `recorder` 是 `Some`）且开关为真且**有恢复入口** | `ViewportCommand::Visible(false)` + 同步 `ShowWindow(SW_HIDE)`；把"**是我们隐藏的**"记下来 |
| 还在录 | 什么都不发（幂等，不会每帧重复发命令） |
| 录制**停止 / 失败 / 导出完成**且"是我们隐藏的" | `ViewportCommand::Visible(true)` + 同步 `ShowWindow(SW_SHOW)` + `SetForegroundWindow` |
| 不是我们隐藏的（用户自己最小化/隐藏） | **什么都不做** —— 绝不覆盖用户的意图 |
| 开关关闭 | **一个 viewport 命令都不发**（对照实验见 11.28.6：日志里连一条隐藏/恢复审计行都没有） |
| **没有恢复入口** | **不隐藏**。恢复入口 = 通知区域图标挂上了 **或** 全局热键注册成功；两个都没有时隐藏等于"把窗口弄丢"（本环境实测就是这样，见 11.28.5） |

隐藏/恢复各留一行 `cap.log` 审计（含 `IsWindowVisible` 前后读数），这是"谁隐藏谁恢复"
唯一的运行期证据。

**踩坑（两条，都是实测）**：

1. **窗口隐藏后 eframe 只调 `App::logic`，不调 `App::ui`**（`update_logic_only`）。
   所以托盘命令与隐藏/恢复判定必须在 `logic` 里**也**走一遍 —— 少了这一处，
   "隐藏到托盘之后按热键停止录制"永远不生效。
2. **隐藏期间不会再有新的帧**（即使在 `logic` 里持续 `request_repaint_after`）：
   于是"先把 `Visible(true)` 发出去、下一帧再置前"这条设计会把窗口**永久留在隐藏状态**
   （实测 `IsWindowVisible` 连续 5 秒为 false，负责第二拍的 `logic` 再也没被调用）。
   修法：**同一帧里就同步把 Win32 的 `ShowWindow` 也做掉**（两条路都走，谁生效都不冲突）；
   隐藏侧同样同步 `ShowWindow(SW_HIDE)`，所以 winit 的 `VISIBLE` 标志与真实状态
   无论 viewport 命令有没有被应用都不会错位。

### 11.28.3 全局快捷键：三选一 + 自定义捕获

* **有效组合的单一算法**：`tray::effective_hotkey(HotkeyPreset, &str) -> Hotkey`
  —— 预设取预设常量、`Custom` 解析配置文本、**解析不了回退默认组合**。
  全程序只有这一处决定"到底注册哪个组合"（设置页显示、告警文案都复用它）。
* 预设（配置里存的名字就是 serde 名）：

| `hotkey_preset` | 组合 | 说明 |
|---|---|---|
| `ctrl_alt_r`（**默认**） | `Ctrl+Alt+R` | `PRESET_CTRL_ALT_R` |
| `ctrl_shift_r` | `Ctrl+Shift+R` | `PRESET_CTRL_SHIFT_R` |
| `custom` | 取 `hotkey_custom` 文本 | 规范化后存回配置（`"ctrl + shift + f9"` → `"Ctrl+Shift+F9"`） |

* 注册：`RegisterHotKey(hwnd, id, MOD_CONTROL|MOD_ALT|MOD_NOREPEAT, 'R')`。
  `MOD_NOREPEAT`：按住不放**只触发一次**（否则会开始/停止来回切）。
* **同一个键切换**：WndProc 收到 `WM_HOTKEY` → 入队 `ToggleRecording` →
  界面帧调 `MainPage::start_recording` / `stop_recording` ——
  **与状态卡片上那个主按钮是同一对函数**（所以导出收尾、预热补建、错误文案完全一致），
  失败同样走底部状态栏那条消息通道。
* **自定义校验**（`tray::parse_hotkey`，错误是**结构化的**，文案走 i18n）：
  * 必须含**至少一个修饰键**（`Ctrl`/`Alt`/`Shift`/`Win`，别名 `Control`/`Meta`/`Super`/`Windows` 也认）；
  * 必须**恰好一个**非修饰键：`A`–`Z`、`0`–`9`、`F1`–`F24`，或 `Space`/`Tab`/`Enter`/`Esc`/
    `Insert`/`Delete`/`Home`/`End`/`PageUp`/`PageDown`/方向键/`Backspace`（含 `Del`/`Ins`/`PgUp`/`PgDn` 等别名）；
  * 拒绝**系统保留组合**：`Ctrl+Alt+Del`、`Ctrl+Shift+Esc`、`Win+L`、`Alt+Tab`、`Alt+Esc`、`Ctrl+Esc`、`Win+Tab`；
  * 成功时统一补 `MOD_NOREPEAT`，并把结果 `label()` 规范化写回配置。
* **捕获控件**（设置页选「自定义」时出现）：点一下进入捕获态 → 按组合键 → 规范化显示；
  **捕获期间外壳会临时 `UnregisterHotKey` 当前热键**（`SettingsOutcome::hotkey_capturing` →
  `App::ui_root` 的边沿判定）。这一步是必须的：不注销的话用户按下的正是当前组合，
  Windows 会把按键**吃掉**并直接开始/停止录制，根本进不了输入框。
  捕获态下按 `Esc` 取消、非法组合**当场**给可见告警；切走标签页自动结束捕获（`stop_capture()`）。
  说明：egui 的 `Modifiers` 只有 `alt/ctrl/shift/mac_cmd/command`，**拿不到 Win 键**，
  所以 `Win+…` 的自定义组合需要在配置文件里手写（解析与注册都支持它）。
* **切换与失败回滚**（`HotkeyRegistrar::set_hotkey(want, fallback)`）：
  1. `UnregisterHotKey` 旧的 → 2. `RegisterHotKey` 新的 → 3. 失败就**回滚到 `fallback`**
  （通常是"换之前那个还注册着的组合"，所以**旧热键仍然可用**）→ 4. **候选探测**：
  依次试 `Ctrl+Alt+R` / `Ctrl+Shift+R` / `Ctrl+Alt+F9` / `Ctrl+Shift+F9`（用**独立 id** 注册后
  立刻注销，不碰用户的热键），把第一个真的能注册成功的组合写进告警与 `cap.log`。
* 失败可见提示：`HotkeyRegisterError { hotkey, code, system, suggestion }` →
  底部状态栏一条 **warn**（`1409` 时文案是"`X` 已被其他程序占用，请更换（例如 `Y`）"）+ `cap.log`
  一行（含 `GetLastError`）。托盘/窗口那类失败**只进日志**、不打扰用户。

### 11.28.4 配置键与缺键兼容

| 键 | 类型 | 默认 | 缺键行为 |
|---|---|---|---|
| `hide_to_tray_on_record_start` | bool | **false** | 回落 false（隐私默认；用户显式 true 才隐藏） |
| `global_hotkey_enabled` | bool | **true** | 仍为 true（同上） |
| `hotkey_preset` | `"ctrl_alt_r" \| "ctrl_shift_r" \| "custom"` | `ctrl_alt_r` | 仍是默认预设 |
| `hotkey_custom` | string | `""` | 仍是空串（空/非法 → 有效组合回退 `Ctrl+Alt+R`） |

**绝不因为缺键而 load 失败**：单测 `legacy_config_without_the_new_keys_still_reads_as_true`
用 `{}` / `{"fps":60}` / 老中文配置三种输入各断言一遍。
i18n 新增键（中英 key 集合仍然完全一致，共 147 + 0 个键）：
`sec_general`、`set_hide_to_tray`、`set_global_hotkey`、`hotkey_label`、`hotkey_preset_custom`、
`hotkey_capture_idle`、`hotkey_capture_prompt`、`hotkey_err_no_modifier`、`hotkey_err_no_key`、
`hotkey_err_reserved`、`hotkey_err_malformed`、`hotkey_invalid_fallback`、
`hotkey_taken_with_suggestion`、`hotkey_taken`、`hotkey_register_failed`、
`tray_tip`、`tray_menu_show`、`tray_menu_stop`、`tray_menu_quit`、`tray_ready`、`tray_unavailable`。

### 11.28.5 设置页最终形态

```
┌ 语言 ────────────────────────────────────────────┐
│ 语言              [ en-US ▾ ]                     │
└──────────────────────────────────────────────────┘
┌ 常规 ────────────────────────────────────────────┐
│ ☑ 录制开始后隐藏到托盘                             │
│ ☑ 启用全局快捷键                                   │
│ 全局快捷键   (•) Ctrl+Alt+R  ( ) Ctrl+Shift+R  ( ) 自定义 │
│              [ 点这里，然后按组合键 ]   ← 仅"自定义"时出现 │
│              ⚠ <非法/保留组合时的可见告警>          │
└──────────────────────────────────────────────────┘
┌ 视频 ────────────────────────────────────────────┐
│ ...（原有：目标码率 / 帧率）                        │
└──────────────────────────────────────────────────┘
```
* 风格沿用现成的 `theme::card` + `checkbox` + `radio_value` + `theme::banner(Warn, ..)`，
  开关关闭时快捷键那一组**整组置灰但位置不变**；**录制中整页仍按既有规则禁用**
  （`add_enabled_ui(false, ..)` 一条指令管住全部子控件，捕获态同时被强制结束）；
* 没有多余说明文字（只有控件本身的标签）。

### 11.28.6 验证记录（本轮实测）

**构建 / 单测 / 回归（全部真跑）**

| 项 | 结果 |
|---|---|
| `cargo build --all-targets` | **0 rustc 告警**（只有 `build.rs` 自己那两条固定提示） |
| `cargo test` lib | **180 passed / 0 failed / 3 ignored**（基线 156 → **+24**） |
| `cargo test` bin `CapLite` | **4 passed**（基线 1 → +3） |
| `cargo test` e2e / doc | **7 passed** / **1 passed**（不变） |
| `scripts\regression.ps1` | **12 / 12 PASS**（0 FAIL） |

新增单测（去重后的要点）：
`tray::tests::preset_resolves_to_the_expected_combo`（三种 preset 的有效组合 + 自定义生效 + 非法回退 + `DEFAULT_HOTKEY`）、
`preset_serializes_with_the_documented_names`（`ctrl_alt_r`/`ctrl_shift_r`/`custom` 的 serde 名）、
`parse_accepts_and_normalizes`、`parse_rejects_invalid_and_reserved`（无修饰键 / 只有修饰键 / 坏键名 / 5 个保留组合）、
`vk_names_round_trip`、`hide_state_*`（5 条：藏一次恢复一次、开关关闭不发命令、不覆盖用户意图、中途改开关仍恢复）、
`rgba_to_bgra_swaps_red_and_blue_only`、`downscale_keeps_dimensions_and_alpha_weighting`、
`tip_is_always_null_terminated`、`command_queue_round_trips`、
**`occupied_hotkey_reports_1409_and_rolls_back`（真机 1409，见下）**、
`hotkey_registrar_tracks_the_current_combo`；
`config::tests::tray_and_hotkey_defaults_are_on`、`legacy_config_without_the_new_keys_still_reads_as_true`、
`save_load_roundtrip_for_the_two_switches`、`custom_hotkey_is_normalized_but_never_dropped`；
`ui::settings::tests::the_three_presets_map_to_the_expected_combos`、`capture_builds_canonical_text`、
`every_mapped_key_name_parses_back`、`parse_errors_have_visible_text_in_both_languages`、`capture_state_can_always_be_reset`；
`main::tests::hotkey_failure_produces_visible_warning_and_log_line`（**告警文案与日志行都带 1409 与建议组合**）、
`hotkey_capture_and_change_edges_are_well_defined`、`headless_settings_page_draws_every_hotkey_preset`。

**占用检测（本需求的核心验收，真机 + 可自动化）**

`tray::tests::occupied_hotkey_reports_1409_and_rolls_back`：在**同一进程的另一个线程**上
真的 `RegisterHotKey(None, id, MOD_CONTROL\|MOD_SHIFT\|MOD_NOREPEAT, VK_F10)` 抢占
`Ctrl+Shift+F10`，等它回报成功后，走**产品自己的注册路径**注册同一个组合：

```
test tray::tests::occupied_hotkey_reports_1409_and_rolls_back ... ok
```

断言全部成立：拿到的 `code == 1409`（`ERROR_HOTKEY_ALREADY_REGISTERED`）、
`err.already_registered()` 为真、**旧热键 `Ctrl+Alt+F11` 回滚成功且仍然注册着**
（`reg.current() == Some(old)`、`reg.is_registered()`）、错误详情的 `describe()` 里同时含
`1409`、冲突组合名与**候选探测结论**。另一条 `hotkey_registrar_tracks_the_current_combo`
覆盖"注册 → 重复设成同一个（空操作）→ 换成另一个 → 关掉"的正常路径。
**"界面/日志确实给了告警"**由 bin 侧单测 `hotkey_failure_produces_visible_warning_and_log_line`
断言（中英各一遍）：状态栏 warn 文案里同时出现冲突组合与建议组合、提示是 `sticky`（不会被
"下一次保存配置成功"顺手撤掉）、`cap.log` 那行里同样带着 `1409` 与建议。

**真机行为证据（发布目录里的那份 `CapLite.exe`，脚本 `_rust\tray-evidence\verify-tray.ps1`）**

本会话不能注入鼠标/键盘，所以用 `EnumWindows`（按 pid + 类名/标题）找到
`CapLite` 主窗口与 `CapLiteTrayMessageWindow`，再 `PostMessageW(tray_hwnd, WM_HOTKEY, 0x0CA1, 0)`
打到**我们自己的消息窗口**上 —— 走的是与物理按键**完全相同**的 WndProc 分支。

`RegisterHotKey` 的真实返回值 = **成功**（`cap.log`：`托盘已就绪（通知区域图标=未挂上）；
全局快捷键 = Ctrl+Alt+R`）。

| 步骤 | 开关 `hide_to_tray_on_record_start=true` | 对照：`false` |
|---|---|---|
| 启动完成（未录制） | `IsWindowVisible=True` | `IsWindowVisible=True` |
| `PostMessage(WM_HOTKEY)` 第 1 次（开始录制） | **`IsWindowVisible=False`**（已隐藏） | `IsWindowVisible=True`（**保持可见**） |
| `PostMessage(WM_HOTKEY)` 第 2 次（停止 + 导出） | **`IsWindowVisible=True`**（已恢复）；产物 `rec-20261013-104532.mp4`（451 338 B） | `IsWindowVisible=True`；产物 `rec-20261013-104552.mp4`（279 609 B） |
| `cap.log` 审计 | 两条：`录制已开始（配置开关=开，且存在恢复入口）→ 发送 Visible(false) 并同步 Win32 隐藏：ShowWindow(SW_HIDE)=true，主窗口 IsWindowVisible=Some(false)`；`录制已结束（停止/失败/导出收尾完成）→ 发送 Visible(true) 并同步 Win32 恢复：置前=false，主窗口 IsWindowVisible Some(false) → Some(true)` | **一条隐藏/恢复行都没有** → "开关关闭时不发任何 viewport 命令"成立 |

（`置前=false` 是 `SetForegroundWindow` 被前台锁拒绝，属于 best-effort：窗口已经恢复可见，
egui 的 `ViewportCommand::Focus` 也发过了；日志如实记录。）

**本环境特有的两个限制（不是实现缺陷，如实记录）**

1. **`Shell_NotifyIconW(NIM_ADD)` 在本会话必然失败：`GetLastError=5（拒绝访问）`**。
   实测根因是 DSH 沙箱把进程跑在 **Low 完整性级别**（`whoami /groups` →
   `Mandatory Label\Low Mandatory Level S-1-16-4096`），而 `explorer.exe`（`Shell_TrayWnd`
   确实存在，`FindWindowW` 能拿到）在 Medium。UIPI 会拦"低完整性 → 高完整性"的
   `SendMessage`，而托盘协议的消息在 `>= WM_USER` 区间 —— 直接实测：
   `SendMessageTimeoutW(Shell_TrayWnd, 0x0000) → ok=1 err=0`，
   而 `0x0400/0x0401/0x04FF` 与 `0x0312` 全部 `ok=0 err=5`。
   所以 **"托盘图标出现在溢出区"与"点击托盘菜单三项"在本会话无法验证**，
   属于"只能由用户肉眼确认"的未验证项；程序侧的行为是**降级**：
   图标挂不上只记一行日志，**消息窗口与全局快捷键照常工作**（它们不需要跨进程消息）。
2. **物理按键无法验证**：`PostMessage(WM_HOTKEY)` 验证的是**热键处理逻辑**
   （同一个键 → 开始 / 再按 → 停止），"真的按下 Ctrl+Alt+R 会被 Windows 投递到我们窗口"
   这一环只由"`RegisterHotKey` 返回成功"覆盖 —— 本会话没有键盘注入能力，**如实列为未验证项**。

### 11.28.7 重编 + 签名 + 打包（CapLite 1.0.0）

* `dist\CapLite-1.0.0\` = **22 458 105 B（21.42 MB）**，**恰好 13 个文件**
  （`CapLite.exe` 16 381 840 B 已签名；5 个 `ffmpeg-min` DLL + 2 个 `openh264*.dll`
  + `LICENSE` + 4 份随包文本，与 §11.22.4 同一份清单）；
* `dist\CapLite-1.0.0-win64.zip` = **9 888 145 B（9.43 MB）**，**13 个条目**，根目录 `CapLite-1.0.0/`；
* 解压到临时目录逐文件核对 SHA256：**13/13 一致、0 不一致、0 缺失、0 多余**
  （解压出的 `CapLite.exe` 也回读一遍签名：`Status=Valid`、`SignatureType=Authenticode`、
  `VersionInfo=1.0.0/CapLite`、`MotW=无`；核对完临时目录已删除）；
* `SHA256(CapLite-1.0.0-win64.zip)` = `5EE1D1D1BABDE6EAA54923D06AA8F9C8169D524F4D76AB7EC25B5116029FE210`；
* `SHA256(CapLite.exe)`（发布目录内已签名版本）= `393EC06AA28D565215DE44FE716A8F0F2A039796709FB0E928241DF7524A0B51`；
* 签名：`Status=Valid`、`StatusMessage=已验证签名。`、`SignatureType=Authenticode`、`MotW=无`
  （同 §11.22.5 / §11.27.6：本会话进程写不了用户密钥库，`dev-sign.ps1` 复用本机既有
  `CN=CapLite dev signing` → 实际签的是 `CN=cap-rust dev signing`，thumb `22DACD13…`）；
* `VersionInfo`：`FileVersion=1.0.0`、`ProductVersion=1.0.0`、`FileDescription=CapLite screen recorder`、
  `ProductName=CapLite`、`CompanyName=CapLite`、`LegalCopyright=Copyright (c) 2026 CapLite`；
* 两层真机验证（§11.28.6 的 A/B）是**拿发布目录里那份签名 exe 的副本**再跑了一遍，
  `SHA256` 与发布目录一致（`393EC06A…`）—— 也就是说"签过名的那一份"确实是被测过的那一份。

**本轮未验证项（如实列出）**
* **通知区域图标是否出现、是否落在 Win11 溢出区，以及点击菜单三项** —— 只能由用户肉眼确认
  （本会话连"挂上去"都做不到，原因见 §11.28.6 的完整性级别实测）；
* **物理按键**（真的按 `Ctrl+Alt+R`）—— 本会话没有键鼠注入能力，
  `PostMessage(WM_HOTKEY)` 只覆盖"处理逻辑"，`RegisterHotKey` 成功覆盖"注册"；
* 自定义快捷键捕获控件里的**真实按键捕获**（需要人按键盘）—— 只做了 headless 绘制与
  `egui::Key → 组合文本 → parse_hotkey` 的纯逻辑闭环；
* 设置页在 125%/150% DPI 与深色系统主题下**没有实拍**（与 §11.27 的说明一致）。

**顺带更新的构建产物**：`assets/font-subset.ttf` 532 684 B → **656 968 B**
（新增的 21 条中文 i18n 文案引入了「册托组藏隐饰他」等不在旧子集里的字，
`ui::startup::tests::embedded_font_subset_covers_all_locale_glyphs` 会挡；
按脚本提示重跑 `scripts\make-font-subset.ps1 -PythonPath ..\_rust\tmp\pylibs` 重新生成 ——
`-PythonPath` 是因为本机 Python 没有 `fontTools`，用工作区里已解开的 `_rust\tmp\pylibs`）。

---

## 11.29 录屏音频链路改造：参考 add/ 方案的“读写分离 + 优先级分层”（2026-10-18 本轮）

### 11.29.1 起因与参考方案

用户提供 `add/`（win_highperf_recorder，789 行独立小项目），主张
**“优先保障音频、视频可丢帧、声画自动同步”**：WASAPI 硬件侧只做内存写入，
两层 SPSC 环形缓冲（0.2 s 小环 + 10 s 大环）垫在采集与业务之间，搬运线程跑 `Highest`；
视频编码线程 `BelowNormal`；封装层以音频为主时钟，视频滞后 >120 ms 直接丢。

对照 CapLite 改造前的音频线程（一个人干：等事件 → 读 PCM → 补静音 → 混音 → AAC →
送封装，队列满还阻塞 200 ms）：这条链任何一环变慢，**WASAPI 设备内部的环形缓冲就会被
覆盖**——那是不可恢复的爆音/周期丢音，正是录屏软件最常见的投诉。改造就是拆掉这个耦合。

### 11.29.2 采纳 / 不采纳

| add/ 机制 | 取舍 | 说明 |
|---|---|---|
| 两层 SPSC 音频环 | ✅ 采纳 | `src/audio/ring.rs`，`Mutex<VecDeque>`+`Condvar` 实现（SPSC 无锁竞争，不引新依赖） |
| 线程优先级分层 | ✅ 采纳 | `src/sys.rs`；音频读/消费 `AboveNormal`、搬运 `Highest`、视频 `BelowNormal` |
| 视频滞后丢弃 | ✅ 采纳 | `VIDEO_LAG_DROP_MS=120`，`src/recorder.rs` 归并写出前 |
| 音频为主时钟 | ⬜ 不采纳 | 现机制等价且更细：音频 pts=AAC 帧计数、视频 pts=真实时刻吸附帧网格（§10.6），实测偏差 18 ms |
| FFmpeg 子进程 + pipe | ❌ 不采纳 | 其自述唯一瓶颈就是 pipe `write_all`；CapLite 进程内 FFmpeg + 自研分片写入没有这条瓶颈 |
| SIMD BGRA→I420 / 异步日志 | ⬜ 已有 | libyuv、tracing+tracing-appener |

### 11.29.3 改造后的线程全景（录制会话）

```text
音频读 cap-audio-read-{i}（AboveNormal，每路一个）
    └─ read_chunk(100ms 事件/排空) → AudioGateway 小环(0.2s，满则丢最旧保最新)
音频搬运（Highest，网关内自动起）
    └─ 小环 → 大环(10s，吸收消费侧停顿)
音频消费 cap-audio（AboveNormal）
    └─ pop 大环 → 墙钟补静音(§11.6.1 口径不变) → 混音 → AAC → send_timeout(200ms) → 封装
视频 cap-video（BelowNormal）
    └─ DXGI → 指针合成 → libyuv → OpenH264 → video 队列(120，满则丢帧)
封装 mux（Normal）
    └─ select! 归并 → **视频滞后 ≥120ms 丢弃(video_lag_dropped)** → 分片 MP4 写入
```

### 11.29.4 设计决策与坑

1. **小环满=丢最旧保最新**：与 add/ 视频 BGRA 队列同一策略。保最新让音频始终贴近实时沿，
   且消费侧“墙钟补静音”天然把丢掉的缺口补成静音，不会时间轴错乱。
2. **大环 10 s 是所有消费侧停顿的总兜底**：AAC 抖动 + 封装 200 ms 背压都在它面前消失；
   只有它也被打穿（`audio_ring_dropped > 0`）才真丢音频——那是“机器被拖到极限”的客观证据。
   该计数**正常必须为 0**，诊断时第一个看它。
3. **停止收尾顺序不能反**：等读取线程全部退出（`reads_done` 计数）→ `gateway.stop()`
   （搬运线程 final drain 把小环残留搬进大环）→ 消费线程抽干大环 → AAC finish → AudioEnd。
   反过来（先停搬运或先退消费）会丢掉尾部最多 10 s 音频。
4. **SilencePadding 不搬**：静默期环回不产包（§11.6.1 实测丢 1/3 音频的根因）。
   消费线程“本轮无数据”时不跳过补静音——只是把 `wait_data(5ms)` 放在取数前当节拍。
5. **视频线程敢垫底的原因**：pts 按真实采集时刻吸附帧网格（§10.6），编码慢只会让帧变稀、
   绝不会把 4 秒录成 2.7 秒快放；而音频线程被抢是**不可恢复**的数据损坏。牺牲可弃的，保住不可弃的。
6. **采集+编码仍在同一线程**：不拆。1080p 实测编码 129 fps ≫ 30 fps 目标，拆开反而多一次
   每帧 3 MB 跨线程拷贝（原 §11.7 结论，仍然成立）。
7. **没用 realtime 优先级**：Windows 上 realtime 有整机卡死风险，add/ 同样规避。
   `src/sys.rs` 的枚举里**刻意不含** `TimeCritical`。

### 11.29.5 验证结果（2026-10-18，沙箱内全部跑通）

| 命令 | 结果 |
|---|---|
| `_build-env-san.bat test --lib` | **319 passed / 0 failed / 3 ignored**（连跑 2 次一致；较基线 307 净增 12） |
| `_build-env-san.bat test --bins` | **4 passed / 0 failed** |
| `_build-env-san.bat test --test e2e` | **7 passed / 0 failed** |
| `_build-env-san.bat build --release --bin CapLite` | 零 rustc 告警 |
| `recorder_smoke 3s --audio system` | **真录通过**：视频 91 帧 / 0 丢弃、音频 143 包、补静音 145 232 帧（≈3.02 s，沙箱静音正好走墙钟补齐路径）、流与导出对账 **0 ms**、导出 234/234 包 0 错误 |

**沙箱内怎么跑通的（重要）**：`reg.exe`（vcvars 探测 SDK 必需）在本机 Program Blacklist 里，
沙箱内必被拦。`_build-env-san.bat` 改为**静态 MSVC 路径**（VS 18 Community /
MSVC 14.51.36231 / SDK 10.0.28000.0，`CC=cl` 让 cc crate 走 PATH 查找、不碰注册表），
全程不调 `vcvars64.bat`，于是不触发 `reg.exe`。
**踩坑记录：批处理必须是 CRLF 行尾** —— LF-only 时 `set`/`echo` 会被 cmd 以难以理解的方式
吞掉（实测 `VCTOOLS` 设后读为空、`cl.exe` 没进 PATH、`ffmpeg-sys-next` 构建脚本失败），
转成 CRLF 即恢复正常。非沙箱环境仍用 `_build-env.bat`（跟随 vcvars 自动升级）。

### 11.29.6 实机验收项（沙箱做不到，留给用户）

1. >10 min 长录无爆音/丢音（环形缓冲 + 优先级分层的最终验收）；
2. 高负载（边录边 `cargo build`）时 `audio_ring_dropped` 仍为 0；
3. 麦克风链路（`recorder_smoke --audio both/mic`）与“设备打不开”报错路径。

### 11.29.7 WASAPI 断供检测与采集会话重建（2026-10-18）

**起因（10 分钟实机取证）**：用户实机录 631.22 s，四层取证全绿，唯一异常是
t=193.77~194.13 s 有 18 个 AAC 最小静音帧（≈353 ms 全零洞），同期视频节奏正常
→ 判定 **WASAPI 环回源头断供**（音频引擎 hiccup / 他程序短暂占用播放设备）。
11.29 的环形缓冲兜的是"消费侧停顿"，兜不了"源头没数据"——这就是本节的动因。

**两边界的界定**

| 现象 | 成因 | 本方案的处理 |
|---|---|---|
| `GetNextPacketSize` 连续返回 0（无数据） | ① 引擎静默（没程序播声音，环回本来就不产包）；② 源头断供 | **只记账不动作**：`DryoutTracker` 累计空窗，>250 ms 告警一次（同一连续段只报一次）。时间轴本来就有墙钟补静音兜底 |
| 采集 API 报错且错误码是 `AUDCLNT_E_*` | 设备失效：切默认输出/拔声卡/音频服务重启/换设备后 mix format 变化 | **自动重建会话**：重解设备 → 重开 → 续录（≤[`crate::recorder::MAX_AUDIO_REBUILDS`] 次） |
| 其他 API 报错 | 配置/用法错误，重建只会得到同样结果 | 按致命处理（整个录制失败，行为与改造前一致） |

**错误分型的依据**（`audio::capture::fault_of_wasapi`，纯函数 + 单测）：
`wasapi::WasapiError::Windows(windows_core::Error)` 携带 HRESULT，
`AUDCLNT_E_DEVICE_INVALIDATED=0x88890004` / `AUDCLNT_E_SERVICE_NOT_RUNNING=0x88890010` /
`AUDCLNT_E_UNSUPPORTED_FORMAT=0x88890008` / `ERROR_NOT_FOUND=0x80070490`
与 `WasapiError::DeviceNotFound` → `ReadFault::DeviceLost`；其余 → `Fatal`。
（`HRESULT(pub i32)` 以补码存负数，比较前 `as u32` 还原成文档里的码。）

**读取线程状态机**（`recorder.rs`，一次循环 = 一次 `read_chunk_core(out, 200ms)`）：

```text
Ok(n>0) ──→ 空窗记账结清（dryout.on_data）→ push 进小环
Ok(0)   ──→ dryout.on_idle(本轮墙上耗时)  累计/取最大；首次越过 250ms → warn 一次
Err(DeviceLost) 且 rebuilds < 3 ──→ take 出旧会话（Drop 即停流）→ 重解设备 → 重开
                                       成功：rebuilds+1、stats.audio_rebuilds+1、INFO 日志
                                       失败：ERROR 日志，放弃该路（余下补静音，录制继续）
Err(DeviceLost) 且 rebuilds >= 3 ──→ ERROR 日志，放弃该路（不再无限重试）
Err(Fatal) ─────────────────────→ ERROR 日志 + RecorderState::Failed + stop（同改造前）
```

退出前（任何路径）把空窗记账写进 `RecorderStats`：`audio_dryout_ms`（累计，含引擎静默）、
`audio_max_dryout_ms`（单源最长一次，`audio_rebuilds`（成功重建次数）。
**判"真的断过"看 `audio_rebuilds > 0`**；`audio_dryout_ms` 大但 rebuilds=0 多半只是静默录制。

**验证**：`test --lib` 323 passed（+4 单测）；e2e 7 passed；release 零告警；
4 s 真录双场景——静默环境空窗≈全程（预期），实时播音环境空窗 0 ms、补静音 16 帧、
188 AAC 包对账 4.03 s、导出 OK（证明有数据时与改造前等价）。
**未实机验证**：切换默认输出设备/拔插声卡触发重建（manual-test 说明第五节）。

**为什么不做更多**：① 不给 UI 加新统计行——会动 i18n 双语文案，触发
`ui::i18n_guard` 回归，而证据链 smoke/日志已足够；② 不做"无声检出后主动放弃音源"——
静默期环回不产包是正常 Windows 行为，与断供无法在本地区分，贸然放弃会把
"用户没播声音"误判成故障。

## 11.30 录屏页"播放无声音"根因与修复（2026-10-18）

### 11.30.1 现象与根因

现象：剪辑页点播放有画面、没声音。根因**不是回归**：`edit::player` 的设计定位就是
"最小可用播放器"——`decode_loop` 只找第一条视频流、只送 RGBA 帧，音频链路从未存在。
录制端（WASAPI→AAC→MP4）经 10 分钟实机四层取证一直正常，所以是"文件有声、应用内播放无声"。

### 11.30.2 播放链新增的一半：`edit::audio_out`

三条纪律与 `player` 对齐，一条是新的：

1. **界面零等待**：打开文件、解码、写设备全在 `clip-audio-play` 线程；
2. **失败只降级不出错**：没有音轨 / 输出设备打不开 / 重采样失败一律记日志后**静音续播**，
   `PlaybackStats::output_live` 记录真实状态。**没有音轨不是错误**（如实降级），
   **没声音也不是播放失败**（所以错误不进 UI）；
3. **seek 与视频同拍**：`reanchor_play` 把同一个 `t` 同时发给视频/音频两条链；
   seek 提前 `AAC_PRIMING_MS`（44 ms）落点，且那段提前量**故意播出来**——
   它正好盖住 AAC 解码器开头的 priming 静默；
4. **写设备必须写静音**：WASAPI 渲染流欠载会停摆/报 discontinuity（"咔哒一声没声"），
   所以设备有空间而队列没数据时要补静音，而不是跳过不写。

关键实现：独立 demuxer/decoder（与 waveform 分段解码同模式，与 player 平行）；
swr 重采样到 48 kHz/2ch/f32（渲染端请求同一格式，autoconvert 不再返工）；
WASAPI Render 事件驱动（`get_available_space_in_frames` + `wait_for_event`）；
待写队列上限 200 ms（超出丢最旧——与采集侧小环同一哲学）。

### 11.30.3 时钟口径（务实取向，遗留待办）

音频侧**不做音画同步**，只按自己的 pts 顺序实时播；两端都从同一 `from_ms` 起跳、
都按墙钟实时，差异只在启动相位（几十毫秒）。真正的"音频主时钟 + 视频追音频"
同步引擎作为独立待办（P32 遗留 1）。

### 11.30.4 验证（2026-10-18）

- `test --lib` **327 passed / 0 failed**（基线 323 + audio_out 4 个）；
- e2e 7 passed；release 0 rustc 告警；
- **`play_smoke` 实机**（新工具 `src/bin/play_smoke.rs`）：一边播正弦 AAC 一边环回采集，
  结果 `PLAY_OK=1`——output_live=true、写设备 2.472 s、解码峰值 0.0907、
  **环回 RMS 0.055 > 0.01**（扬声器出声被系统录到，客观证明有声）。

### 11.30.5 修订：播放卡顿——粒度错配与双线程重构（同日 14:40）

第一版把"解一帧 AAC → 写一批设备"放进同一条线程，实测 **22% 的写入批带静音填充**
（play_smoke 诊断指标 `silent_batches/write_batches`），用户听感 = 周期性咔哒卡顿。
根因是粒度错配：AAC 帧 21.3 ms vs WASAPI 设备周期 ~10 ms，单线程左右横跳时
队列水位在 0 附近抖动，设备来要数据时手里不够一批 → 填静音 → 可闻空洞。

修订为**生产者/消费者双线程**（`edit::audio_out`）：
* `clip-audio-decode`：解码 + 重采样 + 按媒体时刻节流（LEAD_MS=200ms，与 player 同拍），
  经 32 批有界通道交给渲染线程；
* `clip-audio-render`：唯一碰设备的线程，按 `get_available_space_in_frames` + 事件
  的节奏放水；队列恒有 200 ms 存货 → **稳态零静音填充**；
* 语义修正三条：eof 只由渲染线程"放空 + Disconnected"置位（producer 退出≠播完）；
  设备延迟打开（无音轨文件不碰扬声器，开场先囤货再放，无启动爆音）；
  seek 清队不重复扣 pending（防 usize 下溢）。

修复后实测：`silent_batches=0/2014`、`max_write_gap=12ms`、20 s 完整播放、
环回 RMS 0.0624 正常出声。`test --lib` 328 passed（+nap 纯函数测试）。

遗留：seek 瞬间的 AAC priming 44 ms 空窗（固有代价）；音画精确同步引擎（独立待办 B3）。

### 11.31 波形算法：8 分钟分段 + 优先可见区间 + 导入后启动（2026-10-18 晚）

用户三条需求逐条核实：**①"导入完成后再算波形"与 ③"优先算可见区段"此前已实现**
（P23 两阶段导入三判据 + `import_readiness` 超时兜底；P22 补充 1 的
`BasePlan::take_next` 距离优先 + `set_visible` 每帧更新），本次落实的是
**②段长 5 min → 8 min**，顺带把单测口径动态化。

* `SEGMENT_MS` 300_000 → **480_000**（8 min）：段越短 `av_seek_frame` + 开解码器的
  固定开销占比越高，段越长"可见区间插队"越钝（在算的段不打断）；8 min ≈ 一秒内
  解码量，两头都顾上。它正好是 TILE_MS 的 96 倍、BASE_BUCKET_MS 的 4800 倍 ——
  段边界与切片边界严格对齐，写层就是一段连续下标；
* `ALGO_VERSION` 6 → 7（模块纪律：常量一变缓存键就变，旧缓存整体作废重算）；
* **真 worker 集成测试的断言口径修订**（旧口径在满载并行跑时偶发挂）：
  `take_next` 保证的是**领取顺序**按距离排序，而 4 个 worker 的**算完顺序**受调度
  影响可以乱序。改为两条构造保证：**领取不变式**（远处段已开跑 ⇒ 更近的段必已开跑）
  + **第一个算完的段必在最近的 4 段里**（第 5 次领取必须等某个 worker 先算完一段）。
  其余写死 5 min 假设的用例同步改为按常量动态计算。

验证：`test --lib` 328 passed / 0 failed（与改前基数一致，无新增用例）、
`edit::waveform` 47 passed、release 零 rustc 告警。

### 11.32 打开视频不再阻塞界面：流式打开（探测即交 + 续扫关键帧，2026-10-18 晚）

**现象**：部分视频"导入时很慢，象是卡住，过几秒才有反应"。

**根因**：`EditorPage::open` 在 UI 线程（按钮点击回调）里同步调用 `edit::open`，
其中包含**全文件逐包扫描关键帧**（`av_read_frame` 读到文件尾，只解封装）。
代价 ∝ 文件大小：几秒的 clip 是毫秒级（旧注释的假设），10 分钟 1080p 录屏
（几百 MB~GB）就是秒级；冷缓存/机械盘/网络盘更慢。期间窗口一帧不回 = 系统判无响应。
其余链路（首帧预览/缩略图/播放器/波形 worker）本来就在后台线程，可排除。

**改法（三段式）**：

```
edit::open_input_and_info  打开 + find_stream_info → (Input, Clip)   ← 快，毫秒级
edit::probe                = 上面丢掉 Input                          ← 只要元数据
edit::scan_all_keyframes   回绕文件头 + 逐包扫（读错保留已扫部分）    ← 慢，∝ 文件大小
edit::open_streaming       probe → emit(&clip) → 续扫 → 返回 (clip, kfs)
edit::open                 同步包装（烟雾工具/单测/旧调用方，行为不变）
```

界面侧：`open()` 只做内存清理 + 起 `clip-open` 线程就返回；`OpenMsg` 通道
（`Clip` / `Keyframes` / `Failed`，各带**轮次号** `open_epoch`，旧文件的迟到消息丢弃）
由每帧 `poll_open` 收取；打开在途时排 40 ms 兜底重绘（不是心跳：打开一结束就不排）。

**三条配套语义**（缺一条就是 bug）：

1. **首帧优先**：`Clip.keyframes_scanned == false` 时 `thumb_keyframe` 直接取目标时刻
   —— `edit::thumbnail` 自己 seek + 解码，本来就不依赖关键帧表；表回来后再走"最近关键帧"。
2. **导出安全**：表未定稿期间导出按钮**禁用**（`export_enabled()`）+ "正在建立关键帧索引…"。
   空表会让 `snap_start/end` 退化为不吸附 → copy 导出从非关键帧开始 → 产物开头花屏。
3. **自动载入守卫**：`ui()` 的"clip 为空就载入最近录制"旁路必须同时检查
   `open_rx.is_none() && open_pending_name.is_none()` —— 否则打开期间 `clip` 恒为 None，
   它会每帧把最近录制塞进来、盖掉用户刚选的文件（自查发现）。
   **P46 之后这条旁路整体删除**（剪辑页不再自动载入任何东西），守卫也随之失去意义，
   见 §11.46。

**实测**（`p34_probe_vs_open_on_a_long_clip`，114.1 MB / 2 min 1080p）：
probe **6.5 ms**、open **51.1 ms**（扫描段 44.6 ms，页缓存热 2560 MB/s）。
冷盘按设备吞吐：1 GB 机械盘 ≈ 5~10 s —— 与用户观感一致。

**验证**：`test --lib` 334 passed / 0 failed；e2e 7 passed；bins 4 passed
（整窗测试改为"跑真帧等载入"，真覆盖 `poll_open` 生产路径）。
**遗留**：① 扫描不可取消（换文件后旧扫描仍跑完）；② 关键帧表无落盘缓存（二次打开仍要扫）。

### 11.33 编码体检：坏段标记 / 播放跳过 / 导出剔除（2026-10-18 晚）

**用户诉求**：「视频导入完成，波形绘制完成后，能否加入编码检查，标记出编码错误的区块段落，
播放或剪辑到此处时跳过，已防崩溃。」

**先纠正一个前提**：本功能**不是**"防崩溃"。四条链路的 FFmpeg 返回值都检查过，
`player.rs` 里仅有的两处 `unwrap/expect` 都在 `#[cfg(test)]` —— 没有 panic 点。
真正在发生的是**静默截断**：四条链路遇到坏数据时降级方式**一模一样**，
都是把"读错"当成"读完"。

| 链路 | 改造前的写法 | 后果 |
|---|---|---|
| 播放 | `av_read_frame < 0` → 当片尾（不分 `EOF` / `INVALIDDATA`） | 播到坏点就停，像"卡住" |
| 播放 | 不看 `AVFrame::decode_error_flags` | H.264 坏帧照常显示 = 花屏 |
| 波形 | `ret < 0` → `EndReason::Eof` | 坏段之后画成静音，看不出是坏的 |
| 关键帧扫描 | `ret < 0` → `break`，保留半截表，仍置 `keyframes_scanned = true` | **最严重**：坏点之后关键帧全丢 → 后半段吸附失效 → copy 导出从非关键帧起 → **产物开头花屏** |

#### 11.33.1 两档检查（用户定的口径）

| 档 | 实现 | 判据 | 成本 |
|---|---|---|---|
| **快速体检**（随打开自动） | `health::scan_pass`：**一次全片解封装遍历** | ① 读包返回非 `AVERROR_EOF` 负值 → `ReadError`；② 视频包 PTS 倒退 > 200 ms 或跳跃 > 2 s → `TimestampJump`；③ 有视频流却零关键帧 → `NoKeyframe`；④ 实际最大 PTS 比**视频流声称时长**短 > 500 ms → `ShortData` | **零额外 IO** —— 关键帧扫描本来就要逐包读全片，体检只是顺手多记几个判据（`open_streaming` 返回 `(Clip, ScanPass)`） |
| **深度检查**（手动按钮） | `health::deep_scan`：全片解码 | `AVFrame::decode_error_flags != 0`（**连续 ≥3 帧**才算一段）+ `avcodec_send_packet` 硬错误 | 一次全片软解（1080p ≈ 片长的 1/3~1/1）；后台线程 + 每 256 包报进度 + 可取消 |

判据 ④ 是"分片 mp4 被中断"的主判据：这种文件 `av_read_frame` **读得很干净**（正常返回 EOF），
只是数据本身就短了 —— 只看返回值永远查不出来。

#### 11.33.2 硬 / 软是**行为分界线**

```rust
BadKind::is_hard()  // ReadError | ShortData | DecodeError
```

* **硬**：数据确实不能用 → 播放**自动跳过**、导出**自动剔除**；
* **软**（`TimestampJump` / `NoKeyframe`）：只标红提示，**绝不改变播放与导出行为**。

软告警为什么也不许自动跳：时间戳跳变常常只是元数据坏了、画面完全正常；
若也跳，用户会遇到"莫名其妙跳了一段"，比花屏更难接受（用户口径：**仅硬错误自动跳**）。

#### 11.33.3 三处消费，一个真相源

```
health::scan_pass  ──┐
                     ├─→ Clip.health ──┬─→ player::spawn_skipping（播放跳过）
health::deep_scan  ──┘  （merge_deep_into_clip）├─→ paint_bad_ranges（时间轴红带）
                                               └─→ resolve_segments_ex（导出剔除）
```

深度检查的结果**并入 `clip.health`**，不另存一份。最初把它单独放一个 `page.deep` 字段，
结果是"界面说会跳过、导出却没剔"（导出读的是 `clip.health`）—— 自查发现并改掉，有单测钉着。

#### 11.33.4 导出剔除的两条硬约束

`resolve_segments_ex` 在"逐段吸附关键帧"之后，再按硬坏段切开：

1. **切出来的新起点必须重新吸附到关键帧** —— 切除后的起点必然不是关键帧，
   直接 copy 导出会让产物开头花屏（**本功能要消除的正是这个现象，绝不能自己制造**）；
2. **切完 `start >= end` 的子区间丢掉**（不到一个 GOP，导不出东西）；全丢光 → 如实报错
   "选区全部落在编码错误的区块里"，而不是导出一个空文件。

剔除量 `ExportReport.skipped_bad_ms` = `吸附后原始总长 − 实际写出总长`，
界面必须显示（"已跳过约 N 秒"），否则用户会以为导出的就是完整选区。

**幂等性**：干净素材下 `excise` 原样返回、重新吸附落在同一个关键帧上，
所以 `skipped == 0`、区间一字不变 —— 这是"改造前后行为完全一致"的保证（有单测）。

#### 11.33.5 播放跳过为什么不会死循环

`decode_loop` 里落进坏段 → `seek_to(段尾)` → `continue 'playback`。
`AVSEEK_FLAG_BACKWARD` 会把位置落在**前一个关键帧**上，而那个关键帧本身可能就在坏段里。
不死循环靠的是**判断顺序**：`pts_ms + 1 < start_ms` 的热解码判断在坏段判断**之前**，
跳完之后 `start_ms = 段尾`，退回坏段内部的帧会被热解码判断丢掉，走不到坏段判断。

#### 11.33.6 标记（界面）

* **时间轴**：波形区底部 5 px 坏段带（硬=红 `#B42B2B`、软=琥珀 `#C28A1E`），
  与波形/刻度/三根标尺**共用同一个 `x_of`**；**不受 `show_waveform` 影响**
  （关掉波形时最需要看到它）；干净素材零开销（`has_bad` 早退，一次 `rect_filled` 都不发生）。
* **底部文件信息行**：末尾追加"⚠ 编码错误 N 段"（**有坏段才出现**，干净素材一个字符不加）
  + 「深度检查」小链接（检查中显示百分比），悬停状态看每段明细。

#### 11.33.7 验证（2026-10-18）

| 项 | 结果 |
|---|---|
| `test --lib` | **355 passed / 0 failed / 4 ignored**（基线 334 + 21） |
| `test --test e2e` / `--bins` | 7 / 4 passed |
| release | 零 rustc 告警 |
| 真文件对照（`clip_smoke`） | 6 s 正常素材 → `CLIP_HEALTH_BAD=0`、`SKIPPED_BAD_MS=0`、区间 0+6000 不变；同一素材**截断 40%** → `short_data [3567, 6000] hard=true`、关键帧只剩 4 个、导出自动变成 `0+3567`、`CLIP_SKIPPED_BAD_MS=2433` |

**遗留**：① 快速体检抓不到"包结构完好但内容损坏"的花屏（要深度检查）；
② 深度检查无落盘缓存（每次点都重解全片）；③ 坏段不参与剪辑交互（拖进去不拦、不改选区）。

### 11.34 多段合并导出：分段 = **有序播放列表**（P36，2026-10-18 傍晚）

**用户诉求**：设入点/出点 → 添加到片段列表 → 列表可删、可调序 → 影片条标出每段位置 →
导出按钮下方显示总时长 → 按顺序 copy 合并。吸附规则不变（起点向右、终点向左吸关键帧）。

#### 11.34.1 真正的难点：换掉"分段"的语义

改造前的代码把 segments 当作**时间切分**（把影片切成互不重叠的几块）。这个隐含假设散落三处，
**每一处都会让"调序"失效**：

| 位置 | 改造前 | 对调序的破坏 |
|---|---|---|
| `EditorPage::sync_active_segment` | 每帧按 `start` **升序重排** `segments` | 用户拖完顺序，下一帧就被排回去 → 看起来像"拖不动" |
| `edit::resolve_segments_ex` | 硬校验"本段起点不得早于上一段终点" | 把第 3 段拖到第 1 位 → 报"分段无效"，**导不出来** |
| `mux::remux_ranges` 文档 | 写着"`ranges` 必须升序" | **只是调用方契约**：实现是逐段独立 `seek` + `out_end_us` 累加平移，乱序天然正确 |

于是 P36 的第一步是重新定义：**列表顺序即播放顺序，允许乱序、允许重叠、允许同一区间出现两次**。
删掉那两条校验后，干净文件的既有行为**一字未变**（逐段吸附彼此独立），`p35_*` 系列单测守着无退化。

`remux_ranges` 的乱序正确性用**字节**证明（不是"看着对"）：同一对区间按 `[a,b]` 与 `[b,a]`
各导一次，断言时长都是 ≈1500 ms、分辨率相同，但 **`assert_ne!(seq_bytes, rev_bytes)`**。

#### 11.34.2 `sel_*` 的双重身份 —— 最隐蔽的坑

`sel_start_ms` / `sel_end_ms` 同时扮演两个角色：

* **选中某段时**：它是那一段的镜像（拖绿/红把手 = 改那一段）；
* **没选中时**：它是"待添加的选区"（点「添加片段」就把它追加进列表）。

所以 `active_seg` 必须是 `Option<usize>`，而且**「添加片段」之后必须清空选中**。
否则用户接着框选下一段时，每帧的 `sync_active_segment` 会把**刚加的那段**改成新选区 ——
结果是列表里永远只有一个"最新"片段，前面加的凭空消失。
单测 `p36_adding_twice_keeps_both_and_clears_selection` 在两次添加之间**特意插一帧
`sync_active_segment()`** 来复现真实渲染循环（不插这一帧就测不出这个 bug）。

#### 11.34.3 数据模型

```rust
segments: Vec<(i64, i64)>,   // 可为空：空 = 单段模式，走改造前的 export_copy
active_seg: Option<usize>,   // None = 没选中，sel_* 回到"待添加选区"
seg_drag: Option<usize>,     // 正在被拖的芯片下标
```

* `add_segment`：取当前入点→出点**追加到末尾**（不再钳邻居），`ee - es < 50` 报"片段过短"，
  成功后 `active_seg = None`；
* `delete_segment`：未选中时删最后一段，**允许删到空**（回到单段模式）；
* `move_segment(from, to)`：插入语义 —— 先 `remove(from)`，再
  `insert_at = if to > from { to - 1 } else { to }`（少这一句就会整体错一位），`active_seg` 跟着走；
* `segments_total_ms()`：`segment_effective_ranges()` 求和（即**吸附后**的真实导出长度）。

#### 11.34.4 界面：横向芯片，不是竖向列表

第一版按"常驻列表调大最小窗口"做成竖向卡片（序号 + 起止 + 时长 + 行内删除，4 行内部滚动），
**实测把最小窗口顶高 118 px**（`content 635 > viewport 517`），1366×768 装不下。
试过"行数自适应"（降 84）与"列表在场时预览下限 200→120"（降到只差 4），都不理想，
按用户要求**回退成横向芯片**：

* 芯片行约 **23–26 px**（`layout::SEGMENT_LIST_SLACK` 取 26 是上界），`horizontal_wrapped` 放不下自动换行；
* 芯片只写**序号**（「片段 3」），不写入出点（用户第二轮要求）—— 起止位置在时间线上
  一眼就能看到，写进芯片只会让每片变宽一倍；
* 点击 = 选中该段；**按住左右拖动 = 改合成顺序**（指针 x 与各芯片中心比较算落点，松手调
  `move_segment`），拖拽中在目标芯片左边缝画一条 2 px 蓝线当**插入指示**；
* 选中态 `ACCENT` 实心、拖拽中描蓝边、悬停淡蓝底、常态灰边。

**⚠️ 必须是自绘 + `Sense::click_and_drag()`（第一版就栽在这里）**：第一版芯片用的是
`egui::Button`，而 `Button` 默认 `Sense::click()` —— **只感点击、根本不产生 drag 事件**，
`drag_started()` / `drag_stopped()` 永远为假。拖拽分支、落点计算、`move_segment` 全都写好了，
`move_segment` 的纯逻辑单测也全绿，但**界面上按下去什么都不会发生**（用户连报两次
"还是不能移动顺序"）。改成 `ui.allocate_exact_size(size, Sense::click_and_drag())` +
`rect_filled` / `rect_stroke` / `galley` 自绘后才有 `dragged()`。
反向验证：把 `Sense` 临时改回 `click()`，单测
`p36_dragging_a_chip_with_a_real_mouse_reorders_the_playlist` **必红**；改回来必绿。

**净效果：最小窗口 687 → 671**（删掉的视图行省回了一行，比芯片行花掉的还多）。

四个按钮的配色不是随便挑的，各自对应它操作的那根标尺/那个对象：

| 按钮 | 颜色 | 理由 |
|---|---|---|
| 设入点 | `OK_FG` 绿 | 与时间线上的**绿色起点**标尺同色 |
| 设出点 | `DANGER` 红 | 与**红色终点**标尺同色 |
| 添加片段 | `ACCENT` 蓝 | 与芯片/影片条的**蓝色高亮**同色（"加进这份蓝"） |
| 删除片段 | `WARN_FG` 橙 | 唯一有破坏性的按钮，用警示色隔开 |

影片条高亮从灰色改成蓝色系，与芯片选中色同源 —— 用户原话是"目前的灰色很不醒目"。
浓度调过两次：先 `SEG_ACTIVE_FILL` alpha 86（叠白底 `(181,202,248)`），用户仍反馈
"亮蓝色还是太浅，不明显"，**最终 150**（叠白底 `(126,163,243)`）；非当前段保持
`SEG_OTHER_FILL` alpha 34，好让"哪一段被选中"有明确层次。
波形画在这层**之上**（见 §11.12 的绘制顺序），所以加深底色不会压掉波形。
注意 `Color32::from_rgba_unmultiplied` **不是 `const fn`**，const 里必须用
`from_rgba_premultiplied` 并手算预乘值（alpha 150：`0x25×150/255≈22`、`0x63×150/255≈58`、
`0xEB×150/255≈138`）。

#### 11.34.5 布局常量必须实测，不能手改

```
MIN_INNER_H = CHROME_H(126) + PREVIEW_FLOOR_SLACK(4) + SEGMENT_LIST_SLACK(26)
              + max(EDITOR_REQ_H, RECORD_REQ_H)      // EDITOR_REQ_H 本轮 557 → 515
            = 671
```

`EDITOR_REQ_H` 由 `headless_editor_required_height_fits_the_min_window` **实测**钉住；
删掉视图行后实测 515 ≠ 常量 557 —— **按实测更新常量**，而不是反过来改布局去凑常量。
`min_inner_size_covers_every_page` 里 `assert_eq!(MIN_INNER_H, 671.0)` 是防回归的硬断言。

#### 11.34.6 验证（2026-10-18）

| 项 | 结果 |
|---|---|
| `test --lib` | **362 passed / 0 failed / 4 ignored**（基线 355 + 7） |
| `test --test e2e` / `--bins` | 7 / 4 passed |
| release | 零 rustc 告警 |
| 新增单测 | `p36_segment_order_is_preserved_not_sorted`、`p36_repeated_segment_is_allowed`、`remux_ranges_honours_an_unsorted_order`、`p36_adding_twice_keeps_both_and_clears_selection`、`p36_move_segment_reorders_and_follows_the_selection`、`headless_editor_fits_the_min_window_with_segment_chips`、`p36_dragging_a_chip_with_a_real_mouse_reorders_the_playlist`（**真鼠标事件序列**，反向验证过：`Sense` 改回 `click()` 必红） |
| 单段无退化 | `clip_smoke sample-6s-ok.mp4 0 6000` → `SEGMENTS=1`、`SKIPPED_BAD_MS=0`、吸附 0/6000 不变 |
| 播放无退化 | `play_smoke` → `RENDERED_SECS=4.002`、`LOOPBACK_RMS=0.0621`、`MAX_WRITE_GAP_MS=10` |
| 布局实测 | 内容 521.0 / 视口 521.0 / 预览 228.0（zh-CN） |

**遗留**：① 片段重叠时影片条上两段完全重合，靠芯片序号区分，未做视觉错位；
② 芯片换行会多占约 26 px（`SEGMENT_LIST_SLACK` 只算单行），十几段以上最小窗口会不够；
③ 拖拽排序没有键盘微调（上移/下移按钮）。

### 11.37 录制页面 UI 紧凑化（仅间距：PAD_CARD_TIGHT / GAP_CARD_TIGHT / GAP_ROW_TIGHT）（P37，2026-10-18 晚）

用户要求录制页"更紧凑"（附参考图）。**本次只动间距常量，控件尺寸、字号、文案、i18n 一字未改**：

| 常量 | 旧 | 新 | 影响面 |
|---|---|---|---|
| `theme::PAD_CARD_TIGHT` | 10 | 8 | 仅录制页 4 张紧凑卡（`card_tight` 专用） |
| `theme::GAP_CARD_TIGHT` | 8 | 6 | 仅录制页 3 处卡片间距 |
| `theme::GAP_ROW_TIGHT`（新增） | 9 | 6 | 仅录制页行距（采集卡 1 处 + 内容卡 1 处 + 统计/结果卡） |

**为什么不改 `GAP_ROW` 本尊**：剪辑页与设置页共用 `GAP_ROW`，一改就牵动 `EDITOR_REQ_H`
（515，实测钉住）与设置页，两页都要重量。录制页行距是录制页自己的事 —— **单独常量，
改动范围锁死在一页内**。这与 P33（波形分段段长一改、缓存键必须跟着改）是同一条纪律：
常量一动，先在注释里写清"谁会受影响、谁不会"。

**结果**：`RECORD_REQ_H` **396 → 366**（−30px：内边距 16 + 卡片间距 6 + 行距 6 + 差额 2），
中英实测一致（`dump_record_page_heights` 探针）；`MIN_INNER_H` **不变**（671 由剪辑页决定），
录制页在最小窗口下留白从 103px 变成 179px。

**守门**：`record_page_compression_accounts_for_the_spacing_round` 把两轮压缩
（402→396 时间码字号 / 396→366 间距）的账钉成常量关系；
`headless_record_required_height_fits_the_min_window` 逐像素断言"实测 = 常量"。
改间距不是"看着差不多就行"——**每一轮都要重跑探针更新常量，常量与实测不允许长期分叉**。

### 11.38 录制页三项移动：分辨率信息 / 录制范围 / 音源选项并行（P38，2026-10-18 晚）

用户附红框图定了三个目标位置：① 分辨率信息（`全屏录制：1920x1080` 等）到「开始录制」
**左侧**；②「录制范围」单选 +「重新框选」到**显示器下拉右侧**；③ 系统声音/麦克风/下拉/刷新
到「音源（可多选）」标签**右侧**。本质是**两处"两行并一行" + 一处信息搬家**：

| 卡 | 改动 | 高度 |
|---|---|---|
| 采集卡 | 「录制范围」行并入「显示器」行 | 92 → **48**（−44） |
| 内容卡 | 「音源（可多选）」标签行并入复选框行 | 118 → **92**（−26） |
| 状态卡 | 分辨率信息挪进 `right_to_left` 组（按钮左缘） | 52 不变（行内 inline） |

`RECORD_REQ_H` 366 → **296**（P37 间距 + P38 并行）；`MIN_INNER_H` 不变（671 由剪辑页决定）。

#### 11.38.1 合并行真正的风险不是换行，是裁切

换行会让内容变高（layout 测试抓得到）；**控件被 `max_rect` 裁掉时高度一模一样**，
肉眼不盯着看根本发现不了。所以新增守门测试 `record_page_capture_and_audio_rows_fit`：
两种语言各画一遍，拿每行**最后一个控件的右缘**与"卡片内容区右缘"比。
它当场拦下过一次：en-US 采集行初版实测右缘 **830.5**（限 751，超 79px），
最后靠标签列 88→72（`FIELD_LABEL_W_MONITOR`，仅"显示器"行）、下拉 230→200、
「录制范围」标签改自然宽（不套 88 固定框）、行内间距收紧，压到 725.6。

#### 11.38.2 信息不重复原则

分辨率信息上移状态卡后，显示器下拉的**收起态**改用 `MonitorEntry::label_short()`
（只设备名）—— 同一份信息在一屏内出现两遍纯属浪费，且下拉 200px 也装不下
"主显示器 · SE2717H/HX 1920x1080"（≈220px）。展开的列表项仍是全标签（弹层自适应宽）。

#### 11.38.3 egui 的"双计行距"

竖向布局里显式 `add_space(6)` 后紧跟控件，`item_spacing.y = 8` **还会再算一次** →
实际行距 14。P38 的压缩分解（采集 −44 = 一行 30 + 14；内容 −26 = 标签 16 + 10）
只有按这个口径才对得上账。写"省了 X px"之前先确认有没有被双计，否则常量解释永远差一截。

### 11.39 剪辑页「片段列表」标签 + 影片条选中片段改桔色（P39，2026-10-18 晚）

用户两条：① 芯片行前面加「片段列表」；② 点片段后影片条高亮由蓝改**桔**。

#### 11.39.1 标签不独占一行

「片段列表」是 `horizontal_wrapped` 流的**第一项**（小字次要色 + 8px），跟着芯片一起排；
这一行的高度直接进 `SEGMENT_LIST_SLACK`（26，只按单行算），独占一行 = 最小窗口 +23px。
守门测试 `segment_label_sits_before_the_first_chip`（中英）：标签在第一个芯片左边、
同一行、所有芯片同一行——防"文案变长把芯片挤下去"。为此 `TestRects` 加 `seg_label`、
`Measured` 加 `seg_chips` / `seg_label`（`Measured` 只在一处构造，加字段没有别处要改）。

#### 11.39.2 为什么影片条选中色从蓝改桔

影片条上同时存在四类标记：绿=起点、红=终点、蓝=当前标尺、红/琥珀=坏段带。
选中片段再一片蓝（`SEG_ACTIVE_FILL` = `ACCENT` @ 150）时，**它和"当前标尺"同色相**——
用户分不清"选中的是片段还是标尺"。改 `#EA580C`（桔）后色相彻底分开；
浓度沿用 150（波形画在高亮层之上，改色不压波形）。**芯片本身仍是蓝**（用户只说"影片条中"）：
于是芯片蓝=选中、影片条桔=正在编辑的片段、影片条淡蓝=其余片段（`SEG_OTHER_FILL` 不变），
三者分工明确。没选更黄的琥珀色（`#C28A1E`）是因为软告警带已经占了那个色域。

#### 11.39.3 顺手修掉的一个告警

标签矩形探针最初只写在 `#[cfg(test)]` 块里 → 发布构建判 `seg_label` "不需要 mut"（`unused_mut`）。
改成**无条件赋值**、只让读取端 `cfg(test)`——"只写不读"的变量在发布构建一样会被告警盯上。

### 11.41 去掉剪辑页红框内按钮的颜色（P41，2026-10-18 晚）

用户原话「去掉红色方框内按钮的颜色」（附图 2c.png 两个红框）：
① 设入点 / 设出点 / 添加片段 / 删除片段；② 播放 / 导出。

#### 11.41.1 中性按钮只有一个来源：`theme::neutral_button`

六颗按钮统一走一个新积木 `neutral_button(ui, text, size, enabled)`：
静息取当前主题 `widgets.inactive`（白底 + 发丝描边 + 正文色，与 `ghost_button` 同一套默认样式，
差别只在"怎么放"：调用方用 `add_sized` / `put` 摆）；禁用取 `inactive.bg_fill` 灰底 + `MUTED` 文字。
**颜色不写死**，全部读主题控件样式 —— 这样即使将来做深色主题，也不会有"一颗白按钮挂在深灰页面上"。

#### 11.41.2 为什么禁用态要显式给一块灰底

egui 的禁用态与静息态**同色**：`Ui::disable()` 只做两件事——把 `enabled` 置 false（不响应），
以及 `painter.multiply_opacity(visuals.disabled_alpha())`（默认 0.5）。所以"看起来一样但不响应"
是 egui 的既定行为。而剪辑页这几颗按钮的禁用是有语义的（还没点过时间线 → 设入点/设出点不可用；
正在导出 → 导出不可用），必须看得出来，因此 `neutral_button` 在 `!enabled` 时显式给灰底。
（也因此，P41 的守门测试必须先把六颗按钮置于"可用"状态再量颜色：禁用态量到的是半透明色
`#F1F3F680` 这种，跟静息色没法直接比。）

#### 11.41.3 颜色之外一律不动

四颗 mark 按钮只换"怎么上色"，宽度（96/96/108/96）、高度（32）、圆角（6）、字号（15）
与禁用条件全部不变 → **布局零变化**，`EDITOR_REQ_H` / 最小窗口尺寸不需要重新实测。
「导出」由 `primary_button_in` 换成 `neutral_button` + `ui.put(rect, ..)`（位置仍由调用方算好的
矩形决定，预览是流式的，不能靠布局游标）；`primary_button_in` 无其它调用方，已删除，
删除原因记在被删处的文档注释里。录制页「在剪辑器中打开」与弹窗「确定」仍用 `primary_button`（蓝）。

#### 11.41.4 守门测试为什么量"真画出来的矩形"

颜色是**绘制期**才定的事：源码里没有 `.fill(..)`，也可能被上游样式（主题/作用域样式/禁用透明度）
改掉。所以 `p41_red_box_buttons_have_no_semantic_fill` 遍历本帧 `FullOutput.shapes` 里
覆盖每颗按钮的 `Shape::Rect`，断言：① 不得是语义色（`OK_FG`/`OK_SOLID`/`DANGER`/`ACCENT`/`WARN_FG`）；
② 至少有一层是中性控件色。该测试做过一次**反向验证**（把 `neutral_button` 临时填成 `DANGER`，
测试确实报 `[设入点] 按钮底色仍是语义色 #DC_26_26_FF`），确认它守得住。
为此 `TestRects` 增加 `add_seg_btn` / `del_seg_btn`（六颗按钮都要能报到矩形）。

### 11.40 深色模式：主题从"34 个常量"到"两套 Palette"（P40，2026-10-19 早）

用户四条要求：设置页去掉「常规」「剪辑」「视频」三个分区标题；在界面语言后面加
**深色模式**开关；剪辑页在「片段 1/3」后面加「拖拽可以改变片段顺序」。

#### 11.40.1 为什么提成 `Palette` 而不是常量

深色主题要求"同一组语义在两种主题下取值不同"（`bg` 在浅色是 `#F7F8FA`、深色是
`#16181D`）。**常量做不到"同一个名字两种值"**；提成结构体后，`theme::text()` 这类
取色函数读"当前主题那一份"即可，调用点几乎不用改（98 处 `theme::TEXT` → `theme::text()`
是机械重命名，逐文件核对过计数）。字段分组即语义分组：中性 → 成功/警告/错误/信息/危险
→ 时间线 → 控件级 token。

#### 11.40.2 主题存在哪儿：线程局部 + 双样式键

`ThemeKind` 的当前值放在 `thread_local KIND` 里。为什么不显式传参：取色函数
（`theme::text()`）**没有 `&Context` 参数**，而 egui 只在主线程画 UI，线程局部因此
足够，且 98 个调用点零改动。`apply_kind(ctx, kind)` 做三件事：
① `KIND.set(kind)`（取色函数的唯一依据）；② `ctx.set_theme(kind.preference())`
—— **钉死** Light 或 Dark，**不跟随系统**（`System` 会让界面跟着操作系统跳，
出现"系统是深色但程序画浅色"这种说不清的状态）；③ 把同一套样式写进 `Theme::Light`
与 `Theme::Dark` **两个** egui 主题键（否则 egui 内部一切换就打回它自带的默认皮肤）。

#### 11.40.3 深色那份的三条口径

① **只有颜色变**：间距/圆角/字号/控件尺寸与浅色完全相同 → 两套主题下三页布局一模一样，
`ui::layout` 量出来的必需高度都成立（这是"深色不重新实测窗口尺寸"的原因）；
② **色相不跑偏**：强调仍蓝、成功仍绿、警告仍琥珀、错误仍红，只按"深色底上要读得清"
重挑亮度与饱和度；③ **不用纯黑**：页面底 `#16181D`、卡片 `#212630`、轨道 `#12151C`，
靠一档亮度分层（深色下看不见阴影）。亮度下限：正文在卡片上 ≥ 4.5:1、次要文字 ≥ 3:1
（`p40_dark_palette_is_a_dark_room_version` 用 Rec.709 相对亮度逐条断言）。

#### 11.40.4 时间线那 15 个颜色为什么不走 `theme::*`

`theme::*` 只区分**语义**，不区分"画在哪个槽里"。时间线的波形/标尺/片段高亮全画在
同一条轨道上，浅色那套按"浅灰底上的中深色"挑，深色必须整体提亮一档
（`#475569` 画在 `#12151C` 上等于消失）。所以 `editor.rs` 用 `tl(light, dark)`
按 `theme::is_dark()` 现场选值；`p40_timeline_colors_flip_with_dark_mode` 断言
15 个角色深浅**必不相等**，且画在深轨上的四个（波形/硬错误/软告警/编号）必须**更亮**。
另有四个颜色只是换主题取色函数（`cursor/start/end` → `theme::cursor/ok_fg/danger`），
`state_text`、`status_line`、显示器枚举失败提示同理（深色下 `#DC2626` 太闷）。

#### 11.40.5 开关的落点与"勾选即换肤"

「深色模式」复选框排在「界面语言」那一行**下面**（两者都是"换一套外观"）。
点击时直接 `theme::apply_kind(&ctx, ThemeKind::from_dark_mode(dark))` ——
**同一帧剩下的控件就按新主题画出来**，不等到下一帧；`cfg.dark_mode` 跟着置位，
由既有的"调整即保存"链路在帧末 `sanitized()` + 落盘。
`Config::dark_mode` 用容器级 `#[serde(default)]`：老配置缺键 → `false`
（= 一直以来的浅色皮肤），`sanitized()` 不碰它（`p40_dark_mode_defaults_to_false_for_old_configs`）。

#### 11.40.6 删标题不删内容；提示不新增条件部件

三个分区标题删的是"同一张卡片里再念一遍卡片名"的那一行（label + 分隔线），
卡片里的设置项一个没动。剪辑页的「拖拽可以改变片段顺序」放进片段计数的
**同一个条件块**（`!segments.is_empty()`）里，与计数器一起出现/消失：
egui 的自动 id 按部件序号递增，单独包一个 `if` 会让 id 在有无片段之间错位，
把时间线的悬停弄丢（`p40_drag_hint_follows_the_segment_counter` 同时断言语出、
位置在计数右侧同一行、空片段时一起消失）。

#### 11.40.7 修订：深色模式与语言同一行、快捷键选项与开关同一行（P40 修订，2026-10-19 上午）

第一版把深色模式放在了语言**下面单独一行**、快捷键选项放在开关**下面另起一行**（还带一个
「全局快捷键」标签 + 左边一块等宽空白占位）。用户两条修订（附图 7.png）后改成：

* **语言行**：`ui.horizontal` 里先 `field_row(界面语言 + 下拉框)`，`GAP_SECTION`（18px，
  比行内 spacer 大一档，避免读成同一个控件）之后紧跟深色模式复选框。换肤逻辑不变
  （当场 `theme::apply_kind` + `cfg.dark_mode` + 调整即保存）。
* **快捷键行**：`ui.horizontal` 里先放「启用全局快捷键」复选框，`GAP_ROW` 之后把三个预设
  单选与**自定义时的捕获框**一起交给 `add_enabled_ui(cfg.global_hotkey_enabled, ..)`
  （关掉开关整组置灰、位置不变）。`hotkey_label` 标签删除，语言包里对应的键也删掉
  （两个语言文件同步删，key 集合仍对齐）。捕获失败的 `banner` 是**通栏部件**，
  不能 inline 在选项行里，挪到下一行。

**测试台必须装真字体**（这条是本节最重要的附带结论）：无窗口布局配 `FontDefinitions::empty()`
时中文 galley 尺寸全为零，整页被压成"零宽文字"的布局 —— 位置虽然有序，宽度全是假的，
量不出"这一行放不放得下"，也量不出"谁在谁右边"。所以 `settings.rs::tests::Rig` 改成与
`main.rs::install_cjk_font` **同一条路径**（内嵌中文子集 + egui 内置字体）。
附带一个 epaint 细节：**`galley.rect` 在分帧化之前是空的**，文字矩形要逐行
`PlacedRow::rect()` 求并集、再平移 `TextShape::pos` 才是可靠尺寸。

### 11.42 默认主题翻成深色：一个字段的翻转，与"默认值永远要让位于显式选择"（P42，2026-10-19 上午）

用户一句话："设置默认主题为黑色，默认快捷键为ctrl+alt+r"。

* **默认主题 = 深色**：`Config::default().dark_mode` `false` → `true`。容器级
  `#[serde(default)]` 的语义（缺字段 → 取 `Config::default()`）让这条链自动覆盖三种
  入口：新装（无配置文件）、升级（老配置没有这个键）、以及显式 `true`。
  **显式写了 `false` 的用户保持浅色** —— 默认值管"没人表态"的情况，
  表过态的一律以表态为准。`p42_dark_mode_defaults_to_true_for_old_configs`
  把这两个方向都钉死（缺键 → true；显式 false → `sanitized()` 不吞、往返不变）。
* **默认快捷键 = Ctrl+Alt+R**：代码里本来就是，无需改动。三个单一来源
  （`HotkeyPreset::default()` / `Config::default().hotkey_preset` /
  `tray::DEFAULT_HOTKEY = PRESET_CTRL_ALT_R`）都是它；只有用户机器上的旧显式选择
  （`ctrl_shift_r`）盖着它，改配置即可。**"用户说默认是 X，但代码默认本来不是 X"
  与"代码默认是 X 但用户的旧配置盖着"是两回事** —— 前者要改代码，后者改数据。
* **`ThemeKind` 的 `#[default]` 仍留 `Light`**：它只服务"还没应用过主题"的两个瞬间
  （线程局部 `KIND` 的初值 = 无窗口单测各自线程上的默认；`install_cjk_font` 里那次
  兜底 `apply()`）。用户看到的默认永远走"配置 → `apply_kind`"这条正路，
  enum 默认值不是产品默认。文档里写清楚"哪个默认是给用户看的、哪个只是兜底"，
  否则下一个人会以为是漏改。
* **守门测试要自己声明起始状态**：`settings.rs` 的 `Rig` 原来直接 `Config::default()`，
  默认一翻转，`p40_dark_mode_switch_toggles_theme_and_persists` 的脚本
  （浅色 → 点一下 → 深色 → 再点 → 浅色）就会整个错位。改成显式
  `Config { dark_mode: false, ..Config::default() }` —— **测试的起始态是测试自己剧本的
  一部分，不能默认"产品默认永远不变"**。

### 11.43 启动顺序的"三段式"与它的边界（P43 复核，2026-10-19 上午）

用户问："启动过程是先进入 UI 界面，再开始检查显示器、麦克风等硬件，最后再加载 ffmpeg 的吗？"
答案是**是**，而且这不是巧合，是三条独立决定叠出来的结果：

| 段 | 机制 | 谁负责 |
|---|---|---|
| ① 先出 UI | 首帧之前只做"画第一帧必需的事"：装子集字体（0.3 ms）、读配置（0.1 ms）、应用主题（0.0 ms）、建三页（4 ms）、挂托盘/热键（~3 ms） | `App::new` + `update()` 的启动分支 |
| ② 再查硬件 | `DEFER_INIT_FRAMES = 2`：显示器/麦克风枚举、输出目录回退链探测，全部推到首帧之后的 `flush_deferred_init` | `MainPage` |
| ③ 最后碰 FFmpeg | 5 个 DLL 是 `/DELAYLOAD`（进程启动**零映射**）；等**显示器与音频都枚举完**，才在后台线程 `LoadLibraryW` 逐个预映射 | `startup::preload_ffmpeg_dlls_once` |

**为什么这个顺序是对的**：用户感知的"打开快慢"只取决于首帧时刻；②③ 都在首帧之后，
做得再慢也不影响"窗口出画面"，但它们**必须在用户按录制之前做完**（否则第一次录制会顿）。
②→③ 的依赖关系也是有意的：预加载要等枚举结束，避免和 WASAPI/DXGI 的初始化抢同一批系统资源。

**边界（不要误以为还能无限优化）**：首帧 ≈ 310 ms 里，窗口创建 ~100 ms + wgpu 设备/交换链 ~110 ms
是**驱动与框架侧**的固有成本；我们的代码只占 5 ms。实测把 `WGPU_BACKEND` 钉成 DX12 反而更慢更不稳，
所以**不做后端选择**（默认 `Backends::all()` 让驱动自己挑）。exe 17.12 MB 且 PE 里没有 .debug 段，
`strip = true` 拿不到收益，也不要加。

**打包口径（本轮踩到的真问题）**：运行期加载哪个版本的 DLL 完全由"exe 旁边放的是哪 5 个文件"决定。
`manual-test\` 曾放着 BtbN 全功能版（`avcodec-63.dll` 91 MB），而 `dist\` 与 §11.19 的口径是自建最小版
（合计 4.5 MB）。两者功能等价（`clip_smoke` / `play_smoke` 都 PASS），但冷启动时后台预加载要读
118 MB 而不是 4.5 MB —— **发布/手动测试目录一律用 `ffmpeg-min\bin` 的 5 个 DLL**。
判断方法：`ls -la` 看 `avcodec-63.dll` 是 2.2 MB（最小版）还是 91 MB（全功能版）。

### 11.44 版本号：一个常量、两个消费方，以及"全等匹配"的静默陷阱（P44，2026-10-19 上午）

用户："v1.0 版的 ui 和功能设计就可以了，在左上角 CapLite 处添加版本号，这版就定为 v1.0.0。"

* **显示位置 = 系统标题栏**。界面上"左上角的 CapLite"只有标题栏那一处（内容区从 P14 起
  就故意不重复应用名），所以改的是 `ViewportBuilder::with_title`，**内容区布局零改动**。
  这一条值得写进设计文档：用户说"v1.0 的 UI 就这样了"时，任何"顺手加个标签"都是在动定稿排版。
* **版本号只有一个来源**：`APP_VERSION = env!("CARGO_PKG_VERSION")`，
  `WINDOW_TITLE = concat!("CapLite v", env!("CARGO_PKG_VERSION"))`。
  两处字面量必然漏改一处 —— 这类"同一事实写两遍"的债，本项目已经付过一次
  （文档里的实测数字、`build.rs` 里的 DLL 名，都靠测试兜住）。
* **静默陷阱（本轮真正的收获）**：`tray::find_main_proc` 原来按**全等**比对窗口标题来找主窗口。
  标题从 `CapLite` 变成 `CapLite v1.0.0` 之后，这条路径会**无声失效** ——
  托盘图标点不动窗口、没有任何日志、测试也不会红（它只在真机上跑 Win32 枚举）。
  改成 `starts_with(APP_NAME)`：窗口类 + PID 已经把它锁得很死，前缀匹配足够，
  而且对"标题后面再加东西"免疫。**结论：凡是拿字符串当身份用的地方，优先前缀/结构化，
  别用全等 —— 全等的失效方式是"静默"。**

### 11.45 健壮性口径：锁毒化、落盘节流与"空闲不耗电"（P45，2026-10-19 上午）

#### 11.45.1 后台线程的 panic 不许传染给 UI

共享状态（录制状态机、帧计数、音频环形缓冲）被采集/编码/音频线程与 UI 线程同时访问。
`Mutex::lock()` 在**持锁线程 panic 之后**返回 `Err(PoisonError)`；调用点若 `.unwrap()`，
"某个后台线程偶发 panic" 会变成 "UI 每帧取状态时也 panic" —— 一次局部故障升级为整个进程消失。

**口径**：这些锁里的数据没有跨字段不变量（写入都是一次赋值），所以用
[`sync::lock_or_recover`](../../src/sync.rs) **恢复内部数据继续用**（`into_inner()`），
最多丢一次计数。全项目 34 处（`recorder.rs` 26、`audio/ring.rs` 8，含 `Condvar` 的
`wait_timeout`）统一走它。**不要**用它包"必须整体原子更新"的临界区 —— 那类不变量应该
收进一个结构体，让一次写入完成整组更新。

#### 11.45.2 落盘：只有一条路，而且要节流

* **单一路径**：设置页不再自己 `store.save`（参数已删掉，从类型上消灭第二条路），
  统一由外壳 `commit_cfg()` 落盘 —— 否则"每帧两次序列化 + 临时文件 + rename"。
* **节流**：`CFG_SAVE_MIN_GAP = 200ms`。改动立即进内存（用户看到的永远是最新值），
  磁盘最多每 200ms 一次。
* **不丢改动的三条兜底**：松手（`pointer.any_released`）、节流到期（`request_repaint_after`
  唤醒一帧后**直接补写**）、`on_exit`。
* 两个必须记住的坑：
  1. **不能只判断"松手"**：拖完停住不动就没有输入事件了，改动会一直挂在内存里；
  2. **到期分支不能再 `request_repaint_after(0)`**：那会变成空转重绘（每帧都来、永远不写）。

#### 11.45.3 "空闲不耗电"的验收口径

`scripts/resource-sampler.ps1`（本轮新增）给出可复核的数字：**空闲 CPU 基线 0%**，
偶发 3–9% 尖峰（15–45 ms，非周期），工作集/句柄/线程 16 秒零增长。
判据很直接：**如果每个 500ms 采样点都是 30–60%，就是空转重绘**（例如某个
`request_repaint_after(0)` 或每帧无条件 `request_repaint()`）；零星尖峰是系统事件唤醒，正常。

内存口径：工作集 248.6 MB / 提交 506 MB。构成是有界常量（wgpu 设备、winit、托盘、
解码队列 `QUEUE_CAP = 8` 帧、线程栈），不是"越用越多" —— 采样曲线平的就是证据。

### 11.46 剪辑页只认显式打开：删掉两条自动载入（P46，2026-10-19 上午）

**需求**：编辑器启动不自动载入任何视频；只有用户在录制页底部点「在剪辑页打开」时，
才切到剪辑页并打开刚录的那条。

#### 改造前的两条路（这才是"它总是自己冒出一条视频"的原因）

| # | 位置 | 触发条件 | 结果 |
|---|------|----------|------|
| ① | `EditorPage::new(cfg)` | `cfg.last_video_path` 指向的文件存在 | `open()` 它 |
| ② | `EditorPage::ui()` | `clip` 为空 **且** 没有在途打开 | `open()` 录制目录里最新的 mp4 |

①原本被 `App` 改成 `new_without_autoload()` + `pending_editor_open`，
推迟到首帧之后再由 `flush_deferred_init` 执行 —— **推迟不是取消**，启动完照样载入。
②是"兜底"：就算 ① 没命中（`last_video_path` 被删/挪走），进剪辑页也会自动冒出最近那条。

两条都删。现在把文件交给剪辑页的入口只剩三个，**全是用户动作**：
录制页底部「在剪辑页打开」（`App::open_in_editor`）、剪辑页「打开视频」/「重新载入」。

#### 语义边界（哪些留着、为什么）

* `cfg.last_video_path` **留着**，但降级为"目录线索"：`EditorPage::last_video_dir` 用它给
  「打开视频」的文件对话框兜底定位。它**不再**驱动任何载入。
* `set_output_dir()` 留着（对话框默认落在录制目录），但不再需要"作废最近录制缓存"。
* `pick_recording_dir()` / `recording_dir()` 留着：两个文件对话框（打开 / 导出）的起始位置。
* `recent_recording()` / `newest_recording()` / `recent_cache` **删除** —— 它们唯一的调用者
  就是被删的 ②。连带 4 条测试与 `set_mtime` / `test_dir` 两个辅助函数一起删：
  被测函数不存在了，测试留着只会永远绿着、什么也不证明。

#### 两个"看起来能测、其实测不出来"的坑

1. **`loaded_path() == None` 不能证明"没打开"**。`open()` 是异步的（§11.34），
   "正在打开"期间 `clip` 也是 `None` —— 改造前的代码同样会让这条断言通过。
   于是新增 `EditorPage::is_opening()`（`open_rx.is_some() || open_pending_name.is_some()`），
   守门测试断言"**连打开都不该起头**"。
2. **假 `.mp4` 不能当素材**。老逻辑会真的 `open()` 一个假文件 → 异步失败 →
   `loaded_path()` 仍是 `None`。守门测试必须用**随包 ffmpeg 生成的真素材**，
   才能区分"没打开"与"打开了但失败"。

两条都是用**变异检验**验出来的：临时把自动载入加回 `flush_deferred_init`，
第一版测试失败在"30 s 内没载入"（位置不对 → 说明抓错了）；补上 `is_opening()` 之后再验，
在启动那条断言上 2.5 s 内失败。随后删除变异。

#### 守门测试

* `ui::editor::tests::p46_new_never_autoloads_last_video_path` —— 构造器一侧：
  真素材 + `last_video_path` 指向它 → 不载入、不在途、不报错；随后显式 `open()` 仍然正常。
* `tests::p46_startup_never_opens_last_video`（`src/main.rs`）—— 外壳一侧：
  真配置里带 `last_video_path`，跑 4 帧（含 `flush_deferred_init`）→
  `loaded_path() == None`、`!is_opening()`、仍停在录制页；
  随后 `open_in_editor()` → 切到剪辑页并真的把那条素材载入。

#### 界面

空状态文案改成两条真实入口（`no_video_hint`，中英各一份）：
`还没有打开视频：点上面的「打开视频」选择文件，或在录制页点「在剪辑页打开」。`
旧文案是"或先录一段再回到这里"—— P46 之后"回到这里"不会自动有内容，必须说清怎么进来。

### 11.47 代码体检的口径：什么样的"错"必须改、什么样的"警告"不该动（P47，2026-10-19 上午）

本轮做了一次全量体检（clippy + 死代码扫描 + 人工复核）。记录**判据**比记录清单更有用。

#### 一、`pub fn` 解引用裸指针却没标 `unsafe` —— 这是真错误，不是风格

`video/openh264/win.rs` 的 `free_library` 原本是 safe 公开函数，里面 `unsafe { FreeLibrary(h) }`。
安全性契约（"h 必须是 LoadLibraryW 返回且未释放的句柄"）只写在注释里 —— 类型系统不管。

判据：**只要一个函数的正确性依赖调用方遵守一条注释，它就必须是 `unsafe fn`**，
否则这条注释等于不存在。`clippy::not_unsafe_ptr_arg_deref` 是 deny-by-default，
所以这处的存在还说明"这个仓库从来没跑过 clippy"—— 这比单个 bug 更值得记。

#### 二、死代码的判据：零调用 = 删，除非它是**声明**而不是**逻辑**

扫描法：抽出所有 `pub fn/const/struct/enum/type`，统计标识符在 `src/` + `tests/` 的出现次数，
取只出现一次的（= 只有定义），再人工确认。本轮 19 个，约 200 行。

两类**保留**：

* **FFI 镜像常量**（`openh264/abi.rs` 的 `NAL_*` / `VIDEO_FORMAT_*` / `LEVEL_*`）：
  `const` 不产生代码，留着是"这份 ABI 长什么样"的文档；删一半反而误导。
* **公开但只被 cfg(test) 用到的辅助**：本轮没有这种情况，但判据相同 ——
  被测函数若已删除，测试也必须删（否则它永远绿着、什么也不证明，见 P46）。

#### 三、clippy 建议也要审：`as_chunks` 在 stable 上编译不过

3 处 `chunks_exact(N)` 被建议改成 `as_chunks::<N>()`，而 `slice::as_chunks` **至今是 nightly**。
照抄建议 = 代码直接编译失败。所以"clippy 全绿"不能当目标，**"每一条都判过"**才是目标。

其余没动的：doc 缩进（11 处，中文长注释里纯噪声）、`too_many_arguments`（4 处，拆参数要动调用链）、
以及 4 类纯风格项。

#### 四、本轮改了什么（都有等价性保证）

`is_multiple_of` ×9、`div_ceil` ×2、`run_end + 1 <= last` → `run_end < last`、
冗余 cast ×3、`&format!("{}", s)` → `&s` ×2、多余借用 ×2、手写 `impl Default` → `derive` ×2、
`iter().any(...)` → `contains` ×1、索引循环 → `enumerate` ×1、可合并 `if` ×1、`&PathBuf` → `&Path` ×2。

其中 `run_end < last` 这一条不只是好看：原式 `run_end + 1` 在 `run_end == usize::MAX` 时会溢出 panic
（当前不可能发生，但"不可能"是别处的约束，不该由这里承担）。

#### 五、验收口径

`cargo clippy --lib --bins` **exit 0**（lib 30 / bin 0 / error 0）；
三套测试全绿；真实录制闭环与 P45 基线逐项一致（90 帧/2999 ms、60 帧+94 AAC/2006 ms）。
**每一条没改的告警都在 PENDING 里写了理由** —— 免得下一轮有人当成漏项重做一遍。

### §11.48 P48：语言包、配置字段、lint 面 —— 三类"不报错的死代码"

P47 扫的是"零调用的公开项"。本轮换三个维度，发现的共同特征是：**它们都不会让编译器出声**。

#### 一、`#[serde(default)]` 让"兼容字段"成了一个自我实现的谎言

`Config` 上有四个字段（`enc_speed` / `rate_control` / `encoder_threads` / `entropy`），
文档注释写着"保留字段是为了旧配置文件仍能反序列化"。**这个理由永远是错的**：

* serde 对未知键的默认行为就是**忽略**（除非显式 `deny_unknown_fields`）；
* 所以老 `config.json` 里留着 `"enc_speed": "Fast"`，新版本照样能读，那个键被静静丢掉；
* 也就是说，字段存在的唯一效果是"让 `sanitized()` 有东西可赋值、让测试有东西可断言"——
  **一个字段只写不读，它就不是配置，是注释**。

判据：**字段的价值 = 有读取方**。没有读取方，无论注释怎么写，它都该删。
删完之后要留一条守卫测试（`p48_removed_legacy_fields_are_ignored_not_rejected`）：
它的作用不是测"删对了"，而是**钉住"不许加 `deny_unknown_fields`"** ——
一旦有人为了"配置写错要报错"加上它，所有老用户的配置会直接读不出来。

连带教训：`entropy` 是 `prefer_cabac` 的镜像。**镜像字段一定会分叉**，
因为写镜像的代码（`sanitized()`）和读镜像的代码不存在对称约束。宁可只有一个来源。

#### 二、i18n key 的死活：字面量扫描 + 动态前缀白名单

判据：key 的字面量 `"key"` 在 `src/**/*.rs` + `tests/**/*.rs` 里一次都没出现 → 死 key。

**唯一的例外必须显式列出**：`bad_*` 是 `format!("bad_{}", b.kind.key())` 拼出来的，
完整字面量永远不存在。所以白名单是 `const DYNAMIC_PREFIXES: &[&str] = &["bad_"];`
—— 而不是"看到像动态的就跳过"。白名单要短、要有据可查。

守卫测试（`every_locale_key_is_referenced_in_source`）有一个容易被忽略的细节：
**必须断言"源码真的读到了"**（`assert!(!src.is_empty())`）。否则路径写错、递归没进去，
测试会永远绿着，比没有测试更糟。

#### 三、clippy 归零的意义是**信噪比**，不是"听话"

P47 留了 30 条告警并逐条写了"不改的理由"。P48 把它们清零了 —— 理由不是"P47 判断错了"，
而是：**常年挂着 30 条，第 31 条（真的）出现时没人会看见**。

清零的正确姿势是分三类，而不是无脑 `--fix`：

| 类型 | 做法 |
|------|------|
| 语义等价、可读性更好 | 直接改（`fill`、`contains`、结构体更新语法、模式折叠） |
| **建议本身在 stable 上不可用** | 就地或整仓 `allow` + 写明"为什么这条建议不能照做"（`as_chunks` 仍是 nightly） |
| **签名本来就该宽** | 就地 `#[allow]` + 一行理由（线程入口的独立输入、纯函数的策略旋钮、绘制函数的外部状态） |

判据：`allow` 不是"消音"，是**把判断写进代码**。没有理由的 `allow` 和没有 `allow` 的告警一样坏。

#### 四、孤立的文档注释：一种没有任何报错的失效

两处 `///` 和它下面的 item 之间空了一行 —— rustdoc 就不把它挂到任何东西上了，
**编译不报错、测试不报错、页面也不报错**，只是那段文字从此没人看得见。
`clippy::empty_lines_after_doc_comments` 是唯一能发现它的东西。

这条直接推翻了 P47"doc 缩进纯属噪声"的判断：那 11 条里，2 条是**孤立注释**（真失效），
2 条是**行首 `+` 被 markdown 当成列表项**（`a + b + c` 断行成 `+ c`），其余才是缩进。
所以"看起来像格式问题"的 lint，要先看一眼**渲染出来是什么样**再决定改不改。

#### 五、验收

`cargo clippy --lib --bins` **0 warning / 0 error**；三套测试 377 / 6 / 7 全绿；
真实录制闭环与 P45 基线逐项一致；release 零 rustc 告警；exe 小 11,776 字节。

### §11.49 P49：文档链接、依赖清单、重复实现 —— 编译器不看的三块

P47 扫零调用项，P48 扫死 key / 死字段 / lint 面。这轮的三块有一个共同点：
**编译器对它们完全沉默**，只有专门跑一次才会出声。

#### 一、`cargo doc` 是唯一会检查"文档里的代码"的工具

`cargo doc --no-deps --lib` 一次吐出 81 条 `broken_intra_doc_links`，说明这个仓库
**从来没跑过 `cargo doc`**。这些链接不影响编译、不影响运行、不影响测试，
只是"文档里指向的东西没了" —— 而这正是**代码改过名之后唯一留下的痕迹**：

| 断链 | 现状 |
|------|------|
| `waveform::LevelSet` | 现在是 `BasePlan` |
| `waveform::decode_with_progress` | 现在是 `segments_for_view` |
| `startup::set_sink` | 现在是 `enable` |

换句话说：**断链清单 = 一份自动生成的"改名/删除遗留"审计报告**，比人工 grep 可靠得多。

#### 二、`//!` 模块文档的作用域：裸名与 `self::` 都不行，只有 `crate::` 行

这条踩了三次才确认，值得单独记：

```rust
//! [`BASE_BUCKET_MS`]                      // ✗ no item named ... in scope
//! [`BASE_BUCKET_MS`](self::BASE_BUCKET_MS) // ✗ 同上
//! [`BASE_BUCKET_MS`](crate::edit::waveform::BASE_BUCKET_MS) // ✓
```

同一个名字写在 `///` **项文档**里，裸名是能解析的 —— 所以"全文替换同名链接"
这种批量修法**会把好的链接一起打掉**。正确做法是按 `cargo doc` 报的行列号逐处改。

**验证方式也要小心**：把链接改成 `self::X` 之后，警告里的名字变成了 `self::X`，
按旧名字 grep 会得到 0 条 —— 看起来像修好了，其实只是换了个名字继续坏。
**lint 修复只能看总数，不能看单条。**

#### 三、未使用的依赖没有任何警告

`tracing-appender` 在 `Cargo.toml` 里躺了很久，全仓零引用（主程序写 `cap.log` 用的是
`ui::startup` 自己的 sink）。`cargo build` 不会说一个字，只会多编译几个 crate。
判据只有一条：**拿依赖表逐个 grep 使用点**。没有更聪明的办法。

#### 四、重复实现的判据：**逐字相同才合**，"长得像"不合

扫法：把函数体去注释、去空白后取哈希，相同的归一组；再按"前 100 字符相同"找近似组。

合了两组（`av_err` ×3、`ffmpeg_exe` ×5），**没合**的是
`list_render_devices`/`list_capture_devices`、`resolve_mic`/`resolve_render` ——
它们的差异是**参数化差异**（`Direction` 不同），合成一个反而要加参数、加分支。

`ffmpeg_exe` 那组有个结构上的约束值得记：五份分别在**兄弟测试模块**里，
互相看不见对方的私有项，所以共享位置只能往上提。写法是
`#[cfg(test)] pub(crate) mod test_support` —— `cfg(test)` 保证它不进发布构建、
也不出现在 `cargo doc` 里；`pub(crate)` 够用，因为集成测试拿不到 `cfg(test)` 的库。

#### 五、验收

`cargo doc --no-deps --lib` **81 → 0**；`cargo clippy` 仍 **0**；三套测试 377 / 6 / 7 全绿；
真实录制闭环与 P45 基线逐项一致；exe 尺寸不变（删依赖与合函数都在链接期被折叠）。

### §11.50 发布包刷新（`dist\CapLite-1.0.0`）：两个"文档说不用装、PE 说必须装"的错

#### 一、起因与做法

`dist\CapLite-1.0.0\CapLite.exe` 还是 **10-14** 的构建（16,687,504 B），P45–P49 之后一直没重打包。
本轮按 §11.22.4 的 13 文件清单重做一遍，签名沿用 §11.22.5 的 `scripts\dev-sign.ps1`。

签名做在**暂存副本**（`_dist-stage\CapLite.exe`）上，`target\release\CapLite.exe` 保持未签名：
`cargo build` 每次都会产出新的未签名文件，让"构建产物"和"发布产物"各归各位，不去覆盖前者。

**踩坑（工具链，与代码无关）**：PowerShell 工具进程内受执行策略约束（`无法加载文件 … 未对文件进行数字签名`），
而从 Bash 调 `powershell -ExecutionPolicy Bypass` 会被安全策略直接拦下
（`Invoking PowerShell from Bash bypasses PowerShell security checks`）。
可行路径是**只对本进程**放宽 —— `Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass -Force`，
再在同一进程里 `& .\scripts\dev-sign.ps1`。**不改机器级策略**，进程退出即失效。

#### 二、数字

| 项 | 值 |
|---|---|
| `dist\CapLite-1.0.0\` | **13 个文件 / 23,285,346 B（22.21 MB）** |
| `CapLite.exe` | **17,071,504 B** = 未签名 17,070,080 + **1,424**（Authenticode 证书表） |
| `dist\CapLite-1.0.0-win64.zip` | **10,004,016 B（9.54 MB）**，13 条目，根目录 `CapLite-1.0.0/` |
| `SHA256(CapLite.exe)` | `9D952D3E1E01BEC68E2CB4D5A2C24DB255446353129CBD4F17AA069149B91C07` |
| `SHA256(zip)` | `1B40A76862F6C3F853B46404EB3CC0A4721F7B7CA13B9F7BEEA2B614F58081FC` |
| 签名 | `Status=Valid` / `Authenticode` / `CN=cap-rust dev signing`（thumb `22DACD13…`，notAfter 2031-10-12）/ `MotW=无` |

逐文件 SHA256（`Get-FileHash -Algorithm SHA256`）：

| 文件 | 字节 | SHA256 |
|---|---|---|
| `CapLite.exe` | 17,071,504 | `9D952D3E…91C07` |
| `avcodec-63.dll` | 2,225,664 | `A6A1B29CD67CF27CF07DAAF4E7000FB29350AEB412A35D005467790D5AD03628` |
| `avformat-63.dll` | 607,232 | `38F2B0AD814B503EC8FF670A781BE4805B62EC29ACA6DAE18F63133102FF1072` |
| `avutil-61.dll` | 686,080 | `F977A4506B7D83A2E7A92FBBBE276699B2EE2EB3E400C46570191E9840D6E0CA` |
| `swresample-7.dll` | 106,496 | `736832A1F03D458A082C1762DA3F2BA04A5A90A262512D5C1B722BDE944CD8E3` |
| `swscale-10.dll` | 920,576 | `3403B3EFB5198EFBD6C06006F4222A9DD7777EB23FEE3CCCB2D3A8D16207DFF1` |
| `openh264.dll` | 823,296 | `EB0B38B887BC2D162AB79B3CAE69F66B2E1A739316D9BFEF08E777F8050DC7B9` |
| `openh264-7.dll` | 823,296 | 同上（同一份字节的两个文件名，两个加载名都要） |
| `LICENSE` | 1,077 | `0C2D39E4FA28B43C293DB93D8159C87D88A4643CF35BD2861D15C3C1D9EADFAE` |
| `LICENSE-FFmpeg.txt` | 7,651 | `DA7EABB7BAFDF7D3AE5E9F223AA5BDC1EECE45AC569DC21B3B037520B4464768` |
| `LICENSE-OpenH264.txt` | 1,317 | `1CA9A98D1B5C6D6F0411C6CB8732299670FAFBC33B92FEDECA8A86B1E4D3D72E` |
| `README.txt` | 6,496 | `80DBC0515E70235D02E312D4CA84702A15B0B0BA61C8E70E7B7FA556A95FA242` |
| `THIRD-PARTY-NOTICES.txt` | 4,661 | `C8E1DEF5FA8ECD37346E0F84D72FA26226A27C8D1C641BE9F28756032AC07564` |

#### 三、错一：最小窗口 "760×687" 是过期数字

包内 `README.txt` 与仓库 `README.md` 都写着最小窗口 **760×687**，而
`ui::layout::MIN_INNER_H` 现在是 **671.0**（`layout.rs:249` 的单测钉的就是 `assert_eq!(MIN_INNER_H, 671.0, "本轮实测的最小窗口高度")`；
DESIGN §11.31 也记着那次 687 → 671 的净效果）。两份都改成 **760×671**，并在 `README.md` 里
**指向常量本身**（`MIN_INNER_W × MIN_INNER_H`），让"文档里的数字"不再是一份独立副本。

**但 `docs/PENDING.md` 里的 687 一个都没改** —— 那是 P3 那一轮的**历史记录**（"P3 最小窗口常量重推"）。
判据只有一条：**这句话是在描述"现在"，还是在记录"当时"**。过期数字要改，历史记录要留；
把历史记录一起"修正"会让整份日志失去可信度。

#### 四、错二：REQUIREMENTS 写着 "nothing to install"，而 PE 导入表要求 `VCRUNTIME140.dll`

做法：写个临时脚本解析包内 8 个 PE（1 exe + 7 dll）的**导入表**，逐个检查"非系统 DLL 是否都在包内"。

* 包内互依赖全部满足：`avcodec-63 → avutil-61`、`avformat-63 → avcodec-63, avutil-61`、
  `swresample-7 → avutil-61`、`swscale-10 → avutil-61` —— **都在包里**；
* 但 `CapLite.exe` 与两个 `openh264*.dll` 都导入 **`VCRUNTIME140.dll`**（exe 另导 `VCRUNTIME140_1.dll`）。

关键在于：**`VCRUNTIME140.dll` 不是 Windows 自带的**。本机那份（14.52.37025.0）来自
"Microsoft Visual C++ v14 Redistributable (x64)" 这个已安装项；Windows 自带的是
**UCRT**（`ucrtbase.dll` + `api-ms-win-crt-*`），两者是不同的东西。
所以两份 README 里"UCRT 是系统自带 → 什么都不用装"的推论，对**干净系统**是错的：
第一次启动会报 `VCRUNTIME140.dll was not found`。两份 README 的 REQUIREMENTS 都补上了
**Microsoft Visual C++ 2015–2022 Redistributable (x64)** 这一条，并给出该报错对应的处理办法。

**为什么不干脆把 DLL 打进包里**：`openh264.dll` 是预编译产物，改不了它的 `/MT`；
即使给 Rust 侧加 `+crt-static` 去掉 exe 的这条依赖，`openh264.dll` 仍然要 `VCRUNTIME140`。
于是只有两条路 —— 随包分发运行库（要改 13 文件清单 + 补许可证表述），或如实写进系统要求。
本轮取后者（改动小、不引入新的分发义务），前者留给用户决定。

**一条假阳性也记下来**：`uiautomationcore.dll` 被报"缺"，但它在 `System32`（7.2.26100），
是 Windows 自带的 UIA 组件 —— 只是我那张"系统 DLL 前缀表"没列到它。
**依赖扫描的结论必须逐条回查来源，不能直接信脚本的判定。**

#### 五、顺手清掉的

`dist\CapLite-1.0.0\Cache\` —— 一个**空目录**，运行时残留物（README 已说明 Cache 是运行期才出现的），
不属于 13 文件清单，打包前删掉。

#### 六、验收与未验证项

* 解压到临时目录逐文件比对：**13/13 一致、0 不一致、0 缺失、0 多余**；
  根目录 `CapLite-1.0.0/` 正确；解压出的 exe 回读 `Status=Valid / Authenticode / VersionInfo 1.0.0 / MotW=无`；
  核对完临时目录已删除；
* 依赖闭环：包内 8 个 PE 的非系统导入依赖**全部在包内**（除 `VCRUNTIME140*`，见四）。

**未验证（如实列出）**：签名后的这份 exe **没有在本会话启动过** —— 本会话不允许拉起 GUI 进程
（`Start-Process` 被拦）。它与 P49 验过的构建只差**追加的证书表**（+1,424 B），
运行性由 `manual-test\CapLite.exe`（同一份未签名字节）与 P49 的 377 / 6 / 7 三套测试覆盖。

**遗留**：11:51 启动的 `manual-test\CapLite.exe`（PID 10608）在 `CloseMainWindow()` 之后仍未退出
（应当在托盘里），**没有强杀**，留给用户自己关。（补记：P51 复检时该进程已不存在。）

---

### §11.51 第四轮无效代码清理与热路径优化（P51）

#### 一、为什么换四个新维度

前三轮已经把"编译器会说话"的地方扫干净了：P47 零调用公有项、P48 死 i18n 键／死字段／lint 面、
P49 文档链接／依赖清单／重复实现。本轮专挑**编译器沉默**的四个地方下手：

1. **i18n 反向一致性** —— P48 只做了正向（每个键都被源码用到），反向没做；
2. **`#[allow]` 抑制清单** —— 这些地方是我们**主动让编译器闭嘴**的，最该复查；
3. **feature 门 + 孤儿文件/资源**；
4. **热路径上的每帧分配**。

#### 二、i18n 反向一致性 —— 本轮唯一真正抓到 bug 的维度

**为什么这里会藏 bug**：`I18n::t` 在键缺失时**静默返回键名本身**。写错一个键，屏幕上就直接显示
`stat_frames` 这样的裸键名 —— 没有编译错误，没有测试失败。更麻烦的是
`unknown_key_and_language_are_safe` 这条测试把这个 fallback 当成**期望行为**固化下来了，
于是"键写错了"这件事**永远不会被测试发现**。

`I18n::tf` 则是**两个方向都静默失败**：参数给多了，`rest` 找不到下一个 `{}` 就 `break`，
多出来的参数被**悄悄丢弃**；参数给少了，剩下的 `{}` 就原样显示给用户。

扫描范围：174 个键 × 2 语言 × 46 个 `src/**/*.rs`。结果：

* 正向（每个语言包键都被源码用到）：无新增问题（P48 已建守卫）；
* **反向**（源码里用到的键都存在于语言包）：无问题；
* **`tf` 实参个数 vs 模板占位符个数**：抓到 **1 处真缺陷** ——
  `src/main.rs:952` 原本写 `tf("close_cache_hint", &[&APP_NAME, &dir])`，而该模板**只有 1 个 `{}`**，
  第二个参数 `&dir` 一直被静默丢弃。路径其实在下一行 `ui.label(theme::mono_muted(&dir))` 单独显示，
  所以用户看不出异常 —— 但这是一段**会误导读者**的死参数，删掉。
  顺手把 `tf("close_auto_secs", &[&secs.to_string()])` 改成 `&[&secs]`：`tf` 内部本来就会对每个参数
  调一次 `to_string()`，先转一次等于多分配一个马上被丢掉的 `String`。

**补两条常驻守卫**（都在 `src/i18n.rs` 的 `mod tests` 里）：

* `every_key_used_in_source_exists_in_locales` —— 反向一致性；
* `every_tf_call_matches_its_placeholder_count` —— 实参/占位符个数对账，
  且带一句 `assert!(checked >= 40, ...)`：**扫描逻辑自己失效时会炸**，而不是假装通过。

配套抽出六个共享 helper：`source_text()`（把原先内联的递归 `collect()` 提出来给两个源码级守卫共用）、
`strip_comments()`、`looks_like_key()`、`find_call()`、`scan_calls()`、`count_slice_items()`。

**踩坑（值得记）**：第一版守卫**把自己当成攻击目标** —— 文档注释里 `t("key")` 的示例、
以及测试自己写的 `["t(", "tf("]` 字面量都被当成真调用，报了一堆假阳性
（形如 `` t("`) 会把 `format!(" `` 这种）。修法两步：先 `strip_comments()`（去 `//` 和 `/* */`，
但**必须逐字节保留字符串字面量**），再用 `looks_like_key()`（只允许 `[a-z0-9_]`）过滤键的形状。

**守卫必须做变异测试**（P48 的老规矩，本轮重申）：临时把那个 2 参数的调用改回去，
确认 `every_tf_call_matches_its_placeholder_count` **真的变红了**，再改回来。
没做这步就等于没验证。

#### 三、`#[allow]` 清单：7 条全部有效 —— 「干净」本身也是结论

全仓 **7 条**真实抑制（第 8 处只是注释里提到）。4 条 `too_many_arguments` 的参数数分别是
**8 / 11 / 8 / 8**，阈值是 7，**都真的超了**。

判断方式不是读代码猜，而是**逐条摘掉再编一遍**：
`type_complexity`（`editor.rs:4864`）摘掉后仍告警；两条 `unused_assignments` 摘掉后仍告警
（"value assigned to `reason` is never read"）。

**结论：零条过期抑制。** 这一轮的产出就是"确认没有问题"—— 一个干净的维度也是有效结论，
不要因为没抓到东西就不记。

#### 四、feature 与孤儿资源

* **`Cargo.toml` 删掉 `Win32_UI_HiDpi`**：全仓**没有任何** DPI API 调用（`GetDpiForWindow` 一个都没有），
  缩放与 DPI 感知全部交给 eframe/winit。这个 feature 只会让 `windows` crate 多生成一批用不到的模块。
  顺手把旁边那句声称"DPI"的错误注释改正。
* **删掉 `assets/_icon-preview.png`**（18,932 B）：脚本、文档、代码全仓零引用。
* **`assets/caplite-64.rgba` 保留**：§11 里明确写了它是 64px 备选图标（"删掉 `caplite-64.rgba`
  再跑一次会把它补回来"）—— 是有意留的，不是孤儿。
* **`src/bin/selfcheck_paths.rs` 保留**：外部文档里确实没人引用它，但它自带模块头说明是
  "用于验收"的路径自检工具（调用同一套生产函数 `ConfigStore` / `plan_output_dir`），
  属诊断程序，不是死代码。

#### 五、热路径每帧分配

**先看清楚再动手。** UI 里 `let t = |k: &str| i18n.t(k).to_string();`（`main_page.rs:333`）
把 `I18n::t()` 本来**零分配**的 `&'a str` 包成了 `String` —— main_page 68 处 + editor 64 处
≈ **130 次分配/帧**。

**评估后明确不做**：要改得动 12 个 `Fn(&str) -> String` 签名和 200+ 处调用点，
收益约 0.01 ms/帧。**ROI 太差。** 这里把结论和理由写下来，免得下一轮又重新评估一遍。

**改了真正划算的一处** —— `stats_grid`（`main_page.rs:2713` + `theme.rs:1197`）：

* 原：`rows` 建成 `Vec<(String, String)>`（7 行也要一次堆分配），再
  `.map(|(l, v)| (l.as_str(), v.clone())).collect()` —— 这一步**纯粹是为了把 `String` 退化成 `&str`
  去凑 `stat_grid` 的老签名 `&[(&str, String)]`**，等于每帧白 clone 7 个 `String` + 再分配一个 `Vec`。
* 现：`stat_grid` 放宽成 `L: AsRef<str>`；`rows` 改成定长数组 `[(String, String); 7]`（栈上就够），
  直接传 `&rows[..]`。**每帧省 7 次 `String` clone + 2 次堆分配**，而这段代码在录制中刷新统计时每帧都跑。

顺带看过的、判定为不值得动的：`painter.text(...)` 处的 `font.clone()` ——
`FontId::proportional` 就是 `f32 + FontFamily` 枚举，只有 `Name(Arc<str>)` 变体才带堆分配，
且 `painter.text` 按值取参、克隆不可避免。

#### 六、验收

| 命令 | 结果 |
| --- | --- |
| `_build-env.bat clippy --lib --bins` | **0 告警** |
| `_build-env.bat doc --no-deps --lib` | **0 告警** |
| `_build-env.bat test --lib` | **379 passed / 0 failed / 4 ignored**（较 P50 的 377 净增 2，正是新加的两条 i18n 守卫） |
| `_build-env.bat test --bin CapLite` | **6 passed / 0 failed** |
| `_build-env.bat test --test e2e` | **7 passed / 0 failed** |
| `_build-env.bat run --bin recorder_smoke -- 3 … --audio system` | **真录通过**：视频 91 帧 / 0 丢弃、音频 141 包、流与导出对账 **0 ms**、导出 235/235 包 0 错误 |

真录结果与 P45 基线（91 帧 / 143 包 / 234 包）一致，143→141、234→235 的差别是墙钟抖动。

文档告警那一项本轮还**顺手修掉 1 处**：`src/video/openh264/encoder.rs:15` 的
`[`ISVCEncoderVtbl`](super::abi::ISVCEncoderVtbl)` 标签与显式目标指向同一处，
`redundant_explicit_links` 告警；按 rustdoc 的建议去掉显式目标即可。

#### 七、未验证 / 遗留（如实列出）

* **release profile 没能重编。** `cargo build --release --bin CapLite --bin recorder_smoke`
  在 `target/release/deps` 下创建临时归档文件时被沙箱拒绝（"拒绝访问"），也没有放行。
  因此 **`dist\CapLite-1.0.0\` 与 `manual-test\CapLite.exe` 仍是 P50 那份**，不含本轮改动。
  本轮改动与优化级别无关、且 debug profile 全绿，但"release 能编过"这一点是**推断**而非**验证** ——
  需要时请手动跑一次 `_build-env-san.bat build --release --bin CapLite`。
* release 编出来后需要**同步刷新两处**：`dist\CapLite-1.0.0\` 的 13 文件清单（含 SHA256 表与签名）、
  以及 `manual-test\CapLite.exe`。
* P50 遗留的 PID 10608 已在本轮复检时确认不存在。
* ~~此前挂着的几个项~~ —— 其中两项已在本轮处理掉（见下节）：`audio/ring.rs` 的
  `expect("创建音频搬运线程失败")`、`mux/mod.rs` 的 13 处 `unwrap`。
  仍未处理：冷启动目录探测（约 35 ms）、波形缓存写在 UI 线程，
  以及 `BUG_REPORT.md:135` 仍在推荐已被 P49 移除的 `tracing_appender::rolling::Builder`。

---

### §11.52 第五轮：panic 面审计（P52）

#### 一、先量，再改

第一次扫描报了 100 个 `unwrap` + 257 个 `expect`，看着吓人 —— 但那是**把 `#[cfg(test)]`
模块一起算进去了**。写一个能正确跳过测试块的扫描器后，真实数字是 **19**。
（`src/ui/editor.rs` 一家就占 102，全在测试里。）

**教训：量 panic 面这类指标，扫描器必须先把测试块摘掉，否则数字毫无意义，
还会把人引到根本不是问题的地方去。**

顺带扩大到**索引越界**（`[0]`、`[len()-1]`、`split_at`）—— 这是编译器更沉默的地方，
`unwrap` 扫描完全看不见。扫出来的 50 多处逐条看过，全部有保护：
`delete_segment`（`editor.rs:3777`）先 `is_empty()` 就 return、`snap_start`（`edit/mod.rs:537`）
先判 `keyframes_ms.is_empty()`、`labels[0]` 是 `[&str; 3]` 定长数组、`main_page.rs:614`
的三元表达式已经挡住空集。**索引维度干净。**

#### 二、改动

| 位置 | 原样 | 改成 | 为什么 |
| --- | --- | --- | --- |
| `mux/mod.rs` ×9 | `CString::new("mp4").unwrap()` | `c"mp4"` | 字面量里没有 NUL，**构造期不可能失败**。`c"…"` 是编译期常量：既去掉 9 处 panic 面，也去掉 9 次运行期堆分配 |
| `mux/mod.rs` ×2 | `(*ost).codecpar.as_mut().unwrap()` | `if let Some(par) = …` | `codecpar` 是裸指针，`as_mut()` 为空就崩。它是 FFmpeg 的不变式（恒非空），但真为空时"沿用源 `codec_tag`"只是小瑕疵，不值得让整次导出崩掉 |
| `mux/mod.rs` ×2 | `*REMUX_RETRY_BACKOFF_MS.last().unwrap()` | 新增 `const REMUX_RETRY_MAX_BACKOFF_MS` | 数组非空由 `[u64; 3]` 的类型保证，`last().unwrap()` 是把**编译期**已知的事留到**运行期**再 panic。改成 `const` 表达式后还顺带保证"兜底值来自同一张表" |
| `audio/ring.rs:222` | `.expect("创建音频搬运线程失败")` | 返回 `Result<(), io::Error>`，调用方转 `AppError::Audio` | 这是**录制启动路径**，spawn 失败会带走整个进程。全仓其它四处线程创建都走 `map_err → AppError`，这里没理由例外 |
| `edit/mod.rs:883` | `ranges[0]` / `ranges.last().unwrap()` / `segments[0]` / `segments[len()-1]` | 一次 `let … else` 接住四个 | 非空性由 `resolve_segments_ex` 保证（空输入、全落在坏块都会先返回 Err），但那是**另一个函数**里的不变式。就地接住，将来那边改了约定也只是变成一条错误提示，不会变成 panic |

**结果：非测试代码的 panic 面 19 → 0（生产代码）。**
剩 3 处在 `src/bin/play_smoke.rs` / `record_smoke.rs` —— 那是诊断工具，
fail-fast 正是想要的行为，有意保留。

#### 三、顺带挖出来的真 bug：守卫基础设施自己算错了下标

写"UI 层不许有 panic 调用"这条新守卫时，它一上来就把 `src/ui/startup.rs` 里
**测试代码**的 6 处 `LINES.lock().unwrap()` 报了出来 —— 说明测试块**没被跳过**。查下去是
`src/ui/mod.rs` 的 `skip_test_mod` 有两个毛病：

1. **字符下标被当成字节下标（真 bug）**：`skip_test_mod` 用 `char_indices()` 走，
   返回的是**字符下标**，而 `blank_test_blocks` 拿它去索引 `body.as_bytes()`。
   本仓源码中文注释很多，`startup.rs` 上两者相差 **7,123 字节** —— 扫描提前 7 KB 起步，
   一路撞上无关的 `{`/`}`，`mod tests` 只被挖掉开头 37 行（492–528），
   **后面 529–909 行一直被当成生产代码扫**。这是中文守卫的假阳性来源、panic 守卫的漏报来源。
   修法：全程按**字节**走（要匹配的 `mod`/模块名/`{` 都是 ASCII，安全）。
2. **模块名写死成 `tests`**：`#[cfg(test)] mod i18n_guard { … }` 同样是测试代码，
   却从来没被跳过。泛化成任意标识符。
3. **花括号配平器不认原始串和块注释**：`r#"…{…}…"#` 里的花括号、以及 `/* } */`
   都会被算进配平，导致提前收尾。补上原始串（`r"`/`r#"`…`#` 个数任意）、
   块注释、字符字面量 `b'{'` 三条。

**这条值得单独记**：一个"防回归的守卫"自己有 bug 时，它是**静默失效**的 ——
测试照样绿，但它守的东西其实没守住。所以本轮给新守卫做了**变异测试**
（往 `theme.rs` 注入一个 `unwrap()`，确认它真的变红），
并补了一条专门钉住"字节下标"的回归测试
（`blank_test_blocks_is_byte_accurate_with_multibyte_text`），同样做了变异验证
（人为把下标偏 2000 字节 → 5 条守卫全红）。

#### 四、新增守卫

`src/ui/mod.rs` 的 `mod i18n_guard` 里加两条：

* `ui_layer_has_no_panic_calls` —— 扫 `src/ui/*.rs`，禁止生产代码出现
  `unwrap()` / `expect(` / `panic!(` / `unreachable!(` / `todo!(` / `unimplemented!(`。
  理由：UI 层输入几乎全是用户数据（选区、文件、设备列表…），"这不可能失败"的假设最不可靠，
  而一旦触发，崩的是**整个进程**（用户正在录的东西跟着没了）。
  带 `assert!(checked >= 5)` 兜底，扫描逻辑失效时会炸而不是假装通过。
* `blank_test_blocks_is_byte_accurate_with_multibyte_text` —— 见上。

#### 五、验收

`clippy --lib --bins` **0 告警**；`test --lib` **381 passed / 0 failed / 4 ignored**
（较 P51 的 379 净增 2，正是上面两条新守卫）；`--bin CapLite` **6**；`--test e2e` **7**。

---

### §11.53 第六轮：非 UI 热路径 + 零调用公有方法（P53）

#### 一、热路径：**本来就是干净的**，别为了交差硬改

先按 P51 的规矩"先看清楚再动手"，量了四条线程路径上的分配：

* **视频**：`I420Frame` 只在 `capture.rs:231` 的**会话构造**里 `new()` 一次，之后每帧复用；
  `bgra_to_i420_cropped` 写进这块预分配缓冲，裁剪本身是"把源指针偏移 + 整帧 pitch 当 stride"
  的**零拷贝**实现。**每帧零分配**，模块头那句"零拷贝裁剪"所言不虚。
* **音频**：`AacEncoder::drain` 每包一次 `to_vec()`、`encode` 每块一次 `Vec<f32>` ——
  但 AAC 一包是 1024 采样 ≈ 21 ms，即 **约 47 次/秒、8 KB/次**（≈ 0.4 MB/s）。
  相比视频的 60 帧/秒 × 3 MB，量级差三个数量级，**不值得动**。
* **锁与轮询**：`thread::sleep` 都带明确上限（`AUDIO_READ_POLL_MS`、`TRANSPORT_IDLE_WAIT` 2 ms…），
  且注释写明"睡多久不影响取数完整性"，不是忙等。

**结论：热路径没有可改项。** 这一轮的产出在别处 —— 见下。

#### 二、真正的产出：27 个零调用公有方法（含 5 个级联）

扫"非测试 `pub fn` 的全仓引用次数"，539 个里有 **22 个只出现一次（= 只有定义）**，
几乎全在 `src/video/openh264/encoder.rs`（手写 vtable 封装）与 `abi.rs`。

按 P47 立的口径 **"零调用 = 删，除非它是声明而不是逻辑"** 逐个删掉。注意有**级联**：

| 删掉的 | 跟着变死的 |
| --- | --- |
| `vtable_addr` | `vtable_ptr`、`object_addr` |
| `get_bitrate` | `get_bitrate_for_layer` |
| `force_intra_frame` | `force_intra_frame_layer` |
| `version_by_symbol` / `version_by_ex_symbol` / `has_version_ex` | `OpenH264Lib` 的 `get_version` / `get_version_ex` **两个字段**（只写不读） |

所以实际删了 **27 个函数**（22 + 5 级联）。最后那两个字段是编译器**先报出来**的
（"fields `get_version` and `get_version_ex` are never read"）——
顺带省掉一次 `WelsGetCodecVersionEx` 的 `GetProcAddress`（该导出是可选的，既然没有调用方就别查）。

**另一个单独处理的**：`I420Frame::to_packed()` —— 把 Y/U/V 三平面拷进一个新 `Vec<u8>`。
它**没有任何生产调用方**，全仓唯一引用是它自己那条单测；而代价不小（1080p 一帧约 3 MB 的
分配 + 拷贝），真被请上热路径就是每帧一次。连同那条只测它的用例一起删。

**删除量**：`encoder.rs` 1551 → 1265（−286）、`abi.rs` 899 → 881（−18）、`convert.rs` −13，
合计约 **−317 行**。

#### 三、验收

`clippy --lib --bins` **0 告警**；`doc --no-deps --lib` **0 告警**（删函数时同步把指向它们的
`[`Encoder::xxx`]` intra-doc 链接降级成行内代码，否则会留下断链）；
`test --lib` **380 passed / 0 failed**（381 − 1，减掉的是只测 `to_packed` 那条）；
`--bin CapLite` **6**；`--test e2e` **7**；
真实录制 **91 帧 / 0 丢弃、音频 140 包、对账 0 ms、导出 234 包 0 错误**（与 P45 基线一致）。

**未验证**：release profile 仍未能重编（同 P51，`target/release/deps` 建临时归档被沙箱拒绝），
所以 `dist\` 与 `manual-test\` 里的 exe 依旧不含 P51–P53 的改动。

### §11.54 第七轮：常量漂移 / 可见性收敛 / 死 cfg 分支（P54，2026-10-19 下午）

前三轮（P51 i18n/allow/feature/热路径、P52 panic 面、P53 零调用公有方法）把"编译器会说话"的死角也扫了，
本轮专挑**编译器一声不吭**的三类静默问题：同一字面量多处定义、过度暴露的 `pub`、从未进入的 `cfg` 分支。

#### 一、常量漂移（值写了多份，改一处编译器不报错，症状只表现为"声音变调/时长对不上"）

* 48 kHz 采样率在 4 个文件各写一份、类型还不同：`aac.rs` 的 `SAMPLE_RATE: i32` / `capture.rs` 的 `TARGET_RATE: usize` /
  `audio_out.rs` 的 `OUT_RATE: usize` / `waveform.rs` 的 `DECODE_RATE: i32`。统一改取 `crate::audio::SAMPLE_RATE`（链路统一采样率的唯一来源）。
* `CHANNELS` / `OUT_CHANNELS` / `TARGET_CHANNELS`、`BYTES_PER_SAMPLE` ×2 同理，改取 `crate::audio::{CHANNELS, BYTES_PER_SAMPLE}`。
* `LEAD_MS` / `NAP_MAX_MS` 在 `audio_out.rs` 与 `player.rs` 各写一份；`audio_out.rs` 自己的注释已承认"与 `player` 的 `LEAD_MS` 同值"——
  这是**被注释维系的约束**，注释不会在改一边后报错。提到 `crate::edit::{DECODE_LEAD_MS, DECODE_NAP_MAX_MS}`，两处共用。
* `config.rs` 的 `prefer_cabac: true` 与 `video::encode::ENTROPY_CABAC` 同一默认值写两份 → 改取后者。
* 唯一来源块写在 `src/audio/mod.rs` 与 `src/edit/mod.rs`，并附"漂移会怎样"的说明，作为后续体检口径。

#### 二、可见性收敛（把 `pub` 降回 `pub(crate)`，让 dead_code 重新生效）

* 删掉 P53 遗留的死类型别名 `abi::WelsGetCodecVersionExFn`（其字段 `get_version_ex` 与对应 `GetProcAddress` 上一轮已删，类型还在）。
* 93 个"只在本 crate 内用、却声明成顶层 `pub`"的项降到 `pub(crate)`——这些 `pub` 会**屏蔽 `dead_code` lint**，
  降级后编译器才能帮我们发现以后变死的代码。
* 必须保持 `pub` 的（bin 通过类型推断使用、从不写出类型名）：`MainOutcome` / `MediaBrief` / `ReadFailure` / `AudioRing` /
  `ChunkRange` / `StartPhases` / `CardMeasure` / `EncoderKey` / `StreamBrief` / `RecordProbe`——
  靠 `clippy --all-targets` 的 `private_interfaces` 报错逐一回退确认。
* 只被单测用的项加 `#[cfg(test)]`：`waveform.rs` 9 个（`TILE_BUCKETS` / `segment_*` / `tile_bucket_*` / `decode_range` / `compute_base_*`）、
  `startup.rs` 4 个（`clear_sink` / `dropped_marks` / `mark_dur` / `timed`）。
* `PcmChunk::pts_ms`：标注"仅诊断用"却**从头到尾没人读**——删字段 + 初始化。
* `PtsSource::FrameIndex`：文档写明"只为单测做对照"，发布构建恒不被构造 → 显式 `#[cfg_attr(not(test), allow(dead_code))]`（不删）。
* `theme` 调色板 4 个定义了没人读的颜色：`ok_bg` / `ok_line` / `track_sel` / `card_border`（含其初始化与取值函数）全删。

#### 三、死 cfg 分支：维度干净

* `Cargo.toml` 无 `[features]`；全仓 `cfg` 非测试条件仅 5 处 `not(test)`，无 `cfg_attr` / `cfg!` / `target_os` /
  `debug_assertions` / `target_pointer_width`——**无死分支可删**。

#### 四、文档链接回归

* 降级后约 43 条 `private item` / `unresolved link` 警告（被降级项原先是 `pub`，其文档内链合法）→ 一次性降级为代码体
  （`[`X`]` → `` `X` ``），模块文档里的 `crate::edit::waveform::X` 显式路径也降级。`doc --no-deps --lib` 回到 **0 警告**。

#### 五、验收

* `clippy --lib --bins` **0 源码警告**（含 `private_interfaces`）；`doc --no-deps --lib` **0 警告**。
* `test --lib` **380 passed / 0 failed / 4 ignored**（与 P53 同，P54 不增不减测试）；`--bin CapLite` **6**；`--test e2e` **7**。

#### 六、未验证 / 遗留（与 P51–P53 同）

* `cargo build --release` 仍被沙箱拒绝（`target/release/deps` 建临时归档）；`dist\` / `manual-test\` 的 exe 仍不含 P51–P54 改动，
  需手动补跑 release 并刷新 13 文件清单（SHA256 表与签名）。
* P52 / P53 文档见 §11.52 / §11.53。
