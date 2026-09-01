//! Persistent user preferences (sidebar, theme, language, window geometry).
//!
//! Prefs are written to the unified `~/.config/nostra` directory and read back
//! on startup. Any invalid current-schema document falls back to
//! `Preferences::default`. At runtime the current values live in the [`Prefs`]
//! app-global;
//! mutations go through [`update`], which persists synchronously and atomically
//! so settings survive even if the app never quits cleanly.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use gpui::{App, Global, Window};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    llm::{ModelSelection, ProviderCatalogSnapshot, ProviderCatalogSource, ProviderProfile},
    runtime::{CHAT_WORKSPACE_ID, PROJECT_WORKSPACE_ID, WorkspaceId},
    session::SessionId,
};

const FILE_NAME: &str = "preferences.json";
pub const DEFAULT_GLASS_TINT_OPACITY: f32 = 0.85;

/// Stable identity of the first-party JSON preference Provider.
pub const JSON_PROVIDER_NAME: &str = "nostra.preferences.json";

/// Snapshot of user preferences that survives across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    /// Sidebar width in the expanded state.
    pub sidebar_width: f32,
    /// Width of the profile-list column in the provider settings split.  The
    /// detail column takes the remainder, so one value pins the divider.
    /// Clamped into the page's allowed range when restored.
    pub provider_list_width: f32,
    /// Whether the sidebar is collapsed.
    pub sidebar_collapsed: bool,
    /// Whether startup may lazily restore the last explicitly active Chat
    /// session. The GUI control and shell behavior are intentionally separate
    /// from this persisted contract.
    pub restore_last_chat_on_start: bool,
    /// Most recent explicitly active Chat session, retained even while
    /// automatic restoration is disabled.
    pub last_active_chat_session: Option<SessionId>,
    /// Whether startup restores [`last_workspace_id`](Self::last_workspace_id).
    /// Missing values from preferences written before workspace modes existed
    /// migrate to the enabled default instead of invalidating the whole file.
    #[serde(default = "default_restore_last_workspace_on_start")]
    pub restore_last_workspace_on_start: bool,
    /// Most recently selected top-level workspace identity. The persisted
    /// field remains `last_workspace_mode` for the current preferences schema.
    /// This is retained even while automatic restoration is disabled so
    /// re-enabling the preference resumes the user's last explicit choice.
    #[serde(
        rename = "last_workspace_mode",
        default = "default_last_workspace_id",
        serialize_with = "serialize_workspace_id",
        deserialize_with = "deserialize_workspace_id"
    )]
    pub last_workspace_id: WorkspaceId,
    /// Explicit theme mode override.  `None` means "follow system".
    pub theme_mode: Option<ThemeMode>,
    /// Which bundled font the composer input uses.
    pub composer_font: ComposerFont,
    /// Whether user-role message bodies use the Markdown presentation path.
    pub user_message_markdown: bool,
    /// Whether the conversation transcript eases discrete mouse-wheel input.
    pub smooth_chat_scrolling: bool,
    /// Whether supported windows use the native macOS blurred backdrop.
    pub glass_effect: bool,
    /// Opacity of the theme tint drawn above the native blurred backdrop.
    pub glass_tint_opacity: f32,
    /// Whether settings omit the buttons that reveal explanatory text.
    pub hide_settings_info_buttons: bool,
    /// Whether the diagnostic file includes sparse informational lifecycle
    /// events. When false, the logger records warnings and errors only.
    pub detailed_logging: bool,
    /// Global fenced-code wrap value applied whenever the setting changes.
    pub code_block_wrap: bool,
    /// Monotonic reset generation for per-block wrap controls.
    pub code_block_wrap_revision: u64,
    /// Whether fenced code displays a line-number gutter.
    pub code_block_line_numbers: bool,
    /// UI language.
    pub language: Language,
    /// Theme name applied while in light mode.  `None` or an unregistered
    /// name falls back to the built-in default at startup.
    pub light_theme: Option<String>,
    /// Theme name applied while in dark mode.  Same fallback rules.
    pub dark_theme: Option<String>,
    /// Last known main-window geometry (restore bounds).  `None` on first
    /// run; invalid values are clamped or discarded at restore time.
    pub window: Option<WindowGeometry>,
    /// Last known settings-window geometry. `None` until that window has
    /// been opened; invalid values are discarded at restore time.
    pub settings_window: Option<WindowGeometry>,
    /// User-managed OpenAI-compatible endpoints and their model catalogs.
    pub provider_profiles: Vec<ProviderProfile>,
    /// Selection inherited by newly-created conversations.
    pub last_model_selection: Option<ModelSelection>,
    /// Folders the user opened as Agent work projects.  These exist in the UI
    /// before the store registers a project row with its first Agent session;
    /// the store catalog stays authoritative once a session exists.
    pub agent_projects: Vec<AgentProjectRecord>,
}

