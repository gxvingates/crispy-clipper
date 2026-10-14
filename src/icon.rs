//! App icon drawn in code (used by build.rs for the .exe icon and at runtime for tray/window).

/// RGBA pixels: dark rounded square with a record dot. `active` = red dot, else grey.
pub fn rgba(size: u32, active: bool) -> Vec<u8> {
    let s = size as f32;
    let (r_corner, r_dot) = (s * 0.22, s * 0.26);
    let dot = if active { [255, 70, 70] } else { [150, 150, 160] };
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square coverage (1px anti-aliasing).
            let qx = (fx - s / 2.0).abs() - (s / 2.0 - r_corner);
            let qy = (fy - s / 2.0).abs() - (s / 2.0 - r_corner);
            let d_sq = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r_corner;
            let a_sq = (0.5 - d_sq).clamp(0.0, 1.0);
            let d_dot = (fx - s / 2.0).hypot(fy - s / 2.0) - r_dot;
            let a_dot = (0.5 - d_dot).clamp(0.0, 1.0);
            let bg = [24.0, 24.0, 30.0];
            let c: Vec<u8> = (0..3).map(|i| (bg[i] * (1.0 - a_dot) + dot[i] as f32 * a_dot) as u8).collect();
            px.extend([c[0], c[1], c[2], (a_sq * 255.0) as u8]);
        }
    }
    px
}

/// A .ico file (32-bit BMP entries) for the given sizes.
#[allow(dead_code)]
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let mut images = Vec::new();
    for &n in sizes {
        let rgba = rgba(n, true);
        let mut img = Vec::new();
        // BITMAPINFOHEADER; height is doubled to include the (empty) AND mask.
        for v in [40u32, n, n * 2] {
            img.extend(v.to_le_bytes());
        }
        img.extend(1u16.to_le_bytes());
        img.extend(32u16.to_le_bytes());
        img.extend([0u8; 24]);
        for y in (0..n).rev() {
            for x in 0..n {
                let i = ((y * n + x) * 4) as usize;
                img.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        let mask_row = ((n + 31) / 32 * 4) as usize;
        img.extend(vec![0u8; mask_row * n as usize]);
        images.push((n, img));
    }
    let mut out = Vec::new();
    out.extend([0u8, 0, 1, 0]);
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (n, img) in &images {
        let dim = if *n >= 256 { 0 } else { *n as u8 };
        out.extend([dim, dim, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((img.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for (_, img) in images {
        out.extend(img);
    }
    out
}
