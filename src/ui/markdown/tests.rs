use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};

use gpui::{
    Context, IntoElement, Modifiers, MouseButton, Render, TestAppContext, VisualTestContext, point,
};
use gpui_base::TextSelection;
use gpui_component::{ActiveTheme as _, Root};

use crate::runtime::{ContributionDefinition, ContributionId, ContributionRegistry, ScopeId};

use super::*;

struct CodeSelectionTestRoot {
    body: MarkdownBody,
}

struct DropSignal {
    drops: Arc<AtomicUsize>,
}

impl DropSignal {
    fn new(drops: Arc<AtomicUsize>) -> Self {
        Self { drops }
    }
}

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl Render for CodeSelectionTestRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(480.))
            .child(self.body.text_view(TextViewStyle::default()))
    }
}

fn init_markdown_test(cx: &mut TestAppContext) {
    let prefs = preferences::Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        preferences::init_global(prefs.clone(), cx);
        crate::appearance::theme::init(&prefs, cx);
    });
}

#[test]
fn markdown_contribution_snapshot_preserves_stable_builtin_order() {
    const SCOPE: ScopeId = ScopeId::new(800);
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    registry
        .register_batch(
            SCOPE,
            [
                code_block::fenced_code_contribution(),
                crate::ui::math::markdown_contribution(),
                extension_registry::cjk_emphasis_contribution(),
            ],
        )
        .expect("register built-in Markdown contributions");

    let snapshot = registry.snapshot(SCOPE).expect("Markdown snapshot");
    assert_eq!(
        snapshot
            .contributions()
            .iter()
            .map(|entry| (entry.id().as_str(), entry.order()))
            .collect::<Vec<_>>(),
        [
            ("nostra.markdown.cjk", 10),
            ("nostra.markdown.math", 20),
            ("nostra.markdown.fenced-code", 30),
        ]
    );
}

#[test]
fn markdown_contribution_installation_uses_snapshot_order_and_body_context() {
    const SCOPE: ScopeId = ScopeId::new(801);
    const EARLY: ContributionId = ContributionId::new("nostra.markdown.test-early");
    const LATE: ContributionId = ContributionId::new("nostra.markdown.test-late");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let contribution = |id, order, label| {
        let calls = Rc::clone(&calls);
        ContributionDefinition::new(
            id,
            order,
            MarkdownExtensionInstaller::new(move |extensions, context| {
                calls
                    .borrow_mut()
                    .push((label, context.owner_id(), context.source_offset()));
                extensions
            }),
        )
    };
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    registry
        .register_batch(
            SCOPE,
            [
                contribution(LATE, 20, "late"),
                contribution(EARLY, 10, "early"),
            ],
        )
        .expect("register test Markdown contributions");
    let snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("Markdown snapshot"));
    let context = MarkdownExtensionInstallContext::new(
        42,
        17,
        Arc::new(Mutex::new(preferences::Preferences::default())),
    );

    let _ = snapshot.install(&context);

    assert_eq!(
        calls.borrow().as_slice(),
        [("early", 42, 17), ("late", 42, 17)]
    );
}

#[gpui::test]
fn markdown_body_materializes_snapshots_once_and_rejects_stale_revision_completion(
    cx: &mut TestAppContext,
) {
    const SCOPE: ScopeId = ScopeId::new(802);
    const EXTENSION: ContributionId = ContributionId::new("nostra.markdown.test-revision");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let contribution = |label| {
        let calls = Rc::clone(&calls);
        ContributionDefinition::new(
            EXTENSION,
            10,
            MarkdownExtensionInstaller::new(move |extensions, context| {
                calls.borrow_mut().push((label, context.owner_id()));
                extensions
            }),
        )
    };
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    let old_registration = registry
        .register(SCOPE, contribution("old"))
        .expect("register old Markdown contribution");
    let old_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("old Markdown snapshot"));
    cx.update(gpui_component::init);
    let preferences = Arc::new(Mutex::new(preferences::Preferences::default()));
    let presentation = MarkdownPresentation::new(Arc::clone(&preferences), old_snapshot.clone());
    let mut body =
        cx.update(|cx| MarkdownBody::new_with_presentation("body", 42, &presentation, cx));

    assert_eq!(calls.borrow().as_slice(), [("old", 42)]);
    assert_eq!(body.extension_revision(), old_snapshot.revision());
    assert!(!body.update_extension_snapshot(&old_snapshot));
    let _ = body.text_view(TextViewStyle::default());
    let _ = body.text_view(TextViewStyle::default());
    assert_eq!(calls.borrow().as_slice(), [("old", 42)]);

    assert!(
        registry
            .revoke(&old_registration)
            .expect("revoke old Markdown contribution")
    );
    registry
        .register(SCOPE, contribution("new"))
        .expect("register new Markdown contribution");
    let new_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("new Markdown snapshot"));

    assert!(body.update_extension_snapshot(&new_snapshot));
    assert_eq!(body.extension_revision(), new_snapshot.revision());
    assert_eq!(calls.borrow().as_slice(), [("old", 42), ("new", 42)]);

    assert!(!body.update_extension_snapshot(&old_snapshot));
    assert_eq!(body.extension_revision(), new_snapshot.revision());
    assert_eq!(calls.borrow().as_slice(), [("old", 42), ("new", 42)]);
}

