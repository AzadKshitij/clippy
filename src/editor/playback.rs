//! Reproducción de la secuencia: un reproductor por archivo, cursor y vista previa.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui;

use super::player::Player;
use super::EditorApp;

/// Cuadro de 60 fps, para cuando no se sabe el fps del archivo.
const UNKNOWN_FRAME: i64 = 166_667;

impl EditorApp {
    /// Duración de un cuadro (100 ns) de ese archivo: los clips pueden ser de 30, 60 o 120 fps.
    /// Redondeada para arriba: avanzar N cuadros cae justo pasando el borde del cuadro N, no antes.
    pub(super) fn frame_len(&self, path: &Path) -> i64 {
        let fps = self.loader.probe(path).map_or(0.0, |p| p.fps());
        if fps >= 1.0 {
            (1e7 / fps).ceil() as i64
        } else {
            UNKNOWN_FRAME
        }
    }

    /// Duración de un cuadro de la pieza que está bajo el cursor de reproducción.
    pub(super) fn frame_at_cursor(&self) -> i64 {
        self.locate(self.pos()).map_or(UNKNOWN_FRAME, |(i, _)| self.frame_len(&self.tl[i].path))
    }

    /// Si la pieza `i` es la que suena, le aplica su volumen ya. El reproductor es uno por archivo
    /// y lo comparten las piezas de ese archivo: las demás toman su volumen al empezar a sonar.
    pub(super) fn refresh_gain(&self, i: usize) {
        if self.active != Some(i) {
            return;
        }
        if let Some(p) = self.tl.get(i) {
            if let Some(pl) = self.players.get(&p.path) {
                pl.set_gain(p.eff_gain());
            }
        }
    }

    fn player(&mut self, path: &Path) -> Option<&Player> {
        if !self.players.contains_key(path) {
            let pr = self.loader.probe(path)?;
            self.players.insert(path.to_path_buf(), Player::open(path.to_path_buf(), &pr, self.ctx.clone()));
        }
        self.players.get(path)
    }

    pub(super) fn pos(&self) -> i64 {
        // Mientras se arrastra el cursor manda el mouse: el video lo sigue con un poco de demora.
        if let Some(t) = self.scrub_t {
            return t;
        }
        if let Some(i) = self.active {
            if let (Some(p), Some(start)) = (self.tl.get(i), self.starts().get(i)) {
                if let Some(pl) = self.players.get(&p.path) {
                    return start + (pl.pos() - p.src_in).clamp(0, p.len());
                }
            }
        }
        self.pos_hint.clamp(0, self.total())
    }

    pub(super) fn seek(&mut self, t: i64) {
        let t = t.clamp(0, self.total());
        self.pos_hint = t;
        self.prerolled = None;
        let Some((i, st)) = self.locate(t) else {
            self.active = None;
            return;
        };
        if let Some(old) = self.active.filter(|&o| o != i).and_then(|o| self.tl.get(o)) {
            if old.path != self.tl[i].path {
                if let Some(pl) = self.players.get(&old.path) {
                    pl.pause();
                }
            }
        }
        let piece = self.tl[i].clone();
        let playing = self.playing;
        if let Some(pl) = self.player(&piece.path) {
            pl.set_gain(piece.eff_gain());
            pl.seek(st);
            if playing && !pl.playing() {
                pl.play();
            }
        }
        self.active = Some(i);
    }

    fn play(&mut self) {
        if self.tl.is_empty() {
            return;
        }
        let mut t = self.pos();
        let last = self.tl.last().map_or(UNKNOWN_FRAME, |p| self.frame_len(&p.path));
        if t >= self.total() - last {
            t = 0;
        }
        self.playing = true;
        self.seek(t);
        if let Some(p) = self.active.and_then(|i| self.tl.get(i)).cloned() {
            if let Some(pl) = self.players.get(&p.path) {
                pl.play();
            }
        }
    }

    pub(super) fn pause(&mut self) {
        let pos = self.pos();
        if let Some(p) = self.active.and_then(|i| self.tl.get(i)) {
            if let Some(pl) = self.players.get(&p.path) {
                pl.pause();
            }
        }
        self.playing = false;
        self.pos_hint = pos;
    }

    pub(super) fn toggle_play(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Avanza a la siguiente pieza cuando termina la actual (y prepara la siguiente antes).
    pub(super) fn tick(&mut self) {
        if !self.playing {
            return;
        }
        let Some(i) = self.active else {
            self.playing = false;
            return;
        };
        let Some(piece) = self.tl.get(i).cloned() else {
            self.playing = false;
            return;
        };
        let Some(pl) = self.players.get(&piece.path) else { return };
        let src = pl.pos();
        let ended = src >= piece.src_out - self.frame_len(&piece.path) / 2 || (!pl.playing() && pl.shared.ended.load(Ordering::Relaxed));
        if let Some(next) = self.tl.get(i + 1).cloned() {
            if piece.src_out - src < 6_000_000 && self.prerolled != Some(i + 1) && next.path != piece.path {
                if let Some(np) = self.player(&next.path) {
                    np.set_gain(next.eff_gain());
                    np.seek(next.src_in);
                }
                self.prerolled = Some(i + 1);
            }
        }
        if !ended {
            return;
        }
        if let Some(pl) = self.players.get(&piece.path) {
            pl.pause();
        }
        match self.tl.get(i + 1).cloned() {
            Some(next) => {
                let pre = self.prerolled == Some(i + 1);
                if let Some(np) = self.player(&next.path) {
                    np.set_gain(next.eff_gain());
                    if !pre || next.path == piece.path {
                        np.seek(next.src_in);
                    }
                    np.play();
                }
                self.active = Some(i + 1);
                self.prerolled = None;
            }
            None => {
                self.playing = false;
                self.pos_hint = self.total();
            }
        }
    }

    pub(super) fn update_texture(&mut self) {
        let Some(p) = self.active.and_then(|i| self.tl.get(i)) else {
            if self.tl.is_empty() {
                self.tex = None;
            }
            return;
        };
        let Some(pl) = self.players.get(&p.path) else { return };
        let gen = pl.shared.gen.load(Ordering::Relaxed);
        if self.tex_src.0.as_deref() == Some(p.path.as_path()) && self.tex_src.1 == gen {
            return;
        }
        let frame = pl.shared.frame.lock().unwrap().clone();
        if let Some(img) = frame {
            match &mut self.tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.tex = Some(self.ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
            self.tex_src = (Some(p.path.clone()), gen);
        }
    }

    pub(super) fn step(&mut self, dt: i64) {
        if self.playing {
            self.pause();
        }
        let t = self.pos() + dt;
        self.seek(t);
    }

    /// Muestra un cuadro de una pieza mientras se recorta.
    pub(super) fn preview_source(&mut self, i: usize, src_t: i64) {
        let p = self.tl[i].clone();
        if let Some(old) = self.active.and_then(|o| self.tl.get(o)).filter(|o| o.path != p.path) {
            if let Some(pl) = self.players.get(&old.path) {
                pl.pause();
            }
        }
        if let Some(pl) = self.player(&p.path) {
            pl.seek(src_t);
        }
        self.active = Some(i);
        self.last_seek = Instant::now();
    }
}
