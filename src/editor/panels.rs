//! Paneles de la ventana: barra superior, biblioteca, vista previa, propiedades y pistas.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Align, Align2, Color32, CursorIcon, FontId, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2};

use crate::ui_kit::theme::*;
use crate::ui_kit::window::open_folder;
use super::library::Asset;
use super::shortcuts::Act;
use super::shortcuts;
use super::library;
use super::timeline::{MIN_LEN, Piece};
use super::EditorApp;

const PAD: f32 = 16.0;

const GUTTER: f32 = 74.0;

const RULER_H: f32 = 26.0;

const LANE_GAP: f32 = 6.0;
/// Alto mínimo de la pista de video y de la de audio al arrastrar el borde entre las dos.
const LANE_MIN: f32 = 32.0;

/// Lugar debajo de las pistas para la barra de desplazamiento.
const SCROLLBAR_H: f32 = 12.0;

/// Agarradera de un borde de clip, hacia adentro (en un clip corto, un tercio de su ancho).
const GRIP: f32 = 8.0;
/// Margen extra hacia afuera, solo en las dos puntas de la línea.
const GRIP_OUT: f32 = 7.0;
/// Arrastrando a menos de esto de un costado de la vista, la línea se desplaza sola (hasta AUTO_SCROLL px/s).
const EDGE: f32 = 48.0;
const AUTO_SCROLL: f32 = 900.0;
/// Zoom de la línea de tiempo: 1 = entra entera.
const ZOOM_MAX: f32 = 40.0;
/// Mientras se arrastra, el video busca el cuadro como mucho cada tanto (buscar es caro).
const SEEK_EVERY: Duration = Duration::from_millis(40);
/// Alto del título y los datos debajo de la miniatura de una tarjeta de la biblioteca.
const CARD_TEXT_H: f32 = 42.0;

#[derive(Clone)]
pub(super) enum Drag {
    None,
    Scrub,
    /// `press_x` se mide desde el inicio de la línea (no de la pantalla) y `pps` queda fijo mientras dura:
    /// así el borde sigue al mouse aunque la línea se desplace o cambie de largo.
    TrimIn { i: usize, orig: Piece, press_x: f32, pps: f32 },
    TrimOut { i: usize, orig: Piece, press_x: f32, pps: f32, max: i64 },
    Move { i: usize, grab_dx: f32 },
}

/// Índice donde quedaría la pieza `i` si su centro está en `center_x`.
fn insert_index(xs: &[(f32, f32)], i: usize, center_x: f32) -> usize {
    let mut idx = 0;
    for (j, &(a, b)) in xs.iter().enumerate() {
        if j == i {
            continue;
        }
        if center_x > (a + b) / 2.0 {
            idx = j + 1;
        }
    }
    idx
}

#[allow(clippy::too_many_arguments)]
fn draw_piece(painter: &egui::Painter, p: &Piece, info: Option<&library::Info>, a: f32, b: f32, vlane: Rect, alane: Rect, pps: f32, selected: bool, alpha: f32) {
    let vr = Rect::from_min_max(Pos2::new(a + 1.0, vlane.top()), Pos2::new(b - 1.0, vlane.bottom()));
    let ar = Rect::from_min_max(Pos2::new(a + 1.0, alane.top()), Pos2::new(b - 1.0, alane.bottom()));
    let fade = |c: Color32| c.gamma_multiply(alpha);
    painter.rect_filled(vr, 6.0, fade(TRACK_VIDEO));
    painter.rect_filled(ar, 6.0, fade(TRACK_AUDIO));

    // Miniaturas
    let clip = painter.with_clip_rect(vr.shrink(1.0).intersect(painter.clip_rect()));
    if let Some(info) = info {
        if !info.thumbs.is_empty() {
            let tile_w = (vr.height() * 16.0 / 9.0).floor();
            let mut x = vr.left();
            while x < vr.right() {
                let t = p.src_in + ((x - vr.left() + tile_w / 2.0) / pps * 1e7) as i64;
                let k = info.thumbs.partition_point(|(tt, _)| *tt <= t).saturating_sub(1);
                let tex = &info.thumbs[k].1;
                let r = Rect::from_min_size(Pos2::new(x, vr.top()), egui::vec2(tile_w, vr.height()));
                clip.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE.gamma_multiply(alpha));
                x += tile_w;
            }
        }
    }
    // Etiqueta
    if vr.width() > 70.0 {
        let label = format!("{}  ·  {}", library::pretty_name(&p.path), fmt_len(p.len()));
        let g = one_line(painter, label, FontId::proportional(11.5), vr.width() - 22.0);
        let lr = Rect::from_min_size(vr.min + egui::vec2(6.0, 6.0), g.size() + egui::vec2(10.0, 4.0));
        painter.rect_filled(lr, 4.0, BADGE);
        painter.galley(lr.min + egui::vec2(5.0, 2.0), g, WHITE);
    }

    // Forma de onda
    let clip = painter.with_clip_rect(ar.shrink(1.0).intersect(painter.clip_rect()));
    let mid = ar.center().y;
    let half = ar.height() / 2.0 - 5.0;
    match info.and_then(|i| i.wave.as_ref()) {
        Some(w) if !p.muted => {
            // Una barra por píxel de pantalla, alineada a la grilla (sin huecos ni rayas al arrastrar).
            // Cada barra muestra lo que cae en ese píxel: el pico, claro, y el volumen medio (RMS), más fuerte
            // por dentro. Escala en dB de -48 a 0: el audio bajo (lo normal en clips de juego) se ve, y como el
            // RMS queda varios dB debajo del pico, lo fuerte no se vuelve un bloque.
            let gain = p.gain.min(2.0);
            let peak_col = WAVE.gamma_multiply(alpha * 0.45);
            let rms_col = WAVE.gamma_multiply(alpha);
            let px = 1.0 / painter.ctx().pixels_per_point();
            let left = ar.left().max(clip.clip_rect().left());
            let right = ar.right().min(clip.clip_rect().right());
            let t_at = |x: f32| p.src_in + ((x - ar.left()) / pps * 1e7) as i64;
            let size = |v: f32| {
                let v = v * gain;
                let db = if v > 0.0 { 20.0 * v.log10() } else { -100.0 };
                (((db + 48.0) / 48.0).clamp(0.0, 1.0) * half).max(px / 2.0)
            };
            let mut mesh = egui::Mesh::default();
            let mut x = (left / px).floor() * px;
            while x < right {
                let (peak, rms) = w.range(t_at(x), t_at(x + px));
                let (hp, hr) = (size(peak), size(rms));
                mesh.add_colored_rect(Rect::from_min_max(Pos2::new(x, mid - hp), Pos2::new(x + px, mid + hp)), peak_col);
                mesh.add_colored_rect(Rect::from_min_max(Pos2::new(x, mid - hr), Pos2::new(x + px, mid + hr)), rms_col);
                x += px;
            }
            clip.add(egui::Shape::mesh(mesh));
        }
        Some(_) => {
            clip.hline(ar.x_range(), mid, Stroke::new(1.0_f32, MUTED));
            if ar.width() > 80.0 {
                clip.text(Pos2::new(ar.left() + 8.0, mid), Align2::LEFT_CENTER, "Silenciado", FontId::proportional(11.5), MUTED);
            }
        }
        None => {
            if ar.width() > 80.0 {
                clip.text(Pos2::new(ar.left() + 8.0, mid), Align2::LEFT_CENTER, "Cargando audio…", FontId::proportional(11.5), MUTED);
            }
        }
    }

    // Borde y agarraderas
    let outer = Rect::from_min_max(vr.min, Pos2::new(vr.right(), ar.bottom()));
    if selected {
        painter.rect_stroke(vr, 6.0, Stroke::new(2.0_f32, WHITE));
        painter.rect_stroke(ar, 6.0, Stroke::new(2.0_f32, WHITE));
        for x in [vr.left() + 3.0, vr.right() - 3.0] {
            let h = Rect::from_center_size(Pos2::new(x, outer.center().y), egui::vec2(6.0, outer.height()));
            painter.rect_filled(h, 3.0, WHITE);
            painter.line_segment([Pos2::new(x, outer.center().y - 8.0), Pos2::new(x, outer.center().y + 8.0)], Stroke::new(1.0_f32, BLACK));
        }
    } else {
        painter.rect_stroke(vr, 6.0, Stroke::new(1.0_f32, LINE));
        painter.rect_stroke(ar, 6.0, Stroke::new(1.0_f32, LINE));
    }
}