/// One user-opened Agent work project, persisted so the folder reappears in
/// the Agent sidebar after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProjectRecord {
    /// Stable `project-<uuid-v7>` identity.  Reused when the same canonical
    /// path is opened again.
    pub project_id: String,
    /// Canonical absolute path of the folder.
    pub canonical_path: PathBuf,
    /// Folder name shown in the sidebar.
    pub display_name: String,
}

fn default_sidebar_width() -> f32 {
    272.0
}

/// Starting position of the provider settings divider, i.e. where the list
/// column sits until the user drags it.
pub const DEFAULT_PROVIDER_LIST_WIDTH: f32 = 220.0;

impl Default for Preferences {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            provider_list_width: DEFAULT_PROVIDER_LIST_WIDTH,
            sidebar_collapsed: false,
            restore_last_chat_on_start: false,
            last_active_chat_session: None,
            restore_last_workspace_on_start: default_restore_last_workspace_on_start(),
            last_workspace_id: default_last_workspace_id(),
            theme_mode: None,
            composer_font: ComposerFont::default(),
            user_message_markdown: false,
            smooth_chat_scrolling: false,
            glass_effect: false,
            glass_tint_opacity: DEFAULT_GLASS_TINT_OPACITY,
            hide_settings_info_buttons: false,
            detailed_logging: false,
            code_block_wrap: false,
            code_block_wrap_revision: 0,
            code_block_line_numbers: false,
            language: Language::default(),
            light_theme: None,
            dark_theme: None,
            window: None,
            settings_window: None,
            provider_profiles: Vec::new(),
            last_model_selection: None,
            agent_projects: Vec::new(),
        }
    }
}

fn default_restore_last_workspace_on_start() -> bool {
    true
}

fn default_last_workspace_id() -> WorkspaceId {
    CHAT_WORKSPACE_ID
}

fn serialize_workspace_id<S>(id: &WorkspaceId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let persisted = if *id == CHAT_WORKSPACE_ID {
        "chat"
    } else if *id == PROJECT_WORKSPACE_ID {
        "project"
    } else {
        id.as_str()
    };
    serializer.serialize_str(persisted)
}

fn deserialize_workspace_id<'de, D>(deserializer: D) -> Result<WorkspaceId, D::Error>
where
    D: Deserializer<'de>,
{
    let persisted = String::deserialize(deserializer)?;
    // A newer build may persist a workspace this build does not provide yet;
    // keep the rest of the preference document usable and select Chat safely.
    Ok(match persisted.as_str() {
        "project" => PROJECT_WORKSPACE_ID,
        "chat" => CHAT_WORKSPACE_ID,
        _ => CHAT_WORKSPACE_ID,
    })
}

/// Current built-in workspace presentation modes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    #[default]
    Chat,
    Project,
}

impl WorkspaceMode {
    #[must_use]
    pub const fn workspace_id(self) -> WorkspaceId {
        match self {
            Self::Chat => CHAT_WORKSPACE_ID,
            Self::Project => PROJECT_WORKSPACE_ID,
        }
    }

    #[must_use]
    pub fn from_workspace_id(id: WorkspaceId) -> Self {
        if id == PROJECT_WORKSPACE_ID {
            Self::Project
        } else {
            Self::Chat
        }
    }
}

/// UI languages the app can render in.  The serialized form doubles as the
/// stable identifier used by the settings dropdown.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    /// 简体中文 (default).
    #[default]
    ZhCn,
    /// English.
    En,
}

impl Language {
    /// BCP 47 tag understood by rust-i18n's locale lookup; must match the
    /// locale keys used in `locales/nostra.yml` and gpui-component's `ui.yml`.
    pub fn locale(self) -> &'static str {
        match self {
            Language::ZhCn => "zh-CN",
            Language::En => "en",
        }
    }

    /// Native-script label shown in the language dropdown.  Deliberately not
    /// translated: each language names itself.
    pub fn label(self) -> &'static str {
        match self {
            Language::ZhCn => "简体中文",
            Language::En => "English",
        }
    }

    /// Stable identifier for dropdown values (the serde kebab-case form).
    pub fn key(self) -> &'static str {
        match self {
            Language::ZhCn => "zh-cn",
            Language::En => "en",
        }
    }

    /// Inverse of [`Language::key`]; unknown keys fall back to the default.
    pub fn from_key(key: &str) -> Self {
        match key {
            "en" => Language::En,
            _ => Language::default(),
        }
    }

    pub fn all() -> [Language; 2] {
        [Language::ZhCn, Language::En]
    }
}

