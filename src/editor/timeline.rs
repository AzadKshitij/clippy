//! Línea de tiempo: las piezas del video, cortar, duplicar, borrar y deshacer.

use std::path::{Path, PathBuf};

use super::EditorApp;

pub(super) const MIN_LEN: i64 = 2_000_000; // 0,2 s
/// Pasos de deshacer que se guardan.
const UNDO_MAX: usize = 100;

fn push_capped(stack: &mut Vec<Vec<Piece>>, v: Vec<Piece>) {
    stack.push(v);
    if stack.len() > UNDO_MAX {
        stack.remove(0);
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct Piece {
    pub(super) id: u64,
    pub(super) path: PathBuf,
    pub(super) src_in: i64,
    pub(super) src_out: i64,
    pub(super) gain: f32,
    pub(super) muted: bool,
}

impl Piece {
    pub(super) fn len(&self) -> i64 {
        self.src_out - self.src_in
    }
    pub(super) fn eff_gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.gain
        }
    }
}

impl EditorApp {
    pub(super) fn starts(&self) -> Vec<i64> {
        let mut s = Vec::with_capacity(self.tl.len());
        let mut t = 0;
        for p in &self.tl {
            s.push(t);
            t += p.len();
        }
        s
    }

    pub(super) fn total(&self) -> i64 {
        self.tl.iter().map(|p| p.len()).sum()
    }

    /// Pieza y tiempo dentro del archivo para un tiempo de la línea.
    pub(super) fn locate(&self, t: i64) -> Option<(usize, i64)> {
        let mut start = 0;
        for (i, p) in self.tl.iter().enumerate() {
            if t < start + p.len() {
                return Some((i, p.src_in + (t - start).max(0)));
            }
            start += p.len();
        }
        self.tl.last().map(|p| (self.tl.len() - 1, p.src_out))
    }

    pub(super) fn index_of(&self, id: u64) -> Option<usize> {
        self.tl.iter().position(|p| p.id == id)
    }

    /// Guarda cómo estaba la línea (`before`) para poder deshacer el cambio que se hizo o se va a hacer.
    pub(super) fn record_undo(&mut self, before: Vec<Piece>) {
        self.dirty = true;
        push_capped(&mut self.undo, before);
        self.redo.clear();
    }

    fn push_undo(&mut self) {
        self.record_undo(self.tl.clone());
    }

    pub(super) fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            let pos = self.pos();
            self.dirty = true;
            self.redo.push(std::mem::replace(&mut self.tl, prev));
            self.after_edit(pos);
        }
    }

    pub(super) fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            let pos = self.pos();
            self.dirty = true;
            let now = std::mem::replace(&mut self.tl, next);
            push_capped(&mut self.undo, now);
            self.after_edit(pos);
        }
    }

    pub(super) fn add_to_timeline(&mut self, path: &Path) {
        let Some(pr) = self.loader.probe(path) else { return };
        if pr.duration <= MIN_LEN {
            return;
        }
        self.push_undo();
        let id = self.next_id;
        self.next_id += 1;
        let start = self.total();
        self.tl.push(Piece { id, path: path.to_path_buf(), src_in: 0, src_out: pr.duration, gain: 1.0, muted: false });
        self.selected = Some(id);
        self.loader.want_strip(path);
        self.after_edit(start);
        self.message = None;
    }

    pub(super) fn split(&mut self) {
        let t = self.pos();
        let Some((i, st)) = self.locate(t) else { return };
        let p = self.tl[i].clone();
        if st - p.src_in < MIN_LEN || p.src_out - st < MIN_LEN {
            return;
        }
        self.push_undo();
        let mut b = p.clone();
        b.id = self.next_id;
        self.next_id += 1;
        b.src_in = st;
        self.tl[i].src_out = st;
        self.tl.insert(i + 1, b.clone());
        self.selected = Some(b.id);
        self.after_edit(t);
    }

    pub(super) fn delete_selected(&mut self) {
        let Some(i) = self.selected.and_then(|id| self.index_of(id)) else { return };
        self.push_undo();
        let start = self.starts()[i];
        self.tl.remove(i);
        self.selected = self.tl.get(i).or(self.tl.last()).map(|p| p.id);
        self.after_edit(start);
    }

    pub(super) fn duplicate_selected(&mut self) {
        let Some(i) = self.selected.and_then(|id| self.index_of(id)) else { return };
        self.push_undo();
        let mut p = self.tl[i].clone();
        p.id = self.next_id;
        self.next_id += 1;
        let start = self.starts()[i] + self.tl[i].len();
        self.tl.insert(i + 1, p.clone());
        self.selected = Some(p.id);
        self.after_edit(start);
    }

    /// Tras cualquier cambio: pausar, soltar reproductores que ya no se usan y ubicar el cursor.
    /// Las piezas quedan en su lugar de una (deshacer y el teclado no se animan).
    fn after_edit(&mut self, pos: i64) {
        self.settle = None;
        self.pause();
        let used: Vec<PathBuf> = self.tl.iter().map(|p| p.path.clone()).collect();
        self.players.retain(|k, _| used.contains(k));
        for p in &used {
            self.loader.want_strip(p);
        }
        self.active = None;
        self.seek(pos);
    }

    pub(super) fn toggle_mute_selected(&mut self) {
        let Some(i) = self.selected.and_then(|id| self.index_of(id)) else { return };
        self.push_undo();
        self.tl[i].muted = !self.tl[i].muted;
        self.refresh_gain(i);
    }

    pub(super) fn commit(&mut self, before: Option<Vec<Piece>>, pos: i64) {
        if let Some(b) = before {
            if b != self.tl {
                self.record_undo(b);
            }
        }
        self.after_edit(pos);
    }
}
