//! 屏幕采集：DXGI Desktop Duplication（`windows-capture` 封装）。
//!
//! 为什么不用 WGC：本机实测 `GraphicsCaptureItem::TryCreateFromDisplayId` 不可用
//! （windows-capture 报 `ItemConvertFailed`），而 DXGI 路径实测稳定 61.6 fps、
//! 输出 `Bgra8`、`row_pitch == width*4`（无 padding）。
//!
//! 本模块负责：
//! * 显示器枚举与选择（0 = 主显示器）
//! * **虚拟桌面坐标 → 显示器相对裁剪矩形** 的换算（旧代码正是死在 `x as usize` 负坐标上）
//! * 固定帧率输出：`Timeout`（画面无变化）时由调用方复用上一帧；
//!   **抽帧语义是"取当前最新一帧，丢掉中间被超越的帧"**（见 [`CaptureStats::superseded_frames`]）
//! * `AccessLost`（切分辨率/独占全屏/UAC 桌面切换）自动 `recreate()` 恢复
//! * 可选鼠标指针合成（DXGI 帧本身不含指针）

use crate::config::{CaptureMode, Region};
use crate::error::AppError;
use crate::video::convert::{self, CropRect, I420Frame};
use crate::video::cursor;

use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, Error as DupError,
};
use windows_capture::monitor::Monitor;

/// 显示器信息（给 UI 用）
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    /// 0 表示主显示器；其余为 windows-capture 的 1-based 序号
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl MonitorEntry {
    /// 供 UI 显示的标签，例如 `主显示器 · SE2717H/HX 1920x1080`
    pub fn label(&self) -> String {
        format!(
            "{}{} · {}x{}",
            if self.primary { "主显示器 " } else { "" },
            self.name,
            self.width,
            self.height
        )
    }

    /// **短标签**（只有设备名，不带分辨率）：录制页采集行的下拉**收起态**用。
    ///
    /// 为什么分开：P38 把分辨率信息挪到了状态卡片「开始录制」左侧，下拉里再带一份
    /// 既重复又占宽度（"主显示器 · SE2717H/HX 1920x1080" 约 220px，会把
    /// 「录制范围」那一组控件挤出 760 最小宽度）。下拉**展开的列表项**仍用全标签
    /// （弹层宽度自适应，不缺信息）。
    pub fn label_short(&self) -> String {
        format!(
            "{}{}",
            if self.primary { "主显示器 · " } else { "" },
            self.name
        )
    }
}

/// 列出所有显示器
pub fn list_monitors() -> Result<Vec<MonitorEntry>, AppError> {
    let monitors =
        Monitor::enumerate().map_err(|e| AppError::Capture(format!("枚举显示器失败: {e}")))?;
    let primary = Monitor::primary().ok();
    let mut out = Vec::with_capacity(monitors.len());
    for m in monitors {
        let index = m.index().unwrap_or(0);
        out.push(MonitorEntry {
            index: if index == 1 { 0 } else { index }, // 主显示器用 0 表示
            name: m.name().unwrap_or_else(|_| format!("显示器 {index}")),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            primary: primary == Some(m),
        });
    }
    Ok(out)
}

fn resolve_monitor(index: usize) -> Result<Monitor, AppError> {
    if index == 0 {
        Monitor::primary().map_err(|e| AppError::Capture(format!("取主显示器失败: {e}")))
    } else {
        Monitor::from_index(index)
            .map_err(|e| AppError::Capture(format!("取显示器 {index} 失败: {e}")))
    }
}

/// 显示器在虚拟桌面中的矩形（物理像素）：(left, top, right, bottom)
pub fn monitor_rect(monitor: &Monitor) -> Result<(i32, i32, i32, i32), AppError> {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let ok = GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &mut mi);
        if !ok.as_bool() {
            return Err(AppError::Capture("GetMonitorInfoW 失败".into()));
        }
        let r = mi.rcMonitor;
        Ok((r.left, r.top, r.right, r.bottom))
    }
}

