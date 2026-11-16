//! Lectura de clips y exportación (cortar/unir) con Media Foundation. Sin FFmpeg.
//!
//! - Modo copia: los paquetes H.264/HEVC y AAC se copian tal cual (instantáneo, sin pérdida).
//!   El inicio de cada parte cae en el fotograma clave anterior.
//! - Modo recodificar: decodifica y vuelve a codificar (por GPU). Corte exacto al cuadro,
//!   y permite unir clips de distinta resolución o formato.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use windows::core::{Interface, HSTRING, GUID, PROPVARIANT};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;

use crate::clipcache::{Wave, WaveBuilder};
use crate::mf::{aac_type, frames_at, pcm_sample, pcm_type, sample_bytes, time_of, FRAME_BYTES};

pub const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
pub const AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
pub const ANY: u32 = MF_SOURCE_READER_ANY_STREAM.0 as u32;
pub const SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
pub const EOS: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

pub unsafe fn reader(path: &Path, video_processing: bool, d3d: Option<&IMFDXGIDeviceManager>) -> Result<IMFSourceReader> {
    let mut a = None;
    MFCreateAttributes(&mut a, 4)?;
    let a = a.unwrap();
    if video_processing {
        a.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    }
    if let Some(m) = d3d {
        a.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, m)?;
        a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    }
    Ok(MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &a)?)
}

/// Dispositivo de la GPU para Media Foundation: decodificar, escalar y codificar sin pasar por la CPU.
pub unsafe fn d3d_manager() -> Result<IMFDXGIDeviceManager> {
    let mut device = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
    )?;
    let device = device.ok_or_else(|| anyhow!("sin D3D11"))?;
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let mut token = 0u32;
    let mut mgr = None;
    MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
    let mgr = mgr.unwrap();
    mgr.ResetDevice(&device, token)?;
    Ok(mgr)
}

pub unsafe fn seek(r: &IMFSourceReader, t: i64) -> Result<()> {
    r.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(t.max(0)))?;
    Ok(())
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

pub fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

/// Lo que se sabe de un archivo sin decodificarlo.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub duration: i64,
    pub width: u32,
    pub height: u32,
    /// Cuadros por segundo como fracción (60/1, 30000/1001...). 0/0 si el archivo no lo dice.
    pub frame_rate: (u32, u32),
    /// Formato del video (H.264, HEVC...).
    pub subtype: GUID,
    /// Encabezado del video (SPS/PPS): si cambia entre dos clips no se pueden unir sin recodificar.
    pub seq: Vec<u8>,
    /// Formato del audio (tipo, muestras por segundo, canales), si tiene.
    pub audio: Option<(GUID, u32, u32)>,
}

impl Probe {
    pub fn fps(&self) -> f64 {
        let (n, d) = self.frame_rate;
        if d > 0 {
            n as f64 / d as f64
        } else {
            0.0
        }
    }

    pub fn codec(&self) -> &'static str {
        if self.subtype == MFVideoFormat_H264 {
            "H.264"
        } else if self.subtype == MFVideoFormat_HEVC {
            "HEVC"
        } else {
            "otro"
        }
    }

    /// Mismo formato de video y de audio: los clips se pueden unir copiando los paquetes.
    pub fn same_format(&self, o: &Probe) -> bool {
        self.subtype == o.subtype && (self.width, self.height) == (o.width, o.height) && self.seq == o.seq && self.audio == o.audio
    }
}

pub fn probe(path: &Path) -> Result<Probe> {
    unsafe {
        let r = reader(path, false, None)?;
        let duration = r
            .GetPresentationAttribute(SOURCE, &MF_PD_DURATION)
            .ok()
            .and_then(|p| u64::try_from(&p).ok())
            .unwrap_or(0) as i64;
        let vt = r.GetNativeMediaType(VIDEO, 0).map_err(|_| anyhow!("el archivo no tiene video"))?;
        let (width, height) = unpack(vt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0));
        let mut seq = Vec::new();
        if let Ok(n) = vt.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
            seq = vec![0u8; n as usize];
            let _ = vt.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut seq, None);
        }
        let audio = r.GetNativeMediaType(AUDIO, 0).ok().map(|at| {
            (
                at.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default(),
                at.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(0),
                at.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(0),
            )
        });
        Ok(Probe {
            duration,
            width,
            height,
            frame_rate: unpack(vt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0)),
            subtype: vt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default(),
            seq,
            audio,
        })
    }
}

