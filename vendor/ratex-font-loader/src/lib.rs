use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use ratex_font::FontId;
use ratex_types::display_item::DisplayItem;
use ratex_unicode_font::FontData;

pub mod outline_cache;

type CachedFont = Result<Option<FontData>, String>;
type CacheCell = Arc<OnceLock<CachedFont>>;

const FONT_MAP: &[(FontId, &str)] = &[
    (FontId::MainRegular, "KaTeX_Main-Regular.ttf"),
    (FontId::MainBold, "KaTeX_Main-Bold.ttf"),
    (FontId::MainItalic, "KaTeX_Main-Italic.ttf"),
    (FontId::MainBoldItalic, "KaTeX_Main-BoldItalic.ttf"),
    (FontId::MathItalic, "KaTeX_Math-Italic.ttf"),
    (FontId::MathBoldItalic, "KaTeX_Math-BoldItalic.ttf"),
    (FontId::AmsRegular, "KaTeX_AMS-Regular.ttf"),
    (FontId::CaligraphicRegular, "KaTeX_Caligraphic-Regular.ttf"),
    (FontId::FrakturRegular, "KaTeX_Fraktur-Regular.ttf"),
    (FontId::FrakturBold, "KaTeX_Fraktur-Bold.ttf"),
    (FontId::SansSerifRegular, "KaTeX_SansSerif-Regular.ttf"),
    (FontId::SansSerifBold, "KaTeX_SansSerif-Bold.ttf"),
    (FontId::SansSerifItalic, "KaTeX_SansSerif-Italic.ttf"),
    (FontId::ScriptRegular, "KaTeX_Script-Regular.ttf"),
    (FontId::TypewriterRegular, "KaTeX_Typewriter-Regular.ttf"),
    (FontId::Size1Regular, "KaTeX_Size1-Regular.ttf"),
    (FontId::Size2Regular, "KaTeX_Size2-Regular.ttf"),
    (FontId::Size3Regular, "KaTeX_Size3-Regular.ttf"),
    (FontId::Size4Regular, "KaTeX_Size4-Regular.ttf"),
];

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FontSourceKey {
    Embedded,
    Directory(PathBuf),
    SystemUnicode,
    SystemFallback,
    SystemEmoji,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    source: FontSourceKey,
    font_id: FontId,
}

#[derive(Debug, Clone)]
pub struct FontSet {
    font_dir: String,
    fonts: HashMap<FontId, FontData>,
    missing: HashSet<FontId>,
}

impl FontSet {
    fn new(font_dir: &str) -> Self {
        Self {
            font_dir: font_dir.to_string(),
            fonts: HashMap::new(),
            missing: HashSet::new(),
        }
    }

    pub fn get(&self, id: &FontId) -> Option<&[u8]> {
        self.fonts.get(id).map(FontData::as_bytes)
    }

    pub fn get_data(&self, id: &FontId) -> Option<&FontData> {
        self.fonts.get(id)
    }

