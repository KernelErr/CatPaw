//! Canvas 2D from script.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/page").unwrap(),
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

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

#[test]
fn a_context_draws_and_reads_back_pixels() {
    let mut page = load(
        r#"<canvas id="c" width="40" height="30"></canvas><script>
      const c = document.getElementById('c');
      const ctx = c.getContext('2d');
      window.same = ctx === c.getContext('2d') && ctx.canvas === c && c.getContext('webgl') === null;
      ctx.fillStyle = 'rgb(255, 0, 0)';
      ctx.fillRect(0, 0, 20, 30);
      ctx.fillStyle = '#00ff00';
      ctx.beginPath();
      ctx.arc(30, 15, 8, 0, Math.PI * 2);
      ctx.fill();
      window.styles = ctx.fillStyle + ' ' + ctx.strokeStyle + ' ' + ctx.lineWidth + ' ' + ctx.font + ' ' + ctx.globalCompositeOperation;
      ctx.fillStyle = 'not a color';
      window.kept = ctx.fillStyle;
      const red = ctx.getImageData(5, 5, 1, 1).data;
      const green = ctx.getImageData(30, 15, 1, 1).data;
      const clear = ctx.getImageData(22, 2, 1, 1).data;
      window.pixels = [...red, ...green, ...clear].join(',');
      window.inside = ctx.isPointInPath(30, 15) + ' ' + ctx.isPointInPath(5, 5);
      window.url = c.toDataURL().slice(0, 22);
      window.dataLen = ctx.getImageData(0, 0, 40, 30).data.length;
    </script>"#,
    );
    assert_eq!(eval(&mut page, "same"), "true");
    assert_eq!(
        eval(&mut page, "styles"),
        "#00ff00 #000000 1 10px sans-serif source-over"
    );
    assert_eq!(eval(&mut page, "kept"), "#00ff00");
    assert_eq!(eval(&mut page, "pixels"), "255,0,0,255,0,255,0,255,0,0,0,0");
    assert_eq!(eval(&mut page, "inside"), "true false");
    assert_eq!(eval(&mut page, "url"), "data:image/png;base64,");
    assert_eq!(eval(&mut page, "dataLen"), "4800");
}

#[test]
fn image_data_paths_text_and_resizing() {
    let mut page = load(
        r#"<canvas id="c" width="100" height="50"></canvas><script>
      const c = document.getElementById('c');
      const ctx = c.getContext('2d');
      const img = ctx.createImageData(2, 2);
      img.data.fill(255);
      img.data[0] = 0;
      ctx.putImageData(img, 10, 10);
      window.put = ctx.getImageData(10, 10, 1, 1).data.join(',') + ' ' + img.width + 'x' + img.height + ' ' + (img.data === img.data);
      const p = new Path2D();
      p.rect(50, 0, 10, 10);
      ctx.fillStyle = 'blue';
      ctx.fill(p);
      window.path = ctx.getImageData(55, 5, 1, 1).data.join(',');
      ctx.font = 'bold 20px serif';
      const m = ctx.measureText('Hi');
      window.measured = ctx.font + ' ' + (m.width > 10) + ' ' + (m.fontBoundingBoxAscent > 5);
      ctx.fillStyle = 'black';
      ctx.fillText('Hi', 0, 45);
      let dark = 0;
      const all = ctx.getImageData(0, 25, 40, 25).data;
      for (let i = 3; i < all.length; i += 4) if (all[i] > 100) dark++;
      window.darkEnough = dark > 20;
      ctx.save();
      ctx.translate(90, 40);
      ctx.fillStyle = 'red';
      ctx.fillRect(0, 0, 5, 5);
      ctx.restore();
      window.translated = ctx.getImageData(92, 42, 1, 1).data.join(',');
      c.width = 10;
      window.afterResize = c.width + 'x' + c.height + ' ' + ctx.getImageData(0, 0, 1, 1).data.join(',') + ' ' + ctx.fillStyle;
      try { ctx.arc(0, 0, -1, 0, 1); } catch (e) { window.err = e.name; }
    </script>"#,
    );
    assert_eq!(eval(&mut page, "put"), "0,255,255,255 2x2 true");
    assert_eq!(eval(&mut page, "path"), "0,0,255,255");
    assert_eq!(eval(&mut page, "measured"), "bold 20px serif true true");
    assert_eq!(eval(&mut page, "darkEnough"), "true");
    assert_eq!(eval(&mut page, "translated"), "255,0,0,255");
    assert_eq!(eval(&mut page, "afterResize"), "10x50 0,0,0,0 #000000");
    assert_eq!(eval(&mut page, "err"), "IndexSizeError");
}
