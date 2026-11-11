//! Windows Graphics Capture (WGC) per-WINDOW capture source — a drop-in alternative
//! to `dxgi::CaptureDevice` that produces the SAME per-frame BGRA `ID3D11Texture2D`
//! the NVENC encoder (`encode/*`) consumes, so the whole encode→RTP path is reused
//! unchanged. Where DXGI Desktop Duplication captures a whole *monitor* (and a monitor
//! can only be duplicated by ONE owner at a time — the same-monitor co-op collision),
//! WGC captures a single *window* by HWND — letting two panes share one monitor or
//! target two different app windows.
//!
//! It also captures a whole MONITOR (`create_monitor`) on a device of OUR choice — the NVIDIA
//! one. On a hybrid laptop whose panel belongs to the iGPU, Windows then hands the composed
//! desktop to NVIDIA itself (GPU-side) instead of the encoder's CPU staging hop, and a monitor
//! this process already duplicates (a second session on the same screen — DXGI refuses with
//! E_INVALIDARG) can be captured again. `path.rs` decides, by measurement, when to use it.
//!
//! ## Why this mirrors `CaptureDevice` exactly
//! `lib.rs`'s capture thread is written against a small surface: `device` + `context`
//! (handed to `Encoder::new`), `target_size`/`native_size`/`rotation_deg` (encode
//! sizing), and a `run(fps, draw_cursor, requested_output, stop, on_frame)` pacing loop
//! that calls `on_frame(&Frame)` once per paced tick. `WgcCaptureDevice` re-implements
//! that surface so `start_capture_encode` can choose it with NO downstream change.
//!
//! ## The WinRT pipeline (one-time setup, in `create`)
//! ```text
//!   HWND
//!    │  IGraphicsCaptureItemInterop::CreateForWindow         (Win32→WinRT interop)
//!    ▼
//!   GraphicsCaptureItem  ── Size() ⇒ initial content extent
//!    │
//!   our ID3D11Device ── CreateDirect3D11DeviceFromDXGIDevice ⇒ IDirect3DDevice
//!    │
//!   Direct3D11CaptureFramePool::CreateFreeThreaded(device, B8G8R8A8, 2, size)
//!    │  CreateCaptureSession(item) ⇒ GraphicsCaptureSession
//!    │  SetIsBorderRequired(false)   (suppress the Win11 yellow capture border)
//!    │  StartCapture()
//!    ▼
//!   per tick: TryGetNextFrame() ⇒ Direct3D11CaptureFrame
//!    │  .Surface() ⇒ IDirect3DSurface
//!    │  .cast::<IDirect3DDxgiInterfaceAccess>().GetInterface::<ID3D11Texture2D>()
//!    ▼
//!   CopyResource → our stable BGRA `pool` texture  (== Frame.texture)
//! ```
//! The frame pool's textures rotate per buffer, so we `CopyResource` each into one
//! stable `pool` texture and hand THAT to the encoder — exactly as the DXGI path copies
//! the duplication surface into its own pool (the encoder always reads one fixed texture).
//!
//! ## Threading / apartment
//! `CreateFreeThreaded` makes a pool whose frames can be pulled from any thread without a
//! DispatcherQueue, so we POLL `TryGetNextFrame` from the capture thread (matching the
//! DXGI pacing loop) instead of the `FrameArrived` event. The WinRT factory cache
//! (`windows_core::factory`) implicitly bootstraps a process MTA (`CoIncrementMTAUsage`)
//! the first time we touch a runtime factory, so no explicit `RoInitialize` is needed.
#![cfg(windows)]
#![allow(clippy::missing_safety_doc)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::{Interface, PCWSTR};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory1,
    DXGI_ERROR_NOT_FOUND,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::dxgi::RunExit;
use crate::Frame;

/// How many buffers the WGC frame pool rotates. 2 is the Sunshine/standard low-latency
/// choice: one being scanned out, one being filled. More adds latency.
const POOL_BUFFERS: i32 = 2;

