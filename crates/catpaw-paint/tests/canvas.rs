//! The Canvas 2D backend: shapes, state, text and pixels.

use catpaw_paint::canvas::{Canvas2d, Font, Path2d, Style, TextAlign, parse_color};
use catpaw_paint::tiny_skia::Transform;

fn pixel(canvas: &Canvas2d, x: u32, y: u32) -> [u8; 4] {
    let data = canvas.image_data(x as i32, y as i32, 1, 1);
    [data[0], data[1], data[2], data[3]]
}

#[test]
fn rects_fill_stroke_and_clear() {
    let mut c = Canvas2d::new(100, 60);
    assert_eq!(pixel(&c, 5, 5), [0, 0, 0, 0], "starts transparent");
    c.state_mut().fill = Style::Color(parse_color("rgb(255, 0, 0)").unwrap());
    c.fill_rect(10.0, 10.0, 30.0, 20.0);
    assert_eq!(pixel(&c, 20, 20), [255, 0, 0, 255]);
    assert_eq!(pixel(&c, 5, 5), [0, 0, 0, 0]);
    c.state_mut().stroke = Style::Color(parse_color("#00f").unwrap());
    c.state_mut().line_width = 4.0;
    c.stroke_rect(50.0, 10.0, 30.0, 20.0);
    assert_eq!(pixel(&c, 50, 20), [0, 0, 255, 255], "on the stroke");
    assert_eq!(pixel(&c, 65, 20), [0, 0, 0, 0], "inside is untouched");
    c.clear_rect(0.0, 0.0, 100.0, 60.0);
    assert_eq!(pixel(&c, 20, 20), [0, 0, 0, 0]);
}

#[test]
fn transforms_and_alpha_apply() {
    let mut c = Canvas2d::new(100, 100);
    c.state_mut().fill = Style::Color(parse_color("black").unwrap());
    c.translate(50.0, 50.0);
    c.rotate(std::f32::consts::FRAC_PI_4);
    c.save();
    c.state_mut().global_alpha = 0.5;
    c.fill_rect(-10.0, -10.0, 20.0, 20.0);
    c.restore();
    let center = pixel(&c, 50, 50);
    assert_eq!(center[3], 128, "{center:?}");
    assert_eq!(
        pixel(&c, 50, 30),
        [0, 0, 0, 0],
        "outside the rotated square"
    );
    assert_eq!(pixel(&c, 58, 58)[3], 0, "the corner was rotated away");
    assert_eq!(
        pixel(&c, 50, 58)[3],
        128,
        "along the rotated axis is inside"
    );
    assert_eq!(c.state().global_alpha, 1.0, "restored");
    c.set_transform(Transform::identity());
    assert!(c.transform().is_identity());
}

#[test]
fn arcs_and_hit_testing() {
    let mut c = Canvas2d::new(100, 100);
    let mut p = Path2d::default();
    p.arc(50.0, 50.0, 20.0, 0.0, std::f32::consts::TAU, false);
    p.close();
    assert!(c.is_point_in_path(Some(&p), 50.0, 50.0, false));
    assert!(c.is_point_in_path(Some(&p), 65.0, 50.0, false));
    assert!(!c.is_point_in_path(Some(&p), 75.0, 50.0, false));
    c.state_mut().fill = Style::Color(parse_color("green").unwrap());
    c.fill_path(&p, false);
    assert_eq!(pixel(&c, 50, 50), [0, 128, 0, 255]);
    assert_eq!(pixel(&c, 90, 90), [0, 0, 0, 0]);
    // Clipping keeps later drawing inside the circle.
    c.clip(Some(&p), false);
    c.state_mut().fill = Style::Color(parse_color("blue").unwrap());
    c.fill_rect(0.0, 0.0, 100.0, 100.0);
    assert_eq!(pixel(&c, 50, 50), [0, 0, 255, 255]);
    assert_eq!(pixel(&c, 5, 5), [0, 0, 0, 0]);
}

#[test]
fn text_draws_and_measures() {
    let mut c = Canvas2d::new(200, 60);
    c.state_mut().font = Font::parse("bold 24px sans-serif").unwrap();
    assert_eq!(c.state().font.css, "bold 24px sans-serif");
    let metrics = c.measure_text("Hello");
    assert!(metrics.width > 40.0 && metrics.width < 120.0, "{metrics:?}");
    assert!(metrics.font_ascent > 10.0, "{metrics:?}");
    c.state_mut().fill = Style::Color(parse_color("black").unwrap());
    c.draw_text("Hello", 10.0, 40.0, None, false);
    let dark = (0..200)
        .flat_map(|x| (0..60).map(move |y| (x, y)))
        .filter(|&(x, y)| pixel(&c, x, y)[3] > 128)
        .count();
    assert!(dark > 100, "text left {dark} opaque pixels");
    // Centered text straddles its anchor.
    let mut c2 = Canvas2d::new(200, 60);
    c2.state_mut().font = Font::parse("24px serif").unwrap();
    c2.state_mut().text_align = TextAlign::Center;
    c2.draw_text("Hello", 100.0, 40.0, None, false);
    let left = (0..100).any(|x| (0..60).any(|y| pixel(&c2, x, y)[3] > 0));
    let right = (100..200).any(|x| (0..60).any(|y| pixel(&c2, x, y)[3] > 0));
    assert!(left && right);
    assert!(Font::parse("garbage").is_none());
    assert_eq!(
        Font::parse("italic 12pt \"Fira Sans\", serif").unwrap().css,
        "italic 16px \"Fira Sans\", serif"
    );
}

#[test]
fn image_data_round_trips_and_images_draw() {
    let mut c = Canvas2d::new(10, 10);
    let mut data = vec![0u8; 4 * 4 * 4];
    for px in data.chunks_mut(4) {
        px.copy_from_slice(&[10, 20, 30, 255]);
    }
    c.put_image_data(&data, 4, 4, 2, 2);
    assert_eq!(pixel(&c, 3, 3), [10, 20, 30, 255]);
    assert_eq!(pixel(&c, 1, 1), [0, 0, 0, 0]);
    assert_eq!(c.image_data(2, 2, 4, 4), data);
    let mut target = Canvas2d::new(20, 20);
    target.draw_image(c.pixmap(), 2.0, 2.0, 4.0, 4.0, 0.0, 0.0, 8.0, 8.0);
    assert_eq!(pixel(&target, 4, 4), [10, 20, 30, 255]);
    assert_eq!(pixel(&target, 12, 12), [0, 0, 0, 0]);
    let png = target.to_png();
    assert_eq!(&png[1..4], b"PNG");
}
