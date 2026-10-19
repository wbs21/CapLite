//! BGRA → I420 色彩转换（走本地 libyuv 静态库）。
//!
//! 关于 `ARGBToI420` 的名字：libyuv 的 `ARGB` 指的是**内存字节序 B,G,R,A**（不是 RGBA），
//! 所以我们把 DXGI 采集到的 BGRA 数据喂给它才是对的。这一点已用 MSVC 编译的 C 探针实测：
//! ```text
//! 纯红 0xFFFF0000 → Y=82 U=90 V=239   （若按 RGBA 解读会得到 U/V 互换的镜像结果）
//! 黑  → Y=16  U=128 V=128             白 → Y=235 U=128 V=128   （BT.601 limited range）
//! ```
//! stride 单位是**字节**；目标 stride 更宽时填充列不会被写。

use std::ffi::c_void;

extern "C" {
    fn ARGBToI420(
        src_bgra: *const u8,
        src_stride: i32,
        dst_y: *mut u8,
        dst_y_stride: i32,
        dst_u: *mut u8,
        dst_u_stride: i32,
        dst_v: *mut u8,
        dst_v_stride: i32,
        width: i32,
        height: i32,
    ) -> i32;
}

/// 一帧 I420（Y/U/V 平面各自连续存放，stride 分别等于 width / width/2）
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

impl I420Frame {
    pub fn new(width: usize, height: usize) -> Self {
        debug_assert!(width.is_multiple_of(2) && height.is_multiple_of(2), "I420 需要偶数宽高");
        let (w2, h2) = (width / 2, height / 2);
        Self {
            y: vec![0u8; width * height],
            u: vec![0u8; w2 * h2],
            v: vec![0u8; w2 * h2],
            width,
            height,
        }
    }

    pub fn stride_y(&self) -> usize {
        self.width
    }
    pub fn stride_uv(&self) -> usize {
        self.width / 2
    }
    pub fn byte_len(&self) -> usize {
        self.y.len() + self.u.len() + self.v.len()
    }
    pub fn matches(&self, w: usize, h: usize) -> bool {
        self.width == w && self.height == h
    }
    // P53：删掉 `to_packed()`（把 Y/U/V 三个平面拷进一个新的 `Vec<u8>`）。
    // 它**没有任何生产调用方** —— 全仓唯一的引用是它自己的那条单测。
    // 而它的代价不小：1080p 一帧就是约 3 MB 的分配 + 拷贝，若哪天真被请上热路径
    // 每帧都要付一次。按 P47「零调用 = 删」的口径删掉，连同那条只测它的用例。
    //
    // 编码器需要连续 Y/U/V 时，应当直接按平面喂（`I420Frame` 本来就分平面存放），
    // 而不是先拼一个临时大缓冲。
}

/// 采集帧内的裁剪矩形（相对该帧左上角，单位像素）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl CropRect {
    pub fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }
    /// 宽高向下取偶（H.264 4:2:0 要求）；可能返回 0 宽度，调用方需校验
    pub fn even(self) -> Self {
        Self {
            width: self.width & !1,
            height: self.height & !1,
            ..self
        }
    }
    /// 校验：非零、偶数、且完整落在 `frame_w × frame_h` 内
    pub fn validate(&self, frame_w: usize, frame_h: usize) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("裁剪区域宽高不能为 0".into());
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(format!(
                "裁剪区域宽高必须为偶数（H.264 4:2:0），当前 {}x{}",
                self.width, self.height
            ));
        }
        let right = self
            .x
            .checked_add(self.width)
            .ok_or_else(|| "裁剪区域 x+width 溢出".to_string())?;
        let bottom = self
            .y
            .checked_add(self.height)
            .ok_or_else(|| "裁剪区域 y+height 溢出".to_string())?;
        if right > frame_w || bottom > frame_h {
            return Err(format!(
                "裁剪区域越界：[{},{} {}x{}] 超出帧 {}x{}",
                self.x, self.y, self.width, self.height, frame_w, frame_h
            ));
        }
        Ok(())
    }
}