#[gpui::test]
fn newer_markdown_snapshot_reparses_existing_state_without_render_time_registry_access(
    cx: &mut TestAppContext,
) {
    const SCOPE: ScopeId = ScopeId::new(803);
    const EXTENSION: ContributionId = ContributionId::new("nostra.markdown.test-renderer");
    let contribution = |node_name: &'static str| {
        ContributionDefinition::new(
            EXTENSION,
            10,
            MarkdownExtensionInstaller::new(move |extensions, _| {
                extensions
                    .block_parser(move |node, cx| {
                        let markdown_ast::Node::Paragraph(_) = node else {
                            return None;
                        };
                        let source = cx.node_source(node)?;
                        Some(
                            MarkdownNode::new(node_name, ())
                                .text(source.to_string())
                                .markdown(source.to_string()),
                        )
                    })
                    .block_renderer(node_name, move |node, _, _| {
                        div()
                            .debug_selector(move || node_name.into())
                            .child(node.as_text().to_string())
                    })
            }),
        )
    };
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    let old_registration = registry
        .register(SCOPE, contribution("markdown-snapshot-old"))
        .expect("register old renderer");
    let old_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("old renderer snapshot"));
    init_markdown_test(cx);
    let preferences = Arc::new(Mutex::new(preferences::Preferences::default()));
    let presentation = MarkdownPresentation::new(preferences, old_snapshot.clone());
    let content = cx.update(|cx| {
        cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new_with_presentation("existing body", 43, &presentation, cx),
        })
    });
    let (_, cx) = cx.add_window_view(|window, cx| Root::new(content.clone(), window, cx));
    let cx: &mut VisualTestContext = cx;

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(cx.debug_bounds("markdown-snapshot-old").is_some());

    assert!(
        registry
            .revoke(&old_registration)
            .expect("revoke old renderer")
    );
    registry
        .register(SCOPE, contribution("markdown-snapshot-new"))
        .expect("register new renderer");
    let new_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("new renderer snapshot"));
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&new_snapshot));
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    assert!(cx.debug_bounds("markdown-snapshot-old").is_none());
    assert!(cx.debug_bounds("markdown-snapshot-new").is_some());

    content.update(cx, |root, cx| {
        assert!(!root.body.update_extension_snapshot(&old_snapshot));
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(cx.debug_bounds("markdown-snapshot-old").is_none());
    assert!(cx.debug_bounds("markdown-snapshot-new").is_some());
}

#[gpui::test]
fn replacing_and_removing_a_contribution_releases_parser_and_renderer_owners(
    cx: &mut TestAppContext,
) {
    const SCOPE: ScopeId = ScopeId::new(804);
    const EXTENSION: ContributionId = ContributionId::new("nostra.markdown.test-lifecycle");
    let contribution = |node_name: &'static str,
                        parser_drops: Arc<AtomicUsize>,
                        renderer_drops: Arc<AtomicUsize>,
                        parsed_sources: Arc<Mutex<Vec<String>>>| {
        ContributionDefinition::new(
            EXTENSION,
            10,
            MarkdownExtensionInstaller::new(move |extensions, _| {
                let parser_owner = DropSignal::new(Arc::clone(&parser_drops));
                let renderer_owner = DropSignal::new(Arc::clone(&renderer_drops));
                let parsed_sources = Arc::clone(&parsed_sources);
                extensions
                    .block_parser(move |node, cx| {
                        let _owner = &parser_owner;
                        let markdown_ast::Node::Paragraph(_) = node else {
                            return None;
                        };
                        let source = cx.node_source(node)?;
                        parsed_sources
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(source.to_string());
                        Some(
                            MarkdownNode::new(node_name, ())
                                .text(source.to_string())
                                .markdown(source.to_string()),
                        )
                    })
                    .block_renderer(node_name, move |node, _, _| {
                        let _owner = &renderer_owner;
                        div()
                            .debug_selector(move || node_name.into())
                            .child(node.as_text().to_string())
                    })
            }),
        )
    };
    let old_parser_drops = Arc::new(AtomicUsize::new(0));
    let old_renderer_drops = Arc::new(AtomicUsize::new(0));
    let old_sources = Arc::new(Mutex::new(Vec::new()));
    let new_parser_drops = Arc::new(AtomicUsize::new(0));
    let new_renderer_drops = Arc::new(AtomicUsize::new(0));
    let new_sources = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    let old_registration = registry
        .register(
            SCOPE,
            contribution(
                "markdown-lifecycle-old",
                Arc::clone(&old_parser_drops),
                Arc::clone(&old_renderer_drops),
                Arc::clone(&old_sources),
            ),
        )
        .expect("register old lifecycle contribution");
    let old_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("old lifecycle snapshot"));

    init_markdown_test(cx);
    let preferences = Arc::new(Mutex::new(preferences::Preferences::default()));
    let presentation = MarkdownPresentation::new(preferences, old_snapshot.clone());
    let content = cx.update(|cx| {
        cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new_with_presentation("alpha", 44, &presentation, cx),
        })
    });
    let body_entity = content.read_with(cx, |root, _| root.body.entity_id());
    let (_, cx) = cx.add_window_view(|window, cx| Root::new(content.clone(), window, cx));
    let cx: &mut VisualTestContext = cx;

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(cx.debug_bounds("markdown-lifecycle-old").is_some());

    cx.update(|_, cx| {
        content.update(cx, |root, cx| root.body.push_str(" before", cx));
    });
    assert!(
        registry
            .revoke(&old_registration)
            .expect("revoke old lifecycle contribution")
    );
    let new_registration = registry
        .register(
            SCOPE,
            contribution(
                "markdown-lifecycle-new",
                Arc::clone(&new_parser_drops),
                Arc::clone(&new_renderer_drops),
                Arc::clone(&new_sources),
            ),
        )
        .expect("register successor lifecycle contribution");
    let new_snapshot = MarkdownExtensionSnapshot::from(
        &registry
            .snapshot(SCOPE)
            .expect("successor lifecycle snapshot"),
    );
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&new_snapshot));
        root.body.push_str(" after", cx);
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    assert_eq!(old_parser_drops.load(Ordering::SeqCst), 1);
    assert_eq!(old_renderer_drops.load(Ordering::SeqCst), 1);
    assert!(cx.debug_bounds("markdown-lifecycle-old").is_none());
    assert!(cx.debug_bounds("markdown-lifecycle-new").is_some());
    assert!(
        new_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|source| source == "alpha before after")
    );
    assert_eq!(
        content
            .update(cx, |root, cx| root.body.select_all_text(cx))
            .trim_end(),
        "alpha before after"
    );
    assert_eq!(
        content.read_with(cx, |root, _| root.body.entity_id()),
        body_entity
    );

    assert!(
        registry
            .revoke(&new_registration)
            .expect("remove successor lifecycle contribution")
    );
    let empty_snapshot = MarkdownExtensionSnapshot::from(
        &registry.snapshot(SCOPE).expect("empty lifecycle snapshot"),
    );
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&empty_snapshot));
        root.body.push_str(" removed", cx);
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    assert_eq!(new_parser_drops.load(Ordering::SeqCst), 1);
    assert_eq!(new_renderer_drops.load(Ordering::SeqCst), 1);
    assert!(cx.debug_bounds("markdown-lifecycle-new").is_none());
    assert_eq!(
        content
            .update(cx, |root, cx| root.body.select_all_text(cx))
            .trim_end(),
        "alpha before after removed"
    );
    assert_eq!(
        content.read_with(cx, |root, _| root.body.entity_id()),
        body_entity
    );
}

