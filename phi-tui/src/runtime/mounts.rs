use std::any::Any;

use super::focus::FocusHost;
use crate::{
    Cursor, FocusId, InputContext, InputOutcome, KeyInput, MountOptions, MountPlacement,
    OverlayLayer, RenderArea, RenderContext, StyledLine, Theme, Widget, WidgetHandle,
    resolve_overlay_frame,
};

pub(crate) struct RenderedWidgets {
    pub frame: crate::Frame,
    pub overlays: Vec<OverlayLayer>,
}

trait ErasedWidget<A> {
    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine>;
    fn handle_input(&mut self, input: &KeyInput) -> InputOutcome<A>;
    fn cursor(&self, area: RenderArea) -> Option<crate::CursorPosition>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_widget(self: Box<Self>) -> Box<dyn Any>;
}

struct MappedWidget<C, F> {
    widget: C,
    map_action: F,
}

impl<A, C, F> ErasedWidget<A> for MappedWidget<C, F>
where
    A: 'static,
    C: Widget + 'static,
    F: FnMut(C::Action) -> A + 'static,
{
    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        self.widget.render(context)
    }

    fn handle_input(&mut self, input: &KeyInput) -> InputOutcome<A> {
        match self.widget.handle_input(input) {
            InputOutcome::Ignored => InputOutcome::Ignored,
            InputOutcome::Consumed => InputOutcome::Consumed,
            InputOutcome::Action(action) => InputOutcome::Action((self.map_action)(action)),
        }
    }

    fn cursor(&self, area: RenderArea) -> Option<crate::CursorPosition> {
        self.widget.cursor(area)
    }

    fn as_any(&self) -> &dyn Any {
        &self.widget
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.widget
    }

    fn into_widget(self: Box<Self>) -> Box<dyn Any> {
        Box::new(self.widget)
    }
}

struct MountedWidget<A> {
    id: FocusId,
    sequence: u64,
    options: MountOptions,
    widget: Box<dyn ErasedWidget<A>>,
}

pub(crate) struct WidgetRuntime<A> {
    entries: Vec<MountedWidget<A>>,
    focus: FocusHost,
    next_sequence: u64,
}

impl<A: 'static> Default for WidgetRuntime<A> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            focus: FocusHost::default(),
            next_sequence: 0,
        }
    }
}

