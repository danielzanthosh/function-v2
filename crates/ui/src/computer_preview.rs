#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewGeometry {
    pub(super) width: f32,
    pub(super) height: f32,
    pub(super) scale: f32,
}

pub(super) fn preview_geometry(
    image_width: u32,
    image_height: u32,
    max_width: f32,
    max_height: f32,
) -> Option<PreviewGeometry> {
    if image_width == 0 || image_height == 0 || max_width <= 0.0 || max_height <= 0.0 {
        return None;
    }
    let scale = (max_width / image_width as f32).min(max_height / image_height as f32);
    Some(PreviewGeometry {
        width: image_width as f32 * scale,
        height: image_height as f32 * scale,
        scale,
    })
}

pub(super) fn preview_cursor_position(
    x: i32,
    y: i32,
    image_width: u32,
    image_height: u32,
    scale: f32,
) -> Option<(f32, f32)> {
    if image_width == 0 || image_height == 0 || scale <= 0.0 {
        return None;
    }
    Some((
        (x as f32).clamp(0.0, image_width.saturating_sub(1) as f32) * scale,
        (y as f32).clamp(0.0, image_height.saturating_sub(1) as f32) * scale,
    ))
}