    pub fn contains_key(&self, id: &FontId) -> bool {
        self.fonts.contains_key(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FontId, &[u8])> {
        self.fonts.iter().map(|(id, font)| (id, font.as_bytes()))
    }

    pub fn ensure(&mut self, id: FontId) -> Result<Option<&FontData>, String> {
        if self.fonts.contains_key(&id) {
            return Ok(self.fonts.get(&id));
        }
        if self.missing.contains(&id) {
            return Ok(None);
        }

        match cached_font(&self.font_dir, id)? {
            Some(font) => {
                self.fonts.insert(id, font);
                Ok(self.fonts.get(&id))
            }
            None => {
                self.missing.insert(id);
                Ok(None)
            }
        }
    }
}

impl From<HashMap<FontId, Vec<u8>>> for FontSet {
    fn from(fonts: HashMap<FontId, Vec<u8>>) -> Self {
        Self {
            font_dir: String::new(),
            fonts: fonts
                .into_iter()
                .map(|(id, bytes)| {
                    let source = format!("provided:{}", id.as_str());
                    (id, FontData::from_owned(bytes, &source, 0))
                })
                .collect(),
            missing: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FontLoadPlan {
    required: HashSet<FontId>,
}

impl FontLoadPlan {
    pub fn for_display_items(items: &[DisplayItem]) -> Self {
        let mut required = HashSet::new();
        for item in items {
            if let DisplayItem::GlyphPath { font, .. } = item {
                if let Some(font_id) = FontId::parse(font) {
                    required.insert(font_id);
                }
            }
        }
        required.insert(FontId::MainRegular);
        Self { required }
    }

    pub fn required(&self) -> &HashSet<FontId> {
        &self.required
    }

    pub fn all(&self) -> HashSet<FontId> {
        self.required.clone()
    }
}

static FONT_CACHE: OnceLock<RwLock<HashMap<CacheKey, CacheCell>>> = OnceLock::new();

fn cache() -> &'static RwLock<HashMap<CacheKey, CacheCell>> {
    FONT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn cache_cell(key: CacheKey) -> Result<CacheCell, String> {
    if let Some(cell) = cache()
        .read()
        .map_err(|_| "font cache poisoned".to_string())?
        .get(&key)
        .cloned()
    {
        return Ok(cell);
    }

    let mut cache = cache()
        .write()
        .map_err(|_| "font cache poisoned".to_string())?;
    Ok(Arc::clone(
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(OnceLock::new())),
    ))
}

fn cached_font(font_dir: &str, font_id: FontId) -> CachedFont {
    let key = cache_key(font_dir, font_id);
    let cell = cache_cell(key)?;
    cell.get_or_init(|| load_font_data(font_dir, font_id))
        .clone()
}

pub fn load_fonts_for_items(font_dir: &str, items: &[DisplayItem]) -> Result<FontSet, String> {
    let plan = FontLoadPlan::for_display_items(items);
    load_fonts_for_plan(font_dir, &plan)
}

pub fn load_fonts_for_plan(font_dir: &str, plan: &FontLoadPlan) -> Result<FontSet, String> {
    let mut fonts = FontSet::new(font_dir);
    for &font_id in plan.required() {
        if fonts.ensure(font_id)?.is_none() {
            return Err(format!("Missing required font {}", font_id.as_str()));
        }
    }
    Ok(fonts)
}

fn cache_key(font_dir: &str, font_id: FontId) -> CacheKey {
    CacheKey {
        source: source_key(font_dir, font_id),
        font_id,
    }
}

fn source_key(font_dir: &str, font_id: FontId) -> FontSourceKey {
    match font_id {
        FontId::CjkRegular => FontSourceKey::SystemUnicode,
        FontId::CjkFallback => FontSourceKey::SystemFallback,
        FontId::EmojiFallback => FontSourceKey::SystemEmoji,
        _ => katex_source_key(font_dir),
    }
}

#[cfg(feature = "embed-fonts")]
fn katex_source_key(_font_dir: &str) -> FontSourceKey {
    FontSourceKey::Embedded
}

#[cfg(not(feature = "embed-fonts"))]
fn katex_source_key(font_dir: &str) -> FontSourceKey {
    FontSourceKey::Directory(normalize_font_dir(font_dir))
}

#[cfg(not(feature = "embed-fonts"))]
fn normalize_font_dir(font_dir: &str) -> PathBuf {
    let path = std::path::Path::new(font_dir);
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn load_font_data(font_dir: &str, font_id: FontId) -> Result<Option<FontData>, String> {
    match font_id {
        FontId::CjkRegular => Ok(ratex_unicode_font::load_unicode_font_arc()),
        FontId::CjkFallback => Ok(ratex_unicode_font::load_fallback_font_arc()),
        FontId::EmojiFallback => Ok(ratex_unicode_font::load_emoji_font_arc()),
        _ => load_katex_font(font_dir, font_id),
    }
}

#[cfg(not(feature = "embed-fonts"))]
fn load_katex_font(font_dir: &str, font_id: FontId) -> Result<Option<FontData>, String> {
    let Some(filename) = FONT_MAP
        .iter()
        .find(|(id, _)| *id == font_id)
        .map(|(_, filename)| *filename)
    else {
        return Ok(None);
    };
    let path = std::path::Path::new(font_dir).join(filename);
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read(&path)
        .map(|bytes| Some(FontData::from_owned(bytes, &path.to_string_lossy(), 0)))
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))
}

#[cfg(feature = "embed-fonts")]
fn load_katex_font(_font_dir: &str, font_id: FontId) -> Result<Option<FontData>, String> {
    let Some(filename) = FONT_MAP
        .iter()
        .find(|(id, _)| *id == font_id)
        .map(|(_, filename)| *filename)
    else {
        return Ok(None);
    };
    Ok(
        ratex_katex_fonts::ttf_bytes(filename).map(|bytes| match bytes {
            std::borrow::Cow::Borrowed(bytes) => FontData::from_static(bytes, filename, 0),
            std::borrow::Cow::Owned(bytes) => FontData::from_owned(bytes, filename, 0),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratex_types::color::Color;

    fn glyph(font: FontId, char_code: u32) -> DisplayItem {
        DisplayItem::GlyphPath {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            font: font.as_str().to_string(),
            char_code,
            color: Color::BLACK,
        }
    }

    #[test]
    fn ascii_katex_glyph_does_not_request_unicode_fallbacks() {
        let plan = FontLoadPlan::for_display_items(&[glyph(FontId::MainRegular, 'x' as u32)]);
        assert_eq!(plan.required.len(), 1);
        assert!(plan.required.contains(&FontId::MainRegular));
    }

    #[test]
    fn non_ascii_without_katex_metrics_does_not_preload_unicode_fallbacks() {
        let plan = FontLoadPlan::for_display_items(&[glyph(FontId::MainRegular, '⌘' as u32)]);
        assert_eq!(plan.required.len(), 1);
        assert!(plan.required.contains(&FontId::MainRegular));
    }

    #[test]
    fn explicit_cjk_glyph_requires_only_primary_cjk_and_main() {
        let plan = FontLoadPlan::for_display_items(&[glyph(FontId::CjkRegular, '你' as u32)]);
        assert_eq!(plan.required.len(), 2);
        assert!(plan.required.contains(&FontId::CjkRegular));
        assert!(plan.required.contains(&FontId::MainRegular));
    }

    #[test]
    #[cfg(feature = "embed-fonts")]
    fn embedded_katex_font_keeps_rust_embed_storage_without_extra_copy() {
        let font = load_katex_font("", FontId::MainRegular)
            .expect("load result")
            .expect("embedded font");
        assert!(!font.as_bytes().is_empty());
        assert!(matches!(
            font.storage(),
            ratex_unicode_font::FontStorage::Embedded
                | ratex_unicode_font::FontStorage::Owned
        ));
    }

    #[test]
    fn cache_uses_one_initialization_cell_per_source_and_font() {
        let key = cache_key("", FontId::MainRegular);
        let first = cache_cell(key.clone()).expect("first cache cell");
        let second = cache_cell(key).expect("second cache cell");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    #[cfg(all(feature = "embed-fonts", target_os = "macos"))]
    fn explicit_cjk_plan_maps_cjk_without_loading_emoji() {
        let plan = FontLoadPlan::for_display_items(&[glyph(FontId::CjkRegular, '你' as u32)]);
        let fonts = load_fonts_for_plan("", &plan).expect("CJK fonts");

        assert!(fonts.contains_key(&FontId::MainRegular));
        assert!(fonts.contains_key(&FontId::CjkRegular));
        assert!(!fonts.contains_key(&FontId::EmojiFallback));
        assert_eq!(
            fonts.get_data(&FontId::CjkRegular).map(FontData::storage),
            Some(ratex_unicode_font::FontStorage::Mapped)
        );
    }
}
