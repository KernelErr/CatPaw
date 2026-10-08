//! A tree rebuilt from the last one lays out exactly as a tree built from
//! scratch, whatever changed in between.

use std::collections::HashMap;

use catpaw_dom::html::HtmlParseOptions;
use catpaw_dom::{Dom, LocalName, NodeId, NodeKind, QualName, ns, parse_html};
use catpaw_layout::{BuildInput, LayoutTree, Rect, Reused, Viewport};
use catpaw_style::{StyleEngine, StyleOptions};

const CSS: &str = r#"
    body { margin: 0; font-size: 16px }
    .page { display: grid; grid-template-columns: 200px 1fr; gap: 10px }
    .side li.closed > ul { display: none }
    .main p { margin: 4px 0 }
    .float { float: right; width: 120px; height: 60px }
    .tall { height: 150px }
    .row { display: flex; gap: 4px }
    .row > div { flex: 1 }
    .ib { display: inline-block; width: 30px; height: 12px }
    .abs { position: absolute; top: 5px; right: 5px; width: 50px }
    .rel { position: relative }
    .fixed { position: fixed; bottom: 0; left: 0; width: 100px; height: 20px }
    .hide { display: none }
    .wide { width: 300px }
    .big { font-size: 24px }
    h2::before { content: "§ " }
    .note::after { content: " (note)"; display: block }
"#;

const HTML: &str = r#"<!doctype html><body>
<div class=page id=page>
  <nav class=side id=side>
    <ul id=list>
      <li class=closed id=s1>One<ul><li>One.a</li><li>One.b</li></ul></li>
      <li id=s2>Two<ul><li>Two.a</li></ul></li>
      <li id=s3>Three</li>
    </ul>
  </nav>
  <main class=main id=main>
    <h2 id=h>Title</h2>
    <div class=float id=f>float</div>
    <p id=p1>Some text that wraps around the float <span class=ib></span> with an inline
      block and <b>bold</b> words, long enough to need a few lines.</p>
    <p id=p2 class=note>More text in a second paragraph that is long enough to wrap.</p>
    <div class=row id=row><div id=r1>a</div><div id=r2>b b b</div><div id=r3>c</div></div>
    <div class=rel id=rel>relative <span class=abs id=abs>abs</span></div>
    <p id=p3>Last paragraph.</p>
  </main>
</div>
<div class=fixed id=fixed>fixed</div>"#;

const VIEWPORT: Viewport = Viewport {
    width: 800.0,
    height: 600.0,
};

fn build(dom: &Dom, engine: &StyleEngine, previous: Option<LayoutTree>) -> LayoutTree {
    let fonts = catpaw_text::shared_fonts();
    let offsets = HashMap::new();
    let input = BuildInput {
        dom,
        styles: engine,
        fonts: &fonts,
        viewport: VIEWPORT,
        scroll_offsets: &offsets,
    };
    match previous {
        Some(previous) => LayoutTree::rebuild(previous, input),
        None => LayoutTree::build(input),
    }
}

/// Where everything is: the rectangles of every node, and what a grid of
/// points hits.
fn geometry(tree: &LayoutTree, dom: &Dom) -> Vec<String> {
    let mut out = Vec::new();
    for node in dom.descendants(dom.document()) {
        let rects: Vec<Rect> = tree.node_rects(dom, node);
        let name = match dom.kind(node) {
            NodeKind::Element(el) => format!("<{}>", el.name.local),
            NodeKind::Text(t) => format!("{t:?}"),
            _ => continue,
        };
        out.push(format!("{name} {rects:?}"));
    }
    for y in (0..600).step_by(23) {
        for x in (0..800).step_by(31) {
            let hit = tree.hit_test(dom, x as f32, y as f32, (0.0, 0.0));
            out.push(format!(
                "({x},{y}) {:?}",
                hit.map(|h| dom.local_name(h.element))
            ));
        }
    }
    out
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn name(local: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(local))
}

