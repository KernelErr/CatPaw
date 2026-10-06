//! `TreeWalker` and `NodeIterator`.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

// root: a(a1, #text, a2(a21)), <!-- c -->, b(b1), #text
const FIXTURE: &str = r#"<!doctype html><body><div id="root"><p id="a"><i id="a1"></i>text<b id="a2"><u id="a21"></u></b></p><!-- c --><p id="b"><i id="b1"></i></p>tail</div><span id="after"></span>
<script>
  var root = document.getElementById('root');
  function $(id) { return document.getElementById(id); }
  function label(n) { return n ? (n.id ? n.id : n.nodeName) : 'null'; }
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  // Every node a traverser reaches with `step`, in order.
  function all(traverser, step) { var out = [], n; while ((n = traverser[step]())) out.push(label(n)); return out.join(); }
  var skipA2 = { acceptNode: function (n) { return n.id === 'a2' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; } };
  function rejectA(n) { return n.id === 'a' ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT; }
</script>"#;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    let report = page.with_cx(|cx| {
        scripting::load_document(cx, html);
        event_loop::run(cx, &LoopLimits::default())
    });
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
    page
}

fn check(page: &mut BoaPage, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        let actual = match page.eval_to_string(source) {
            Ok(text) => text,
            Err(e) => format!("THROWN {e}"),
        };
        assert_eq!(actual, *expected, "{source}");
    }
}

#[test]
fn node_filter_holds_the_constants() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "typeof NodeFilter + ' ' + NodeFilter.name",
                "function NodeFilter",
            ),
            (
                "[NodeFilter.FILTER_ACCEPT, NodeFilter.FILTER_REJECT, NodeFilter.FILTER_SKIP, NodeFilter.SHOW_ALL, NodeFilter.SHOW_ELEMENT, NodeFilter.SHOW_TEXT, NodeFilter.SHOW_COMMENT].join()",
                "1,2,3,4294967295,1,4,128",
            ),
            (
                "attempt(function () { return new NodeFilter(); })",
                "TypeError",
            ),
            ("NodeFilter.SHOW_ELEMENT = 5; NodeFilter.SHOW_ELEMENT", "1"),
        ],
    );
}

#[test]
fn tree_walkers_walk_what_they_are_shown() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var w = document.createTreeWalker(root); [w instanceof TreeWalker, w.root === root, w.currentNode === root, w.whatToShow, w.filter].join()",
                "true,true,true,4294967295,",
            ),
            (
                "all(w, 'nextNode')",
                "a,a1,#text,a2,a21,#comment,b,b1,#text",
            ),
            (
                "all(w, 'previousNode')",
                "b1,b,#comment,a21,a2,#text,a1,a,root",
            ),
            (
                "all(document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT), 'nextNode')",
                "a,a1,a2,a21,b,b1",
            ),
            (
                "all(document.createTreeWalker(root, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_COMMENT), 'nextNode')",
                "#text,#comment,#text",
            ),
            // A skipped node's children are still visited; a rejected node's are not.
            (
                "var s = document.createTreeWalker(root, 1, skipA2); s.filter === skipA2 && all(s, 'nextNode')",
                "a,a1,a21,b,b1",
            ),
            (
                "all(document.createTreeWalker(root, 1, rejectA), 'nextNode')",
                "b,b1",
            ),
            (
                "all(document.createTreeWalker(root, 1, function (n) { return n.id.length === 2; }), 'nextNode')",
                "a1,a2,b1",
            ),
            // Step by step.
            (
                "w = document.createTreeWalker(root, 1); [w.firstChild(), w.nextSibling(), w.nextSibling(), w.previousSibling(), w.lastChild(), w.parentNode(), w.parentNode(), w.parentNode()].map(label).join()",
                "a,b,null,a,a2,a,root,null",
            ),
            (
                "w.currentNode = $('a21'); [w.parentNode(), w.previousSibling(), w.firstChild()].map(label).join()",
                "a2,a1,null",
            ),
            // With a node skipped, its children take its place among the siblings.
            (
                "s.currentNode = $('a1'); label(s.nextSibling()) + ' ' + label(s.nextSibling()) + ' ' + label(s.parentNode())",
                "a21 null a",
            ),
            // The walker stays within its root.
            (
                "w.currentNode = root; label(w.nextSibling()) + ' ' + label(w.parentNode()) + ' ' + label(w.previousNode())",
                "null null null",
            ),
            ("w.currentNode = $('b1'); label(w.nextNode())", "null"),
        ],
    );
}