#[gpui::test]
fn replacing_and_removing_fenced_code_releases_generation_owned_cache_and_task(
    cx: &mut TestAppContext,
) {
    const SCOPE: ScopeId = ScopeId::new(805);
    const OWNER_ID: u64 = 8_005;
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    let old_registration = registry
        .register(SCOPE, code_block::fenced_code_contribution())
        .expect("register old fenced-code contribution");
    let old_owner =
        MarkdownContributionOwner::new(FENCED_CODE_EXTENSION_ID, old_registration.generation());
    let old_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("old fenced snapshot"));
    let source = format!(
        "```rust\n{}\n```",
        "let value = 42;\nprintln!(\"{value}\");\n".repeat(700)
    );

    init_markdown_test(cx);
    reset_background_probe();
    let background_gates = block_background_highlights(2);
    let preferences = Arc::new(Mutex::new(preferences::Preferences::default()));
    let presentation = MarkdownPresentation::new(preferences, old_snapshot);
    let content = cx.update(|cx| {
        cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new_with_presentation(&source, OWNER_ID, &presentation, cx),
        })
    });
    let body_entity = content.read_with(cx, |root, _| root.body.entity_id());
    let (_, cx) = cx.add_window_view(|window, cx| Root::new(content.clone(), window, cx));
    let cx: &mut VisualTestContext = cx;

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let first = background_probe();
    assert_eq!(first.background_spawns, 1);
    assert_eq!(first.active_caches, 1);
    assert_eq!(first.active_task_owners, 1);
    assert_eq!(first.last_created_owner, Some(old_owner));

    cx.update(|_, cx| {
        content.update(cx, |root, cx| {
            root.body.push_str("\nbefore replacement", cx)
        });
    });
    assert!(
        registry
            .revoke(&old_registration)
            .expect("revoke old fenced-code contribution")
    );
    let successor_registration = registry
        .register(SCOPE, code_block::fenced_code_contribution())
        .expect("register successor fenced-code contribution");
    let successor_owner = MarkdownContributionOwner::new(
        FENCED_CODE_EXTENSION_ID,
        successor_registration.generation(),
    );
    let successor_snapshot = MarkdownExtensionSnapshot::from(
        &registry.snapshot(SCOPE).expect("successor fenced snapshot"),
    );
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&successor_snapshot));
        root.body.push_str(" after replacement", cx);
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let replaced = background_probe();
    assert_eq!(replaced.background_spawns, 2);
    assert_eq!(replaced.cache_releases, 1);
    assert_eq!(replaced.task_owner_releases, 1);
    assert_eq!(replaced.active_caches, 1);
    assert_eq!(replaced.active_task_owners, 1);
    assert_eq!(replaced.last_released_owner, Some(old_owner));
    assert_eq!(replaced.last_created_owner, Some(successor_owner));

    assert!(
        registry
            .revoke(&successor_registration)
            .expect("remove successor fenced-code contribution")
    );
    let empty_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("empty fenced snapshot"));
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&empty_snapshot));
        root.body.push_str(" after removal", cx);
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let removed = background_probe();
    assert_eq!(removed.cache_releases, 2);
    assert_eq!(removed.task_owner_releases, 2);
    assert_eq!(removed.active_caches, 0);
    assert_eq!(removed.active_task_owners, 0);
    assert_eq!(removed.last_released_owner, Some(successor_owner));
    assert_eq!(removed.background_installs, 0);

    drop(background_gates);
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert_eq!(
        background_probe().background_installs,
        0,
        "released highlight workers cannot install into a successor or removed cache"
    );
    let selected = content.update(cx, |root, cx| root.body.select_all_text(cx));
    assert!(selected.contains("let value = 42;"));
    assert!(
        selected
            .trim_end()
            .ends_with("before replacement after replacement after removal")
    );
    assert_eq!(
        content.read_with(cx, |root, _| root.body.entity_id()),
        body_entity
    );
}

