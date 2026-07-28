//! Searchable model list used by the title-bar model picker.
//!
//! The committed model remains a [`ModelSelection`] owned by the active
//! conversation. `IndexPath` is only the list's temporary keyboard cursor, so
//! filtering and regrouping can never invalidate the committed value.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Pixels,
    Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, IndexPath, Sizable as _, Size, StyleSized as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    list::{List, ListDelegate, ListState},
    popover::{Popover, PopoverState},
    searchable_list::SearchableListItemElement,
    v_flex,
};
use rust_i18n::t;

use crate::{llm::ModelSelection, preferences, providers, providers::SelectableModel};

type ConfirmHandler = Rc<dyn Fn(ModelSelection, &mut App) -> bool>;

#[derive(Clone, Default)]
struct PopoverDismissHandle(Rc<RefCell<Option<WeakEntity<PopoverState>>>>);

impl PopoverDismissHandle {
    fn bind(&self, state: WeakEntity<PopoverState>) {
        *self.0.borrow_mut() = Some(state);
    }

    fn dismiss(&self, window: &mut Window, cx: &mut App) -> bool {
        let Some(state) = self.0.borrow().as_ref().and_then(WeakEntity::upgrade) else {
            return false;
        };
        window.defer(cx, move |window, cx| {
            state.update(cx, |state, cx| state.dismiss(window, cx));
        });
        true
    }

    fn dismiss_then(
        &self,
        window: &mut Window,
        cx: &mut App,
        after: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        let state = self.0.borrow().as_ref().and_then(WeakEntity::upgrade);
        window.defer(cx, move |window, cx| {
            if let Some(state) = state {
                state.update(cx, |state, cx| state.dismiss(window, cx));
            }
            after(window, cx);
        });
    }
}

/// Width at which the pill stops growing and the model name truncates.  Longer
/// model ids were clipping noticeably at the previous 280px, so this buys ~20%
/// more room; much beyond that and the pill starts eating into the title bar's
/// drag region on a narrow window.
const MODEL_PILL_MAX_WIDTH: Pixels = px(336.);
const MODEL_MENU_WIDTH: Pixels = px(320.);
const MODEL_MENU_HEIGHT: Pixels = px(320.);

/// Complete state boundary for the title-bar model picker.
pub(crate) struct ModelPicker {
    list: Entity<ListState<ModelListDelegate>>,
    models: Vec<SelectableModel>,
    open: bool,
    dismiss_pending: bool,
    selection: Option<ModelSelection>,
    label: Option<(SharedString, SharedString)>,
    pending: bool,
    on_confirm: ConfirmHandler,
    popover: PopoverDismissHandle,
    _catalog_subscription: Subscription,
}