/// Per-window WGC capture source. Public shape mirrors `dxgi::CaptureDevice` so it slots
/// into `start_capture_encode` as an alternative source: `device`/`context` go to
/// `Encoder::new`; `target_size`/`native_size`/`rotation_deg` drive encode sizing; `run`
/// is the pacing loop that emits `Frame`s.
pub struct WgcCaptureDevice {
    /// PUBLIC: encode.rs opens the NVENC session on THIS device (it AddRefs it; same
    /// contract as `CaptureDevice::device`). It is the D3D11 device the frame-pool textures
    /// live on, so the encoder's first `CopyResource`/Blt is same-device.
    pub device: ID3D11Device,
    /// PUBLIC: the immediate context — encode's pre-Blt copy of `Frame.texture` runs on it.
    pub context: ID3D11DeviceContext,

    // --- private WGC state ---
    /// The captured window (kept to detect close + re-query size); `None` = a whole monitor.
    hwnd: Option<HWND>,
    /// The WinRT capture item (HWND→item). Holds a Closed event we don't subscribe to;
    /// we detect closure by `IsWindow(hwnd)` + `TryGetNextFrame` returning errors instead.
    item: GraphicsCaptureItem,
    /// The WinRT D3D device wrapper around `device`, kept alive for the pool/session's lifetime
    /// (they were created from it). Underscore-prefixed: held for its refcount, never read now that
    /// a content-resize rebuilds the whole device instead of `Recreate`-ing the pool in place.
    _rt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
    /// The free-threaded frame pool (B8G8R8A8). Recreated on a content-size change.
    pool: Direct3D11CaptureFramePool,
    /// The live capture session (Drop closes it → stops capture).
    session: GraphicsCaptureSession,
    /// Stable BGRA pool texture (DEFAULT, RT|SRV) — the surface handed to the encoder each
    /// tick. We `CopyResource` each WGC frame's texture into this so the encoder always
    /// reads ONE fixed texture (exactly like the DXGI path's `pool`).
    tex: Option<ID3D11Texture2D>,
    /// Current capture (content) size — the frame-pool + `tex` dimensions. On a content-size
    /// change `run` returns `RunExit::Switch` so lib.rs rebuilds capture+encoder at the new size.
    width: u32,
    height: u32,
    /// The monitor index this session was started with. WGC captures a WINDOW (not a monitor), so
    /// this is never used to pick a source; it is carried purely so a content-resize `RunExit::Switch`
    /// hands lib.rs back the SAME index (keeping its `output_idx`/`current_output()` stable across
    /// the rebuild instead of clobbering it with a placeholder).
    output_idx: u32,
    /// Session-stop flag (cloned from the capture thread's `stop`); polled in `run`.
    stop: Arc<AtomicBool>,
    /// Auto-reset event the pool's FrameArrived handler sets, so `run` waits for the next
    /// frame instead of polling once per tick (a fresh frame is encoded as it arrives).
    arrived: HANDLE,
    arrived_token: i64,
    /// `SystemRelativeTime` (QPC ns) of the frame last copied into `tex` → `Frame::present_ns`.
    last_present_ns: i64,
}

/// Outcome of one `pump_latest` drain: a fresh frame was copied into `tex`, no new frame arrived
/// this tick (reuse the last surface), or the captured window's content size changed (which
/// invalidates the encoder's fixed-size textures → `run` triggers a full rebuild).
enum Pump {
    Fresh,
    Idle,
    Resized,
}

