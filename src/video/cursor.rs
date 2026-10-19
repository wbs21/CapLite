//! 鼠标指针合成：DXGI Desktop Duplication **不包含鼠标指针**，需要在采集帧上自行画出。
//!
//! 做法（不依赖 `DrawIconEx` 的 GDI 叠加，而是自己按位图做 alpha 合成，边缘更干净）：
//! 1. `GetCursorInfo` 取当前光标句柄与屏幕位置（未显示则跳过）
//! 2. `GetIconInfo` 取彩色位图 + 单色 AND 掩码位图
//! 3. `GetDIBits` 把两张位图取出到内存（32bpp 顶朝下 BGRA / 1bpp 掩码）
//! 4. 逐像素 alpha 混合进采集帧；`alpha == 0` 的像素再按 AND 掩码决定"涂黑"还是"透明"
//! 5. 用热点（`xHotspot`/`yHotspot`）修正绘制位置，并裁剪到帧内
//!
//! 注意坐标系：光标位置是**虚拟桌面物理像素**。调用方需要减去显示器原点与裁剪偏移。
//! 这要求进程是 per-monitor DPI aware（eframe/winit 会设置），否则坐标会被 DPI 虚拟化。

use crate::error::AppError;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, ICONINFO,
};

/// 一次光标读取的结果（已转成内存位图，可直接合成）
pub(crate) struct CursorImage {
    pub width: usize,
    pub height: usize,
    /// 顶朝下的 BGRA；`None` 表示单色光标（只有掩码）
    pub color: Option<Vec<u8>>,
    /// 1bpp AND 掩码，行按 4 字节对齐
    pub mask: Option<Vec<u8>>,
    pub mask_pitch: usize,
    pub hotspot: (i32, i32),
    /// 光标在虚拟桌面中的位置（物理像素）
    pub screen_pos: (i32, i32),
}

impl CursorImage {
    /// 该光标应该画在帧内的哪个位置（帧已相对某偏移裁剪）
    pub fn frame_pos(&self, frame_origin: (i32, i32)) -> (i32, i32) {
        (
            self.screen_pos.0 - self.hotspot.0 - frame_origin.0,
            self.screen_pos.1 - self.hotspot.1 - frame_origin.1,
        )
    }
}

/// 读取当前光标。返回 `Ok(None)` 表示此刻不显示光标。
pub(crate) fn read_cursor() -> Result<Option<CursorImage>, AppError> {
    unsafe {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        GetCursorInfo(&mut ci).map_err(|e| AppError::Capture(format!("GetCursorInfo: {e}")))?;
        read_cursor_from(ci)
    }
}

/// 从已取到的 `CURSORINFO` 读位图。抽出来给 [`CursorCache`] 复用，避免每帧
/// 重复 `GetCursorInfo`。
fn read_cursor_from(ci: CURSORINFO) -> Result<Option<CursorImage>, AppError> {
    unsafe {
        if ci.flags != CURSOR_SHOWING || ci.hCursor.is_invalid() {
            return Ok(None);
        }

        let mut ii = ICONINFO::default();
        GetIconInfo(ci.hCursor.into(), &mut ii)
            .map_err(|e| AppError::Capture(format!("GetIconInfo: {e}")))?;
        // 位图句柄用完必须释放
        let _guard = BitmapGuard(ii.hbmColor, ii.hbmMask);

        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return Err(AppError::Capture("GetDC 失败".into()));
        }
        let result = read_bitmaps(hdc, &ii, ci.ptScreenPos);
        ReleaseDC(None, hdc);
        result.map(Some)
    }
}

/// 光标位图缓存。
///
/// `GetCursorInfo` 每帧都要调（位置会变），但 `GetIconInfo` + 两次 `GetDIBits`
/// 只在**光标句柄变化**或每 100 ms（覆盖动画光标）时做。旧实现每帧都做整套，
/// 4K/60fps 下是持续的 GDI 调用 + 两次位图分配。
pub(crate) struct CursorCache {
    hcursor: isize,
    image: Option<CursorImage>,
    refreshed: Option<Instant>,
}

impl CursorCache {
    pub fn new() -> Self {
        Self {
            hcursor: 0,
            image: None,
            refreshed: None,
        }
    }

    /// 读当前光标；返回的引用在本结构下一次 `read` 前有效。位置字段会每帧更新。
    pub fn read(&mut self) -> Result<Option<&CursorImage>, AppError> {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            GetCursorInfo(&mut ci)
                .map_err(|e| AppError::Capture(format!("GetCursorInfo: {e}")))?;
        }
        if ci.flags != CURSOR_SHOWING || ci.hCursor.is_invalid() {
            return Ok(None);
        }

