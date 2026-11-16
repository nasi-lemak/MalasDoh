//! Selecting in a file list the way file managers do: Ctrl+click toggles,
//! Shift+click takes a range, Ctrl+A takes everything, and the arrow keys
//! move (Shift extends). Shared by the server and local file lists.

/// A list's selection: what is selected, where a Shift range starts, and
/// the item the keyboard is on.
pub struct Selection<T> {
    pub selected: Vec<T>,
    anchor: Option<T>,
    cursor: Option<T>,
    /// The list was clicked last, so keys go to it.
    pub focused: bool,
}

impl<T> Default for Selection<T> {
    fn default() -> Self {
        Self { selected: Vec::new(), anchor: None, cursor: None, focused: false }
    }
}

/// What a key asks of the list's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction<T> {
    /// Enter on one item.
    Open(T),
    Delete,
    /// F2 on one item.
    Rename(T),
    /// Backspace: the folder above.
    Parent,
}

impl<T: Clone + PartialEq> Selection<T> {
    pub fn contains(&self, item: &T) -> bool {
        self.selected.contains(item)
    }

    /// A click on `item`; `order` is the list as shown.
    pub fn click(&mut self, order: &[T], item: &T, ctrl: bool, shift: bool) {
        self.focused = true;
        let range = |a: &T| {
            let (i, j) = (order.iter().position(|x| x == a)?, order.iter().position(|x| x == item)?);
            Some(order[i.min(j)..=i.max(j)].to_vec())
        };
        match (shift, self.anchor.as_ref().and_then(range)) {
            (true, Some(r)) if ctrl => {
                for x in r {
                    if !self.selected.contains(&x) {
                        self.selected.push(x);
                    }
                }
            }
            (true, Some(r)) => self.selected = r,
            _ if ctrl => {
                if let Some(i) = self.selected.iter().position(|x| x == item) {
                    self.selected.remove(i);
                } else {
                    self.selected.push(item.clone());
                }
                self.anchor = Some(item.clone());
            }
            _ => {
                self.selected = vec![item.clone()];
                self.anchor = Some(item.clone());
            }
        }
        self.cursor = Some(item.clone());
    }

    /// Selects `item` alone (a drag or a right-click on an unselected one).
    pub fn only(&mut self, item: &T) {
        self.selected = vec![item.clone()];
        self.anchor = Some(item.clone());
        self.cursor = Some(item.clone());
    }

    pub fn select_all(&mut self, order: &[T]) {
        self.selected = order.to_vec();
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
    }

    /// Moves the keyboard to `to` (an index into `order`, clamped); with
    /// `extend`, the selection runs from the anchor to there. Returns the
    /// new index, to scroll to.
    pub fn move_to(&mut self, order: &[T], to: isize, extend: bool) -> Option<usize> {
        if order.is_empty() {
            return None;
        }
        let i = to.clamp(0, order.len() as isize - 1) as usize;
        let item = order[i].clone();
        if extend && self.anchor.is_some() {
            self.click(order, &item, false, true);
        } else {
            self.only(&item);
        }
        Some(i)
    }

    /// Where the keyboard is in `order` (the last selected item when it
    /// has not moved yet).
    fn cursor_index(&self, order: &[T]) -> Option<usize> {
        let at = self.cursor.as_ref().or(self.selected.last())?;
        order.iter().position(|x| x == at)
    }