/// 把配置里的选区（虚拟桌面坐标）换算成**显示器相对**的裁剪矩形。
///
/// 返回 `(裁剪矩形, 帧左上角在虚拟桌面中的物理坐标)`。
/// 纯函数，便于对"副屏在主屏左侧（负坐标）"这类情况做回归测试。
pub fn region_to_crop(
    mode: CaptureMode,
    region: Region,
    monitor: (i32, i32, i32, i32),
) -> Result<(CropRect, (i32, i32)), String> {
    let (left, top, right, bottom) = monitor;
    let mw = (right - left).max(0) as usize;
    let mh = (bottom - top).max(0) as usize;
    if mw == 0 || mh == 0 {
        return Err("显示器尺寸为 0".into());
    }

    let crop = match mode {
        CaptureMode::FullScreen => CropRect::new(0, 0, mw, mh),
        CaptureMode::Region => {
            // i64 运算避免负坐标/溢出（旧代码 `x as usize` 在这里会变成天文数字）
            let x = region.x as i64 - left as i64;
            let y = region.y as i64 - top as i64;
            if x < 0 || y < 0 {
                return Err(format!(
                    "选区起点 ({}, {}) 在显示器 ({}..{}, {}..{}) 之外",
                    region.x, region.y, left, right, top, bottom
                ));
            }
            let rect = CropRect::new(
                x as usize,
                y as usize,
                region.width as usize,
                region.height as usize,
            )
            .even();
            if rect.width == 0 || rect.height == 0 {
                return Err("选区宽高取偶后为 0".into());
            }
            rect
        }
    };

    crop.validate(mw, mh)?;
    let origin = (left + crop.x as i32, top + crop.y as i32);
    Ok((crop, origin))
}

/// 一次抓帧的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabOutcome {
    /// 拿到新帧（`frame()` 已更新）
    New,
    /// 画面无变化（`Timeout`）：调用方应复用上一帧以维持固定帧率
    Reused,
    /// 设备丢失后已重建会话（本帧未更新，调用方复用上一帧）
    Recovered,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStats {
    pub frames: u64,
    pub no_change: u64,
    pub access_lost: u64,
    pub other_errors: u64,
    /// **被舍弃/超越的帧数**：DXGI 自己报告的"自上次取帧以来被合并掉的帧数"之和。
    ///
    /// 这是"高刷新率屏幕先舍弃多余帧"的可观测证据：144 Hz 的屏幕每秒呈现 144 帧，
    /// 而我们按目标帧率（≤60）取帧，中间那些帧由系统合并掉，这个计数就会增长。
    pub superseded_frames: u64,
}

/// 采集参数
#[derive(Debug, Clone, Copy)]
pub struct CaptureSpec {
    pub monitor_index: usize,
    pub mode: CaptureMode,
    pub region: Region,
    pub draw_cursor: bool,
}

pub struct Capture {
    /// `Option` 是为了在 `AccessLost` 时能 `take()` 出来交给 `recreate()`（它消耗 self）
    dup: Option<DxgiDuplicationApi>,
    crop: CropRect,
    frame_origin: (i32, i32),
    draw_cursor: bool,
    /// 合成指针时用的 BGRA 暂存（仅裁剪区域大小）
    scratch: Option<Vec<u8>>,
    /// 光标位图缓存（每帧只读位置，位图按句柄/100ms 刷新）。
    cursor: cursor::CursorCache,
    frame: I420Frame,
    stats: CaptureStats,
}