/// Alto de las pistas de video y de audio cuando todo (regla incluida) mide `tracks_h`. `split` es la
/// parte del alto que usa el video (la elige el usuario arrastrando el borde entre las dos pistas).
fn lanes(tracks_h: f32, split: f32) -> (f32, f32) {
    let avail = (tracks_h - RULER_H - 2.0 * LANE_GAP).max(0.0);
    let min = LANE_MIN.min(avail / 2.0);
    let video = (avail * split).clamp(min, avail - min);
    (video, avail - video)
}

/// Regla: "0:12" en los segundos enteros y "0:12.5" en los medios.
fn fmt_ruler(s: f32) -> String {
    let ds = (s * 10.0).round() as u64; // décimas
    if ds.is_multiple_of(10) {
        fmt_clock(ds / 10)
    } else {
        format!("{}.{}", fmt_clock(ds / 10), ds % 10)
    }
}

/// Largo de un clip: "12.5 s" hasta el minuto (al recortar importa la décima) y "1:05" desde ahí.
fn fmt_len(t: i64) -> String {
    if t < 600_000_000 {
        format!("{:.1} s", t as f64 / 1e7)
    } else {
        fmt_clock(((t + 5_000_000) / 10_000_000) as u64)
    }
}

fn asset_card(ui: &mut egui::Ui, a: &Asset, info: Option<&library::Info>, size: Vec2, used: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let thumb = Rect::from_min_size(rect.min, egui::vec2(size.x, size.y - CARD_TEXT_H));
    let p = ui.painter();
    let mut shapes = vec![Shape::rect_filled(thumb, 6.0, CONTROL)];
    match info.and_then(|i| i.cover.as_ref()) {
        Some(t) => {
            shapes.push(Shape::image(t.id(), thumb, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE));
        }
        None => {
            let msg = if info.and_then(|i| i.error.as_ref()).is_some() { "No se puede leer" } else { "Cargando…" };
            shapes.push(ui.fonts(|f| Shape::text(f, thumb.center(), Align2::CENTER_CENTER, msg, FontId::proportional(12.0), MUTED)));
        }
    }
    if let Some(pr) = info.and_then(|i| i.probe.as_ref()) {
        let g = p.layout_no_wrap(fmt_len(pr.duration), FontId::proportional(11.5), WHITE);
        let r = Rect::from_min_size(thumb.right_bottom() - g.size() - egui::vec2(14.0, 10.0), g.size() + egui::vec2(8.0, 4.0));
        shapes.push(Shape::rect_filled(r, 4.0, BADGE));
        shapes.push(Shape::galley(r.min + egui::vec2(4.0, 2.0), g, WHITE));
    }
    if used {
        let r = Rect::from_min_size(thumb.min + egui::vec2(6.0, 6.0), egui::vec2(18.0, 18.0));
        shapes.push(Shape::circle_filled(r.center(), 9.0, WHITE));
        shapes.push(Shape::line_segment([r.center() + egui::vec2(-4.0, 0.0), r.center() + egui::vec2(-1.0, 3.5)], Stroke::new(2.0_f32, BLACK)));
        shapes.push(Shape::line_segment([r.center() + egui::vec2(-1.0, 3.5), r.center() + egui::vec2(4.5, -3.5)], Stroke::new(2.0_f32, BLACK)));
    }
    if resp.hovered() {
        shapes.push(Shape::rect_stroke(thumb, 6.0, Stroke::new(2.0_f32, WHITE)));
        shapes.push(Shape::rect_filled(thumb, 6.0, HOVER_DIM));
        shapes.push(Shape::circle_filled(thumb.center(), 18.0, WHITE));
        shapes.push(Shape::line_segment([thumb.center() - egui::vec2(7.0, 0.0), thumb.center() + egui::vec2(7.0, 0.0)], Stroke::new(2.5_f32, BLACK)));
        shapes.push(Shape::line_segment([thumb.center() - egui::vec2(0.0, 7.0), thumb.center() + egui::vec2(0.0, 7.0)], Stroke::new(2.5_f32, BLACK)));
    }
    let title = one_line(p, a.title.clone(), FontId::proportional(13.0), size.x - 4.0);
    shapes.push(Shape::galley(Pos2::new(rect.left() + 2.0, thumb.bottom() + 6.0), title, TEXT));
    let sub = format!("{}{}", fmt_bytes(a.size as f64), if a.imported { " · importado" } else { "" });
    let sub = one_line(p, sub, FontId::proportional(11.5), size.x - 4.0);
    shapes.push(Shape::galley(Pos2::new(rect.left() + 2.0, thumb.bottom() + 25.0), sub, MUTED));
    paint_pressed(ui, rect.center(), press(ui, &resp), PRESS_DEPTH, shapes);
    resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(format!("{}\nAgregar a la línea de tiempo", a.title))
}