impl ModelPicker {
    pub(crate) fn new(
        selection: Option<ModelSelection>,
        pending: bool,
        on_confirm: impl Fn(ModelSelection, &mut App) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let models = providers::selectable_models(cx);
        let label = find_model_label(&models, selection.as_ref());
        let on_confirm = Rc::new(on_confirm);
        let popover = PopoverDismissHandle::default();
        let catalog_subscription =
            cx.observe_global_in::<preferences::Prefs>(window, |this, window, cx| {
                let models = providers::selectable_models(cx);
                if this.models != models {
                    this.refresh_catalog(models, window, cx);
                    cx.notify();
                }
            });
        let list = new_model_list(
            models.clone(),
            selection.clone(),
            on_confirm.clone(),
            popover.clone(),
            window,
            cx,
        );

        Self {
            list,
            models,
            open: false,
            dismiss_pending: false,
            selection,
            label,
            pending,
            on_confirm,
            popover,
            _catalog_subscription: catalog_subscription,
        }
    }

    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open && !self.dismiss_pending {
            self.dismiss_pending = self.popover.dismiss(window, cx);
        }
    }

    pub(crate) fn set_conversation(
        &mut self,
        selection: Option<ModelSelection>,
        pending: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selection_changed = self.selection != selection;
        let pending_changed = self.pending != pending;

        self.pending = pending;
        if pending {
            self.dismiss(window, cx);
        }
        if selection_changed {
            self.selection = selection;
            self.refresh_catalog(self.models.clone(), window, cx);
        }
        if selection_changed || pending_changed {
            cx.notify();
        }
    }

    fn refresh_catalog(
        &mut self,
        models: Vec<SelectableModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.label = find_model_label(&models, self.selection.as_ref());
        self.models = models.clone();
        let selection = self.selection.clone();
        self.list.update(cx, |list, cx| {
            list.delegate_mut().set_models(models, selection);
            let cursor = list.delegate().initial_index();
            list.set_selected_index(cursor, window, cx);
            cx.notify();
        });
    }

    fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open {
            let models = providers::selectable_models(cx);
            self.label = find_model_label(&models, self.selection.as_ref());
            self.models = models.clone();
            let list = new_model_list(
                models,
                self.selection.clone(),
                self.on_confirm.clone(),
                self.popover.clone(),
                window,
                cx,
            );
            defer_model_list_focus(list.clone(), window, cx);
            self.list = list;
        }

        self.open = open;
        self.dismiss_pending = false;
        cx.notify();
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if let Some((profile, model)) = self.label.clone() {
            h_flex()
                .min_w_0()
                .gap_1p5()
                .child(div().flex_shrink_0().opacity(0.55).child(profile))
                .child(div().min_w_0().truncate().child(model))
                .into_any_element()
        } else {
            div()
                .min_w_0()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(if self.selection.is_some() {
                    t!("chat.unavailable_model").to_string()
                } else {
                    t!("chat.select_model").to_string()
                })
                .into_any_element()
        };

        let list = self.list.clone();
        let popover = self.popover.clone();
        Popover::new("model-picker")
            .p_0()
            .text_sm()
            .open(self.open)
            .on_open_change(cx.listener(|this, open, window, cx| {
                this.set_open(*open, window, cx);
            }))
            .trigger(
                Button::new("model-pill")
                    .ghost()
                    .small()
                    .dropdown_caret(true)
                    .disabled(self.pending)
                    .max_w(MODEL_PILL_MAX_WIDTH)
                    .child(title),
            )
            .track_focus(&self.list.focus_handle(cx))
            .content(move |_, _, cx| {
                popover.bind(cx.weak_entity());
                List::new(&list)
                    .small()
                    .p_1()
                    .search_placeholder(t!("chat.search_model").to_string())
            })
            .w(MODEL_MENU_WIDTH)
            .h(MODEL_MENU_HEIGHT)
    }
}

#[derive(Clone)]
struct ModelItem {
    selection: ModelSelection,
    model_name: SharedString,
}

struct ModelGroup {
    profile_name: SharedString,
    items: Vec<ModelItem>,
}

/// Delegate for the searchable model list inside the title-bar popover.
struct ModelListDelegate {
    catalog: ModelCatalog,
    cursor: Option<IndexPath>,
    on_confirm: ConfirmHandler,
    popover: PopoverDismissHandle,
}

struct ModelCatalog {
    models: Vec<SelectableModel>,
    groups: Vec<ModelGroup>,
    query: String,
    committed: Option<ModelSelection>,
}

impl ModelCatalog {
    fn new(models: Vec<SelectableModel>, committed: Option<ModelSelection>) -> Self {
        let mut this = Self {
            models,
            groups: Vec::new(),
            query: String::new(),
            committed,
        };
        this.rebuild();
        this
    }

    /// Replace the provider projection while preserving the current query.
    fn set_models(&mut self, models: Vec<SelectableModel>, committed: Option<ModelSelection>) {
        self.models = models;
        self.committed = committed;
        self.rebuild();
    }

    /// Locate the committed model in the current filtered grouping.
    fn committed_index(&self) -> Option<IndexPath> {
        let committed = self.committed.as_ref()?;
        self.groups.iter().enumerate().find_map(|(section, group)| {
            group
                .items
                .iter()
                .position(|item| &item.selection == committed)
                .map(|row| IndexPath::default().section(section).row(row))
        })
    }

    fn item(&self, ix: IndexPath) -> Option<&ModelItem> {
        self.groups.get(ix.section)?.items.get(ix.row)
    }

    fn apply_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let query = self.query.to_lowercase();
        let mut groups: Vec<ModelGroup> = Vec::new();
        let mut current_profile: Option<&str> = None;

        for model in &self.models {
            let matches = query.is_empty()
                || model.model_name.to_lowercase().contains(&query)
                || model.profile_name.to_lowercase().contains(&query);
            if !matches {
                continue;
            }

            let profile_id = model.selection.profile_id.as_str();
            let item = ModelItem {
                selection: model.selection.clone(),
                model_name: model.model_name.clone().into(),
            };

            match groups.last_mut() {
                Some(group) if current_profile == Some(profile_id) => group.items.push(item),
                _ => {
                    current_profile = Some(profile_id);
                    groups.push(ModelGroup {
                        profile_name: model.profile_name.clone().into(),
                        items: vec![item],
                    });
                }
            }
        }

        self.groups = groups;
    }
}

