//! Keyboard events as pages read them, the legacy fields included.

use catpaw_engine::{PageOptions, with_html};
use url::Url;

#[test]
fn key_events_carry_the_codes_pages_read() {
    let html = r#"<!doctype html><input id=target>
<script>
const seen = [];
for (const type of ['keydown', 'keypress', 'keyup']) {
  target.addEventListener(type, e => seen.push(type + ':' + e.key + ':' + e.keyCode + ':' + e.which));
}
</script>"#;
    let url = Url::parse("https://keys.example/").unwrap();
    with_html(url, html.to_string(), PageOptions::default(), |page| {
        page.eval("target.focus()").unwrap();
        page.press("Tab").unwrap();
        // Tab took focus on to the next element.
        page.eval("target.focus()").unwrap();
        page.press("a").unwrap();
        assert_eq!(
            page.eval("seen.join(' ')").unwrap(),
            "keydown:Tab:9:9 keyup:Tab:9:9 keydown:a:65:65 keypress:a:97:97 keyup:a:65:65"
        );
    })
    .unwrap();
}
