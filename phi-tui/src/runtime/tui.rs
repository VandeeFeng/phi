use std::time::{Duration, Instant};

use super::mounts::WidgetRuntime;
use crate::{
    FocusId, Frame, InputContext, InputOutcome, KeyBindingResolver, KeyInput, MountOptions,
    RenderArea, Theme, TuiRenderer, Widget, WidgetHandle, poll_key_input_with_timeout,
    render_overlay_layers,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderRequest {
    pub revision: u64,
    pub priority: RenderPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum RenderPriority {
    Background,
    Animation,
    #[default]
    Normal,
    Interactive,
    Immediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderInvalidator {
    revision: u64,
    requested: bool,
    priority: RenderPriority,
}

impl RenderInvalidator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn request(&mut self) {
        self.request_with_priority(RenderPriority::Normal);
    }

    pub fn request_with_priority(&mut self, priority: RenderPriority) {
        self.revision = self.revision.saturating_add(1);
        self.requested = true;
        self.priority = self.priority.max(priority);
    }

    pub fn is_requested(&self) -> bool {
        self.requested
    }

    pub fn take_request(&mut self) -> Option<RenderRequest> {
        if !self.requested {
            self.priority = RenderPriority::Normal;
            return None;
        }
        self.requested = false;
        let priority = self.priority;
        self.priority = RenderPriority::Normal;
        Some(RenderRequest {
            revision: self.revision,
            priority,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderScheduler {
    invalidator: RenderInvalidator,
    last_rendered_revision: u64,
}

impl RenderScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_render(&mut self) {
        self.invalidator.request();
    }

    pub fn request_render_with_priority(&mut self, priority: RenderPriority) {
        self.invalidator.request_with_priority(priority);
    }

    pub fn is_requested(&self) -> bool {
        self.invalidator.is_requested()
    }

    pub fn next_request(&mut self) -> Option<RenderRequest> {
        let request = self.invalidator.take_request()?;
        (request.revision != self.last_rendered_revision).then_some(request)
    }

    pub fn mark_rendered(&mut self, request: RenderRequest) {
        self.last_rendered_revision = request.revision;
    }

    pub fn priority(&self) -> RenderPriority {
        self.invalidator.priority
    }

    pub fn revision(&self) -> u64 {
        self.invalidator.revision()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Idle,
    Resize,
    TerminalFocus(bool),
    Key(KeyInput),
}

impl RuntimeEvent {
    pub fn from_key_input(key: KeyInput) -> Self {
        match key {
            KeyInput::None => Self::Idle,
            KeyInput::Resize => Self::Resize,
            KeyInput::FocusGained => Self::TerminalFocus(true),
            KeyInput::FocusLost => Self::TerminalFocus(false),
            key => Self::Key(key),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ComposedFrame {
    pub frame: Frame,
    pub overlays: Vec<crate::OverlayLayer>,
}

pub struct TuiRuntime<A: 'static> {
    widgets: WidgetRuntime<A>,
    bindings: KeyBindingResolver<A>,
    render: RenderScheduler,
    min_render_interval: Duration,
    last_render_at: Option<Instant>,
    last_terminal_size: Option<(u16, u16)>,
    terminal_focused: bool,
}

impl<A: 'static> Default for TuiRuntime<A> {
    fn default() -> Self {
        Self {
            widgets: WidgetRuntime::default(),
            bindings: KeyBindingResolver::default(),
            render: RenderScheduler::default(),
            min_render_interval: Duration::from_millis(16),
            last_render_at: None,
            last_terminal_size: None,
            terminal_focused: true,
        }
    }
}

impl<A: 'static> TuiRuntime<A> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mount<C, F>(
        &mut self,
        widget: C,
        options: MountOptions,
        map_action: F,
    ) -> WidgetHandle<C>
    where
        C: Widget + 'static,
        F: FnMut(C::Action) -> A + 'static,
    {
        let handle = self.widgets.mount(widget, options, map_action);
        self.request_render();
        handle
    }

    pub fn mount_widget<C>(&mut self, widget: C, options: MountOptions) -> WidgetHandle<C>
    where
        C: Widget<Action = A> + 'static,
    {
        self.mount(widget, options, |action| action)
    }

    pub fn widget<C: 'static>(&self, handle: WidgetHandle<C>) -> Option<&C> {
        self.widgets.get(handle)
    }

    pub fn update_widget<C: 'static, R>(
        &mut self,
        handle: WidgetHandle<C>,
        update: impl FnOnce(&mut C) -> R,
    ) -> Option<R> {
        let result = self.widgets.update(handle, update);
        if result.is_some() {
            self.request_render();
        }
        result
    }

    pub fn remove_widget<C: 'static>(&mut self, handle: WidgetHandle<C>) -> Option<C> {
        let removed = self.widgets.remove(handle);
        if removed.is_some() {
            self.request_render();
        }
        removed
    }

    pub fn is_widget_visible<C: 'static>(&self, handle: WidgetHandle<C>) -> bool {
        self.widgets.is_visible(handle)
    }

    pub fn show_widget<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        self.set_widget_visible(handle, true)
    }

    pub fn hide_widget<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        self.set_widget_visible(handle, false)
    }

    pub fn show_and_focus_widget<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        let shown = self.show_widget(handle);
        self.focus_widget(handle) || shown
    }

    pub fn set_widget_input_context<C: 'static>(
        &mut self,
        handle: WidgetHandle<C>,
        context: InputContext,
    ) -> bool {
        let changed = self.widgets.set_input_context(handle, context);
        self.request_render_when(changed);
        changed
    }

    pub fn set_widget_visible<C: 'static>(
        &mut self,
        handle: WidgetHandle<C>,
        visible: bool,
    ) -> bool {
        let changed = self.widgets.set_visible(handle, visible);
        self.request_render_when(changed);
        changed
    }

    pub fn focus_widget<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        let changed = self.widgets.focus(handle);
        self.request_render_when(changed);
        changed
    }

    pub fn unfocus_widget<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        let changed = self.widgets.unfocus(handle);
        self.request_render_when(changed);
        changed
    }

    pub fn restore_focus(&mut self) -> FocusId {
        let focus = self.widgets.restore_focus();
        self.request_render();
        focus
    }

    pub fn active_focus(&self) -> FocusId {
        self.widgets.active_focus()
    }

    pub fn input_context(&self) -> InputContext {
        self.widgets.input_context()
    }

    pub fn set_key_bindings(&mut self, bindings: KeyBindingResolver<A>) {
        self.bindings = bindings;
    }

    pub fn key_bindings(&self) -> &KeyBindingResolver<A> {
        &self.bindings
    }

    pub fn dispatch_key(&mut self, key: KeyInput) -> InputOutcome<A>
    where
        A: Clone,
    {
        let mut outcome = self.widgets.handle_input(&key);
        if matches!(outcome, InputOutcome::Ignored)
            && let Some(action) = self.bindings.resolve(self.input_context(), &key)
        {
            outcome = InputOutcome::Action(action);
        }
        if outcome.is_consumed() {
            self.request_interactive_render();
        }
        outcome
    }

    pub fn poll_event(&mut self) -> anyhow::Result<RuntimeEvent> {
        self.poll_event_with_timeout(crate::terminal::DEFAULT_POLL_TIMEOUT)
    }

    pub fn poll_event_with_timeout(&mut self, timeout: Duration) -> anyhow::Result<RuntimeEvent> {
        let event = RuntimeEvent::from_key_input(poll_key_input_with_timeout(timeout)?);
        match event {
            RuntimeEvent::Resize => self.request_immediate_render(),
            RuntimeEvent::TerminalFocus(focused) => {
                self.terminal_focused = focused;
                self.request_render();
            }
            RuntimeEvent::Idle | RuntimeEvent::Key(_) => {}
        }
        Ok(event)
    }

    pub fn terminal_focused(&self) -> bool {
        self.terminal_focused
    }

    pub fn terminal_size(&self) -> anyhow::Result<(u16, u16)> {
        Ok(crossterm::terminal::size()?)
    }

    pub fn render_area(&self) -> anyhow::Result<RenderArea> {
        let (width, height) = self.terminal_size()?;
        Ok(RenderArea {
            width: width.saturating_sub(1).max(24),
            height: Some(height as usize),
        })
    }

    pub fn terminal_resized(&mut self) -> anyhow::Result<bool> {
        let size = self.terminal_size()?;
        let resized = self
            .last_terminal_size
            .is_some_and(|previous| previous != size);
        self.last_terminal_size = Some(size);
        self.request_render_when(resized);
        Ok(resized)
    }

    pub fn request_render(&mut self) {
        self.render.request_render();
    }

    pub fn request_render_with_priority(&mut self, priority: RenderPriority) {
        self.render.request_render_with_priority(priority);
    }

    pub fn request_interactive_render(&mut self) {
        self.request_render_with_priority(RenderPriority::Interactive);
    }

    pub fn request_immediate_render(&mut self) {
        self.request_render_with_priority(RenderPriority::Immediate);
    }

    pub fn request_render_when(&mut self, condition: bool) {
        if condition {
            self.request_render();
        }
    }

    pub fn render_requested(&self) -> bool {
        self.render.is_requested()
    }

    pub fn set_min_render_interval(&mut self, interval: Duration) {
        self.min_render_interval = interval;
    }

    fn mark_rendered_now(&mut self) {
        self.last_render_at = Some(Instant::now());
    }

    pub fn render_due(&self) -> bool {
        self.render_requested()
            && self.last_render_at.is_none_or(|last_render_at| {
                last_render_at.elapsed()
                    >= self.render_interval_for_priority(self.render.priority())
            })
    }

    pub fn render_wait_duration(&self) -> Option<Duration> {
        if !self.render_requested() {
            return None;
        }
        let last_render_at = self.last_render_at?;
        self.render_interval_for_priority(self.render.priority())
            .checked_sub(last_render_at.elapsed())
    }

    fn next_render_request(&mut self) -> Option<RenderRequest> {
        self.render.next_request()
    }

    fn mark_rendered(&mut self, request: RenderRequest) {
        self.render.mark_rendered(request);
        self.mark_rendered_now();
    }

    pub fn render_revision(&self) -> u64 {
        self.render.revision()
    }

    pub fn compose(&self, area: RenderArea, base: &Frame, theme: &Theme) -> ComposedFrame {
        self.compose_owned(area, base.clone(), theme)
    }

    fn compose_owned(&self, area: RenderArea, mut base: Frame, theme: &Theme) -> ComposedFrame {
        let rendered = self.widgets.render(area, theme, base.lines.len());
        base.lines.extend(rendered.frame.lines);
        base.cursor = rendered.frame.cursor.or(base.cursor);
        ComposedFrame {
            frame: base,
            overlays: rendered.overlays,
        }
    }

    pub fn render_widgets(
        &mut self,
        renderer: &mut TuiRenderer,
        theme: &Theme,
    ) -> anyhow::Result<()> {
        let area = self.render_area()?;
        self.render_widgets_in_area(renderer, area, theme)
    }

    pub fn render_widgets_in_area(
        &mut self,
        renderer: &mut TuiRenderer,
        area: RenderArea,
        theme: &Theme,
    ) -> anyhow::Result<()> {
        self.render_frame(renderer, area, &Frame::default(), theme)
    }

    pub fn render_with_base(
        &mut self,
        renderer: &mut TuiRenderer,
        base: &Frame,
        theme: &Theme,
    ) -> anyhow::Result<()> {
        let area = self.render_area()?;
        self.render_frame(renderer, area, base, theme)
    }

    pub fn render_frame(
        &mut self,
        renderer: &mut TuiRenderer,
        area: RenderArea,
        base: &Frame,
        theme: &Theme,
    ) -> anyhow::Result<()> {
        self.render_owned_frame(renderer, area, base.clone(), theme)
    }

    pub fn render_owned_frame(
        &mut self,
        renderer: &mut TuiRenderer,
        area: RenderArea,
        base: Frame,
        theme: &Theme,
    ) -> anyhow::Result<()> {
        let request = self.next_render_request();
        let composed = self.compose_owned(area, base, theme);
        let result = paint_composed(renderer, area, composed);
        self.finish_render(request, result)
    }

    pub fn render_if_requested(
        &mut self,
        renderer: &mut TuiRenderer,
        base: &Frame,
        theme: &Theme,
    ) -> anyhow::Result<bool> {
        if !self.render_due() {
            return Ok(false);
        }
        self.render_with_base(renderer, base, theme)?;
        Ok(true)
    }

    fn finish_render(
        &mut self,
        request: Option<RenderRequest>,
        result: anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if let Err(error) = result {
            if let Some(request) = request {
                self.request_render_with_priority(request.priority);
            }
            return Err(error);
        }
        if let Some(request) = request {
            self.mark_rendered(request);
        } else {
            self.mark_rendered_now();
        }
        Ok(())
    }

    fn render_interval_for_priority(&self, priority: RenderPriority) -> Duration {
        match priority {
            RenderPriority::Immediate => Duration::ZERO,
            RenderPriority::Interactive => self.min_render_interval / 4,
            RenderPriority::Normal => self.min_render_interval,
            RenderPriority::Animation | RenderPriority::Background => self.min_render_interval * 2,
        }
    }
}