/// Marcas de tiempo de los fotogramas clave. En MP4 se leen del índice del archivo
/// (milisegundos); si no se puede, se recorren los cuadros sin decodificarlos.
pub fn keyframes(path: &Path) -> Result<Vec<i64>> {
    if let Some(k) = mp4_keyframes(path) {
        return Ok(k);
    }
    unsafe {
        let r = reader(path, false, None)?;
        r.SetStreamSelection(ANY, false)?;
        r.SetStreamSelection(VIDEO, true)?;
        let mut v = Vec::new();
        loop {
            let (mut flags, mut ts, mut s) = (0u32, 0i64, None);
            r.ReadSample(VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))?;
            if let Some(s) = s {
                if s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 {
                    v.push(ts);
                }
            }
            if flags & EOS != 0 {
                break;
            }
        }
        if v.is_empty() {
            v.push(0);
        }
        Ok(v)
    }
}

// --- Exportación -------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Segment {
    pub path: PathBuf,
    pub name: String,
    pub start: i64,
    pub end: i64,
    /// Volumen del audio de esta parte (0 = mudo, 1 = normal).
    pub gain: f32,
}

pub struct Progress {
    bits: AtomicU32,
    pub cancel: AtomicBool,
    pub result: Mutex<Option<Result<String, String>>>,
}

impl Progress {
    pub fn new() -> Self {
        Self { bits: AtomicU32::new(0), cancel: AtomicBool::new(false), result: Mutex::new(None) }
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
    fn set(&self, v: f32) {
        self.bits.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("cancelado");
        }
        Ok(())
    }
}

pub struct ExportOpts {
    /// Bitrate de video si hay que renderizar sin un destino elegido.
    pub bitrate: u32,
    /// Formato elegido por el usuario. Si está, siempre se renderiza con estos valores.
    pub target: Option<Target>,
}

#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// Alto en píxeles (el ancho se calcula manteniendo la proporción). 0 = original.
    pub height: u32,
    /// Cuadros por segundo. 0 = original.
    pub fps: u32,
    /// Bitrate de video (bits/s).
    pub bitrate: u32,
    /// Bitrate constante (para no pasarse de un tamaño máximo).
    pub constant: bool,
}

/// ¿`t` cae justo en uno de los fotogramas clave `keys`? (entonces se puede cortar ahí sin recodificar)
pub fn on_keyframe(keys: &[i64], t: i64) -> bool {
    keys.iter().any(|&x| (x - t).abs() <= 2_000)
}