/// Main-window restore bounds in global screen coordinates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowGeometry {
    /// Capture normal restore bounds (not transient maximized/fullscreen size).
    pub fn from_window(window: &Window) -> Self {
        let bounds = window.window_bounds().get_bounds();
        Self {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        }
    }
}

/// Fonts currently offered for the composer. The bundled primary faces provide
/// reliable cross-platform defaults: Maple Mono CN covers Latin, CJK, and
/// fullwidth punctuation, while JetBrains Mono lets CJK fall back to the
/// platform font. Production shaping determines soft-wrap boundaries for both
/// choices, so font coverage is no longer a wrapping-correctness requirement.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComposerFont {
    /// Maple Mono CN — rounded Latin + 圆体 CJK, fully self-contained.
    #[default]
    MapleMonoCn,
    /// JetBrains Mono for Latin, system font for CJK.
    JetBrainsMono,
}

impl ComposerFont {
    /// The family name recorded in the bundled TTF's name table; must match
    /// exactly for `font_family` to resolve to the embedded font.
    pub fn family(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "Maple Mono CN",
            ComposerFont::JetBrainsMono => "JetBrains Mono",
        }
    }

    /// Human-readable label for menus.  Font names are proper nouns, so the
    /// label is not routed through i18n.
    pub fn label(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "Maple Mono 圆体",
            ComposerFont::JetBrainsMono => "JetBrains Mono + 系统中文",
        }
    }

    /// Stable identifier for dropdown values (the serde kebab-case form,
    /// which splits on every capital: `jet-brains-mono`).
    pub fn key(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "maple-mono-cn",
            ComposerFont::JetBrainsMono => "jet-brains-mono",
        }
    }

    /// Inverse of [`ComposerFont::key`]; unknown keys fall back to default.
    pub fn from_key(key: &str) -> Self {
        match key {
            "jet-brains-mono" => ComposerFont::JetBrainsMono,
            _ => ComposerFont::default(),
        }
    }

    pub fn all() -> [ComposerFont; 2] {
        [ComposerFont::MapleMonoCn, ComposerFont::JetBrainsMono]
    }
}

/// Serializable theme mode.  We keep this decoupled from
/// `gpui_component::ThemeMode` so preferences can be read without the UI
/// crate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, ThemeMode::Dark)
    }
}

pub type PreferenceSaver = Arc<dyn Fn(&Preferences) -> anyhow::Result<()> + Send + Sync>;

struct PreferenceState {
    preferences: Arc<Mutex<Preferences>>,
    saver: PreferenceSaver,
}

/// Explicit application-scoped preference handle. The handle owns the
/// Provider state and keeps persistence behind a narrow read/write boundary.
/// Clones refer to the same live snapshot and Provider.
#[derive(Clone)]
pub struct PreferenceHandle {
    state: Arc<PreferenceState>,
}

impl PreferenceHandle {
    /// Build the default JSON-backed Provider.
    pub fn json(prefs: Preferences) -> Self {
        Self::with_saver(prefs, Arc::new(save))
    }

    /// Build a volatile Provider for tests and process-local compositions.
    pub fn in_memory(prefs: Preferences) -> Self {
        Self::with_saver(prefs, Arc::new(|_| Ok(())))
    }