impl WgcCaptureDevice {
    /// Build a WGC capture targeting `hwnd`: create a D3D11 device on the adapter owning the
    /// window's monitor (so the encoder's hybrid bridge sees the right vendor), make the
    /// WinRT `IDirect3DDevice`, the capture item, the free-threaded B8G8R8A8 frame pool, the
    /// session (border suppressed), and `StartCapture`. Any failure is an `Err` so the host
    /// can fall back to ffmpeg, exactly like `CaptureDevice::create`.
    ///
    /// `_fast_transient` is accepted for signature-parity with `CaptureDevice::create`
    /// (WGC has no equivalent transient-duplication retry budget); it is ignored. `output_idx`
    /// is likewise carried only so a content-resize rebuild can round-trip it back to lib.rs
    /// (WGC follows a window, not a monitor).
    pub unsafe fn create(
        hwnd: isize,
        output_idx: u32,
        stop: Arc<AtomicBool>,
        _fast_transient: bool,
    ) -> windows::core::Result<Self> {
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "WGC: target HWND is not a window",
            ));
        }

        // 1. D3D11 device. BGRA_SUPPORT (WGC surfaces are B8G8R8A8) + VIDEO_SUPPORT (the
        //    encoder's VideoProcessor path may run on this device in the same-GPU fast path).
        //    Use the default hardware adapter — WGC composites on whichever GPU owns the
        //    window; a separate NVENC device is built by the encoder when the vendors differ.
        let (device, context) = Self::create_device()?;

        // 2. HWND → GraphicsCaptureItem via the activation-factory interop. The factory is
        //    cached + bootstraps the process MTA on first use (windows_core::factory).
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
        Self::start(device, context, Some(hwnd), item, output_idx, stop)
    }

    /// Build a WGC capture of the whole MONITOR at `output_idx` (DXGI attached-output order,
    /// the same index `CaptureDevice::create` takes). The device lives on the NVIDIA adapter
    /// when there is one: WGC delivers the composed desktop onto it GPU-side, so the encoder
    /// takes its same-device path even when the monitor belongs to the iGPU.
    pub unsafe fn create_monitor(output_idx: u32, stop: Arc<AtomicBool>) -> windows::core::Result<Self> {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let (out_adapter, output, actual_idx) =
            crate::dxgi::CaptureDevice::find_output(&factory, output_idx)?;
        let hmon = output.GetDesc()?.Monitor;
        let adapter = crate::path::nvidia_adapter(&factory).unwrap_or(out_adapter);
        let (device, context) = Self::create_device_on(&adapter)?;
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(hmon)?;
        Self::start(device, context, None, item, actual_idx, stop)
    }

    /// The common tail of `create`/`create_monitor`: WinRT device, frame pool, frame-arrival
    /// event, session, start.
    unsafe fn start(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        hwnd: Option<HWND>,
        item: GraphicsCaptureItem,
        output_idx: u32,
        stop: Arc<AtomicBool>,
    ) -> windows::core::Result<Self> {
        // SetMaximumFrameLatency(1): keep capture→encode queueing minimal (same as DXGI), and
        // the same max GPU scheduling priority as the DXGI device (PULSAR_GPU_PRIO=0 disables).
        if let Ok(dxgi_dev) = device.cast::<IDXGIDevice1>() {
            let _ = dxgi_dev.SetMaximumFrameLatency(1);
            if std::env::var("PULSAR_GPU_PRIO").map(|v| v != "0").unwrap_or(true) {
                let _ = dxgi_dev.SetGPUThreadPriority(7);
            }
        }

        // Wrap our D3D11 device as a WinRT IDirect3DDevice (the frame pool/session API takes
        // the WinRT handle, not the raw ID3D11Device). Bridge via the DXGI device.
        let dxgi_device: IDXGIDevice = device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        let rt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice =
            inspectable.cast()?;

        // Initial capture size: the item's reported extent (client area). WGC uses the item
        // Size for the pool; content can resize later → we Recreate the pool then.
        let size = item.Size()?;
        let (width, height) = (size.Width.max(1) as u32, size.Height.max(1) as u32);

        // 4. Free-threaded frame pool (poll-able from the capture thread w/o a dispatcher).
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &rt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            size,
        )?;

        // 5. Session from the pool+item; suppress the Win11 yellow border, then start.
        let session = pool.CreateCaptureSession(&item)?;
        // IsBorderRequired is the IGraphicsCaptureSession2 method (Win11+). windows-rs flattens
        // it onto GraphicsCaptureSession; on a Win10 build lacking it the call returns an error
        // HRESULT which we ignore — capture still works, just with the (older-OS) border.
        let _ = session.SetIsBorderRequired(false);
        // Win11 24H2+: no minimum spacing between delivered frames (1 ms); older builds lack
        // the property and keep their default.
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 });

        // Frame arrival → an auto-reset event `run` waits on (the pool is free-threaded: the
        // handler runs on a system thread, so it only signals).
        let arrived = CreateEventW(None, false, false, PCWSTR::null())?;
        let ev = arrived.0 as isize;
        let handler = TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
            move |_, _| {
                let _ = SetEvent(HANDLE(ev as *mut core::ffi::c_void));
                Ok(())
            },
        );
        let arrived_token = match pool.FrameArrived(&handler) {
            Ok(t) => t,
            Err(e) => {
                let _ = CloseHandle(arrived);
                return Err(e);
            }
        };
        if let Err(e) = session.StartCapture() {
            let _ = pool.RemoveFrameArrived(arrived_token);
            let _ = CloseHandle(arrived);
            return Err(e);
        }

        let mut me = WgcCaptureDevice {
            device,
            context,
            hwnd,
            item,
            _rt_device: rt_device,
            pool,
            session,
            tex: None,
            width,
            height,
            output_idx,
            stop,
            arrived,
            arrived_token,
            last_present_ns: 0,
        };
        me.build_pool_texture()?;
        Ok(me)
    }

    /// Create the D3D11 device WGC composites into. Mirrors `CaptureDevice::create_device`
    /// but on the DEFAULT hardware adapter (WGC handles cross-GPU window composition itself,
    /// unlike Desktop Duplication which is adapter-bound), with VIDEO_SUPPORT so the encoder's
    /// same-GPU VideoProcessor fast path can run here.
    unsafe fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // First try a chosen hardware adapter (the one owning the foreground monitor); on any
        // failure fall back to the default-adapter HARDWARE device so capture still comes up.
        if let Ok(adapter) = Self::default_adapter() {
            let mut lvl = D3D_FEATURE_LEVEL_11_0;
            let r = D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN, // explicit adapter ⇒ UNKNOWN driver type
                windows::Win32::Foundation::HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut lvl),
                Some(&mut context),
            );
            if r.is_ok() {
                return Ok((device.unwrap(), context.unwrap()));
            }
            device = None;
            context = None;
        }
        // Default hardware adapter (null adapter ⇒ DRIVER_TYPE_HARDWARE).
        let mut lvl = D3D_FEATURE_LEVEL_11_0;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut lvl),
            Some(&mut context),
        )?;
        Ok((device.unwrap(), context.unwrap()))
    }

    /// A D3D11 device on a specific adapter (the NVIDIA one for a monitor capture).
    unsafe fn create_device_on(
        adapter: &IDXGIAdapter1,
    ) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut lvl = D3D_FEATURE_LEVEL_11_0;
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::Foundation::HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut lvl),
            Some(&mut context),
        )?;
        Ok((device.unwrap(), context.unwrap()))
    }

    /// The first enumerated hardware adapter (index 0). WGC composites the window on whatever
    /// GPU owns it regardless of which device we pass, so adapter 0 is a fine default; the
    /// encoder independently picks/creates the NVIDIA device for NVENC when vendors differ.
    unsafe fn default_adapter() -> windows::core::Result<IDXGIAdapter1> {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        match factory.EnumAdapters1(0) {
            Ok(a) => Ok(a),
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => {
                Err(windows::core::Error::from(DXGI_ERROR_NOT_FOUND))
            }
            Err(e) => Err(e),
        }
    }

    /// (Re)build the stable BGRA `tex` the encoder reads, sized to the current `width`/`height`.
    /// DEFAULT usage + RT|SRV binds so the encoder's VideoProcessorBlt can sample it as an SRV
    /// — identical desc to the DXGI path's pool texture so the encoder treats both the same.
    unsafe fn build_pool_texture(&mut self) -> windows::core::Result<()> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width.max(2),
            Height: self.height.max(2),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        self.tex = tex;
        Ok(())
    }

    /// ENCODE size selection — same contract as `CaptureDevice::target_size`: an explicit
    /// (non-zero) request is honored, clamped to the capture size and even-aligned (NV12);
    /// AUTO (0/0) = the native window size. The encoder downscales native→encode in its Blt.
    pub fn target_size(&self, want_w: u32, want_h: u32) -> (u32, u32) {
        let nw = self.width.max(2);
        let nh = self.height.max(2);
        let (w, h) = if want_w > 0 && want_h > 0 {
            if self.hwnd.is_some() {
                // A WINDOW has its own shape (a portrait app, a split-pane-fitted window): the
                // client's request is a size BUDGET (its ladder point, e.g. 1280×720), not an
                // aspect — clamping each axis separately squashed an 856×1056 window into 16:9.
                fit_aspect(nw, nh, want_w, want_h)
            } else {
                (want_w.min(nw), want_h.min(nh))
            }
        } else {
            (nw, nh)
        };
        (w.max(2) & !1, h.max(2) & !1)
    }

    /// Native (full) capture size = the window content extent. The encoder sizes its bridge +
    /// VideoProcessor input to this (same role as `CaptureDevice::native_size`).
    pub fn native_size(&self) -> (u32, u32) {
        (self.width.max(2) & !1, self.height.max(2) & !1)
    }

    /// WGC always composites the window UPRIGHT (the compositor bakes any monitor rotation),
    /// so there is never a residual rotation to apply — unlike Desktop Duplication, which hands
    /// back the unrotated scan-out surface + a rotation. Always 0 (the encoder skips rotation).
    pub fn rotation_deg(&self) -> u32 {
        0
    }

    /// The Sunshine-style pacing loop, mirroring `CaptureDevice::run`: emit `on_frame` once per
    /// paced client tick with the CURRENT `tex` (a fresh WGC frame copied in, or the last one
    /// reused on a tick with no new frame) so the encoder sees a steady cadence. Returns
    /// `RunExit::Stop` when `stop` is set, the window closes, or capture fails unrecoverably.
    ///
    /// `_requested_output` / `draw_cursor` are taken for signature-parity with the DXGI loop.
    /// A WGC source has no monitor index to switch to (it follows ITS window), so a monitor
    /// switch is never returned — the host re-targets a window by tearing the session down and
    /// building a new `WgcCaptureDevice`, not via `RunExit::Switch`. The cursor is composited by
    /// WGC itself when the session's cursor-capture is enabled (default on), so `draw_cursor`
    /// needs no per-frame work here.
    pub unsafe fn run(
        &mut self,
        client_fps: u32,
        draw_cursor: bool,
        requested_output: &AtomicU32,
        rebuild: &AtomicBool,
        stop: &AtomicBool,
        mut on_frame: impl FnMut(&Frame),
    ) -> RunExit {
        use crate::dxgi::platform::{HiResTimer, Qpc};

        // A monitor capture honours the cursor flag like the DXGI path (WGC composites the
        // cursor itself); a window capture keeps WGC's default (on).
        if self.hwnd.is_none() {
            let _ = self.session.SetIsCursorCaptureEnabled(draw_cursor);
        }
        // Encode a fresh frame the moment it arrives (see the DXGI loop, pacing.rs step 2).
        let emit_on_arrival = std::env::var("PULSAR_EMIT_ON_ARRIVAL")
            .map(|v| v != "0")
            .unwrap_or(true);

        let fps = client_fps.max(1);
        let interval_ns: i64 = 1_000_000_000i64 / fps as i64;
        let timer = match HiResTimer::new() {
            Ok(t) => t,
            Err(_) => return RunExit::Stop,
        };
        let qpc = Qpc::new();

        let mut have_content = false;
        // QPC anchor: deadline for frame N is `start + N*interval`. A content-resize is absorbed
        // by Recreate in-band (no DXGI-style mode-change reinit), but we DO re-anchor on a big
        // pacing overrun below (A17), so `start` is mutable.
        let mut start = qpc.now_ns();
        let mut frame_no: i64 = 0;

        while !stop.load(Ordering::Relaxed) {
            // is_new for THIS tick: true when we pulled a fresh WGC frame, false on the paced
            // reuse path (no new frame this interval) — drives Frame.is_new like the DXGI loop.
            let last_was_new;
            match self.hwnd {
                // The window closing is terminal for a per-window source: stop so the host tears
                // the session down (it can re-pick a target). Cheap user32 check each tick.
                Some(hwnd) => {
                    if !IsWindow(Some(hwnd)).as_bool() {
                        return RunExit::Stop;
                    }
                }
                // A monitor source follows the session-menu monitor picker like the DXGI loop.
                None => {
                    let req = requested_output.swap(u32::MAX, Ordering::AcqRel);
                    if req != u32::MAX && req != self.output_idx {
                        return RunExit::Switch(req);
                    }
                }
            }
            if rebuild.swap(false, Ordering::AcqRel) {
                return RunExit::Switch(self.output_idx);
            }

            // A17: re-anchor the cadence on a big overrun (a stall / long encode / scheduler
            // hiccup), mirroring the DXGI pacing loop (dxgi/pacing.rs ~99-108). Without this the
            // post-stall deadlines are all in the past and the loop bursts several catch-up frames
            // back-to-back, which the client renders as a jump+latency SPIKE. One long tick now
            // costs ~1 frame, not a burst.
            let now0 = qpc.now_ns();
            let mut deadline = start + frame_no * interval_ns;
            if now0 - deadline > 2 * interval_ns {
                start = now0 - frame_no * interval_ns;
                deadline = now0;
            }
            frame_no += 1;

            // 1. Drain the freshest frame: WGC's free-threaded pool buffers up to POOL_BUFFERS
            //    frames; pull the MOST RECENT (drop older ones) to minimize latency, copy it
            //    into the stable `tex`. Nothing queued → wait for the FrameArrived event until
            //    the deadline. `Idle` (no new frame this interval) → reuse last `tex`.
            let mut pumped = self.pump_latest();
            while emit_on_arrival && matches!(pumped, Ok(Pump::Idle) | Err(_)) {
                let now = qpc.now_ns();
                if now >= deadline {
                    break;
                }
                let ms = ((deadline - now + 999_999) / 1_000_000) as u32;
                if WaitForSingleObject(self.arrived, ms) != WAIT_OBJECT_0 {
                    break;
                }
                pumped = self.pump_latest();
            }
            match pumped {
                Ok(Pump::Fresh) => {
                    have_content = true;
                    last_was_new = true;
                }
                Ok(Pump::Idle) => {
                    // No new frame this tick — reuse the last surface (paced reuse path).
                    last_was_new = false;
                }
                Ok(Pump::Resized) => {
                    // The captured window's content size changed. Unlike the DXGI path, we can't
                    // just recreate the pool in-band: the encoder's textures are sized to the OLD
                    // capture size (in the HYBRID cross-adapter path `amd_shared` is a fixed-size
                    // STAGING, so submit()'s CopyResource(amd_shared, frame.texture) would be a
                    // dimension mismatch D3D11 silently drops → the client freezes on the last
                    // pre-resize frame). Return Switch so lib.rs rebuilds capture+encoder at the new
                    // size (it re-creates this WgcCaptureDevice on the SAME window and re-opens NVENC
                    // sized to the new extent — the encoder rebuild also forces a fresh IDR). The
                    // carried index is just this session's output_idx, kept stable across the rebuild.
                    return RunExit::Switch(self.output_idx);
                }
                Err(_e) => {
                    // A transient WGC error (e.g. mid-resize). Reuse last surface; if we never
                    // got content, just keep waiting until the deadline below.
                    last_was_new = false;
                }
            }

            // 2. Pace to the deadline with the hi-res timer (returns at once if we're late) —
            //    AFTER the emit for a fresh frame (step 4), before it for a reused surface.
            let fresh = emit_on_arrival && last_was_new;
            if !fresh {
                let now2 = qpc.now_ns();
                if deadline > now2 {
                    timer.sleep_for(std::time::Duration::from_nanos((deadline - now2) as u64));
                }
            }

            // 3. Emit the current stable texture (fresh or reused) once we have any content.
            if have_content {
                if let Some(tex) = self.tex.as_ref() {
                    let frame = Frame {
                        texture: tex,
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        width: self.width.max(2) & !1,
                        height: self.height.max(2) & !1,
                        is_new: last_was_new,
                        // A content-resize is NOT handled in-band; it exits via RunExit::Switch (see
                        // pump_latest → the Pump::Resized arm above) so the encoder is rebuilt at the
                        // new size and forces its own fresh IDR. Steady-state frames never force one.
                        force_idr: false,
                        present_ns: if last_was_new { self.last_present_ns } else { 0 },
                    };
                    on_frame(&frame);
                }
            }
            // 4. One frame per slot: after an on-arrival emit, sleep out the rest of the slot.
            if fresh {
                let now3 = qpc.now_ns();
                if deadline > now3 {
                    timer.sleep_for(std::time::Duration::from_nanos((deadline - now3) as u64));
                }
            }
        }
        RunExit::Stop
    }

    /// Pull the most-recent available WGC frame (dropping any older queued ones to cap latency)
    /// and copy it into the stable `tex`. Returns `Pump::Fresh` when a frame was copied,
    /// `Pump::Idle` when the pool had no new frame this tick, `Pump::Resized` when the window's
    /// content size changed (the caller rebuilds capture+encoder), or `Err` on a copy/interop
    /// failure.
    unsafe fn pump_latest(&mut self) -> windows::core::Result<Pump> {
        let mut got: Option<ID3D11Texture2D> = None;
        let mut got_ns: i64 = 0;
        let mut resized = false;

        // TryGetNextFrame returns Ok(frame) while frames are queued and an error / null when
        // empty. Loop to drain to the newest so we never encode a stale buffered frame.
        loop {
            let frame = match self.pool.TryGetNextFrame() {
                Ok(f) => f,
                // No more frames queued (or a benign empty-pool error): stop draining.
                Err(_) => break,
            };
            // Detect a content-size change (the window resized): WGC keeps delivering at the
            // OLD pool size until the pool is recreated, but ContentSize reports the new extent.
            if let Ok(cs) = frame.ContentSize() {
                let (cw, ch) = (cs.Width.max(1) as u32, cs.Height.max(1) as u32);
                if cw != self.width || ch != self.height {
                    resized = true;
                }
            }
            // WinRT surface → ID3D11Texture2D via the DXGI-interface-access bridge.
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let tex: ID3D11Texture2D = access.GetInterface()?;
            got = Some(tex);
            // When DWM composed it — 100 ns units on the QPC clock (the `Qpc::now_ns` base).
            got_ns = frame.SystemRelativeTime().map(|t| t.Duration * 100).unwrap_or(0);
            // `frame` drops here, returning its buffer to the pool — keep looping for a newer one.
        }

        // A content-size change must NOT be absorbed in-band: the encoder's textures (and, in the
        // hybrid cross-adapter path, its fixed-size `amd_shared` STAGING) are sized to the OLD
        // capture size, so a differently-sized frame would be silently dropped by submit()'s
        // CopyResource. Signal a rebuild; the just-pulled OLD-size `got` is discarded.
        if resized {
            return Ok(Pump::Resized);
        }

        let Some(src) = got else {
            return Ok(Pump::Idle);
        };
        let Some(dst) = self.tex.as_ref() else {
            return Ok(Pump::Idle);
        };
        // Straight GPU→GPU blit (same B8G8R8A8 format, same size) on the immediate context —
        // the encoder reads `dst` next. Same pattern as the DXGI `snapshot` CopyResource.
        self.context.CopyResource(dst, &src);
        self.last_present_ns = got_ns;
        Ok(Pump::Fresh)
    }
}

