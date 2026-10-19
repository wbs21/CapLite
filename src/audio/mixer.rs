//! 多路音频混音。
//!
//! 系统声音与麦克风都是 48 kHz / 2ch / f32（由 WASAPI 的 autoconvert 保证），
//! 所以混音只需要"逐样本相加 + 限幅"，不需要重采样。
//! 若某一路本轮没有数据（静音/设备暂时无输出），按静音补齐，保证两路时间轴对齐。

use crate::audio::capture::TARGET_CHANNELS;

/// 把多路交错 f32 相加并限幅到 `[-1, 1]`，结果写入 `out`（会先清空）。
/// 返回混合后的**帧数**。
pub fn mix_into(sources: &[&[f32]], out: &mut Vec<f32>) -> usize {
    let max_frames = sources
        .iter()
        .map(|s| s.len() / TARGET_CHANNELS)
        .max()
        .unwrap_or(0);
    out.clear();
    out.reserve(max_frames * TARGET_CHANNELS);

    for f in 0..max_frames {
        let base = f * TARGET_CHANNELS;
        for c in 0..TARGET_CHANNELS {
            let mut acc = 0.0f32;
            for s in sources {
                let idx = base + c;
                if idx < s.len() {
                    acc += s[idx];
                }
            }
            // 硬限幅：宁可轻微削顶，也不要因为溢出产生爆音
            out.push(acc.clamp(-1.0, 1.0));
        }
    }
    max_frames
}

/// 向缓冲追加 `frames` 帧静音
pub fn push_silence(out: &mut Vec<f32>, frames: usize) {
    out.resize(out.len() + frames * TARGET_CHANNELS, 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixes_two_sources_and_clamps() {
        let a = vec![0.5f32, 0.5, -0.25, -0.25]; // 2 帧立体声
        let b = vec![0.25f32, 0.25, -0.25, -0.25];
        let mut out = Vec::new();
        let frames = mix_into(&[&a, &b], &mut out);
        assert_eq!(frames, 2);
        assert_eq!(out, vec![0.75, 0.75, -0.5, -0.5]);

        // 溢出必须被限幅
        let loud = vec![0.9f32, 0.9];
        let louder = vec![0.9f32, 0.9];
        mix_into(&[&loud, &louder], &mut out);
        assert_eq!(out, vec![1.0, 1.0], "1.8 应被限幅到 1.0");
    }

    #[test]
    fn shorter_source_is_padded_with_silence() {
        let long = vec![0.25f32; 8]; // 4 帧
        let short = vec![0.5f32, 0.5]; // 1 帧
        let mut out = Vec::new();
        let frames = mix_into(&[&long, &short], &mut out);
        assert_eq!(frames, 4);
        assert_eq!(&out[0..2], &[0.75, 0.75]);
        assert_eq!(&out[2..4], &[0.25, 0.25], "短的那路后面按静音补齐");
    }

    #[test]
    fn empty_input_is_handled() {
        let mut out = vec![1.0];
        assert_eq!(mix_into(&[], &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn silence_helper_appends_expected_length() {
        let mut v = vec![0.1f32];
        push_silence(&mut v, 3);
        assert_eq!(v.len(), 1 + 3 * TARGET_CHANNELS);
    }
}