fn paint_composed(
    renderer: &mut TuiRenderer,
    area: RenderArea,
    composed: ComposedFrame,
) -> anyhow::Result<()> {
    let ComposedFrame { frame, overlays } = composed;
    let rendered_overlays = render_overlay_layers(
        &overlays,
        area.width,
        area.height.unwrap_or(frame.lines.len()),
    );
    renderer.render_materialized_with_overlays(
        frame.lines,
        &rendered_overlays,
        frame.cursor,
        frame.logical_origin,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CursorPosition, StyledLine, ThemeColor};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Action {
        Submit,
    }

    struct TestWidget {
        outcome: InputOutcome<&'static str>,
    }

    impl Widget for TestWidget {
        type Action = &'static str;

        fn render(&self, context: &crate::RenderContext<'_>) -> Vec<StyledLine> {
            let area = context.area;
            vec![StyledLine::new("test", ThemeColor::Default).fit(area.width)]
        }

        fn handle_input(&mut self, _key: &KeyInput) -> InputOutcome<Self::Action> {
            self.outcome.clone()
        }

        fn cursor(&self, _area: RenderArea) -> Option<CursorPosition> {
            Some(CursorPosition { row: 0, col: 0 })
        }
    }

    fn focused_options(context: InputContext) -> MountOptions {
        MountOptions::after_root(context).focusable()
    }

    #[test]
    fn maps_widget_actions_and_requests_render() {
        let mut runtime = TuiRuntime::<Action>::new();
        let handle = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Action("submit"),
            },
            focused_options(InputContext::Editor),
            |_| Action::Submit,
        );
        runtime.focus_widget(handle);

        assert_eq!(
            runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(Action::Submit)
        );
        assert!(runtime.render_requested());
        assert_eq!(runtime.input_context(), InputContext::Editor);
    }

    #[test]
    fn focus_restores_visible_widgets_but_not_hidden_history() {
        let mut runtime = TuiRuntime::<Action>::new();
        let first = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            focused_options(InputContext::Editor),
            |_| Action::Submit,
        );
        let second = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            focused_options(InputContext::Picker),
            |_| Action::Submit,
        );

        runtime.focus_widget(first);
        runtime.focus_widget(second);
        runtime.set_widget_visible(second, false);

        assert_eq!(runtime.active_focus(), first.id());
        runtime.set_widget_visible(first, false);

        assert_eq!(runtime.active_focus(), FocusId::ROOT);
        assert_eq!(runtime.input_context(), InputContext::Global);
    }

    #[test]
    fn updates_focused_widget_input_context() {
        let mut runtime = TuiRuntime::<Action>::new();
        let handle = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions::input(InputContext::Editor).focused(),
            |_| Action::Submit,
        );

        assert!(runtime.set_widget_input_context(handle, InputContext::Picker));
        assert_eq!(runtime.input_context(), InputContext::Picker);
        assert!(runtime.render_requested());
    }

    #[test]
    fn update_requests_render() {
        let mut runtime = TuiRuntime::<Action>::new();
        let handle = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions::default(),
            |_| Action::Submit,
        );
        let request = runtime.next_render_request().expect("mount render request");
        runtime.mark_rendered(request);

        runtime.update_widget(handle, |_| {});

        assert!(runtime.render_requested());
    }

    #[test]
    fn failed_render_requeues_the_consumed_priority() {
        let mut runtime = TuiRuntime::<Action>::new();
        runtime.request_render_with_priority(RenderPriority::Immediate);
        let request = runtime.next_render_request();

        assert!(
            runtime
                .finish_render(request, Err(anyhow::anyhow!("paint failed")))
                .is_err()
        );
        assert!(runtime.render_requested());
        assert_eq!(runtime.render.priority(), RenderPriority::Immediate);
    }

    #[test]
    fn preserves_base_cursor_when_input_only_widget_owns_focus() {
        let mut runtime = TuiRuntime::<Action>::new();
        runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions::input(InputContext::Editor).focused(),
            |_| Action::Submit,
        );
        let base = Frame {
            lines: vec![StyledLine::new("root", ThemeColor::Default)],
            cursor: Some(crate::Cursor { row: 0, col: 2 }),
            ..Frame::default()
        };

        let composed = runtime.compose(RenderArea::width(20), &base, &Theme::default());

        assert_eq!(composed.frame.cursor, base.cursor);
    }

    #[test]
    fn composes_headlessly_over_an_explicit_root_frame() {
        let mut runtime = TuiRuntime::<Action>::new();
        runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions::default(),
            |_| Action::Submit,
        );
        let base = Frame {
            lines: vec![StyledLine::new("root", ThemeColor::Default)],
            cursor: None,
            ..Frame::default()
        };

        let composed = runtime.compose(
            RenderArea {
                width: 20,
                height: Some(10),
            },
            &base,
            &Theme::default(),
        );

        assert_eq!(
            composed
                .frame
                .lines
                .iter()
                .map(|line| line.text.trim())
                .collect::<Vec<_>>(),
            vec!["root", "test"]
        );
        assert!(composed.overlays.is_empty());
    }

    #[test]
    fn nested_overlay_show_and_focus_policy_is_explicit() {
        let mut runtime = TuiRuntime::<Action>::new();
        let first = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions::overlay(
                InputContext::Overlay,
                crate::OverlayOptions::centered_panel(),
            ),
            |_| Action::Submit,
        );
        let second = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions::overlay(
                InputContext::Dialog,
                crate::OverlayOptions::centered_panel(),
            ),
            |_| Action::Submit,
        );

        runtime.hide_widget(second);
        assert_eq!(runtime.active_focus(), first.id());
        runtime.show_widget(second);
        assert_eq!(runtime.active_focus(), first.id());
        assert!(runtime.show_and_focus_widget(second));
        assert_eq!(runtime.active_focus(), second.id());
        assert_eq!(runtime.dispatch_key(KeyInput::Esc), InputOutcome::Consumed);
    }

    #[test]
    fn capturing_widget_blocks_fallback_bindings() {
        let mut runtime = TuiRuntime::<Action>::new();
        runtime.set_key_bindings(KeyBindingResolver::new().bind(
            InputContext::Global,
            KeyInput::Esc,
            Action::Submit,
        ));
        let handle = runtime.mount(
            TestWidget {
                outcome: InputOutcome::Ignored,
            },
            MountOptions {
                capture_input: true,
                ..focused_options(InputContext::Overlay)
            },
            |_| Action::Submit,
        );
        runtime.focus_widget(handle);

        assert_eq!(runtime.dispatch_key(KeyInput::Esc), InputOutcome::Consumed);
    }
}