/// Exporta las partes a `out` con cortes exactos. Si todos los cortes caen en fotogramas clave
/// y los clips tienen el mismo formato, copia el video sin recodificar (instantáneo, sin pérdida).
/// Devuelve una descripción del modo usado.
pub fn export(segs: &[Segment], out: &Path, opts: &ExportOpts, prog: &Progress) -> Result<String> {
    if segs.is_empty() {
        bail!("no hay nada para exportar");
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = out.with_extension("clippy-tmp.mp4");
    let _ = std::fs::remove_file(&tmp);
    let r = unsafe {
        let infos = segs.iter().map(|s| probe(&s.path)).collect::<Result<Vec<_>>>()?;
        let compatible = infos.windows(2).all(|w| w[0].same_format(&w[1]));
        let mut keys = std::collections::HashMap::new();
        let aligned = segs.iter().all(|s| {
            let k = keys.entry(&s.path).or_insert_with(|| keyframes(&s.path).unwrap_or_default());
            on_keyframe(k, s.start)
        });
        let unity = segs.iter().all(|s| (s.gain - 1.0).abs() < 0.001);
        if opts.target.is_none() && compatible && aligned {
            export_copy(segs, &infos[0], &tmp, prog, !unity).map(|_| {
                if unity { "sin recodificar: instantáneo y sin pérdida" } else { "video sin recodificar, audio ajustado" }.to_string()
            })
        } else {
            let why = if opts.target.is_some() { "renderizado con la GPU" } else { "renderizado con la GPU para cortar exacto" };
            // Todo en la GPU; si esa placa o ese archivo no se dejan, se decodifica por CPU.
            let gpu = d3d_manager();
            match &gpu {
                Ok(m) => export_reencode(segs, &infos, &tmp, opts, prog, Some(m)).or_else(|e| {
                    prog.check()?;
                    crate::log::log(&format!("exportar: por GPU no se pudo ({e:?}), se decodifica por CPU"));
                    let _ = std::fs::remove_file(&tmp);
                    export_reencode(segs, &infos, &tmp, opts, prog, None)
                }),
                Err(_) => export_reencode(segs, &infos, &tmp, opts, prog, None),
            }
            .map(|_| why.to_string())
        }
    };
    match r {
        Ok(m) => {
            let _ = std::fs::remove_file(out);
            std::fs::rename(&tmp, out)?;
            prog.set(1.0);
            Ok(m)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// `hardware`: encoder por GPU. `d3d`: las imágenes llegan ya en la GPU; ahí Media Foundation frena
/// la lectura si el encoder va atrasado (si no, se acumularían cuadros en la memoria de video).
unsafe fn sink(out: &Path, hardware: bool, d3d: Option<&IMFDXGIDeviceManager>) -> Result<IMFSinkWriter> {
    let mut a = None;
    MFCreateAttributes(&mut a, 3)?;
    let a = a.unwrap();
    a.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
    match d3d {
        Some(m) => a.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, m)?,
        None => a.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?,
    }
    if hardware {
        a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    }
    Ok(MFCreateSinkWriterFromURL(&HSTRING::from(out.as_os_str()), None, &a)?)
}

unsafe fn clone_type(t: &IMFMediaType) -> Result<IMFMediaType> {
    let n = MFCreateMediaType()?;
    t.CopyAllItems(&n)?;
    Ok(n)
}

/// Índice real (0, 1, ...) del primer stream de video del lector.
unsafe fn video_index(r: &IMFSourceReader) -> u32 {
    for i in 0..8 {
        if let Ok(t) = r.GetNativeMediaType(i, 0) {
            if t.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Video) {
                return i;
            }
        }
    }
    0
}

/// Copia el video tal cual. El audio también se copia, salvo que `audio_pcm` pida
/// decodificarlo para aplicar el volumen de cada parte.
unsafe fn export_copy(segs: &[Segment], info: &Probe, out: &Path, prog: &Progress, audio_pcm: bool) -> Result<()> {
    let first = reader(&segs[0].path, false, None)?;
    let vt = clone_type(&first.GetNativeMediaType(VIDEO, 0)?)?;
    let at = match info.audio {
        Some(_) => Some(clone_type(&first.GetNativeMediaType(AUDIO, 0)?)?),
        None => None,
    };
    drop(first);

    let w = sink(out, false, None)?;
    let vs = w.AddStream(&vt)?;
    w.SetInputMediaType(vs, &vt, None)?;
    let pcm_stream = match (&at, audio_pcm) {
        (Some(_), true) => {
            let s = w.AddStream(&aac_type()?)?;
            w.SetInputMediaType(s, &pcm_type()?, None)?;
            Some(s)
        }
        _ => None,
    };
    let a_s = match (&at, audio_pcm) {
        (Some(t), false) => {
            let s = w.AddStream(t)?;
            w.SetInputMediaType(s, t, None)?;
            Some(s)
        }
        _ => None,
    };
    w.BeginWriting()?;

    let total: i64 = segs.iter().map(|s| (s.end - s.start).max(1)).sum();
    let mut offset = 0i64;
    let mut done = 0i64;
    for seg in segs {
        let r = reader(&seg.path, false, None)?;
        let vidx = video_index(&r);
        if a_s.is_none() {
            let _ = r.SetStreamSelection(AUDIO, false);
        }
        // Ubicar el fotograma clave donde arranca la parte y volver a posicionarse ahí,
        // así el audio también empieza en ese punto (si no, arrancaría unos ms más tarde).
        seek(&r, seg.start)?;
        loop {
            let (mut flags, mut ts, mut s) = (0u32, 0i64, None);
            r.ReadSample(VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))?;
            if let Some(s) = s {
                if s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 {
                    seek(&r, ts)?;
                    break;
                }
            }
            if flags & EOS != 0 {
                seek(&r, seg.start)?;
                break;
            }
        }
        let mut base: Option<i64> = None;
        let mut pending_audio: Vec<(IMFSample, i64)> = Vec::new();
        let (mut vdone, mut adone) = (false, a_s.is_none());
        let mut seg_end = seg.start;
        while !(vdone && adone) {
            prog.check()?;
            let (mut idx, mut flags, mut ts, mut s) = (0u32, 0u32, 0i64, None);
            r.ReadSample(ANY, 0, Some(&mut idx), Some(&mut flags), Some(&mut ts), Some(&mut s))?;
            let is_video = idx == vidx;
            if flags & EOS != 0 {
                if is_video {
                    vdone = true;
                } else {
                    adone = true;
                }
            }
            let Some(s) = s else { continue };
            if is_video {
                if vdone {
                    continue;
                }
                if base.is_none() {
                    if s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 0 {
                        continue;
                    }
                    base = Some(ts);
                    if let Some(a_s) = a_s {
                        for (asmp, ats) in pending_audio.drain(..) {
                            if ats >= ts && ats < seg.end {
                                asmp.SetSampleTime(ats - ts + offset)?;
                                w.WriteSample(a_s, &asmp)?;
                            }
                        }
                    }
                }
                if ts >= seg.end {
                    vdone = true;
                    continue;
                }
                let b = base.unwrap();
                let dur = s.GetSampleDuration().unwrap_or(0);
                s.SetSampleTime(ts - b + offset)?;
                w.WriteSample(vs, &s)?;
                seg_end = seg_end.max(ts + dur);
                prog.set((done + (ts - seg.start).max(0)) as f32 / total as f32);
            } else if let Some(a_s) = a_s {
                if adone {
                    continue;
                }
                match base {
                    None => pending_audio.push((s, ts)),
                    Some(b) => {
                        if ts >= seg.end {
                            adone = true;
                        } else if ts >= b {
                            s.SetSampleTime(ts - b + offset)?;
                            w.WriteSample(a_s, &s)?;
                        }
                    }
                }
            }
        }
        let b = base.ok_or_else(|| anyhow!("no se encontró video en {}", seg.name))?;
        if let Some(ps) = pcm_stream {
            write_audio_range(&w, ps, &seg.path, b, seg.end, offset, seg.gain, prog)?;
        }
        offset += (seg_end - b).max(0);
        done += (seg.end - seg.start).max(1);
    }
    w.Finalize()?;
    Ok(())
}

/// Decodifica el audio de [from, to) del archivo, le aplica `gain` y lo escribe desde `offset`.
#[allow(clippy::too_many_arguments)]
unsafe fn write_audio_range(w: &IMFSinkWriter, stream: u32, path: &Path, from: i64, to: i64, offset: i64, gain: f32, prog: &Progress) -> Result<()> {
    let r = reader(path, false, None)?;
    r.SetStreamSelection(ANY, false)?;
    r.SetStreamSelection(AUDIO, true)?;
    r.SetCurrentMediaType(AUDIO, None, &pcm_type()?)?;
    seek(&r, from)?;
    loop {
        prog.check()?;
        let (mut flags, mut ts, mut s) = (0u32, 0i64, None);
        r.ReadSample(AUDIO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))?;
        if let Some(s) = s {
            if ts >= to {
                break;
            }
            write_pcm_part(w, stream, &s, ts, (from, to), offset, gain)?;
        }
        if flags & EOS != 0 {
            break;
        }
    }
    Ok(())
}

