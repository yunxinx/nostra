//! Typed Markdown extension contributions and body-local installation context.

use std::rc::Rc;

use gpui_component::text::MarkdownExtensions;

use crate::runtime::{
    CapabilityKey, ContributionDefinition, ContributionId, ContributionKey, ContributionSnapshot,
};

use super::PreferenceState;

pub(crate) const CJK_EMPHASIS_ID: ContributionId = ContributionId::new("nostra.markdown.cjk");
const CJK_EMPHASIS_ORDER: u32 = 10;
#[cfg(test)]
const TEST_EXTENSION_REVISION: u64 = 1;

pub(crate) struct MarkdownExtensionKey;

impl ContributionKey for MarkdownExtensionKey {
    type Value = MarkdownExtensionInstaller;

    const NAME: &'static str = "nostra.markdown.extensions";
}

impl CapabilityKey for MarkdownExtensionKey {
    type Handle = MarkdownExtensionSnapshot;

    const NAME: &'static str = <Self as ContributionKey>::NAME;
}

pub(crate) type MarkdownExtensionDefinition = ContributionDefinition<MarkdownExtensionKey>;

/// Immutable foreground projection of one contribution registry revision.
#[derive(Clone)]
pub(crate) struct MarkdownExtensionSnapshot {
    revision: u64,
    definitions: Rc<[MarkdownExtensionDefinition]>,
}

impl MarkdownExtensionSnapshot {
    pub(super) fn empty() -> Self {
        Self {
            revision: 0,
            definitions: Vec::new().into(),
        }
    }

    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg(test)]
    pub(crate) fn definitions(&self) -> &[MarkdownExtensionDefinition] {
        &self.definitions
    }

    pub(super) fn install(&self, context: &MarkdownExtensionContext) -> MarkdownExtensions {
        install_extensions(
            self.definitions
                .iter()
                .map(MarkdownExtensionDefinition::value),
            context,
        )
    }
}

impl From<&ContributionSnapshot<MarkdownExtensionKey>> for MarkdownExtensionSnapshot {
    fn from(snapshot: &ContributionSnapshot<MarkdownExtensionKey>) -> Self {
        Self {
            revision: snapshot.revision(),
            definitions: snapshot
                .contributions()
                .iter()
                .map(|contribution| {
                    MarkdownExtensionDefinition::new(
                        contribution.id(),
                        contribution.order(),
                        contribution.value().clone(),
                    )
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

type InstallExtension =
    dyn Fn(MarkdownExtensions, &MarkdownExtensionContext) -> MarkdownExtensions + 'static;

/// One statically linked extension installer stored as a typed contribution.
#[derive(Clone)]
pub(crate) struct MarkdownExtensionInstaller {
    install: Rc<InstallExtension>,
}

impl MarkdownExtensionInstaller {
    pub(crate) fn new(
        install: impl Fn(MarkdownExtensions, &MarkdownExtensionContext) -> MarkdownExtensions + 'static,
    ) -> Self {
        Self {
            install: Rc::new(install),
        }
    }

    fn install(
        &self,
        extensions: MarkdownExtensions,
        context: &MarkdownExtensionContext,
    ) -> MarkdownExtensions {
        (self.install)(extensions, context)
    }
}

/// Per-body values required by parser and renderer contributions.
pub(crate) struct MarkdownExtensionContext {
    owner_id: u64,
    source_offset: usize,
    preference_state: PreferenceState,
}

impl MarkdownExtensionContext {
    pub(crate) fn new(
        owner_id: u64,
        source_offset: usize,
        preference_state: PreferenceState,
    ) -> Self {
        Self {
            owner_id,
            source_offset,
            preference_state,
        }
    }

    pub(crate) const fn owner_id(&self) -> u64 {
        self.owner_id
    }

    pub(crate) const fn source_offset(&self) -> usize {
        self.source_offset
    }

    pub(crate) const fn preference_state(&self) -> &PreferenceState {
        &self.preference_state
    }
}

pub(crate) fn cjk_emphasis_contribution() -> MarkdownExtensionDefinition {
    MarkdownExtensionDefinition::new(
        CJK_EMPHASIS_ID,
        CJK_EMPHASIS_ORDER,
        MarkdownExtensionInstaller::new(|extensions, _| extensions.cjk_emphasis_compatibility()),
    )
}

pub(crate) fn builtin_extension_contributions() -> [MarkdownExtensionDefinition; 3] {
    [
        cjk_emphasis_contribution(),
        crate::ui::math::markdown_contribution(),
        super::code_block::fenced_code_contribution(),
    ]
}

pub(super) fn install_extensions<'a>(
    installers: impl IntoIterator<Item = &'a MarkdownExtensionInstaller>,
    context: &MarkdownExtensionContext,
) -> MarkdownExtensions {
    installers
        .into_iter()
        .fold(MarkdownExtensions::default(), |extensions, installer| {
            installer.install(extensions, context)
        })
}

#[cfg(test)]
pub(crate) fn test_extension_snapshot() -> MarkdownExtensionSnapshot {
    let mut contributions = builtin_extension_contributions();
    contributions.sort_unstable_by_key(|contribution| (contribution.order(), contribution.id()));
    MarkdownExtensionSnapshot {
        revision: TEST_EXTENSION_REVISION,
        definitions: Rc::from(contributions),
    }
}
