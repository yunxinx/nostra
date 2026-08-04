//! Glyph outlines as SVG `<path>` with fallback fonts loaded only after a miss.

use ab_glyph::{Font, FontRef, GlyphId, OutlineCurve};
use ratex_font::FontId;
use ratex_font_loader::FontSet;

#[derive(Debug)]
pub(crate) enum StandaloneGlyph {
    Path(String),
    Image {
        href: String,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
}

pub(crate) fn standalone_glyph(
    px: f32,
    py: f32,
    glyph_em: f32,
    font_name: &str,
    char_code: u32,
    fonts: &mut FontSet,
) -> Option<StandaloneGlyph> {
    let font_id = FontId::parse(font_name).unwrap_or(FontId::MainRegular);
    let ch = ratex_font::katex_ttf_glyph_char(font_id, char_code);
    let requested_glyph = font_glyph_id(fonts, font_id, ch)?;
    let (resolved_font_id, glyph_id) = if requested_glyph.0 != 0 {
        (font_id, requested_glyph)
    } else if font_id != FontId::MainRegular {
        match font_glyph_id(fonts, FontId::MainRegular, ch) {
            Some(main_glyph) if main_glyph.0 != 0 => (FontId::MainRegular, main_glyph),
            _ => {
                return try_system_unicode_fallback_svg(px, py, glyph_em, ch, fonts, true);
            }
        }
    } else {
        return try_system_unicode_fallback_svg(px, py, glyph_em, ch, fonts, false);
    };

    if resolved_font_id == FontId::EmojiFallback {
        return try_emoji_raster_or_vector_svg(px, py, glyph_em, ch, fonts, glyph_id);
    }

    if resolved_font_id == FontId::CjkRegular {
        if let Some(path) =
            outline_to_d_for_font(px, py, glyph_em, FontId::CjkRegular, fonts, glyph_id)
        {
            return Some(StandaloneGlyph::Path(path));
        }
        if let Some(glyph) = try_emoji_raster_then_vector_svg(px, py, glyph_em, ch, fonts) {
            return Some(glyph);
        }
        return try_outline_for_char(px, py, glyph_em, FontId::CjkFallback, ch, fonts);
    }

    if resolved_font_id == FontId::CjkFallback {
        if let Some(path) =
            outline_to_d_for_font(px, py, glyph_em, FontId::CjkFallback, fonts, glyph_id)
        {
            return Some(StandaloneGlyph::Path(path));
        }
        return try_emoji_raster_then_vector_svg(px, py, glyph_em, ch, fonts);
    }

    if let Some(path) =
        outline_to_d_for_font(px, py, glyph_em, resolved_font_id, fonts, glyph_id)
    {
        return Some(StandaloneGlyph::Path(path));
    }

    try_system_unicode_fallback_svg(
        px,
        py,
        glyph_em,
        ch,
        fonts,
        resolved_font_id == FontId::MainRegular,
    )
}

fn with_font<T>(
    fonts: &mut FontSet,
    font_id: FontId,
    callback: impl FnOnce(&FontRef<'_>, u64) -> Option<T>,
) -> Option<T> {
    let data = fonts.ensure(font_id).ok()??;
    let font = FontRef::try_from_slice_and_index(data.as_bytes(), data.face_index()).ok()?;
    callback(&font, data.identity())
}

fn font_glyph_id(fonts: &mut FontSet, font_id: FontId, ch: char) -> Option<GlyphId> {
    with_font(fonts, font_id, |font, _| Some(font.glyph_id(ch)))
}

fn try_outline_for_char(
    px: f32,
    py: f32,
    em: f32,
    font_id: FontId,
    ch: char,
    fonts: &mut FontSet,
) -> Option<StandaloneGlyph> {
    let glyph_id = font_glyph_id(fonts, font_id, ch)?;
    if glyph_id.0 == 0 {
        return None;
    }
    outline_to_d_for_font(px, py, em, font_id, fonts, glyph_id).map(StandaloneGlyph::Path)
}

fn try_emoji_png_data_url(px: f32, py: f32, em: f32, ch: char) -> Option<StandaloneGlyph> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    #[cfg(target_os = "macos")]
    let request_em = em * 2.0;
    #[cfg(not(target_os = "macos"))]
    let request_em = em;

    let strike = ratex_unicode_font::emoji_png_raster_for_char(ch, request_em)?;
    let pixels_per_em = f32::from(strike.pixels_per_em.max(1));
    let mut scale = em / pixels_per_em;
    let actual_width_em = f32::from(strike.width) / pixels_per_em;
    if actual_width_em > 1.01 {
        scale /= actual_width_em;
    }
    let x = px + f32::from(strike.x) * scale;
    let mut y = py - (f32::from(strike.y) + f32::from(strike.height)) * scale;
    let center_strike = (f32::from(strike.y) + f32::from(strike.height) / 2.0) / pixels_per_em;
    let axis = ratex_font::get_global_metrics(0).axis_height as f32;
    y += (center_strike - axis) * em;
    let width = f32::from(strike.width) * scale;
    let height = f32::from(strike.height) * scale;
    let href = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(strike.data.as_ref())
    );
    Some(StandaloneGlyph::Image {
        href,
        x,
        y,
        w: width,
        h: height,
    })
}