/// Escribe la parte de la muestra PCM `s` (que empieza en `ts`) que cae en `[from, to)`, con el
/// volumen `gain`, ubicada a partir de `offset` en la salida.
unsafe fn write_pcm_part(w: &IMFSinkWriter, stream: u32, s: &IMFSample, ts: i64, (from, to): (i64, i64), offset: i64, gain: f32) -> Result<()> {
    let pcm = sample_bytes(s)?;
    let frames = pcm.len() / FRAME_BYTES;
    let skip = (frames_at((from - ts).max(0)) as usize).min(frames);
    let keep = (frames_at((to - ts).max(0)) as usize).min(frames);
    if keep > skip {
        let part = apply_gain(&pcm[skip * FRAME_BYTES..keep * FRAME_BYTES], gain);
        w.WriteSample(stream, &pcm_sample(&part, ts.max(from) - from + offset)?)?;
    }
    Ok(())
}

fn apply_gain(pcm: &[u8], gain: f32) -> Vec<u8> {
    if (gain - 1.0).abs() < 0.001 {
        return pcm.to_vec();
    }
    pcm.as_chunks::<2>()
        .0
        .iter()
        .flat_map(|&c| {
            let v = (i16::from_le_bytes(c) as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            v.to_le_bytes()
        })
        .collect()
}

/// Forma de onda del audio del archivo (decodifica solo el audio, una vez; después va a la caché).
pub fn waveform(path: &Path) -> Result<Wave> {
    unsafe {
        let r = reader(path, false, None)?;
        r.SetStreamSelection(ANY, false)?;
        r.SetStreamSelection(AUDIO, true)?;
        r.SetCurrentMediaType(AUDIO, None, &pcm_type()?)?;
        let mut w = WaveBuilder::new();
        loop {
            let (mut flags, mut ts, mut s) = (0u32, 0i64, None);
            r.ReadSample(AUDIO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))?;
            if let Some(s) = s {
                // Rellenar huecos (por ejemplo si el audio empieza más tarde).
                w.pad_to_time(ts);
                for c in sample_bytes(&s)?.as_chunks::<FRAME_BYTES>().0 {
                    w.push(i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]]));
                }
            }
            if flags & EOS != 0 {
                break;
            }
        }
        Ok(w.finish())
    }
}

