//! Tray icon: the QuitCode logo (`packaging/icons/tray.png`, made by
//! `packaging/make_icons.py`) with a status badge in the lower right corner —
//! none when idle, red while recording, amber when something needs attention.

use std::sync::OnceLock;

/// Number of steps in the menu level meters.
pub const STEPS: u8 = 8;

const LOGO_PNG: &[u8] = include_bytes!("../../../packaging/icons/tray.png");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    Idle,
    Recording,
    Problem,
}

/// Peak level (0..1) → meter length in steps, 6 dB per step from −48 dBFS.
pub fn level_steps(peak: f32) -> u8 {
    if peak <= 0.0 {
        return 0;
    }
    let db = 20.0 * peak.log10();
    ((db + 48.0) / 6.0).ceil().clamp(0.0, STEPS as f32) as u8
}

/// The decoded logo: (RGBA pixels, size).
fn logo() -> &'static (Vec<u8>, u32) {
    static LOGO: OnceLock<(Vec<u8>, u32)> = OnceLock::new();
    LOGO.get_or_init(|| {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(LOGO_PNG));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().expect("embedded tray.png is valid");
        let mut buf = vec![0; reader.output_buffer_size().expect("small image")];
        let info = reader
            .next_frame(&mut buf)
            .expect("embedded tray.png is valid");
        assert_eq!(
            info.color_type,
            png::ColorType::Rgba,
            "tray.png must be RGBA"
        );
        assert_eq!(info.width, info.height, "tray.png must be square");
        buf.truncate(info.buffer_size());
        (buf, info.width)
    })
}

/// RGBA pixels and their side length.
pub fn render(dot: Dot) -> (Vec<u8>, u32) {
    let (logo, size) = logo();
    let mut px = logo.clone();
    let rgb = match dot {
        Dot::Idle => return (px, *size),
        Dot::Recording => [255, 59, 48],
        Dot::Problem => [255, 159, 10],
    };
    // Badge with a transparent ring so it reads on any menu bar colour.
    let s = *size as f32;
    let (cx, cy, r, ring) = (s * 0.76, s * 0.76, s * 0.22, s * 0.07);
    for y in 0..*size {
        for x in 0..*size {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * size + x) * 4) as usize;
            let fill = (r + 0.5 - d).clamp(0.0, 1.0);
            let cut = (r + ring + 0.5 - d).clamp(0.0, 1.0);
            if fill > 0.0 {
                px[i..i + 3].copy_from_slice(&rgb);
                px[i + 3] = (fill * 255.0).round() as u8;
            } else if cut > 0.0 {
                px[i + 3] = (px[i + 3] as f32 * (1.0 - cut)).round() as u8;
            }
        }
    }
    (px, *size)
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
    fn badge_is_drawn_over_the_logo() {
        let (idle, size) = render(Dot::Idle);
        assert_eq!(idle.len(), (size * size * 4) as usize);
        let (rec, _) = render(Dot::Recording);
        let c = (size as f32 * 0.76) as u32;
        let i = ((c * size + c) * 4) as usize;
        assert_eq!(&rec[i..i + 4], &[255, 59, 48, 255]);
        assert_ne!(idle, rec);
    }
}
