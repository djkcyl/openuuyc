//! Verified sender geometry rules, independent of capture/renderer APIs.
pub(crate) fn dimensions(quality: i32) -> (u32, u32) {
    match quality {
        1 => (1280, 720),
        3 => (2560, 1440),
        4 | 6 => (3840, 2160),
        _ => (1920, 1080),
    }
}

pub(crate) fn output_size(width: u32, height: u32, quality: i32) -> (u32, u32) {
    fit_size(width, height, dimensions(quality))
}

pub(crate) fn fit_size(width: u32, height: u32, maximum: (u32, u32)) -> (u32, u32) {
    if width <= maximum.0 && height <= maximum.1 {
        return ((width & !1).max(2), (height & !1).max(2));
    }
    // T CCE2C0/CD3A70: single-precision min scale, then 2*round(x/2).
    // The bounds keep their axes on portrait sources; they are not swapped.
    let scale = (maximum.0 as f32 / width as f32)
        .min(maximum.1 as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale * 0.5).round() as u32 * 2).max(2),
        ((height as f32 * scale * 0.5).round() as u32 * 2).max(2),
    )
}