/// Decodifica y vuelve a codificar. Con `gpu`, los cuadros no salen de la placa de video (la GPU
/// decodifica, escala y codifica); sin ella se decodifica por CPU y se codifica por GPU.
unsafe fn export_reencode(segs: &[Segment], infos: &[Probe], out: &Path, opts: &ExportOpts, prog: &Progress, gpu: Option<&IMFDXGIDeviceManager>) -> Result<()> {
    let (sw, sh) = (infos[0].width.max(2), infos[0].height.max(2));
    let src_rate = if infos[0].fps() > 0.0 { infos[0].frame_rate } else { (60, 1) };
    let target = opts.target.unwrap_or(Target { height: 0, fps: 0, bitrate: opts.bitrate, constant: false });
    // Tamaño de salida: el alto elegido (nunca más grande que el original) y el ancho proporcional.
    let th = (if target.height == 0 || target.height >= sh { sh } else { target.height }) & !1;
    let tw = ((sw as u64 * th as u64 / sh as u64) as u32).max(2) & !1;
    let fps = if target.fps == 0 || (target.fps as f64) >= src_rate.0 as f64 / src_rate.1 as f64 - 0.5 {
        src_rate
    } else {
        (target.fps, 1)
    };
    let has_audio = infos.iter().any(|i| i.audio.is_some());

    let w = sink(out, true, gpu)?;
    let vo = MFCreateMediaType()?;
    vo.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    vo.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    vo.SetUINT32(&MF_MT_AVG_BITRATE, target.bitrate)?;
    vo.SetUINT64(&MF_MT_FRAME_SIZE, pack(tw, th))?;
    vo.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.0, fps.1.max(1)))?;
    vo.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    vo.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    vo.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    let vs = w.AddStream(&vo)?;
    let vi = MFCreateMediaType()?;
    vi.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    vi.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    vi.SetUINT64(&MF_MT_FRAME_SIZE, pack(tw, th))?;
    vi.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.0, fps.1.max(1)))?;
    vi.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    vi.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    // Control de bitrate del encoder: constante para respetar un tamaño máximo, variable para calidad.
    let mut enc = None;
    MFCreateAttributes(&mut enc, 2)?;
    let enc = enc.unwrap();
    let mode = if target.constant { eAVEncCommonRateControlMode_CBR } else { eAVEncCommonRateControlMode_UnconstrainedVBR };
    enc.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, mode.0 as u32)?;
    enc.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, target.bitrate)?;
    if w.SetInputMediaType(vs, &vi, &enc).is_err() {
        w.SetInputMediaType(vs, &vi, None)?;
    }

    let a_s = if has_audio {
        let s = w.AddStream(&aac_type()?)?;
        w.SetInputMediaType(s, &pcm_type()?, None)?;
        Some(s)
    } else {
        None
    };
    w.BeginWriting()?;

    let total: i64 = segs.iter().map(|s| (s.end - s.start).max(1)).sum();
    let mut offset = 0i64;
    let mut done = 0i64;
    // El video recodificado sale a cuadros fijos: cada marca se ajusta a la grilla y nunca se repite.
    let fdur = 10_000_000 * fps.1.max(1) as i64 / fps.0.max(1) as i64;
    let mut last_vt = -fdur;
    for (seg, info) in segs.iter().zip(infos) {
        let r = reader(&seg.path, true, gpu)?;
        let vidx = video_index(&r);
        r.SetStreamSelection(ANY, false)?;
        r.SetStreamSelection(VIDEO, true)?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(tw, th))?;
        if r.SetCurrentMediaType(VIDEO, None, &t).is_err() {
            if info.subtype == MFVideoFormat_HEVC {
                bail!("para recodificar clips HEVC hace falta instalar \"Extensiones de video HEVC\" (Microsoft Store)");
            }
            bail!("no se pudo decodificar {}", seg.name);
        }
        // Alto real de los cuadros que entrega el decodificador (puede ser 1088 en vez de 1080).
        let cur = r.GetCurrentMediaType(VIDEO)?;
        let alloc_h = unpack(cur.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(pack(tw, th))).1.max(th);
        let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|v| v as i32).unwrap_or(tw as i32);
        let seg_audio = a_s.is_some() && info.audio.is_some();
        if seg_audio {
            r.SetStreamSelection(AUDIO, true)?;
            r.SetCurrentMediaType(AUDIO, None, &pcm_type()?)?;
        }
        seek(&r, seg.start)?;
        let (mut vdone, mut adone) = (false, !seg_audio);
        while !(vdone && adone) {
            prog.check()?;
            let (mut idx, mut flags, mut ts, mut s) = (0u32, 0u32, 0i64, None);
            r.ReadSample(ANY, 0, Some(&mut idx), Some(&mut flags), Some(&mut ts), Some(&mut s))?;
            let is_video = idx == vidx;
            if flags & EOS != 0 {
                if is_video {
                    vdone = true;
                } else {
                    adone = true;
                }
            }
            let Some(s) = s else { continue };
            if is_video {
                if vdone || ts < seg.start {
                    continue;
                }
                if ts >= seg.end {
                    vdone = true;
                    continue;
                }
                // Cada cuadro va a la grilla de la salida. Si dos caen en el mismo lugar
                // (por ejemplo al pasar de 60 a 30 fps) se descarta el sobrante.
                let exact = ts - seg.start + offset;
                let mut t = (exact + fdur / 2) / fdur * fdur;
                if t <= last_vt {
                    if exact > last_vt + fdur * 3 / 4 {
                        t = last_vt + fdur;
                    } else {
                        continue;
                    }
                }
                last_vt = t;
                // En la GPU el cuadro va directo al encoder; por CPU se copia a un búfer compacto.
                let s = if gpu.is_some() { s } else { nv12_packed(&s, tw, th, alloc_h, stride)? };
                s.SetSampleTime(t)?;
                s.SetSampleDuration(fdur)?;
                w.WriteSample(vs, &s)?;
                prog.set((done + ts - seg.start) as f32 / total as f32);
            } else if !adone {
                let dur = s.GetSampleDuration().unwrap_or(0);
                if ts >= seg.end {
                    adone = true;
                    continue;
                }
                if ts + dur <= seg.start {
                    continue;
                }
                if let Some(a) = a_s {
                    write_pcm_part(&w, a, &s, ts, (seg.start, seg.end), offset, seg.gain)?;
                }
            }
        }
        if let (Some(a), false) = (a_s, seg_audio) {
            // Este clip no tiene audio: silencio para no desincronizar los siguientes.
            let frames = frames_at(seg.end - seg.start);
            let mut f = 0;
            while f < frames {
                let n = 4800.min(frames - f);
                w.WriteSample(a, &pcm_sample(&vec![0u8; n as usize * FRAME_BYTES], offset + time_of(f))?)?;
                f += n;
            }
        }
        offset += seg.end - seg.start;
        done += (seg.end - seg.start).max(1);
    }
    w.Finalize()?;
    Ok(())
}