    /// Build a Provider with an explicit persistence boundary.
    pub fn with_saver(prefs: Preferences, saver: PreferenceSaver) -> Self {
        Self {
            state: Arc::new(PreferenceState {
                preferences: Arc::new(Mutex::new(prefs)),
                saver,
            }),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Preferences {
        match self.state.preferences.lock() {
            Ok(prefs) => prefs.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Share the live Provider snapshot with presentation state that is
    /// constructed from this handle. Consumers never need to resolve the
    /// foreground `Prefs` adapter to observe updates.
    pub(crate) fn shared_preferences(&self) -> Arc<Mutex<Preferences>> {
        Arc::clone(&self.state.preferences)
    }

    /// Replace the live snapshot and persist it through this Provider.
    /// The live mutation remains applied if persistence fails.
    pub fn replace(&self, prefs: Preferences) -> anyhow::Result<()> {
        {
            let mut current = match self.state.preferences.lock() {
                Ok(current) => current,
                Err(poisoned) => poisoned.into_inner(),
            };
            *current = prefs.clone();
        }
        (self.state.saver)(&prefs)
    }

    /// Replace the live snapshot without invoking persistence.
    pub fn replace_in_memory(&self, prefs: Preferences) {
        let mut current = match self.state.preferences.lock() {
            Ok(current) => current,
            Err(poisoned) => poisoned.into_inner(),
        };
        *current = prefs;
    }

    /// Mutate the live snapshot and persist it through this Provider.
    pub fn update(&self, f: impl FnOnce(&mut Preferences)) -> anyhow::Result<()> {
        let mut prefs = self.snapshot();
        f(&mut prefs);
        self.replace(prefs)
    }

    /// Mutate the live snapshot without persistence.
    pub fn update_in_memory(&self, f: impl FnOnce(&mut Preferences)) {
        let mut prefs = self.snapshot();
        f(&mut prefs);
        self.replace_in_memory(prefs);
    }

    /// Persist an already prepared snapshot, without changing live state.
    pub fn save_snapshot(&self, prefs: &Preferences) -> anyhow::Result<()> {
        (self.state.saver)(prefs)
    }
}

/// Provider routing is derived from the live preference state, so a profile
/// edit reaches generation on the next request without replacing the
/// generation Provider. Preferences remain the single writer; there is no
/// second catalog to keep in step.
impl ProviderCatalogSource for PreferenceHandle {
    fn catalog(&self) -> ProviderCatalogSnapshot {
        ProviderCatalogSnapshot::new(self.snapshot().provider_profiles)
    }
}

/// App-global foreground adapter for the explicit preference capability.
/// Render code can continue to borrow a stable snapshot while composition
/// consumers receive the cloneable [`PreferenceHandle`].
pub struct Prefs {
    preferences: Preferences,
    handle: PreferenceHandle,
}

impl Global for Prefs {}

/// Seed the [`Prefs`] global from the loaded preferences.  Must run during
/// app init, before any UI reads settings.
pub fn init_global(prefs: Preferences, cx: &mut App) {
    init_global_with_handle(PreferenceHandle::json(prefs), cx);
}

/// Seed the foreground adapter from an explicitly selected Provider.
pub fn init_global_with_handle(handle: PreferenceHandle, cx: &mut App) {
    let preferences = handle.snapshot();
    cx.set_global(Prefs {
        preferences,
        handle,
    });
}

/// Clone the active preference capability handle.
pub fn handle(cx: &App) -> PreferenceHandle {
    cx.global::<Prefs>().handle.clone()
}

#[cfg(test)]
pub(crate) fn test_handle(cx: &App) -> PreferenceHandle {
    cx.try_global::<Prefs>()
        .map(|prefs| prefs.handle.clone())
        .unwrap_or_else(|| PreferenceHandle::in_memory(Preferences::default()))
}

/// The live preferences for test assertions that exercise the foreground
/// adapter. Production consumers receive an explicit snapshot or handle.
#[cfg(test)]
pub fn get(cx: &App) -> &Preferences {
    &cx.global::<Prefs>().preferences
}

/// Mutate the live preferences and persist the result.  The write happens
/// synchronously on purpose: the file is a few hundred bytes, and spawning
/// each save onto the background pool would let two rapid changes race on
/// the same path (fs::write is not atomic — last-spawned is not guaranteed
/// last-written).  Save errors are logged and otherwise ignored — a failed
/// write never breaks the running app.
pub fn update(cx: &mut App, f: impl FnOnce(&mut Preferences)) {
    let handle = handle(cx);
    update_with(cx, &handle, f);
}

/// Mutate through an explicit handle while keeping the foreground adapter in
/// sync and notifying GPUI global observers.
pub fn update_with(cx: &mut App, handle: &PreferenceHandle, f: impl FnOnce(&mut Preferences)) {
    let prefs = cx.global_mut::<Prefs>();
    prefs.handle = handle.clone();
    f(&mut prefs.preferences);
    let snapshot = prefs.preferences.clone();
    let result = handle.replace(snapshot);
    if let Err(e) = result {
        crate::logging::error(
            "preferences",
            format_args!("failed to save preferences: {e:?}"),
        );
    }
}

/// Record the user's explicit workspace selection. Entity tests keep this
/// mutation in memory so exercising the account menu never writes to the real
/// user configuration directory.
pub fn set_last_workspace_id(id: WorkspaceId, cx: &mut App) {
    #[cfg(not(test))]
    update(cx, |prefs| prefs.last_workspace_id = id);
    #[cfg(test)]
    update_in_memory(cx, |prefs| prefs.last_workspace_id = id);
}

pub fn remove_agent_project(project_id: &str, cx: &mut App) {
    #[cfg(not(test))]
    update(cx, |prefs| {
        prefs
            .agent_projects
            .retain(|record| record.project_id != project_id);
    });
    #[cfg(test)]
    update_in_memory(cx, |prefs| {
        prefs
            .agent_projects
            .retain(|record| record.project_id != project_id);
    });
}

/// Mutate live preferences without persistence so entity tests can exercise
/// global observation without writing to the user's configuration directory.
#[cfg(test)]
pub(crate) fn update_in_memory(cx: &mut App, f: impl FnOnce(&mut Preferences)) {
    let handle = handle(cx);
    update_with_in_memory(cx, &handle, f);
}

/// Mutate through an explicit handle without persistence while keeping the
/// foreground adapter in sync.
pub fn update_with_in_memory(
    cx: &mut App,
    handle: &PreferenceHandle,
    f: impl FnOnce(&mut Preferences),
) {
    let prefs = cx.global_mut::<Prefs>();
    prefs.handle = handle.clone();
    f(&mut prefs.preferences);
    handle.replace_in_memory(prefs.preferences.clone());
}

/// Fold exit-time state into the live preferences and return the merged
/// snapshot.  Unlike [`update`] this does not spawn a save — quit hooks run
/// the flush themselves so gpui can await it before the process exits.
pub fn snapshot_with(cx: &mut App, f: impl FnOnce(&mut Preferences)) -> Preferences {
    let handle = handle(cx);
    let prefs = cx.global_mut::<Prefs>();
    f(&mut prefs.preferences);
    let snapshot = prefs.preferences.clone();
    handle.replace_in_memory(snapshot.clone());
    snapshot
}

/// Full path where preferences are stored.  `None` on platforms where no
/// standard config directory can be resolved from the environment.
pub fn path() -> Option<PathBuf> {
    crate::paths::nostra_config_dir().map(|d| d.join(FILE_NAME))
}

/// Load preferences, or return defaults if the file is missing / corrupt.
pub fn load() -> Preferences {
    let Some(p) = path() else {
        return Preferences::default();
    };
    let Ok(contents) = std::fs::read_to_string(&p) else {
        return Preferences::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Persist preferences to disk.  Errors are returned so the caller can log
/// them; nothing about a save failure prevents the app from working.
pub fn save(prefs: &Preferences) -> anyhow::Result<()> {
    let Some(p) = path() else {
        anyhow::bail!("no config directory available on this platform");
    };
    save_to_path(&p, prefs)
}

/// Persist preferences by atomically replacing the target with a fully
/// written temporary file from the same directory.
fn save_to_path(path: &Path, prefs: &Preferences) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(prefs)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&json)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    // Persist the directory entry as well as the file contents on Unix.
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn incomplete_or_unknown_schema_is_rejected() {
        assert!(serde_json::from_str::<Preferences>(r#"{"language":"en"}"#).is_err());
        let mut value = serde_json::to_value(Preferences::default()).expect("serialize");
        value["language"] = serde_json::Value::String("unknown".into());
        assert!(serde_json::from_value::<Preferences>(value).is_err());

        let mut unknown_top_level =
            serde_json::to_value(Preferences::default()).expect("serialize");
        unknown_top_level["legacy_provider"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Preferences>(unknown_top_level).is_err());

        let mut missing_current_field =
            serde_json::to_value(Preferences::default()).expect("serialize");
        assert!(
            missing_current_field
                .as_object_mut()
                .expect("preferences object")
                .remove("hide_settings_info_buttons")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing_current_field).is_err());

        let mut missing_detailed_logging =
            serde_json::to_value(Preferences::default()).expect("serialize");
        assert!(
            missing_detailed_logging
                .as_object_mut()
                .expect("preferences object")
                .remove("detailed_logging")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing_detailed_logging).is_err());

        let mut missing_wrap_revision =
            serde_json::to_value(Preferences::default()).expect("serialize");
        assert!(
            missing_wrap_revision
                .as_object_mut()
                .expect("preferences object")
                .remove("code_block_wrap_revision")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing_wrap_revision).is_err());

        let mut missing_user_message_markdown =
            serde_json::to_value(Preferences::default()).expect("serialize");
        assert!(
            missing_user_message_markdown
                .as_object_mut()
                .expect("preferences object")
                .remove("user_message_markdown")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing_user_message_markdown).is_err());

        let mut missing_smooth_chat_scrolling =
            serde_json::to_value(Preferences::default()).expect("serialize preferences");
        assert!(
            missing_smooth_chat_scrolling
                .as_object_mut()
                .expect("preferences object")
                .remove("smooth_chat_scrolling")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing_smooth_chat_scrolling).is_err());

        let mut missing_restore_last_chat =
            serde_json::to_value(Preferences::default()).expect("serialize preferences");
        assert!(
            missing_restore_last_chat
                .as_object_mut()
                .expect("preferences object")
                .remove("restore_last_chat_on_start")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing_restore_last_chat).is_err());

        let mut unknown_geometry = serde_json::to_value(Preferences {
            window: Some(WindowGeometry {
                x: 0.,
                y: 0.,
                width: 800.,
                height: 600.,
            }),
            ..Preferences::default()
        })
        .expect("serialize");
        unknown_geometry["window"]["legacy_scale"] = serde_json::Value::from(2);
        assert!(serde_json::from_value::<Preferences>(unknown_geometry).is_err());
    }

