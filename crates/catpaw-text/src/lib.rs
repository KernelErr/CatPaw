//! Fonts and text shaping for CatPaw: a Parley font context over a bundled,
//! deterministic font set, and the metrics the style and layout engines
//! ask of it.
//!
//! The bundled fonts are DejaVu Sans, Serif and Sans Mono (see
//! `fonts/LICENSE`); they stand in for the generic families so that a page
//! lays out the same on every machine. The `system-fonts` feature adds the
//! fonts installed on the machine behind them, for scripts the bundled set
//! does not cover.

use std::sync::{Arc, Mutex, OnceLock};

pub use fontique;
pub use parley;
pub use skrifa;

use fontique::{Blob, Collection, CollectionOptions, FamilyId, GenericFamily, SourceCache};
use parley::{FontContext, LayoutContext};
use skrifa::MetadataProvider as _;

/// What a laid-out run of text belongs to: the arena key of the DOM node
/// (as `KeyData::as_ffi`) whose style the run was shaped with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Brush {
    pub node: u64,
}

/// A bundled font file.
struct Bundled {
    data: &'static [u8],
    family: GenericFamily,
}

const BUNDLED: &[Bundled] = &[
    Bundled {
        data: include_bytes!("../fonts/DejaVuSans.ttf"),
        family: GenericFamily::SansSerif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSans-Bold.ttf"),
        family: GenericFamily::SansSerif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSans-Oblique.ttf"),
        family: GenericFamily::SansSerif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSans-BoldOblique.ttf"),
        family: GenericFamily::SansSerif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSerif.ttf"),
        family: GenericFamily::Serif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSerif-Bold.ttf"),
        family: GenericFamily::Serif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSerif-Italic.ttf"),
        family: GenericFamily::Serif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSerif-BoldItalic.ttf"),
        family: GenericFamily::Serif,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSansMono.ttf"),
        family: GenericFamily::Monospace,
    },
    Bundled {
        data: include_bytes!("../fonts/DejaVuSansMono-Bold.ttf"),
        family: GenericFamily::Monospace,
    },
];

/// The font collection and the shaping context layouts are built with.
pub struct Fonts {
    pub font_cx: FontContext,
    pub layout_cx: LayoutContext<Brush>,
}

impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

impl Fonts {
    /// A context over the bundled fonts (and, with the `system-fonts`
    /// feature, the machine's fonts after them).
    pub fn new() -> Self {
        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: cfg!(feature = "system-fonts"),
        });
        let mut sans = Vec::new();
        let mut serif = Vec::new();
        let mut mono = Vec::new();
        for font in BUNDLED {
            let blob = Blob::new(Arc::new(font.data));
            for (family, _) in collection.register_fonts(blob, None) {
                let list = match font.family {
                    GenericFamily::Serif => &mut serif,
                    GenericFamily::Monospace => &mut mono,
                    _ => &mut sans,
                };
                if !list.contains(&family) {
                    list.push(family);
                }
            }
        }
        let generic = |collection: &mut Collection, generic: GenericFamily, ids: &[FamilyId]| {
            // The bundled family comes first; whatever the machine offers
            // for the generic stays behind it as a fallback.
            let existing: Vec<FamilyId> = collection.generic_families(generic).collect();
            let mut all = ids.to_vec();
            all.extend(existing.into_iter().filter(|id| !ids.contains(id)));
            collection.set_generic_families(generic, all.into_iter());
        };
        for family in [
            GenericFamily::SansSerif,
            GenericFamily::SystemUi,
            GenericFamily::UiSansSerif,
            GenericFamily::Cursive,
            GenericFamily::Fantasy,
            GenericFamily::Emoji,
            GenericFamily::Math,
            GenericFamily::FangSong,
            GenericFamily::UiRounded,
        ] {
            generic(&mut collection, family, &sans);
        }
        for family in [GenericFamily::Serif, GenericFamily::UiSerif] {
            generic(&mut collection, family, &serif);
        }
        for family in [GenericFamily::Monospace, GenericFamily::UiMonospace] {
            generic(&mut collection, family, &mono);
        }
        Self {
            font_cx: FontContext {
                collection,
                source_cache: SourceCache::default(),
            },
            layout_cx: LayoutContext::new(),
        }
    }

    /// Metrics of the first font that matches `families` and `attributes`,
    /// scaled to `size` CSS pixels. `None` when no font matches at all.
    pub fn metrics<'a>(
        &mut self,
        families: impl Iterator<Item = fontique::QueryFamily<'a>>,
        attributes: fontique::Attributes,
        size: f32,
    ) -> Option<FontMetrics> {
        let mut query = self
            .font_cx
            .collection
            .query(&mut self.font_cx.source_cache);
        query.set_families(families);
        query.set_attributes(attributes);
        let mut found = None;
        query.matches_with(|font| {
            found = Some(font.clone());
            fontique::QueryStatus::Stop
        });
        let font = found?;
        let font_ref = skrifa::FontRef::from_index(font.blob.as_ref(), font.index).ok()?;
        Some(FontMetrics::of(&font_ref, size))
    }
}

