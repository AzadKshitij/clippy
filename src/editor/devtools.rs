//! Pruebas del editor sin ventana (solo con `--features devtools`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use super::media::{self, Progress, Segment};
use crate::mf::Mf;
use super::player::{self, Player};

/// Pruebas sin ventana:
/// `--test-export <salida.mp4> <archivo> <ini_s> <fin_s> [<archivo> <ini_s> <fin_s> ...]`
/// `--test-frame <archivo> <t_s> <salida.ppm>`
pub fn test_cli(args: &[String]) -> i32 {
    let _mf = Mf::init();
    let secs = |s: &String| (s.parse::<f64>().unwrap_or(0.0) * 1e7) as i64;
    match args.first().map(|s| s.as_str()) {
        Some("--test-export") => {
            let out = PathBuf::from(&args[1]);
            // Cada parte: <archivo> <ini_s> <fin_s> <volumen 0..2>
            let segs: Vec<Segment> = args[2..]
                .chunks(4)
                .map(|c| Segment {
                    path: PathBuf::from(&c[0]),
                    name: c[0].clone(),
                    start: secs(&c[1]),
                    end: secs(&c[2]),
                    gain: c.get(3).and_then(|g| g.parse().ok()).unwrap_or(1.0),
                })
                .collect();
            let cfg = crate::config::load();
            let target = std::env::var("CLIPPY_TARGET").ok().map(|v| {
                let p: Vec<&str> = v.split(',').collect();
                media::Target {
                    height: p.first().and_then(|x| x.parse().ok()).unwrap_or(0),
                    fps: p.get(1).and_then(|x| x.parse().ok()).unwrap_or(0),
                    bitrate: (p.get(2).and_then(|x| x.parse::<f64>().ok()).unwrap_or(8.0) * 1e6) as u32,
                    constant: p.get(3) == Some(&"c"),
                }
            });
            let opts = media::ExportOpts { bitrate: cfg.bitrate_mbps * 1_000_000, target };
            let t = Instant::now();
            match media::export(&segs, &out, &opts, &Progress::new()) {
                Ok(m) => {
                    println!("OK {m} en {:.2} s", t.elapsed().as_secs_f64());
                    0
                }
                Err(e) => {
                    println!("ERROR {e:?}");
                    1
                }
            }
        }
        Some("--test-strip") => {
            let path = PathBuf::from(&args[1]);
            let t = Instant::now();
            let pr = media::probe(&path).unwrap();
            println!("probe: {:?}", t.elapsed());
            let t = Instant::now();
            let w = media::waveform(&path).unwrap();
            println!("forma de onda ({} valores): {:?}", w.len(), t.elapsed());
            let t = Instant::now();
            let k = media::keyframes(&path).unwrap();
            println!("fotogramas clave ({}): {:?}", k.len(), t.elapsed());
            let t = Instant::now();
            let c = crate::clipcache::load(&path);
            println!("caché: {} en {:?}", if c.is_some() { "encontrada" } else { "no hay" }, t.elapsed());
            let t = Instant::now();
            let mut v = player::Video::open(&path, 160, 90).unwrap();
            println!("abrir decodificador: {:?}", t.elapsed());
            let t = Instant::now();
            let mut n = 0;
            let mut s = 0;
            while s < pr.duration {
                if v.frame_at(s).ok().flatten().is_some() {
                    n += 1;
                }
                s += 10_000_000;
            }
            println!("{n} miniaturas (GPU): {:?}", t.elapsed());
            let t = Instant::now();
            let mut v = player::Video::open_cpu(&path, 160, 90).unwrap();
            println!("abrir decodificador CPU: {:?}", t.elapsed());
            let t = Instant::now();
            let mut s = 0;
            n = 0;
            while s < pr.duration {
                if v.frame_at(s).ok().flatten().is_some() {
                    n += 1;
                }
                s += 10_000_000;
            }
            println!("{n} miniaturas (CPU): {:?}", t.elapsed());
            0
        }
        Some("--test-frame") => {
            let path = PathBuf::from(&args[1]);
            let probe = media::probe(&path).unwrap();
            let p = Player::open(path, &probe, egui::Context::default());
            std::thread::sleep(Duration::from_millis(800));
            p.seek(secs(&args[2]));
            std::thread::sleep(Duration::from_millis(800));
            if let Some(e) = p.shared.error.lock().unwrap().clone() {
                println!("ERROR {e}");
                return 1;
            }
            let f = p.shared.frame.lock().unwrap().clone();
            match f {
                Some(f) => {
                    let [w, h] = f.size;
                    let mut ppm = format!("P6 {w} {h} 255\n").into_bytes();
                    for px in &f.pixels {
                        ppm.extend_from_slice(&[px.r(), px.g(), px.b()]);
                    }
                    std::fs::write(&args[3], ppm).unwrap();
                    println!("OK {w}x{h}");
                    0
                }
                None => {
                    println!("ERROR sin cuadro");
                    1
                }
            }
        }
        _ => 2,
    }
}