#[gpui::test]
fn replacing_and_removing_math_releases_generation_owned_formula_cache(cx: &mut TestAppContext) {
    const SCOPE: ScopeId = ScopeId::new(806);
    const OWNER_ID: u64 = 8_006;
    const FORMULA_START: usize = 0;
    let mut registry = ContributionRegistry::<extension_registry::MarkdownExtensionKey>::new(SCOPE);
    let old_registration = registry
        .register(SCOPE, crate::ui::math::markdown_contribution())
        .expect("register old math contribution");
    let old_owner = MarkdownContributionOwner::new(
        crate::ui::math::MATH_EXTENSION_ID,
        old_registration.generation(),
    );
    let old_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("old math snapshot"));

    init_markdown_test(cx);
    let preferences = Arc::new(Mutex::new(preferences::Preferences::default()));
    let presentation = MarkdownPresentation::new(preferences, old_snapshot);
    let content = cx.update(|cx| {
        cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new_with_presentation("$x$", OWNER_ID, &presentation, cx),
        })
    });
    let (_, cx) = cx.add_window_view(|window, cx| Root::new(content.clone(), window, cx));
    let cx: &mut VisualTestContext = cx;

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let old_cache =
        crate::ui::math::formula_cache_snapshot_for_owner(old_owner, OWNER_ID, FORMULA_START)
            .expect("old formula cache");
    assert!(old_cache.active);

    assert!(
        registry
            .revoke(&old_registration)
            .expect("revoke old math contribution")
    );
    let successor_registration = registry
        .register(SCOPE, crate::ui::math::markdown_contribution())
        .expect("register successor math contribution");
    let successor_owner = MarkdownContributionOwner::new(
        crate::ui::math::MATH_EXTENSION_ID,
        successor_registration.generation(),
    );
    let successor_snapshot = MarkdownExtensionSnapshot::from(
        &registry.snapshot(SCOPE).expect("successor math snapshot"),
    );
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&successor_snapshot));
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let old_cache =
        crate::ui::math::formula_cache_snapshot_for_owner(old_owner, OWNER_ID, FORMULA_START)
            .expect("released old formula cache");
    assert!(!old_cache.active);
    assert_eq!(old_cache.release_count, 1);
    let successor_cache =
        crate::ui::math::formula_cache_snapshot_for_owner(successor_owner, OWNER_ID, FORMULA_START)
            .expect("successor formula cache");
    assert!(successor_cache.active);

    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(
        crate::ui::math::formula_cache_snapshot_for_owner(
            successor_owner,
            OWNER_ID,
            FORMULA_START,
        )
        .is_some_and(|cache| cache.active && cache.ready)
    );

    assert!(
        registry
            .revoke(&successor_registration)
            .expect("remove successor math contribution")
    );
    let empty_snapshot =
        MarkdownExtensionSnapshot::from(&registry.snapshot(SCOPE).expect("empty math snapshot"));
    content.update(cx, |root, cx| {
        assert!(root.body.update_extension_snapshot(&empty_snapshot));
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let successor_cache =
        crate::ui::math::formula_cache_snapshot_for_owner(successor_owner, OWNER_ID, FORMULA_START)
            .expect("released successor formula cache");
    assert!(!successor_cache.active);
    assert_eq!(successor_cache.release_count, 1);
    assert_eq!(successor_cache.image_drop_count, 1);
}

fn assert_fenced_code_drag_copy(cx: &mut TestAppContext, wrap: bool) {
    const OWNER_ID: u64 = 7;
    const SOURCE: &str = "```text\nfirst 你好\n\n🙂 third\n```";
    const CODE: &str = "first 你好\n\n🙂 third";

    init_markdown_test(cx);
    cx.update(|cx| {
        set_global_wrap_in_memory(wrap, cx);
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = true;
        });
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new(SOURCE, OWNER_ID, cx),
        });
        Root::new(content, window, cx)
    });
    let cx: &mut VisualTestContext = cx;

    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let code = cx
        .debug_bounds("markdown-code-line-7-0-0")
        .expect("continuous code bounds");
    let logical_line_height = code.size.height / 3.;
    assert!(logical_line_height > px(0.));

    let start = point(code.left() + px(1.), code.top() + logical_line_height / 2.);
    let end = point(
        code.right() - px(1.),
        code.bottom() - logical_line_height / 2.,
    );
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());

    let selected = cx.update(TextSelection::selected_text);
    assert_eq!(selected.trim_end_matches('\n'), CODE);

    cx.dispatch_action(gpui_component::input::Copy);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(CODE.to_string()),
        "drag-copy must preserve Unicode and empty lines without painted line numbers"
    );
}