    /// The list's keys, when it has the keyboard. Returns what the owner
    /// should do, and a row to scroll to.
    pub fn keys(&mut self, ctx: &egui::Context, order: &[T]) -> (Option<KeyAction<T>>, Option<usize>) {
        use egui::{Key, Modifiers};
        // Not while typing elsewhere, nor under a dialog: Escape there clears
        // the text box's focus before this runs, and is the dialog's to use.
        if !self.focused || ctx.memory(|m| m.focused().is_some() || m.top_modal_layer().is_some()) {
            return (None, None);
        }
        let mut act = None;
        let mut scroll = None;
        let page = 10isize;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::COMMAND, Key::A) {
                self.select_all(order);
            }
            // Moves in order, each with its own Shift: a quick Shift+Down is
            // often over before the frame that sees it.
            let mut moves = Vec::new();
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. }
                    if !modifiers.command && !modifiers.ctrl && !modifiers.alt =>
                {
                    let k = *key;
                    let known =
                        matches!(k, Key::ArrowDown | Key::ArrowUp | Key::PageDown | Key::PageUp | Key::Home | Key::End);
                    if known {
                        moves.push((k, modifiers.shift));
                    }
                    !known
                }
                _ => true,
            });
            for (key, shift) in moves {
                let here = self.cursor_index(order).map(|x| x as isize);
                let to = match key {
                    Key::ArrowDown => here.map_or(0, |h| h + 1),
                    Key::ArrowUp => here.map_or(0, |h| h - 1),
                    Key::PageDown => here.map_or(0, |h| h + page),
                    Key::PageUp => here.map_or(0, |h| h - page),
                    Key::Home => 0,
                    _ => order.len() as isize - 1,
                };
                scroll = self.move_to(order, to, shift);
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                self.clear();
            }
            let one = (self.selected.len() == 1).then(|| self.selected[0].clone());
            if i.consume_key(Modifiers::NONE, Key::Enter)
                && let Some(x) = one.clone()
            {
                act = Some(KeyAction::Open(x));
            }
            if i.consume_key(Modifiers::NONE, Key::F2)
                && let Some(x) = one
            {
                act = Some(KeyAction::Rename(x));
            }
            if i.consume_key(Modifiers::NONE, Key::Delete) && !self.selected.is_empty() {
                act = Some(KeyAction::Delete);
            }
            if i.consume_key(Modifiers::NONE, Key::Backspace) {
                act = Some(KeyAction::Parent);
            }
        });
        (act, scroll)
    }

    /// Clicks elsewhere take the keyboard away from the list, including
    /// clicks on a window lying over it.
    pub fn track_focus(&mut self, ui: &egui::Ui, list: egui::Rect) {
        let ctx = ui.ctx();
        let pressed = ctx.input(|i| i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten());
        if let Some(p) = pressed {
            self.focused = list.contains(p) && ctx.layer_id_at(p).is_none_or(|l| l == ui.layer_id());
        }
    }

    /// Forgets items no longer in the list (after a refresh).
    pub fn retain(&mut self, keep: impl Fn(&T) -> bool) {
        self.selected.retain(&keep);
        if self.anchor.as_ref().is_some_and(|a| !keep(a)) {
            self.anchor = None;
        }
        if self.cursor.as_ref().is_some_and(|c| !keep(c)) {
            self.cursor = None;
        }
    }
}

/// The scroll offset that shows row `i` (of height `row_h`) in a view of
/// height `view` now at `offset`; `None` when it is already in view.
pub fn scroll_for(i: usize, row_h: f32, offset: f32, view: f32) -> Option<f32> {
    let top = i as f32 * row_h;
    if top < offset {
        Some(top)
    } else if top + row_h > offset + view {
        Some(top + row_h - view)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<&'static str> {
        vec!["a", "b", "c", "d", "e"]
    }

    #[test]
    fn clicks_like_a_file_manager() {
        let order = list();
        let mut s = Selection::default();
        s.click(&order, &"b", false, false);
        assert_eq!(s.selected, ["b"]);
        // Shift: the range from the last plain click.
        s.click(&order, &"d", false, true);
        assert_eq!(s.selected, ["b", "c", "d"]);
        // Upwards too, from the same anchor.
        s.click(&order, &"a", false, true);
        assert_eq!(s.selected, ["a", "b"]);
        // Ctrl toggles and moves the anchor.
        s.click(&order, &"e", true, false);
        assert_eq!(s.selected, ["a", "b", "e"]);
        s.click(&order, &"a", true, false);
        assert_eq!(s.selected, ["b", "e"]);
        // Ctrl+Shift adds a range to what is there.
        s.click(&order, &"c", true, true);
        assert_eq!(s.selected, ["b", "e", "a", "c"]);
        // A plain click starts over.
        s.click(&order, &"d", false, false);
        assert_eq!(s.selected, ["d"]);
        assert!(s.focused);
    }

    #[test]
    fn keys_move_and_extend() {
        let order = list();
        let mut s = Selection::default();
        s.select_all(&order);
        assert_eq!(s.selected.len(), 5);
        assert_eq!(s.move_to(&order, 0, false), Some(0));
        assert_eq!(s.selected, ["a"]);
        // Shift+Down twice from a: a..c.
        s.move_to(&order, 1, true);
        s.move_to(&order, 2, true);
        assert_eq!(s.selected, ["a", "b", "c"]);
        // Past the ends stays at the ends.
        assert_eq!(s.move_to(&order, 99, false), Some(4));
        assert_eq!(s.selected, ["e"]);
        assert_eq!(s.move_to(&order, -3, false), Some(0));
        assert_eq!(Selection::<&str>::default().move_to(&[], 1, false), None);
        // A refresh that removed items forgets them.
        s.select_all(&order);
        s.retain(|x| *x != "c");
        assert_eq!(s.selected, ["a", "b", "d", "e"]);
    }

    #[test]
    fn scrolls_only_when_the_row_is_out_of_view() {
        assert_eq!(scroll_for(2, 20.0, 0.0, 100.0), None);
        assert_eq!(scroll_for(7, 20.0, 0.0, 100.0), Some(60.0));
        assert_eq!(scroll_for(1, 20.0, 50.0, 100.0), Some(20.0));
    }
}
