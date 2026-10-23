//! Service queue: the sequence of windows, files and links to show one after another.
//! Logic only; the UI is in `main.rs`.

use serde::{Deserialize, Serialize};

use crate::win::WindowInfo;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Kind {
    /// Open window whose title contains this text.
    Window(String),
    File(String),
    Url(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    pub label: String,
    pub kind: Kind,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Queue {
    pub items: Vec<Item>,
    /// Item shown last; `None` = not started yet.
    pub pos: Option<usize>,
}

impl Queue {
    pub fn add(&mut self, item: Item) {
        self.items.push(item);
    }

    pub fn remove(&mut self, i: usize) {
        if i >= self.items.len() {
            return;
        }
        self.items.remove(i);
        // Removing current: next becomes the one that took its place.
        self.pos = match self.pos {
            Some(p) if i < p => Some(p - 1),
            Some(p) if i == p => p.checked_sub(1),
            p => p,
        };
    }

    pub fn move_up(&mut self, i: usize) {
        if i > 0 && i < self.items.len() {
            self.swap(i - 1, i);
        }
    }

    pub fn move_down(&mut self, i: usize) {
        if i + 1 < self.items.len() {
            self.swap(i, i + 1);
        }
    }

    /// Swaps two neighbors keeping the highlight with the current item.
    fn swap(&mut self, a: usize, b: usize) {
        self.items.swap(a, b);
        self.pos = match self.pos {
            Some(p) if p == a => Some(b),
            Some(p) if p == b => Some(a),
            p => p,
        };
    }

    /// Advances and returns the new current item. At the end of the queue it stops (returns `None`)
    /// until someone clicks Restart.
    pub fn next(&mut self) -> Option<&Item> {
        let n = self.pos.map_or(0, |p| p + 1);
        if n >= self.items.len() {
            return None;
        }
        self.pos = Some(n);
        self.items.get(n)
    }

    pub fn reset(&mut self) {
        self.pos = None;
    }
}

/// The open window matching the item (title contains, case insensitive).
pub fn find_window<'a>(item: &Item, windows: &'a [WindowInfo]) -> Option<&'a WindowInfo> {
    let Kind::Window(part) = &item.kind else { return None };
    let part = part.to_lowercase();
    windows.iter().find(|w| w.title.to_lowercase().contains(&part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::Hwnd;

    fn item(s: &str) -> Item {
        Item { label: s.into(), kind: Kind::Window(s.into()) }
    }

    fn fila(n: usize) -> Queue {
        let mut q = Queue::default();
        for s in ["a", "b", "c", "d"].iter().take(n) {
            q.add(item(s));
        }
        q
    }

    fn labels(q: &Queue) -> Vec<&str> {
        q.items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn add_poe_no_fim() {
        let q = fila(3);
        assert_eq!(labels(&q), ["a", "b", "c"]);
        assert_eq!(q.pos, None);
    }

    #[test]
    fn remove_ajusta_o_atual() {
        let mut q = fila(4);
        q.pos = Some(2);
        q.remove(0);
        assert_eq!((labels(&q), q.pos), (vec!["b", "c", "d"], Some(1)));
        // Removing current: next becomes the one that took its place.
        q.remove(1);
        assert_eq!(q.pos, Some(0));
        assert_eq!(q.next().unwrap().label, "d");
        q.remove(9); // fora da lista: nada acontece
        assert_eq!(labels(&q), ["b", "d"]);
    }

    #[test]
    fn move_up_leva_o_destaque_junto() {
        let mut q = fila(3);
        q.pos = Some(1);
        q.move_up(1);
        assert_eq!((labels(&q), q.pos), (vec!["b", "a", "c"], Some(0)));
        q.move_up(0); // already first
        assert_eq!(labels(&q), ["b", "a", "c"]);
    }

    #[test]
    fn move_down_leva_o_destaque_junto() {
        let mut q = fila(3);
        q.pos = Some(2);
        q.move_down(1);
        assert_eq!((labels(&q), q.pos), (vec!["a", "c", "b"], Some(1)));
        q.move_down(2); // already last
        assert_eq!(labels(&q), ["a", "c", "b"]);
    }

    #[test]
    fn next_avanca_e_para_no_fim() {
        let mut q = fila(2);
        assert_eq!(q.next().unwrap().label, "a");
        assert_eq!(q.next().unwrap().label, "b");
        assert!(q.next().is_none());
        assert_eq!(q.pos, Some(1));
        assert!(Queue::default().next().is_none());
    }

    #[test]
    fn reset_volta_ao_comeco() {
        let mut q = fila(2);
        q.next();
        q.next();
        q.reset();
        assert_eq!(q.next().unwrap().label, "a");
    }

    #[test]
    fn find_window_por_parte_do_titulo() {
        let w = |h, t: &str| WindowInfo {
            hwnd: Hwnd(h),
            title: t.into(),
            exe: String::new(),
            exe_path: String::new(),
            minimized: false,
            uwp: false,
        };
        let abertas = [w(1, "Letras - Holyrics"), w(2, "Culto.pptx - PowerPoint")];
        assert_eq!(find_window(&item("powerpoint"), &abertas).unwrap().hwnd, Hwnd(2));
        assert!(find_window(&item("VLC"), &abertas).is_none());
        let arquivo = Item { label: "x".into(), kind: Kind::File("Letras".into()) };
        assert!(find_window(&arquivo, &abertas).is_none());
    }
}
