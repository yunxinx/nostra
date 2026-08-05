//! Shared inline delete confirmation popover.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{
    Anchor, App, ElementId, FocusHandle, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, Styled as _, WeakEntity, Window, div,
};
use gpui_component::{
    ElementExt as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    popover::{Popover, PopoverState},
    v_flex,
};

type OpenChangeHandler = Rc<dyn Fn(&bool, &mut Window, &mut App)>;
type ConfirmHandler = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Default)]
struct ConfirmationPopoverBinding {
    state: Option<WeakEntity<PopoverState>>,
    return_focus: Option<FocusHandle>,
    prepared_id: Option<ElementId>,
}

/// Handle used by an owning view to close a confirmation before unmounting it.
#[derive(Clone, Default)]
pub(crate) struct InlineDeleteConfirmationHandle(Rc<RefCell<ConfirmationPopoverBinding>>);

impl InlineDeleteConfirmationHandle {
    fn is_prepared(&self, id: &ElementId) -> bool {
        self.0.borrow().prepared_id.as_ref() == Some(id)
    }

    fn mark_prepared(&self, id: ElementId) -> bool {
        let mut binding = self.0.borrow_mut();
        if binding.prepared_id.as_ref() == Some(&id) {
            return false;
        }
        binding.prepared_id = Some(id);
        true
    }

    fn bind(&self, state: WeakEntity<PopoverState>, return_focus: Option<FocusHandle>) {
        let mut binding = self.0.borrow_mut();
        binding.state = Some(state);
        if binding.return_focus.is_none() {
            binding.return_focus = return_focus;
        }
    }

    fn restore_focus(
        &self,
        state: Option<&WeakEntity<PopoverState>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let return_focus = {
            let mut binding = self.0.borrow_mut();
            if binding.state.as_ref() != state {
                return;
            }
            binding.state = None;
            binding.return_focus.take()
        };
        self.0.borrow_mut().prepared_id = None;
        let Some(return_focus) = return_focus else {
            return;
        };
        window.defer(cx, move |window, cx| return_focus.focus(window, cx));
    }

    fn abandon_return_focus(&self) {
        self.0.borrow_mut().return_focus = None;
    }

    /// Close a mounted confirmation before its owner removes it for another reason.
    pub(crate) fn dismiss_for_unmount(&self, window: &mut Window, cx: &mut App) {
        let state = {
            let mut binding = self.0.borrow_mut();
            binding.return_focus = None;
            binding.prepared_id = None;
            binding.state.take().and_then(|state| state.upgrade())
        };
        let Some(state) = state else {
            return;
        };
        window.defer(cx, move |window, cx| {
            state.update(cx, |state, cx| state.dismiss(window, cx));
        });
    }
}

/// A controlled delete-confirmation Popover with a complete dismissal lifecycle.
#[derive(IntoElement)]
pub(crate) struct InlineDeleteConfirmation {
    id: ElementId,
    trigger: Button,
    title: SharedString,
    cancel_label: SharedString,
    confirm_label: SharedString,
    handle: InlineDeleteConfirmationHandle,
    on_open_change: OpenChangeHandler,
    on_confirm: ConfirmHandler,
}

impl InlineDeleteConfirmation {
    /// Create an inline delete confirmation with a stable id and trigger.
    pub(crate) fn new(
        id: impl Into<ElementId>,
        trigger: Button,
        title: impl Into<SharedString>,
        cancel_label: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        handle: InlineDeleteConfirmationHandle,
    ) -> Self {
        Self {
            id: id.into(),
            trigger,
            title: title.into(),
            cancel_label: cancel_label.into(),
            confirm_label: confirm_label.into(),
            handle,
            on_open_change: Rc::new(|_, _, _| {}),
            on_confirm: Rc::new(|_, _| {}),
        }
    }

    /// Set the handler called when the confirmation opens or closes.
    pub(crate) fn on_open_change(
        mut self,
        handler: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Rc::new(handler);
        self
    }

    /// Set the handler called after the Popover has closed for confirmation.
    pub(crate) fn on_confirm(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_confirm = Rc::new(handler);
        self
    }

    fn child_id(&self, name: &'static str) -> ElementId {
        ElementId::NamedChild(Arc::new(self.id.clone()), SharedString::new_static(name))
    }
}

