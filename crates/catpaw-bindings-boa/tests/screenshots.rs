//! Screenshots show what form controls hold: values typed or set from
//! script, checked boxes, the chosen option, and where focus is.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    page
}

struct Shot {
    width: usize,
    rgba: Vec<u8>,
}

fn shot(page: &BoaPage) -> Shot {
    let png = catpaw_web::screenshot(page.page(), false);
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("a PNG");
    let mut rgba = vec![0; reader.output_buffer_size().expect("a size")];
    let info = reader.next_frame(&mut rgba).expect("a frame");
    assert_eq!(info.color_type, png::ColorType::Rgba);
    Shot {
        width: info.width as usize,
        rgba,
    }
}

impl Shot {
    fn count(&self, (x, y, w, h): (u32, u32, u32, u32), test: impl Fn([u8; 3]) -> bool) -> usize {
        let mut n = 0;
        for row in y..y + h {
            for col in x..x + w {
                let i = (row as usize * self.width + col as usize) * 4;
                if test([self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]]) {
                    n += 1;
                }
            }
        }
        n
    }

    /// Dark pixels: text.
    fn ink(&self, rect: (u32, u32, u32, u32)) -> usize {
        self.count(rect, |[r, g, b]| r < 100 && g < 100 && b < 100)
    }

    fn blue(&self, rect: (u32, u32, u32, u32)) -> usize {
        self.count(rect, |[r, _, b]| b > 150 && r < 100)
    }
}

/// The content box of `#id` (inside border and padding), in whole pixels.
fn content_box(page: &mut BoaPage, id: &str) -> (u32, u32, u32, u32) {
    let text = page
        .eval_to_string(&format!(
            "var e = document.getElementById('{id}'), r = e.getBoundingClientRect(), s = getComputedStyle(e); var l = parseFloat(s.borderLeftWidth) + parseFloat(s.paddingLeft), t = parseFloat(s.borderTopWidth) + parseFloat(s.paddingTop), rr = parseFloat(s.borderRightWidth) + parseFloat(s.paddingRight), b = parseFloat(s.borderBottomWidth) + parseFloat(s.paddingBottom); [Math.ceil(r.x + l), Math.ceil(r.y + t), Math.floor(r.width - l - rr), Math.floor(r.height - t - b)].join(',')"
        ))
        .expect("a box");
    let v: Vec<u32> = text
        .split(',')
        .map(|n| n.parse().expect("a number"))
        .collect();
    (v[0], v[1], v[2], v[3])
}

#[test]
fn screenshots_show_what_controls_hold() {
    let mut page = load(
        r#"<!doctype html><body style="margin:10px"><input id="name"> <input type="password" id="secret"> <input type="checkbox" id="agree"> <select id="pick"><option>One</option><option>Second choice</option></select> <textarea id="note"></textarea> <input type="text" id="hint" placeholder="Search here">"#,
    );
    let name = content_box(&mut page, "name");
    let secret = content_box(&mut page, "secret");
    let agree = content_box(&mut page, "agree");
    let hint = content_box(&mut page, "hint");
    let note = content_box(&mut page, "note");
    let before = shot(&page);
    assert_eq!(before.ink(name), 0, "an empty field");
    assert_eq!(before.blue(agree), 0, "an unchecked box");
    assert!(
        before.count(hint, |[r, g, b]| r < 200 && r == g && g == b) > 20,
        "a grey placeholder"
    );

    page.eval(
        "document.getElementById('name').value = 'Grace Hopper'; document.getElementById('secret').value = 'hunter2'; document.getElementById('agree').checked = true; document.getElementById('pick').selectedIndex = 1; document.getElementById('note').value = 'two\\nlines'; document.getElementById('name').focus();",
    )
    .expect("script");
    let after = shot(&page);
    assert!(after.ink(name) > 60, "the value shows");
    assert!(after.ink(secret) > 20, "bullets show for a password");
    assert!(after.blue(agree) > 30, "the box is checked");
    assert!(after.ink(note) > 40, "the textarea's text shows");
    // A ring around the focused field, just outside its border box.
    let (x, y, w, _) = name;
    let ring = (x, y.saturating_sub(8), w, 4);
    assert!(after.blue(ring) > 20, "a focus ring");
    assert_eq!(before.blue(ring), 0);
    // The select shows the option chosen now.
    let pick = content_box(&mut page, "pick");
    assert!(after.ink(pick) > before.ink(pick), "the longer label shows");
}
