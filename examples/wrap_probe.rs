//! Empirical probe: compare LineWrapper's wrap boundaries (what the input's
//! display map uses to decide soft-wrap points) against the width shape_line
//! actually paints for each wrapped segment.  Run: cargo run --example wrap_probe

use gpui::*;

struct Probe;

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn main() {
    let app = gpui_platform::application();
    app.run(move |cx| {
        // Register the bundled fonts the same way the app does, so the probe
        // measures exactly what the composer renders.
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/MapleMono-CN-Regular.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice(),
                ),
            ])
            .expect("register bundled fonts");

        cx.open_window(WindowOptions::default(), |window, cx| {
            // First family: what the UI inherits by default (proportional —
            // exhibits the estimator drift).  Then the theme mono font, then
            // the two bundled fonts the composer actually uses (each must
            // show zero phase-1 overflows for chat.rs to be safe).
            let mono_family = if cfg!(target_os = "macos") {
                "Menlo"
            } else if cfg!(target_os = "windows") {
                "Consolas"
            } else {
                "DejaVu Sans Mono"
            };
            for family in [
                ".SystemUIFont",
                mono_family,
                "Maple Mono CN",
                "JetBrains Mono",
            ] {
                println!("######## font = {family} ########");
                let ui_font = font(family);
                let app_ts = cx.text_system().clone();
                let font_id = app_ts.resolve_font(&ui_font);

                // Direct advance-width check: is the family metrically mono
                // (Latin half-width, CJK/fullwidth-punct exactly double)?
                let wa = app_ts.layout_width(font_id, px(16.), 'a');
                let wz = app_ts.layout_width(font_id, px(16.), '中');
                let wp = app_ts.layout_width(font_id, px(16.), '，');
                println!("metrics@16px a={wa:?} 中={wz:?} ，={wp:?}");

            let samples: &[&str] = &[
                // Pure CJK prose
                "这是一段比较长的中文内容用来验证软换行的断行位置估算是否与实际绘制宽度一致再补充一些文字",
                // CJK with fullwidth punctuation, similar to chat prose
                "而应该是这样的，建议你在输入框里粘贴一大段中文，然后观察最右侧的字符是否被遮挡了一半。",
                // Mixed CJK + ASCII
                "中文与English混排的情况test一下wrap行为123，看看结果如何。",
            ];

            for &size in &[px(14.), px(16.)] {
                for &wrap_width in &[px(200.), px(300.), px(420.)] {
                    for (si, text) in samples.iter().enumerate() {
                        let mut wrapper = app_ts.line_wrapper(ui_font.clone(), size);
                        let mut ranges: Vec<(usize, usize)> = Vec::new();
                        let mut prev = 0usize;
                        for b in wrapper.wrap_line(&[LineFragment::text(text)], wrap_width) {
                            ranges.push((prev, b.ix));
                            prev = b.ix;
                        }
                        ranges.push((prev, text.len()));
                        drop(wrapper);

                        for (i, (s, e)) in ranges.iter().enumerate() {
                            let seg = &text[*s..*e];
                            // Estimate the way LineWrapper accumulates widths.
                            let est: Pixels = seg
                                .chars()
                                .map(|c| app_ts.layout_width(font_id, size, c))
                                .fold(px(0.), |a, w| a + w);
                            let shaped = window.text_system().shape_line(
                                seg.to_string().into(),
                                size,
                                &[TextRun {
                                    len: seg.len(),
                                    font: ui_font.clone(),
                                    color: black(),
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                }],
                                None,
                            );
                            let over = shaped.width - wrap_width;
                            println!(
                                "size={:>4} wrap={:>5} sample={} seg={} est={:>8.2?} shaped={:>8.2?} overflow={:>7.2?} {}",
                                format!("{:?}", size),
                                format!("{:?}", wrap_width),
                                si,
                                i,
                                est,
                                shaped.width,
                                over,
                                if shaped.width > wrap_width { "<-- OVERFLOW" } else { "" },
                            );
                        }
                    }
                }
            }

            // Phase 2: the FIXED pipeline (as patched gpui-component now works) —
            // boundaries from shape_text's shaped glyph positions, then each
            // segment re-shaped standalone exactly the way element.rs paints it.
            println!("==== fixed pipeline (shape_text boundaries) ====");
            let mut bad = 0usize;
            for &size in &[px(14.), px(16.)] {
                for &wrap_width in &[px(200.), px(300.), px(420.)] {
                    for (si, text) in samples.iter().enumerate() {
                        let run = TextRun {
                            len: text.len(),
                            font: ui_font.clone(),
                            color: black(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let wrapped = window
                            .text_system()
                            .shape_text(
                                (*text).to_string().into(),
                                size,
                                &[run],
                                Some(wrap_width),
                                None,
                            )
                            .expect("shape_text");
                        let line = wrapped.first().expect("one line");
                        let mut ranges: Vec<(usize, usize)> = Vec::new();
                        let mut prev = 0usize;
                        for b in line.wrap_boundaries.iter() {
                            let ix = line.unwrapped_layout.runs[b.run_ix].glyphs[b.glyph_ix].index;
                            ranges.push((prev, ix));
                            prev = ix;
                        }
                        ranges.push((prev, text.len()));

                        for (i, (s, e)) in ranges.iter().enumerate() {
                            let seg = &text[*s..*e];
                            let shaped = window.text_system().shape_line(
                                seg.to_string().into(),
                                size,
                                &[TextRun {
                                    len: seg.len(),
                                    font: ui_font.clone(),
                                    color: black(),
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                }],
                                None,
                            );
                            let over = shaped.width - wrap_width;
                            if shaped.width > wrap_width {
                                bad += 1;
                            }
                            println!(
                                "size={:>4} wrap={:>5} sample={} seg={} shaped={:>8.2?} overflow={:>7.2?} {}",
                                format!("{:?}", size),
                                format!("{:?}", wrap_width),
                                si,
                                i,
                                shaped.width,
                                over,
                                if shaped.width > wrap_width { "<-- OVERFLOW" } else { "" },
                            );
                        }
                    }
                }
            }
            println!("==== fixed pipeline overflow count: {} ====", bad);
            }

            std::process::exit(0);
            #[allow(unreachable_code)]
            cx.new(|_| Probe)
        })
        .expect("open probe window");
    });
}
