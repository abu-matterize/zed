use super::*;
use crate::display_map::RowGeometryLease;
use futures::channel::oneshot;
use gpui::WindowId;

#[derive(Clone, Default)]
pub(super) struct CapturedInput {
    pub(super) completion: Option<completions::CapturedCompletionAction>,
    pub(super) edit_prediction: Option<edit_prediction::CapturedEditPredictionAction>,
    pub(super) clipboard: Option<clipboard::CapturedClipboardAction>,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(super) primary: Option<clipboard::CapturedClipboardAction>,
}

impl CapturedInput {
    pub(super) fn current(cx: &App) -> Option<&Self> {
        cx.try_global::<InputScope>()?.0.as_ref()
    }

    pub(super) fn completion(completion: completions::CapturedCompletionAction) -> Self {
        Self {
            completion: Some(completion),
            ..Self::default()
        }
    }

    pub(super) fn edit_prediction(
        edit_prediction: edit_prediction::CapturedEditPredictionAction,
    ) -> Self {
        Self {
            edit_prediction: Some(edit_prediction),
            ..Self::default()
        }
    }

    pub(super) fn run<C: std::borrow::BorrowMut<App>, R>(
        mut self,
        cx: &mut C,
        handler: impl FnOnce(&mut C) -> R,
    ) -> R {
        if let Some(current) = Self::current(cx.borrow_mut()) {
            self.completion = self.completion.or_else(|| current.completion.clone());
            self.edit_prediction = self
                .edit_prediction
                .or_else(|| current.edit_prediction.clone());
            self.clipboard = self.clipboard.or_else(|| current.clipboard.clone());
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            {
                self.primary = self.primary.or_else(|| current.primary.clone());
            }
        }
        Self::run_optional(Some(self), cx, handler)
    }

    fn run_optional<C: std::borrow::BorrowMut<App>, R>(
        input: Option<Self>,
        cx: &mut C,
        handler: impl FnOnce(&mut C) -> R,
    ) -> R {
        let previous = mem::replace(&mut cx.borrow_mut().default_global::<InputScope>().0, input);
        let result = handler(cx);
        cx.borrow_mut().default_global::<InputScope>().0 = previous;
        result
    }
}

#[derive(Default)]
struct InputScope(Option<CapturedInput>);

impl Global for InputScope {}

pub(super) type InputContinuation = Box<dyn FnOnce(&mut Editor, &mut Window, &mut Context<Editor>)>;

#[derive(Default)]
pub(super) struct DeferredInput {
    pub(super) transactions: HashMap<TransactionId, TransactionId>,
    pub(super) clipboard: clipboard::DeferredClipboard,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(super) primary: clipboard::DeferredClipboard,
    queue: VecDeque<DeferredInputOperation>,
    task: Option<Task<()>>,
    window: Option<(WindowId, Subscription)>,
    executing: bool,
    insertion_index: usize,
    completions: Vec<oneshot::Sender<Result<()>>>,
    geometry: Vec<(EntityId, RowGeometryLease)>,
}

struct DeferredInputOperation {
    geometry: Vec<(EntityId, RowGeometryLease)>,
    pending: Option<PendingRowGeometry>,
    input: Option<CapturedInput>,
    continuation: DeferredInputContinuation,
}

enum DeferredInputContinuation {
    Editor(InputContinuation),
    External(Box<dyn FnOnce(&mut Window, &mut App)>),
}

