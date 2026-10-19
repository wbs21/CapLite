pub fn bgra_to_i420(bgra: &[u8], width: usize, height: usize, out_yuv: &mut Vec<u8>) {
    let y_size = width * height;
    let uv_size = y_size / 4;
    out_yuv.resize(y_size + uv_size * 2, 0);
    let (y_plane, rest) = out_yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    #[cfg(target_arch = "x86_64")]
    {
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(2) {
                let idx0 = (y * width + x) * 4;
                let idx1 = idx0 + 4;
                let idx2 = ((y + 1) * width + x) * 4;
                let idx3 = idx2 + 4;
                let b0 = bgra[idx0]; let g0 = bgra[idx0+1]; let r0 = bgra[idx0+2];
                let b1 = bgra[idx1]; let g1 = bgra[idx1+1]; let r1 = bgra[idx1+2];
                let b2 = bgra[idx2]; let g2 = bgra[idx2+1]; let r2 = bgra[idx2+2];
                let b3 = bgra[idx3]; let g3 = bgra[idx3+1]; let r3 = bgra[idx3+2];

                fn rgb2y(r: u8, g: u8, b: u8) -> u8 {
                    ((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) + 0.5) as u8
                }
                fn rgb2u(r: u8, g: u8, b: u8) -> u8 {
                    ((-0.14713 * r as f32 -0.28886 * g as f32 +0.436 * b as f32)+128.0 +0.5) as u8
                }
                fn rgb2v(r: u8, g: u8, b: u8) -> u8 {
                    ((0.615 * r as f32 -0.51499 * g as f32 -0.10001 * b as f32)+128.0 +0.5) as u8
                }
                let y0 = rgb2y(r0,g0,b0);
                let y1 = rgb2y(r1,g1,b1);
                let y2 = rgb2y(r2,g2,b2);
                let y3 = rgb2y(r3,g3,b3);
                let u = ((rgb2u(r0,g0,b0) as u16 + rgb2u(r1,g1,b1) as u16 + rgb2u(r2,g2,b2) as u16 + rgb2u(r3,g3,b3) as u16)/4) as u8;
                let v = ((rgb2v(r0,g0,b0) as u16 + rgb2v(r1,g1,b1) as u16 + rgb2v(r2,g2,b2) as u16 + rgb2v(r3,g3,b3) as u16)/4) as u8;

                y_plane[y*width + x] = y0;
                y_plane[y*width + x+1] = y1;
                y_plane[(y+1)*width + x] = y2;
                y_plane[(y+1)*width + x+1] = y3;
                let uv_idx = (y/2)*(width/2)+(x/2);
                u_plane[uv_idx] = u;
                v_plane[uv_idx] = v;
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        for y in 0..height {
            for x in 0..width {
                let pos = (y*width + x)*4;
                let b = bgra[pos];
                let g = bgra[pos+1];
                let r = bgra[pos+2];
                let y_val = ((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32)+0.5) as u8;
                y_plane[y*width + x] = y_val;
            }
        }
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(2) {
                let mut sum_u = 0u16;
                let mut sum_v = 0u16;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let pos = ((y+dy)*width + (x+dx)) *4;
                        let b = bgra[pos];
                        let g = bgra[pos+1];
                        let r = bgra[pos+2];
                        let u = ((-0.14713 * r as f32 -0.28886 * g as f32 +0.436 * b as f32)+128.0 +0.5) as u16;
                        let v = ((0.615 * r as f32 -0.51499 * g as f32 -0.10001 * b as f32)+128.0 +0.5) as u16;
                        sum_u += u;
                        sum_v += v;
                    }
                }
                let uv_idx = (y/2)*(width/2)+(x/2);
                u_plane[uv_idx] = (sum_u/4) as u8;
                v_plane[uv_idx] = (sum_v/4) as u8;
            }
        }
    }
}
