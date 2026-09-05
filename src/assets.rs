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
#[include = "themes/**/*.json"]
struct AppEmbed;

const BUNDLED_FONTS: &[(&str, &[u8])] = &[
    (
        "fonts/MapleMono-CN-Regular.ttf",
        include_bytes!("../assets/fonts/MapleMono-CN-Regular.ttf"),
    ),
    (
        "fonts/JetBrainsMono-Regular.ttf",
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    ),
];

/// Raw bytes of an app-embedded asset (e.g. a bundled font), by path
/// relative to the `assets/` folder.
pub fn embedded(path: &str) -> Option<Cow<'static, [u8]>> {
    if let Some((_, bytes)) = BUNDLED_FONTS
        .iter()
        .find(|(font_path, _)| *font_path == path)
    {
        return Some(Cow::Borrowed(bytes));
    }
    AppEmbed::get(path).map(|file| file.data)
}

/// Combined asset source: app-embedded first, gpui-component-assets second.
pub struct NostraAssets;

impl AssetSource for NostraAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }

        if let Some(file) = embedded(path) {
            return Ok(Some(file));
        }

        ComponentAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = AppEmbed::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect();
        names.extend(
            BUNDLED_FONTS
                .iter()
                .filter(|(font_path, _)| font_path.starts_with(path))
                .map(|(font_path, _)| (*font_path).into()),
        );

        if let Ok(more) = ComponentAssets.list(path) {
            names.extend(more);
        }

        names.sort();
        names.dedup();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    #[test]
    fn code_wrap_icon_is_embedded() {
        assert!(super::embedded("icons/wrap-text.svg").is_some());
    }

    /// App-owned icons are addressed by path, so a rename or a missed embed
    /// include shows up as a silently blank button rather than a build error.
    #[test]
    fn app_owned_icons_are_embedded() {
        for path in [
            "icons/square-pen.svg",
            "icons/wrap-text.svg",
            "icons/trash-2.svg",
            "icons/tool.svg",
        ] {
            assert!(super::embedded(path).is_some(), "missing embed: {path}");
        }
    }

    #[test]
    fn bundled_fonts_are_static_borrowed_bytes_in_every_profile() {
        for (path, _) in super::BUNDLED_FONTS {
            assert!(matches!(super::embedded(path), Some(Cow::Borrowed(_))));
        }
    }
}