impl Capture {
    pub fn open(spec: CaptureSpec) -> Result<Self, AppError> {
        let monitor = resolve_monitor(spec.monitor_index)?;
        let rect = monitor_rect(&monitor)?;
        let (crop, frame_origin) = region_to_crop(spec.mode, spec.region, rect)
            .map_err(|e| AppError::Capture(format!("选区换算失败: {e}")))?;

        // 明确要求 Bgra8：libyuv 的 ARGBToI420 正好吃这个字节序
        let dup = DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8])
            .map_err(|e| AppError::Capture(format!("创建 DXGI 复制会话失败: {e}")))?;

        let scratch = if spec.draw_cursor {
            Some(vec![0u8; crop.width * crop.height * 4])
        } else {
            None
        };

        tracing::info!(
            monitor = spec.monitor_index,
            crop = ?crop,
            origin = ?frame_origin,
            draw_cursor = spec.draw_cursor,
            "采集会话已建立"
        );

        Ok(Self {
            dup: Some(dup),
            crop,
            frame_origin,
            draw_cursor: spec.draw_cursor,
            scratch,
            cursor: cursor::CursorCache::new(),
            frame: I420Frame::new(crop.width, crop.height),
            stats: CaptureStats::default(),
        })
    }

    pub fn frame(&self) -> &I420Frame {
        &self.frame
    }
    pub fn stats(&self) -> CaptureStats {
        self.stats
    }
    pub fn crop(&self) -> CropRect {
        self.crop
    }
    /// 帧左上角在虚拟桌面中的物理坐标（指针合成要用）
    pub fn frame_origin(&self) -> (i32, i32) {
        self.frame_origin
    }

    /// 抓一帧并把结果转成 I420 写进内部缓冲。阻塞最多 `wait_ms`。
    pub fn grab(&mut self, wait_ms: u32) -> Result<GrabOutcome, AppError> {
        // 只借用 self.dup 字段，随后对其他字段的访问与之不冲突
        let dup = self
            .dup
            .as_mut()
            .ok_or_else(|| AppError::Capture("采集会话已丢失且未重建".into()))?;

        let mut frame = match dup.acquire_next_frame(wait_ms) {
            Ok(f) => f,
            Err(DupError::Timeout) => {
                self.stats.no_change += 1;
                return Ok(GrabOutcome::Reused);
            }
            Err(DupError::AccessLost) => {
                self.stats.access_lost += 1;
                tracing::warn!("DXGI 会话丢失，重建中（常见于切分辨率/独占全屏/UAC 桌面切换）");
                let old = self
                    .dup
                    .take()
                    .ok_or_else(|| AppError::Capture("内部错误：会话缺失".into()))?;
                self.dup = Some(
                    old.recreate()
                        .map_err(|e| AppError::Capture(format!("重建 DXGI 会话失败: {e}")))?,
                );
                return Ok(GrabOutcome::Recovered);
            }
            Err(e) => {
                self.stats.other_errors += 1;
                return Err(AppError::Capture(format!("取帧失败: {e}")));
            }
        };

        // ---- "先舍弃多余帧"的依据与可观测证据 ----
        //
        // `windows-capture` 把这层封装成 `acquire_next_frame()`，它内部就是
        // `IDXGIOutputDuplication::AcquireNextFrame`，并把 `DXGI_OUTDUPL_FRAME_INFO`
        // 原样透出（`DxgiDuplicationFrame::frame_info()`）。其中：
        // * `AccumulatedFrames` —— 操作系统**自上次取帧以来累计并合并**的帧数（含本次返回的
        //   这一帧）。也就是说 DXGI 只把**当前最新的一幅桌面图像**交给我们，中间那些帧
        //   不是"排队等我们取"，而是被系统合并掉了：不存在"按刷新率取到历史帧"的可能。
        // * `LastPresentTime` —— 最近一次呈现的 QPC 时刻；我们不需要它（采集时刻自己用
        //   单调时钟记），但它同样说明"这里是呈现事件的时间，不是刷新率节拍"。
        //
        // 于是"多出来的帧" = `AccumulatedFrames - 1`，把它计入统计，作为 144 Hz 屏幕上
        // 按 30/60 fps 抽帧时"多余帧被主动舍弃"的证据。
        self.stats.superseded_frames += u64::from(frame.frame_info().AccumulatedFrames.saturating_sub(1));

        let w = self.crop.width;
        let h = self.crop.height;

        {
            let mut buf = frame
                .buffer_crop(
                    self.crop.x as u32,
                    self.crop.y as u32,
                    (self.crop.x + w) as u32,
                    (self.crop.y + h) as u32,
                )
                .map_err(|e| AppError::Capture(format!("裁剪帧失败: {e}")))?;

            let pitch = buf.row_pitch() as usize;
            let raw = buf.as_raw_buffer();
            let tight = CropRect::new(0, 0, w, h);

            if self.draw_cursor {
                // 防御异常驱动/RDP：pitch 小于 w*4 或缓冲不够高时，逐行切片会 panic 在
                // 视频线程（= 录制静默中断）。先校验，宁可本帧返回明确错误。
                let needed = pitch.saturating_mul(h);
                if pitch < w * 4 || raw.len() < needed {
                    return Err(AppError::Capture(format!(
                        "采集缓冲尺寸异常：pitch={pitch}（需要 ≥ {}），len={}（需要 ≥ {needed}）",
                        w * 4,
                        raw.len()
                    )));
                }
                // 先读光标：光标隐藏/在选区外时，直接对 raw 做 BGRA→I420，
                // 省掉一次整帧拷贝（4K 下约 33 MB/帧）。
                let cursor = &mut self.cursor;
                let scratch = &mut self.scratch;
                let mut composed = false;
                match cursor.read() {
                    Ok(Some(cur)) => {
                        let (cx, cy) = cur.frame_pos(self.frame_origin);
                        let cw = cur.width as i32;
                        let ch = cur.height as i32;
                        let intersects = cx < w as i32
                            && cy < h as i32
                            && cx.saturating_add(cw) > 0
                            && cy.saturating_add(ch) > 0;
                        if intersects {
                            if let Some(buf) = scratch.as_mut() {
                                // 整理成紧凑 stride（映射缓冲的 pitch 可能大于 w*4）
                                for y in 0..h {
                                    let src = &raw[y * pitch..y * pitch + w * 4];
                                    buf[y * w * 4..(y + 1) * w * 4].copy_from_slice(src);
                                }
                                let _ = cursor::composite(buf, w * 4, w, h, (cx, cy), cur);
                                composed = true;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // 指针合成失败不该中断录制；日志限流由 tracing 的调用点负责，
                        // 这里只在 debug 级别记（发布版通常没有订阅者）。
                        tracing::debug!(error = %e, "读取鼠标指针失败，本帧跳过指针");
                    }
                }
                if composed {
                    let buf = scratch
                        .as_deref()
                        .ok_or_else(|| AppError::Capture("内部错误：缺少指针合成暂存".into()))?;
                    convert::bgra_to_i420_cropped(buf, w * 4, h, tight, &mut self.frame)
                        .map_err(|e| AppError::Capture(format!("BGRA→I420 失败: {e}")))?;
                } else {
                    convert::bgra_to_i420_cropped(raw, pitch, h, tight, &mut self.frame)
                        .map_err(|e| AppError::Capture(format!("BGRA→I420 失败: {e}")))?;
                }
            } else {
                convert::bgra_to_i420_cropped(raw, pitch, h, tight, &mut self.frame)
                    .map_err(|e| AppError::Capture(format!("BGRA→I420 失败: {e}")))?;
            }
        }

        self.stats.frames += 1;
        Ok(GrabOutcome::New)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主屏在原点 1920x1080；副屏在主屏左侧（x 为负）1680x1050
    const PRIMARY: (i32, i32, i32, i32) = (0, 0, 1920, 1080);
    const LEFT_MONITOR: (i32, i32, i32, i32) = (-1680, 0, 0, 1050);

    #[test]
    fn fullscreen_uses_whole_monitor() {
        let (crop, origin) = region_to_crop(CaptureMode::FullScreen, Region::default(), PRIMARY).unwrap();
        assert_eq!(crop, CropRect::new(0, 0, 1920, 1080));
        assert_eq!(origin, (0, 0));
    }

    #[test]
    fn region_on_primary_is_offset_by_monitor_origin() {
        let r = Region { x: 100, y: 50, width: 800, height: 600 };
        let (crop, origin) = region_to_crop(CaptureMode::Region, r, PRIMARY).unwrap();
        assert_eq!(crop, CropRect::new(100, 50, 800, 600));
        assert_eq!(origin, (100, 50));
    }

    #[test]
    fn negative_coordinates_do_not_wrap_around() {
        // 旧代码在这里 `x as usize` 会得到天文数字 → 越界读 GPU 内存
        let r = Region { x: -1680, y: 0, width: 1680, height: 1050 };
        let (crop, origin) = region_to_crop(CaptureMode::Region, r, LEFT_MONITOR).unwrap();
        assert_eq!(crop, CropRect::new(0, 0, 1680, 1050));
        assert_eq!(origin, (-1680, 0));
    }

    #[test]
    fn region_must_fit_inside_one_monitor() {
        // 完全落在左侧副屏内 → 正常换算
        let inside = Region { x: -1600, y: 0, width: 400, height: 300 };
        let (crop, origin) = region_to_crop(CaptureMode::Region, inside, LEFT_MONITOR).unwrap();
        assert_eq!(crop, CropRect::new(80, 0, 400, 300));
        assert_eq!(origin, (-1600, 0));

        // 从副屏跨到主屏（右边界越过 0）→ 必须报错，而不是静默截断
        let crossing = Region { x: -100, y: 0, width: 400, height: 300 };
        assert!(
            region_to_crop(CaptureMode::Region, crossing, LEFT_MONITOR).is_err(),
            "跨越显示器边界应报错"
        );

        // 区域其实在主屏上，却按副屏换算 → 报错（提示用户选错了显示器）
        let wrong_monitor = Region { x: 100, y: 0, width: 400, height: 300 };
        assert!(
            region_to_crop(CaptureMode::Region, wrong_monitor, LEFT_MONITOR).is_err(),
            "选区不属于该显示器应报错"
        );
    }

    #[test]
    fn odd_region_is_rounded_down_to_even() {
        let r = Region { x: 10, y: 10, width: 801, height: 601 };
        let (crop, _) = region_to_crop(CaptureMode::Region, r, PRIMARY).unwrap();
        assert_eq!((crop.width, crop.height), (800, 600));
    }

    #[test]
    fn monitor_rect_of_real_monitors_is_sane() {
        // 真机上跑：至少能枚举出显示器，且主显示器矩形有效
        let monitors = list_monitors().expect("枚举显示器失败");
        assert!(!monitors.is_empty(), "应至少有一个显示器");
        assert!(monitors.iter().any(|m| m.primary), "应存在主显示器");
        let primary = resolve_monitor(0).unwrap();
        let (l, t, r, b) = monitor_rect(&primary).unwrap();
        assert!(r > l && b > t, "主显示器矩形非法: {l},{t},{r},{b}");
        println!("枚举到 {} 个显示器: {:?}", monitors.len(), monitors.iter().map(|m| m.label()).collect::<Vec<_>>());
    }
}