fn try_emoji_raster_then_vector_svg(
    px: f32,
    py: f32,
    em: f32,
    ch: char,
    fonts: &mut FontSet,
) -> Option<StandaloneGlyph> {
    if let Some(image) = try_emoji_png_data_url(px, py, em, ch) {
        return Some(image);
    }
    try_outline_for_char(px, py, em, FontId::EmojiFallback, ch, fonts)
}

fn try_emoji_raster_or_vector_svg(
    px: f32,
    py: f32,
    em: f32,
    ch: char,
    fonts: &mut FontSet,
    glyph_id: GlyphId,
) -> Option<StandaloneGlyph> {
    if let Some(image) = try_emoji_png_data_url(px, py, em, ch) {
        return Some(image);
    }
    outline_to_d_for_font(px, py, em, FontId::EmojiFallback, fonts, glyph_id)
        .map(StandaloneGlyph::Path)
}

fn try_system_unicode_fallback_svg(
    px: f32,
    py: f32,
    em: f32,
    ch: char,
    fonts: &mut FontSet,
    skip_main_regular: bool,
) -> Option<StandaloneGlyph> {
    if !skip_main_regular {
        if let Some(glyph) = try_outline_for_char(px, py, em, FontId::MainRegular, ch, fonts) {
            return Some(glyph);
        }
    }
    if let Some(glyph) = try_outline_for_char(px, py, em, FontId::CjkRegular, ch, fonts) {
        return Some(glyph);
    }
    if let Some(glyph) = try_emoji_raster_then_vector_svg(px, py, em, ch, fonts) {
        return Some(glyph);
    }
    try_outline_for_char(px, py, em, FontId::CjkFallback, ch, fonts)
}

fn outline_to_d_for_font(
    px: f32,
    py: f32,
    em: f32,
    font_id: FontId,
    fonts: &mut FontSet,
    glyph_id: GlyphId,
) -> Option<String> {
    with_font(fonts, font_id, |font, identity| {
        outline_to_d(px, py, em, font_id, identity, font, glyph_id)
    })
}

fn outline_to_d(
    px: f32,
    py: f32,
    em: f32,
    font_id: FontId,
    font_identity: u64,
    font: &FontRef<'_>,
    glyph_id: GlyphId,
) -> Option<String> {
    let curves = ratex_font_loader::outline_cache::get_or_compute_outline(
        font_identity,
        font_id,
        font,
        glyph_id,
    )?;
    let units_per_em = font.units_per_em().unwrap_or(1000.0);
    let mut scale = em / units_per_em;

    if font_id == FontId::EmojiFallback {
        let actual_advance_em = font.h_advance_unscaled(glyph_id) / units_per_em;
        if actual_advance_em > 1.01 {
            scale /= actual_advance_em;
        }
    }

    let mut path = String::new();
    let mut last_end: Option<(f32, f32)> = None;

    for curve in curves.iter() {
        let (start, end) = match curve {
            OutlineCurve::Line(p0, p1) => (
                (px + p0.x * scale, py - p0.y * scale),
                (px + p1.x * scale, py - p1.y * scale),
            ),
            OutlineCurve::Quad(p0, _, p2) => (
                (px + p0.x * scale, py - p0.y * scale),
                (px + p2.x * scale, py - p2.y * scale),
            ),
            OutlineCurve::Cubic(p0, _, _, p3) => (
                (px + p0.x * scale, py - p0.y * scale),
                (px + p3.x * scale, py - p3.y * scale),
            ),
        };

        let needs_move = last_end.is_none_or(|(last_x, last_y)| {
            (last_x - start.0).abs() > 0.01 || (last_y - start.1).abs() > 0.01
        });
        if needs_move {
            if last_end.is_some() {
                path.push_str("Z ");
            }
            use std::fmt::Write as _;
            let _ = write!(
                &mut path,
                "M{} {} ",
                super::fmt_num(start.0 as f64),
                super::fmt_num(start.1 as f64)
            );
        }

        use std::fmt::Write as _;
        match curve {
            OutlineCurve::Line(_, p1) => {
                let _ = write!(
                    &mut path,
                    "L{} {} ",
                    super::fmt_num((px + p1.x * scale) as f64),
                    super::fmt_num((py - p1.y * scale) as f64)
                );
            }
            OutlineCurve::Quad(_, p1, p2) => {
                let _ = write!(
                    &mut path,
                    "Q{} {} {} {} ",
                    super::fmt_num((px + p1.x * scale) as f64),
                    super::fmt_num((py - p1.y * scale) as f64),
                    super::fmt_num((px + p2.x * scale) as f64),
                    super::fmt_num((py - p2.y * scale) as f64)
                );
            }
            OutlineCurve::Cubic(_, p1, p2, p3) => {
                let _ = write!(
                    &mut path,
                    "C{} {} {} {} {} {} ",
                    super::fmt_num((px + p1.x * scale) as f64),
                    super::fmt_num((py - p1.y * scale) as f64),
                    super::fmt_num((px + p2.x * scale) as f64),
                    super::fmt_num((py - p2.y * scale) as f64),
                    super::fmt_num((px + p3.x * scale) as f64),
                    super::fmt_num((py - p3.y * scale) as f64)
                );
            }
        }
        last_end = Some(end);
    }

    if last_end.is_some() {
        path.push('Z');
    }
    let path = path.trim().to_string();
    (!path.is_empty()).then_some(path)
}
