//! Asynchronous RaTeX rendering and explicit GPUI image-cache ownership.

use std::sync::Arc;

use gpui::{App, AppContext as _, Hsla, Pixels, RenderImage, Rgba, Task, Window, px};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parse;
use ratex_svg::{SvgColorSyntax, SvgOptions, render_to_svg_with_color_syntax};
use ratex_types::{Color, MathStyle};

// RaTeX's SVG coordinate system is expressed in em units multiplied by this
// font size. These small pads prevent antialiasing at the outer glyph/stroke
// edge from being clipped; display formulas receive more breathing room.
const INLINE_SVG_PADDING: f64 = 1.0;
const DISPLAY_SVG_PADDING: f64 = 4.0;
const SVG_STROKE_WIDTH: f64 = 1.5;
#[derive(Clone)]
pub(super) struct RenderedFormula {
    pub(super) image: Arc<RenderImage>,
    pub(super) width: Pixels,
    pub(super) height: Pixels,
    pub(super) ascent: Pixels,
    pub(super) descent: Pixels,
}

/// Markdown marks that affect formula glyph generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FormulaStyle {
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) strikethrough: bool,
}

impl FormulaStyle {
    fn apply(self, source: &str) -> String {
        let mut styled = if self.bold {
            // `\boldsymbol` retains mathematical classes and the conventional
            // italic form of variables while applying Markdown's bold weight.
            format!(r"\boldsymbol{{{source}}}")
        } else if self.italic {
            format!(r"\mathit{{{source}}}")
        } else {
            source.to_string()
        };
        if self.strikethrough {
            styled = format!(r"\sout{{{styled}}}");
        }
        styled
    }
}

#[derive(Clone, PartialEq)]
struct FormulaFingerprint {
    source: String,
    inline: bool,
    style: FormulaStyle,
    font_size: f32,
    color: Hsla,
    raster_scale: f32,
}

pub(super) struct FormulaRequest<'a> {
    pub(super) source: &'a str,
    pub(super) inline: bool,
    pub(super) style: FormulaStyle,
    pub(super) start: usize,
    pub(super) owner_id: u64,
    pub(super) font_size: f32,
    pub(super) color: Hsla,
}

enum FormulaStatus {
    Idle,
    Pending,
    Ready(RenderedFormula),
    Failed,
}

struct FormulaCache {
    fingerprint: FormulaFingerprint,
    generation: u64,
    status: FormulaStatus,
    _task: Option<Task<()>>,
}

impl FormulaCache {
    fn new(fingerprint: FormulaFingerprint) -> Self {
        Self {
            fingerprint,
            generation: 0,
            status: FormulaStatus::Idle,
            _task: None,
        }
    }

    fn rendered(&self) -> Option<RenderedFormula> {
        match &self.status {
            FormulaStatus::Ready(rendered) => Some(rendered.clone()),
            FormulaStatus::Idle | FormulaStatus::Pending | FormulaStatus::Failed => None,
        }
    }

    fn take_image(&mut self) -> Option<Arc<RenderImage>> {
        let status = std::mem::replace(&mut self.status, FormulaStatus::Idle);
        match status {
            FormulaStatus::Ready(rendered) => Some(rendered.image),
            FormulaStatus::Idle | FormulaStatus::Pending | FormulaStatus::Failed => None,
        }
    }
}