#[gpui::test]
fn nowrap_fenced_code_drag_copy_preserves_lines_and_empty_line_height(cx: &mut TestAppContext) {
    assert_fenced_code_drag_copy(cx, false);
}

#[gpui::test]
fn wrapped_fenced_code_drag_copy_preserves_lines_and_empty_line_height(cx: &mut TestAppContext) {
    assert_fenced_code_drag_copy(cx, true);
}

#[gpui::test]
fn code_block_surfaces_remain_distinct_in_every_theme(cx: &mut TestAppContext) {
    use gpui_component::{Theme, ThemeMode};

    let prefs = preferences::Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        preferences::init_global(prefs.clone(), cx);
        crate::appearance::theme::init(&prefs, cx);

        for dark in [true, false] {
            Theme::change(
                if dark {
                    ThemeMode::Dark
                } else {
                    ThemeMode::Light
                },
                None,
                cx,
            );

            for name in crate::appearance::theme::theme_names(dark, cx) {
                crate::appearance::theme::select_theme_for_test(name.as_ref(), cx);

                let body = opaque_surface(cx.theme().background, cx.theme().muted);
                let header = code_header_surface(cx);
                let active_wrap = wrap_toggle_surface(true, cx).expect("active surface");
                assert!(wrap_toggle_surface(false, cx).is_none());
                assert!(
                    surface_contrast(header.color, body) >= MIN_ADJACENT_SURFACE_CONTRAST,
                    "{name}: code header must stand out from the code body"
                );
                assert!(
                    surface_contrast(active_wrap.color, header.color)
                        >= MIN_ADJACENT_SURFACE_CONTRAST,
                    "{name}: active wrap control must stand out from the code header"
                );
            }
        }
    });
}

#[test]
fn language_identifiers_are_ascii_case_insensitive() {
    let registry = gpui_component::highlighter::LanguageRegistry::singleton();
    for language in [
        "Python",
        "PYTHON",
        "Py",
        "PY",
        "Rust",
        "RS",
        "JavaScript",
        "JAVASCRIPT",
        "TypeScript",
        "TYPESCRIPT",
    ] {
        assert!(
            registry
                .language(&normalized_language_id(Some(language)))
                .is_some(),
            "{language} must resolve through the case-insensitive normalizer"
        );
    }

    let code = "name = 'world'\nprint(f'hello {name}')";
    let mut highlighter = SyntaxHighlighter::new(&normalized_language_id(Some("PYTHON")));
    let rope = Rope::from(code);
    assert!(highlighter.update(None, &rope, None));
    assert!(
        !highlighter
            .styles(&(0..code.len()), HighlightTheme::default_dark().as_ref())
            .is_empty(),
        "uppercase Python must produce syntax styles rather than plain text"
    );
}

#[gpui::test]
fn highlight_cache_matches_language_identifiers_case_insensitively(cx: &mut TestAppContext) {
    init_markdown_test(cx);
    let original = FencedCode {
        code: "print('hello')".into(),
        language: Some("Python".into()),
        start: 0,
    };
    let casing_changed = FencedCode {
        language: Some("PYTHON".into()),
        ..original.clone()
    };
    let cache = cx.update(|cx| {
        HighlightCache::new(
            &original,
            MarkdownContributionOwner::new(FENCED_CODE_EXTENSION_ID, 1),
            cx,
        )
    });

    assert!(cx.update(|cx| cache.matches(&casing_changed, cx)));
}

