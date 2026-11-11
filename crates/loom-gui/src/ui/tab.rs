use std::cmp::Ordering;

use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, InteractiveElement, IntoElement, ParentElement,
    Pixels, RenderOnce, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{ActiveTheme, h_flex};

const START_SLOT_SIZE: Pixels = px(14.);
const END_SLOT_SIZE: Pixels = px(18.);
const CONTAINER_HEIGHT: Pixels = px(32.);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabPosition {
    First,
    Middle(Ordering),
    Last,
}

#[derive(IntoElement)]
pub struct Tab {
    div: Stateful<Div>,
    selected: bool,
    position: TabPosition,
    start_slot: Option<AnyElement>,
    end_slot: Option<AnyElement>,
    children: Vec<AnyElement>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            div: div().id(id.into()),
            selected: false,
            position: TabPosition::First,
            start_slot: None,
            end_slot: None,
            children: Vec::new(),
        }
    }

    pub fn position(mut self, position: TabPosition) -> Self {
        self.position = position;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn start_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.start_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    pub fn end_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.end_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    /// Calls `listener` when the middle button is pressed and released on
    /// the tab, as browsers close tabs.
    pub fn on_middle_click(
        self,
        listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_aux_click(move |event, window, cx| {
            if event.is_middle_click() {
                cx.stop_propagation();
                listener(event, window, cx);
            }
        })
    }

    pub fn container_height() -> Pixels {
        CONTAINER_HEIGHT
    }

    pub fn position_for(ix: usize, len: usize, active: usize) -> TabPosition {
        if ix == 0 {
            TabPosition::First
        } else if ix + 1 == len {
            TabPosition::Last
        } else {
            TabPosition::Middle(ix.cmp(&active))
        }
    }
}

impl InteractiveElement for Tab {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl StatefulInteractiveElement for Tab {}

impl ParentElement for Tab {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for Tab {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (text_color, bg) = if self.selected {
            (theme.tab_active_foreground, theme.tab_active)
        } else {
            (theme.tab_foreground, theme.tab)
        };
        let start_slot = h_flex()
            .size(START_SLOT_SIZE)
            .justify_center()
            .children(self.start_slot);
        let end_slot = h_flex()
            .size(END_SLOT_SIZE)
            .justify_center()
            .children(self.end_slot);
        self.div
            .h(CONTAINER_HEIGHT)
            .flex_none()
            .bg(bg)
            .border_color(theme.border)
            .map(|this| match self.position {
                TabPosition::First => {
                    if self.selected {
                        this.pl_px().border_r_1().pb_px()
                    } else {
                        this.pl_px().pr_px().border_b_1()
                    }
                }
                TabPosition::Last => {
                    if self.selected {
                        this.border_l_1().border_r_1().pb_px()
                    } else {
                        this.pl_px().border_b_1().border_r_1()
                    }
                }
                TabPosition::Middle(Ordering::Equal) => this.border_l_1().border_r_1().pb_px(),
                TabPosition::Middle(Ordering::Less) => this.border_l_1().pr_px().border_b_1(),
                TabPosition::Middle(Ordering::Greater) => this.border_r_1().pl_px().border_b_1(),
            })
            .cursor_pointer()
            .child(
                h_flex()
                    .group("")
                    .relative()
                    .h(CONTAINER_HEIGHT - px(1.))
                    .px_2()
                    .gap_1p5()
                    .text_sm()
                    .text_color(text_color)
                    .child(start_slot)
                    .children(self.children)
                    .child(end_slot),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, Modifiers, MouseButton, Render, TestAppContext, VisualTestContext, Window,
    };

    use super::*;

    /// Two tabs that record which of them was middle-clicked.
    struct Tabs {
        closed: Vec<usize>,
    }

    impl Render for Tabs {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            gpui_component::h_flex().children((0..2).map(|ix| {
                Tab::new(("tab", ix))
                    .debug_selector(move || format!("tab-{ix}"))
                    .on_middle_click(cx.listener(move |this, _, _, _| this.closed.push(ix)))
                    .child(format!("tab {ix}"))
            }))
        }
    }

    fn click(
        cx: &mut VisualTestContext,
        down: &'static str,
        up: &'static str,
        button: MouseButton,
    ) {
        let down = cx.debug_bounds(down).unwrap().center();
        let up = cx.debug_bounds(up).unwrap().center();
        cx.simulate_mouse_down(down, button, Modifiers::default());
        cx.simulate_mouse_move(up, button, Modifiers::default());
        cx.simulate_mouse_up(up, button, Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_middle_click_on_a_tab_calls_its_listener(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_, _| Tabs { closed: Vec::new() });
        cx.run_until_parked();
        let closed = |cx: &mut VisualTestContext| view.read_with(cx, |v, _| v.closed.clone());

        click(cx, "tab-1", "tab-1", MouseButton::Middle);
        assert_eq!(closed(cx), [1]);

        // Other buttons do nothing.
        click(cx, "tab-0", "tab-0", MouseButton::Left);
        click(cx, "tab-0", "tab-0", MouseButton::Right);
        assert_eq!(closed(cx), [1]);

        // Pressed on one tab and released on another, it is no click at all.
        click(cx, "tab-0", "tab-1", MouseButton::Middle);
        assert_eq!(closed(cx), [1]);

        click(cx, "tab-0", "tab-0", MouseButton::Middle);
        assert_eq!(closed(cx), [1, 0]);
    }
}
