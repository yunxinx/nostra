use gpui::{AppContext as _, Entity, TestAppContext, px};
use gpui_component::Root;
use rust_i18n::t;

use super::{
    LIST_MAX_WIDTH, LIST_MIN_WIDTH, ModelField, ModelFieldBinding, ProvidersPage,
    changed_list_width, clamp_list_width,
};
use crate::llm::{CompatibilityProfile, ModelConfig, Protocol, ProviderProfile, SecretString};
use crate::preferences;

fn redraw(cx: &mut gpui::VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect("element should be drawn");
    cx.simulate_click(bounds.center(), Default::default());
    redraw(cx);
}

fn add_providers_window(
    cx: &mut TestAppContext,
    profiles: Vec<ProviderProfile>,
) -> (Entity<ProvidersPage>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        preferences::init_global(
            preferences::Preferences {
                provider_profiles: profiles,
                ..Default::default()
            },
            cx,
        );
    });
    let (root, cx) = cx.add_window_view(|window, cx| {
        let page = cx.new(|cx| ProvidersPage::new(window, cx));
        Root::new(page, window, cx)
    });
    let page = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ProvidersPage>()
            .expect("Root must contain ProvidersPage")
    });
    (page, cx)
}

/// A persisted width outside the drag range (hand-edited file, or a range
/// narrowed in a later version) must not place the divider somewhere the
/// user can't drag it back from.
#[test]
fn persisted_list_width_is_gated_by_the_drag_range() {
    assert_eq!(clamp_list_width(px(240.)), px(240.));
    assert_eq!(clamp_list_width(px(40.)), LIST_MIN_WIDTH);
    assert_eq!(clamp_list_width(px(9_000.)), LIST_MAX_WIDTH);
}

#[test]
fn resized_list_width_is_normalized_and_only_writes_changes() {
    assert_eq!(changed_list_width(220.0, px(220.)), None);
    assert_eq!(changed_list_width(220.0, px(288.)), Some(288.0));
    assert_eq!(
        changed_list_width(220.0, px(9_000.)),
        Some(LIST_MAX_WIDTH.as_f32())
    );
}

/// The page seeds its split from preferences, so reopening the settings
/// window restores the divider instead of snapping back to the default.
#[gpui::test]
fn page_restores_the_persisted_divider_position(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        preferences::init_global(
            preferences::Preferences {
                provider_list_width: 288.0,
                ..Default::default()
            },
            cx,
        );
    });
    let cx = cx.add_empty_window();
    let page = cx.update(|window, cx| cx.new(|cx| ProvidersPage::new(window, cx)));

    cx.update(|_, cx| assert_eq!(page.read(cx).list_width, px(288.)));
}

fn edit_profile(id: &str, upstream_id: &str, display_name: &str) -> ProviderProfile {
    ProviderProfile {
        id: id.into(),
        name: id.into(),
        base_url: "https://example.com/v1".into(),
        api_key: SecretString::default(),
        protocol: Protocol::Responses,
        compatibility: CompatibilityProfile::default(),
        models: vec![
            ModelConfig {
                id: "edited".into(),
                model_id: upstream_id.into(),
                display_name: Some(display_name.into()),
            },
            ModelConfig {
                id: "existing".into(),
                model_id: "vendor/taken".into(),
                display_name: Some("Taken".into()),
            },
        ],
    }
}

#[gpui::test]
fn inline_confirm_deletes_original_profile_after_selection_switch(cx: &mut TestAppContext) {
    let profiles = vec![
        edit_profile("alpha", "vendor/alpha", "Alpha"),
        edit_profile("beta", "vendor/beta", "Beta"),
    ];
    let (page, cx) = add_providers_window(cx, profiles);
    redraw(cx);

    click(cx, "provider-actions-alpha");
    cx.simulate_keystrokes("down enter");
    redraw(cx);
    assert_eq!(
        page.read_with(cx, |page, _| page.confirming.clone()),
        Some("alpha".into())
    );

    cx.update(|window, cx| {
        page.update(cx, |page, cx| page.select("beta".into(), window, cx));
    });
    redraw(cx);
    click(cx, "provider-delete-confirm-alpha-confirm");

    assert_eq!(
        page.read_with(cx, |page, _| page.selected.clone()),
        Some("beta".into())
    );
    assert_eq!(
        cx.update(|_, cx| {
            crate::providers::profiles(cx)
                .iter()
                .map(|profile| profile.id.clone())
                .collect::<Vec<_>>()
        }),
        vec!["beta".to_string()]
    );
}

/// Every field on this page explains itself through a hover icon whose
/// text is looked up at render time.  A missing or misspelled key resolves
/// to the key path itself and would ship as visible gibberish, so each one
/// must exist in both locales.
#[test]
fn every_field_description_resolves_in_both_locales() {
    const KEYS: [&str; 13] = [
        "name_desc",
        "base_url_desc",
        "wire_api_desc",
        "api_key_desc",
        "models_desc",
        "compatibility_desc",
        "max_tokens_field_desc",
        "system_role_desc",
        "reasoning_field_desc",
        "responses_instructions_desc",
        "stream_usage_desc",
        "nullable_tool_fields_desc",
        "object_tool_arguments_desc",
    ];

    for key in KEYS {
        let path = format!("settings.providers.{key}");
        for locale in ["zh-CN", "en"] {
            let text = t!(&path, locale = locale);
            assert_ne!(text, path, "{path} is missing for {locale}");
            assert!(!text.is_empty(), "{path} is empty for {locale}");
        }
    }
}

#[test]
fn delete_profile_labels_resolve_in_both_locales() {
    for locale in ["zh-CN", "en"] {
        for key in [
            "delete_profile",
            "delete_profile_title",
            "delete_profile_confirm",
            "delete_profile_cancel",
        ] {
            let path = format!("settings.providers.{key}");
            assert_ne!(t!(&path, locale = locale).to_string(), path);
        }
    }
}

/// The unnamed-row label is resolved at render time and numbered, so it
/// must interpolate rather than leak a `%{index}` placeholder, and it must
/// follow whichever locale is active.
#[test]
fn unnamed_provider_label_is_numbered_per_locale() {
    for (locale, expected) in [("zh-CN", "未命名供应商 2"), ("en", "Unnamed provider 2")] {
        assert_eq!(
            t!("settings.providers.unnamed", locale = locale, index = 2),
            expected
        );
    }
}

#[test]
fn duplicate_model_notifications_resolve_in_both_locales() {
    for key in ["duplicate_model_id", "duplicate_model_name"] {
        let path = format!("settings.providers.{key}");
        for locale in ["zh-CN", "en"] {
            let text = t!(&path, locale = locale);
            assert_ne!(text, path, "{path} is missing for {locale}");
            assert!(!text.is_empty(), "{path} is empty for {locale}");
        }
    }
}

#[test]
fn duplicate_edits_retain_the_value_captured_on_focus() {
    let profile = edit_profile("owner", "vendor/original", "Original");
    for field in [ModelField::DisplayName, ModelField::UpstreamId] {
        let (initial, intermediate, duplicate) = match field {
            ModelField::DisplayName => ("Original", "Take", "Taken"),
            ModelField::UpstreamId => ("vendor/original", "vendor/take", "vendor/taken"),
        };
        let mut binding = ModelFieldBinding::new(&profile.id, "edited", field);
        binding.remember_focus(initial);

        assert_eq!(
            binding.accepted_value(&profile, intermediate.to_string()),
            Some(intermediate.to_string())
        );
        assert_eq!(
            binding.accepted_value(&profile, duplicate.to_string()),
            None
        );
        assert_eq!(binding.value_on_focus, initial);
    }
}
