use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder, px, uniform_list,
};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::proto::{ChangeStatus, DirEntry, EntryKind};

use crate::state::AgentConnection;

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 14.;

#[derive(Default)]
struct DirState {
    entries: Option<Rc<Vec<DirEntry>>>,
    error: Option<String>,
    truncated: bool,
    loading: bool,
}

#[derive(Clone)]
struct Row {
    path: String,
    name: String,
    depth: usize,
    kind: EntryKind,
    ignored: bool,
    expanded: bool,
    loading: bool,
    note: Option<String>,
}

pub enum FileTreeEvent {
    Open(String),
}

#[derive(Default)]
struct GitMarks {
    revision: Option<u64>,
    files: HashMap<String, ChangeStatus>,
    dirs: HashSet<String>,
}

pub struct FileTreeView {
    connection: Entity<AgentConnection>,
    dirs: HashMap<String, DirState>,
    expanded: HashSet<String>,
    selected: Option<String>,
    marks: GitMarks,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    _subscription: Subscription,
}

impl EventEmitter<FileTreeEvent> for FileTreeView {}

pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn status_color(status: ChangeStatus, theme: &gpui_component::Theme) -> Hsla {
    match status {
        ChangeStatus::Added | ChangeStatus::Untracked => theme.green,
        ChangeStatus::Conflicted | ChangeStatus::Deleted => theme.red,
        ChangeStatus::Renamed | ChangeStatus::Copied => theme.blue,
        ChangeStatus::Modified | ChangeStatus::TypeChanged => theme.yellow,
    }
}

impl FileTreeView {
    pub fn new(connection: Entity<AgentConnection>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&connection, |this, conn, cx| {
            let revision = conn.read(cx).git.revision;
            if this.marks.revision != Some(revision) {
                this.refresh(cx);
                cx.notify();
            }
        });
        let mut this = Self {
            connection,
            dirs: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            marks: GitMarks::default(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            _subscription: subscription,
        };
        this.load(String::new(), cx);
        this
    }

    pub fn connection(&self) -> &Entity<AgentConnection> {
        &self.connection
    }

    pub fn select(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.selected != path {
            self.selected = path;
            cx.notify();
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let mut paths: Vec<String> = vec![String::new()];
        paths.extend(self.expanded.iter().cloned());
        for path in paths {
            self.load(path, cx);
        }
    }

    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        cx.notify();
    }