pub(super) fn cached_formula(
    request: FormulaRequest<'_>,
    window: &mut Window,
    cx: &mut App,
) -> Option<RenderedFormula> {
    let fingerprint = FormulaFingerprint {
        source: request.source.to_string(),
        inline: request.inline,
        style: request.style,
        font_size: request.font_size,
        color: request.color,
        // render_single_frame always assigns a 2x logical image scale. Feeding
        // `DPR / 2` produces DPR-sized pixels while preserving the formula's
        // logical width and height at 1x, 2x, and 3x displays.
        raster_scale: (window.scale_factor() / gpui::SMOOTH_SVG_SCALE_FACTOR).max(0.5),
    };
    let key = format!("markdown-math-{}-{}", request.owner_id, request.start);
    let cache = window.use_keyed_state(key, cx, |window, cache_cx| {
        let cache = FormulaCache::new(fingerprint.clone());
        // `RenderImage` has no Drop implementation and ImageSource::Render is
        // outside GPUI's resource cache. When keyed element state ages out,
        // this release callback is the last owner with both the image and a
        // live Window, so it must explicitly evict the sprite-atlas entry.
        window
            .observe_release(
                &cache_cx.entity(),
                cache_cx,
                |cache: &mut FormulaCache, window, cx| {
                    if let Some(image) = cache.take_image() {
                        cx.drop_image(image, Some(window));
                    }
                },
            )
            .detach();
        cache
    });

    let needs_render = {
        let cached = cache.read(cx);
        cached.fingerprint != fingerprint || matches!(cached.status, FormulaStatus::Idle)
    };
    if needs_render {
        let stale_image = cache.update(cx, |cache, _| {
            let stale_image = cache.take_image();
            cache.generation = cache.generation.wrapping_add(1);
            cache.fingerprint = fingerprint.clone();
            cache.status = FormulaStatus::Pending;
            // Dropping the previous task cancels work that can no longer win.
            cache._task = None;
            stale_image
        });
        if let Some(image) = stale_image {
            cx.drop_image(image, Some(window));
        }

        let generation = cache.read(cx).generation;
        let weak_cache = cache.downgrade();
        let svg_renderer = cx.svg_renderer();
        let render_fingerprint = fingerprint.clone();
        // Parse, layout, SVG serialization, and rasterization are all CPU-heavy
        // and must remain off the UI thread. The foreground continuation only
        // installs the completed immutable image and requests one redraw.
        let background =
            cx.background_spawn(
                async move { render_formula_image(&render_fingerprint, &svg_renderer) },
            );
        let task = window.spawn(cx, async move |async_cx| {
            let rendered = background.await;
            let mut discarded_image = None;
            let _ = async_cx.update(|window, cx| {
                let _ = weak_cache.update(cx, |cache, cache_cx| {
                    // A streamed formula or theme/style change can supersede a
                    // background render. Never install stale pixels; explicitly
                    // release them because they may already have entered the
                    // current window's sprite atlas during an earlier frame.
                    if cache.generation != generation || cache.fingerprint != fingerprint {
                        discarded_image = rendered.map(|rendered| rendered.image);
                        return;
                    }
                    cache.status = match rendered {
                        Some(rendered) => FormulaStatus::Ready(rendered),
                        None => FormulaStatus::Failed,
                    };
                    cache_cx.notify();
                });
                if let Some(image) = discarded_image.take() {
                    cx.drop_image(image, Some(window));
                }
            });
        });
        cache.update(cx, |cache, _| cache._task = Some(task));
    }
    cache.read(cx).rendered()
}

fn render_formula_image(
    fingerprint: &FormulaFingerprint,
    svg_renderer: &gpui::SvgRenderer,
) -> Option<RenderedFormula> {
    let (svg, width, height, ascent, descent) = render_formula_svg(
        &fingerprint.source,
        fingerprint.inline,
        fingerprint.style,
        fingerprint.font_size,
        fingerprint.color,
    )?;
    let image = svg_renderer
        .render_single_frame(svg.as_bytes(), fingerprint.raster_scale)
        .ok()?;
    Some(RenderedFormula {
        image,
        width: px(width),
        height: px(height),
        ascent: px(ascent),
        descent: px(descent),
    })
}