impl ModelListDelegate {
    fn new(
        models: Vec<SelectableModel>,
        committed: Option<ModelSelection>,
        on_confirm: ConfirmHandler,
        popover: PopoverDismissHandle,
    ) -> Self {
        Self {
            catalog: ModelCatalog::new(models, committed),
            cursor: None,
            on_confirm,
            popover,
        }
    }

    fn set_models(&mut self, models: Vec<SelectableModel>, committed: Option<ModelSelection>) {
        self.catalog.set_models(models, committed);
        self.cursor = None;
    }

    fn committed_index(&self) -> Option<IndexPath> {
        self.catalog.committed_index()
    }

    fn initial_index(&self) -> Option<IndexPath> {
        self.committed_index().or_else(|| {
            self.catalog
                .groups
                .first()
                .is_some_and(|group| !group.items.is_empty())
                .then(IndexPath::default)
        })
    }
}

fn new_model_list(
    models: Vec<SelectableModel>,
    committed: Option<ModelSelection>,
    on_confirm: ConfirmHandler,
    popover: PopoverDismissHandle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ListState<ModelListDelegate>> {
    let list = cx.new(|cx| {
        ListState::new(
            ModelListDelegate::new(models, committed, on_confirm, popover),
            window,
            cx,
        )
        .searchable(true)
    });
    list.update(cx, |list, cx| {
        let cursor = list.delegate().initial_index();
        list.set_selected_index(cursor, window, cx);
    });
    list
}

fn defer_model_list_focus(
    list: Entity<ListState<ModelListDelegate>>,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        list.update(cx, |list, cx| list.focus(window, cx));
    });
}

fn find_model_label(
    models: &[SelectableModel],
    selection: Option<&ModelSelection>,
) -> Option<(SharedString, SharedString)> {
    let selection = selection?;
    models
        .iter()
        .find(|model| &model.selection == selection)
        .map(|model| {
            (
                model.profile_name.clone().into(),
                model.model_name.clone().into(),
            )
        })
}

impl ListDelegate for ModelListDelegate {
    type Item = SearchableListItemElement;

    fn sections_count(&self, _: &App) -> usize {
        self.catalog.groups.len()
    }

    fn items_count(&self, section: usize, _: &App) -> usize {
        self.catalog
            .groups
            .get(section)
            .map_or(0, |group| group.items.len())
    }

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.catalog.apply_query(query);
        cx.spawn_in(window, async move |list, window| {
            _ = list.update_in(window, |list, window, cx| {
                // ListState chooses from its pre-search row cache immediately
                // after perform_search returns. Resolve against the new
                // projection once the current update has released the entity.
                let cursor = list.delegate().initial_index();
                list.set_selected_index(cursor, window, cx);
                cx.notify();
            });
        })
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let item = self.catalog.item(ix)?;
        let confirmed = self.catalog.committed.as_ref() == Some(&item.selection);

        Some(
            SearchableListItemElement::new(ix.row)
                .checked(confirmed)
                .small()
                .child(div().whitespace_nowrap().child(item.model_name.clone())),
        )
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let group = self.catalog.groups.get(section)?;