impl<A: 'static> WidgetRuntime<A> {
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
        let id = self.focus.allocate();
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.entries.push(MountedWidget {
            id,
            sequence,
            options,
            widget: Box::new(MappedWidget { widget, map_action }),
        });
        if options.visible && options.focusable && options.focus_on_mount {
            self.focus_id(id);
        }
        WidgetHandle::new(id)
    }

    pub fn get<C: 'static>(&self, handle: WidgetHandle<C>) -> Option<&C> {
        self.entry(handle.id)?.widget.as_any().downcast_ref()
    }

    pub fn update<C: 'static, R>(
        &mut self,
        handle: WidgetHandle<C>,
        update: impl FnOnce(&mut C) -> R,
    ) -> Option<R> {
        let widget = self
            .entry_mut(handle.id)?
            .widget
            .as_any_mut()
            .downcast_mut()?;
        Some(update(widget))
    }

    pub fn remove<C: 'static>(&mut self, handle: WidgetHandle<C>) -> Option<C> {
        let index = self.index(handle.id)?;
        let entry = self.entries.remove(index);
        self.focus.remove(handle.id);
        entry
            .widget
            .into_widget()
            .downcast()
            .ok()
            .map(|value| *value)
    }

    pub fn is_visible<C: 'static>(&self, handle: WidgetHandle<C>) -> bool {
        self.entry(handle.id)
            .is_some_and(|entry| entry.options.visible)
    }

    pub fn set_input_context<C: 'static>(
        &mut self,
        handle: WidgetHandle<C>,
        context: InputContext,
    ) -> bool {
        let Some(entry) = self.entry_mut(handle.id) else {
            return false;
        };
        if !entry.widget.as_any().is::<C>() || entry.options.context == context {
            return false;
        }
        entry.options.context = context;
        true
    }

    pub fn set_visible<C: 'static>(&mut self, handle: WidgetHandle<C>, visible: bool) -> bool {
        let Some(index) = self.index(handle.id) else {
            return false;
        };
        if self.entries[index].options.visible == visible {
            return false;
        }
        let was_focused = self.focus.active() == handle.id;
        if !visible {
            self.focus.remove(handle.id);
        }
        self.entries[index].options.visible = visible;
        if was_focused {
            self.restore_active_focus();
        }
        true
    }

    pub fn focus<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        self.focus_id(handle.id)
    }

    pub fn unfocus<C: 'static>(&mut self, handle: WidgetHandle<C>) -> bool {
        if self.focus.active() != handle.id {
            return false;
        }
        self.restore_focus();
        true
    }

    pub fn restore_focus(&mut self) -> FocusId {
        self.restore_visible_focus();
        self.focus.active()
    }

    pub fn active_focus(&self) -> FocusId {
        self.focus.active()
    }

    pub fn input_context(&self) -> InputContext {
        self.entry(self.focus.active())
            .filter(|entry| entry.options.visible)
            .map_or(InputContext::Global, |entry| entry.options.context)
    }

    pub fn handle_input(&mut self, input: &KeyInput) -> InputOutcome<A> {
        let active = self.focus.active();
        let Some(entry) = self.entry_mut(active).filter(|entry| entry.options.visible) else {
            return InputOutcome::Ignored;
        };
        let capture_input = entry.options.capture_input;
        let outcome = entry.widget.handle_input(input);
        if capture_input && matches!(outcome, InputOutcome::Ignored) {
            InputOutcome::Consumed
        } else {
            outcome
        }
    }

    pub fn render(
        &self,
        area: RenderArea,
        theme: &Theme,
        inline_origin_row: usize,
    ) -> RenderedWidgets {
        let active = self.focus.active();
        let mut indices = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.options.visible)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        indices.sort_by_key(|index| {
            let entry = &self.entries[*index];
            (entry.options.order, entry.sequence)
        });

        let mut lines = Vec::new();
        let mut overlays = Vec::new();
        let mut cursor = None;
        for index in indices {
            let entry = &self.entries[index];
            let focused = entry.id == active;
            match entry.options.placement {
                MountPlacement::AfterRoot => {
                    let origin_row = inline_origin_row + lines.len();
                    let context = RenderContext::new(area, theme).focused(focused);
                    let rendered = entry.widget.render(&context);
                    if focused {
                        cursor = entry
                            .widget
                            .cursor(area)
                            .map(|cursor| translate_cursor(cursor, origin_row, 0));
                    }
                    lines.extend(rendered);
                }
                MountPlacement::InputOnly => {}
                MountPlacement::Overlay(options) => {
                    let height = area.height.unwrap_or(24);
                    let initial = resolve_overlay_frame(options, area.width, height, 0);
                    let overlay_area = RenderArea {
                        width: initial.width,
                        height: initial.max_height,
                    };
                    let context = RenderContext::new(overlay_area, theme).focused(focused);
                    let rendered = entry.widget.render(&context);
                    let frame = resolve_overlay_frame(options, area.width, height, rendered.len());
                    if focused {
                        cursor = entry
                            .widget
                            .cursor(overlay_area)
                            .filter(|cursor| {
                                overlay_cursor_is_visible(*cursor, frame, rendered.len())
                            })
                            .map(|cursor| translate_cursor(cursor, frame.row, frame.col));
                    }
                    overlays.push(OverlayLayer {
                        lines: rendered,
                        options,
                    });
                }
            }
        }
        RenderedWidgets {
            frame: crate::Frame {
                lines,
                cursor,
                logical_origin: 0,
            },
            overlays,
        }
    }

    fn focus_id(&mut self, id: FocusId) -> bool {
        let Some(entry) = self.entry(id) else {
            return false;
        };
        if !entry.options.visible || !entry.options.focusable || self.focus.active() == id {
            return false;
        }
        self.focus.focus(id);
        true
    }

    fn restore_visible_focus(&mut self) {
        self.focus.restore();
        self.restore_active_focus();
    }

    fn restore_active_focus(&mut self) {
        while self.focus.active() != FocusId::ROOT
            && !self
                .entry(self.focus.active())
                .is_some_and(|entry| entry.options.visible && entry.options.focusable)
        {
            self.focus.remove(self.focus.active());
        }
    }

    fn index(&self, id: FocusId) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    fn entry(&self, id: FocusId) -> Option<&MountedWidget<A>> {
        self.index(id).map(|index| &self.entries[index])
    }

    fn entry_mut(&mut self, id: FocusId) -> Option<&mut MountedWidget<A>> {
        let index = self.index(id)?;
        Some(&mut self.entries[index])
    }
}

