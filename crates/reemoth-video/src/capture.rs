//! Windows desktop capture via DXGI Desktop Duplication.
//!
//! The OS hands us a GPU texture only when the screen changed. We copy it to
//! a CPU-readable staging texture for the software encoder; a hardware
//! encoder will take the GPU texture directly instead.
//!
//! The mouse pointer is not part of the duplicated image; the viewer will
//! draw it from pointer updates (M3).

use std::time::Duration;

use anyhow::{Context, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource,
};
use windows::core::Interface;

/// One captured desktop image, borrowed from the staging texture.
pub struct CapturedFrame<'a> {
    pub bgra: &'a [u8],
    pub width: usize,
    pub height: usize,
    /// Bytes per row.
    pub stride: usize,
}

pub enum CaptureResult<'a> {
    Frame(CapturedFrame<'a>),
    /// Nothing changed within the timeout.
    Unchanged,
}

pub struct DesktopCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    mapped: bool,
    width: usize,
    height: usize,
}

// SAFETY: D3D11 devices are free-threaded; the context and duplication are
// only ever used from the thread that owns this struct.
unsafe impl Send for DesktopCapture {}

impl DesktopCapture {
    /// Captures the primary display (the first output of the first adapter
    /// that has one; on hybrid-GPU laptops that is the GPU driving the panel).
    pub fn primary() -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.context("creating DXGI factory")?;
        let (adapter, output) = first_output(&factory)?;

        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("creating D3D11 device")?;
        let device = device.context("no D3D11 device")?;
        let context = context.context("no D3D11 context")?;
        // Fails while another capture of this display is still open (e.g. a
        // previous session winding down), so retry for a while.
        let mut attempt = 0;
        let duplication = loop {
            match duplicate(&output, &device) {
                Ok(d) => break d,
                Err(e) if attempt < 40 => {
                    tracing::debug!("desktop duplication busy, retrying: {e:#}");
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(e),
            }
        };
        let desc = unsafe { duplication.GetDesc() };
        Ok(Self {
            device,
            context,
            output,
            duplication,
            staging: None,
            mapped: false,
            width: desc.ModeDesc.Width as usize,
            height: desc.ModeDesc.Height as usize,
        })
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Waits up to `timeout` for the screen to change.
    pub fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<CaptureResult<'_>> {
        self.unmap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            self.duplication
                .AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource)
        };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(CaptureResult::Unchanged),
            // Desktop switch (UAC prompt, lock screen), mode change, or full-screen app.
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.recreate()?;
                return Ok(CaptureResult::Unchanged);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }

        // Only the pointer moved: no new image.
        if info.LastPresentTime == 0 {
            unsafe { self.duplication.ReleaseFrame() }.context("ReleaseFrame")?;
            return Ok(CaptureResult::Unchanged);
        }

        let texture: ID3D11Texture2D = resource
            .context("frame without a resource")?
            .cast()
            .context("frame is not a texture")?;
        let staging = self.staging_for(&texture)?;
        unsafe {
            self.context.CopyResource(&staging, &texture);
            self.duplication.ReleaseFrame().context("ReleaseFrame")?;
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("mapping staging texture")?;
        }
        self.mapped = true;
        let stride = mapped.RowPitch as usize;
        // SAFETY: the mapping stays valid until `unmap`, which only runs on
        // the next call (it takes `&mut self`, so the borrow ends first).
        let bgra =
            unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, stride * self.height) };
        Ok(CaptureResult::Frame(CapturedFrame {
            bgra,
            width: self.width,
            height: self.height,
            stride,
        }))
    }

    fn staging_for(&mut self, source: &ID3D11Texture2D) -> anyhow::Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if let Some(staging) = &self.staging {
            let mut have = D3D11_TEXTURE2D_DESC::default();
            unsafe { staging.GetDesc(&mut have) };
            if (have.Width, have.Height, have.Format) == (desc.Width, desc.Height, desc.Format) {
                return Ok(staging.clone());
            }
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .context("creating staging texture")?;
        let staging = staging.context("no staging texture")?;
        self.width = desc.Width as usize;
        self.height = desc.Height as usize;
        self.staging = Some(staging.clone());
        Ok(staging)
    }

    fn unmap(&mut self) {
        if self.mapped {
            if let Some(staging) = &self.staging {
                unsafe { self.context.Unmap(staging, 0) };
            }
            self.mapped = false;
        }
    }

    /// Re-creates the duplication after access was lost. Retries for a while
    /// because the secure desktop (UAC, lock screen) refuses duplication.
    fn recreate(&mut self) -> anyhow::Result<()> {
        self.unmap();
        for _ in 0..50 {
            match duplicate(&self.output, &self.device) {
                Ok(d) => {
                    let desc = unsafe { d.GetDesc() };
                    self.width = desc.ModeDesc.Width as usize;
                    self.height = desc.ModeDesc.Height as usize;
                    self.duplication = d;
                    self.staging = None;
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!("duplication not available yet: {e:#}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        bail!("desktop duplication stayed unavailable")
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        self.unmap();
    }
}

fn first_output(factory: &IDXGIFactory1) -> anyhow::Result<(IDXGIAdapter1, IDXGIOutput1)> {
    for i in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(i) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e).context("enumerating adapters"),
        };
        if let Ok(output) = unsafe { adapter.EnumOutputs(0) } {
            let output: IDXGIOutput1 = output.cast().context("IDXGIOutput1 unsupported")?;
            return Ok((adapter, output));
        }
    }
    bail!("no display attached to any GPU")
}

fn duplicate(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> anyhow::Result<IDXGIOutputDuplication> {
    unsafe { output.DuplicateOutput(device) }.context("DuplicateOutput (desktop duplication)")
}
