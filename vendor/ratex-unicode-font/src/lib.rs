//! Discover system Unicode fonts without retaining anonymous copies of stable files.

mod emoji_raster;

pub use emoji_raster::{EmojiRasterStrike, emoji_png_raster_for_char, emoji_raster_for_char};

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use system_fonts::{FontStyle, FoundFontSource, find_for_system_locale};

type SharedFontBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontStorage {
    Embedded,
    Owned,
    Mapped,
}

#[derive(Clone)]
pub struct FontData {
    bytes: SharedFontBytes,
    face_index: u32,
    identity: u64,
    storage: FontStorage,
}

impl std::fmt::Debug for FontData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontData")
            .field("len", &self.as_bytes().len())
            .field("face_index", &self.face_index)
            .field("identity", &self.identity)
            .field("storage", &self.storage)
            .finish()
    }
}

impl FontData {
    pub fn from_static(bytes: &'static [u8], source: &str, face_index: u32) -> Self {
        Self {
            bytes: Arc::new(StaticFontBytes(bytes)),
            face_index,
            identity: font_identity(source, face_index),
            storage: FontStorage::Embedded,
        }
    }

    pub fn from_owned(bytes: Vec<u8>, source: &str, face_index: u32) -> Self {
        Self {
            bytes: Arc::new(bytes),
            face_index,
            identity: font_identity(source, face_index),
            storage: FontStorage::Owned,
        }
    }