impl EditorApp {
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        // El aviso ("Listo: …", un error) aparece con un fundido corto; al irse, se va de una.
        let shown = self.job.is_none() && self.message.is_some();
        let fade = ui.ctx().animate_bool_with_time_and_easing(egui::Id::new("message"), shown, if shown { T_FADE } else { 0.0 }, ease_out);
        ui.horizontal_centered(|ui| {
            app_mark(ui, 26.0);
            ui.label(RichText::new("Clippy").family(bold()).size(18.0).color(TEXT));
            ui.label(RichText::new("Editor").size(18.0).color(MUTED));
            ui.add_space(24.0);
            ui.add_enabled_ui(!self.undo.is_empty(), |ui| {
                let tip = format!("Deshacer ({})", shortcuts::short(&self.prefs.keys, Act::Undo));
                if icon_only_button(ui, Glyph::Undo, Kind::Ghost, &tip).clicked() {
                    self.undo();
                }
            });
            ui.add_enabled_ui(!self.redo.is_empty(), |ui| {
                let tip = format!("Rehacer ({})", shortcuts::short(&self.prefs.keys, Act::Redo));
                if icon_only_button(ui, Glyph::Redo, Kind::Ghost, &tip).clicked() {
                    self.redo();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let tip = format!("Ajustes del editor ({})", shortcuts::short(&self.prefs.keys, Act::Shortcuts));
                if gear_button(ui).on_hover_text(tip).clicked() {
                    self.show_keys = true;
                }
                ui.add_space(6.0);
                if let Some(job) = &self.job {
                    let p = job.progress.get();
                    if button(ui, "Cancelar", Kind::Secondary).clicked() {
                        job.progress.cancel.store(true, Ordering::Relaxed);
                    }
                    ui.add(
                        egui::ProgressBar::new(p)
                            .desired_width(240.0)
                            .desired_height(32.0)
                            .rounding(6.0)
                            .text(RichText::new(format!("Exportando… {:.0}%", p * 100.0)).color(BLACK)),
                    );
                } else {
                    let enabled = !self.tl.is_empty();
                    let r = ui.add_enabled_ui(enabled, |ui| icon_button(ui, Glyph::Export, "Exportar video", Kind::Primary, egui::vec2(164.0, 36.0))).inner;
                    if r.clicked() {
                        self.export();
                    }
                    if !enabled {
                        r.on_disabled_hover_text("Agregá al menos un clip a la línea de tiempo");
                    }
                    if let Some((ok, msg, out)) = self.message.clone() {
                        ui.add_space(8.0);
                        ui.scope(|ui| {
                            ui.multiply_opacity(fade);
                            if let Some(out) = out {
                                if icon_button(ui, Glyph::Folder, "Abrir carpeta", Kind::Secondary, Vec2::ZERO).clicked() {
                                    if let Some(d) = out.parent() {
                                        open_folder(d);
                                    }
                                }
                                if button(ui, "Ver video", Kind::Secondary).clicked() {
                                    let _ = std::process::Command::new("explorer").arg(&out).spawn();
                                }
                            }
                            ui.add(egui::Label::new(RichText::new(msg).color(if ok { TEXT } else { DANGER })).truncate());
                        });
                    }
                }
            });
        });
    }

