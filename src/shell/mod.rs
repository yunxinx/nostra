//! Application shell: the root view, its window, and the actions both use.
//!
//! Everything in this module is about hosting the app rather than about any one
//! feature. [`app`] owns the root [`ChatApp`](app::ChatApp) view (sidebar,
//! layout, conversation switching), [`window`] owns native window creation and
//! platform integration, and [`actions`] declares every app-level action plus
//! its keybinding so menus and global handlers can refer to one definition.

pub(crate) mod actions;
pub(crate) mod app;
pub(crate) mod window;