fn render_formula_svg(
    source: &str,
    inline: bool,
    style: FormulaStyle,
    font_size: f32,
    foreground: Hsla,
) -> Option<(String, f32, f32, f32, f32)> {
    if source.trim().is_empty() || !font_size.is_finite() || font_size <= 0.0 {
        return None;
    }

    let source = style.apply(source);
    let nodes = parse(&source).ok()?;
    if nodes.is_empty() {
        return None;
    }

    let style = if inline {
        MathStyle::Text
    } else {
        MathStyle::Display
    };
    let layout = layout(
        &nodes,
        &LayoutOptions::default()
            .with_style(style)
            .with_color(ratex_color(foreground)),
    );
    let display_list = to_display_list(&layout);
    if display_list.width <= 0.0 || display_list.total_height() <= 0.0 {
        return None;
    }

    let font_size = f64::from(font_size);
    let padding = if inline {
        INLINE_SVG_PADDING
    } else {
        DISPLAY_SVG_PADDING
    };
    let svg = render_to_svg_with_color_syntax(
        &display_list,
        &SvgOptions {
            font_size,
            padding,
            stroke_width: SVG_STROKE_WIDTH,
            embed_glyphs: true,
            font_dir: String::new(),
        },
        SvgColorSyntax::Rgb,
    );
    let width = (display_list.width * font_size + padding * 2.0) as f32;
    let ascent = (display_list.height * font_size + padding) as f32;
    let descent = (display_list.depth * font_size + padding) as f32;
    let height = ascent + descent;
    Some((svg, width, height, ascent, descent))
}

fn ratex_color(color: Hsla) -> Color {
    let rgba: Rgba = color.into();
    Color::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_path_based_self_contained_svg() {
        let (svg, width, height, ascent, descent) = render_formula_svg(
            r"\frac{1}{2} + \sqrt{x}",
            true,
            FormulaStyle::default(),
            16.0,
            Hsla::black(),
        )
        .expect("formula should render");
        assert!(svg.starts_with("<svg"));
        assert!(svg.as_bytes().windows(5).any(|window| window == b"<path"));
        assert!(width > 0.0);
        assert!(height > 0.0);
        assert!(ascent > 0.0);
        assert!(descent > 0.0);
        assert!((ascent + descent - height).abs() < f32::EPSILON);
        assert!(!svg.as_bytes().windows(5).any(|window| window == b"<text"));
    }

    #[test]
    fn rasterizes_formula_svg_to_a_ready_render_image() {
        let (svg, _, _, _, _) = render_formula_svg(
            r"x^2 + y^2",
            false,
            FormulaStyle::default(),
            16.0,
            Hsla::black(),
        )
        .expect("formula should render");
        let renderer = gpui::SvgRenderer::new(Arc::new(()));
        let image = renderer
            .render_single_frame(svg.as_bytes(), 1.0)
            .expect("formula SVG should rasterize");
        assert_eq!(image.frame_count(), 1);
        assert!(image.size(0).width > gpui::DevicePixels(0));
        assert!(image.size(0).height > gpui::DevicePixels(0));
        assert!(
            image
                .as_bytes(0)
                .expect("raster frame")
                .chunks_exact(4)
                .any(|pixel| pixel[3] > 0)
        );
    }

    #[test]
    fn renders_representative_display_formulas() {
        for source in [
            r"\sum_{n=1}^{\infty} \frac{1}{n^2} = \frac{\pi^2}{6}",
            r"\begin{bmatrix} 1 & 2 & 3 \\ 4 & 5 & 6 \end{bmatrix}",
            r"f(x) = \begin{cases} x^2, & x \ge 0 \\ -x, & x < 0 \end{cases}",
        ] {
            let rendered =
                render_formula_svg(source, false, FormulaStyle::default(), 16.0, Hsla::black());
            assert!(
                rendered.is_some(),
                "display formula did not render: {source}"
            );
            let (svg, _, _, _, _) = rendered.expect("checked above");
            assert!(
                svg.as_bytes().windows(5).any(|window| window == b"<path"),
                "display formula fell back to text: {source}"
            );
        }
    }

    #[test]
    fn unsupported_formula_degrades_without_panicking() {
        assert!(
            render_formula_svg(
                r"\includegraphics{missing.png}",
                true,
                FormulaStyle::default(),
                16.0,
                Hsla::black(),
            )
            .is_none()
        );
    }

    #[test]
    fn markdown_formula_marks_are_applied_inside_ratex() {
        let style = FormulaStyle {
            bold: true,
            italic: true,
            strikethrough: true,
        };
        assert_eq!(style.apply("x^2"), r"\sout{\boldsymbol{x^2}}");
        assert!(render_formula_svg("x^2", true, style, 16.0, Hsla::black()).is_some());
    }
}