// --- Índice MP4 (moov/trak/.../stss + stts) -------------------------------------------

fn mp4_keyframes(path: &Path) -> Option<Vec<i64>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut pos = 0u64;
    // Buscar la caja "moov" entre las de primer nivel sin leer los datos de video.
    let moov = loop {
        if pos + 8 > len {
            return None;
        }
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8]).ok()?;
        let mut size = u32::from_be_bytes(h[..4].try_into().ok()?) as u64;
        let mut header = 8u64;
        if size == 1 {
            f.read_exact(&mut h[8..16]).ok()?;
            size = u64::from_be_bytes(h[8..16].try_into().ok()?);
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header {
            return None;
        }
        if &h[4..8] == b"moov" {
            let mut buf = vec![0u8; (size - header) as usize];
            f.read_exact(&mut buf).ok()?;
            break buf;
        }
        pos += size;
    };
    for trak in boxes(&moov).filter(|(t, _)| t == b"trak").map(|(_, b)| b) {
        let mdia = child(trak, b"mdia")?;
        let hdlr = child(mdia, b"hdlr")?;
        if hdlr.get(8..12)? != b"vide" {
            continue;
        }
        let mdhd = child(mdia, b"mdhd")?;
        let timescale = if mdhd[0] == 1 { be32(mdhd, 20)? } else { be32(mdhd, 12)? } as i64;
        let stbl = child(child(mdia, b"minf")?, b"stbl")?;
        let stts = child(stbl, b"stts")?;
        // Tiempo (en unidades del track) de cada cuadro.
        let mut times = Vec::new();
        let mut t = 0i64;
        for i in 0..be32(stts, 4)? as usize {
            let count = be32(stts, 8 + i * 8)?;
            let delta = be32(stts, 12 + i * 8)? as i64;
            for _ in 0..count {
                times.push(t);
                t += delta;
            }
        }
        let to_100ns = |x: i64| x * 10_000_000 / timescale.max(1);
        let keys: Vec<i64> = match child(stbl, b"stss") {
            Some(stss) => (0..be32(stss, 4)? as usize)
                .filter_map(|i| be32(stss, 8 + i * 4))
                .filter_map(|n| times.get((n as usize).checked_sub(1)?).copied())
                .map(to_100ns)
                .collect(),
            None => times.iter().copied().map(to_100ns).collect(), // sin tabla: todos son clave
        };
        return (!keys.is_empty()).then_some(keys);
    }
    None
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn boxes(mut b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes(b[..4].try_into().ok()?) as usize;
        let typ: [u8; 4] = b[4..8].try_into().ok()?;
        let (hdr, size) = if size == 1 {
            (16, u64::from_be_bytes(b.get(8..16)?.try_into().ok()?) as usize)
        } else if size == 0 {
            (8, b.len())
        } else {
            (8, size)
        };
        if size < hdr || size > b.len() {
            return None;
        }
        let body = &b[hdr..size];
        b = &b[size..];
        Some((typ, body))
    })
}