#[test]
fn large_mixed_case_languages_produce_syntax_styles() {
    let theme = HighlightTheme::default_dark();
    for (language, line) in [
        ("PYTHON", "name = 'world'\nprint(name)\n"),
        ("Rust", "fn main() { let value: usize = 42; }\n"),
    ] {
        let code = line.repeat(1000);
        assert!(code.len() > BG_HIGHLIGHT_BYTES);
        let styles = compute_code_styles(&code, &normalized_language_id(Some(language)), &theme);
        assert!(
            styles.len() > 1,
            "large {language} code must produce syntax styles"
        );
    }
}

/// A panicking highlighter (tree-sitter does panic on pathological streamed
/// input) must degrade to an empty style list instead of propagating the
/// panic — the block then renders as plain text rather than crashing the
/// render path (short block) or the foreground task (long block).
#[test]
fn compute_code_styles_recovers_from_panicking_highlighter() {
    let theme = HighlightTheme::default_dark();
    let styles = parse_code_styles(
        "fn main() { let value: usize = 42; }",
        "rust",
        &theme,
        |_, _, _| {
            panic!("tree-sitter panicked on pathological input");
        },
    );
    assert!(
        styles.is_empty(),
        "a panicking highlighter must degrade to an empty style list"
    );
}

#[test]
fn distinguishes_fenced_code_from_indented_code_source() {
    for source in ["```rust\nfn main() {}\n```", "   ~~~py\nprint(1)\n   ~~~"] {
        assert!(is_fenced_code_source(source), "fenced source: {source:?}");
    }
    for source in [
        "    print('indented')",
        "    ```not-a-fence",
        "\tprint('tab-indented')",
    ] {
        assert!(
            !is_fenced_code_source(source),
            "indented source: {source:?}"
        );
    }
}

#[gpui::test]
fn streamed_extension_syntax_stays_in_the_authoritative_text_state(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let mut body = cx.update(|cx| MarkdownBody::new("prefix `", 1, cx));

    cx.update(|cx| body.push_str("`", cx));
    cx.update(|cx| body.push_str("`rust\nfn main() {}\n```\n", cx));

    cx.update(|cx| body.push_str("$", cx));
    cx.update(|cx| body.push_str("x", cx));
    cx.update(|cx| body.push_str("$", cx));
    cx.run_until_parked();

    let selected = cx.update(|cx| body.select_all_text(cx));
    assert!(selected.contains("fn main() {}"));
    assert!(selected.contains("$x$"));
}

#[gpui::test]
fn replacement_then_append_uses_the_replacement_as_its_worker_baseline(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let mut body = cx.update(|cx| MarkdownBody::new("old", 1, cx));

    cx.update(|cx| body.set_text("new $x$", cx));
    cx.update(|cx| body.push_str(" tail", cx));
    cx.run_until_parked();

    assert_eq!(
        cx.update(|cx| body.select_all_text(cx)).trim(),
        "new $x$ tail"
    );
}

#[test]
fn ignores_highlights_that_end_before_later_lines() {
    let lines = highlighted_lines("first\nsecond", &[(0..5, HighlightStyle::default())]);

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].styles.len(), 1);
    assert_eq!(lines[0].styles[0].0, 0..5);
    assert!(lines[1].styles.is_empty());
}

#[test]
fn clips_highlights_that_span_multiple_lines() {
    let lines = highlighted_lines("abc\ndef", &[(1..6, HighlightStyle::default())]);

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].styles[0].0, 1..3);
    assert_eq!(lines[1].styles[0].0, 0..2);
}

#[test]
fn projects_ordered_highlights_across_empty_and_unstyled_lines() {
    let first = HighlightStyle::default();
    let last = HighlightStyle::default();
    let lines = highlighted_lines("ab\n\nplain\nxyz", &[(0..2, first), (10..13, last)]);

    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0].styles, vec![(0..2, first)]);
    assert!(lines[1].styles.is_empty());
    assert!(lines[2].styles.is_empty());
    assert_eq!(lines[3].styles, vec![(0..3, last)]);
}

/// P0 spike: prove the syntax-highlighting chain can run on a background
/// thread. `SyntaxHighlighter` owns a tree-sitter `Parser` (not `Send`), so
/// we must construct it inside the worker thread rather than move it across.
/// This test asserts that (a) a fresh highlighter can be built and driven
/// purely from the thread-safe `LanguageRegistry::singleton()`, and (b) the
/// produced `Vec<(Range, HighlightStyle)>` is `Send` and can cross back.
#[test]
fn syntax_highlighter_runs_off_thread_and_returns_send_styles() {
    let code = Rope::from("fn main() -> usize { 42 }");
    let styles = std::thread::spawn(move || {
        let mut highlighter = SyntaxHighlighter::new("rust");
        highlighter.update(None, &code, None);
        highlighter.styles(&(0..code.len()), HighlightTheme::default_dark().as_ref())
    })
    .join()
    .expect("worker thread");

    assert!(
        !styles.is_empty(),
        "rust fenced code must yield syntax styles off the main thread"
    );
}