        let handle = ci.hCursor.0 as isize;
        let now = Instant::now();
        let refresh = self.hcursor != handle
            || self.image.is_none()
            || self
                .refreshed
                .map(|t| now.saturating_duration_since(t) >= Duration::from_millis(100))
                .unwrap_or(true);
        if refresh {
            match read_cursor_from(ci)? {
                Some(mut img) => {
                    self.hcursor = handle;
                    img.screen_pos = (ci.ptScreenPos.x, ci.ptScreenPos.y);
                    self.image = Some(img);
                    self.refreshed = Some(now);
                }
                None => self.image = None,
            }
        }
        if let Some(img) = self.image.as_mut() {
            img.screen_pos = (ci.ptScreenPos.x, ci.ptScreenPos.y);
        }
        Ok(self.image.as_ref())
    }
}

/// 保证 `GetIconInfo` 返回的两张位图被释放
struct BitmapGuard(HBITMAP, HBITMAP);
impl Drop for BitmapGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_invalid() {
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(HGDIOBJ(self.0.0));
            }
            if !self.1.is_invalid() {
                let _ = windows::Win32::Graphics::Gdi::DeleteObject(HGDIOBJ(self.1.0));
            }
        }
    }
}

unsafe fn read_bitmaps(
    hdc: HDC,
    ii: &ICONINFO,
    pos: POINT,
) -> Result<CursorImage, AppError> {
    // 尺寸优先看彩色位图，其次看掩码位图（掩码是双倍高度存放 XOR 部分）
    let mut width = 0usize;
    let mut height = 0usize;
    for hbm in [ii.hbmColor, ii.hbmMask] {
        if hbm.is_invalid() {
            continue;
        }
        let mut bmp = BITMAP::default();
        let n = GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut std::ffi::c_void),
        );
        if n > 0 {
            width = width.max(bmp.bmWidth.max(0) as usize);
            height = height.max(bmp.bmHeight.max(0) as usize);
        }
    }
    if width == 0 || height == 0 {
        return Err(AppError::Capture("光标位图尺寸为 0".into()));
    }

    let color = if !ii.hbmColor.is_invalid() {
        Some(get_dibits_bgra(hdc, ii.hbmColor, width, height)?)
    } else {
        None
    };

    let mask_pitch = width.div_ceil(32) * 4;
    let mask = if !ii.hbmMask.is_invalid() {
        // 单色光标时 hbmMask 高度是 2*height（AND + XOR）；这里只取前一半的 AND 部分
        let h = if color.is_none() { height / 2 } else { height };
        let h = h.max(1);
        Some(get_dibits_mask(hdc, ii.hbmMask, width, h)?)
    } else {
        None
    };

    Ok(CursorImage {
        width,
        height: match &mask {
            Some(m) if color.is_none() => (m.len() / mask_pitch).max(1),
            _ => height,
        },
        color,
        mask,
        mask_pitch,
        hotspot: (ii.xHotspot as i32, ii.yHotspot as i32),
        screen_pos: (pos.x, pos.y),
    })
}