        Some(
            div()
                .py_0p5()
                .px_2()
                .list_size(Size::Small)
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(group.profile_name.clone()),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let catalog_empty = self.catalog.models.is_empty();
        let popover = self.popover.clone();
        v_flex()
            .size_full()
            .justify_center()
            .items_center()
            .gap_2()
            .px_2()
            .text_xs()
            .child(if catalog_empty {
                t!("chat.no_models").to_string()
            } else {
                t!("chat.no_matching_models").to_string()
            })
            .when(catalog_empty, |this| {
                this.child(
                    Button::new("model-empty-settings")
                        .outline()
                        .xsmall()
                        .icon(IconName::Settings)
                        .label(t!("account.settings").to_string())
                        .on_click(move |_, window, cx| {
                            popover.dismiss_then(window, cx, |window, cx| {
                                window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
                            });
                        }),
                )
            })
            .into_any_element()
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.cursor = ix;
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(selection) = self
            .cursor
            .and_then(|ix| self.catalog.item(ix))
            .map(|item| item.selection.clone())
        else {
            return;
        };

        if (self.on_confirm)(selection.clone(), cx) {
            self.catalog.committed = Some(selection);
            self.popover.dismiss(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{Focusable as _, TestAppContext};

    use crate::llm::{CompatibilityProfile, ModelConfig, Protocol, ProviderProfile, SecretString};

    use super::*;

    fn model(profile_id: &str, profile_name: &str, model_name: &str) -> SelectableModel {
        SelectableModel {
            selection: ModelSelection {
                profile_id: profile_id.into(),
                model_id: format!("{profile_id}-{model_name}"),
            },
            profile_name: profile_name.into(),
            model_name: model_name.into(),
        }
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog::new(
            vec![
                model("openai", "OpenAI", "gpt-5.2"),
                model("openai", "OpenAI", "gpt-5.2-mini"),
                model("anthropic", "Anthropic", "claude-fable-5"),
            ],
            None,
        )
    }

    fn grouping(catalog: &ModelCatalog) -> Vec<(String, Vec<String>)> {
        catalog
            .groups
            .iter()
            .map(|group| {
                let rows = group
                    .items
                    .iter()
                    .map(|item| item.model_name.to_string())
                    .collect();
                (group.profile_name.to_string(), rows)
            })
            .collect()
    }

    fn provider_profile(model_name: &str) -> ProviderProfile {
        ProviderProfile {
            id: "provider".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "model".into(),
                model_id: "vendor/model".into(),
                display_name: Some(model_name.into()),
            }],
        }
    }

    #[test]
    fn models_are_sectioned_by_provider() {
        assert_eq!(
            grouping(&catalog()),
            vec![
                (
                    "OpenAI".to_string(),
                    vec!["gpt-5.2".to_string(), "gpt-5.2-mini".to_string()]
                ),
                ("Anthropic".to_string(), vec!["claude-fable-5".to_string()]),
            ]
        );
    }

    #[test]
    fn provider_and_model_queries_filter_the_catalog() {
        let mut catalog = catalog();

        catalog.apply_query("anthro");
        assert_eq!(
            grouping(&catalog),
            vec![("Anthropic".to_string(), vec!["claude-fable-5".to_string()])]
        );

        catalog.apply_query("mini");
        assert_eq!(
            grouping(&catalog),
            vec![("OpenAI".to_string(), vec!["gpt-5.2-mini".to_string()])]
        );
    }

    #[test]
    fn a_non_matching_query_is_empty_even_with_a_committed_model() {
        let committed = ModelSelection {
            profile_id: "openai".into(),
            model_id: "openai-gpt-5.2".into(),
        };
        let mut catalog = catalog();
        catalog.committed = Some(committed);

        catalog.apply_query("does-not-exist");

        assert!(catalog.groups.is_empty());
        assert_eq!(catalog.committed_index(), None);
    }

    #[test]
    fn committed_index_is_resolved_from_the_current_filter() {
        let committed = ModelSelection {
            profile_id: "anthropic".into(),
            model_id: "anthropic-claude-fable-5".into(),
        };
        let mut catalog = catalog();
        catalog.committed = Some(committed);

        assert_eq!(
            catalog.committed_index(),
            Some(IndexPath::default().section(1).row(0))
        );

        catalog.apply_query("claude");

        assert_eq!(
            catalog.committed_index(),
            Some(IndexPath::default().section(0).row(0))
        );
    }

    #[test]
    fn same_named_providers_stay_separate_sections() {
        let catalog = ModelCatalog::new(
            vec![
                model("first", "Gateway", "gpt-5.2"),
                model("second", "Gateway", "claude-fable-5"),
            ],
            None,
        );

        assert_eq!(
            grouping(&catalog),
            vec![
                ("Gateway".to_string(), vec!["gpt-5.2".to_string()]),
                ("Gateway".to_string(), vec!["claude-fable-5".to_string()]),
            ]
        );
    }

    #[gpui::test]
    fn picker_observes_catalog_changes_outside_render(cx: &mut TestAppContext) {
        let selection = ModelSelection {
            profile_id: "provider".into(),
            model_id: "model".into(),
        };
        cx.update(|cx| {
            gpui_component::init(cx);
            let prefs = preferences::Preferences {
                provider_profiles: vec![provider_profile("Initial")],
                ..Default::default()
            };
            preferences::init_global(prefs, cx);
        });
        let cx = cx.add_empty_window();
        let picker = cx.update(|window, cx| {
            cx.new(|cx| ModelPicker::new(selection.clone().into(), false, |_, _| true, window, cx))
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(picker.read(cx).label.as_ref().unwrap().1, "Initial");
        });

        cx.update(|_, cx| {
            providers::update_model_in_memory("provider", "model", cx, |model| {
                model.display_name = Some("Updated".into());
            });
        });
        cx.run_until_parked();

        cx.update(|_, cx| {
            assert_eq!(picker.read(cx).label.as_ref().unwrap().1, "Updated");
        });
    }

    #[gpui::test]
    fn dismiss_handle_restores_focus_through_popover_state(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let cx = cx.add_empty_window();
        let (previous_focus, popover) = cx.update(|window, cx| {
            let previous_focus = cx.focus_handle();
            previous_focus.focus(window, cx);
            let popover = cx.new(|cx| PopoverState::new(false, cx));
            popover.update(cx, |state, cx| state.show(window, cx));
            (previous_focus, popover)
        });
        cx.update(|window, cx| {
            assert!(popover.read(cx).focus_handle(cx).is_focused(window));
        });

        let dismiss = PopoverDismissHandle::default();
        dismiss.bind(popover.downgrade());
        cx.update(|window, cx| assert!(dismiss.dismiss(window, cx)));
        cx.run_until_parked();

        cx.update(|window, cx| {
            assert!(previous_focus.is_focused(window));
            assert!(!popover.read(cx).focus_handle(cx).is_focused(window));
        });
    }

    #[gpui::test]
    fn deferred_focus_moves_from_replaced_list_to_new_search_input(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let cx = cx.add_empty_window();
        let create_list = |window: &mut Window, cx: &mut App| {
            new_model_list(
                catalog().models,
                None,
                Rc::new(|_, _| true),
                PopoverDismissHandle::default(),
                window,
                cx,
            )
        };

        let old_list = cx.update(|window, cx| create_list(window, cx));
        cx.update(|window, cx| {
            old_list.update(cx, |list, cx| list.focus(window, cx));
        });
        cx.update(|window, cx| {
            assert!(old_list.read(cx).focus_handle(cx).is_focused(window));
        });

        let new_list = cx.update(|window, cx| {
            let list = create_list(window, cx);
            defer_model_list_focus(list.clone(), window, cx);
            list
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            assert!(new_list.read(cx).focus_handle(cx).is_focused(window));
            assert!(!old_list.read(cx).focus_handle(cx).is_focused(window));
        });
    }

    #[gpui::test]
    fn reopening_after_search_resets_all_state_and_repeated_query_confirms(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            let prefs = preferences::Preferences {
                provider_profiles: vec![provider_profile("gpt-5.2")],
                ..Default::default()
            };
            preferences::init_global(prefs, cx);
        });
        let confirmed = Rc::new(RefCell::new(Vec::new()));
        let cx = cx.add_empty_window();
        let picker = cx.update({
            let confirmed = confirmed.clone();
            move |window, cx| {
                cx.new(|cx| {
                    ModelPicker::new(
                        None,
                        false,
                        move |selection, _| {
                            confirmed.borrow_mut().push(selection);
                            true
                        },
                        window,
                        cx,
                    )
                })
            }
        });

