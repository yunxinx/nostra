//! Extract and cache color-emoji bitmap strikes (`sbix`, `CBDT`, ...).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use ttf_parser::{Face, RasterImageFormat};

const EMOJI_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
const CACHE_ENTRY_OVERHEAD: usize = 64;

#[derive(Debug, Clone)]
pub struct EmojiRasterStrike {
    pub format: RasterImageFormat,
    pub data: Arc<[u8]>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub pixels_per_em: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EmojiCacheKey {
    font_identity: u64,
    ch: char,
    strike: u16,
}

struct CacheEntry {
    value: Option<EmojiRasterStrike>,
    charge: usize,
    last_used: u64,
}

struct EmojiRasterCache {
    entries: HashMap<EmojiCacheKey, CacheEntry>,
    used_bytes: usize,
    clock: u64,
    max_bytes: usize,
}

impl EmojiRasterCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            used_bytes: 0,
            clock: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: EmojiCacheKey) -> Option<Option<EmojiRasterStrike>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = self.clock;
        Some(entry.value.clone())
    }

    fn insert(&mut self, key: EmojiCacheKey, value: Option<EmojiRasterStrike>) {
        self.clock = self.clock.wrapping_add(1);
        let charge = value.as_ref().map_or(CACHE_ENTRY_OVERHEAD, |strike| {
            CACHE_ENTRY_OVERHEAD.saturating_add(strike.data.len())
        });
        if let Some(old) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(old.charge);
        }
        if charge > self.max_bytes {
            return;
        }
        self.used_bytes = self.used_bytes.saturating_add(charge);
        self.entries.insert(
            key,
            CacheEntry {
                value,
                charge,
                last_used: self.clock,
            },
        );
        self.evict_to_budget(Some(key));
    }

    fn evict_to_budget(&mut self, protected: Option<EmojiCacheKey>) {
        while self.used_bytes > self.max_bytes && self.entries.len() > 1 {
            let victim = self
                .entries
                .iter()
                .filter(|(key, _)| Some(**key) != protected)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key);
            let Some(victim) = victim else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.used_bytes = self.used_bytes.saturating_sub(entry.charge);
            }
        }
    }
}

static EMOJI_CACHE: LazyLock<Mutex<EmojiRasterCache>> =
    LazyLock::new(|| Mutex::new(EmojiRasterCache::new(EMOJI_CACHE_MAX_BYTES)));

pub fn emoji_raster_for_char(ch: char, glyph_em_px: f32) -> Option<EmojiRasterStrike> {
    let font = super::load_emoji_font_arc()?;
    let strike = glyph_em_px.round().clamp(8.0, 256.0) as u16;
    let key = EmojiCacheKey {
        font_identity: font.identity(),
        ch,
        strike,
    };

    if let Some(cached) = EMOJI_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
    {
        return cached;
    }

    let value = extract_raster(&font, ch, strike);
    EMOJI_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, value.clone());
    value
}

fn extract_raster(font: &super::FontData, ch: char, strike: u16) -> Option<EmojiRasterStrike> {
    let face = Face::parse(font.as_bytes(), font.face_index()).ok()?;
    let gid = face.glyph_index(ch)?;
    let img = face
        .glyph_raster_image(gid, strike)
        .or_else(|| face.glyph_raster_image(gid, u16::MAX))?;
    Some(EmojiRasterStrike {
        format: img.format,
        data: Arc::from(img.data),
        x: img.x,
        y: img.y,
        width: img.width,
        height: img.height,
        pixels_per_em: img.pixels_per_em.max(1),
    })
}

pub fn emoji_png_raster_for_char(ch: char, glyph_em_px: f32) -> Option<EmojiRasterStrike> {
    let strike = emoji_raster_for_char(ch, glyph_em_px)?;
    matches!(strike.format, RasterImageFormat::PNG).then_some(strike)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ch: char) -> EmojiCacheKey {
        EmojiCacheKey {
            font_identity: 1,
            ch,
            strike: 16,
        }
    }

    fn strike(size: usize) -> EmojiRasterStrike {
        EmojiRasterStrike {
            format: RasterImageFormat::PNG,
            data: Arc::from(vec![0; size]),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            pixels_per_em: 16,
        }
    }

    #[test]
    fn cache_evicts_oldest_entry_to_stay_within_byte_budget() {
        let mut cache = EmojiRasterCache::new(200);
        cache.insert(key('a'), Some(strike(80)));
        cache.insert(key('b'), Some(strike(80)));

        assert!(cache.get(key('a')).is_none());
        assert!(cache.get(key('b')).is_some());
        assert!(cache.used_bytes <= cache.max_bytes);
    }

    #[test]
    fn negative_results_are_cached_and_charged() {
        let mut cache = EmojiRasterCache::new(128);
        cache.insert(key('a'), None);
        assert!(matches!(cache.get(key('a')), Some(None)));
        assert_eq!(cache.used_bytes, CACHE_ENTRY_OVERHEAD);
    }

    #[test]
    fn entry_larger_than_the_budget_is_not_retained() {
        let mut cache = EmojiRasterCache::new(128);
        cache.insert(key('a'), Some(strike(129)));

        assert!(cache.get(key('a')).is_none());
        assert_eq!(cache.used_bytes, 0);
    }
}
