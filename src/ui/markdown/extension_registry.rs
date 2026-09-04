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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MarkdownContributionOwner {
    id: ContributionId,
    generation: u64,
}

impl MarkdownContributionOwner {
    pub(crate) const fn new(id: ContributionId, generation: u64) -> Self {
        Self { id, generation }
    }

    #[cfg(test)]
    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }

    pub(crate) fn keyed_state_id(
        self,
        namespace: &str,
        body_owner_id: u64,
        source_start: usize,
    ) -> String {
        format!(
            "{namespace}-{}-{}-{body_owner_id}-{source_start}",
            self.id, self.generation
        )
    }
}

#[derive(Clone)]
struct MarkdownExtensionContribution {
    definition: MarkdownExtensionDefinition,
    owner: MarkdownContributionOwner,
}

/// Immutable foreground projection of one contribution registry revision.
#[derive(Clone)]
pub(crate) struct MarkdownExtensionSnapshot {
    revision: u64,
    contributions: Rc<[MarkdownExtensionContribution]>,
}

impl MarkdownExtensionSnapshot {
    pub(super) fn empty() -> Self {
        Self {
            revision: 0,
            contributions: Vec::new().into(),
        }
    }

    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg(test)]
    pub(crate) fn definitions(&self) -> Vec<&MarkdownExtensionDefinition> {
        self.contributions
            .iter()
            .map(|contribution| &contribution.definition)
            .collect()
    }

    pub(super) fn install(&self, context: &MarkdownExtensionInstallContext) -> MarkdownExtensions {
        self.contributions
            .iter()
            .fold(MarkdownExtensions::default(), |extensions, contribution| {
                let contribution_context = context.for_contribution(contribution.owner);
                contribution
                    .definition
                    .value()
                    .install(extensions, &contribution_context)
            })
    }
}

impl From<&ContributionSnapshot<MarkdownExtensionKey>> for MarkdownExtensionSnapshot {
    fn from(snapshot: &ContributionSnapshot<MarkdownExtensionKey>) -> Self {
        Self {
            revision: snapshot.revision(),
            contributions: snapshot
                .contributions()
                .iter()
                .map(|contribution| MarkdownExtensionContribution {
                    definition: MarkdownExtensionDefinition::new(
                        contribution.id(),
                        contribution.order(),
                        contribution.value().clone(),
                    ),
                    owner: MarkdownContributionOwner::new(
                        contribution.id(),
                        contribution.generation(),
                    ),
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

pub(crate) struct MarkdownExtensionInstallContext {
    owner_id: u64,
    source_offset: usize,
    preference_state: PreferenceState,
    streaming: bool,
}

impl MarkdownExtensionInstallContext {
    pub(crate) fn new(
        owner_id: u64,
        source_offset: usize,
        preference_state: PreferenceState,
        streaming: bool,
    ) -> Self {
        Self {
            owner_id,
            source_offset,
            preference_state,
            streaming,
        }
    }

    #[cfg(test)]
    pub(crate) const fn owner_id(&self) -> u64 {
        self.owner_id
    }

    /// Returns whether the streaming flag changed.
    pub(super) fn set_streaming(&mut self, streaming: bool) -> bool {
        if self.streaming == streaming {
            return false;
        }
        self.streaming = streaming;
        true
    }

    fn for_contribution(
        &self,
        contribution_owner: MarkdownContributionOwner,
    ) -> MarkdownExtensionContext {
        MarkdownExtensionContext {
            owner_id: self.owner_id,
            source_offset: self.source_offset,
            preference_state: self.preference_state.clone(),
            contribution_owner,
            streaming: self.streaming,
        }
    }
}

/// Per-body values required by one parser and renderer contribution.
pub(crate) struct MarkdownExtensionContext {
    owner_id: u64,
    source_offset: usize,
    preference_state: PreferenceState,
    contribution_owner: MarkdownContributionOwner,
    streaming: bool,
}

impl MarkdownExtensionContext {
    pub(crate) const fn owner_id(&self) -> u64 {
        self.owner_id
    }

    pub(crate) const fn source_offset(&self) -> usize {
        self.source_offset
    }

    pub(crate) const fn preference_state(&self) -> &PreferenceState {
        &self.preference_state
    }

    pub(crate) const fn contribution_owner(&self) -> MarkdownContributionOwner {
        self.contribution_owner
    }

    pub(crate) const fn streaming(&self) -> bool {
        self.streaming
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

#[cfg(test)]
pub(crate) fn test_extension_snapshot() -> MarkdownExtensionSnapshot {
    let mut contributions = builtin_extension_contributions();
    contributions.sort_unstable_by_key(|contribution| (contribution.order(), contribution.id()));
    MarkdownExtensionSnapshot {
        revision: TEST_EXTENSION_REVISION,
        contributions: contributions
            .into_iter()
            .enumerate()
            .map(|(index, definition)| MarkdownExtensionContribution {
                owner: MarkdownContributionOwner::new(
                    definition.id(),
                    u64::try_from(index + 1).unwrap_or(u64::MAX),
                ),
                definition,
            })
            .collect::<Vec<_>>()
            .into(),
    }
}
