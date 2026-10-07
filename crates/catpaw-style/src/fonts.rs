//! Font metrics for the style engine: `ex`, `ch`, `cap` and `ic` units,
//! `line-height: normal` and the like are resolved against the fonts
//! `catpaw-text` bundles.

use std::sync::{Arc, Mutex};

use catpaw_text::Fonts;
use catpaw_text::fontique::{
    Attributes, FontStyle, FontWeight, FontWidth, GenericFamily, QueryFamily,
};
use style::device::servo::FontMetricsProvider;
use style::font_metrics::FontMetrics;
use style::properties::style_structs::Font;
use style::values::computed::font::{GenericFontFamily, QueryFontMetricsFlags, SingleFontFamily};
use style::values::computed::{CSSPixelLength, Length};

/// Answers Stylo's font metrics queries from the shared font context.
pub(crate) struct CatFontMetricsProvider {
    fonts: Arc<Mutex<Fonts>>,
}

impl CatFontMetricsProvider {
    pub(crate) fn new() -> Self {
        Self {
            fonts: catpaw_text::shared_fonts(),
        }
    }
}

impl std::fmt::Debug for CatFontMetricsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CatFontMetricsProvider")
    }
}

/// The Parley generic family a CSS generic stands for.
pub fn generic_family(generic: GenericFontFamily) -> GenericFamily {
    match generic {
        GenericFontFamily::Serif => GenericFamily::Serif,
        GenericFontFamily::Monospace => GenericFamily::Monospace,
        GenericFontFamily::Cursive => GenericFamily::Cursive,
        GenericFontFamily::Fantasy => GenericFamily::Fantasy,
        GenericFontFamily::SystemUi => GenericFamily::SystemUi,
        GenericFontFamily::SansSerif | GenericFontFamily::None => GenericFamily::SansSerif,
    }
}

/// The families of a `font-family` list as Parley queries them.
pub fn query_families(font: &Font) -> Vec<QueryFamily<'_>> {
    font.font_family
        .families
        .list
        .iter()
        .map(|family| match family {
            SingleFontFamily::FamilyName(name) => {
                // Legacy names for the platform's UI font.
                let name: &str = &name.name;
                if matches!(name, "-apple-system" | "BlinkMacSystemFont") {
                    QueryFamily::Generic(GenericFamily::SystemUi)
                } else {
                    QueryFamily::Named(name)
                }
            }
            SingleFontFamily::Generic(generic) => QueryFamily::Generic(generic_family(*generic)),
        })
        .collect()
}

/// The weight, width and style of a `font` as Parley matches them.
pub fn query_attributes(font: &Font) -> Attributes {
    Attributes {
        width: FontWidth::from_percentage(font.font_width.0.to_float()),
        weight: FontWeight::new(font.font_weight.value()),
        style: match font.font_style {
            style::values::computed::font::FontStyle::NORMAL => FontStyle::Normal,
            style::values::computed::font::FontStyle::ITALIC => FontStyle::Italic,
            oblique => FontStyle::Oblique(Some(oblique.oblique_degrees())),
        },
    }
}

impl FontMetricsProvider for CatFontMetricsProvider {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        font: &Font,
        base_size: CSSPixelLength,
        _flags: QueryFontMetricsFlags,
    ) -> FontMetrics {
        let size = base_size.px();
        let metrics = {
            let mut fonts = self.fonts.lock().unwrap_or_else(|e| e.into_inner());
            fonts.metrics(
                query_families(font).into_iter(),
                query_attributes(font),
                size,
            )
        };
        let Some(metrics) = metrics else {
            return FontMetrics::default();
        };
        let length = |px: f32| Length::new(px);
        FontMetrics {
            x_height: metrics.x_height.map(length),
            zero_advance_measure: metrics.zero_advance.map(length),
            cap_height: metrics.cap_height.map(length),
            ic_width: metrics.ic_advance.map(length),
            ascent: length(metrics.ascent),
            script_percent_scale_down: None,
            script_script_percent_scale_down: None,
        }
    }

    fn base_size_for_generic(&self, generic: GenericFontFamily) -> Length {
        // Browsers size monospace text at 13px by default, everything else at 16px.
        Length::new(if generic == GenericFontFamily::Monospace {
            13.0
        } else {
            16.0
        })
    }
}