/// P0 spike: the current theme's `HighlightTheme` is passed from the main
/// thread into the background worker (`Arc<HighlightTheme>`), so the whole
/// `Arc<HighlightTheme>` must be `Send` (i.e. `HighlightTheme: Send + Sync`).
#[test]
fn highlighted_theme_arc_is_send_across_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<Arc<HighlightTheme>>();
}

/// A short code block (≤ `BG_HIGHLIGHT_BYTES`) is highlighted synchronously:
/// the background worker is never spawned, so the first paint is complete
/// and there is no placeholder flash.
#[gpui::test]
fn short_code_block_highlights_synchronously_without_background(cx: &mut TestAppContext) {
    init_markdown_test(cx);
    reset_background_probe();
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new("```rust\nlet x = 1;\n```", 7, cx),
        });
        Root::new(content, window, cx)
    });
    let cx: &mut VisualTestContext = cx;
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert_eq!(
        background_probe().background_spawns,
        0,
        "short code block must highlight synchronously, not on a background thread"
    );
}

/// The byte threshold is inclusive for synchronous highlighting: a block of
/// exactly `BG_HIGHLIGHT_BYTES` renders synchronously, while one byte over
/// defers to the background worker. This pins the boundary so a future edit
/// cannot silently move it.
#[gpui::test]
fn background_threshold_boundary_is_inclusive_for_sync(cx: &mut TestAppContext) {
    init_markdown_test(cx);
    let at_threshold = FencedCode {
        code: "x".repeat(BG_HIGHLIGHT_BYTES).into(),
        language: Some("text".into()),
        start: 0,
    };
    let over_threshold = FencedCode {
        code: "x".repeat(BG_HIGHLIGHT_BYTES + 1).into(),
        language: Some("text".into()),
        start: 0,
    };
    let owner = MarkdownContributionOwner::new(FENCED_CODE_EXTENSION_ID, 1);
    let at_cache = cx.update(|cx| HighlightCache::new(&at_threshold, owner, cx));
    let over_cache = cx.update(|cx| HighlightCache::new(&over_threshold, owner, cx));
    assert!(
        at_cache.styles.is_some(),
        "a block at exactly the byte threshold must highlight synchronously"
    );
    assert!(
        over_cache.styles.is_none(),
        "a block one byte over the threshold must defer to the background worker"
    );
}

/// A long code block (> `BG_HIGHLIGHT_BYTES`) defers highlighting to a
/// background worker exactly once; once the worker installs styles, a
/// subsequent frame must not spawn any new worker. The uppercase language
/// exercises the same normalization used by the background path.
#[gpui::test]
fn long_code_block_defers_highlight_to_background_once(cx: &mut TestAppContext) {
    init_markdown_test(cx);
    reset_background_probe();
    // ~38 bytes/line × 1000 lines ≈ 38 KiB > BG_HIGHLIGHT_BYTES.
    let source = format!(
        "```PYTHON\n{}\n```",
        "name = 'world'\nprint(f'hello {name}')\n".repeat(1000)
    );
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new(&source, 7, cx),
        });
        Root::new(content, window, cx)
    });
    let cx: &mut VisualTestContext = cx;
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert_eq!(
        background_probe().background_spawns,
        1,
        "long code block must defer to a background highlight worker"
    );

    // Drive the worker to completion, then re-render: styles are now
    // present, so no second worker is spawned.
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert_eq!(
        background_probe().background_spawns,
        1,
        "once background styles are installed, no re-spawn may occur"
    );
    let probe = background_probe();
    assert_eq!(probe.background_installs, 1);
    assert_eq!(probe.last_generation, Some(0));
    assert!(
        probe.last_style_count > 1,
        "a completed uppercase-Python worker must install syntax styles"
    );
}

