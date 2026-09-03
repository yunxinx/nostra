//! Asynchronous RaTeX rendering and explicit GPUI image-cache ownership.

use std::{borrow::Cow, sync::Arc, time::Duration};

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use gpui::{App, AppContext as _, Hsla, Pixels, RenderImage, Rgba, Task, Window, px};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parse;
use ratex_svg::{SvgColorSyntax, SvgOptions, render_to_svg_with_color_syntax};
use ratex_types::{Color, DisplayList, MathStyle};

use crate::ui::markdown::MarkdownContributionOwner;

// RaTeX's SVG coordinate system is expressed in em units multiplied by this
// font size. These small pads prevent antialiasing at the outer glyph/stroke
// edge from being clipped; display formulas receive more breathing room.
const INLINE_SVG_PADDING: f64 = 1.0;
const DISPLAY_SVG_PADDING: f64 = 4.0;
const SVG_STROKE_WIDTH: f64 = 1.5;
pub(crate) const FORMULA_DEBOUNCE: Duration = Duration::from_millis(120);
const GLYPH_NORMALIZATIONS: &[(char, &str)] = &[('·', "⋅"), ('℃', "°C")];
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
    pub(super) fn apply(self, source: &str, display: bool) -> String {
        if display || source.contains("\\begin{") {
            return source.to_string();
        }
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
    pub(super) contribution_owner: MarkdownContributionOwner,
    pub(super) font_size: f32,
    pub(super) color: Hsla,
}

enum FormulaStatus {
    Idle,
    Pending,
    Ready,
    Failed,
}

pub(super) struct FormulaSnapshot {
    pub(super) rendered: Option<RenderedFormula>,
    pub(super) failed: bool,
    pub(super) last_height: Option<Pixels>,
}

