//! The Pulsar menu in a window of its own, for a session shown in a picture-in-picture window
//! too small to hold it (issue #18; a PiP big enough shows it inside, as usual): the egui panel
//! is drawn alone into this borderless popup, sized to the panel and centred on the PiP (kept
//! on the PiP's monitor, so the scale and the pointer mapping are the PiP's), and the video
//! keeps playing in the PiP.
//!
//! * Never takes the focus (`WS_EX_NOACTIVATE`): keyboard input reaches the menu the way it always
//!   does — the app's webview relays it over stdin (`kin …`) — and the PiP keeps its input.
//! * Owned by the PiP's top-level window: stays above it, minimizes and closes with it.
//! * Mouse input is fed to the same egui event queue the in-window overlay uses (`super::push_pointer`).
//! * Shares the renderer's D3D11 device and egui painter; its own flip-model swapchain.

use windows::core::{w, Interface, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11RenderTargetView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
	IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
	DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
	CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetWindowRect, RegisterClassW, SetWindowPos,
	ShowWindow, GA_ROOT, HWND_TOPMOST, MA_NOACTIVATE, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNA, WM_MOUSEACTIVATE,
	WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};


pub(super) struct MenuWin {
	pub hwnd: HWND,
	swap: IDXGISwapChain1,
	pub rtv: Option<ID3D11RenderTargetView>,
	pub width: u32,
	pub height: u32,
	shown: bool,
}

unsafe extern "system" fn menu_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
	if msg == WM_MOUSEACTIVATE {
		return LRESULT(MA_NOACTIVATE as isize);
	}
	if super::push_pointer(msg, wp, lp) {
		return LRESULT(0);
	}
	DefWindowProcW(hwnd, msg, wp, lp)
}

/// Where the menu goes (physical): centred on the PiP, inside the PiP's monitor's work area.
unsafe fn place(owner: HWND, w: i32, h: i32) -> (i32, i32, i32, i32) {
	let mut pip = RECT::default();
	let _ = GetWindowRect(owner, &mut pip);
	let mon = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
	let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
	let wa = if GetMonitorInfoW(mon, &mut mi).as_bool() { mi.rcWork } else { pip };
	let (w, h) = (w.min(wa.right - wa.left), h.min(wa.bottom - wa.top));
	let (cx, cy) = ((pip.left + pip.right) / 2, (pip.top + pip.bottom) / 2);
	let (x, y) = (cx - w / 2, cy - h / 2);
	(x.clamp(wa.left, wa.right - w), y.clamp(wa.top, wa.bottom - h), w, h)
}

impl MenuWin {
	/// Create the popup (hidden) for the PiP whose renderer child is `child`, `pw`×`ph` px.
	pub unsafe fn new(device: &ID3D11Device, child: HWND, pw: i32, ph: i32) -> Result<Self> {
		let owner = GetAncestor(child, GA_ROOT);
		let hinstance = GetModuleHandleW(None)?;
		let class = w!("PulsarRenderMenu");
		let wc = WNDCLASSW { lpfnWndProc: Some(menu_wndproc), hInstance: hinstance.into(), lpszClassName: class, ..Default::default() };
		RegisterClassW(&wc); // a second registration fails harmlessly
		let (x, y, w, h) = place(owner, pw, ph);
		let hwnd = CreateWindowExW(
			WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
			class,
			PCWSTR::null(),
			WS_POPUP,
			x,
			y,
			w,
			h,
			Some(owner),
			None,
			Some(hinstance.into()),
			None,
		)?;
		let dxgi: IDXGIDevice = device.cast()?;
		let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
		let desc = DXGI_SWAP_CHAIN_DESC1 {
			Width: w as u32,
			Height: h as u32,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
			BufferCount: 2,
			SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
			..Default::default()
		};
		let swap = factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None)?;
		let mut me = Self { hwnd, swap, rtv: None, width: w as u32, height: h as u32, shown: false };
		me.make_rtv(device)?;
		Ok(me)
	}

	unsafe fn make_rtv(&mut self, device: &ID3D11Device) -> Result<()> {
		let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
		let mut rtv = None;
		device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
		self.rtv = rtv;
		Ok(())
	}

	/// Show it centred on the PiP, `pw`×`ph` px (the panel's size; follows each page's height).
	pub unsafe fn show(&mut self, device: &ID3D11Device, child: HWND, pw: i32, ph: i32) {
		let owner = GetAncestor(child, GA_ROOT);
		let (x, y, w, h) = place(owner, pw, ph);
		if (w as u32, h as u32) != (self.width, self.height) {
			self.rtv = None;
			let _ = self.swap.ResizeBuffers(0, w as u32, h as u32, DXGI_FORMAT_B8G8R8A8_UNORM, Default::default());
			self.width = w as u32;
			self.height = h as u32;
			let _ = self.make_rtv(device);
		}
		let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
		if !self.shown {
			let _ = ShowWindow(self.hwnd, SW_SHOWNA);
			self.shown = true;
		}
	}

	pub unsafe fn hide(&mut self) {
		if self.shown {
			let _ = ShowWindow(self.hwnd, SW_HIDE);
			self.shown = false;
		}
	}

	pub unsafe fn present(&self) {
		let _ = self.swap.Present(1, Default::default());
	}
}

impl Drop for MenuWin {
	fn drop(&mut self) {
		self.rtv = None;
		// SAFETY: our own window, destroyed on the thread that created it (the render thread).
		unsafe {
			let _ = DestroyWindow(self.hwnd);
		}
	}
}
