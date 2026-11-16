//! Captura del monitor principal con DXGI Desktop Duplication.
//! La imagen nunca sale de la GPU: se copia a una textura propia y listo.
//! No se inyecta nada en el juego (compatible con anticheats como Vanguard).

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::log::log;

pub enum Poll {
    Ok,
    SizeChanged,
}

pub struct Capture {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    pub width: u32,
    pub height: u32,
    /// Última imagen del escritorio (BGRA), en la GPU.
    pub latest: Option<ID3D11Texture2D>,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    retry_at: i64,
}

impl Capture {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut found = None;
            let mut i = 0;
            'outer: while let Ok(adapter) = factory.EnumAdapters1(i) {
                let mut j = 0;
                while let Ok(output) = adapter.EnumOutputs(j) {
                    let d = output.GetDesc()?;
                    let r = d.DesktopCoordinates;
                    if d.AttachedToDesktop.as_bool() && r.left == 0 && r.top == 0 {
                        found = Some((adapter.clone(), output));
                        break 'outer;
                    }
                    j += 1;
                }
                i += 1;
            }
            let (adapter, output) = found.ok_or_else(|| anyhow!("no se encontró el monitor principal"))?;
            let adesc = adapter.GetDesc1()?;

            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or_else(|| anyhow!("sin dispositivo D3D11"))?;
            let context = context.ok_or_else(|| anyhow!("sin contexto D3D11"))?;
            // El encoder de Media Foundation usa el mismo dispositivo desde sus propios hilos.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            let output: IDXGIOutput1 = output.cast()?;
            let r = output.GetDesc()?.DesktopCoordinates;
            let mut cap = Capture {
                device,
                context,
                luid: adesc.AdapterLuid,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                latest: None,
                output,
                dup: None,
                retry_at: 0,
            };
            cap.open()?;
            Ok(cap)
        }
    }

    /// Abre la duplicación. Devuelve false si cambió la resolución del escritorio.
    fn open(&mut self) -> Result<bool> {
        self.dup = None;
        let dup = unsafe { self.output.DuplicateOutput(&self.device)? };
        let d = unsafe { dup.GetDesc() };
        let same = d.ModeDesc.Width == self.width && d.ModeDesc.Height == self.height;
        self.dup = Some(dup);
        Ok(same)
    }

    /// Toma la imagen más reciente si hubo cambios (no bloquea).
    pub fn poll(&mut self, now: i64) -> Result<Poll> {
        if self.dup.is_none() {
            if now < self.retry_at {
                return Ok(Poll::Ok);
            }
            match self.open() {
                Ok(true) => {}
                Ok(false) => return Ok(Poll::SizeChanged),
                Err(_) => {
                    // Escritorio seguro (UAC, bloqueo) o cambio de modo: reintentar luego.
                    self.retry_at = now + 5_000_000;
                    return Ok(Poll::Ok);
                }
            }
        }
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(0, &mut info, &mut res) } {
            Ok(()) => {
                let r = if info.LastPresentTime != 0 {
                    match res {
                        Some(res) => res.cast::<ID3D11Texture2D>().map_err(Into::into).and_then(|t| self.copy(&t)),
                        None => Ok(()),
                    }
                } else {
                    Ok(())
                };
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
                r?;
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(e) => {
                if e.code() != DXGI_ERROR_ACCESS_LOST {
                    log(&format!("duplicación: {e}"));
                }
                self.dup = None;
                self.retry_at = now;
            }
        }
        Ok(Poll::Ok)
    }

    fn copy(&mut self, frame: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            if self.latest.is_none() {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                frame.GetDesc(&mut d);
                d.MipLevels = 1;
                d.ArraySize = 1;
                d.Usage = D3D11_USAGE_DEFAULT;
                d.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                d.CPUAccessFlags = 0;
                d.MiscFlags = 0;
                let mut t = None;
                self.device.CreateTexture2D(&d, None, Some(&mut t))?;
                self.latest = t;
            }
            if let Some(dst) = &self.latest {
                self.context.CopyResource(dst, frame);
            }
        }
        Ok(())
    }
}