struct FormulaCache {
    fingerprint: FormulaFingerprint,
    generation: u64,
    status: FormulaStatus,
    displayed: Option<RenderedFormula>,
    last_height: Option<Pixels>,
    /// True while a fingerprint change happened inside the 120ms coalesce
    /// window. False means the next change is a first appearance or a gap
    /// long enough to render immediately.
    coalesce_open: bool,
    recent_change: Option<Task<()>>,
    debounce: Option<Task<()>>,
    _task: Option<Task<()>>,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FormulaCacheSnapshot {
    pub(crate) source: String,
    pub(crate) inline: bool,
    pub(crate) color: Hsla,
    pub(crate) generation: u64,
    pub(crate) ready: bool,
    pub(crate) active: bool,
    pub(crate) release_count: usize,
    pub(crate) image_drop_count: usize,
    /// Baseline metrics of the displayed image, once one is installed.
    pub(crate) ascent: Option<Pixels>,
    pub(crate) descent: Option<Pixels>,
    /// True while a raster is on screen, including re-render Pending.
    pub(crate) has_displayed: bool,
}

#[cfg(test)]
type FormulaProbeKey = (MarkdownContributionOwner, u64, usize);

#[cfg(test)]
std::thread_local! {
    static FORMULA_RENDER_SUBMITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FORMULA_LAST_FAILURE: std::cell::RefCell<Option<(String, String)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn record_formula_render_submit() {
    FORMULA_RENDER_SUBMITS.with(|submits| submits.set(submits.get().saturating_add(1)));
}

#[cfg(test)]
fn record_formula_failure(stage: &str, summary: &str) {
    FORMULA_LAST_FAILURE.with(|failure| {
        *failure.borrow_mut() = Some((stage.to_string(), summary.to_string()));
    });
}

#[cfg(test)]
pub(crate) fn formula_background_submits() -> usize {
    FORMULA_RENDER_SUBMITS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_formula_background_submits() {
    FORMULA_RENDER_SUBMITS.with(|submits| submits.set(0));
}

#[cfg(test)]
pub(crate) fn last_formula_failure() -> Option<(String, String)> {
    FORMULA_LAST_FAILURE.with(|failure| failure.borrow().clone())
}

#[cfg(test)]
fn formula_cache_probes() -> &'static Mutex<HashMap<FormulaProbeKey, FormulaCacheSnapshot>> {
    static PROBES: OnceLock<Mutex<HashMap<FormulaProbeKey, FormulaCacheSnapshot>>> =
        OnceLock::new();
    PROBES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn record_formula_cache(
    contribution_owner: MarkdownContributionOwner,
    owner_id: u64,
    start: usize,
    fingerprint: &FormulaFingerprint,
    generation: u64,
    status: &FormulaStatus,
    displayed: Option<&RenderedFormula>,
) {
    let mut probes = formula_cache_probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = probes.get(&(contribution_owner, owner_id, start));
    let release_count = previous.map_or(0, |snapshot| snapshot.release_count);
    let image_drop_count = previous.map_or(0, |snapshot| snapshot.image_drop_count);
    probes.insert(
        (contribution_owner, owner_id, start),
        FormulaCacheSnapshot {
            source: fingerprint.source.clone(),
            inline: fingerprint.inline,
            color: fingerprint.color,
            generation,
            ready: matches!(status, FormulaStatus::Ready),
            active: true,
            release_count,
            image_drop_count,
            ascent: displayed.map(|rendered| rendered.ascent),
            descent: displayed.map(|rendered| rendered.descent),
            has_displayed: displayed.is_some(),
        },
    );
}

#[cfg(test)]
fn record_formula_release(
    contribution_owner: MarkdownContributionOwner,
    owner_id: u64,
    start: usize,
    dropped_image: bool,
) {
    if let Some(snapshot) = formula_cache_probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(&(contribution_owner, owner_id, start))
    {
        snapshot.active = false;
        snapshot.release_count += 1;
        snapshot.image_drop_count += usize::from(dropped_image);
    }
}

#[cfg(test)]
fn record_formula_image_drop(
    contribution_owner: MarkdownContributionOwner,
    owner_id: u64,
    start: usize,
) {
    if let Some(snapshot) = formula_cache_probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(&(contribution_owner, owner_id, start))
    {
        snapshot.image_drop_count += 1;
    }
}

#[cfg(test)]
pub(crate) fn formula_cache_snapshot(owner_id: u64, start: usize) -> Option<FormulaCacheSnapshot> {
    formula_cache_probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter(|((_, candidate_owner, candidate_start), _)| {
            *candidate_owner == owner_id && *candidate_start == start
        })
        .max_by_key(|((contribution_owner, _, _), _)| contribution_owner.generation())
        .map(|(_, snapshot)| snapshot.clone())
}

#[cfg(test)]
pub(crate) fn formula_cache_snapshot_for_owner(
    contribution_owner: MarkdownContributionOwner,
    owner_id: u64,
    start: usize,
) -> Option<FormulaCacheSnapshot> {
    formula_cache_probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(contribution_owner, owner_id, start))
        .cloned()
}

#[cfg(test)]
pub(crate) fn formula_cache_snapshots(owner_id: u64) -> Vec<(usize, FormulaCacheSnapshot)> {
    let mut snapshots = formula_cache_probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter_map(
            |(&(contribution_owner, candidate_owner, start), snapshot)| {
                if candidate_owner == owner_id {
                    Some((contribution_owner.generation(), start, snapshot.clone()))
                } else {
                    None
                }
            },
        )
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|(generation, start, _)| (*start, *generation));
    snapshots
        .into_iter()
        .fold(Vec::new(), |mut latest, (_, start, snapshot)| {
            if latest
                .last()
                .is_some_and(|(latest_start, _)| *latest_start == start)
            {
                latest.pop();
            }
            latest.push((start, snapshot));
            latest
        })
}

impl FormulaCache {
    fn new(fingerprint: FormulaFingerprint) -> Self {
        Self {
            fingerprint,
            generation: 0,
            status: FormulaStatus::Idle,
            displayed: None,
            last_height: None,
            coalesce_open: false,
            recent_change: None,
            debounce: None,
            _task: None,
        }
    }

