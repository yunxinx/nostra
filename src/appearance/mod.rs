//! Visual appearance subsystems and their persisted settings.
//!
//! Each submodule owns one axis of how the app looks, and each is the single
//! place its axis is mutated: [`theme`] wraps gpui-component's `ThemeRegistry`
//! (bundled themes, light/dark slots, mode), [`fonts`] registers the bundled
//! composer fonts and tracks the active one, and [`glass`] owns the macOS
//! native glass appearance and its tint.
//!
//! Views read the resulting colors through `cx.theme()` and never reach into
//! these modules' state directly.

pub(crate) mod fonts;
pub(crate) mod glass;
pub(crate) mod theme;
