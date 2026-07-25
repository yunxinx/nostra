//! GitHub-style "soft dark" palette applied on top of gpui-component's default
//! dark theme.  Runs after `Theme::change`, so the token cache is rebuilt.

use gpui::{App, rgb};
use gpui_component::{ActiveTheme, Theme, theme::ThemeTokens};

pub fn apply_dark_palette(cx: &mut App) {
    if !cx.theme().mode.is_dark() {
        return;
    }

    let theme = Theme::global_mut(cx);
    let c = &mut theme.colors;

    // Canvas / surfaces
    c.background = rgb(0x22272e).into();
    c.foreground = rgb(0xadbac7).into();
    c.overlay = rgb(0x2d333b).into();
    c.popover = rgb(0x2d333b).into();
    c.popover_foreground = rgb(0xadbac7).into();

    // Borders & muted surfaces
    c.border = rgb(0x444c56).into();
    c.muted = rgb(0x2d333b).into();
    c.muted_foreground = rgb(0x768390).into();
    c.accent = rgb(0x373e47).into();
    c.accent_foreground = rgb(0xadbac7).into();

    // Sidebar
    c.sidebar = rgb(0x2d333b).into();
    c.sidebar_foreground = rgb(0xadbac7).into();
    c.sidebar_accent = rgb(0x373e47).into();
    c.sidebar_accent_foreground = rgb(0xcdd9e5).into();
    c.sidebar_border = rgb(0x444c56).into();
    c.sidebar_primary = rgb(0x316dca).into();
    c.sidebar_primary_foreground = rgb(0xcdd9e5).into();

    // Primary (accent blue)
    c.primary = rgb(0x316dca).into();
    c.primary_hover = rgb(0x386bc0).into();
    c.primary_active = rgb(0x2d5aab).into();
    c.primary_foreground = rgb(0xcdd9e5).into();

    // Secondary
    c.secondary = rgb(0x373e47).into();
    c.secondary_hover = rgb(0x444c56).into();
    c.secondary_active = rgb(0x4b5460).into();
    c.secondary_foreground = rgb(0xadbac7).into();

    // Button neutrals
    c.button = rgb(0x373e47).into();
    c.button_hover = rgb(0x444c56).into();
    c.button_active = rgb(0x4b5460).into();
    c.button_foreground = rgb(0xadbac7).into();
    c.button_secondary = rgb(0x373e47).into();
    c.button_secondary_hover = rgb(0x444c56).into();
    c.button_secondary_active = rgb(0x4b5460).into();
    c.button_secondary_foreground = rgb(0xadbac7).into();
    c.button_primary = rgb(0x316dca).into();
    c.button_primary_hover = rgb(0x386bc0).into();
    c.button_primary_active = rgb(0x2d5aab).into();
    c.button_primary_foreground = rgb(0xcdd9e5).into();

    // Input
    c.input = rgb(0x373e47).into();

    // Selection / focus
    c.selection = rgb(0x2e4a70).into();
    c.ring = rgb(0x316dca).into();
    c.caret = rgb(0xadbac7).into();

    // Title/status bars
    c.title_bar = rgb(0x22272e).into();
    c.title_bar_border = rgb(0x2d333b).into();
    c.status_bar = rgb(0x22272e).into();
    c.status_bar_border = rgb(0x2d333b).into();

    // Scrollbar
    c.scrollbar = rgb(0x22272e).into();
    c.scrollbar_thumb = rgb(0x444c56).into();
    c.scrollbar_thumb_hover = rgb(0x4b5460).into();

    // List
    c.list = rgb(0x22272e).into();
    c.list_hover = rgb(0x2d333b).into();
    c.list_active = rgb(0x373e47).into();
    c.list_active_border = rgb(0x316dca).into();
    c.list_even = rgb(0x262c34).into();
    c.list_head = rgb(0x2d333b).into();

    // Regenerate cached tokens from mutated colors.
    theme.tokens = ThemeTokens::from(theme.colors);
}