    pub(super) fn library_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Biblioteca").family(bold()).size(17.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let tip = format!("Agregar videos de cualquier carpeta ({})", shortcuts::short(&self.prefs.keys, Act::Import));
                if icon_button(ui, Glyph::Plus, "Importar", Kind::Secondary, Vec2::ZERO).on_hover_text(tip).clicked() {
                    self.import_dialog();
                }
            });
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Hacé clic en un video para sumarlo a la línea de tiempo.").size(12.5).color(MUTED));
        ui.add_space(8.0);

        // Abajo de todo; la lista usa lo que queda (en un panel angosto este texto ocupa dos líneas).
        egui::TopBottomPanel::bottom("lib_hint")
            .frame(egui::Frame::none().inner_margin(egui::Margin { top: 8.0, ..Default::default() }))
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                ui.label(RichText::new("También podés arrastrar videos a la ventana.").size(12.0).color(MUTED));
            });

        let mut add: Option<PathBuf> = None;
        let mut remove: Option<PathBuf> = None;
        let mut reveal: Option<PathBuf> = None;
        if self.assets.is_empty() {
            ui.add_space(24.0);
            ui.label(RichText::new("No hay videos todavía.").color(TEXT));
            ui.label(RichText::new("Guardá un clip mientras jugás o tocá Importar.").color(MUTED));
        } else {
            let gap = 10.0;
            let w = ((ui.available_width() - gap) / 2.0).floor();
            let card = egui::vec2(w, (w * 9.0 / 16.0).round() + CARD_TEXT_H);
            let used: HashSet<&Path> = self.tl.iter().map(|p| p.path.as_path()).collect();
            // Solo se arman las filas que se ven: la carpeta puede tener cientos de clips.
            egui::ScrollArea::vertical()
                .id_salt("lib")
                .auto_shrink([false, false])
                .show_rows(ui, card.y + 6.0, self.assets.len().div_ceil(2), |ui, rows| {
                    let shown = &self.assets[rows.start * 2..(rows.end * 2).min(self.assets.len())];
                    // Portadas solo de lo que se ve. Antes de tomar `info`, que `want_cover` también bloquea.
                    for a in shown {
                        self.loader.want_cover(&a.path, (a.size, a.modified));
                    }
                    let info = self.loader.info.lock().unwrap();
                    for pair in shown.chunks(2) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for a in pair {
                                let r = asset_card(ui, a, info.get(&a.path), card, used.contains(a.path.as_path()));
                                if r.clicked() {
                                    add = Some(a.path.clone());
                                }
                                r.context_menu(|ui| {
                                    if ui.button("Agregar a la línea de tiempo").clicked() {
                                        add = Some(a.path.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button("Mostrar en la carpeta").clicked() {
                                        reveal = Some(a.path.clone());
                                        ui.close_menu();
                                    }
                                    if a.imported && ui.button("Quitar de la biblioteca").clicked() {
                                        remove = Some(a.path.clone());
                                        ui.close_menu();
                                    }
                                });
                            }
                        });
                        ui.add_space(6.0);
                    }
                });
        }
        if let Some(p) = add {
            self.add_to_timeline(&p);
        }
        if let Some(p) = remove {
            self.imported.retain(|x| *x != p);
            library::save_imported(&self.imported);
            self.scan();
        }
        if let Some(p) = reveal {
            let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
        }
    }

    pub(super) fn preview(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click());
        let p = ui.painter();
        p.rect_filled(rect, 8.0, Color32::BLACK);
        if self.tl.is_empty() {
            p.text(rect.center() - egui::vec2(0.0, 12.0), Align2::CENTER_CENTER, "Tu video aparece acá", FontId::proportional(20.0), TEXT);
            p.text(
                rect.center() + egui::vec2(0.0, 16.0),
                Align2::CENTER_CENTER,
                "Elegí un clip de la biblioteca para empezar",
                FontId::proportional(14.0),
                MUTED,
            );
            return;
        }
        let err = self
            .active
            .and_then(|i| self.tl.get(i))
            .and_then(|pc| self.players.get(&pc.path))
            .and_then(|pl| pl.shared.error.lock().unwrap().clone());
        if let Some(e) = err {
            let mut job = LayoutJob::simple(e, FontId::proportional(14.0), MUTED, (rect.width() - 48.0).clamp(120.0, 480.0));
            job.halign = Align::Center;
            let g = p.layout_job(job);
            p.galley(Pos2::new(rect.center().x, rect.center().y - g.size().y / 2.0), g, MUTED);
        } else if let Some(t) = &self.tex {
            let s = t.size_vec2();
            let scale = (rect.width() / s.x).min(rect.height() / s.y);
            let img = Rect::from_center_size(rect.center(), s * scale);
            p.image(t.id(), img, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        if resp.clicked() {
            self.toggle_play();
        }
    }

    pub(super) fn props_panel(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Clip").family(bold()).size(17.0).color(TEXT));
        ui.add_space(6.0);
        let Some(i) = self.selected.and_then(|id| self.index_of(id)) else {
            ui.label(RichText::new("Tocá un clip en la línea de tiempo para editarlo.").color(MUTED));
            ui.add_space(18.0);
            caption(ui, "Atajos");
            for act in [Act::PlayPause, Act::Split, Act::Duplicate, Act::Delete, Act::Undo, Act::PrevFrame] {
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(egui::vec2(96.0, 20.0), Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new(shortcuts::short(&self.prefs.keys, act)).color(TEXT).family(bold()).size(13.0));
                    });
                    ui.add(egui::Label::new(RichText::new(act.label()).color(MUTED).size(13.0)).truncate());
                });
            }
            ui.add_space(4.0);
            if button(ui, "Ver y cambiar atajos", Kind::Ghost).clicked() {
                self.show_keys = true;
            }
            ui.add_space(14.0);
            caption(ui, "Recortar");
            ui.label(RichText::new("Arrastrá los bordes de un clip en la línea de tiempo. Arrastralo del medio para cambiarlo de lugar.").size(13.0).color(MUTED));
            return;
        };
        let p = self.tl[i].clone();
        let start = self.starts()[i];
        ui.label(RichText::new(library::pretty_name(&p.path)).color(TEXT));
        ui.label(
            RichText::new(format!("Dura {}  ·  en la línea {}", fmt_time(p.len()), fmt_time(start)))
                .size(12.5)
                .color(MUTED),
        );
        if let Some(pr) = self.loader.probe(&p.path) {
            ui.label(RichText::new(format!("{}×{} · {:.0} fps · {}", pr.width, pr.height, pr.fps(), pr.codec())).size(12.5).color(MUTED));
        }
        ui.add_space(14.0);

        caption(ui, "Audio de este clip");
        let before = self.tl.clone();
        let mut on = !p.muted;
        let mut vol = (p.gain * 100.0).round() as u32;
        let mut changed = false;
        let mut start_edit = false;
        ui.horizontal(|ui| {
            let r = toggle(ui, &mut on).on_hover_text(if on { "Silenciar" } else { "Activar sonido" });
            if r.changed() {
                changed = true;
                start_edit = true;
            }
            ui.label(RichText::new(if on { "Con sonido" } else { "Silenciado" }).color(if on { TEXT } else { MUTED }));
        });
        ui.add_enabled_ui(on, |ui| {
            ui.spacing_mut().slider_width = ui.available_width() - 70.0;
            let r = ui.add(egui::Slider::new(&mut vol, 0..=200).suffix(" %"));
            if r.changed() {
                changed = true;
            }
            if r.drag_started() || (r.changed() && !r.dragged()) {
                start_edit = true;
            }
        });
        if changed {
            self.dirty = true;
            if start_edit {
                self.record_undo(before);
            }
            self.tl[i].muted = !on;
            self.tl[i].gain = vol as f32 / 100.0;
            self.refresh_gain(i);
        }
        ui.add_space(18.0);
        caption(ui, "Acciones");
        for act in [Act::Split, Act::Duplicate, Act::MuteClip, Act::Delete] {
            let text = format!("{}   ·   {}", act.label(), shortcuts::short(&self.prefs.keys, act));
            let glyph = match act {
                Act::Split => Glyph::Scissors,
                Act::Duplicate => Glyph::Duplicate,
                Act::MuteClip => Glyph::Speaker,
                _ => Glyph::Trash,
            };
            if button_fit(ui, Some(glyph), &text, Kind::Secondary, egui::vec2(ui.available_width(), 36.0)).clicked() {
                self.run_action(act);
            }
        }
    }

    pub(super) fn timeline_panel(&mut self, ui: &mut egui::Ui) {
        // Ocupar todo el panel: su fondo y su tamaño (el que se guarda al arrastrar el borde) son los de esto.
        ui.set_min_height(ui.available_height());
        // Barra de herramientas
        let pos = self.pos();
        let total = self.total();
        let has_sel = self.selected.and_then(|id| self.index_of(id)).is_some();
        ui.horizontal(|ui| {
            ui.set_height(40.0);
            let third = ui.available_width() / 3.0;
            ui.allocate_ui_with_layout(egui::vec2(third, 40.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add_enabled_ui(!self.tl.is_empty(), |ui| {
                    if icon_button(ui, Glyph::Scissors, "Dividir", Kind::Secondary, Vec2::ZERO).on_hover_text(format!("Cortar en el cursor ({})", shortcuts::short(&self.prefs.keys, Act::Split))).clicked() {
                        self.split();
                    }
                });
                ui.add_enabled_ui(has_sel, |ui| {
                    if icon_button(ui, Glyph::Trash, "Eliminar", Kind::Secondary, Vec2::ZERO).on_hover_text(format!("Eliminar el clip elegido ({})", shortcuts::short(&self.prefs.keys, Act::Delete))).clicked() {
                        self.delete_selected();
                    }
                });
            });
            ui.allocate_ui_with_layout(egui::vec2(third, 40.0), Layout::left_to_right(Align::Center), |ui| {
                let time = format!("{}  /  {}", fmt_time(pos), fmt_time(total));
                let tw = ui.painter().layout_no_wrap(time.clone(), FontId::proportional(15.0), TEXT).size().x;
                let group = 36.0 + 12.0 + tw;
                ui.add_space(((third - group) / 2.0).max(0.0));
                ui.add_enabled_ui(!self.tl.is_empty(), |ui| {
                    if play_button(ui, self.playing, 36.0).on_hover_text(format!("Reproducir / pausar ({})", shortcuts::short(&self.prefs.keys, Act::PlayPause))).clicked() {
                        self.toggle_play();
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(time).size(15.0).color(TEXT));
            });
            ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 40.0), Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Ajustar", Kind::Ghost).on_hover_text("Ver toda la línea de tiempo").clicked() {
                    self.run_action(Act::ZoomFit);
                }
                let tip = format!("Acercar ({})", shortcuts::short(&self.prefs.keys, Act::ZoomIn));
                if icon_only_button(ui, Glyph::Plus, Kind::Ghost, &tip).clicked() {
                    self.run_action(Act::ZoomIn);
                }
                ui.spacing_mut().slider_width = 110.0;
                let mut zoom = self.zoom;
                if ui.add(egui::Slider::new(&mut zoom, 1.0..=ZOOM_MAX).logarithmic(true).show_value(false)).changed() {
                    self.zoom_to = Some((zoom, None));
                }
                let tip = format!("Alejar ({})", shortcuts::short(&self.prefs.keys, Act::ZoomOut));
                if icon_only_button(ui, Glyph::Minus, Kind::Ghost, &tip).clicked() {
                    self.run_action(Act::ZoomOut);
                }
                ui.label(RichText::new("Zoom").size(13.0).color(MUTED));
            });
        });
        ui.add_space(6.0);

        // Pistas: ocupan el alto que le dé el usuario al panel.
        let tracks_h = (ui.available_height() - SCROLLBAR_H).max(RULER_H + 2.0 * LANE_GAP);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let (g, _) = ui.allocate_exact_size(egui::vec2(GUTTER, tracks_h), Sense::hover());
            let (video_h, audio_h) = lanes(tracks_h, self.prefs.lane_split);
            let p = ui.painter();
            let vy = g.top() + RULER_H + LANE_GAP;
            let ay = vy + video_h + LANE_GAP;
            p.text(Pos2::new(g.left() + 4.0, vy + video_h / 2.0), Align2::LEFT_CENTER, "Video", FontId::proportional(13.0), MUTED);
            p.text(Pos2::new(g.left() + 4.0, ay + audio_h / 2.0), Align2::LEFT_CENTER, "Audio", FontId::proportional(13.0), MUTED);
            self.tracks(ui, tracks_h);
        });
    }

    /// Acerca (k > 1) o aleja la línea de tiempo. Se aplica al dibujarla, con el cursor de reproducción quieto.
    pub(super) fn zoom_by(&mut self, k: f32) {
        let z = self.zoom_to.map_or(self.zoom, |(z, _)| z);
        self.zoom_to = Some((z * k, None));
    }

    fn tracks(&mut self, ui: &mut egui::Ui, tracks_h: f32) {
        let view_w = ui.available_width();
        let left = ui.cursor().min.x;
        let total = self.total();
        let total_s = (total as f32 / 1e7).max(1.0);
        let fit = ((view_w - 2.0 * PAD) / total_s).max(1.0);
        let frozen = match self.drag {
            Drag::TrimIn { pps, .. } | Drag::TrimOut { pps, .. } => Some(pps),
            _ => None,
        };

        // Zoom con Ctrl + rueda (alrededor del puntero), desplazamiento con la rueda.
        let hovered = ui.rect_contains_pointer(Rect::from_min_size(ui.cursor().min, egui::vec2(view_w, tracks_h + SCROLLBAR_H)));
        if hovered {
            let (zoom_delta, scroll_y, at) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.y, i.pointer.hover_pos().map(|p| p.x)));
            if zoom_delta != 1.0 {
                self.zoom_to = Some((self.zoom * zoom_delta, at));
            } else if scroll_y != 0.0 {
                self.set_scroll = Some((self.scroll_x - scroll_y).max(0.0));
            }
        }
        // El zoom cambia la escala dejando quieto un punto: el puntero, o el cursor de reproducción si se ve,
        // o el centro. Mientras se recorta espera: la escala está fija hasta soltar.
        if frozen.is_none() {
            if let Some((z, at)) = self.zoom_to.take() {
                let z = z.clamp(1.0, ZOOM_MAX);
                let (old, new) = (fit * self.zoom, fit * z);
                let ph = PAD + self.pos() as f32 / 1e7 * old - self.scroll_x;
                let vx = at.map_or(if (0.0..=view_w).contains(&ph) { ph } else { view_w / 2.0 }, |x| x - left);
                let s = (self.scroll_x + vx - PAD) / old;
                self.zoom = z;
                self.set_scroll = Some((PAD + s * new - vx).max(0.0));
            }
        }
        let pps = frozen.unwrap_or(fit * self.zoom);
        let mut content_w = (total_s * pps + 2.0 * PAD).max(view_w);
        if frozen.is_some() {
            // Si la línea se achicara al recortar, el desplazamiento se correría y el borde saltaría bajo el mouse.
            content_w = content_w.max(self.scroll_x + view_w);
        }
        // Seguir el cursor mientras se reproduce.
        let ph_x = PAD + self.pos() as f32 / 1e7 * pps;
        if self.playing && (ph_x > self.scroll_x + view_w - 60.0 || ph_x < self.scroll_x) {
            self.set_scroll = Some((ph_x - 80.0).max(0.0));
        }
        // Arrastrando cerca de un costado, la línea se desplaza sola: más rápido cuanto más cerca (o más afuera).
        if !matches!(self.drag, Drag::None) && ui.input(|i| i.pointer.is_decidedly_dragging()) {
            if let Some(x) = ui.input(|i| i.pointer.interact_pos()).map(|p| p.x) {
                let push = ((x - (left + view_w - EDGE)).max(0.0) - (left + EDGE - x).max(0.0)) / EDGE;
                let from = self.set_scroll.unwrap_or(self.scroll_x);
                let dt = ui.input(|i| i.stable_dt).min(0.05);
                let to = (from + push.clamp(-1.0, 1.0) * AUTO_SCROLL * dt).clamp(0.0, content_w - view_w);
                if to != from {
                    self.set_scroll = Some(to);
                    ui.ctx().request_repaint();
                }
            }
        }

        let mut sa = egui::ScrollArea::horizontal().id_salt("tracks").auto_shrink([false, true]);
        if let Some(x) = self.set_scroll.take() {
            sa = sa.horizontal_scroll_offset(x);
        }
        let out = sa.show(ui, |ui| {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(content_w, tracks_h), Sense::click_and_drag());
            self.draw_tracks(ui, rect, &resp, pps);
        });
        self.scroll_x = out.state.offset.x;
    }

    /// Dónde se ve cada pieza (x de inicio y fin), con `tx` de tiempo a x. Durante un recorte por la izquierda
    /// el borde derecho queda quieto; recién soltado un clip, cada pieza se desliza desde donde se veía.
    fn piece_xs(&mut self, ui: &egui::Ui, tx: impl Fn(i64) -> f32) -> Vec<(f32, f32)> {
        let mut xs: Vec<(f32, f32)> = self.tl.iter().zip(self.starts()).map(|(p, s)| (tx(s), tx(s + p.len()))).collect();
        if let Drag::TrimIn { i, orig, .. } = &self.drag {
            let shift = tx(self.tl[*i].src_in) - tx(orig.src_in);
            for x in xs.iter_mut().skip(*i) {
                x.0 += shift;
                x.1 += shift;
            }
        }
        if let Some((t0, from)) = self.settle.take() {
            let p = ((ui.input(|i| i.time) - t0) as f32 / T_SETTLE).min(1.0);
            if p < 1.0 && from.len() == xs.len() {
                let k = 1.0 - ease_out(p);
                for (x, d) in xs.iter_mut().zip(&from) {
                    x.0 += d * k;
                    x.1 += d * k;
                }
                self.settle = Some((t0, from));
                ui.ctx().request_repaint();
            }
        }
        xs
    }

    fn draw_tracks(&mut self, ui: &mut egui::Ui, rect: Rect, resp: &egui::Response, pps: f32) {
        let x0 = rect.left() + PAD;
        let tx = |t: i64| x0 + t as f32 / 1e7 * pps;
        let xt = |x: f32| ((x - x0) / pps * 1e7) as i64;
        let ruler = Rect::from_min_size(rect.min, egui::vec2(rect.width(), RULER_H));
        let vtop = ruler.bottom() + LANE_GAP;
        // Borde entre video y audio: se arrastra (como el de los paneles) y el alto se reparte al instante.
        // Toma el clic y el arrastre de esa franja, así no se mueve el cursor ni un clip debajo.
        let (video_h, _) = lanes(rect.height(), self.prefs.lane_split);
        let split_rect = Rect::from_min_max(Pos2::new(rect.left(), vtop + video_h - 2.0), Pos2::new(rect.right(), vtop + video_h + LANE_GAP + 2.0));
        let split = ui.interact(split_rect, ui.id().with("lane_split"), Sense::click_and_drag());
        if let Some(p) = split.interact_pointer_pos().filter(|_| split.dragged()) {
            let (v, a) = lanes(rect.height(), (p.y - vtop - LANE_GAP / 2.0) / (rect.height() - RULER_H - 2.0 * LANE_GAP).max(1.0));
            self.prefs.lane_split = v / (v + a).max(1.0);
        }
        if split.drag_stopped() {
            self.prefs.save();
        }
        let on_split = split.hovered() || split.dragged();
        let (video_h, audio_h) = lanes(rect.height(), self.prefs.lane_split);
        let vlane = Rect::from_min_size(Pos2::new(rect.left(), vtop), egui::vec2(rect.width(), video_h));
        let alane = Rect::from_min_size(Pos2::new(rect.left(), vlane.bottom() + LANE_GAP), egui::vec2(rect.width(), audio_h));
        let xs = self.piece_xs(ui, tx);

        // --- Interacción ---
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        let last = xs.len().saturating_sub(1);
        // Qué hay en (x, y): pieza y -1 = borde izquierdo, 0 = medio, 1 = borde derecho.
        let hit = |x: f32, y: f32| -> Option<(usize, i32)> {
            if y < vlane.top() || y > alane.bottom() {
                return None;
            }
            xs.iter().enumerate().find_map(|(i, &(a, b))| {
                // Entre dos clips, cada lado del corte es del clip que está de ese lado.
                let lo = if i == 0 { a - GRIP_OUT } else { a };
                let hi = if i == last { b + GRIP_OUT } else { b };
                if x < lo || x > hi {
                    return None;
                }
                let g = ((b - a) / 3.0).min(GRIP);
                Some((i, if x <= a + g { -1 } else if x >= b - g { 1 } else { 0 }))
            })
        };
        // Apretar en la regla o fuera de los clips lleva el cursor ahí en el acto, y desde ahí se arrastra.
        let pressed = resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed());
        if pressed && pointer.is_some_and(|h| hit(h.x, h.y).is_none()) {
            self.drag = Drag::Scrub;
        }
        // Un clic lleva el cursor y sigue reproduciendo; recién al arrastrar se pausa (como antes).
        if resp.drag_started() && matches!(self.drag, Drag::Scrub) {
            self.pause();
        }
        if resp.drag_started() && matches!(self.drag, Drag::None) {
            let press = ui.input(|i| i.pointer.press_origin()).or(pointer).unwrap_or_default();
            let press_x = press.x - rect.left();
            self.drag_before = Some(self.tl.clone());
            self.pause();
            self.drag = match hit(press.x, press.y) {
                Some((i, -1)) => Drag::TrimIn { i, orig: self.tl[i].clone(), press_x, pps },
                Some((i, 1)) => {
                    let max = self.loader.probe(&self.tl[i].path).map(|p| p.duration).unwrap_or(self.tl[i].src_out);
                    Drag::TrimOut { i, orig: self.tl[i].clone(), press_x, pps, max }
                }
                Some((i, _)) => Drag::Move { i, grab_dx: press.x - xs[i].0 },
                None => Drag::Scrub,
            };
            if let Drag::TrimIn { i, .. } | Drag::TrimOut { i, .. } | Drag::Move { i, .. } = &self.drag {
                self.selected = Some(self.tl[*i].id);
            }
        }
        if resp.is_pointer_button_down_on() {
            if let Some(h) = pointer {
                let throttle = self.last_seek.elapsed() > SEEK_EVERY;
                match self.drag.clone() {
                    Drag::Scrub => {
                        // El cursor sigue al mouse en cada cuadro; el video, como mucho cada SEEK_EVERY.
                        let t = xt(h.x).clamp(0, self.total());
                        if self.scrub_t != Some(t) {
                            self.scrub_t = Some(t);
                            ui.ctx().request_repaint(); // la hora de la barra se dibujó antes que esto
                        }
                        if t != self.pos_hint {
                            if pressed || throttle {
                                self.seek(t);
                                self.last_seek = Instant::now();
                            } else {
                                ui.ctx().request_repaint_after(SEEK_EVERY);
                            }
                        }
                    }
                    Drag::TrimIn { i, orig, press_x, .. } => {
                        let dt = ((h.x - rect.left() - press_x) / pps * 1e7) as i64;
                        self.tl[i].src_in = (orig.src_in + dt).clamp(0, orig.src_out - MIN_LEN);
                        if throttle {
                            self.preview_source(i, self.tl[i].src_in);
                        }
                    }
                    Drag::TrimOut { i, orig, press_x, max, .. } => {
                        let dt = ((h.x - rect.left() - press_x) / pps * 1e7) as i64;
                        self.tl[i].src_out = (orig.src_out + dt).clamp(orig.src_in + MIN_LEN, max);
                        if throttle {
                            self.preview_source(i, self.tl[i].src_out - self.frame_len(&self.tl[i].path));
                        }
                    }
                    Drag::Move { .. } | Drag::None => {}
                }
            }
        } else if !matches!(self.drag, Drag::None) {
            // Se soltó (o Esc cortó el arrastre).
            let before = self.drag_before.take();
            match std::mem::replace(&mut self.drag, Drag::None) {
                Drag::TrimIn { i, .. } => {
                    let s = self.starts()[i];
                    self.commit(before, s);
                }
                Drag::TrimOut { i, .. } => {
                    let s = self.starts()[i] + self.tl[i].len() - self.frame_len(&self.tl[i].path);
                    self.commit(before, s);
                }
                Drag::Move { i, grab_dx } => {
                    if let Some(h) = pointer {
                        let a = h.x - grab_dx;
                        let target = insert_index(&xs, i, a + (xs[i].1 - xs[i].0) / 2.0);
                        let t = if target > i { target - 1 } else { target };
                        // Dónde se ve cada pieza al soltar (la arrastrada, bajo el mouse), en el orden nuevo.
                        let mut was: Vec<f32> = xs.iter().map(|x| x.0).collect();
                        was[i] = a;
                        if t != i {
                            let p = self.tl.remove(i);
                            self.tl.insert(t, p);
                            let w = was.remove(i);
                            was.insert(t, w);
                            let s = self.starts()[t];
                            self.commit(before, s);
                        }
                        if !reduced_motion() {
                            let from = self.starts().iter().zip(&was).map(|(&s, &w)| w - tx(s)).collect();
                            self.settle = Some((ui.input(|i| i.time), from));
                        }
                    }
                }
                Drag::Scrub | Drag::None => {
                    let t = pointer.map_or(self.pos(), |h| xt(h.x)).clamp(0, self.total());
                    self.scrub_t = None;
                    if t != self.pos_hint {
                        self.seek(t);
                    }
                }
            }
        }
        let cursor = match &self.drag {
            _ if on_split => Some(CursorIcon::ResizeVertical),
            Drag::Move { .. } => Some(CursorIcon::Grabbing),
            Drag::TrimIn { .. } | Drag::TrimOut { .. } => Some(CursorIcon::ResizeHorizontal),
            Drag::Scrub | Drag::None => resp.hover_pos().and_then(|h| match hit(h.x, h.y) {
                Some((_, 0)) => Some(CursorIcon::Grab),
                Some(_) => Some(CursorIcon::ResizeHorizontal),
                None => ruler.contains(h).then_some(CursorIcon::PointingHand),
            }),
        };
        if let Some(c) = cursor {
            ui.ctx().set_cursor_icon(c);
        }
        if resp.clicked() {
            if let Some(h) = pointer {
                match hit(h.x, h.y) {
                    Some((i, _)) => self.selected = Some(self.tl[i].id),
                    None if !ruler.contains(h) => self.selected = None,
                    None => {}
                }
                let t = xt(h.x).clamp(0, self.total());
                if t != self.pos_hint {
                    self.seek(t);
                }
            }
        }

        // --- Dibujo ---
        // Soltar pudo cambiar la línea (orden, largos): se dibuja como quedó.
        let xs = self.piece_xs(ui, tx);
        let painter = ui.painter_at(rect);
        // Regla
        let step = [0.5f32, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0]
            .into_iter()
            .find(|s| s * pps >= 70.0)
            .unwrap_or(600.0);
        let mut s = 0.0f32;
        let end_s = (rect.width() - PAD) / pps;
        while s <= end_s {
            let x = x0 + s * pps;
            painter.line_segment([Pos2::new(x, ruler.bottom() - 8.0), Pos2::new(x, ruler.bottom())], Stroke::new(1.0_f32, MUTED));
            painter.text(Pos2::new(x + 4.0, ruler.top() + 2.0), Align2::LEFT_TOP, fmt_ruler(s), FontId::proportional(11.0), MUTED);
            let half = x + step * pps / 2.0;
            painter.line_segment([Pos2::new(half, ruler.bottom() - 4.0), Pos2::new(half, ruler.bottom())], Stroke::new(1.0_f32, LINE));
            s += step;
        }
        painter.hline(rect.x_range(), ruler.bottom(), Stroke::new(1.0_f32, LINE));

        if self.tl.is_empty() {
            let r = Rect::from_min_max(Pos2::new(x0, vlane.top()), Pos2::new(rect.right() - PAD, alane.bottom()));
            painter.rect_stroke(r, 8.0, Stroke::new(1.0_f32, CONTROL_HOVER));
            painter.text(r.center(), Align2::CENTER_CENTER, "Hacé clic en un video de la biblioteca o arrastralo acá", FontId::proportional(14.0), MUTED);
            return;
        }

        let info = self.loader.info.clone();
        let info = info.lock().unwrap();
        let moving = if let Drag::Move { i, .. } = self.drag { Some(i) } else { None };
        for (i, p) in self.tl.iter().enumerate() {
            if Some(i) == moving {
                continue;
            }
            let (a, b) = xs[i];
            draw_piece(&painter, p, info.get(&p.path), a, b, vlane, alane, pps, self.selected == Some(p.id), 1.0);
        }
        // Pieza que se está moviendo: sigue al mouse y una barra marca dónde va a quedar.
        if let (Some(i), Some(h)) = (moving, pointer) {
            if let Drag::Move { grab_dx, .. } = self.drag {
                let w = xs[i].1 - xs[i].0;
                let a = h.x - grab_dx;
                let target = insert_index(&xs, i, a + w / 2.0);
                let bar_x = if target < xs.len() { xs[target].0 } else { xs.last().map(|x| x.1).unwrap_or(x0) };
                painter.rect_filled(Rect::from_center_size(Pos2::new(bar_x - 1.0, (vlane.top() + alane.bottom()) / 2.0), egui::vec2(3.0, alane.bottom() - vlane.top() + 6.0)), 1.5, WHITE);
                let p = &self.tl[i];
                let lift = egui::vec2(0.0, -4.0);
                draw_piece(&painter, p, info.get(&p.path), a, a + w, vlane.translate(lift), alane.translate(lift), pps, true, 0.75);
            }
        }

        // El borde entre pistas se marca con el mouse encima (sin fundido) y más claro mientras se arrastra.
        if on_split {
            let y = vlane.bottom() + LANE_GAP / 2.0;
            painter.hline(rect.x_range(), y, Stroke::new(2.0_f32, if split.dragged() { TEXT } else { BORDER }));
        }

        // Cursor de reproducción
        let px = tx(self.pos());
        painter.line_segment([Pos2::new(px, ruler.top() + 6.0), Pos2::new(px, alane.bottom())], Stroke::new(2.0_f32, WHITE));
        painter.rect_filled(Rect::from_center_size(Pos2::new(px, ruler.top() + 8.0), egui::vec2(12.0, 14.0)), 3.0, WHITE);
    }
}
