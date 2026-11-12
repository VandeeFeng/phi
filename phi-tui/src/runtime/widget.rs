use std::marker::PhantomData;

use crate::{FocusId, InputContext, KeyInput, OverlayOptions, StyledLine, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountPlacement {
    AfterRoot,
    InputOnly,
    Overlay(OverlayOptions),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountOptions {
    pub(crate) context: InputContext,
    pub(crate) placement: MountPlacement,
    pub(crate) visible: bool,
    pub(crate) focusable: bool,
    pub(crate) focus_on_mount: bool,
    pub(crate) capture_input: bool,
    pub(crate) order: i32,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            context: InputContext::Global,
            placement: MountPlacement::AfterRoot,
            visible: true,
            focusable: false,
            focus_on_mount: false,
            capture_input: false,
            order: 0,
        }
    }
}

impl MountOptions {
    pub fn after_root(context: InputContext) -> Self {
        Self {
            context,
            ..Self::default()
        }
    }

    pub fn input(context: InputContext) -> Self {
        Self {
            context,
            placement: MountPlacement::InputOnly,
            focusable: true,
            ..Self::default()
        }
    }

    pub fn overlay(context: InputContext, options: OverlayOptions) -> Self {
        Self {
            context,
            placement: MountPlacement::Overlay(options),
            focusable: true,
            focus_on_mount: true,
            capture_input: true,
            ..Self::default()
        }
    }

    pub fn hidden(mut self) -> Self {
        self.visible = false;
        self.focus_on_mount = false;
        self
    }

    pub fn focusable(mut self) -> Self {
        self.focusable = true;
        self
    }

    pub fn focused(mut self) -> Self {
        self.focusable = true;
        self.focus_on_mount = true;
        self
    }

    pub fn capture_input(mut self) -> Self {
        self.focusable = true;
        self.capture_input = true;
        self
    }

    pub fn ordered(mut self, order: i32) -> Self {
        self.order = order;
        self
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct WidgetHandle<C> {
    pub(crate) id: FocusId,
    marker: PhantomData<fn() -> C>,
}

impl<C> WidgetHandle<C> {
    pub(crate) fn new(id: FocusId) -> Self {
        Self {
            id,
            marker: PhantomData,
        }
    }

    pub fn id(self) -> FocusId {
        self.id
    }
}

impl<C> Clone for WidgetHandle<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for WidgetHandle<C> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderArea {
    pub width: u16,
    pub height: Option<usize>,
}

impl RenderArea {
    pub fn width(width: u16) -> Self {
        Self {
            width,
            height: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    pub area: RenderArea,
    pub theme: &'a Theme,
    pub focused: bool,
}

impl<'a> RenderContext<'a> {
    pub fn new(area: RenderArea, theme: &'a Theme) -> Self {
        Self {
            area,
            theme,
            focused: false,
        }
    }

    pub fn width(width: u16, theme: &'a Theme) -> Self {
        Self::new(RenderArea::width(width), theme)
    }

    pub fn with_area(self, area: RenderArea) -> Self {
        Self { area, ..self }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub row: usize,
    pub col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome<A> {
    Ignored,
    Consumed,
    Action(A),
}

impl<A> InputOutcome<A> {
    pub fn is_consumed(&self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

pub trait Widget {
    type Action;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine>;

    fn handle_input(&mut self, _input: &KeyInput) -> InputOutcome<Self::Action> {
        InputOutcome::Ignored
    }

    fn cursor(&self, _area: RenderArea) -> Option<CursorPosition> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_area_preserves_parent_context() {
        let theme = Theme::default();
        let context = RenderContext::new(
            RenderArea {
                width: 80,
                height: Some(24),
            },
            &theme,
        )
        .focused(true)
        .with_area(RenderArea {
            width: 40,
            height: Some(10),
        });

        assert_eq!(context.area.width, 40);
        assert_eq!(context.area.height, Some(10));
        assert!(context.focused);
        assert!(std::ptr::eq(context.theme, &theme));
    }
}
