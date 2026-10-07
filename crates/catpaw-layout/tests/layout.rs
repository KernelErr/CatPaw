//! Layout of small documents: where boxes, lines and fragments land.

use std::collections::HashMap;

use catpaw_dom::html::HtmlParseOptions;
use catpaw_dom::{Dom, NodeId, parse_html};
use catpaw_layout::{BuildInput, LayoutTree, Rect, Viewport};
use catpaw_style::{StyleEngine, StyleOptions};

struct Page {
    dom: Dom,
    tree: LayoutTree,
}

fn layout(html: &str, css: &str) -> Page {
    layout_in(html, css, 800.0, 600.0)
}

fn layout_in(html: &str, css: &str, width: f32, height: f32) -> Page {
    let result = parse_html(html, &HtmlParseOptions::default());
    let mut engine = StyleEngine::new(&StyleOptions {
        viewport_width: width,
        viewport_height: height,
        ..StyleOptions::default()
    });
    engine.set_quirks_mode(result.dom.quirks_mode());
    if !css.is_empty() {
        engine.add_author_stylesheet(css);
    }
    engine.restyle(&result.dom);
    let fonts = catpaw_text::shared_fonts();
    let tree = LayoutTree::build(BuildInput {
        dom: &result.dom,
        styles: &engine,
        fonts: &fonts,
        viewport: Viewport { width, height },
        scroll_offsets: &HashMap::new(),
    });
    Page {
        dom: result.dom,
        tree,
    }
}

impl Page {
    fn find(&self, id: &str) -> NodeId {
        self.dom
            .descendants(self.dom.document())
            .find(|n| self.dom.attr(*n, "id") == Some(id))
            .unwrap_or_else(|| panic!("no element with id {id}"))
    }

    fn rect(&self, id: &str) -> Rect {
        let node = self.find(id);
        self.tree
            .bounding_rect(&self.dom, node)
            .unwrap_or_else(|| panic!("{id} has no rectangle"))
    }