impl Drop for WgcCaptureDevice {
    fn drop(&mut self) {
        // Closing the session stops capture; closing the pool releases its buffers. Both are
        // best-effort — the COM refs drop regardless. `item`/`rt_device`/`device` drop after.
        let _ = self.session.Close();
        let _ = self.pool.RemoveFrameArrived(self.arrived_token);
        let _ = self.pool.Close();
        unsafe {
            let _ = CloseHandle(self.arrived);
        }
        let _ = &self.item;
        let _ = self.stop.load(Ordering::Relaxed);
    }
}

// The WGC device + its COM/WinRT handles are created and only ever touched on the single
// dedicated capture thread (lib.rs spawns it, never shares it), exactly like CaptureDevice.
// The free-threaded frame pool is explicitly safe to pull from any single thread. So marking
// it Send for the thread-handoff in `start_capture_encode` is sound under our single-thread use.
unsafe impl Send for WgcCaptureDevice {}

// Silence an unused warning for the timestamp import on builds that don't reference it; the
// type is kept imported so a future FrameArrived/SystemRelativeTime pacing variant compiles.
#[allow(dead_code)]
const _: Option<TimeSpan> = None;

/// Largest `w × h` with the `nw × nh` source's aspect that fits the `bw × bh` budget box and
/// never exceeds the source itself (no upscale).
fn fit_aspect(nw: u32, nh: u32, bw: u32, bh: u32) -> (u32, u32) {
    let scale = (bw as f64 / nw as f64).min(bh as f64 / nh as f64).min(1.0);
    (((nw as f64 * scale).round() as u32).max(2), ((nh as f64 * scale).round() as u32).max(2))
}

#[cfg(test)]
mod tests {
    use super::fit_aspect;

    #[test]
    fn a_portrait_window_keeps_its_shape_in_a_16_9_budget() {
        // 856×1056 window, client ladder point 1280×720 → height-bound, aspect kept.
        let (w, h) = fit_aspect(856, 1056, 1280, 720);
        assert_eq!(h, 720);
        assert!((w as i32 - 584).abs() <= 1, "{w}");
    }

    #[test]
    fn never_upscales() {
        assert_eq!(fit_aspect(800, 600, 1920, 1080), (800, 600));
    }
}