/// Changes the document at random, the way script would.
fn mutate(dom: &mut Dom, rng: &mut Lcg, keep: &[NodeId]) -> String {
    let body = dom
        .descendants(dom.document())
        .find(|&n| dom.is_html_element(n, "body"))
        .unwrap();
    let elements: Vec<NodeId> = dom
        .descendants(body)
        .filter(|&n| dom.is_element(n))
        .collect();
    let target = elements[rng.next(elements.len())];
    match rng.next(7) {
        0 | 1 => {
            let class = ["closed", "hide", "wide", "big", "tall", "float"][rng.next(6)];
            let data = dom.element_mut(target).unwrap();
            let mut classes: Vec<String> = data.classes().map(str::to_string).collect();
            match classes.iter().position(|c| c == class) {
                Some(i) => {
                    classes.remove(i);
                }
                None => classes.push(class.to_string()),
            }
            data.set_attr(name("class"), classes.join(" "));
            format!("class {class}")
        }
        2 => {
            let texts: Vec<NodeId> = dom
                .descendants(body)
                .filter(|&n| dom.node(n).is_text())
                .collect();
            let text = texts[rng.next(texts.len())];
            let NodeKind::Text(t) = &mut dom.node_mut(text).kind else {
                unreachable!()
            };
            if t.len() > 12 && rng.next(2) == 0 {
                t.truncate(t.len() / 2);
            } else {
                t.push_str(" and some more words");
            }
            "text".into()
        }
        3 => {
            let local = ["li", "p", "span", "div"][rng.next(4)];
            let new = dom.create_html_element(local, Vec::new());
            let text = dom.create_text(format!("new {local}"));
            dom.append_child(new, text);
            let children: Vec<NodeId> = dom.children(target).collect();
            let reference = rng.next(children.len() + 1);
            dom.insert_before(target, new, children.get(reference).copied());
            format!("insert {local}")
        }
        4 => {
            if !keep.contains(&target) {
                dom.detach(target);
            }
            "remove".into()
        }
        5 => {
            let value = [
                "width: 150px",
                "height: 40px",
                "margin-top: 12px",
                "padding: 3px",
                "",
            ][rng.next(5)];
            dom.element_mut(target)
                .unwrap()
                .set_attr(name("style"), value);
            format!("style {value}")
        }
        _ => {
            // A change that shows nowhere.
            dom.element_mut(target)
                .unwrap()
                .set_attr(name("data-x"), rng.next(9).to_string());
            "data".into()
        }
    }
}

#[test]
fn rebuilt_trees_lay_out_as_trees_built_from_scratch() {
    let mut total = Reused::default();
    for seed in 1..=10u64 {
        let mut dom = parse_html(HTML, &HtmlParseOptions::default()).dom;
        let mut engine = StyleEngine::new(&StyleOptions {
            viewport_width: VIEWPORT.width,
            viewport_height: VIEWPORT.height,
            ..StyleOptions::default()
        });
        engine.set_quirks_mode(dom.quirks_mode());
        engine.add_author_stylesheet(CSS);
        engine.restyle(&dom);
        let keep: Vec<NodeId> = ["page", "side", "main", "list"]
            .iter()
            .map(|id| {
                dom.descendants(dom.document())
                    .find(|&n| dom.attr(n, "id") == Some(*id))
                    .unwrap()
            })
            .collect();
        let mut tree = build(&dom, &engine, None);
        let mut rng = Lcg(seed);
        for step in 0..30 {
            let what = mutate(&mut dom, &mut rng, &keep);
            engine.restyle(&dom);
            tree = build(&dom, &engine, Some(tree));
            let fresh = build(&dom, &engine, None);
            let (got, expected) = (geometry(&tree, &dom), geometry(&fresh, &dom));
            for (g, e) in got.iter().zip(&expected) {
                assert_eq!(g, e, "seed {seed} step {step} ({what})");
            }
            assert_eq!(got.len(), expected.len());
            assert_eq!(tree.len(), fresh.len(), "seed {seed} step {step} ({what})");
            let reused = tree.reused();
            total.shaped += reused.shaped;
            total.reshaped += reused.reshaped;
            total.laid_out += reused.laid_out;
        }
    }
    // The comparison is only worth something if trees were reused.
    assert!(total.shaped > total.reshaped, "{total:?}");
    assert!(total.laid_out > 0, "{total:?}");
}

#[test]
fn an_unchanged_document_reuses_everything() {
    let dom = parse_html(HTML, &HtmlParseOptions::default()).dom;
    let mut engine = StyleEngine::new(&StyleOptions::default());
    engine.set_quirks_mode(dom.quirks_mode());
    engine.add_author_stylesheet(CSS);
    engine.restyle(&dom);
    let first = build(&dom, &engine, None);
    let boxes = first.len();
    let expected = geometry(&first, &dom);
    let second = build(&dom, &engine, Some(first));
    assert_eq!(geometry(&second, &dom), expected);
    let reused = second.reused();
    assert_eq!(reused.reshaped, 0, "{reused:?}");
    assert_eq!(
        reused.laid_out, boxes,
        "the root's layout was kept: {reused:?}"
    );
}
