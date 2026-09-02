//! Stable component, scope, and activation-generation identities.

use std::{fmt, num::NonZeroU64};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(&'static str);

impl ComponentId {
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(!value.is_empty(), "component ID must not be empty");
        assert!(
            has_supported_name_characters(value),
            "component ID contains an unsupported character"
        );
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Debug for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ComponentId").field(&self.0).finish()
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(u64);

impl ScopeId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentGeneration(NonZeroU64);

impl ComponentGeneration {
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    #[must_use]
    pub(crate) const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }
}

pub(super) const fn has_supported_name_characters(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_') {
            return false;
        }
        index += 1;
    }
    true
}
