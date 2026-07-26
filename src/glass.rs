//! macOS native glass appearance and its persisted application-wide setting.

use gpui::{App, Global, Hsla, Window, WindowBackgroundAppearance};

use crate::preferences;

#[cfg(target_os = "macos")]
pub const MIN_TINT_PERCENT: f32 = 20.;
#[cfg(target_os = "macos")]
pub const MAX_TINT_PERCENT: f32 = 95.;

#[cfg(target_os = "macos")]
struct GlassTintPreview(Option<f32>);

#[cfg(target_os = "macos")]
impl Global for GlassTintPreview {}

pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.set_global(GlassTintPreview(None));

    let _ = cx;
}

pub fn enabled(cx: &App) -> bool {
    preferences::get(cx).glass_effect
}

pub fn window_background(enabled: bool) -> WindowBackgroundAppearance {
    #[cfg(target_os = "macos")]
    if enabled {
        return WindowBackgroundAppearance::Blurred;
    }

    let _ = enabled;
    WindowBackgroundAppearance::Opaque
}

/// Preserve the theme hue while exposing the native backdrop on macOS.
pub fn background(color: Hsla, cx: &App) -> Hsla {
    #[cfg(target_os = "macos")]
    if enabled(cx) {
        return color.opacity(tint_opacity(cx));
    }

    let _ = cx;
    color
}

#[cfg(target_os = "macos")]
pub fn tint_opacity(cx: &App) -> f32 {
    let persisted = preferences::get(cx).glass_tint_opacity;
    let preview = cx
        .try_global::<GlassTintPreview>()
        .and_then(|preview| preview.0);
    resolve_tint_opacity(preview, persisted)
}

#[cfg(target_os = "macos")]
pub fn preview_tint_opacity(opacity: f32, cx: &mut App) {
    cx.global_mut::<GlassTintPreview>().0 = Some(clamp_opacity(opacity));
    cx.refresh_windows();
}

#[cfg(target_os = "macos")]
pub fn persist_tint_opacity(opacity: f32, cx: &mut App) {
    let opacity = clamp_opacity(opacity);
    cx.global_mut::<GlassTintPreview>().0 = None;
    preferences::update(cx, |prefs| prefs.glass_tint_opacity = opacity);
}

/// Commit an in-progress drag when the settings window closes before the
/// slider emits `Release`.
#[cfg(target_os = "macos")]
pub fn commit_tint_preview(cx: &mut App) {
    let preview = cx.global_mut::<GlassTintPreview>().0.take();
    if let Some(opacity) = preview {
        preferences::update(cx, |prefs| prefs.glass_tint_opacity = opacity);
    }
}

#[cfg(target_os = "macos")]
fn clamp_opacity(opacity: f32) -> f32 {
    opacity.clamp(MIN_TINT_PERCENT / 100., MAX_TINT_PERCENT / 100.)
}

#[cfg(target_os = "macos")]
fn resolve_tint_opacity(preview: Option<f32>, persisted: f32) -> f32 {
    clamp_opacity(preview.unwrap_or(persisted))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn tint_opacity_is_clamped_to_the_slider_range() {
        assert_eq!(clamp_opacity(0.), MIN_TINT_PERCENT / 100.);
        assert_eq!(clamp_opacity(0.75), 0.75);
        assert_eq!(clamp_opacity(1.), MAX_TINT_PERCENT / 100.);
    }

    #[test]
    fn preview_temporarily_overrides_the_persisted_opacity() {
        assert_eq!(resolve_tint_opacity(Some(0.6), 0.85), 0.6);
        assert_eq!(resolve_tint_opacity(None, 0.85), 0.85);
        assert_eq!(resolve_tint_opacity(None, 2.), MAX_TINT_PERCENT / 100.);
    }
}

/// The Root background is fixed when a window is created, so it must remain
/// transparent on macOS; the live content layers provide the opaque fallback.
#[cfg(target_os = "macos")]
pub fn root_background(_: Hsla) -> Hsla {
    gpui::transparent_black()
}

#[cfg(not(target_os = "macos"))]
pub fn root_background(color: Hsla) -> Hsla {
    color
}

/// Persist and apply the setting to every open window immediately.
#[cfg(target_os = "macos")]
pub fn set_enabled(enabled: bool, current_window: &mut Window, cx: &mut App) {
    preferences::update(cx, |prefs| prefs.glass_effect = enabled);

    let appearance = window_background(enabled);
    let current_id = current_window.window_handle().window_id();
    current_window.set_background_appearance(appearance);

    for handle in cx.windows() {
        if handle.window_id() != current_id {
            handle
                .update(cx, |_, window, _| {
                    window.set_background_appearance(appearance);
                })
                .ok();
        }
    }

    cx.refresh_windows();
}
