//! Asset source that layers app-owned SVGs on top of the default
//! `gpui-component-assets` bundle.
//!
//! Anything under `assets/icons/**/*.svg` in this crate is embedded at compile
//! time via `rust-embed`; requests that miss fall through to the component
//! crate's asset source.  This lets the app introduce new icons (like
//! `square-pen`) without forking the upstream asset bundle.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component_assets::Assets as ComponentAssets;

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
#[include = "fonts/**/*.ttf"]
struct AppEmbed;

/// Raw bytes of an app-embedded asset (e.g. a bundled font), by path
/// relative to the `assets/` folder.
pub fn embedded(path: &str) -> Option<Cow<'static, [u8]>> {
    AppEmbed::get(path).map(|file| file.data)
}

/// Combined asset source: app-embedded first, gpui-component-assets second.
pub struct NostraAssets;

impl AssetSource for NostraAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }

        if let Some(file) = AppEmbed::get(path) {
            return Ok(Some(file.data));
        }

        ComponentAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = AppEmbed::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect();

        if let Ok(more) = ComponentAssets.list(path) {
            names.extend(more);
        }

        names.sort();
        names.dedup();
        Ok(names)
    }
}