#[test]
fn filters_may_throw_but_not_reenter() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var thrower = document.createTreeWalker(root, 1, function () { throw new RangeError('no'); });
                 attempt(function () { thrower.nextNode(); }) + ' ' + label(thrower.currentNode)",
                "RangeError root",
            ),
            // After a throw the walker can be used again.
            ("attempt(function () { thrower.firstChild(); })", "RangeError"),
            (
                "var again = document.createTreeWalker(root, 1, function () { return again.nextNode() ? 1 : 1; });
                 attempt(function () { again.nextNode(); })",
                "InvalidStateError",
            ),
            ("attempt(function () { return document.createTreeWalker(); })", "TypeError"),
            ("attempt(function () { return document.createTreeWalker(root, 1, 5); })", "TypeError"),
        ],
    );
}

#[test]
fn node_iterators_remember_where_they_are() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT);
                 [it instanceof NodeIterator, it.root === root, it.referenceNode === root, it.pointerBeforeReferenceNode, it.whatToShow, it.filter].join()",
                "true,true,true,true,1,",
            ),
            // The root itself comes first.
            ("all(it, 'nextNode')", "root,a,a1,a2,a21,b,b1"),
            ("label(it.referenceNode) + ' ' + it.pointerBeforeReferenceNode", "b1 false"),
            ("all(it, 'previousNode')", "b1,b,a21,a2,a1,a,root"),
            (
                "label(it.referenceNode) + ' ' + it.pointerBeforeReferenceNode + ' ' + label(it.previousNode())",
                "root true null",
            ),
            // Going back and forth returns the same node twice.
            ("[it.nextNode(), it.nextNode(), it.previousNode(), it.nextNode()].map(label).join()", "root,a,a,a"),
            // A rejected node's children are still visited: an iterator has no tree to prune.
            ("all(document.createNodeIterator(root, 1, rejectA), 'nextNode')", "root,a1,a2,a21,b,b1"),
            ("it.detach(); all(document.createNodeIterator(root, NodeFilter.SHOW_COMMENT, skipA2), 'nextNode')", "#comment"),
        ],
    );
}

#[test]
fn node_iterators_step_aside_when_their_node_is_removed() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            // Removing what the iterator just returned leaves it after what came before.
            ("var it = document.createNodeIterator(root, 1); it.nextNode(); it.nextNode(); label(it.referenceNode)", "a"),
            ("$('a').remove(); label(it.referenceNode) + ' ' + it.pointerBeforeReferenceNode", "root false"),
            ("all(it, 'nextNode')", "b,b1"),
            // Removing as one goes, the way sanitizers do.
            (
                "root.innerHTML = '<i id=\"x1\"></i><i id=\"x2\"><u id=\"x21\"></u></i><i id=\"x3\"></i>';
                 var seen = [], n, clean = document.createNodeIterator(root, 1);
                 while ((n = clean.nextNode())) { seen.push(label(n)); if (n.id === 'x2') n.remove(); }
                 seen.join() + ' ' + root.children.length",
                "root,x1,x2,x3 2",
            ),
            // Standing before a node that goes, the iterator moves on to the next.
            (
                "var back = document.createNodeIterator(root, 1); back.nextNode(); back.nextNode(); back.previousNode();
                 label(back.referenceNode) + ' ' + back.pointerBeforeReferenceNode",
                "x1 true",
            ),
            (
                "$('x1').remove(); label(back.referenceNode) + ' ' + back.pointerBeforeReferenceNode + ' ' + label(back.nextNode())",
                "x3 true x3",
            ),
        ],
    );
}