fn child<'a>(b: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(b).find(|(t, _)| t == typ).map(|(_, body)| body)
}

/// Copia un cuadro NV12 a un búfer compacto de exactamente `w`×`h`.
/// El decodificador puede entregar cuadros con más alto (alineado a 16) y otro paso de fila;
/// si se pasaban tal cual, el plano de color quedaba corrido: barra verde y colores duplicados.
unsafe fn nv12_packed(s: &IMFSample, w: u32, h: u32, alloc_h: u32, stride: i32) -> Result<IMFSample> {
    let (w, h) = (w as usize, h as usize);
    let src = s.ConvertToContiguousBuffer()?;
    let mut out = vec![0u8; w * h * 3 / 2];
    let copy = |base: *const u8, pitch: usize, plane_rows: usize, out: &mut [u8]| {
        for y in 0..h {
            std::ptr::copy_nonoverlapping(base.add(y * pitch), out[y * w..].as_mut_ptr(), w);
        }
        let uv = base.add(pitch * plane_rows);
        for y in 0..h / 2 {
            std::ptr::copy_nonoverlapping(uv.add(y * pitch), out[w * h + y * w..].as_mut_ptr(), w);
        }
    };
    if let Ok(b2) = src.cast::<IMF2DBuffer2>() {
        let (mut p, mut pitch, mut start, mut len) = (std::ptr::null_mut(), 0i32, std::ptr::null_mut(), 0u32);
        b2.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut p, &mut pitch, &mut start, &mut len)?;
        let pitch_u = pitch.unsigned_abs() as usize;
        // Filas reales del plano Y = tamaño del búfer / paso / 1,5.
        let rows = ((len as usize).saturating_sub(p.offset_from(start) as usize) / pitch_u.max(1) * 2 / 3).max(h);
        copy(p, pitch_u, rows, &mut out);
        b2.Unlock2D()?;
    } else if let Ok(b2) = src.cast::<IMF2DBuffer>() {
        let mut p = std::ptr::null_mut();
        let mut pitch = 0i32;
        b2.Lock2D(&mut p, &mut pitch)?;
        copy(p, pitch.unsigned_abs() as usize, alloc_h as usize, &mut out);
        b2.Unlock2D()?;
    } else {
        let mut p = std::ptr::null_mut();
        let mut len = 0u32;
        src.Lock(&mut p, None, Some(&mut len))?;
        let pitch = (stride.unsigned_abs() as usize).max(w);
        // El alto real del plano Y se deduce del tamaño del búfer (Y + UV/2).
        let rows = (len as usize / pitch * 2 / 3).max(h);
        copy(p, pitch, rows, &mut out);
        src.Unlock()?;
    }
    let buf = MFCreateMemoryBuffer(out.len() as u32)?;
    let mut p = std::ptr::null_mut();
    buf.Lock(&mut p, None, None)?;
    std::ptr::copy_nonoverlapping(out.as_ptr(), p, out.len());
    buf.Unlock()?;
    buf.SetCurrentLength(out.len() as u32)?;
    let o = MFCreateSample()?;
    o.AddBuffer(&buf)?;
    Ok(o)
}