fn overlay_cursor_is_visible(
    cursor: crate::CursorPosition,
    frame: crate::OverlayFrame,
    rendered_height: usize,
) -> bool {
    let visible_height = frame
        .max_height
        .unwrap_or(rendered_height)
        .min(rendered_height);
    cursor.row < visible_height && cursor.col < frame.width
}

fn translate_cursor(position: crate::CursorPosition, row: usize, col: u16) -> Cursor {
    Cursor {
        row: row.saturating_add(position.row),
        col: col.saturating_add(position.col),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CursorPosition, ThemeColor};

    struct TestWidget(&'static str);

    impl Widget for TestWidget {
        type Action = ();

        fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
            vec![StyledLine::new(self.0, ThemeColor::Default).fit(context.area.width)]
        }

        fn cursor(&self, _area: RenderArea) -> Option<CursorPosition> {
            Some(CursorPosition { row: 0, col: 0 })
        }
    }

    #[test]
    fn renders_inline_widgets_by_order_and_offsets_cursor_from_base() {
        let mut runtime = WidgetRuntime::<()>::default();
        let focused = runtime.mount(
            TestWidget("focused"),
            MountOptions::after_root(InputContext::Editor)
                .focused()
                .ordered(10),
            |_| (),
        );
        runtime.mount(
            TestWidget("first"),
            MountOptions::default().ordered(-10),
            |_| (),
        );
        runtime.focus(focused);

        let rendered = runtime.render(
            RenderArea {
                width: 20,
                height: Some(10),
            },
            &Theme::default(),
            5,
        );

        assert_eq!(
            rendered
                .frame
                .lines
                .iter()
                .map(|line| line.text.trim())
                .collect::<Vec<_>>(),
            vec!["first", "focused"]
        );
        assert_eq!(rendered.frame.cursor, Some(Cursor { row: 6, col: 0 }));
    }

    #[test]
    fn suppresses_cursor_outside_clipped_overlay() {
        struct TallOverlay;

        impl Widget for TallOverlay {
            type Action = ();

            fn render(&self, _context: &RenderContext<'_>) -> Vec<StyledLine> {
                (0..5)
                    .map(|row| StyledLine::new(row.to_string(), ThemeColor::Default))
                    .collect()
            }

            fn cursor(&self, _area: RenderArea) -> Option<CursorPosition> {
                Some(CursorPosition { row: 4, col: 0 })
            }
        }

        let mut runtime = WidgetRuntime::<()>::default();
        runtime.mount(
            TallOverlay,
            MountOptions::overlay(
                InputContext::Overlay,
                crate::OverlayOptions {
                    max_height: Some(crate::OverlaySize::Cells(2)),
                    ..crate::OverlayOptions::default()
                },
            ),
            |_| (),
        );

        let rendered = runtime.render(
            RenderArea {
                width: 40,
                height: Some(10),
            },
            &Theme::default(),
            0,
        );

        assert_eq!(rendered.frame.cursor, None);
    }

    #[test]
    fn renders_overlay_separately_from_inline_frame() {
        let mut runtime = WidgetRuntime::<()>::default();
        runtime.mount(
            TestWidget("overlay"),
            MountOptions::overlay(
                InputContext::Overlay,
                crate::OverlayOptions::centered_panel(),
            ),
            |_| (),
        );

        let rendered = runtime.render(
            RenderArea {
                width: 80,
                height: Some(24),
            },
            &Theme::default(),
            0,
        );

        assert!(rendered.frame.lines.is_empty());
        assert_eq!(rendered.overlays.len(), 1);
        assert_eq!(rendered.overlays[0].lines[0].text.trim(), "overlay");
        assert!(rendered.frame.cursor.is_some());
    }
}