unsafe fn get_dibits_bgra(
    hdc: HDC,
    hbm: HBITMAP,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, AppError> {
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // 负数 = 顶朝下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; width * height * 4];
    let lines = GetDIBits(
        hdc,
        hbm,
        0,
        height as u32,
        Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
        &mut bi,
        DIB_RGB_COLORS,
    );
    if lines == 0 {
        return Err(AppError::Capture("GetDIBits(彩色位图) 失败".into()));
    }
    Ok(buf)
}

unsafe fn get_dibits_mask(
    hdc: HDC,
    hbm: HBITMAP,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, AppError> {
    let pitch = width.div_ceil(32) * 4;
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; pitch * height];
    let lines = GetDIBits(
        hdc,
        hbm,
        0,
        height as u32,
        Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
        &mut bi,
        DIB_RGB_COLORS,
    );
    if lines == 0 {
        return Err(AppError::Capture("GetDIBits(掩码位图) 失败".into()));
    }
    Ok(buf)
}

/// 把光标合成到 BGRA 帧上。`at` 为光标左上角在帧内的坐标（可为负，会被裁剪）。
/// 返回是否真的画了像素。
pub(crate) fn composite(frame: &mut [u8], pitch: usize, frame_w: usize, frame_h: usize, at: (i32, i32), cur: &CursorImage) -> bool {
    let mut any = false;
    for cy in 0..cur.height as i32 {
        let dy = at.1 + cy;
        if dy < 0 || dy >= frame_h as i32 {
            continue;
        }
        for cx in 0..cur.width as i32 {
            let dx = at.0 + cx;
            if dx < 0 || dx >= frame_w as i32 {
                continue;
            }
            let (b, g, r, a) = match &cur.color {
                Some(color) => {
                    let o = (cy as usize * cur.width + cx as usize) * 4;
                    (color[o], color[o + 1], color[o + 2], color[o + 3])
                }
                None => (0, 0, 0, 0),
            };

            // 掩码位为 1 表示"该处不画"（AND 掩码语义）
            let masked_out = match &cur.mask {
                Some(m) => {
                    let byte = m
                        .get(cy as usize * cur.mask_pitch + (cx as usize) / 8)
                        .copied()
                        .unwrap_or(0xFF);
                    (byte >> (7 - (cx as usize) % 8)) & 1 == 1
                }
                None => false,
            };

            let dst_o = dy as usize * pitch + dx as usize * 4;
            if dst_o + 3 >= frame.len() {
                continue;
            }

            if a == 0 {
                // 32bpp 光标里 alpha=0 的像素：由掩码决定是"画黑"还是"透明"
                if cur.color.is_some() && !masked_out {
                    frame[dst_o] = 0;
                    frame[dst_o + 1] = 0;
                    frame[dst_o + 2] = 0;
                    frame[dst_o + 3] = 255;
                    any = true;
                }
                continue;
            }
            if masked_out && cur.color.is_none() {
                continue;
            }
            let af = a as u32;
            let inv = 255 - af;
            for (i, src) in [b, g, r].iter().enumerate() {
                let d = frame[dst_o + i] as u32;
                frame[dst_o + i] = ((*src as u32 * af + d * inv + 127) / 255) as u8;
            }
            frame[dst_o + 3] = 255;
            any = true;
        }
    }
    any
}

/// 便捷入口：读取并合成当前光标。`frame_origin` 是帧左上角在虚拟桌面中的物理坐标。
pub fn draw_current_cursor(
    frame: &mut [u8],
    pitch: usize,
    frame_w: usize,
    frame_h: usize,
    frame_origin: (i32, i32),
) -> Result<bool, AppError> {
    let Some(cur) = read_cursor()? else {
        return Ok(false);
    };
    let at = cur.frame_pos(frame_origin);
    Ok(composite(frame, pitch, frame_w, frame_h, at, &cur))
}

/// 供单元测试：把一段内存位图当光标合成（不碰系统光标）
#[cfg(test)]
fn composite_for_test(frame: &mut [u8], pitch: usize, w: usize, h: usize, at: (i32, i32), cur: &CursorImage) -> bool {
    composite(frame, pitch, w, h, at, cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor_32x32() -> CursorImage {
        // 4x4 全不透明红光标，无掩码
        let mut color = Vec::new();
        for _ in 0..16 {
            color.extend_from_slice(&[0, 0, 255, 255]); // BGRA 红
        }
        CursorImage {
            width: 4,
            height: 4,
            color: Some(color),
            mask: None,
            mask_pitch: 4,
            hotspot: (0, 0),
            screen_pos: (0, 0),
        }
    }

    #[test]
    fn opaque_cursor_overwrites_pixels() {
        let (w, h) = (8usize, 8usize);
        let mut frame = vec![255u8; w * h * 4]; // 白底
        let cur = cursor_32x32();
        assert!(composite_for_test(&mut frame, w * 4, w, h, (1, 1), &cur));
        let o = (w + 1) * 4;
        assert_eq!(&frame[o..o + 3], &[0, 0, 255], "应变成红");
        // 区域外不受影响
        let o2 = 0;
        assert_eq!(&frame[o2..o2 + 3], &[255, 255, 255]);
    }

    #[test]
    fn half_transparent_cursor_blends() {
        let (w, h) = (4usize, 4usize);
        let mut frame = vec![0u8; w * h * 4]; // 黑底
        let mut color = Vec::new();
        for _ in 0..16 {
            color.extend_from_slice(&[255, 255, 255, 128]); // 50% 白
        }
        let cur = CursorImage {
            width: 4,
            height: 4,
            color: Some(color),
            mask: None,
            mask_pitch: 4,
            hotspot: (0, 0),
            screen_pos: (0, 0),
        };
        composite_for_test(&mut frame, w * 4, w, h, (0, 0), &cur);
        // 0*0.5 + 255*0.5 ≈ 128
        assert!((frame[0] as i32 - 128).abs() <= 2, "混合结果 {}", frame[0]);
    }

    #[test]
    fn negative_position_is_clipped() {
        let (w, h) = (4usize, 4usize);
        let mut frame = vec![0u8; w * h * 4];
        let cur = cursor_32x32();
        // 画在 (-2,-2)：只有右下 2x2 落在帧内
        assert!(composite_for_test(&mut frame, w * 4, w, h, (-2, -2), &cur));
        assert_eq!(&frame[0..3], &[0, 0, 255], "左上角应被覆盖");
        // 完全在帧外则什么都不画
        let mut f2 = vec![0u8; w * h * 4];
        assert!(!composite_for_test(&mut f2, w * 4, w, h, (-100, -100), &cur));
    }

    #[test]
    fn hotspot_offsets_position() {
        let mut cur = cursor_32x32();
        cur.hotspot = (2, 3);
        cur.screen_pos = (10, 20);
        // 帧原点为 (0,0) → 绘制点 = (10-2, 20-3)
        assert_eq!(cur.frame_pos((0, 0)), (8, 17));
        // 帧原点非零（例如裁剪了区域）
        assert_eq!(cur.frame_pos((5, 5)), (3, 12));
    }
}
