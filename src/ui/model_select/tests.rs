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
        cx.new(|cx| ModelPicker::new(selection.clone().into(), |_, _| true, window, cx))
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
fn reopening_after_search_resets_all_state_and_repeated_query_confirms(cx: &mut TestAppContext) {
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