    #[test]
    fn explicit_preference_handle_preserves_snapshot_and_save_boundary() {
        let saves = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&saves);
        let handle = PreferenceHandle::with_saver(
            Preferences::default(),
            Arc::new(move |prefs| {
                assert!(prefs.sidebar_collapsed);
                observed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
        );

        handle
            .update(|prefs| prefs.sidebar_collapsed = true)
            .expect("custom Provider accepts the snapshot");
        assert!(handle.snapshot().sidebar_collapsed);
        assert_eq!(saves.load(Ordering::Relaxed), 1);

        handle.update_in_memory(|prefs| prefs.sidebar_collapsed = false);
        assert!(!handle.snapshot().sidebar_collapsed);
        assert_eq!(saves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn preference_handle_catalog_reflects_live_profile_edits() {
        let handle = PreferenceHandle::in_memory(Preferences::default());
        assert!(handle.catalog().profiles().is_empty());

        handle.update_in_memory(|prefs| {
            prefs.provider_profiles.push(ProviderProfile {
                id: "provider".into(),
                name: "Provider".into(),
                base_url: "https://example.com/v1".into(),
                api_key: crate::llm::SecretString::default(),
                protocol: crate::llm::Protocol::Responses,
                compatibility: crate::llm::CompatibilityProfile::default(),
                models: vec![crate::llm::ModelConfig {
                    id: "model".into(),
                    model_id: "vendor/model".into(),
                    display_name: None,
                }],
            });
        });

        assert_eq!(handle.catalog().profiles().len(), 1);
        assert_eq!(handle.catalog().profiles()[0].id, "provider");
    }

    #[test]
    fn preference_handle_keeps_live_state_when_save_fails() {
        let handle = PreferenceHandle::with_saver(
            Preferences::default(),
            Arc::new(|_| Err(anyhow::anyhow!("save failed"))),
        );

        assert!(
            handle
                .update(|prefs| prefs.detailed_logging = true)
                .is_err()
        );
        assert!(handle.snapshot().detailed_logging);
    }

    #[test]
    fn appearance_preferences_have_expected_defaults() {
        let prefs = Preferences::default();
        assert!(!prefs.glass_effect);
        assert_eq!(prefs.glass_tint_opacity, DEFAULT_GLASS_TINT_OPACITY);
        assert!(!prefs.hide_settings_info_buttons);
        assert!(!prefs.detailed_logging);
        assert!(!prefs.user_message_markdown);
        assert!(!prefs.smooth_chat_scrolling);
        assert!(!prefs.code_block_wrap);
        assert_eq!(prefs.code_block_wrap_revision, 0);
        assert!(!prefs.code_block_line_numbers);
        assert!(!prefs.restore_last_chat_on_start);
        assert!(prefs.last_active_chat_session.is_none());
        assert!(prefs.restore_last_workspace_on_start);
        assert_eq!(prefs.last_workspace_id, CHAT_WORKSPACE_ID);
    }

    #[test]
    fn workspace_preferences_migrate_old_documents_and_round_trip() {
        let mut old_document =
            serde_json::to_value(Preferences::default()).expect("serialize preferences");
        let object = old_document.as_object_mut().expect("preferences object");
        object.remove("restore_last_workspace_on_start");
        object.remove("last_workspace_mode");

        let migrated: Preferences =
            serde_json::from_value(old_document).expect("migrate workspace preferences");
        assert!(migrated.restore_last_workspace_on_start);
        assert_eq!(migrated.last_workspace_id, CHAT_WORKSPACE_ID);

        let prefs = Preferences {
            restore_last_workspace_on_start: false,
            last_workspace_id: PROJECT_WORKSPACE_ID,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize workspace preferences");
        let restored: Preferences =
            serde_json::from_str(&json).expect("deserialize workspace preferences");
        assert!(!restored.restore_last_workspace_on_start);
        assert_eq!(restored.last_workspace_id, PROJECT_WORKSPACE_ID);
    }

    #[test]
    fn workspace_identity_keeps_persisted_aliases_and_falls_back_safely() {
        let prefs = Preferences {
            sidebar_collapsed: true,
            last_workspace_id: PROJECT_WORKSPACE_ID,
            ..Preferences::default()
        };
        let mut value = serde_json::to_value(&prefs).expect("serialize workspace identity");
        assert_eq!(value["last_workspace_mode"], "project");
        let default_value = serde_json::to_value(Preferences::default())
            .expect("serialize default workspace identity");
        assert_eq!(default_value["last_workspace_mode"], "chat");

        let restored: Preferences =
            serde_json::from_value(value.clone()).expect("deserialize workspace identity");
        assert_eq!(restored.last_workspace_id, PROJECT_WORKSPACE_ID);
        assert!(restored.sidebar_collapsed);

        value["last_workspace_mode"] = serde_json::Value::String("nostra.workspace.future".into());
        let unknown: Preferences =
            serde_json::from_value(value).expect("unknown workspace should fall back");
        assert_eq!(unknown.last_workspace_id, CHAT_WORKSPACE_ID);
        assert!(unknown.sidebar_collapsed);

        let mut missing = serde_json::to_value(&prefs).expect("serialize workspace identity");
        missing
            .as_object_mut()
            .expect("preferences object")
            .remove("last_workspace_mode");
        let missing: Preferences =
            serde_json::from_value(missing).expect("missing workspace should use default");
        assert_eq!(missing.last_workspace_id, CHAT_WORKSPACE_ID);
        assert!(missing.sidebar_collapsed);

        let mut invalid = serde_json::to_value(&prefs).expect("serialize workspace identity");
        invalid["last_workspace_mode"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<Preferences>(invalid).is_err());
    }

    #[test]
    fn workspace_mode_adapter_uses_typed_ids_and_defaults_unknown_ids_to_chat() {
        assert_eq!(WorkspaceMode::Chat.workspace_id(), CHAT_WORKSPACE_ID);
        assert_eq!(WorkspaceMode::Project.workspace_id(), PROJECT_WORKSPACE_ID);
        assert_eq!(
            WorkspaceMode::from_workspace_id(PROJECT_WORKSPACE_ID),
            WorkspaceMode::Project
        );
        assert_eq!(
            WorkspaceMode::from_workspace_id(WorkspaceId::new("nostra.workspace.future")),
            WorkspaceMode::Chat
        );
    }

    #[test]
    fn agent_projects_default_empty_and_round_trip() {
        assert!(Preferences::default().agent_projects.is_empty());

        let prefs = Preferences {
            agent_projects: vec![AgentProjectRecord {
                project_id: "project-018f6b2e-9d4a-7b3c-8e5f-1a2b3c4d5e6f".to_string(),
                canonical_path: PathBuf::from("/tmp/work"),
                display_name: "work".to_string(),
            }],
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize agent projects");
        let restored: Preferences =
            serde_json::from_str(&json).expect("deserialize agent projects");
        assert_eq!(restored.agent_projects, prefs.agent_projects);

        // A missing field rejects the document under the strict schema.
        let mut missing = serde_json::to_value(&prefs).expect("serialize");
        assert!(
            missing
                .as_object_mut()
                .expect("preferences object")
                .remove("agent_projects")
                .is_some()
        );
        assert!(serde_json::from_value::<Preferences>(missing).is_err());
    }

    #[test]
    fn chat_startup_preferences_round_trip_without_enabling_restore() {
        let session_id = SessionId::new(crate::session::SessionDomain::Chat);
        let prefs = Preferences {
            restore_last_chat_on_start: false,
            last_active_chat_session: Some(session_id.clone()),
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize chat startup preferences");
        let restored: Preferences =
            serde_json::from_str(&json).expect("deserialize chat startup preferences");
        assert!(!restored.restore_last_chat_on_start);
        assert_eq!(restored.last_active_chat_session, Some(session_id));
    }

    #[test]
    fn text_rendering_preferences_round_trip() {
        let prefs = Preferences {
            user_message_markdown: true,
            smooth_chat_scrolling: true,
            code_block_wrap: true,
            code_block_wrap_revision: 7,
            code_block_line_numbers: true,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize code preferences");
        let back: Preferences = serde_json::from_str(&json).expect("deserialize code preferences");
        assert!(back.code_block_wrap);
        assert_eq!(back.code_block_wrap_revision, 7);
        assert!(back.code_block_line_numbers);
        assert!(back.user_message_markdown);
        assert!(back.smooth_chat_scrolling);
    }

    #[test]
    fn detailed_logging_round_trips_without_changing_the_safe_default() {
        assert!(!Preferences::default().detailed_logging);
        let prefs = Preferences {
            detailed_logging: true,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize diagnostics preference");
        let back: Preferences =
            serde_json::from_str(&json).expect("deserialize diagnostics preference");
        assert!(back.detailed_logging);
    }

    /// Both split geometries are plain widths, and both must survive a round
    /// trip — the provider divider was persisted later than the sidebar, so
    /// this pins them together.
    #[test]
    fn split_widths_round_trip() {
        assert_eq!(
            Preferences::default().provider_list_width,
            DEFAULT_PROVIDER_LIST_WIDTH
        );

        let prefs = Preferences {
            sidebar_width: 300.0,
            provider_list_width: 288.0,
            ..Preferences::default()
        };
        let back: Preferences =
            serde_json::from_str(&serde_json::to_string(&prefs).expect("serialize"))
                .expect("deserialize");
        assert_eq!(back.sidebar_width, 300.0);
        assert_eq!(back.provider_list_width, 288.0);
    }

    #[test]
    fn atomic_save_replaces_an_existing_preferences_file() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "truncated old contents").expect("seed old file");
        let prefs = Preferences {
            sidebar_width: 336.0,
            language: Language::En,
            hide_settings_info_buttons: true,
            ..Preferences::default()
        };

        save_to_path(&path, &prefs).expect("save preferences atomically");

        let saved = std::fs::read_to_string(path).expect("read saved preferences");
        let parsed: Preferences = serde_json::from_str(&saved).expect("saved JSON must parse");
        assert_eq!(parsed.sidebar_width, 336.0);
        assert_eq!(parsed.language, Language::En);
        assert!(parsed.hide_settings_info_buttons);
    }

    #[test]
    fn window_geometry_round_trips() {
        let prefs = Preferences {
            window: Some(WindowGeometry {
                x: -12.5,
                y: 40.0,
                width: 1180.0,
                height: 760.0,
            }),
            settings_window: Some(WindowGeometry {
                x: 120.0,
                y: 80.0,
                width: 820.0,
                height: 560.0,
            }),
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize");
        let back: Preferences = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.window, prefs.window);
        assert_eq!(back.settings_window, prefs.settings_window);
    }

    /// Language keys are the stable dropdown identifiers; the round trip
    /// must hold and unknown keys must land on the default.
    #[test]
    fn language_key_round_trip() {
        for lang in Language::all() {
            assert_eq!(Language::from_key(lang.key()), lang);
        }
        assert_eq!(Language::from_key("nope"), Language::ZhCn);
        for font in ComposerFont::all() {
            assert_eq!(ComposerFont::from_key(font.key()), font);
        }
    }

    #[test]
    fn provider_profiles_and_plaintext_secret_round_trip() {
        let prefs = Preferences {
            provider_profiles: vec![ProviderProfile {
                id: "profile-1".into(),
                name: "Local gateway".into(),
                base_url: "http://localhost:8080/v1".into(),
                api_key: crate::llm::SecretString::new("plain-text-key"),
                protocol: crate::llm::Protocol::Responses,
                compatibility: crate::llm::CompatibilityProfile::default(),
                models: vec![crate::llm::ModelConfig {
                    id: "model-1".into(),
                    model_id: "gpt-compatible".into(),
                    display_name: Some("Local model".into()),
                }],
            }],
            last_model_selection: Some(ModelSelection {
                profile_id: "profile-1".into(),
                model_id: "model-1".into(),
            }),
            ..Preferences::default()
        };

        let json = serde_json::to_string(&prefs).expect("serialize provider settings");
        assert!(json.contains("plain-text-key"));
        let back: Preferences = serde_json::from_str(&json).expect("deserialize provider settings");
        assert_eq!(back.provider_profiles, prefs.provider_profiles);
        assert_eq!(back.last_model_selection, prefs.last_model_selection);
    }

    #[test]
    fn provider_preferences_reject_unknown_nested_fields() {
        let prefs = Preferences {
            provider_profiles: vec![ProviderProfile {
                id: "profile-1".into(),
                name: "Provider".into(),
                base_url: "https://example.com/v1".into(),
                api_key: crate::llm::SecretString::default(),
                protocol: crate::llm::Protocol::Responses,
                compatibility: crate::llm::CompatibilityProfile::default(),
                models: vec![crate::llm::ModelConfig {
                    id: "model-1".into(),
                    model_id: "gpt".into(),
                    display_name: None,
                }],
            }],
            last_model_selection: Some(ModelSelection {
                profile_id: "profile-1".into(),
                model_id: "model-1".into(),
            }),
            ..Preferences::default()
        };

        for path in [
            "/provider_profiles/0/legacy",
            "/provider_profiles/0/models/0/legacy",
            "/provider_profiles/0/compatibility/legacy",
            "/last_model_selection/legacy",
        ] {
            let mut value = serde_json::to_value(&prefs).expect("serialize");
            value
                .pointer_mut(path.rsplit_once('/').map_or("", |(parent, _)| parent))
                .and_then(serde_json::Value::as_object_mut)
                .expect("object")
                .insert("legacy".into(), serde_json::Value::Bool(true));
            assert!(
                serde_json::from_value::<Preferences>(value).is_err(),
                "{path}"
            );
        }
    }
}
