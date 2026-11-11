//! The session tab groups and pane layouts, saved across restarts.
//!
//! The file names each group by host, workspace, and session rather than by
//! agent instance, because a restarted agent gets a new instance ID. A group
//! is restored once its agent is connected and has listed its sessions;
//! until then it stays in the file as it was.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ui::panes::{Axis, Member, PaneAxis, PaneId};

pub const FILE_NAME: &str = "layout.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedLayout {
    pub version: u32,
    pub groups: Vec<SavedGroup>,
    /// The group that was shown.
    pub active: Option<GroupKey>,
}

impl Default for SavedLayout {
    fn default() -> Self {
        Self {
            version: VERSION,
            groups: Vec::new(),
            active: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupKey {
    pub host: String,
    pub workspace: PathBuf,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedGroup {
    #[serde(flatten)]
    pub key: GroupKey,
    pub root: SavedMember,
    /// The active pane, as an index into the panes in tree order.
    pub active_pane: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedMember {
    Pane(SavedPane),
    Split {
        axis: SavedAxis,
        members: Vec<SavedMember>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPane {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
}

impl SavedPane {
    /// A pane of the tabs worth saving (`None` entries are left out), or
    /// `None` when there are none. The active tab stays active, or the
    /// nearest kept tab before it when it is left out.
    pub fn keep(tabs: impl IntoIterator<Item = Option<SavedTab>>, active: usize) -> Option<Self> {
        let mut kept = Vec::new();
        let mut kept_active = 0;
        for (ix, tab) in tabs.into_iter().enumerate() {
            if let Some(tab) = tab {
                if ix <= active {
                    kept_active = kept.len();
                }
                kept.push(tab);
            }
        }
        (!kept.is_empty()).then_some(Self {
            tabs: kept,
            active: kept_active,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedTab {
    Conversation,
    /// A terminal is reattached when its agent instance still runs it, and
    /// replaced by a new one otherwise.
    Terminal {
        instance: String,
        id: u64,
    },
    /// A diff against `HEAD`, by path relative to the workspace.
    GitDiff {
        path: String,
        old_path: Option<String>,
    },
    /// The changes the session made to a file, by absolute path.
    SessionDiff {
        path: PathBuf,
    },
    /// A read-only file, by path relative to the workspace.
    File {
        path: String,
    },
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// Reads the saved layout. A missing, unreadable, or newer file gives an
/// empty layout, so a bad file never keeps the GUI from starting.
pub fn load(path: &Path) -> SavedLayout {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return SavedLayout::default(),
        Err(err) => {
            tracing::warn!("cannot read {}: {err}", path.display());
            return SavedLayout::default();
        }
    };
    match serde_json::from_str::<SavedLayout>(&text) {
        Ok(layout) if layout.version == VERSION => layout,
        Ok(layout) => {
            tracing::warn!(
                "ignoring {}: unknown version {}",
                path.display(),
                layout.version
            );
            SavedLayout::default()
        }
        Err(err) => {
            tracing::warn!("ignoring {}: {err}", path.display());
            SavedLayout::default()
        }
    }
}

/// Writes the layout through a temporary file, so a crash never leaves a
/// half-written file behind.
pub fn save(path: &Path, layout: &SavedLayout) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(layout).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Converts a pane tree for saving. `pane` describes each pane; panes it
/// returns `None` for (with nothing worth saving) are left out, and splits
/// left with one member collapse into it.
pub fn save_member(
    member: &Member,
    pane: &mut impl FnMut(PaneId) -> Option<SavedPane>,
) -> Option<SavedMember> {
    match member {
        Member::Pane(id) => pane(*id).map(SavedMember::Pane),
        Member::Axis(axis) => {
            let mut members: Vec<SavedMember> = axis
                .members
                .iter()
                .filter_map(|m| save_member(m, pane))
                .collect();
            match members.len() {
                0 => None,
                1 => members.pop(),
                _ => Some(SavedMember::Split {
                    axis: match axis.axis {
                        Axis::Horizontal => SavedAxis::Horizontal,
                        Axis::Vertical => SavedAxis::Vertical,
                    },
                    members,
                }),
            }
        }
    }
}

/// Rebuilds a saved pane tree with new pane IDs from `alloc`, returning the
/// tree and its panes in tree order. Panes without tabs are dropped, and
/// splits left with one member collapse into it.
pub fn restore_member<'a>(
    saved: &'a SavedMember,
    alloc: &mut impl FnMut() -> PaneId,
    panes: &mut Vec<(PaneId, &'a SavedPane)>,
) -> Option<Member> {
    match saved {
        SavedMember::Pane(pane) if pane.tabs.is_empty() => None,
        SavedMember::Pane(pane) => {
            let id = alloc();
            panes.push((id, pane));
            Some(Member::Pane(id))
        }
        SavedMember::Split { axis, members } => {
            let mut members: Vec<Member> = members
                .iter()
                .filter_map(|m| restore_member(m, alloc, panes))
                .collect();
            match members.len() {
                0 => None,
                1 => members.pop(),
                _ => Some(Member::Axis(PaneAxis {
                    id: 0,
                    axis: match axis {
                        SavedAxis::Horizontal => Axis::Horizontal,
                        SavedAxis::Vertical => Axis::Vertical,
                    },
                    members,
                })),
            }
        }
    }
}

/// The panes of a saved tree in tree order.
pub fn saved_panes(member: &SavedMember) -> Vec<&SavedPane> {
    fn walk<'a>(member: &'a SavedMember, out: &mut Vec<&'a SavedPane>) {
        match member {
            SavedMember::Pane(pane) => out.push(pane),
            SavedMember::Split { members, .. } => {
                for m in members {
                    walk(m, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(member, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::panes::{PaneGroup, SplitDirection};

    fn pane(tabs: Vec<SavedTab>, active: usize) -> SavedPane {
        SavedPane { tabs, active }
    }

    #[test]
    fn a_split_layout_round_trips() {
        let mut group = PaneGroup::new(1);
        group.split(1, 2, SplitDirection::Right);
        group.split(2, 3, SplitDirection::Down);
        let tabs = |id: PaneId| match id {
            1 => Some(pane(vec![SavedTab::Conversation], 0)),
            2 => Some(pane(
                vec![
                    SavedTab::Terminal {
                        instance: "i".into(),
                        id: 4,
                    },
                    SavedTab::File {
                        path: "src/main.rs".into(),
                    },
                ],
                1,
            )),
            3 => Some(pane(
                vec![SavedTab::GitDiff {
                    path: "b".into(),
                    old_path: Some("a".into()),
                }],
                0,
            )),
            _ => None,
        };
        let saved = save_member(&group.root, &mut { tabs }).unwrap();
        let text = serde_json::to_string(&saved).unwrap();
        let parsed: SavedMember = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, saved);

        let mut next = 10;
        let mut panes = Vec::new();
        let root = restore_member(
            &parsed,
            &mut || {
                next += 1;
                next
            },
            &mut panes,
        )
        .unwrap();
        let restored = PaneGroup::from_root(root);
        assert_eq!(restored.panes(), [11, 12, 13]);
        let shape = |g: &PaneGroup| {
            g.rects()
                .into_iter()
                .map(|(_, r)| (r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&restored), shape(&group));
        let ids: Vec<_> = panes.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [11, 12, 13]);
        assert_eq!(panes[1].1.active, 1);
        assert_eq!(saved_panes(&parsed).len(), 3);
    }

    #[test]
    fn keeps_the_active_tab_when_others_are_left_out() {
        let file = |p: &str| Some(SavedTab::File { path: p.into() });
        let paths = |pane: &SavedPane| -> Vec<String> {
            pane.tabs
                .iter()
                .map(|t| match t {
                    SavedTab::File { path } => path.clone(),
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        };
        // An ended terminal before the active tab shifts its index.
        let pane = SavedPane::keep([file("a"), None, file("c"), file("d")], 2).unwrap();
        assert_eq!(paths(&pane), ["a", "c", "d"]);
        assert_eq!(pane.active, 1);
        // A left-out active tab gives way to the tab before it.
        let pane = SavedPane::keep([file("a"), file("b"), None], 2).unwrap();
        assert_eq!(pane.active, 1);
        let pane = SavedPane::keep([None, file("b")], 0).unwrap();
        assert_eq!(pane.active, 0);
        assert_eq!(SavedPane::keep([None, None], 1), None);
    }

    #[test]
    fn empty_panes_are_dropped_and_splits_collapse() {
        let mut group = PaneGroup::new(1);
        group.split(1, 2, SplitDirection::Right);
        group.split(2, 3, SplitDirection::Down);
        // Pane 3 has nothing to save, so the vertical split collapses.
        let saved = save_member(&group.root, &mut |id| {
            (id != 3).then(|| pane(vec![SavedTab::Conversation], 0))
        })
        .unwrap();
        let SavedMember::Split { axis, members } = &saved else {
            panic!("expected a split: {saved:?}");
        };
        assert_eq!(*axis, SavedAxis::Horizontal);
        assert!(members.iter().all(|m| matches!(m, SavedMember::Pane(_))));

        let saved = SavedMember::Split {
            axis: SavedAxis::Vertical,
            members: vec![
                SavedMember::Pane(pane(Vec::new(), 0)),
                SavedMember::Pane(pane(vec![SavedTab::Conversation], 0)),
            ],
        };
        let mut panes = Vec::new();
        let root = restore_member(&saved, &mut || 7, &mut panes).unwrap();
        assert_eq!(root, Member::Pane(7));
        assert_eq!(panes.len(), 1);
    }

    #[test]
    fn a_missing_or_bad_file_gives_an_empty_layout() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path());
        assert_eq!(load(&file), SavedLayout::default());
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(load(&file), SavedLayout::default());
        std::fs::write(&file, r#"{"version": 99, "groups": [], "active": null}"#).unwrap();
        assert_eq!(load(&file), SavedLayout::default());

        let layout = SavedLayout {
            version: VERSION,
            groups: vec![SavedGroup {
                key: GroupKey {
                    host: "local".into(),
                    workspace: "/w".into(),
                    session: "s1".into(),
                },
                root: SavedMember::Pane(pane(
                    vec![SavedTab::SessionDiff {
                        path: "/w/a.rs".into(),
                    }],
                    0,
                )),
                active_pane: 0,
            }],
            active: None,
        };
        let nested = dir.path().join("state/loom").join(FILE_NAME);
        save(&nested, &layout).unwrap();
        assert_eq!(load(&nested), layout);
        assert!(!nested.with_extension("json.tmp").exists());
    }
}