/// 把 BGRA 缓冲中的 `crop` 区域转成 I420 写入 `dst`（`dst` 尺寸必须等于 crop 尺寸）。
///
/// **零拷贝裁剪**：不复制源像素，只是把源指针偏移到 crop 左上角并把整帧 `src_pitch`
/// 作为源 stride 传给 libyuv —— 所以裁剪本身不产生额外内存带宽。
///
/// * `src_pitch`：整帧每行字节数（DXGI 的 `row_pitch`，可能大于 `width*4`）
/// * `src_height`：整帧高度（用于越界校验）
pub fn bgra_to_i420_cropped(
    src: &[u8],
    src_pitch: usize,
    src_height: usize,
    crop: CropRect,
    dst: &mut I420Frame,
) -> Result<(), String> {
    let frame_w = src_pitch / 4;
    crop.validate(frame_w, src_height)?;

    if !dst.matches(crop.width, crop.height) {
        return Err(format!(
            "目标 I420 尺寸 {}x{} 与裁剪区域 {}x{} 不一致",
            dst.width, dst.height, crop.width, crop.height
        ));
    }
    let need = src_pitch
        .checked_mul(src_height)
        .ok_or_else(|| "src_pitch*height 溢出".to_string())?;
    if src.len() < need {
        return Err(format!(
            "源缓冲太小：{} < {}（pitch {} × 高 {}）",
            src.len(),
            need,
            src_pitch,
            src_height
        ));
    }

    // SAFETY: 上面的 validate/长度校验保证 crop 区域完整落在缓冲内；
    // 指针偏移量为 crop.y*pitch + crop.x*4，仍在缓冲内；libyuv 只读 width/height 指定的区域。
    let src_ptr = unsafe { src.as_ptr().add(crop.y * src_pitch + crop.x * 4) };
    let ret = unsafe {
        ARGBToI420(
            src_ptr,
            src_pitch as i32,
            dst.y.as_mut_ptr(),
            dst.stride_y() as i32,
            dst.u.as_mut_ptr(),
            dst.stride_uv() as i32,
            dst.v.as_mut_ptr(),
            dst.stride_uv() as i32,
            crop.width as i32,
            crop.height as i32,
        )
    };
    if ret != 0 {
        return Err(format!("ARGBToI420 返回 {ret}"));
    }
    let _ = std::marker::PhantomData::<c_void>;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 `w×h` 的 BGRA 纯色图
    fn solid_bgra(w: usize, h: usize, b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            v.extend_from_slice(&[b, g, r, a]);
        }
        v
    }

    #[test]
    fn known_colors_match_msvc_c_probe() {
        // 这些期望值来自 MSVC 编译的 C 探针（BT.601 limited range）
        let cases: [(&str, [u8; 4], u8, u8, u8); 3] = [
            ("black", [0, 0, 0, 255], 16, 128, 128),
            ("white", [255, 255, 255, 255], 235, 128, 128),
            ("red", [0, 0, 255, 255], 82, 90, 239), // BGRA：B=0,G=0,R=255
        ];
        for (name, px, ey, eu, ev) in cases {
            let src = solid_bgra(4, 4, px[0], px[1], px[2], px[3]);
            let mut dst = I420Frame::new(4, 4);
            bgra_to_i420_cropped(&src, 16, 4, CropRect::new(0, 0, 4, 4), &mut dst).unwrap();
            assert!(
                (dst.y[0] as i32 - ey as i32).abs() <= 2,
                "{name}: Y={} 期望 {ey}",
                dst.y[0]
            );
            assert!(
                (dst.u[0] as i32 - eu as i32).abs() <= 2,
                "{name}: U={} 期望 {eu}",
                dst.u[0]
            );
            assert!(
                (dst.v[0] as i32 - ev as i32).abs() <= 2,
                "{name}: V={} 期望 {ev}",
                dst.v[0]
            );
        }
    }

    #[test]
    fn crop_picks_the_right_region_and_honours_pitch() {
        // 8x4：左半红、右半绿。裁剪右半应得到"绿"的 Y。
        let (w, h) = (8usize, 4usize);
        let pitch = w * 4;
        let mut src = vec![0u8; pitch * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * pitch + x * 4;
                if x < 4 {
                    src[o..o + 4].copy_from_slice(&[0, 0, 255, 255]); // 红
                } else {
                    src[o..o + 4].copy_from_slice(&[0, 255, 0, 255]); // 绿
                }
            }
        }
        let mut dst = I420Frame::new(4, 4);
        bgra_to_i420_cropped(&src, pitch, h, CropRect::new(4, 0, 4, 4), &mut dst).unwrap();
        // 绿（0,255,0,255）实测 Y=144
        assert!(
            (dst.y[0] as i32 - 144).abs() <= 3,
            "裁剪后半区应是绿: Y={}",
            dst.y[0]
        );

        // 带 padding 的源（pitch > width*4）也必须正确
        let padded_pitch = pitch + 64;
        let mut padded = vec![0xA5u8; padded_pitch * h];
        for y in 0..h {
            padded[y * padded_pitch..y * padded_pitch + pitch]
                .copy_from_slice(&src[y * pitch..(y + 1) * pitch]);
        }
        let mut dst2 = I420Frame::new(4, 4);
        bgra_to_i420_cropped(&padded, padded_pitch, h, CropRect::new(4, 0, 4, 4), &mut dst2).unwrap();
        assert_eq!(dst.y, dst2.y, "padding 不应影响结果");
    }

    #[test]
    fn invalid_crops_are_rejected() {
        let src = vec![0u8; 8 * 4 * 4];
        let mut dst = I420Frame::new(4, 4);
        // 奇数宽
        assert!(bgra_to_i420_cropped(&src, 32, 4, CropRect::new(0, 0, 3, 4), &mut dst).is_err());
        // 越界
        assert!(bgra_to_i420_cropped(&src, 32, 4, CropRect::new(6, 0, 4, 4), &mut dst).is_err());
        // 高度越界
        assert!(bgra_to_i420_cropped(&src, 32, 4, CropRect::new(0, 2, 4, 4), &mut dst).is_err());
        // 零尺寸
        assert!(bgra_to_i420_cropped(&src, 32, 4, CropRect::new(0, 0, 0, 0), &mut dst).is_err());
        // 源缓冲过小
        assert!(bgra_to_i420_cropped(&src[..16], 32, 4, CropRect::new(0, 0, 4, 4), &mut dst).is_err());
    }
}