    fn from_shared(
        bytes: SharedFontBytes,
        source: &str,
        face_index: u32,
        storage: FontStorage,
    ) -> Self {
        Self {
            bytes,
            face_index,
            identity: font_identity(source, face_index),
            storage,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref().as_ref()
    }

    pub fn face_index(&self) -> u32 {
        self.face_index
    }

    pub fn identity(&self) -> u64 {
        self.identity
    }

    pub fn storage(&self) -> FontStorage {
        self.storage
    }
}

struct StaticFontBytes(&'static [u8]);

impl AsRef<[u8]> for StaticFontBytes {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

fn font_identity(source: &str, face_index: u32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    face_index.hash(&mut hasher);
    hasher.finish()
}

static UNICODE_FONT: OnceLock<Option<FontData>> = OnceLock::new();
static SYSTEM_FALLBACK_FONT: OnceLock<Option<FontData>> = OnceLock::new();
static EMOJI_FONT: OnceLock<Option<FontData>> = OnceLock::new();

// Different fallback roles commonly resolve to the same file. Keep one mapping
// per canonical path so they share both VM metadata and clean file-backed pages.
static MAPPED_FILES: LazyLock<Mutex<HashMap<PathBuf, SharedFontBytes>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn load_unicode_font_arc() -> Option<FontData> {
    UNICODE_FONT.get_or_init(load_unicode_fallback_font).clone()
}

pub fn unicode_font_face_index() -> Option<u32> {
    UNICODE_FONT
        .get_or_init(load_unicode_fallback_font)
        .as_ref()
        .map(FontData::face_index)
}

pub fn load_fallback_font_arc() -> Option<FontData> {
    SYSTEM_FALLBACK_FONT
        .get_or_init(discover_system_font)
        .clone()
}

pub fn fallback_font_face_index() -> Option<u32> {
    SYSTEM_FALLBACK_FONT
        .get_or_init(discover_system_font)
        .as_ref()
        .map(FontData::face_index)
}

pub fn load_emoji_font_arc() -> Option<FontData> {
    EMOJI_FONT.get_or_init(discover_emoji_font).clone()
}

pub fn emoji_font_face_index() -> Option<u32> {
    EMOJI_FONT
        .get_or_init(discover_emoji_font)
        .as_ref()
        .map(FontData::face_index)
}

fn is_sfnt_single_font(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && (bytes[..4] == [0x00, 0x01, 0x00, 0x00]
            || bytes[..4] == [0x4F, 0x54, 0x54, 0x4F]
            || bytes[..4] == [0x74, 0x72, 0x75, 0x65])
}

fn is_sfnt_container(bytes: &[u8]) -> bool {
    is_sfnt_single_font(bytes) || bytes.get(0..4) == Some(b"ttcf")
}

fn load_unicode_fallback_font() -> Option<FontData> {
    if let Ok(spec) = std::env::var("RATEX_UNICODE_FONT") {
        if let Some(font) = load_font_spec(&spec, FontStorage::Owned) {
            eprintln!("[ratex-unicode-font] loaded from RATEX_UNICODE_FONT: {spec}");
            return Some(font);
        }
    }
    discover_system_font()
}

fn discover_system_font() -> Option<FontData> {
    #[rustfmt::skip]
    let candidates: &[&str] = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc#Noto Sans CJK SC",
        "/Library/Fonts/Arial Unicode.ttf",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "C:\\Windows\\Fonts\\NotoSansSC-VF.ttf",
        "C:\\Windows\\Fonts\\msyh.ttc#Microsoft YaHei",
    ];

    for &spec in candidates {
        if let Some(font) = load_font_spec(spec, FontStorage::Mapped) {
            eprintln!("[ratex-unicode-font] found via builtin path: {spec}");
            return Some(font);
        }
    }

    let (_locale, region, fonts) = find_for_system_locale(FontStyle::Sans);
    for found in fonts {
        let FoundFontSource::Path(path) = found.source else {
            continue;
        };
        let spec = if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ttc"))
        {
            format!("{}#{}", path.display(), found.family)
        } else {
            path.display().to_string()
        };
        if let Some(font) = load_font_spec(&spec, FontStorage::Mapped) {
            eprintln!("[ratex-unicode-font] found via system-fonts: {spec} ({region:?})");
            return Some(font);
        }
    }

    eprintln!("[ratex-unicode-font] no Unicode font found");
    None
}

enum FaceSelector<'a> {
    Index(u32),
    Family(&'a str),
}

fn split_font_spec(spec: &str) -> (&str, Option<FaceSelector<'_>>) {
    if let Some((path, suffix)) = spec.rsplit_once('#') {
        if path.is_empty() || suffix.is_empty() {
            return (spec, None);
        }
        if let Ok(index) = suffix.parse::<u32>() {
            return (path, Some(FaceSelector::Index(index)));
        }
        return (path, Some(FaceSelector::Family(suffix)));
    }
    (spec, None)
}

fn load_font_spec(spec: &str, storage: FontStorage) -> Option<FontData> {
    let (path, selector) = split_font_spec(spec);
    let path = Path::new(path);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut db = fontdb::Database::new();

    match storage {
        FontStorage::Mapped => db.load_font_file(&canonical).ok()?,
        FontStorage::Owned => db.load_font_data(std::fs::read(&canonical).ok()?),
        FontStorage::Embedded => return None,
    }

    let selected = match selector {
        None => db.faces().find(|face| face.index == 0).map(|face| face.id),
        Some(FaceSelector::Index(index)) => db
            .faces()
            .find(|face| face.index == index)
            .map(|face| face.id),
        Some(FaceSelector::Family(family)) => {
            let first = db.faces().next()?.id;
            if db
                .with_face_data(first, |bytes, _| is_sfnt_single_font(bytes))
                .unwrap_or(false)
            {
                return None;
            }
            db.faces()
                .find(|face| face.families.iter().any(|(name, _)| name == family))
                .map(|face| face.id)
        }
    }?;

    font_data_from_database(&mut db, selected, &canonical, storage)
}

fn font_data_from_database(
    db: &mut fontdb::Database,
    id: fontdb::ID,
    path: &Path,
    storage: FontStorage,
) -> Option<FontData> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let source = canonical.to_string_lossy();
    let face_index = db.face(id)?.index;

    let bytes = if storage == FontStorage::Mapped {
        let mut mappings = MAPPED_FILES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(bytes) = mappings.get(&canonical) {
            Arc::clone(bytes)
        } else {
            // SAFETY: this path is used only for OS-managed font files. They are
            // opened read-only and expected not to be truncated while the app is
            // running. User-provided RATEX_UNICODE_FONT files use Owned storage.
            let (bytes, _) = unsafe { db.make_shared_face_data(id) }?;
            mappings.insert(canonical.clone(), Arc::clone(&bytes));
            bytes
        }
    } else {
        // Binary fontdb sources are already shared Arc-backed owned bytes.
        let (bytes, _) = unsafe { db.make_shared_face_data(id) }?;
        bytes
    };

    if !is_sfnt_container(bytes.as_ref().as_ref()) {
        return None;
    }
    Some(FontData::from_shared(bytes, &source, face_index, storage))
}

fn discover_emoji_font() -> Option<FontData> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    #[cfg(target_os = "macos")]
    let emoji_families: &[&str] = &["Apple Color Emoji"];
    #[cfg(target_os = "linux")]
    let emoji_families: &[&str] = &["Noto Color Emoji", "Noto Emoji"];
    #[cfg(target_os = "windows")]
    let emoji_families: &[&str] = &["Segoe UI Emoji"];
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let emoji_families: &[&str] = &[];

