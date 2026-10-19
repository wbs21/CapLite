//! Deterministic synthetic I420 test frames and a small hash helper.
//!
//! Kept in `src/` (behind `#[doc(hidden)]`) so integration tests, examples and any
//! downstream crate can reproduce the exact frames this crate's test suite uses.

/// A synthetic I420 frame: tightly packed (stride == width for Y, width/2 for U/V).
#[derive(Clone, PartialEq, Eq)]
pub struct I420Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    pub fn stride_y(&self) -> usize {
        self.width
    }
    pub fn stride_uv(&self) -> usize {
        self.width / 2
    }
    pub fn len(&self) -> usize {
        self.y.len() + self.u.len() + self.v.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Contiguous I420 blob (Y then U then V), as libyuv's `ARGBToI420` would produce.
    pub fn to_i420(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        out.extend_from_slice(&self.y);
        out.extend_from_slice(&self.u);
        out.extend_from_slice(&self.v);
        out
    }
}

/// Builds frame `index` of a synthetic clip:
/// a smooth luma gradient, gently drifting chroma, plus a bright moving square.
///
/// The moving square guarantees real inter-frame difference, so P frames carry actual
/// residual data instead of being all-skip, and the encoder genuinely runs motion
/// estimation. Deterministic: the same `(width, height, index)` gives identical bytes.
pub fn synthetic_i420(width: usize, height: usize, index: usize) -> I420Frame {
    assert!(width > 0 && height > 0, "dimensions must be positive");
    assert!(width.is_multiple_of(2) && height.is_multiple_of(2), "I420 needs even dimensions");

    let cw = width / 2;
    let ch = height / 2;

    let mut y = vec![0u8; width * height];
    for row in 0..height {
        let base = row * width;
        let row_term = row * 40 / height.max(1);
        for col in 0..width {
            let val = 30 + (col * 150 / width.max(1)) + row_term;
            y[base + col] = val.min(235) as u8;
        }
    }

    // A square marching right, wrapping to the next row every `steps_x` frames.
    let bw = (width / 8).max(8);
    let bh = (height / 8).max(8);
    let steps_x = (width / 16).max(1);
    let steps_y = (height / 16).max(1);
    let bx = (index % steps_x) * 16;
    let by = ((index / steps_x) % steps_y) * 16;
    for row in by..(by + bh).min(height) {
        let base = row * width;
        for col in bx..(bx + bw).min(width) {
            y[base + col] = 235;
        }
    }

    let drift = ((index * 2) % 60) as u8;
    let u = vec![128u8.saturating_add(drift / 2); cw * ch];
    let v = vec![128u8.saturating_sub(drift / 2); cw * ch];

    I420Frame { width, height, y, u, v }
}

/// FNV-1a 64-bit hash: used by tests to prove two configurations really produced
/// *different* bitstreams rather than both producing nothing.
pub fn digest(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
