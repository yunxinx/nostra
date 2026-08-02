//! Real-font soft-wrap regression probe.
//!
//! The legacy phase intentionally contrasts GPUI's isolated-character width
//! estimator with the width eventually painted by shaping. The production
//! phase follows gpui-component's input path: `shape_text` chooses boundaries,
//! public shaped-run/glyph APIs map them to UTF-8 offsets, and each visual line
//! is shaped again exactly as it is painted.
//!
//! Run with `cargo run --example wrap_probe`.

use std::ops::Range;

use gpui::{
    AppContext as _, Context, IntoElement, LineFragment, Pixels, Render, TextRun, Window,
    WindowOptions, black, div, font, px,
};

const WRAP_WIDTH_EPSILON: Pixels = px(0.01);

const SAMPLES: &[&str] = &[
    "这是一段比较长的中文内容用来验证软换行的断行位置估算是否与实际绘制宽度一致再补充一些文字",
    "而应该是这样的，建议你在输入框里粘贴一大段中文，然后观察最右侧的字符是否被遮挡了一半。",
    "中文与English混排的情况test一下wrap行为123，看看结果如何。",
];

struct Probe;

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn main() {
    gpui_platform::application().run(move |cx| {
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/MapleMono-CN-Regular.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice(),
                ),
            ])
            .expect("register bundled probe fonts");

        cx.open_window(WindowOptions::default(), |window, cx| {
            let platform_mono = if cfg!(target_os = "macos") {
                "Menlo"
            } else if cfg!(target_os = "windows") {
                "Consolas"
            } else {
                "DejaVu Sans Mono"
            };
            let families = [
                ".SystemUIFont",
                platform_mono,
                "Maple Mono CN",
                "JetBrains Mono",
            ];
            let mut legacy_overflows = 0;
            let mut production_overflows = 0;

            for family in families {
                let font = font(family);
                let text_system = cx.text_system().clone();
                let font_id = text_system.resolve_font(&font);
                println!("######## font={family} ########");
                println!(
                    "metrics@16px a={:?} 中={:?} ，={:?}",
                    text_system.layout_width(font_id, px(16.), 'a'),
                    text_system.layout_width(font_id, px(16.), '中'),
                    text_system.layout_width(font_id, px(16.), '，'),
                );

                for size in [px(14.), px(16.)] {
                    for wrap_width in [px(200.), px(300.), px(420.)] {
                        for (sample_ix, text) in SAMPLES.iter().copied().enumerate() {
                            let report = ProbeReport {
                                family,
                                sample_ix,
                                text,
                                wrap_width,
                                font: font.clone(),
                                font_size: size,
                            };
                            let legacy_ranges = legacy_ranges(
                                text,
                                wrap_width,
                                font.clone(),
                                size,
                                &text_system,
                            );
                            legacy_overflows += report_ranges(
                                "legacy-estimator",
                                &legacy_ranges,
                                &report,
                                window,
                            );

                            let Some(production_ranges) = shaped_ranges(
                                text,
                                wrap_width,
                                font.clone(),
                                size,
                                window,
                            ) else {
                                production_overflows += 1;
                                eprintln!(
                                    "production boundary mapping failed: font={family} size={size:?} wrap={wrap_width:?} sample={sample_ix}"
                                );
                                continue;
                            };
                            production_overflows += report_ranges(
                                "production-shaped",
                                &production_ranges,
                                &report,
                                window,
                            );
                        }
                    }
                }
            }

            println!("legacy estimator visual overflow count: {legacy_overflows}");
            println!("production shaped visual overflow count: {production_overflows}");
            assert_eq!(
                production_overflows, 0,
                "production soft-wrap lines must stay within width + {WRAP_WIDTH_EPSILON:?}"
            );

            std::process::exit(0);
            #[allow(unreachable_code)]
            cx.new(|_| Probe)
        })
        .expect("open probe window");
    });
}

fn legacy_ranges(
    text: &str,
    wrap_width: Pixels,
    font: gpui::Font,
    font_size: Pixels,
    text_system: &std::sync::Arc<gpui::TextSystem>,
) -> Vec<Range<usize>> {
    let mut wrapper = text_system.line_wrapper(font, font_size);
    let mut ranges = Vec::new();
    let mut start = 0;
    for boundary in wrapper.wrap_line(&[LineFragment::text(text)], wrap_width) {
        if start < boundary.ix {
            ranges.push(start..boundary.ix);
        }
        start = boundary.ix;
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    ranges
}

fn shaped_ranges(
    text: &str,
    wrap_width: Pixels,
    font: gpui::Font,
    font_size: Pixels,
    window: &mut Window,
) -> Option<Vec<Range<usize>>> {
    let run = text_run(text.len(), font);
    let lines = window
        .text_system()
        .shape_text(
            text.to_string().into(),
            font_size,
            &[run],
            Some(wrap_width),
            None,
        )
        .ok()?;
    let line = lines.first()?;
    let mut ranges = Vec::with_capacity(line.wrap_boundaries().len() + 1);
    let mut start = 0;
    for boundary in line.wrap_boundaries() {
        let end = line
            .runs()
            .get(boundary.run_ix)?
            .glyphs
            .get(boundary.glyph_ix)?
            .index;
        if end <= start || end > text.len() || !text.is_char_boundary(end) {
            return None;
        }
        ranges.push(start..end);
        start = end;
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    Some(ranges)
}

struct ProbeReport<'a> {
    family: &'a str,
    sample_ix: usize,
    text: &'a str,
    wrap_width: Pixels,
    font: gpui::Font,
    font_size: Pixels,
}

fn report_ranges(
    phase: &str,
    ranges: &[Range<usize>],
    report: &ProbeReport<'_>,
    window: &mut Window,
) -> usize {
    let mut overflows = 0;
    for (line_ix, range) in ranges.iter().enumerate() {
        let segment = &report.text[range.clone()];
        let shaped = window.text_system().shape_line(
            segment.to_string().into(),
            report.font_size,
            &[text_run(segment.len(), report.font.clone())],
            None,
        );
        let overflow = shaped.width() - report.wrap_width;
        let visual_overflow = shaped.width() > report.wrap_width + WRAP_WIDTH_EPSILON;
        overflows += usize::from(visual_overflow);
        let ProbeReport {
            family,
            sample_ix,
            wrap_width,
            font_size,
            ..
        } = report;
        println!(
            "phase={phase} font={family} size={font_size:?} wrap={wrap_width:?} sample={sample_ix} line={line_ix} shaped={:?} overflow={overflow:?}{}",
            shaped.width(),
            if visual_overflow {
                " <-- VISUAL OVERFLOW"
            } else {
                ""
            },
        );
    }
    overflows
}

fn text_run(len: usize, font: gpui::Font) -> TextRun {
    TextRun {
        len,
        font,
        color: black(),
        ..Default::default()
    }
}
