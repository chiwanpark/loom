use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, AppContext, Context, FontWeight, HighlightStyle, ParentElement, SharedString, Styled,
    StyledText, Task, Window, div, px,
};
use gpui_component::list::{List, ListDelegate, ListItem, ListState};
use gpui_component::{ActiveTheme, IndexPath, WindowExt, h_flex};

use crate::fuzzy;

pub type Confirm = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone)]
pub struct PickerItem {
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub hint: Option<SharedString>,
    pub confirm: Confirm,
}

impl PickerItem {
    pub fn new(
        label: impl Into<SharedString>,
        confirm: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            detail: None,
            hint: None,
            confirm: Rc::new(confirm),
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint.map(Into::into);
        self
    }
}

pub struct PickerDelegate {
    items: Vec<PickerItem>,
    matches: Vec<fuzzy::FuzzyMatch>,
    selected: Option<IndexPath>,
}

impl PickerDelegate {
    fn new(items: Vec<PickerItem>) -> Self {
        let mut this = Self {
            items,
            matches: Vec::new(),
            selected: None,
        };
        this.search("");
        this
    }

    fn search(&mut self, query: &str) {
        let candidates: Vec<String> = self
            .items
            .iter()
            .map(|item| match &item.detail {
                Some(detail) => format!("{} {detail}", item.label),
                None => item.label.to_string(),
            })
            .collect();
        self.matches = fuzzy::filter(query, candidates.iter().map(String::as_str));
        self.selected = (!self.matches.is_empty()).then(|| IndexPath::new(0));
    }
}

fn label_highlights(
    label: &str,
    positions: &[usize],
    color: gpui::Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    positions
        .iter()
        .filter(|&&pos| pos < label.len())
        .filter_map(|&pos| {
            let len = label[pos..].chars().next()?.len_utf8();
            Some((
                pos..pos + len,
                HighlightStyle {
                    color: Some(color),
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                },
            ))
        })
        .collect()
}

impl ListDelegate for PickerDelegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.search(query);
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matches.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let theme = cx.theme();
        let found = self.matches.get(ix.row)?;
        let item = self.items.get(found.index)?;
        let label = item.label.clone();
        let highlights = label_highlights(&label, &found.positions, theme.primary);
        let selected = self.selected == Some(ix);
        Some(
            ListItem::new(("picker-item", ix.row))
                .selected(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            div()
                                .flex_none()
                                .child(StyledText::new(label).with_highlights(highlights)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(item.detail.clone().unwrap_or_default()),
                        )
                        .children(item.hint.clone().map(|hint| {
                            div()
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .text_xs()
                                .bg(theme.muted)
                                .text_color(theme.muted_foreground)
                                .child(hint)
                        })),
                ),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self
            .selected
            .and_then(|ix| self.matches.get(ix.row))
            .and_then(|found| self.items.get(found.index))
        else {
            return;
        };
        let confirm = item.confirm.clone();
        window.close_dialog(cx);
        confirm(window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        window.close_dialog(cx);
    }
}

pub fn open_picker(
    title: &str,
    placeholder: &str,
    items: Vec<PickerItem>,
    window: &mut Window,
    cx: &mut App,
) {
    let state =
        cx.new(|cx| ListState::new(PickerDelegate::new(items), window, cx).searchable(true));
    let title = SharedString::from(title.to_owned());
    let placeholder = SharedString::from(placeholder.to_owned());
    let list_state = state.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(px(640.)).child(
            div().h(px(420.)).child(
                List::new(&list_state)
                    .search_placeholder(placeholder.clone())
                    .scrollbar_visible(true),
            ),
        )
    });
    window.defer(cx, move |window, cx| {
        state.update(cx, |state, cx| {
            if state.delegate().items_count(0, cx) > 0 {
                state.set_selected_index(Some(IndexPath::new(0)), window, cx);
            }
            state.focus(window, cx);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_whole_characters() {
        let color = gpui::Hsla::default();
        let ranges: Vec<_> = label_highlights("a세b", &[0, 1, 9], color)
            .into_iter()
            .map(|(range, _)| range)
            .collect();
        assert_eq!(ranges, vec![0..1, 1..4]);
    }

    #[test]
    fn filters_on_label_and_detail() {
        let mut delegate = PickerDelegate::new(vec![
            PickerItem::new("alpha", |_, _| {}).detail("local"),
            PickerItem::new("beta", |_, _| {}).detail("ssh"),
        ]);
        assert_eq!(delegate.matches.len(), 2);
        delegate.search("ssh");
        assert_eq!(delegate.matches.len(), 1);
        assert_eq!(delegate.matches[0].index, 1);
        assert_eq!(delegate.selected, Some(IndexPath::new(0)));
        delegate.search("zzz");
        assert!(delegate.selected.is_none());
    }
}
