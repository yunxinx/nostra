//! Byte-bounded glyph outline cache shared by standalone renderers.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Arc, LazyLock, Mutex};

use ab_glyph::{Font, FontRef, GlyphId, OutlineCurve, VariableFont};
use ratex_font::FontId;

const OUTLINE_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;
const CACHE_ENTRY_OVERHEAD: usize = 64;

type OutlineData = Arc<[OutlineCurve]>;
type OutlineKey = (u64, FontId, GlyphId);

struct CacheEntry {
    curves: OutlineData,
    charge: usize,
    last_used: u64,
}

struct OutlineCache {
    entries: HashMap<OutlineKey, CacheEntry>,
    used_bytes: usize,
    clock: u64,
    max_bytes: usize,
}

impl OutlineCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            used_bytes: 0,
            clock: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: &OutlineKey) -> Option<OutlineData> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(Arc::clone(&entry.curves))
    }

    fn insert(&mut self, key: OutlineKey, curves: OutlineData) -> OutlineData {
        if let Some(existing) = self.get(&key) {
            return existing;
        }
        self.clock = self.clock.wrapping_add(1);
        let charge = CACHE_ENTRY_OVERHEAD
            .saturating_add(curves.len().saturating_mul(size_of::<OutlineCurve>()));
        if charge > self.max_bytes {
            return curves;
        }
        self.used_bytes = self.used_bytes.saturating_add(charge);
        self.entries.insert(
            key,
            CacheEntry {
                curves: Arc::clone(&curves),
                charge,
                last_used: self.clock,
            },
        );
        self.evict_to_budget(Some(key));
        curves
    }

    fn evict_to_budget(&mut self, protected: Option<OutlineKey>) {
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

static OUTLINE_CACHE: LazyLock<Mutex<OutlineCache>> =
    LazyLock::new(|| Mutex::new(OutlineCache::new(OUTLINE_CACHE_MAX_BYTES)));

pub fn get_or_compute_outline(
    font_identity: u64,
    font_id: FontId,
    font: &FontRef<'_>,
    glyph_id: GlyphId,
) -> Option<Arc<[OutlineCurve]>> {
    let key = (font_identity, font_id, glyph_id);
    if let Some(cached) = OUTLINE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return Some(cached);
    }

    let needs_variation = font.variations().iter().any(|axis| &axis.tag == b"wght");
    let outline = if needs_variation {
        let mut instance = font.clone();
        for axis in instance.variations() {
            if &axis.tag == b"wght" {
                let weight = if axis.min_value <= 400.0 && 400.0 <= axis.max_value {
                    400.0
                } else {
                    axis.default_value
                };
                instance.set_variation(b"wght", weight);
                break;
            }
        }
        instance.outline(glyph_id)?
    } else {
        font.outline(glyph_id)?
    };
    let curves: Arc<[OutlineCurve]> = outline.curves.into();
    Some(
        OUTLINE_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, curves),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ab_glyph::point;

    fn curves(count: usize) -> OutlineData {
        vec![OutlineCurve::Line(point(0.0, 0.0), point(1.0, 1.0)); count].into()
    }

    #[test]
    fn cache_evicts_oldest_outline_to_stay_within_budget() {
        let one_curve_charge = CACHE_ENTRY_OVERHEAD + size_of::<OutlineCurve>();
        let mut cache = OutlineCache::new(one_curve_charge * 2);
        let first = (1, FontId::MainRegular, GlyphId(1));
        let second = (1, FontId::MainRegular, GlyphId(2));
        let third = (1, FontId::MainRegular, GlyphId(3));
        cache.insert(first, curves(1));
        cache.insert(second, curves(1));
        cache.insert(third, curves(1));

        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_some());
        assert!(cache.get(&third).is_some());
        assert!(cache.used_bytes <= cache.max_bytes);
    }

    #[test]
    fn font_identity_is_part_of_the_cache_key() {
        let mut cache = OutlineCache::new(1024);
        let first = (1, FontId::CjkRegular, GlyphId(7));
        let second = (2, FontId::CjkRegular, GlyphId(7));
        cache.insert(first, curves(1));
        cache.insert(second, curves(2));
        assert_ne!(
            cache.get(&first).unwrap().len(),
            cache.get(&second).unwrap().len()
        );
    }

    #[test]
    fn entry_larger_than_the_budget_is_not_retained() {
        let mut cache = OutlineCache::new(CACHE_ENTRY_OVERHEAD);
        let key = (1, FontId::MainRegular, GlyphId(1));
        let outline = curves(1);

        assert_eq!(cache.insert(key, Arc::clone(&outline)).len(), outline.len());
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.used_bytes, 0);
    }
}
