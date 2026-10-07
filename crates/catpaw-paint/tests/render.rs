//! Pixels of small documents.

use std::collections::HashMap;

use catpaw_dom::html::HtmlParseOptions;
use catpaw_dom::parse_html;
use catpaw_layout::{BuildInput, LayoutTree, Viewport};
use catpaw_paint::{Options, render};
use catpaw_style::{StyleEngine, StyleOptions};

fn paint(html: &str, width: u32, height: u32) -> catpaw_paint::tiny_skia::Pixmap {
    let result = parse_html(html, &HtmlParseOptions::default());
    let mut engine = StyleEngine::new(&StyleOptions {
        viewport_width: width as f32,
        viewport_height: height as f32,
        ..StyleOptions::default()
    });
    engine.set_quirks_mode(result.dom.quirks_mode());
    engine.restyle(&result.dom);
    let fonts = catpaw_text::shared_fonts();
    let tree = LayoutTree::build(BuildInput {
        dom: &result.dom,
        styles: &engine,
        fonts: &fonts,
        viewport: Viewport {
            width: width as f32,
            height: height as f32,
        },
        scroll_offsets: &HashMap::new(),
    });
    render(
        &tree,
        &result.dom,
        &Options {
            width,
            height,
            scroll: (0.0, 0.0),
            scale: 1.0,
        },
    )
}

fn pixel(pixmap: &catpaw_paint::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * pixmap.width() + x) * 4) as usize;
    let d = pixmap.data();
    [d[i], d[i + 1], d[i + 2], d[i + 3]]
}

#[test]
fn backgrounds_borders_and_the_canvas() {
    let pixmap = paint(
        r#"<!doctype html><body style="margin:0;background:#ff0000"><div style="margin:10px;width:100px;height:50px;background:rgb(0,0,255);border:5px solid #00ff00"></div>"#,
        200,
        100,
    );
    assert_eq!(
        pixel(&pixmap, 5, 5),
        [255, 0, 0, 255],
        "canvas takes the body background"
    );
    assert_eq!(pixel(&pixmap, 12, 12), [0, 255, 0, 255], "border");
    assert_eq!(pixel(&pixmap, 60, 35), [0, 0, 255, 255], "box background");
    assert_eq!(pixel(&pixmap, 150, 90), [255, 0, 0, 255], "outside the box");
}

#[test]
fn text_is_drawn_in_its_colour() {
    let pixmap = paint(
        r#"<!doctype html><body style="margin:0"><p style="margin:0;font:32px sans-serif;color:#000080">MMMMMMMM</p>"#,
        300,
        60,
    );
    // Somewhere in the first 150 x 30 px there are dark blue pixels, and
    // the lower-right corner stays white.
    let mut dark = 0;
    for y in 0..30 {
        for x in 0..150 {
            let [r, g, b, _] = pixel(&pixmap, x, y);
            if r < 60 && g < 60 && b > 60 {
                dark += 1;
            }
        }
    }
    assert!(dark > 100, "{dark} text pixels");
    assert_eq!(pixel(&pixmap, 290, 55), [255, 255, 255, 255]);
}

#[test]
fn overflow_hidden_clips_descendants() {
    let pixmap = paint(
        r#"<!doctype html><body style="margin:0"><div style="width:50px;height:50px;overflow:hidden"><div style="width:200px;height:200px;background:#000"></div></div>"#,
        100,
        100,
    );
    assert_eq!(pixel(&pixmap, 25, 25), [0, 0, 0, 255]);
    assert_eq!(
        pixel(&pixmap, 75, 25),
        [255, 255, 255, 255],
        "clipped to the right"
    );
    assert_eq!(
        pixel(&pixmap, 25, 75),
        [255, 255, 255, 255],
        "clipped below"
    );
}

#[test]
fn hidden_boxes_are_not_drawn() {
    let pixmap = paint(
        r#"<!doctype html><body style="margin:0"><div style="width:50px;height:50px;background:#000;visibility:hidden"></div><div style="width:50px;height:50px;background:#000;display:none"></div>"#,
        100,
        100,
    );
    assert_eq!(pixel(&pixmap, 25, 25), [255, 255, 255, 255]);
    assert_eq!(pixel(&pixmap, 25, 75), [255, 255, 255, 255]);
}