impl RenderOnce for InlineDeleteConfirmation {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let cancel_id = self.child_id("cancel");
        let confirm_id = self.child_id("confirm");
        let cancel_selector = format!("{}-cancel", self.id);
        let confirm_selector = format!("{}-confirm", self.id);
        let handle = self.handle.clone();
        let on_open_change = self.on_open_change.clone();
        let on_confirm = self.on_confirm.clone();
        let instance_state = Rc::new(RefCell::new(None::<WeakEntity<PopoverState>>));
        let title = self.title.clone();
        let cancel_label = self.cancel_label.clone();
        let confirm_label = self.confirm_label.clone();
        let prepared = self.handle.is_prepared(&self.id);
        let parent_view_id = window.current_view();
        let trigger = self.trigger.on_prepaint({
            let handle = self.handle.clone();
            let id = self.id.clone();
            move |_, _, cx| {
                if handle.mark_prepared(id) {
                    cx.notify(parent_view_id);
                }
            }
        });

        Popover::new(self.id)
            // A closed prepaint captures trigger bounds without registering a
            // deferred Popover. The next render can open and bind its state in
            // one synchronous pass, leaving no registered-but-unbound frame.
            .open(prepared)
            .anchor(Anchor::TopRight)
            .p_0()
            .on_open_change({
                let instance_state = instance_state.clone();
                move |open, window, cx| {
                    if !*open {
                        handle.restore_focus(instance_state.borrow().as_ref(), window, cx);
                    }
                    on_open_change(open, window, cx);
                }
            })
            .trigger(trigger)
            .content({
                let handle = self.handle;
                let instance_state = instance_state.clone();
                move |state, window, cx| {
                    let weak_state = cx.weak_entity();
                    let content_focus = state.focus_handle(cx);
                    let focused_in_content = content_focus.contains_focused(window, cx);
                    handle.bind(
                        weak_state.clone(),
                        (!focused_in_content).then(|| window.focused(cx)).flatten(),
                    );
                    *instance_state.borrow_mut() = Some(weak_state);
                    if !focused_in_content {
                        window.defer(cx, move |window, cx| content_focus.focus(window, cx));
                    }

                    v_flex()
                        .gap_1()
                        .p_2()
                        .child(div().w_full().text_center().text_sm().child(title.clone()))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new(cancel_id.clone())
                                        .debug_selector({
                                            let cancel_selector = cancel_selector.clone();
                                            move || cancel_selector
                                        })
                                        .ghost()
                                        .small()
                                        .flex_1()
                                        .label(cancel_label.clone())
                                        .on_click(cx.listener(|state, _, window, cx| {
                                            state.dismiss(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new(confirm_id.clone())
                                        .debug_selector({
                                            let confirm_selector = confirm_selector.clone();
                                            move || confirm_selector
                                        })
                                        .danger()
                                        .small()
                                        .flex_1()
                                        .label(confirm_label.clone())
                                        .on_click(cx.listener({
                                            let handle = handle.clone();
                                            let on_confirm = on_confirm.clone();
                                            move |state, _, window, cx| {
                                                handle.abandon_return_focus();
                                                state.dismiss(window, cx);
                                                on_confirm(window, cx);
                                            }
                                        })),
                                ),
                        )
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{AppContext as _, Context, Entity, Render, TestAppContext};
    use gpui_component::{
        ElementExt as _, Root,
        input::{Input, InputState},
    };

    use super::*;

    struct ConfirmationTestView {
        open: bool,
        confirmed: usize,
        handle: InlineDeleteConfirmationHandle,
    }

    struct FirstFrameUnmountTestView {
        open: bool,
        handle: InlineDeleteConfirmationHandle,
        input: Entity<InputState>,
        context_menu_builds: Rc<Cell<usize>>,
        close_on_trigger_prepaint: bool,
    }

    impl FirstFrameUnmountTestView {
        fn close_for_unmount(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            self.handle.dismiss_for_unmount(window, cx);
            self.open = false;
            cx.notify();
        }
    }

    impl ConfirmationTestView {
        fn close_for_unmount(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            self.handle.dismiss_for_unmount(window, cx);
            self.open = false;
            cx.notify();
        }
    }

    impl Render for ConfirmationTestView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.open {
                return Button::new("test-confirmation-trigger")
                    .debug_selector(|| "test-confirmation-trigger".into())
                    .label("Delete")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open = true;
                        cx.notify();
                    }))
                    .into_any_element();
            }

            let close = cx.weak_entity();
            let confirm = cx.weak_entity();
            InlineDeleteConfirmation::new(
                "test-confirmation",
                Button::new("test-confirmation-trigger").label("Delete"),
                "Delete this item?",
                "Cancel",
                "Delete",
                self.handle.clone(),
            )
            .on_open_change(move |open, _, cx| {
                close
                    .update(cx, |this, cx| {
                        this.open = *open;
                        cx.notify();
                    })
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm.update(cx, |this, _| this.confirmed += 1).ok();
            })
            .into_any_element()
        }
    }

