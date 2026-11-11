pub type PaneId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    Up,
    Down,
    Left,
    Right,
}

impl SplitDirection {
    pub fn axis(self) -> Axis {
        match self {
            SplitDirection::Up | SplitDirection::Down => Axis::Vertical,
            SplitDirection::Left | SplitDirection::Right => Axis::Horizontal,
        }
    }

    pub fn increasing(self) -> bool {
        matches!(self, SplitDirection::Down | SplitDirection::Right)
    }

    pub fn from_drop(x: f32, y: f32, width: f32, height: f32, proportion: f32) -> Option<Self> {
        let size = width.min(height) * proportion;
        if x < size || x > width - size || y < size || y > height - size {
            [
                (SplitDirection::Up, y),
                (SplitDirection::Right, width - x),
                (SplitDirection::Down, height - y),
                (SplitDirection::Left, x),
            ]
            .into_iter()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(direction, _)| direction)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    Pane(PaneId),
    Axis(PaneAxis),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneAxis {
    pub id: u64,
    pub axis: Axis,
    pub members: Vec<Member>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneGroup {
    pub root: Member,
    next_axis: u64,
}

impl PaneGroup {
    pub fn new(pane: PaneId) -> Self {
        Self {
            root: Member::Pane(pane),
            next_axis: 0,
        }
    }

    /// A group with an existing tree (a restored layout). Its splits get
    /// fresh IDs, which only need to be unique within the group.
    pub fn from_root(mut root: Member) -> Self {
        fn number(member: &mut Member, next: &mut u64) {
            if let Member::Axis(axis) = member {
                *next += 1;
                axis.id = *next;
                for child in &mut axis.members {
                    number(child, next);
                }
            }
        }
        let mut next_axis = 0;
        number(&mut root, &mut next_axis);
        Self { root, next_axis }
    }

    fn new_axis(&mut self, old: PaneId, new: PaneId, direction: SplitDirection) -> Member {
        self.next_axis += 1;
        let members = if direction.increasing() {
            vec![Member::Pane(old), Member::Pane(new)]
        } else {
            vec![Member::Pane(new), Member::Pane(old)]
        };
        Member::Axis(PaneAxis {
            id: self.next_axis,
            axis: direction.axis(),
            members,
        })
    }

    pub fn split(&mut self, old: PaneId, new: PaneId, direction: SplitDirection) {
        let target = if self.contains(old) {
            old
        } else {
            self.first_pane()
        };
        let mut root = std::mem::replace(&mut self.root, Member::Pane(target));
        self.split_member(&mut root, target, new, direction);
        self.root = root;
    }

    fn split_member(
        &mut self,
        member: &mut Member,
        old: PaneId,
        new: PaneId,
        direction: SplitDirection,
    ) -> bool {
        match member {
            Member::Pane(pane) => {
                if *pane == old {
                    *member = self.new_axis(old, new, direction);
                    true
                } else {
                    false
                }
            }
            Member::Axis(axis) => {
                for idx in 0..axis.members.len() {
                    match &mut axis.members[idx] {
                        Member::Pane(pane) if *pane == old => {
                            if direction.axis() == axis.axis {
                                let at = if direction.increasing() { idx + 1 } else { idx };
                                axis.members.insert(at, Member::Pane(new));
                            } else {
                                axis.members[idx] = self.new_axis(old, new, direction);
                            }
                            return true;
                        }
                        Member::Pane(_) => {}
                        child @ Member::Axis(_) => {
                            if self.split_member(child, old, new, direction) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
        }
    }

    pub fn remove(&mut self, pane: PaneId) -> bool {
        match &mut self.root {
            Member::Pane(_) => false,
            Member::Axis(axis) => match remove_from_axis(axis, pane) {
                Some(Some(last)) => {
                    self.root = last;
                    true
                }
                Some(None) => true,
                None => false,
            },
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes().contains(&pane)
    }

    pub fn first_pane(&self) -> PaneId {
        let mut member = &self.root;
        loop {
            match member {
                Member::Pane(pane) => return *pane,
                Member::Axis(axis) => member = &axis.members[0],
            }
        }
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        collect_panes(&self.root, &mut panes);
        panes
    }

    pub fn rects(&self) -> Vec<(PaneId, Rect)> {
        let mut rects = Vec::new();
        collect_rects(
            &self.root,
            Rect {
                x: 0.,
                y: 0.,
                width: 1.,
                height: 1.,
            },
            &mut rects,
        );
        rects
    }

    pub fn find_pane_in_direction(
        &self,
        pane: PaneId,
        direction: SplitDirection,
    ) -> Option<PaneId> {
        let rects = self.rects();
        let bounds = rects.iter().find(|(id, _)| *id == pane)?.1;
        let center = (bounds.x + bounds.width / 2., bounds.y + bounds.height / 2.);
        let step = 1e-4;
        let target = match direction {
            SplitDirection::Left => (bounds.x - step, center.1),
            SplitDirection::Right => (bounds.x + bounds.width + step, center.1),
            SplitDirection::Up => (center.0, bounds.y - step),
            SplitDirection::Down => (center.0, bounds.y + bounds.height + step),
        };
        rects
            .iter()
            .find(|(_, rect)| rect.contains(target.0, target.1))
            .map(|(id, _)| *id)
    }
}

fn remove_from_axis(axis: &mut PaneAxis, pane: PaneId) -> Option<Option<Member>> {
    let mut found = false;
    let mut remove_member = None;
    for (idx, member) in axis.members.iter_mut().enumerate() {
        match member {
            Member::Axis(child) => {
                if let Some(last) = remove_from_axis(child, pane) {
                    if let Some(last) = last {
                        *member = last;
                    }
                    found = true;
                    break;
                }
            }
            Member::Pane(existing) => {
                if *existing == pane {
                    found = true;
                    remove_member = Some(idx);
                    break;
                }
            }
        }
    }
    if !found {
        return None;
    }
    if let Some(idx) = remove_member {
        axis.members.remove(idx);
    }
    if axis.members.len() == 1 {
        Some(axis.members.pop())
    } else {
        Some(None)
    }
}

fn collect_panes(member: &Member, panes: &mut Vec<PaneId>) {
    match member {
        Member::Pane(pane) => panes.push(*pane),
        Member::Axis(axis) => {
            for member in &axis.members {
                collect_panes(member, panes);
            }
        }
    }
}

fn collect_rects(member: &Member, bounds: Rect, rects: &mut Vec<(PaneId, Rect)>) {
    match member {
        Member::Pane(pane) => rects.push((*pane, bounds)),
        Member::Axis(axis) => {
            let count = axis.members.len().max(1) as f32;
            for (idx, member) in axis.members.iter().enumerate() {
                let offset = idx as f32 / count;
                let child = match axis.axis {
                    Axis::Horizontal => Rect {
                        x: bounds.x + bounds.width * offset,
                        width: bounds.width / count,
                        ..bounds
                    },
                    Axis::Vertical => Rect {
                        y: bounds.y + bounds.height * offset,
                        height: bounds.height / count,
                        ..bounds
                    },
                };
                collect_rects(member, child, rects);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis_of(group: &PaneGroup) -> &PaneAxis {
        match &group.root {
            Member::Axis(axis) => axis,
            Member::Pane(_) => panic!("expected an axis"),
        }
    }

    #[test]
    fn splits_along_and_across_axes() {
        let mut group = PaneGroup::new(1);
        group.split(1, 2, SplitDirection::Right);
        assert_eq!(group.panes(), vec![1, 2]);
        assert_eq!(axis_of(&group).axis, Axis::Horizontal);
        group.split(1, 3, SplitDirection::Left);
        assert_eq!(group.panes(), vec![3, 1, 2]);
        assert_eq!(axis_of(&group).members.len(), 3);
        group.split(2, 4, SplitDirection::Down);
        assert_eq!(group.panes(), vec![3, 1, 2, 4]);
        match &axis_of(&group).members[2] {
            Member::Axis(child) => {
                assert_eq!(child.axis, Axis::Vertical);
                assert_eq!(child.members, vec![Member::Pane(2), Member::Pane(4)]);
            }
            Member::Pane(_) => panic!("expected a nested axis"),
        }
    }

    #[test]
    fn a_restored_tree_gets_unique_split_ids() {
        let tree = Member::Axis(PaneAxis {
            id: 0,
            axis: Axis::Horizontal,
            members: vec![
                Member::Pane(1),
                Member::Axis(PaneAxis {
                    id: 0,
                    axis: Axis::Vertical,
                    members: vec![Member::Pane(2), Member::Pane(3)],
                }),
            ],
        });
        let mut group = PaneGroup::from_root(tree);
        let outer = axis_of(&group);
        let Member::Axis(inner) = &outer.members[1] else {
            panic!("expected a nested split");
        };
        assert_ne!(outer.id, inner.id);
        let ids = [outer.id, inner.id];
        group.split(1, 4, SplitDirection::Down);
        let Member::Axis(added) = &axis_of(&group).members[0] else {
            panic!("expected a new split");
        };
        assert!(!ids.contains(&added.id), "a later split reuses an ID");
    }

    #[test]
    fn splitting_an_unknown_pane_splits_the_first() {
        let mut group = PaneGroup::new(1);
        group.split(9, 2, SplitDirection::Up);
        assert_eq!(group.panes(), vec![2, 1]);
    }

    #[test]
    fn removing_collapses_single_member_axes() {
        let mut group = PaneGroup::new(1);
        group.split(1, 2, SplitDirection::Right);
        group.split(2, 3, SplitDirection::Down);
        assert!(group.remove(3));
        assert_eq!(group.panes(), vec![1, 2]);
        assert_eq!(
            axis_of(&group).members,
            vec![Member::Pane(1), Member::Pane(2)]
        );
        assert!(group.remove(1));
        assert_eq!(group.root, Member::Pane(2));
        assert!(!group.remove(2));
        assert!(!group.remove(7));
    }

    #[test]
    fn finds_neighbours_by_direction() {
        let mut group = PaneGroup::new(1);
        group.split(1, 2, SplitDirection::Right);
        group.split(2, 3, SplitDirection::Down);
        assert_eq!(
            group.find_pane_in_direction(1, SplitDirection::Right),
            Some(3)
        );
        assert_eq!(
            group.find_pane_in_direction(2, SplitDirection::Down),
            Some(3)
        );
        assert_eq!(
            group.find_pane_in_direction(3, SplitDirection::Left),
            Some(1)
        );
        assert_eq!(group.find_pane_in_direction(3, SplitDirection::Up), Some(2));
        assert_eq!(group.find_pane_in_direction(1, SplitDirection::Left), None);
    }

    #[test]
    fn drop_position_picks_the_nearest_edge() {
        let at = |x, y| SplitDirection::from_drop(x, y, 400., 200., 0.2);
        assert_eq!(at(200., 100.), None);
        assert_eq!(at(10., 100.), Some(SplitDirection::Left));
        assert_eq!(at(390., 100.), Some(SplitDirection::Right));
        assert_eq!(at(200., 5.), Some(SplitDirection::Up));
        assert_eq!(at(200., 195.), Some(SplitDirection::Down));
    }
}