        cx.update(|window, cx| {
            picker.update(cx, |picker, cx| picker.set_open(true, window, cx));
        });
        cx.run_until_parked();
        let first_list = cx.update(|_, cx| picker.read(cx).list.clone());

        cx.update(|window, cx| {
            first_list.update(cx, |list, cx| {
                list.delegate_mut()
                    .perform_search("does-not-exist", window, cx)
                    .detach();
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(first_list.read(cx).delegate().sections_count(cx), 0);
            assert_eq!(first_list.read(cx).selected_index(), None);
        });

        cx.update(|window, cx| {
            first_list.update(cx, |list, cx| {
                list.delegate_mut()
                    .perform_search("gpt", window, cx)
                    .detach();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(first_list.read(cx).delegate().sections_count(cx), 1);
            assert_eq!(
                first_list.read(cx).selected_index(),
                Some(IndexPath::default())
            );
            first_list.update(cx, |list, cx| {
                list.delegate_mut().confirm(false, window, cx)
            });
        });
        assert_eq!(confirmed.borrow().len(), 1);

        cx.update(|window, cx| {
            picker.update(cx, |picker, cx| {
                picker.set_open(false, window, cx);
                picker.set_open(true, window, cx);
            });
        });
        cx.run_until_parked();
        let second_list = cx.update(|_, cx| picker.read(cx).list.clone());
        assert_ne!(first_list.entity_id(), second_list.entity_id());
        cx.update(|_, cx| {
            assert_eq!(second_list.read(cx).delegate().sections_count(cx), 1);
            assert_eq!(
                second_list.read(cx).selected_index(),
                Some(IndexPath::default())
            );
        });

        cx.update(|window, cx| {
            second_list.update(cx, |list, cx| {
                list.delegate_mut()
                    .perform_search("gpt", window, cx)
                    .detach();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(second_list.read(cx).delegate().sections_count(cx), 1);
            assert_eq!(
                second_list.read(cx).selected_index(),
                Some(IndexPath::default())
            );
            second_list.update(cx, |list, cx| {
                list.delegate_mut().confirm(false, window, cx)
            });
        });
        assert_eq!(confirmed.borrow().len(), 2);
    }
}