    impl Render for FirstFrameUnmountTestView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let confirmation = if self.open {
                let close = cx.weak_entity();
                let close_on_prepaint = cx.weak_entity();
                InlineDeleteConfirmation::new(
                    "first-frame-confirmation",
                    Button::new("first-frame-trigger")
                        .label("Delete")
                        .on_prepaint(move |_, window, cx| {
                            close_on_prepaint
                                .update(cx, |this, cx| {
                                    if this.close_on_trigger_prepaint {
                                        this.close_on_trigger_prepaint = false;
                                        this.close_for_unmount(window, cx);
                                    }
                                })
                                .ok();
                        }),
                    "Delete this item?",
                    "Cancel",
                    "Delete",
                    self.handle.clone(),
                )
                .on_open_change(move |open, _, cx| {
                    close
                        .update(cx, |this, cx| {
                            this.open = *open;
                            cx.notify();
                        })
                        .ok();
                })
                .into_any_element()
            } else {
                Button::new("first-frame-trigger")
                    .debug_selector(|| "first-frame-trigger".into())
                    .label("Delete")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open = true;
                        cx.notify();
                    }))
                    .into_any_element()
            };
            let context_menu_builds = self.context_menu_builds.clone();

            v_flex().child(confirmation).child(
                div().debug_selector(|| "context-menu-input".into()).child(
                    Input::new(&self.input).context_menu(move |menu, _, _| {
                        context_menu_builds.set(context_menu_builds.get() + 1);
                        menu
                    }),
                ),
            )
        }
    }

    fn setup(
        cx: &mut TestAppContext,
    ) -> (Entity<ConfirmationTestView>, &mut gpui::VisualTestContext) {
        cx.update(gpui_component::init);
        let view = cx.update(|cx| {
            cx.new(|_| ConfirmationTestView {
                open: false,
                confirmed: 0,
                handle: InlineDeleteConfirmationHandle::default(),
            })
        });
        let (_, cx) = cx.add_window_view({
            let view = view.clone();
            move |window, cx| Root::new(view, window, cx)
        });
        (view, cx)
    }

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

    #[gpui::test]
    fn forced_open_focuses_content_and_every_close_path_can_reopen(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        redraw(cx);
        let trigger_focus = cx.update(|window, cx| {
            window.focus_next(cx);
            window.focused(cx).expect("trigger should receive focus")
        });

        click(cx, "test-confirmation-trigger");
        assert!(cx.update(|_, cx| view.read(cx).open));
        assert!(cx.update(|window, cx| window.focused(cx).as_ref() != Some(&trigger_focus)));

        cx.simulate_keystrokes("escape");
        redraw(cx);
        assert!(!cx.update(|_, cx| view.read(cx).open));
        cx.update(|window, cx| {
            assert_eq!(window.focused(cx).as_ref(), Some(&trigger_focus));
        });

        click(cx, "test-confirmation-trigger");
        click(cx, "test-confirmation-cancel");
        assert!(!cx.update(|_, cx| view.read(cx).open));
        cx.update(|window, cx| {
            assert_eq!(window.focused(cx).as_ref(), Some(&trigger_focus));
        });

        click(cx, "test-confirmation-trigger");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.close_for_unmount(window, cx));
        });
        redraw(cx);
        assert!(!cx.update(|_, cx| view.read(cx).open));

        click(cx, "test-confirmation-trigger");
        click(cx, "test-confirmation-confirm");
        assert!(!cx.update(|_, cx| view.read(cx).open));
        assert_eq!(cx.update(|_, cx| view.read(cx).confirmed), 1);
    }

    #[gpui::test]
    fn first_frame_unmount_does_not_block_input_context_menus(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let context_menu_builds = Rc::new(Cell::new(0));
        let (root, cx) = cx.add_window_view({
            let context_menu_builds = context_menu_builds.clone();
            move |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx));
                let view = cx.new(|_| FirstFrameUnmountTestView {
                    open: false,
                    handle: InlineDeleteConfirmationHandle::default(),
                    input,
                    context_menu_builds,
                    close_on_trigger_prepaint: false,
                });
                Root::new(view, window, cx)
            }
        });
        let view = root.read_with(cx, |root, _| {
            root.view()
                .clone()
                .downcast::<FirstFrameUnmountTestView>()
                .expect("Root must contain FirstFrameUnmountTestView")
        });
        redraw(cx);

        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.open = true;
                view.close_on_trigger_prepaint = true;
                cx.notify();
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        redraw(cx);
        assert!(!cx.update(|_, cx| view.read(cx).open));

        let input = cx
            .debug_bounds("context-menu-input")
            .expect("input should be drawn");
        cx.simulate_mouse_down(input.center(), gpui::MouseButton::Right, Default::default());
        cx.simulate_mouse_up(input.center(), gpui::MouseButton::Right, Default::default());
        assert_eq!(context_menu_builds.get(), 1);
    }
}