    fn take_image(&mut self) -> Option<Arc<RenderImage>> {
        self.displayed.take().map(|rendered| rendered.image)
    }
}

pub(super) fn cached_formula_snapshot(
    request: FormulaRequest<'_>,
    window: &mut Window,
    cx: &mut App,
) -> FormulaSnapshot {
    #[cfg(test)]
    let probe_key = (request.contribution_owner, request.owner_id, request.start);
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
    let key =
        request
            .contribution_owner
            .keyed_state_id("markdown-math", request.owner_id, request.start);
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
                move |cache: &mut FormulaCache, window, cx| {
                    let image = cache.take_image();
                    #[cfg(test)]
                    record_formula_release(probe_key.0, probe_key.1, probe_key.2, image.is_some());
                    if let Some(image) = image {
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
        schedule_formula_render(
            &cache,
            fingerprint.clone(),
            #[cfg(test)]
            probe_key,
            window,
            cx,
        );
    }

    let cached = cache.read(cx);
    FormulaSnapshot {
        rendered: cached.displayed.clone(),
        failed: matches!(cached.status, FormulaStatus::Failed),
        last_height: cached.last_height,
    }
}

fn schedule_formula_render(
    cache: &gpui::Entity<FormulaCache>,
    fingerprint: FormulaFingerprint,
    #[cfg(test)] probe_key: FormulaProbeKey,
    window: &mut Window,
    cx: &mut App,
) {
    let (generation, within_window) = cache.update(cx, |cache, _| {
        let within_window = cache.coalesce_open;
        cache.coalesce_open = true;
        cache.generation = cache.generation.wrapping_add(1);
        cache.fingerprint = fingerprint.clone();
        cache.status = FormulaStatus::Pending;
        cache.debounce = None;
        cache._task = None;
        (cache.generation, within_window)
    });

    #[cfg(test)]
    {
        let cached = cache.read(cx);
        record_formula_cache(
            probe_key.0,
            probe_key.1,
            probe_key.2,
            &cached.fingerprint,
            cached.generation,
            &cached.status,
            cached.displayed.as_ref(),
        );
    }

    arm_recent_change_window(cache, window, cx);

    if within_window {
        let weak_cache = cache.downgrade();
        let render_fingerprint = fingerprint.clone();
        let debounce = window.spawn(cx, async move |async_cx| {
            async_cx.background_executor().timer(FORMULA_DEBOUNCE).await;
            let Some(cache) = weak_cache.upgrade() else {
                return;
            };
            let _ = async_cx.update(|window, cx| {
                start_formula_render(
                    cache,
                    generation,
                    render_fingerprint,
                    #[cfg(test)]
                    probe_key,
                    window,
                    cx,
                );
            });
        });
        cache.update(cx, |cache, _| cache.debounce = Some(debounce));
        return;
    }

    start_formula_render(
        cache.clone(),
        generation,
        fingerprint,
        #[cfg(test)]
        probe_key,
        window,
        cx,
    );
}

fn arm_recent_change_window(cache: &gpui::Entity<FormulaCache>, window: &mut Window, cx: &mut App) {
    let weak_cache = cache.downgrade();
    let recent = window.spawn(cx, async move |async_cx| {
        async_cx.background_executor().timer(FORMULA_DEBOUNCE).await;
        let Some(cache) = weak_cache.upgrade() else {
            return;
        };
        let _ = async_cx.update(|_, cx| {
            cache.update(cx, |cache, _| cache.coalesce_open = false);
        });
    });
    cache.update(cx, |cache, _| cache.recent_change = Some(recent));
}

fn start_formula_render(
    cache: gpui::Entity<FormulaCache>,
    generation: u64,
    fingerprint: FormulaFingerprint,
    #[cfg(test)] probe_key: FormulaProbeKey,
    window: &mut Window,
    cx: &mut App,
) {
    let stale = {
        let current = cache.read(cx);
        current.generation != generation || current.fingerprint != fingerprint
    };
    if stale {
        return;
    }

    #[cfg(test)]
    record_formula_render_submit();

    let weak_cache = cache.downgrade();
    let svg_renderer = cx.svg_renderer();
    let render_fingerprint = fingerprint.clone();
    let background = cx
        .background_spawn(async move { render_formula_image(&render_fingerprint, &svg_renderer) });
    let task = window.spawn(cx, async move |async_cx| {
        let rendered = background.await;
        let mut discarded_image = None;
        let mut replaced_image = None;
        let _ = async_cx.update(|window, cx| {
            let _ = weak_cache.update(cx, |cache, cache_cx| {
                if cache.generation != generation || cache.fingerprint != fingerprint {
                    discarded_image = rendered.map(|rendered| rendered.image);
                    return;
                }
                match rendered {
                    Some(rendered) => {
                        cache.last_height = Some(rendered.height);
                        replaced_image = cache
                            .displayed
                            .replace(rendered)
                            .map(|rendered| rendered.image);
                        cache.status = FormulaStatus::Ready;
                    }
                    None => {
                        replaced_image = cache.take_image();
                        cache.status = FormulaStatus::Failed;
                    }
                }
                #[cfg(test)]
                record_formula_cache(
                    probe_key.0,
                    probe_key.1,
                    probe_key.2,
                    &cache.fingerprint,
                    cache.generation,
                    &cache.status,
                    cache.displayed.as_ref(),
                );
                cache_cx.notify();
            });
            if let Some(image) = discarded_image.take() {
                #[cfg(test)]
                record_formula_image_drop(probe_key.0, probe_key.1, probe_key.2);
                cx.drop_image(image, Some(window));
            }
            if let Some(image) = replaced_image.take() {
                #[cfg(test)]
                record_formula_image_drop(probe_key.0, probe_key.1, probe_key.2);
                cx.drop_image(image, Some(window));
            }
        });
    });
    cache.update(cx, |cache, _| cache._task = Some(task));
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
    match svg_renderer.render_single_frame(svg.as_bytes(), fingerprint.raster_scale) {
        Ok(image) => Some(RenderedFormula {
            image,
            width: px(width),
            height: px(height),
            ascent: px(ascent),
            descent: px(descent),
        }),
        Err(error) => {
            log_formula_failure("raster", error);
            None
        }
    }
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

    let source = style.apply(&normalize_glyphs(source), !inline);
    let nodes = match parse(&source) {
        Ok(nodes) if !nodes.is_empty() => nodes,
        Ok(_) => {
            log_formula_failure("parse", "empty node list");
            return None;
        }
        Err(error) => {
            log_formula_failure("parse", error);
            return None;
        }
    };

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
        log_formula_failure("layout", "non-positive metrics");
        return None;
    }

    let svg = render_display_list_svg(&display_list, font_size, inline);
    let font_size = f64::from(font_size);
    let padding = if inline {
        INLINE_SVG_PADDING
    } else {
        DISPLAY_SVG_PADDING
    };
    let width = (display_list.width * font_size + padding * 2.0) as f32;
    let ascent = (display_list.height * font_size + padding) as f32;
    let descent = (display_list.depth * font_size + padding) as f32;
    let height = ascent + descent;
    Some((svg, width, height, ascent, descent))
}

fn render_display_list_svg(display_list: &DisplayList, font_size: f32, inline: bool) -> String {
    let font_size = f64::from(font_size);
    let padding = if inline {
        INLINE_SVG_PADDING
    } else {
        DISPLAY_SVG_PADDING
    };
    render_to_svg_with_color_syntax(
        display_list,
        &SvgOptions {
            font_size,
            padding,
            stroke_width: SVG_STROKE_WIDTH,
            embed_glyphs: true,
            font_dir: String::new(),
        },
        SvgColorSyntax::Rgb,
    )
}

fn ratex_color(color: Hsla) -> Color {
    let rgba: Rgba = color.into();
    Color::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

pub(super) fn normalize_glyphs(source: &str) -> Cow<'_, str> {
    if !source.chars().any(|character| {
        GLYPH_NORMALIZATIONS
            .iter()
            .any(|(from, _)| character == *from)
    }) {
        return Cow::Borrowed(source);
    }
    let mut normalized = String::with_capacity(source.len());
    for character in source.chars() {
        if let Some((_, replacement)) = GLYPH_NORMALIZATIONS
            .iter()
            .find(|(from, _)| character == *from)
        {
            normalized.push_str(replacement);
        } else {
            normalized.push(character);
        }
    }
    Cow::Owned(normalized)
}

fn log_formula_failure(stage: &str, summary: impl std::fmt::Display) {
    let message = format!("{stage}: {summary}");
    crate::logging::warn("ui.math", &message);
    #[cfg(test)]
    record_formula_failure(stage, &message);
}

pub(super) fn estimate_display_placeholder_height(source: &str, font_size: f32) -> Pixels {
    // Display rows use a 1.18 scale and 1.2 line-height. Large operators such
    // as `\sum` with limits are taller than one source line of prose, so keep
    // a two-line floor plus SVG padding. AC10 allows at most one line-height
    // of jump once the raster arrives.
    const DISPLAY_PLACEHOLDER_SCALE: f32 = 1.18;
    const DISPLAY_PLACEHOLDER_LINE_HEIGHT: f32 = 1.2;
    const MIN_DISPLAY_PLACEHOLDER_SIZE: f32 = 12.0;
    let lines = source.lines().count().max(2) as f32;
    let scaled = (font_size * DISPLAY_PLACEHOLDER_SCALE).max(MIN_DISPLAY_PLACEHOLDER_SIZE);
    px(scaled * DISPLAY_PLACEHOLDER_LINE_HEIGHT * lines + DISPLAY_SVG_PADDING as f32 * 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratex_types::DisplayItem;

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
            r"\int_{-\infty}^{\infty} e^{-x^2}\,dx = \sqrt{\pi}",
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
        assert_eq!(style.apply("x^2", false), r"\sout{\boldsymbol{x^2}}");
        assert_eq!(
            style.apply(r"\begin{aligned}a\\b\end{aligned}", false),
            r"\begin{aligned}a\\b\end{aligned}"
        );
        assert_eq!(style.apply("x^2", true), "x^2");
        assert!(render_formula_svg("x^2", true, style, 16.0, Hsla::black()).is_some());
    }

    #[test]
    fn glyph_normalization_rewrites_missing_ratex_symbols() {
        assert_eq!(normalize_glyphs("a·b℃").as_ref(), "a⋅b°C");
        assert!(
            render_formula_svg("a·b", true, FormulaStyle::default(), 16.0, Hsla::black()).is_some()
        );
    }

    #[test]
    fn display_sum_placeholder_stays_within_one_em_of_raster_height() {
        let source = r"\sum_{n=1}^{N} n";
        let font_size = 16.0;
        let (_, _, height, _, _) = render_formula_svg(
            source,
            false,
            FormulaStyle::default(),
            font_size,
            Hsla::black(),
        )
        .expect("sum should render");
        let estimate = f32::from(estimate_display_placeholder_height(source, font_size));
        assert!(
            (estimate - height).abs() <= font_size * 1.2,
            "placeholder {estimate} vs raster {height} (em {font_size})"
        );
    }

    #[test]
    fn failed_formulas_record_a_stage_and_summary() {
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
        let (stage, summary) = last_formula_failure().expect("failure log");
        assert_eq!(stage, "parse");
        assert!(summary.contains("parse"), "{summary}");
    }

    #[test]
    fn unicode_edge_glyphs_always_have_visible_or_selectable_output() {
        let cases = [
            ('⌘', "ordinary symbol"),
            ('\u{0301}', "combining mark"),
            ('\u{fe0e}', "text variation selector"),
            ('\u{fe0f}', "emoji variation selector"),
            ('\u{200d}', "zero width joiner"),
            ('\u{10ffff}', "missing glyph"),
        ];

        for (ch, label) in cases {
            let svg = render_display_list_svg(&glyph_path_list("Main-Regular", ch), 16.0, true);
            assert!(
                svg.contains("<path") || svg.contains("<image") || svg.contains("<text"),
                "{label} disappeared from SVG: {svg}"
            );
        }
    }

    #[test]
    fn requested_font_miss_uses_main_regular_outline() {
        let svg = render_display_list_svg(&glyph_path_list("Size4-Regular", 'x'), 16.0, true);
        assert!(
            svg.contains("<path"),
            "main font fallback was not outlined: {svg}"
        );
    }

    #[test]
    fn implicit_geometry_inherits_non_black_layout_color() {
        let color = gpui::hsla(0.0, 1.0, 0.5, 1.0);
        let source = r"\frac{1}{2}";
        let (svg, _, _, _, _) =
            render_formula_svg(source, true, FormulaStyle::default(), 16.0, color)
                .expect("formula should render");
        let expected_color = ratex_color(color);
        let expected_paint = rgb_paint(color);
        let nodes = parse(source).expect("frac should parse");
        let display_list = to_display_list(&layout(
            &nodes,
            &LayoutOptions::default()
                .with_style(MathStyle::Text)
                .with_color(expected_color),
        ));
        let rule_colors: Vec<_> = display_list
            .items
            .iter()
            .filter_map(|item| match item {
                DisplayItem::Line {
                    color: rule_color, ..
                } => Some(*rule_color),
                _ => None,
            })
            .collect();
        assert!(
            !rule_colors.is_empty(),
            "fraction rule missing from display list"
        );
        assert!(
            rule_colors
                .iter()
                .all(|rule_color| *rule_color == expected_color),
            "implicit geometry used {rule_colors:?} instead of {expected_color:?}"
        );

        let rule_fills = svg_rect_fills(&svg);
        assert!(
            rule_fills.iter().any(|fill| fill == &expected_paint),
            "fraction rule missing theme color {expected_paint}: {svg}"
        );
        assert!(
            rule_fills.iter().all(|fill| !paint_is_black(fill)),
            "fraction rule SVG paint stayed black: {svg}"
        );
    }

    fn glyph_path_list(font: &str, ch: char) -> DisplayList {
        DisplayList {
            items: vec![DisplayItem::GlyphPath {
                x: 0.0,
                y: 1.0,
                scale: 1.0,
                font: font.to_string(),
                char_code: u32::from(ch),
                color: Color::BLACK,
            }],
            width: 1.2,
            height: 1.2,
            depth: 0.0,
        }
    }

    fn rgb_paint(color: Hsla) -> String {
        let rgba: Rgba = color.into();
        format!(
            "rgb({},{},{})",
            (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u8,
            (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u8,
            (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }

    fn svg_rect_fills(svg: &str) -> Vec<String> {
        let mut fills = Vec::new();
        let mut rest = svg;
        while let Some(start) = rest.find("<rect") {
            rest = &rest[start + 5..];
            let Some(end) = rest.find('>') else {
                break;
            };
            let tag = &rest[..end];
            if let Some(fill_at) = tag.find("fill=\"") {
                let value = &tag[fill_at + 6..];
                if let Some(close) = value.find('"') {
                    fills.push(value[..close].to_string());
                }
            }
            rest = &rest[end..];
        }
        fills
    }

    fn paint_is_black(paint: &str) -> bool {
        paint == "rgb(0,0,0)" || paint.starts_with("rgba(0,0,0,")
    }
}
