use std::cmp;

pub type PanelId = u64;

pub struct TabList<T> {
    items: Vec<(PanelId, T)>,
    active: usize,
}

impl<T> Default for TabList<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            active: 0,
        }
    }
}

impl<T> TabList<T> {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&T> {
        self.items.get(self.active).map(|(_, item)| item)
    }

    pub fn iter(&self) -> impl Iterator<Item = (PanelId, &T)> {
        self.items.iter().map(|(id, item)| (*id, item))
    }

    pub fn get(&self, ix: usize) -> Option<&T> {
        self.items.get(ix).map(|(_, item)| item)
    }

    pub fn position(&self, id: PanelId) -> Option<usize> {
        self.items.iter().position(|(existing, _)| *existing == id)
    }

    pub fn find(&self, pred: impl Fn(&T) -> bool) -> Option<usize> {
        self.items.iter().position(|(_, item)| pred(item))
    }

    pub fn activate(&mut self, ix: usize) -> bool {
        if ix < self.items.len() {
            self.active = ix;
            true
        } else {
            false
        }
    }

    pub fn activate_next(&mut self) {
        if !self.items.is_empty() {
            self.active = (self.active + 1) % self.items.len();
        }
    }

    pub fn activate_previous(&mut self) {
        if !self.items.is_empty() {
            let len = self.items.len();
            self.active = (self.active + len - 1) % len;
        }
    }

    pub fn add(&mut self, id: PanelId, item: T, destination: Option<usize>) -> usize {
        let index = cmp::min(
            destination.unwrap_or(if self.items.is_empty() {
                0
            } else {
                self.active + 1
            }),
            self.items.len(),
        );
        self.items.insert(index, (id, item));
        self.active = index;
        index
    }

    pub fn move_item(&mut self, id: PanelId, destination: usize) -> bool {
        let Some(existing) = self.position(id) else {
            return false;
        };
        let mut insertion = cmp::min(destination, self.items.len());
        if existing == insertion {
            self.active = existing;
            return false;
        }
        let item = self.items.remove(existing);
        if existing < self.active {
            self.active -= 1;
        }
        insertion = insertion.min(self.items.len());
        self.items.insert(insertion, item);
        self.active = insertion;
        true
    }

    pub fn remove(&mut self, ix: usize) -> Option<T> {
        if ix >= self.items.len() {
            return None;
        }
        let (_, item) = self.items.remove(ix);
        if self.items.is_empty() {
            self.active = 0;
        } else if ix < self.active || self.active >= self.items.len() {
            self.active -= 1;
        }
        Some(item)
    }

    pub fn drain(&mut self) -> Vec<T> {
        self.active = 0;
        self.items.drain(..).map(|(_, item)| item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(names: &[&'static str]) -> TabList<&'static str> {
        let mut tabs = TabList::default();
        for (ix, name) in names.iter().enumerate() {
            tabs.add(ix as PanelId, *name, Some(ix));
        }
        tabs
    }

    fn names(tabs: &TabList<&'static str>) -> Vec<&'static str> {
        tabs.iter().map(|(_, name)| *name).collect()
    }

    #[test]
    fn adds_after_the_active_tab() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.activate(0);
        tabs.add(9, "x", None);
        assert_eq!(names(&tabs), ["a", "x", "b", "c"]);
        assert_eq!(tabs.active(), Some(&"x"));
    }

    #[test]
    fn moves_right_onto_a_tab_and_lands_after_it() {
        let mut tabs = list(&["a", "b", "c", "d"]);
        tabs.activate(3);
        assert!(tabs.move_item(0, 2));
        assert_eq!(names(&tabs), ["b", "c", "a", "d"]);
        assert_eq!(tabs.active(), Some(&"a"));
    }

    #[test]
    fn moves_left_onto_a_tab_and_lands_before_it() {
        let mut tabs = list(&["a", "b", "c", "d"]);
        assert!(tabs.move_item(3, 1));
        assert_eq!(names(&tabs), ["a", "d", "b", "c"]);
        assert_eq!(tabs.active_index(), 1);
    }

    #[test]
    fn moves_to_the_end_when_dropped_on_the_bar() {
        let mut tabs = list(&["a", "b", "c"]);
        assert!(tabs.move_item(0, tabs.len()));
        assert_eq!(names(&tabs), ["b", "c", "a"]);
        assert!(!tabs.move_item(42, 0));
    }

    #[test]
    fn removing_keeps_a_sensible_active_tab() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.activate(2);
        tabs.remove(0);
        assert_eq!(tabs.active(), Some(&"c"));
        tabs.remove(1);
        assert_eq!(tabs.active(), Some(&"b"));
        tabs.remove(0);
        assert!(tabs.is_empty());
        assert_eq!(tabs.active(), None);
    }

    #[test]
    fn cycles_through_tabs() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.activate(2);
        tabs.activate_next();
        assert_eq!(tabs.active(), Some(&"a"));
        tabs.activate_previous();
        assert_eq!(tabs.active(), Some(&"c"));
    }
}
