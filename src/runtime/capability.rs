//! Typed capability identities and their internal erased form.

use std::{any::TypeId, fmt};

use super::component::has_supported_name_characters;

pub trait CapabilityKey: 'static {
    type Handle: Clone + 'static;

    const NAME: &'static str;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilityId {
    key_type: TypeId,
    name: &'static str,
}

impl CapabilityId {
    #[must_use]
    pub fn of<K: CapabilityKey>() -> Self {
        assert!(!K::NAME.is_empty(), "capability name must not be empty");
        assert!(
            has_supported_name_characters(K::NAME),
            "capability name contains an unsupported character"
        );
        Self {
            key_type: TypeId::of::<K>(),
            name: K::NAME,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub fn is<K: CapabilityKey>(self) -> bool {
        self.key_type == TypeId::of::<K>()
    }
}

impl fmt::Debug for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CapabilityId").field(&self.name).finish()
    }
}