    fn rects(&self, id: &str) -> Vec<Rect> {
        let node = self.find(id);
        self.tree.node_rects(&self.dom, node)
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.51
}

macro_rules! assert_rect {
    ($rect:expr, $x:expr, $y:expr, $w:expr, $h:expr) => {{
        let r = $rect;
        assert!(
            close(r.x, $x) && close(r.y, $y) && close(r.width, $w) && close(r.height, $h),
            "expected ({}, {}, {}, {}), got {:?}",
            $x,
            $y,
            $w,
            $h,
            r
        );
    }};
}

#[test]
fn blocks_stack_below_the_body_margin() {
    let page = layout(
        r#"<div id=a style="height:50px"></div><div id=b style="height:30px;margin-top:10px"></div>"#,
        "",
    );
    assert_rect!(page.rect("a"), 8.0, 8.0, 784.0, 50.0);
    assert_rect!(page.rect("b"), 8.0, 68.0, 784.0, 30.0);
}

#[test]
fn text_wraps_to_the_width_it_gets() {
    let page = layout(
        r#"<div id=t style="width:200px;font:16px sans-serif">The quick brown fox jumps over the lazy dog and keeps running through the field</div>"#,
        "",
    );
    let rect = page.rect("t");
    assert!(close(rect.width, 200.0), "{rect:?}");
    // DejaVu Sans at 16px needs more than one line for that sentence.
    assert!(rect.height > 36.0 && rect.height < 120.0, "{rect:?}");
    let one_line = layout(r#"<div id=t style="font:16px sans-serif">short</div>"#, "").rect("t");
    assert!(
        one_line.height > 16.0 && one_line.height < 22.0,
        "{one_line:?}"
    );
    assert!(
        rect.height > 2.0 * one_line.height,
        "{rect:?} vs {one_line:?}"
    );
}

#[test]
fn inline_blocks_sit_in_the_line() {
    let page = layout(
        r#"<div id=p style="font:16px sans-serif"><span id=s>ab</span><span id=ib style="display:inline-block;width:40px;height:20px"></span>cd</div>"#,
        "",
    );
    let s = page.rect("s");
    let ib = page.rect("ib");
    assert!(s.x >= 8.0 && s.width > 5.0, "{s:?}");
    assert!(close(ib.x, s.right()), "{s:?} then {ib:?}");
    assert!(close(ib.width, 40.0) && close(ib.height, 20.0), "{ib:?}");
    assert!(page.rect("p").height >= 20.0);
}

#[test]
fn absolute_boxes_use_their_containing_block() {
    let page = layout(
        r#"<div id=cb style="position:relative;margin:20px;height:100px"><div><div id=abs style="position:absolute;left:10px;top:5px;width:30px;height:30px"></div></div></div>
        <div id=fixed style="position:fixed;right:0;bottom:0;width:20px;height:10px"></div>"#,
        "",
    );
    // The body's margin collapses with the container's.
    assert_rect!(page.rect("abs"), 38.0, 25.0, 30.0, 30.0);
    let fixed = page.rect("fixed");
    assert_rect!(fixed, 780.0, 590.0, 20.0, 10.0);
    assert!(
        page.tree
            .is_fixed(page.tree.box_of(page.find("fixed")).unwrap())
    );
}

#[test]
fn flex_rows_place_items_side_by_side() {
    let page = layout(
        r#"<div id=f style="display:flex;width:300px"><div id=a style="flex:1;height:10px"></div><div id=b style="flex:2;height:10px"></div></div>"#,
        "",
    );
    assert_rect!(page.rect("a"), 8.0, 8.0, 100.0, 10.0);
    assert_rect!(page.rect("b"), 108.0, 8.0, 200.0, 10.0);
}

#[test]
fn grid_places_items_in_tracks() {
    let page = layout(
        r#"<div style="display:grid;grid-template-columns:100px 1fr;width:400px"><div id=a style="height:10px"></div><div id=b style="height:10px"></div></div>"#,
        "",
    );
    assert_rect!(page.rect("a"), 8.0, 8.0, 100.0, 10.0);
    assert_rect!(page.rect("b"), 108.0, 8.0, 300.0, 10.0);
}

#[test]
fn inline_elements_have_one_rectangle_per_line() {
    let page = layout(
        r#"<div style="width:120px;font:16px sans-serif">aaa <span id=s>bbbb bbbb bbbb bbbb bbbb bbbb</span> ccc</div>"#,
        "",
    );
    let rects = page.rects("s");
    assert!(rects.len() >= 2, "{rects:?}");
    for pair in rects.windows(2) {
        assert!(pair[1].y > pair[0].y, "{rects:?}");
    }
    let bounding = page.rect("s");
    assert!(bounding.height > rects[0].height, "{bounding:?}");
}

#[test]
fn scroll_containers_report_their_content_extent() {
    let page = layout(
        r#"<div id=sc style="overflow:auto;width:100px;height:50px;border:2px solid"><div style="height:300px;width:150px"></div></div>"#,
        "",
    );
    let id = page.tree.box_of(page.find("sc")).unwrap();
    let metrics = page.tree.scroll_metrics(id);
    assert_rect!(metrics.client, 10.0, 10.0, 100.0, 50.0);
    assert!(close(metrics.scroll_width, 150.0), "{metrics:?}");
    assert!(close(metrics.scroll_height, 300.0), "{metrics:?}");
    assert!(page.tree.is_scroll_container(id));
}

#[test]
fn hit_testing_finds_the_element_under_a_point() {
    let page = layout(
        r#"<div id=a style="height:50px"><span id=s style="font:16px sans-serif">hello</span></div><div id=b style="height:50px"></div>"#,
        "",
    );
    let hit = |x: f32, y: f32| {
        page.tree
            .hit_test(&page.dom, x, y, (0.0, 0.0))
            .map(|h| h.element)
    };
    assert_eq!(hit(400.0, 80.0), Some(page.find("b")));
    let s = page.rect("s");
    assert_eq!(hit(s.x + 2.0, s.y + s.height / 2.0), Some(page.find("s")));
    assert_eq!(hit(700.0, 30.0), Some(page.find("a")));
    let html = page.dom.child_elements(page.dom.document()).next().unwrap();
    assert_eq!(hit(400.0, 500.0), Some(html));
}

#[test]
fn hidden_and_contents_elements_generate_no_boxes() {
    let page = layout(
        r#"<div id=gone style="display:none"><div id=inside></div></div><div id=c style="display:contents"><div id=child style="height:10px"></div></div>"#,
        "",
    );
    assert!(page.rects("gone").is_empty());
    assert!(page.rects("inside").is_empty());
    assert!(page.tree.box_of(page.find("c")).is_none());
    assert_rect!(page.rect("child"), 8.0, 8.0, 784.0, 10.0);
}

#[test]
fn replaced_elements_take_their_attributes() {
    let page = layout(
        r#"<img id=i width=100 height=50><br><input id=in><br><canvas id=cv></canvas>"#,
        "",
    );
    let img = page.rect("i");
    assert!(
        close(img.width, 100.0) && close(img.height, 50.0),
        "{img:?}"
    );
    let input = page.rect("in");
    assert!(input.width > 50.0 && input.height > 10.0, "{input:?}");
    let canvas = page.rect("cv");
    assert!(
        close(canvas.width, 300.0) && close(canvas.height, 150.0),
        "{canvas:?}"
    );
}

#[test]
fn pseudo_element_text_takes_part_in_lines() {
    let page = layout(
        r#"<div id=p style="font:16px sans-serif"><span id=s>x</span></div>"#,
        "#s::before { content: 'before '; } #s::after { content: ' after'; }",
    );
    let with = page.rect("s").width;
    let without = layout(
        r#"<div id=p style="font:16px sans-serif"><span id=s>x</span></div>"#,
        "",
    )
    .rect("s")
    .width;
    assert!(with > without + 20.0, "{with} vs {without}");
}

#[test]
fn percent_heights_resolve_against_the_viewport() {
    let page = layout_in(
        r#"<div id=h style="height:50%"></div>"#,
        "html, body { margin: 0; height: 100%; }",
        1000.0,
        400.0,
    );
    assert_rect!(page.rect("h"), 0.0, 0.0, 1000.0, 200.0);
}

#[test]
fn floats_are_placed_at_the_edges() {
    let page = layout(
        r#"<div style="width:300px"><div id=l style="float:left;width:50px;height:20px"></div><div id=r style="float:right;width:50px;height:20px"></div></div>"#,
        "",
    );
    assert_rect!(page.rect("l"), 8.0, 8.0, 50.0, 20.0);
    assert_rect!(page.rect("r"), 258.0, 8.0, 50.0, 20.0);
}

#[test]
fn offset_parent_is_the_nearest_positioned_ancestor() {
    let page = layout(
        r#"<div id=outer style="position:relative"><div><span id=s>text</span></div></div><div id=plain></div>"#,
        "",
    );
    let body = page
        .dom
        .descendants(page.dom.document())
        .find(|n| page.dom.is_html_element(*n, "body"))
        .unwrap();
    assert_eq!(
        page.tree.offset_parent(&page.dom, page.find("s")),
        Some(page.find("outer"))
    );
    assert_eq!(
        page.tree.offset_parent(&page.dom, page.find("plain")),
        Some(body)
    );
    assert_eq!(page.tree.offset_parent(&page.dom, body), None);
}

#[test]
fn white_space_collapses_across_inline_boundaries() {
    let width = |html: &str| layout(html, "").rect("p").width;
    let plain =
        width(r#"<p id=p style="display:inline-block;font:16px sans-serif">one two three</p>"#);
    let spans = width(
        r#"<p id=p style="display:inline-block;font:16px sans-serif">one <span>two</span> three</p>"#,
    );
    let inner = width(
        r#"<p id=p style="display:inline-block;font:16px sans-serif">one<span> two </span>three</p>"#,
    );
    let newlines = width(
        "<p id=p style=\"display:inline-block;font:16px sans-serif\">\n  one\n  two   three\n</p>",
    );
    assert!(close(spans, plain), "{spans} vs {plain}");
    assert!(close(inner, plain), "{inner} vs {plain}");
    assert!(close(newlines, plain), "{newlines} vs {plain}");
    let no_space = width(
        r#"<p id=p style="display:inline-block;font:16px sans-serif">one<span>two</span>three</p>"#,
    );
    assert!(no_space < plain - 5.0, "{no_space} vs {plain}");
    let pre = width(
        "<p id=p style=\"display:inline-block;font:16px sans-serif;white-space:pre\">one  two</p>",
    );
    let one_space = width(
        "<p id=p style=\"display:inline-block;font:16px sans-serif;white-space:pre\">one two</p>",
    );
    assert!(pre > one_space + 2.0, "{pre} vs {one_space}");
}

#[test]
fn line_boxes_take_the_tallest_run() {
    let page = layout(
        r#"<p id=p style="margin:0;font:16px sans-serif;line-height:1.6">one two<sup style="font-size:smaller;line-height:1">[1]</sup> three<br>second line</p>"#,
        "",
    );
    let rect = page.rect("p");
    assert!(close(rect.height, 2.0 * 25.6), "{rect:?}");
}

#[test]
fn atomic_boxes_inside_inline_elements_get_boxes() {
    let page = layout(
        r#"<p style="font:16px sans-serif"><label>pick <input id=r type=radio> me</label> <a href=#><img id=i width=20 height=10></a></p>"#,
        "",
    );
    let r = page.rect("r");
    assert!(close(r.width, 13.0) && close(r.height, 13.0), "{r:?}");
    let i = page.rect("i");
    assert!(
        close(i.width, 20.0) && close(i.height, 10.0) && i.x > r.right(),
        "{i:?} after {r:?}"
    );
}

#[test]
fn lines_flow_around_floats() {
    let page = layout(
        r#"<div style="width:400px;font:16px sans-serif"><div id=f style="float:left;width:100px;height:20px"></div><p id=p style="margin:0">one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen</p></div>"#,
        "",
    );
    let f = page.rect("f");
    let p = page.rect("p");
    let first_line = page.rects("p");
    assert_rect!(f, 8.0, 8.0, 100.0, 20.0);
    // The paragraph's box starts at the top beside the float, its text
    // starts after the float.
    assert!(close(p.x, 8.0) && close(p.y, 8.0), "{p:?}");
    let text_node = {
        let p = page.find("p");
        page.dom.children(p).next().unwrap()
    };
    let lines = page.tree.node_rects(&page.dom, text_node);
    assert!(lines.len() >= 3, "{lines:?}");
    assert!(
        close(lines[0].x, 108.0),
        "first line beside the float: {:?}",
        lines[0]
    );
    assert!(lines[0].width <= 300.5, "{:?}", lines[0]);
    let below: Vec<_> = lines.iter().filter(|l| l.y >= 28.0).collect();
    assert!(
        !below.is_empty() && below.iter().all(|l| close(l.x, 8.0)),
        "{lines:?}"
    );
    let _ = first_line;
}

#[test]
fn content_drops_below_floats_that_fill_the_width() {
    let page = layout(
        r#"<div style="width:400px"><div id=a style="float:left;width:50%;height:30px"></div><div id=b style="float:left;width:50%;height:30px"></div><button id=go style="display:inline-block;width:60px;height:20px"></button></div>"#,
        "",
    );
    assert_rect!(page.rect("a"), 8.0, 8.0, 200.0, 30.0);
    assert_rect!(page.rect("b"), 208.0, 8.0, 200.0, 30.0);
    let go = page.rect("go");
    assert!(close(go.x, 8.0) && go.y >= 38.0, "{go:?}");
}

#[test]
fn a_clearfix_after_gives_a_row_its_floats_height() {
    let page = layout(
        r#"<div id=row><div id=col style="float:left;width:50px;height:30px"></div></div><div id=next style="height:10px"></div>"#,
        "#row::after { content: \"\"; display: table; clear: both; }",
    );
    assert_rect!(page.rect("row"), 8.0, 8.0, 784.0, 30.0);
    assert_rect!(page.rect("next"), 8.0, 38.0, 784.0, 10.0);
    let plain = layout(
        r#"<div id=row><div style="float:left;width:50px;height:30px"></div></div><div id=next style="height:10px"></div>"#,
        "",
    );
    assert_rect!(plain.rect("row"), 8.0, 8.0, 784.0, 0.0);
    assert_rect!(plain.rect("next"), 8.0, 8.0, 784.0, 10.0);
}