/// The metrics style and layout need of a font at one size, in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub x_height: Option<f32>,
    pub cap_height: Option<f32>,
    /// Advance of `0`, the `ch` unit.
    pub zero_advance: Option<f32>,
    /// Advance of `水` (U+6C34), the `ic` unit.
    pub ic_advance: Option<f32>,
}

impl FontMetrics {
    fn of(font: &skrifa::FontRef<'_>, size: f32) -> Self {
        use skrifa::instance::{LocationRef, Size};
        let location = LocationRef::default();
        let metrics = skrifa::metrics::Metrics::new(font, Size::new(size), location);
        let glyphs = skrifa::metrics::GlyphMetrics::new(font, Size::new(size), location);
        let charmap = font.charmap();
        let advance = |ch: char| charmap.map(ch).and_then(|id| glyphs.advance_width(id));
        // Fonts without the OS/2 fields (DejaVu among them) get the heights
        // measured from the glyphs browsers measure them from.
        let glyph_top = |ch: char| {
            let id = charmap.map(ch)?;
            let outline = font.outline_glyphs().get(id)?;
            let mut pen = TopPen {
                top: f32::NEG_INFINITY,
            };
            outline
                .draw(
                    skrifa::outline::DrawSettings::unhinted(Size::new(size), location),
                    &mut pen,
                )
                .ok()?;
            pen.top.is_finite().then_some(pen.top)
        };
        Self {
            ascent: metrics.ascent,
            descent: -metrics.descent,
            line_gap: metrics.leading,
            x_height: metrics.x_height.or_else(|| glyph_top('x')),
            cap_height: metrics.cap_height.or_else(|| glyph_top('H')),
            zero_advance: advance('0'),
            ic_advance: advance('\u{6C34}'),
        }
    }
}

/// Records the highest point an outline reaches.
struct TopPen {
    top: f32,
}

impl skrifa::outline::OutlinePen for TopPen {
    fn move_to(&mut self, _x: f32, y: f32) {
        self.top = self.top.max(y);
    }
    fn line_to(&mut self, _x: f32, y: f32) {
        self.top = self.top.max(y);
    }
    fn quad_to(&mut self, _cx0: f32, cy0: f32, _x: f32, y: f32) {
        self.top = self.top.max(cy0).max(y);
    }
    fn curve_to(&mut self, _cx0: f32, cy0: f32, _cx1: f32, cy1: f32, _x: f32, y: f32) {
        self.top = self.top.max(cy0).max(cy1).max(y);
    }
    fn close(&mut self) {}
}

/// The process-wide font context: building one parses every bundled font,
/// and the fonts never change.
pub fn shared_fonts() -> Arc<Mutex<Fonts>> {
    static FONTS: OnceLock<Arc<Mutex<Fonts>>> = OnceLock::new();
    FONTS
        .get_or_init(|| Arc::new(Mutex::new(Fonts::new())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_answer_generic_families() {
        let mut fonts = Fonts::new();
        for (generic, expected) in [
            (GenericFamily::SansSerif, "DejaVu Sans"),
            (GenericFamily::Serif, "DejaVu Serif"),
            (GenericFamily::Monospace, "DejaVu Sans Mono"),
        ] {
            let id = fonts
                .font_cx
                .collection
                .generic_families(generic)
                .next()
                .expect("a family for the generic");
            let name = fonts
                .font_cx
                .collection
                .family_name(id)
                .expect("the family has a name")
                .to_string();
            assert_eq!(name, expected);
        }
    }

    #[test]
    fn metrics_scale_with_size() {
        let mut fonts = Fonts::new();
        let at = |fonts: &mut Fonts, size: f32| {
            fonts
                .metrics(
                    std::iter::once(fontique::QueryFamily::Generic(GenericFamily::SansSerif)),
                    fontique::Attributes::default(),
                    size,
                )
                .expect("metrics")
        };
        let m16 = at(&mut fonts, 16.0);
        let m32 = at(&mut fonts, 32.0);
        assert!(m16.ascent > 10.0 && m16.ascent < 20.0, "{m16:?}");
        assert!((m32.ascent - 2.0 * m16.ascent).abs() < 0.01);
        assert!(m16.zero_advance.unwrap() > 5.0);
        assert!(m16.x_height.unwrap() > 5.0 && m16.x_height.unwrap() < m16.ascent);
    }
}