    fn load(&mut self, path: String, cx: &mut Context<Self>) {
        let state = self.dirs.entry(path.clone()).or_default();
        if state.loading {
            return;
        }
        state.loading = true;
        let task = self.connection.read(cx).list_directory(path.clone());
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = this.dirs.entry(path).or_default();
                state.loading = false;
                match result {
                    Some(Ok(listing)) => {
                        let changed = state.entries.as_deref() != Some(&listing.entries)
                            || state.error.is_some()
                            || state.truncated != listing.truncated;
                        if changed {
                            state.entries = Some(Rc::new(listing.entries));
                            state.truncated = listing.truncated;
                            state.error = None;
                        }
                    }
                    Some(Err(err)) => state.error = Some(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle(&mut self, path: String, cx: &mut Context<Self>) {
        if self.expanded.remove(&path) {
            let prefix = format!("{path}/");
            self.expanded.retain(|p| !p.starts_with(&prefix));
        } else {
            self.expanded.insert(path.clone());
            self.load(path, cx);
        }
        cx.notify();
    }

    fn update_marks(&mut self, cx: &App) {
        let git = &self.connection.read(cx).git;
        if self.marks.revision == Some(git.revision) {
            return;
        }
        let mut marks = GitMarks {
            revision: Some(git.revision),
            ..GitMarks::default()
        };
        if let Some(changes) = git.changes.as_ref().filter(|c| c.repository) {
            for file in &changes.files {
                marks.files.insert(file.path.clone(), file.status);
                let mut dir = file.path.as_str();
                while let Some((parent, _)) = dir.rsplit_once('/') {
                    if !marks.dirs.insert(parent.to_string()) {
                        break;
                    }
                    dir = parent;
                }
            }
        }
        self.marks = marks;
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.push_rows("", 0, &mut rows);
        rows
    }

    fn push_rows(&self, dir: &str, depth: usize, rows: &mut Vec<Row>) {
        let Some(state) = self.dirs.get(dir) else {
            return;
        };
        let note = |text: String| Row {
            path: format!("{dir}\u{0}note"),
            name: String::new(),
            depth,
            kind: EntryKind::Other,
            ignored: true,
            expanded: false,
            loading: false,
            note: Some(text),
        };
        if let Some(error) = &state.error {
            rows.push(note(error.clone()));
        }
        let Some(entries) = &state.entries else {
            return;
        };
        for entry in entries.iter() {
            let path = join(dir, &entry.name);
            let expanded = entry.kind == EntryKind::Directory && self.expanded.contains(&path);
            let loading = self.dirs.get(&path).is_some_and(|d| d.loading);
            rows.push(Row {
                path: path.clone(),
                name: entry.name.clone(),
                depth,
                kind: entry.kind,
                ignored: entry.ignored,
                expanded,
                loading,
                note: None,
            });
            if expanded {
                self.push_rows(&path, depth + 1, rows);
            }
        }
        if state.truncated {
            rows.push(note("Too many entries; the list is truncated.".into()));
        }
    }

    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let indent = px(8. + INDENT * row.depth as f32);
        if let Some(note) = row.note {
            return div()
                .id(SharedString::from(row.path))
                .h(px(ROW_HEIGHT))
                .pl(indent + px(16.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(note)
                .into_any_element();
        }
        let is_dir = row.kind == EntryKind::Directory;
        let status = self.marks.files.get(&row.path).copied();
        let color = match status {
            Some(status) => status_color(status, &theme),
            None if is_dir && self.marks.dirs.contains(&row.path) => theme.yellow,
            None if row.ignored => theme.muted_foreground,
            None => theme.foreground,
        };
        let selected = self.selected.as_deref() == Some(row.path.as_str());
        let icon = if is_dir {
            if row.expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            }
        } else {
            IconName::File
        };
        let path = row.path.clone();
        h_flex()
            .id(SharedString::from(row.path.clone()))
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(indent)
            .pr_2()
            .gap_1()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.list_active))
            .hover(|el| el.bg(theme.list_hover))
            .child(div().w(px(12.)).flex_none().when(is_dir, |el| {
                el.child(
                    Icon::new(if row.loading {
                        IconName::LoaderCircle
                    } else if row.expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                )
            }))
            .child(Icon::new(icon).xsmall().text_color(if is_dir {
                theme.blue
            } else {
                theme.muted_foreground
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_sm()
                    .text_color(color)
                    .child(row.name),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                if is_dir {
                    this.selected = Some(path.clone());
                    this.toggle(path.clone(), cx);
                } else {
                    this.selected = Some(path.clone());
                    cx.emit(FileTreeEvent::Open(path.clone()));
                    cx.notify();
                }
            }))
            .into_any_element()
    }
}

impl Focusable for FileTreeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FileTreeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_marks(cx);
        let theme = cx.theme().clone();
        let rows = Rc::new(self.rows());
        let count = rows.len();
        let root_loading = self
            .dirs
            .get("")
            .is_none_or(|d| d.entries.is_none() && d.error.is_none());
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .when(root_loading, |el| {
                el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Loading files…"),
                )
            })
            .when(!root_loading && count == 0, |el| {
                el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("The workspace is empty."),
                )
            })
            .child(
                uniform_list(
                    "file-tree",
                    count,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .filter_map(|ix| rows.get(ix).cloned())
                            .map(|row| this.render_row(row, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .w_full()
                .py_1()
                .track_scroll(&self.scroll),
            )
    }
}