/// Replacing a long block while its worker is in flight drops (cancels)
/// the stale task, so the render path spawns a fresh worker for the new
/// generation instead of waiting on the old one. A result from the old
/// generation is discarded; only the current generation may install.
#[gpui::test]
fn cache_replacement_drops_stale_task_and_discards_stale_result(cx: &mut TestAppContext) {
    init_markdown_test(cx);
    reset_background_probe();
    let old_source = FencedCode {
        code: "fn main() { let value = 0; }".repeat(800).into(),
        language: Some("Rust".into()),
        start: 0,
    };
    let new_source = FencedCode {
        code: "fn main() { let value = 1; }".repeat(800).into(),
        language: Some("Rust".into()),
        start: 0,
    };
    let owner = MarkdownContributionOwner::new(FENCED_CODE_EXTENSION_ID, 1);
    let mut cache = cx.update(|cx| HighlightCache::new(&old_source, owner, cx));
    cache.own_highlight_task(Task::ready(()));
    cache.highlight_task_generation = Some(cache.generation);
    let old_generation = cache.generation;
    cx.update(|cx| cache.replace(&new_source, cx));

    assert_eq!(cache.generation, old_generation + 1);
    // The stale task and its generation guard are dropped on rebuild, so
    // the next render will spawn a fresh worker for the new generation.
    assert!(cache.highlight_task.is_none());
    assert!(cache.highlight_task_generation.is_none());
    assert!(cache.styles.is_none());

    // A late result from the old generation is discarded by the guard.
    let stale_styles: CodeStyles = vec![(0..3, HighlightStyle::default())].into();
    assert!(!cache.try_apply_styles(old_generation, stale_styles));
    assert!(cache.styles.is_none());

    // A result for the current generation is accepted.
    let theme = cache.theme.clone();
    let fresh_styles = compute_code_styles(
        new_source.code.as_ref(),
        &normalized_language_id(new_source.language.as_deref()),
        theme.as_ref(),
    );
    assert!(cache.try_apply_styles(cache.generation, fresh_styles));
    assert!(cache.styles.is_some());
}

/// Streaming growth end to end through the real render path: once a long
/// block's content changes, the replaced cache no longer matches, so the
/// render path starts a fresh background worker for the new generation and
/// installs that generation's styles when it completes, without further
/// re-spawning. The drop-on-rebuild semantics of `replace` are asserted
/// deterministically by `cache_replacement_drops_stale_task_and_discards_stale_result`;
/// this test confirms the integrated render path restarts the worker and
/// lets the replacement generation's styles win.
#[gpui::test]
fn replacing_a_long_block_restarts_the_background_worker_for_the_new_generation(
    cx: &mut TestAppContext,
) {
    init_markdown_test(cx);
    reset_background_probe();
    const OWNER_ID: u64 = 7;
    let old_source = format!(
        "```python\n{}\n```",
        "name = 'old'\nprint(f'hello {name}')\n".repeat(600)
    );
    let new_source = format!(
        "```python\n{}\n```",
        "name = 'new'\nprint(f'hello {name}')\n".repeat(600)
    );
    let content = cx.update(|cx| {
        cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new(&old_source, OWNER_ID, cx),
        })
    });
    let (_, cx) = cx.add_window_view(|window, cx| Root::new(content.clone(), window, cx));
    let cx: &mut VisualTestContext = cx;

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert_eq!(
        background_probe().background_spawns,
        1,
        "first long block defers to a background worker"
    );

    // Change the block content (streaming growth): the cache no longer
    // matches, so the stale worker must be dropped and a fresh worker
    // started for the new generation. Before this fix the stale task was
    // retained, so no new worker was spawned here and the block stayed as
    // a placeholder until a later frame.
    cx.update(|_, cx| {
        content.update(cx, |root, cx| root.body.set_text(&new_source, cx));
    });
    // Large full replacements are parsed off the UI thread. Wait for the
    // replacement AST before drawing the new code block so this assertion
    // observes the worker generation transition rather than the old frame
    // that remains visible while parsing is in flight.
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert_eq!(
        background_probe().background_spawns,
        2,
        "a content change must drop the stale worker and spawn a fresh one"
    );

    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let probe = background_probe();
    assert_eq!(
        probe.background_spawns, 2,
        "once the replacement's styles are installed, no further worker may spawn"
    );
    assert_eq!(
        probe.last_generation,
        Some(1),
        "the replacement generation installs its styles"
    );
    assert!(
        probe.last_style_count > 1,
        "the replacement worker must install real syntax styles"
    );
}

/// Italic comments and bold keywords are present in the configured themes.
/// Installing those styles must not change the wrapped code block's bounds
/// relative to its plain-text placeholder.
#[gpui::test]
fn long_wrapped_placeholder_keeps_code_bounds(cx: &mut TestAppContext) {
    init_markdown_test(cx);
    reset_background_probe();
    cx.update(|cx| {
        set_global_wrap_in_memory(true, cx);
        preferences::update_in_memory(cx, |prefs| prefs.code_block_line_numbers = true);
    });
    let source = format!(
        "```Rust\n{}\n```",
        ("// this is a deliberately long comment that exercises wrapped italic text\n").repeat(600)
    );
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|cx| CodeSelectionTestRoot {
            body: MarkdownBody::new(&source, 7, cx),
        });
        Root::new(content, window, cx)
    });
    let cx: &mut VisualTestContext = cx;
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let placeholder = cx
        .debug_bounds("markdown-code-line-7-0-0")
        .expect("wrapped code placeholder bounds");

    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let highlighted = cx
        .debug_bounds("markdown-code-line-7-0-0")
        .expect("wrapped highlighted code bounds");
    assert_eq!(
        placeholder.size, highlighted.size,
        "background styles must not change wrapped code layout"
    );
}