    for family in emoji_families {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        let Some(id) = db.query(&query) else {
            continue;
        };
        let path = match &db.face(id)?.source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => path.clone(),
            fontdb::Source::Binary(_) => continue,
        };
        if let Some(font) = font_data_from_database(&mut db, id, &path, FontStorage::Mapped) {
            return Some(font);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_font_data_keeps_identity_and_bytes() {
        let font = FontData::from_owned(vec![1, 2, 3], "fixture", 2);
        assert_eq!(font.as_bytes(), &[1, 2, 3]);
        assert_eq!(font.face_index(), 2);
        assert_eq!(font.storage(), FontStorage::Owned);
        assert_eq!(font.identity(), font_identity("fixture", 2));
    }

    #[test]
    fn concurrent_first_load_returns_one_shared_font_mapping() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(load_unicode_font_arc))
            .collect();
        let fonts: Vec<_> = handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("font loader thread"))
            .collect();

        if let Some(first) = fonts.first() {
            assert!(fonts.iter().all(|font| {
                font.identity == first.identity && Arc::ptr_eq(&font.bytes, &first.bytes)
            }));
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_load_font_spec_macos() {
        let ttf = "/Library/Fonts/Arial Unicode.ttf";
        if Path::new(ttf).exists() {
            let result = load_font_spec(ttf, FontStorage::Mapped);
            assert!(result.is_some(), "Should load Arial Unicode.ttf");
            if let Some(font) = result {
                assert!(!font.as_bytes().is_empty());
                assert_eq!(font.face_index(), 0);
                assert_eq!(font.storage(), FontStorage::Mapped);
            }

            let first = load_font_spec(ttf, FontStorage::Mapped).expect("first mapping");
            let second = load_font_spec(ttf, FontStorage::Mapped).expect("second mapping");
            assert!(Arc::ptr_eq(&first.bytes, &second.bytes));

            let result = load_font_spec(&format!("{ttf}#0"), FontStorage::Mapped);
            assert_eq!(result.map(|font| font.face_index()), Some(0));
            assert!(load_font_spec(&format!("{ttf}#1"), FontStorage::Mapped).is_none());
            assert!(
                load_font_spec(&format!("{ttf}#Arial Unicode MS"), FontStorage::Mapped).is_none()
            );

            let owned = load_font_spec(ttf, FontStorage::Owned).expect("owned font");
            assert_eq!(owned.storage(), FontStorage::Owned);
        }

        let ttc = "/System/Library/Fonts/PingFang.ttc";
        if Path::new(ttc).exists() {
            let family = load_font_spec(&format!("{ttc}#PingFang SC"), FontStorage::Mapped)
                .expect("PingFang SC face");
            let default = load_font_spec(ttc, FontStorage::Mapped).expect("default face");
            assert_eq!(default.face_index(), 0);
            let by_index = load_font_spec(
                &format!("{ttc}#{}", family.face_index()),
                FontStorage::Mapped,
            )
            .expect("face by index");
            assert_eq!(family.face_index(), by_index.face_index());
            assert!(
                load_font_spec(&format!("{ttc}#NonExistent Font"), FontStorage::Mapped).is_none()
            );
        }
    }
}
