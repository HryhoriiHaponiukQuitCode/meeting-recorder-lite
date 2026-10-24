//! Tray icon drawn at runtime: a dot (grey idle, red recording, amber when
//! something needs attention) and two level bars, microphone and system.

pub const SIZE: u32 = 32;
/// Number of visible bar heights.
pub const STEPS: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    Idle,
    Recording,
    Problem,
}

/// Peak level (0..1) → bar height in steps, 6 dB per step from −48 dBFS.
pub fn level_steps(peak: f32) -> u8 {
    if peak <= 0.0 {
        return 0;
    }
    let db = 20.0 * peak.log10();
    ((db + 48.0) / 6.0).ceil().clamp(0.0, STEPS as f32) as u8
}

/// RGBA pixels, `SIZE`×`SIZE`.
pub fn render(dot: Dot, bars: [u8; 2]) -> Vec<u8> {
    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    let dot_rgb = match dot {
        Dot::Idle => [142, 142, 147],
        Dot::Recording => [255, 59, 48],
        Dot::Problem => [255, 159, 10],
    };
    let bar_rgb = if dot == Dot::Idle {
        [142, 142, 147]
    } else {
        [52, 199, 89]
    };
    let (cx, cy, r) = (10.0f32, 16.0f32, 8.5f32);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let mut cover = (r + 0.5 - d).clamp(0.0, 1.0);
            if dot == Dot::Idle {
                // Ring instead of a filled dot.
                cover = cover.min((d - (r - 3.0) + 0.5).clamp(0.0, 1.0));
            }
            if cover > 0.0 {
                put(&mut px, x, y, dot_rgb, cover);
            }
        }
    }
    let (bottom, top) = (28u32, 4u32);
    let step_px = (bottom - top) / STEPS as u32;
    for (i, &level) in bars.iter().enumerate() {
        let x0 = 21 + i as u32 * 6;
        let h = (level.min(STEPS) as u32 * step_px).max(2);
        for y in bottom - h..bottom {
            for x in x0..x0 + 4 {
                put(&mut px, x, y, bar_rgb, 1.0);
            }
        }
    }
    px
}

fn put(px: &mut [u8], x: u32, y: u32, rgb: [u8; 3], alpha: f32) {
    let i = ((y * SIZE + x) * 4) as usize;
    px[i..i + 3].copy_from_slice(&rgb);
    px[i + 3] = (alpha * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_steps() {
        assert_eq!(level_steps(0.0), 0);
        assert_eq!(level_steps(1.0), STEPS);
        assert_eq!(level_steps(0.001), 0); // −60 dBFS
        assert!(level_steps(0.1) > level_steps(0.01));
    }

    #[test]
    fn render_has_expected_size() {
        let px = render(Dot::Recording, [STEPS, 0]);
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        // Centre of the dot is opaque red.
        let i = ((16 * SIZE + 10) * 4) as usize;
        assert_eq!(&px[i..i + 4], &[255, 59, 48, 255]);
    }
}
