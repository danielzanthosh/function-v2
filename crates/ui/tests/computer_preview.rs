#[path = "../src/computer_preview.rs"]
mod computer_preview;

use computer_preview::{preview_cursor_position, preview_geometry};

#[test]
fn cursor_coordinates_follow_the_contained_image_for_wide_and_tall_windows() {
    let wide = preview_geometry(1920, 1080, 460.0, 300.0).unwrap();
    assert_eq!((wide.width, wide.height), (460.0, 258.75));
    assert_eq!(
        preview_cursor_position(960, 540, 1920, 1080, wide.scale),
        Some((230.0, 129.375))
    );

    let tall = preview_geometry(800, 1600, 460.0, 300.0).unwrap();
    assert_eq!((tall.width, tall.height), (150.0, 300.0));
    assert_eq!(
        preview_cursor_position(400, 800, 800, 1600, tall.scale),
        Some((75.0, 150.0))
    );
    assert_eq!(
        preview_cursor_position(-2, 1800, 800, 1600, tall.scale),
        Some((0.0, 299.8125))
    );
}

#[test]
fn zero_sized_frames_do_not_create_cursor_geometry() {
    assert!(preview_geometry(0, 10, 460.0, 300.0).is_none());
    assert!(preview_geometry(10, 10, 0.0, 300.0).is_none());
    assert!(preview_cursor_position(2, 2, 0, 10, 1.0).is_none());
}
