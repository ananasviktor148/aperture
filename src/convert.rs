//! BGRA -> NV12 conversion with optional bilinear downscale.
//!
//! H.264 hardware encoders require NV12 input, while Windows Graphics Capture
//! produces BGRA. Conversion runs across all cores because it sits directly in
//! the capture callback's critical path.

use rayon::prelude::*;

pub struct Nv12 {
    pub data: Vec<u8>,
}

impl Nv12 {
    pub fn with_buffer(mut data: Vec<u8>, width: u32, height: u32) -> Self {
        data.resize((width * height * 3 / 2) as usize, 0);
        Self { data }
    }
}

/// Fit `src` into the given bounds, preserving aspect ratio, rounding down to
/// even dimensions (NV12 and H.264 both require even sizes).
pub fn fit_even(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = f64::min(
        1.0,
        f64::min(max_w as f64 / src_w as f64, max_h as f64 / src_h as f64),
    );
    let w = ((src_w as f64 * scale) as u32).max(2) & !1;
    let h = ((src_h as f64 * scale) as u32).max(2) & !1;
    (w, h)
}

struct Axis {
    lo: Vec<u32>,
    hi: Vec<u32>,
    weight: Vec<u32>,
}

fn build_axis(dst: u32, src: u32) -> Axis {
    let ratio = src as f64 / dst as f64;
    let mut lo = Vec::with_capacity(dst as usize);
    let mut hi = Vec::with_capacity(dst as usize);
    let mut weight = Vec::with_capacity(dst as usize);
    for d in 0..dst {
        let pos = (d as f64 + 0.5) * ratio - 0.5;
        let pos = pos.clamp(0.0, (src - 1) as f64);
        let base = pos.floor() as u32;
        let frac = pos - base as f64;
        lo.push(base);
        hi.push((base + 1).min(src - 1));
        weight.push((frac * 256.0) as u32);
    }
    Axis { lo, hi, weight }
}

#[inline(always)]
fn blend(a: i32, b: i32, w: i32) -> i32 {
    a + (((b - a) * w) >> 8)
}

#[inline(always)]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b) >> 8) + 16).clamp(0, 255) as u8
}

#[inline(always)]
fn rgb_to_u(r: i32, g: i32, b: i32) -> u8 {
    (((-26 * r - 86 * g + 112 * b) >> 8) + 128).clamp(0, 255) as u8
}

#[inline(always)]
fn rgb_to_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 102 * g - 10 * b) >> 8) + 128).clamp(0, 255) as u8
}

/// Convert a BGRA frame into `out` as NV12, scaling to `dst_w` x `dst_h`.
///
/// `pitch` is the source row stride in bytes (may exceed `src_w * 4`).
pub fn bgra_to_nv12(
    src: &[u8],
    pitch: usize,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    out: &mut [u8],
) {
    debug_assert!(dst_w % 2 == 0 && dst_h % 2 == 0);

    let xs = build_axis(dst_w, src_w);
    let ys = build_axis(dst_h, src_h);
    let (y_len, uv_len) = (
        (dst_w * dst_h) as usize,
        (dst_w * dst_h / 2) as usize,
    );
    debug_assert!(out.len() >= y_len + uv_len);
    let (y_plane, uv_plane) = out.split_at_mut(y_len);

    // Sample one source pixel location, bilinearly filtered.
    #[inline(always)]
    fn rgb_at(src: &[u8], pitch: usize, xs: &Axis, ys: &Axis, x: u32, y: u32) -> (i32, i32, i32) {
        let x0 = xs.lo[x as usize] as usize * 4;
        let x1 = xs.hi[x as usize] as usize * 4;
        let wx = xs.weight[x as usize] as i32;
        let y0 = ys.lo[y as usize] as usize * pitch;
        let y1 = ys.hi[y as usize] as usize * pitch;
        let wy = ys.weight[y as usize] as i32;

        let p00 = y0 + x0;
        let p01 = y0 + x1;
        let p10 = y1 + x0;
        let p11 = y1 + x1;
        let b = blend(
            blend(src[p00] as i32, src[p01] as i32, wx),
            blend(src[p10] as i32, src[p11] as i32, wx),
            wy,
        );
        let g = blend(
            blend(src[p00 + 1] as i32, src[p01 + 1] as i32, wx),
            blend(src[p10 + 1] as i32, src[p11 + 1] as i32, wx),
            wy,
        );
        let r = blend(
            blend(src[p00 + 2] as i32, src[p01 + 2] as i32, wx),
            blend(src[p10 + 2] as i32, src[p11 + 2] as i32, wx),
            wy,
        );
        (r, g, b)
    }

    y_plane
        .par_chunks_mut(dst_w as usize)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, px) in row.iter_mut().enumerate() {
                let (r, g, b) = rgb_at(src, pitch, &xs, &ys, x as u32, y as u32);
                *px = rgb_to_y(r, g, b);
            }
        });

    let cw = dst_w / 2;
    uv_plane
        .par_chunks_mut(dst_w as usize)
        .enumerate()
        .for_each(|(cy, row)| {
            for cx in 0..cw as usize {
                let dx = (cx as u32) * 2;
                let dy = (cy as u32) * 2;
                let (r0, g0, b0) = rgb_at(src, pitch, &xs, &ys, dx, dy);
                let (r1, g1, b1) = rgb_at(src, pitch, &xs, &ys, dx + 1, dy);
                let (r2, g2, b2) = rgb_at(src, pitch, &xs, &ys, dx, dy + 1);
                let (r3, g3, b3) = rgb_at(src, pitch, &xs, &ys, dx + 1, dy + 1);
                let r = (r0 + r1 + r2 + r3) >> 2;
                let g = (g0 + g1 + g2 + g3) >> 2;
                let b = (b0 + b1 + b2 + b3) >> 2;
                row[cx * 2] = rgb_to_u(r, g, b);
                row[cx * 2 + 1] = rgb_to_v(r, g, b);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_black_and_white() {
        let (w, h) = (16u32, 8u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for px in src.chunks_exact_mut(4) {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
            px[3] = 255;
        }
        let mut out = vec![0u8; (w * h * 3 / 2) as usize];
        bgra_to_nv12(&src, (w * 4) as usize, w, h, w, h, &mut out);
        assert_eq!(out[0], 16, "black maps to limited-range Y=16");
        assert_eq!(out[(w * h) as usize], 128, "neutral chroma");

        for px in src.chunks_exact_mut(4) {
            px[0] = 255;
            px[1] = 255;
            px[2] = 255;
        }
        bgra_to_nv12(&src, (w * 4) as usize, w, h, w, h, &mut out);
        assert!(out[0] >= 234, "white maps to ~Y=235, got {}", out[0]);
    }

    #[test]
    fn downscale_produces_valid_planes() {
        let (sw, sh) = (64u32, 36u32);
        let (dw, dh) = (32u32, 18u32);
        let src = vec![120u8; (sw * sh * 4) as usize];
        let mut out = vec![0u8; (dw * dh * 3 / 2) as usize];
        bgra_to_nv12(&src, (sw * 4) as usize, sw, sh, dw, dh, &mut out);
        assert!(out.iter().all(|&v| v > 0));
    }

    #[test]
    fn fit_even_keeps_aspect() {
        assert_eq!(fit_even(2560, 1440, 1920, 1080), (1920, 1080));
        assert_eq!(fit_even(2560, 1440, 1280, 1280), (1280, 720));
        assert_eq!(fit_even(1920, 1080, 3840, 2160), (1920, 1080));
    }
}