impl Editor {
    pub fn defer_keystroke_input(
        &mut self,
        continuation: impl FnOnce(&mut Window, &mut App) -> Box<dyn FnOnce(&mut Window, &mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.input_is_deferred() {
            return false;
        }
        self.capture_input(None, cx).run(cx, |cx| {
            let continuation = continuation(window, cx);
            self.defer_external_input(continuation, cx)
        })
    }

    pub fn defer_geometry(
        &mut self,
        pending: PendingRowGeometry,
        continuation: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.defer_geometry_continuation(
            pending,
            DeferredInputContinuation::Editor(Box::new(continuation)),
            window,
            cx,
        );
    }

    pub fn defer_external_geometry(
        &mut self,
        pending: PendingRowGeometry,
        continuation: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.defer_geometry_continuation(
            pending,
            DeferredInputContinuation::External(Box::new(continuation)),
            window,
            cx,
        );
    }

    pub fn defer_external_input(
        &mut self,
        continuation: impl FnOnce(&mut Window, &mut App) + 'static,
        cx: &App,
    ) -> bool {
        if !self.input_is_deferred() {
            return false;
        }
        self.enqueue_deferred_input(DeferredInputOperation {
            geometry: Vec::new(),
            pending: None,
            input: CapturedInput::current(cx).cloned(),
            continuation: DeferredInputContinuation::External(Box::new(continuation)),
        });
        true
    }

    pub fn defer_external_input_after_pending(
        &mut self,
        continuation: impl FnOnce(&mut Window, &mut App) + 'static,
    ) -> bool {
        if self.deferred_input.task.is_none() {
            return false;
        }
        self.deferred_input.queue.push_back(DeferredInputOperation {
            geometry: Vec::new(),
            pending: None,
            input: None,
            continuation: DeferredInputContinuation::External(Box::new(continuation)),
        });
        true
    }

    pub fn request_row_geometry(&self, pending: PendingRowGeometry, cx: &mut App) -> bool {
        let Some(source) = self.display_map_for_geometry(&pending, cx) else {
            return false;
        };
        source.update(cx, |map, cx| map.request_row_geometry(pending, cx));
        true
    }

    pub fn defer_input(
        &mut self,
        continuation: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) + 'static,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.input_is_deferred() {
            return false;
        }
        self.enqueue_deferred_input(DeferredInputOperation {
            geometry: Vec::new(),
            pending: None,
            input: CapturedInput::current(cx).cloned(),
            continuation: DeferredInputContinuation::Editor(Box::new(continuation)),
        });
        true
    }

    pub fn input_completion(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if self.deferred_input.task.is_none() {
            return Task::ready(Ok(()));
        }
        let (sender, receiver) = oneshot::channel();
        self.deferred_input.completions.push(sender);
        cx.background_spawn(async move {
            receiver
                .await
                .context("editor closed before deferred input completed")?
        })
    }

    pub(super) fn defer_native_input(
        &mut self,
        continuation: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) + 'static,
    ) -> bool {
        if !self.input_is_deferred() {
            return false;
        }
        self.enqueue_deferred_input_preserving_composition(DeferredInputOperation {
            geometry: Vec::new(),
            pending: None,
            input: None,
            continuation: DeferredInputContinuation::Editor(Box::new(continuation)),
        });
        true
    }

    pub(super) fn defer_input_task<T: Send + 'static>(
        &mut self,
        continuation: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) -> Task<Result<T>>
        + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<T>> {
        if !self.input_is_deferred() {
            return continuation(self, window, cx);
        }
        let (sender, receiver) = oneshot::channel();
        self.defer_input(
            move |editor, window, cx| {
                if !sender.is_canceled() {
                    sender.send(continuation(editor, window, cx)).ok();
                }
            },
            window,
            cx,
        );
        cx.background_spawn(async move {
            receiver
                .await
                .context("editor closed before deferred input started")?
                .await
        })
    }

    pub(super) fn input_is_deferred(&self) -> bool {
        self.deferred_input.task.is_some()
            && (!self.deferred_input.executing || self.deferred_input.insertion_index > 0)
    }

    pub(super) fn input_is_replaying(&self) -> bool {
        self.deferred_input.executing
    }

    pub(super) fn ready_input_plan<T>(
        &mut self,
        plan: Result<T, PendingRowGeometry>,
        retry: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<T> {
        match plan {
            Ok(plan) => Some(plan),
            Err(pending) => {
                self.defer_geometry(pending, retry, window, cx);
                None
            }
        }
    }

    pub(super) fn defer_action<A: Clone + 'static>(
        &mut self,
        action: &A,
        apply: fn(&mut Editor, &A, &mut Window, &mut Context<Editor>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.input_is_deferred() {
            return false;
        }
        let input = self.capture_input(Some(action), cx);
        let action = action.clone();
        input.run(cx, |cx| {
            self.defer_input(
                move |editor, window, cx| apply(editor, &action, window, cx),
                window,
                cx,
            )
        })
    }

    pub(super) fn defer_action_dispatch<A: Action>(
        &mut self,
        action: &A,
        window: &mut Window,
        cx: &mut Context<Self>,
        handler: impl FnOnce(&A, &mut Window, &mut App) + 'static,
    ) -> bool {
        if !self.input_is_deferred() {
            return false;
        }
        let Some(continuation) =
            window.defer_action_dispatch(action, cx, move |action, window, cx| {
                if let Some(action) = action.as_any().downcast_ref::<A>() {
                    handler(action, window, cx);
                } else {
                    cx.propagate();
                }
            })
        else {
            return false;
        };
        self.capture_input(Some(action), cx)
            .run(cx, |cx| self.defer_external_input(continuation, cx))
    }

    fn capture_input(&self, action: Option<&dyn Any>, cx: &Context<Self>) -> CapturedInput {
        CapturedInput {
            completion: Some(
                action
                    .and_then(|action| self.capture_completion_action(action, cx))
                    .unwrap_or_else(|| self.capture_completion_target(None, cx)),
            ),
            edit_prediction: Some(self.capture_edit_prediction_target(cx)),
            clipboard: Some(self.capture_clipboard(cx)),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            primary: Some(self.capture_primary(cx)),
        }
    }

    fn defer_geometry_continuation(
        &mut self,
        pending: PendingRowGeometry,
        continuation: DeferredInputContinuation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.weak_entity();
        let continuation = move |window: &mut Window, cx: &mut App| match continuation {
            DeferredInputContinuation::Editor(continuation) => {
                editor
                    .update(cx, |editor, cx| continuation(editor, window, cx))
                    .ok();
            }
            DeferredInputContinuation::External(continuation) => continuation(window, cx),
        };
        let mut input = CapturedInput::current(cx).cloned();
        let continuation = match window.defer_current_action_dispatch(cx, continuation) {
            Ok(continuation) => {
                input = Some(self.capture_input(None, cx));
                continuation
            }
            Err(continuation) => Box::new(continuation),
        };
        let mut geometry = self.deferred_input.geometry.clone();
        let mut requested_source = None;
        for source in self.geometry_sources(cx) {
            if !geometry.iter().any(|(id, _)| *id == source.entity_id()) {
                geometry.push((source.entity_id(), source.read(cx).retain_row_geometry()));
            }
            if source.entity_id() == pending.display_map_id() {
                requested_source = Some(source);
            }
        }
        let requested = requested_source.is_some();
        if let Some(source) = requested_source {
            source.update(cx, |map, cx| map.request_row_geometry(pending.clone(), cx));
        }
        self.enqueue_deferred_input(DeferredInputOperation {
            geometry,
            pending: requested.then_some(pending),
            input,
            continuation: DeferredInputContinuation::External(continuation),
        });
        if self.deferred_input.task.is_some() {
            return;
        }
        let window_id = window.window_handle().window_id();
        let editor = cx.weak_entity();
        let closed = cx.on_window_closed(move |cx, closed_id| {
            if closed_id == window_id {
                editor
                    .update(cx, |editor, _| {
                        if editor
                            .deferred_input
                            .window
                            .as_ref()
                            .is_some_and(|(id, _)| *id == window_id)
                        {
                            editor.finish_deferred_input(Some(
                                "editor window closed before deferred input completed",
                            ));
                        }
                    })
                    .ok();
            }
        });
        self.deferred_input.window = Some((window_id, closed));
        self.deferred_input.task = Some(cx.spawn_in(window, async move |editor, cx| {
            loop {
                let operation = editor.update(cx, |editor, _| {
                    if editor
                        .deferred_input
                        .window
                        .as_ref()
                        .is_none_or(|(id, _)| *id != window_id)
                    {
                        return None;
                    }
                    let operation = editor.deferred_input.queue.pop_front();
                    if operation.is_none() {
                        editor.finish_deferred_input(None);
                    }
                    operation
                });
                let Ok(Some(DeferredInputOperation {
                    geometry,
                    pending,
                    input,
                    continuation,
                })) = operation
                else {
                    return;
                };
                if let Some(pending) = &pending {
                    pending.wait().await;
                }
                let result = cx
                    .update(|window, cx| -> Result<()> {
                        editor.update(cx, |editor, _| {
                            editor.deferred_input.executing = true;
                            editor.deferred_input.insertion_index = 0;
                            editor.deferred_input.geometry = geometry;
                        })?;
                        CapturedInput::run_optional(input, cx, |cx| {
                            match continuation {
                                DeferredInputContinuation::Editor(continuation) => {
                                    editor.update(cx, |editor, cx| {
                                        continuation(editor, window, cx)
                                    })?;
                                }
                                DeferredInputContinuation::External(continuation) => {
                                    continuation(window, cx)
                                }
                            }
                            Ok(())
                        })
                    })
                    .and_then(|result| result)
                    .and_then(|()| {
                        editor.update(cx, |editor, _| {
                            if editor
                                .deferred_input
                                .window
                                .as_ref()
                                .is_some_and(|(id, _)| *id == window_id)
                            {
                                editor.deferred_input.executing = false;
                                editor.deferred_input.geometry.clear();
                            }
                        })
                    });
                drop(pending);
                if let Err(error) = result {
                    editor
                        .update(cx, |editor, _| {
                            if editor
                                .deferred_input
                                .window
                                .as_ref()
                                .is_some_and(|(id, _)| *id == window_id)
                            {
                                editor.finish_deferred_input(Some(&format!(
                                    "deferred input lost its window: {error}"
                                )));
                            }
                        })
                        .ok();
                    return;
                }
            }
        }));
    }

    fn finish_deferred_input(&mut self, error: Option<&str>) {
        self.queued_ime_composition = None;
        self.deferred_input.queue.clear();
        self.deferred_input.geometry.clear();
        self.deferred_input.clipboard = clipboard::DeferredClipboard::default();
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            self.deferred_input.primary = clipboard::DeferredClipboard::default();
        }
        self.deferred_input.executing = false;
        self.deferred_input.insertion_index = 0;
        self.deferred_input.transactions.clear();
        self.deferred_input.window.take();
        self.deferred_input.task.take();
        for completion in self.deferred_input.completions.drain(..) {
            completion
                .send(error.map_or(Ok(()), |error| Err(anyhow!("{error}"))))
                .ok();
        }
    }

    fn display_map_for_geometry(
        &self,
        pending: &PendingRowGeometry,
        cx: &App,
    ) -> Option<Entity<DisplayMap>> {
        self.geometry_sources(cx)
            .into_iter()
            .find(|source| source.entity_id() == pending.display_map_id())
    }

    fn geometry_sources(&self, cx: &App) -> Vec<Entity<DisplayMap>> {
        let mut sources = vec![self.display_map.clone()];
        sources.extend(self.placeholder_display_map.clone());
        let mut index = 0;
        while let Some(source) = sources.get(index) {
            if let Some((companion, _)) = &source.read(cx).companion
                && let Some(companion) = companion.upgrade()
                && !sources.contains(&companion)
            {
                sources.push(companion);
            }
            index += 1;
        }
        sources
    }

    fn enqueue_deferred_input(&mut self, operation: DeferredInputOperation) {
        if !self.deferred_input.executing {
            self.queued_ime_composition = None;
        }
        self.enqueue_deferred_input_preserving_composition(operation);
    }

    fn enqueue_deferred_input_preserving_composition(
        &mut self,
        mut operation: DeferredInputOperation,
    ) {
        if self.deferred_input.executing {
            if operation.geometry.is_empty() {
                operation.geometry = self.deferred_input.geometry.clone();
            }
            self.deferred_input
                .queue
                .insert(self.deferred_input.insertion_index, operation);
            self.deferred_input.insertion_index += 1;
        } else {
            self.deferred_input.queue.push_back(operation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        editor_tests::init_test,
        inlays::Inlay,
        test::{build_editor, editor_test_context::EditorTestContext},
    };
    use gpui::{Empty, TestAppContext, rgba};
    use language::{LanguageConfig, PLAIN_TEXT, language_settings::SoftWrap};

    gpui::actions!(deferred_input_test, [InsertQueuedAction]);

    #[derive(Clone, PartialEq, Action)]
    #[action(no_json, no_register)]
    struct QueuedSelectionAction(Arc<str>);

    #[gpui::test]
    async fn test_deferred_input_fifo_replans_all_selections(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(format!("ab\ncd\nef\n{}\ngh", "x".repeat(2_000)), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([
                    Point::new(0, 1)..Point::new(0, 1),
                    Point::new(2, 1)..Point::new(2, 1),
                ]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            assert_eq!(
                editor
                    .selections
                    .all::<Point>(&editor.display_snapshot(cx))
                    .into_iter()
                    .map(|selection| selection.head())
                    .collect::<Vec<_>>(),
                [Point::new(0, 1), Point::new(2, 1)],
            );
            editor.handle_input("A", window, cx);
            editor.move_down(&MoveDown, window, cx);
            editor.replay_insert_event("B", Some(-1..0), window, cx);
            editor.buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    [(MultiBufferOffset(0)..MultiBufferOffset(0), "q\n")],
                    None,
                    cx,
                );
            });
            editor.input_completion(cx)
        });
        completion.await.expect("deferred input completed");
        cx.assert_editor_state(&format!("q\nab\ncAd\neBˇ\nxA{}\ngBˇ", "x".repeat(1_999)));
        cx.update_editor(|editor, window, cx| {
            editor.move_left(&MoveLeft, window, cx);
            assert!(!editor.input_is_deferred());
            editor
                .input_completion(cx)
                .now_or_never()
                .expect("ready input completes synchronously")
                .expect("input completed");
        });
    }

    #[gpui::test]
    async fn test_deferred_multicursor_geometry_lifetime(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        cx.background_executor.set_block_on_ticks(0..=0);
        let text = format!(
            "{}{}",
            "xx\n".repeat(200),
            format!("xx\n{}\n", "x".repeat(1_025)).repeat(64)
        );
        let sources = (0..64)
            .map(|index| Point::new(200 + index * 2, 1))
            .collect::<Vec<_>>();
        for cancel in [false, true] {
            let window = cx.add_window(|window, cx| {
                let buffer = MultiBuffer::build_simple(&text, cx);
                let mut editor = build_editor(buffer, window, cx);
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                let style = editor.style(cx).clone();
                editor.set_style(style, window, cx);
                editor
            });
            let mut editor_cx = EditorTestContext::for_editor(window, cx).await;
            if cancel {
                editor_cx.update(|window, cx| {
                    window.replace_root(cx, |_, _| Empty);
                });
            }
            let completion = editor_cx.update_editor(|editor, window, cx| {
                if cancel {
                    let position = editor
                        .buffer
                        .read(cx)
                        .snapshot(cx)
                        .anchor_after(Point::new(327, 10));
                    editor.display_map.update(cx, |map, cx| {
                        map.splice_inlays(
                            &[],
                            vec![Inlay::color(0, position, rgba(0xff0000ff))],
                            cx,
                        );
                    });
                }
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges(sources.iter().copied().map(|point| point..point));
                });
                assert_eq!(
                    editor.display_map.read(cx).retained_row_counts(None),
                    (0, 0, 0)
                );
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                editor.input_completion(cx)
            });
            editor_cx.run_until_parked();
            if cancel {
                editor_cx.update_editor(|editor, _, cx| {
                    assert!(editor.input_is_deferred());
                    assert_eq!(
                        editor.display_map.read(cx).retained_row_counts(None),
                        (64, 63, 64)
                    );
                    let pending = editor
                        .display_snapshot(cx)
                        .long_row(DisplayRow(327))
                        .err()
                        .expect("unmeasured final target");
                    assert!(editor.request_row_geometry(pending, cx));
                });
                editor_cx.update(|window, _| window.remove_window());
                assert_eq!(
                    completion
                        .await
                        .expect_err("window closed during retry")
                        .to_string(),
                    "editor window closed before deferred input completed"
                );
            } else {
                completion.await.expect("all cold targets resolved");
            }
            cx.run_until_parked();
            editor_cx.editor.update(cx, |editor, cx| {
                assert!(editor.deferred_input.task.is_none());
                assert!(editor.deferred_input.queue.is_empty());
                assert!(editor.deferred_input.geometry.is_empty());
                if !cancel {
                    let position_map = editor.last_position_map.as_ref().expect("painted viewport");
                    assert_eq!(
                        editor
                            .display_map
                            .read(cx)
                            .retained_row_counts(Some(&position_map.snapshot.display_snapshot)),
                        (0, 0, 0)
                    );
                }
                assert_eq!(editor.text(cx), text);
                let snapshot = editor.display_snapshot(cx);
                assert_eq!(
                    editor.selections.ranges::<Point>(&snapshot),
                    sources
                        .iter()
                        .map(|source| {
                            let point = Point::new(source.row + u32::from(!cancel), source.column);
                            point..point
                        })
                        .collect::<Vec<_>>()
                );
            });
            if !cancel {
                editor_cx.update_editor(|editor, window, cx| {
                    editor.set_scroll_position(gpui::Point::new(0., 0.), window, cx);
                });
                editor_cx.update(|window, cx| window.draw(cx).clear(cx));
                editor_cx.run_until_parked();
                editor_cx.editor.read_with(cx, |editor, _| {
                    let position_map = editor.last_position_map.as_ref().expect("short viewport");
                    assert_eq!(position_map.visible_row_range.start, DisplayRow(0));
                    assert!(position_map.visible_row_range.end <= DisplayRow(200));
                });
            }
            editor_cx.editor.read_with(cx, |editor, cx| {
                assert_eq!(
                    editor.display_map.read(cx).retained_row_counts(None),
                    (0, 0, 0)
                );
            });
        }
    }

    #[gpui::test]
    async fn test_deferred_geometry_routes_placeholder_and_removed_source(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let events = Rc::new(RefCell::new(Vec::new()));
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_placeholder_text(&"x".repeat(2_000), window, cx);
            let pending = editor
                .placeholder_display_map
                .as_ref()
                .expect("placeholder map")
                .update(cx, |map, cx| {
                    map.snapshot(cx)
                        .long_row(DisplayRow(0))
                        .err()
                        .expect("cold placeholder geometry")
                });
            let events = events.clone();
            editor.defer_geometry(
                pending.clone(),
                move |editor, window, cx| {
                    assert_placeholder_geometry_ready(editor, pending, events, window, cx);
                },
                window,
                cx,
            );
            editor.input_completion(cx)
        });
        completion.await.expect("placeholder geometry completed");
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_placeholder_text(&"y".repeat(2_000), window, cx);
            let pending = editor
                .placeholder_display_map
                .as_ref()
                .expect("placeholder map")
                .update(cx, |map, cx| {
                    map.snapshot(cx)
                        .long_row(DisplayRow(0))
                        .err()
                        .expect("cold replacement placeholder geometry")
                });
            editor.set_placeholder_text("", window, cx);
            let events = events.clone();
            editor.defer_geometry(
                pending,
                move |editor, _, cx| {
                    assert_eq!(editor.placeholder_text(cx), Some(String::new()));
                    events.borrow_mut().push("removed");
                },
                window,
                cx,
            );
            editor.input_completion(cx)
        });
        completion.await.expect("removed geometry source replanned");
        assert_eq!(*events.borrow(), ["placeholder", "removed"]);
    }

    #[gpui::test]
    async fn test_deferred_input_window_close_cancels_waiters(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut editor_cx = EditorTestContext::new(cx).await;
        let text = format!("ab\n{}", "x".repeat(2_000));
        let completions = editor_cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.replace_and_mark_text_in_range(None, "a", None, window, cx);
            [editor.input_completion(cx), editor.input_completion(cx)]
        });
        editor_cx.update(|window, _| window.remove_window());
        editor_cx.editor.read_with(cx, |editor, cx| {
            assert!(editor.deferred_input.task.is_none());
            assert!(editor.deferred_input.queue.is_empty());
            assert!(editor.queued_ime_composition.is_none());
            assert_eq!(editor.text(cx), text);
        });
        for completion in completions {
            assert_eq!(
                completion
                    .await
                    .expect_err("closed window cancels input")
                    .to_string(),
                "editor window closed before deferred input completed"
            );
        }
    }

    #[gpui::test]
    async fn test_deferred_clipboard_commands_preserve_fifo_and_payload(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let text = format!("ab\n{}\ncd", "x".repeat(2_000));
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            cx.write_to_clipboard(ClipboardItem::new_string(String::from("old")));
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.cut(&Cut, window, cx);
            editor.kill_ring_cut(&KillRingCut, window, cx);
            editor.kill_ring_yank(&KillRingYank, window, cx);
            editor.paste(&Paste, window, cx);
            editor.paste_item(&ClipboardItem::new_string(String::from("!")), window, cx);
            assert_eq!(editor.text(cx), text);
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(String::from("old"))
            );
            editor.input_completion(cx)
        });
        completion
            .await
            .expect("deferred clipboard input completed");
        cx.assert_editor_state(&format!("{text}!ˇ"));
        cx.update_editor(|_, _, cx| {
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(format!("{}\n", "x".repeat(2_000)))
            );
        });
    }

    #[gpui::test]
    async fn test_deferred_registered_action_releases_editor_borrow(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let _subscription = cx.update_editor(|editor, _, cx| {
            let handle = cx.weak_entity();
            let subscription = editor.register_action(move |_: &InsertQueuedAction, window, cx| {
                handle
                    .update(cx, |editor, cx| {
                        assert_eq!(
                            editor
                                .selected_text_range(false, window, cx)
                                .map(|selection| selection.range),
                            Some(4..4)
                        );
                        editor.buffer.update(cx, |buffer, cx| {
                            let end = buffer.len(cx);
                            buffer.edit([(end..end, format!("\n{}", "y".repeat(2_000)))], None, cx);
                        });
                        editor.move_down(&MoveDown, window, cx);
                        assert!(editor.input_is_deferred());
                        editor.replace_text_in_range(None, "A", window, cx);
                    })
                    .ok();
            });
            let events = cx.subscribe_self::<EditorEvent>(|editor, event, cx| {
                if let EditorEvent::InputHandled { text, .. } = event
                    && text.as_ref() == "A"
                {
                    assert!(!editor.defer_external_input(
                        |_, _| { panic!("event followup must remain in the executing input") },
                        cx
                    ));
                }
            });
            cx.notify();
            Subscription::join(subscription, events)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let editor = cx.editor.clone();
        let completion = cx.update(|window, cx| {
            let focus = editor.update(cx, |editor, cx| {
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor.set_text(format!("ab\n{}", "x".repeat(2_000)), window, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
                });
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                editor.focus_handle.clone()
            });
            focus.dispatch_action(&InsertQueuedAction, window, cx);
            editor.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(None, "B", None, window, cx);
                editor.input_completion(cx)
            })
        });
        completion
            .await
            .expect("deferred registered action completed");
        cx.assert_editor_state(&format!(
            "ab\n{}\nyABˇ{}",
            "x".repeat(2_000),
            "y".repeat(1_999)
        ));
    }

    #[gpui::test]
    async fn test_deferred_rendered_action_preserves_payload_and_propagation(
        cx: &mut TestAppContext,
    ) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let events = Rc::new(RefCell::new(Vec::new()));
        let payload = Arc::<str>::from("A");
        let _subscription = cx.update_editor(|editor, _, cx| {
            let events = events.clone();
            let payload = payload.clone();
            let subscription = editor.register_action_renderer(move |editor, window, cx| {
                for name in ["first", "second"] {
                    let editor_handle = cx.weak_entity();
                    let events = events.clone();
                    let payload = payload.clone();
                    editor.register_action_handler(
                        window,
                        cx,
                        move |action: &QueuedSelectionAction, window, cx| {
                            assert!(Arc::ptr_eq(&action.0, &payload));
                            editor_handle
                                .update(cx, |editor, cx| {
                                    events.borrow_mut().push((
                                        name,
                                        editor
                                            .selections
                                            .newest::<Point>(&editor.display_snapshot(cx))
                                            .head(),
                                    ));
                                    if name == "second" {
                                        editor.handle_input(&action.0, window, cx);
                                    }
                                })
                                .expect("registered editor is alive");
                            if name == "first" {
                                cx.propagate();
                            }
                        },
                    );
                }
            });
            cx.notify();
            subscription
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let editor = cx.editor.clone();
        let action = QueuedSelectionAction(payload);
        let completion = cx.update(|window, cx| {
            let focus = editor.update(cx, |editor, cx| {
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor.set_text(format!("ab\n{}", "x".repeat(2_000)), window, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
                });
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                editor.focus_handle.clone()
            });
            focus.dispatch_action(&action, window, cx);
            assert!(events.borrow().is_empty());
            editor.update(cx, |editor, cx| {
                editor.handle_input("!", window, cx);
                editor.input_completion(cx)
            })
        });
        completion.await.expect("rendered action completed");
        assert_eq!(
            *events.borrow(),
            [("first", Point::new(1, 1)), ("second", Point::new(1, 1))]
        );
        cx.assert_editor_state(&format!("ab\nxA!ˇ{}", "x".repeat(1_999)));
        cx.update(|window, cx| {
            editor
                .read(cx)
                .focus_handle
                .clone()
                .dispatch_action(&action, window, cx);
            assert_eq!(
                *events.borrow(),
                [
                    ("first", Point::new(1, 1)),
                    ("second", Point::new(1, 1)),
                    ("first", Point::new(1, 3)),
                    ("second", Point::new(1, 3)),
                ]
            );
        });
        cx.assert_editor_state(&format!("ab\nxA!Aˇ{}", "x".repeat(1_999)));
    }

    #[gpui::test]
    async fn test_deferred_columnar_selection_reanchors_tail(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(format!("ab\n{}\ncd", "x".repeat(2_000)), window, cx);
            editor.select(
                SelectPhase::BeginColumnar {
                    position: DisplayPoint::new(DisplayRow(0), 2),
                    goal_column: 4,
                    reset: true,
                    mode: ColumnarMode::FromMouse,
                },
                window,
                cx,
            );
            editor.select(
                SelectPhase::Update {
                    position: DisplayPoint::new(DisplayRow(2), 1),
                    goal_column: 1,
                    scroll_delta: gpui::Point::default(),
                },
                window,
                cx,
            );
            assert!(editor.input_is_deferred());
            editor.select(SelectPhase::End, window, cx);
            editor.buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    [(MultiBufferOffset(0)..MultiBufferOffset(0), "prefix\n")],
                    None,
                    cx,
                );
            });
            editor.input_completion(cx)
        });
        completion.await.expect("deferred selection completed");
        cx.update_editor(|editor, _, cx| {
            assert_eq!(
                editor
                    .selections
                    .all::<Point>(&editor.display_snapshot(cx))
                    .into_iter()
                    .map(|selection| (selection.range(), selection.reversed))
                    .collect::<Vec<_>>(),
                [
                    (Point::new(1, 1)..Point::new(1, 2), true),
                    (Point::new(2, 1)..Point::new(2, 4), true),
                    (Point::new(3, 1)..Point::new(3, 2), true),
                ],
            );
        });
    }

    #[gpui::test]
    async fn test_deferred_comment_advance_preserves_nested_redo_selection(
        cx: &mut TestAppContext,
    ) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| {
            buffer.set_language(
                Some(Arc::new(Language::new(
                    LanguageConfig {
                        line_comments: vec![Arc::from("# ")],
                        ..LanguageConfig::default()
                    },
                    None,
                ))),
                cx,
            );
        });
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(format!("ab\n{}", "x".repeat(2_000)), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.finalize_last_transaction(cx);
            editor.transact(window, cx, |editor, window, cx| {
                editor.toggle_comments(
                    &ToggleComments {
                        advance_downwards: true,
                        ignore_indent: false,
                        comment_empty_lines: false,
                    },
                    window,
                    cx,
                );
            });
            assert!(editor.input_is_deferred());
            editor.undo(&Undo, window, cx);
            editor.redo(&Redo, window, cx);
            editor.input_completion(cx)
        });
        completion
            .await
            .expect("deferred comment advance completed");
        cx.assert_editor_state(&format!("# ab\nxxxˇ{}", "x".repeat(1_997)));
    }

    #[gpui::test]
    async fn test_deferred_ime_autoclose_skip_records_existing_text(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        for replace_closer in [false, true] {
            let mut editor_cx = EditorTestContext::new(cx).await;
            editor_cx.update_buffer(|buffer, cx| {
                buffer.set_language(Some(PLAIN_TEXT.clone()), cx);
            });
            let completion = editor_cx.update_editor(|editor, window, cx| {
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor.set_text(format!("ab\n{}", "x".repeat(2_000)), window, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
                });
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
                editor.replace_text_in_range(None, "(", window, cx);
                editor.replace_text_in_range(None, ")", window, cx);
                assert_eq!(
                    editor
                        .selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(3..3)
                );
                if replace_closer {
                    editor.replace_text_in_range(Some(2..3), "]", window, cx);
                }
                editor.input_completion(cx)
            });
            completion.await.expect("autoclose input completed");
            let pair = if replace_closer { "(]" } else { "()" };
            editor_cx.assert_editor_state(&format!("ab\n{}{pair}ˇ", "x".repeat(2_000)));
        }
    }

    #[gpui::test]
    async fn test_deferred_ime_surrounding_query_excludes_unresolved_destination(
        cx: &mut TestAppContext,
    ) {
        init_test(cx, |_| {});
        for use_adjusted_range in [false, true] {
            let mut editor_cx = EditorTestContext::new(cx).await;
            let text = format!("ab\n{}", "x".repeat(2_000));
            let completion = editor_cx.update_editor(|editor, window, cx| {
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor.set_text(text.clone(), window, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
                });
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                assert_eq!(editor.text_for_range(0..2, &mut None, window, cx), None);
                editor.replace_text_in_range(None, "a", window, cx);
                let selection = editor
                    .selected_text_range(false, window, cx)
                    .expect("projected selection");
                assert_eq!(selection.range, 2..2);
                let requested = selection.range.start - 2..selection.range.start;
                let mut adjusted_range = None;
                assert_eq!(
                    editor.text_for_range(requested.clone(), &mut adjusted_range, window, cx),
                    Some(String::from("a"))
                );
                assert_eq!(adjusted_range, Some(1..2));
                let replacement = if use_adjusted_range {
                    adjusted_range.expect("known queued text range")
                } else {
                    requested
                };
                editor.replace_text_in_range(Some(replacement), "A", window, cx);
                assert_eq!(
                    editor
                        .selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(if use_adjusted_range { 2..2 } else { 1..1 })
                );
                assert_eq!(editor.text(cx), text);
                editor.input_completion(cx)
            });
            completion.await.expect("surrounding replacement completed");
            let expected = if use_adjusted_range {
                format!("ab\nxAˇ{}", "x".repeat(1_999))
            } else {
                format!("ab\nAˇ{}", "x".repeat(1_999))
            };
            editor_cx.assert_editor_state(&expected);
        }
    }

    #[gpui::test]
    async fn test_deferred_ime_explicit_replacement_after_roundtrip_movement(
        cx: &mut TestAppContext,
    ) {
        init_test(cx, |_| {});
        for (replacement, selection, expected) in
            [(0..2, 1..1, "Aˇb"), (1..3, 2..2, "aAˇ"), (0..3, 1..1, "Aˇ")]
        {
            let mut editor_cx = EditorTestContext::new(cx).await;
            let text = format!("ab\n{}", "x".repeat(2_000));
            let completion = editor_cx.update_editor(|editor, window, cx| {
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor.set_text(text.clone(), window, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
                });
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                editor.move_up(&MoveUp, window, cx);
                editor.replace_text_in_range(None, "a", window, cx);
                editor.replace_text_in_range(Some(replacement), "A", window, cx);
                assert_eq!(
                    editor
                        .selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(selection)
                );
                assert_eq!(editor.text(cx), text);
                editor.input_completion(cx)
            });
            completion.await.expect("roundtrip replacement completed");
            editor_cx.assert_editor_state(&format!("{expected}\n{}", "x".repeat(2_000)));
        }
    }

    #[gpui::test]
    async fn test_deferred_ime_mixed_replacements_share_replay_origin(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let text = format!("ab\n{}", "x".repeat(2_000));
        let destination = "ab\n".len();
        let mut expected = text.clone();
        for (range, replacement) in [(1..1, "a"), (0..1, "Z"), (0..2, "Q")] {
            expected.replace_range(
                destination + range.start..destination + range.end,
                replacement,
            );
        }
        assert_eq!(expected, format!("ab\nQ{}", "x".repeat(1_999)));
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.replace_text_in_range(None, "a", window, cx);
            editor.replace_text_in_range(Some(0..1), "Z", window, cx);
            assert_eq!(
                editor.text_for_range(0..2, &mut None, window, cx),
                Some(String::from("Za"))
            );
            editor.replace_text_in_range(Some(0..2), "Q", window, cx);
            assert_eq!(editor.text(cx), text);
            editor.input_completion(cx)
        });
        completion.await.expect("mixed replacement input completed");
        expected.insert(destination + 1, 'ˇ');
        cx.assert_editor_state(&expected);
    }

    #[gpui::test]
    async fn test_deferred_ime_restarts_origin_after_queued_movement(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let text = format!("ab\n{}", "x".repeat(2_000));
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.replace_text_in_range(None, "a", window, cx);
            assert_eq!(
                editor.text_for_range(1..2, &mut None, window, cx),
                Some(String::from("a"))
            );
            editor.move_left(&MoveLeft, window, cx);
            assert!(editor.queued_ime_composition.is_none());
            editor.replace_text_in_range(None, "b", window, cx);
            let mut adjusted_range = None;
            assert_eq!(
                editor.text_for_range(1..3, &mut adjusted_range, window, cx),
                Some(String::from("b"))
            );
            assert_eq!(adjusted_range, Some(1..2));
            editor.replace_text_in_range(Some(1..3), "Q", window, cx);
            assert_eq!(editor.text(cx), text);
            editor.input_completion(cx)
        });
        completion
            .await
            .expect("movement-separated input completed");
        cx.assert_editor_state(&format!("ab\nxQˇ{}", "x".repeat(1_999)));
    }

    #[gpui::test]
    async fn test_deferred_ime_tracks_newest_writable_selection(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        for writable in [true, false] {
            let first = format!("ab\n{}", "x".repeat(2_000));
            let second = format!("cd\n{}", "y".repeat(2_000));
            let buffers =
                [(first.as_str(), writable), (second.as_str(), false)].map(|(text, writable)| {
                    cx.new(|cx| {
                        let mut buffer = Buffer::local(text, cx);
                        if !writable {
                            buffer.set_capability(language::Capability::ReadOnly, cx);
                        }
                        buffer
                    })
                });
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::new(language::Capability::ReadWrite);
                for (index, buffer) in buffers.iter().enumerate() {
                    multibuffer.set_excerpts_for_path(
                        PathKey::sorted(index as u64),
                        buffer.clone(),
                        [Point::zero()..buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                }
                multibuffer
            });
            let window = cx.add_window(|window, cx| {
                let editor = build_editor(multibuffer, window, cx);
                window.focus(&editor.focus_handle(cx), cx);
                editor
            });
            let mut editor_cx = EditorTestContext::for_editor(window, cx).await;
            let completion = editor_cx.update_editor(|editor, window, cx| {
                assert!(!editor.read_only(cx));
                editor.set_soft_wrap_mode(SoftWrap::None, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([
                        Point::new(0, 1)..Point::new(0, 1),
                        Point::new(2, 1)..Point::new(2, 1),
                    ]);
                });
                assert_eq!(
                    editor
                        .selections
                        .newest::<Point>(&editor.display_snapshot(cx))
                        .head(),
                    Point::new(2, 1)
                );
                editor.move_down(&MoveDown, window, cx);
                assert!(editor.input_is_deferred());
                editor.replace_text_in_range(None, "a", window, cx);
                let selection = editor
                    .selected_text_range(false, window, cx)
                    .expect("projected input selection");
                let end = selection.range.end;
                assert_eq!(
                    editor.text_for_range(end - 1..end, &mut None, window, cx),
                    Some(String::from("a"))
                );
                editor.replace_text_in_range(Some(end - 1..end), "Q", window, cx);
                editor.input_completion(cx)
            });
            completion.await.expect("mixed-capability input completed");
            editor_cx.update_editor(|editor, _, cx| assert!(!editor.read_only(cx)));
            let expected_first = if writable {
                format!("ab\nxQ{}", "x".repeat(1_999))
            } else {
                first
            };
            let mut texts = editor_cx.multibuffer(|buffer, cx| {
                buffer
                    .all_buffers()
                    .into_iter()
                    .map(|buffer| buffer.read(cx).text())
                    .collect::<Vec<_>>()
            });
            texts.sort();
            assert_eq!(texts, [expected_first, second]);
            editor_cx.update_editor(|editor, _, cx| {
                let heads = editor
                    .selections
                    .all::<Point>(&editor.display_snapshot(cx))
                    .into_iter()
                    .map(|selection| selection.head())
                    .collect::<Vec<_>>();
                assert_eq!(
                    heads,
                    if writable {
                        vec![Point::new(1, 2)]
                    } else {
                        vec![Point::new(1, 1), Point::new(3, 1)]
                    }
                );
            });
        }
    }

    #[gpui::test]
    async fn test_deferred_manipulation_helpers_preserve_fifo(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let text = format!("ab\n{}", "x".repeat(2_000));
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.manipulate_text(window, cx, |text| {
                assert_eq!(text, "x".repeat(2_000));
                text.to_uppercase()
            });
            editor.manipulate_immutable_lines(window, cx, |lines| {
                assert_eq!(lines.as_slice(), ["X".repeat(2_000).as_str()]);
                lines.push("done");
            });
            assert_eq!(editor.text(cx), text);
            editor.input_completion(cx)
        });
        completion
            .await
            .expect("shared manipulation helpers completed");
        cx.assert_editor_state(&format!("ab\n«{}\ndoneˇ»", "X".repeat(2_000)));
    }

    #[gpui::test]
    async fn test_deferred_ime_projects_consecutive_commits(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let text = format!("ab\n{}", "x".repeat(2_000));
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.replace_text_in_range(None, "a", window, cx);
            assert_eq!(
                editor
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range),
                Some(2..2)
            );
            editor.replace_text_in_range(None, "b", window, cx);
            assert_eq!(
                editor
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range),
                Some(3..3)
            );
            assert_eq!(
                editor.text_for_range(1..3, &mut None, window, cx),
                Some(String::from("ab"))
            );
            editor.replace_text_in_range(Some(1..2), "A", window, cx);
            assert_eq!(
                editor
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range),
                Some(2..2)
            );
            assert_eq!(editor.marked_text_range(window, cx), None);
            let mut adjusted_range = None;
            assert_eq!(
                editor.text_for_range(0..4, &mut adjusted_range, window, cx),
                Some(String::from("Ab"))
            );
            assert_eq!(adjusted_range, Some(1..3));
            assert_eq!(editor.text(cx), text);
            editor.input_completion(cx)
        });
        completion.await.expect("projected IME input completed");
        cx.assert_editor_state(&format!("ab\nxAˇb{}", "x".repeat(1_999)));
    }

    #[gpui::test]
    async fn test_deferred_ime_preserves_marked_and_replacement_ranges(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;
        let completion = cx.update_editor(|editor, window, cx| {
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_text(format!("ab\n{}", "x".repeat(2_000)), window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
            });
            editor.move_down(&MoveDown, window, cx);
            assert!(editor.input_is_deferred());
            editor.replace_and_mark_text_in_range(Some(1..1), "é", Some(1..1), window, cx);
            editor.replace_and_mark_text_in_range(Some(0..1), "😀", Some(2..2), window, cx);
            assert_eq!(editor.marked_text_range(window, cx), Some(1..3));
            assert_eq!(
                editor
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range),
                Some(3..3),
            );
            let mut adjusted_range = None;
            assert_eq!(
                editor.text_for_range(2..2, &mut adjusted_range, window, cx),
                Some(String::from("😀")),
            );
            assert_eq!(adjusted_range, Some(1..3));
            let replacement = editor
                .marked_text_range(window, cx)
                .expect("queued composition range");
            editor.unmark_text(window, cx);
            assert_eq!(editor.marked_text_range(window, cx), None);
            assert_eq!(
                editor
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range),
                Some(3..3)
            );
            editor.replace_text_in_range(Some(replacement), "界", window, cx);
            assert_eq!(editor.marked_text_range(window, cx), None);
            assert_eq!(
                editor
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range),
                Some(2..2)
            );
            assert_eq!(
                editor.text_for_range(1..2, &mut None, window, cx),
                Some(String::from("界"))
            );
            editor.replace_text_in_range(Some(0..1), "Z", window, cx);
            editor.handle_input("!", window, cx);
            editor.buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    [(MultiBufferOffset(0)..MultiBufferOffset(0), "q\n")],
                    None,
                    cx,
                );
            });
            editor.input_completion(cx)
        });
        completion.await.expect("deferred input completed");
        cx.assert_editor_state(&format!("q\nab\nZ!ˇ界{}", "x".repeat(1_999)));
        for (add_cursor, length) in [(false, 2_001), (true, 2_002)] {
            let completion = cx.update_editor(|editor, window, cx| {
                editor.clear_highlights(HighlightKey::InputComposition, cx);
                editor.set_text(format!("ab\n{}", "y".repeat(length)), window, cx);
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([Point::new(0, 1)..Point::new(0, 1)]);
                });
                editor.replace_and_mark_text_in_range(None, "é", None, window, cx);
                if add_cursor {
                    editor.add_selection_below(
                        &AddSelectionBelow {
                            skip_soft_wrap: false,
                        },
                        window,
                        cx,
                    );
                } else {
                    editor.move_down(&MoveDown, window, cx);
                }
                assert!(editor.input_is_deferred());
                editor.replace_and_mark_text_in_range(Some(0..1), "界", None, window, cx);
                editor.clear_highlights(HighlightKey::InputComposition, cx);
                editor.input_completion(cx)
            });
            completion.await.expect("captured marked range completed");
            cx.assert_editor_state(&format!("a界ˇb\n{}", "y".repeat(length)));
        }
    }

    fn assert_placeholder_geometry_ready(
        editor: &mut Editor,
        requested: PendingRowGeometry,
        events: Rc<RefCell<Vec<&'static str>>>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let source = editor
            .placeholder_display_map
            .as_ref()
            .expect("placeholder map");
        assert_eq!(source.entity_id(), requested.display_map_id());
        let snapshot = source.update(cx, |map, cx| map.snapshot(cx));
        match snapshot.long_row(DisplayRow(0)) {
            Ok(ready) => {
                assert!(ready.is_some());
                events.borrow_mut().push("placeholder");
            }
            Err(pending) => {
                assert!(
                    !requested.is_current(&snapshot),
                    "current placeholder request completed without geometry"
                );
                editor.defer_geometry(
                    pending.clone(),
                    move |editor, window, cx| {
                        assert_placeholder_geometry_ready(editor, pending, events, window, cx);
                    },
                    window,
                    cx,
                );
            }
        }
    }
}
