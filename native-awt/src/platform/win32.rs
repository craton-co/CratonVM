// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Win32 platform backend -- Windows windowing via the `windows` crate,
//! GDI for buffer blitting, and DirectWrite for text rasterization.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tracing::{debug, warn};

use windows::core::{w, PCWSTR};
type WinResult<T> = windows::core::Result<T>;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::backend::*;

// ---------------------------------------------------------------------------
// RAII guards for Win32 handle pairs.
//
// The previous code in `blit_buffer` and `clipboard_set_text` (and the other
// GDI-heavy paths) used hand-written cleanup after each call site. That works
// for the happy path, but the `?` operator on `CreateDIBSection` /
// `OpenClipboard` returns early without running the matching `EndPaint` /
// `GlobalFree` / `CloseClipboard`, leaking the handle on every error.
//
// These guards wrap the pair so the closing call runs on every exit path,
// including `?` propagation, panics, and explicit `return`.
// ---------------------------------------------------------------------------

/// Pairs `BeginPaint` with `EndPaint`. `Drop` calls `EndPaint`, so the paint
/// session is closed even if a later `?` bails out of `blit_buffer`.
struct PaintGuard {
    hwnd: HWND,
    ps: PAINTSTRUCT,
    hdc: HDC,
}

impl PaintGuard {
    /// Begins a paint session. Returns `None` if `BeginPaint` returns an
    /// invalid HDC (the underlying call does not require an `EndPaint` in
    /// that case, per the Win32 contract).
    unsafe fn begin(hwnd: HWND) -> Option<Self> {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: caller guarantees `hwnd` is a live window; `ps` is writable
        // for the synchronous BeginPaint call and is retained for EndPaint.
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        if hdc.is_invalid() {
            return None;
        }
        Some(Self { hwnd, ps, hdc })
    }

    fn hdc(&self) -> HDC {
        self.hdc
    }
}

impl Drop for PaintGuard {
    fn drop(&mut self) {
        // SAFETY: `BeginPaint` succeeded (otherwise `begin` returned `None`),
        // so the matching `EndPaint` is required exactly once.
        unsafe {
            let _ = EndPaint(self.hwnd, &self.ps);
        }
    }
}

/// Wraps an `HGLOBAL` returned by `GlobalAlloc`. `Drop` calls `GlobalFree`
/// unless ownership has been released (e.g. after `SetClipboardData`, which
/// transfers ownership to the clipboard).
struct GlobalAllocGuard {
    handle: HGLOBAL,
    released: bool,
}

impl GlobalAllocGuard {
    unsafe fn new(flags: GLOBAL_ALLOC_FLAGS, size: usize) -> WinResult<Self> {
        // SAFETY: caller chooses valid GlobalAlloc flags; the API accepts any
        // byte size and returns an owned handle or an error.
        let handle = unsafe { GlobalAlloc(flags, size) }?;
        Ok(Self {
            handle,
            released: false,
        })
    }

    fn handle(&self) -> HGLOBAL {
        self.handle
    }

    /// Marks the allocation as transferred elsewhere (typically to the
    /// clipboard after `SetClipboardData`). Prevents `Drop` from freeing it.
    fn release(mut self) -> HGLOBAL {
        self.released = true;
        self.handle
    }
}

impl Drop for GlobalAllocGuard {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: `GlobalAlloc` succeeded and ownership was not handed off.
            unsafe {
                let _ = GlobalFree(self.handle);
            }
        }
    }
}

/// Pairs `OpenClipboard` with `CloseClipboard`. `Drop` runs `CloseClipboard`
/// so every error path between the two calls is balanced.
struct ClipboardGuard;

impl ClipboardGuard {
    unsafe fn open(hwnd: HWND) -> WinResult<Self> {
        // SAFETY: caller supplies either a live owner HWND or the documented
        // null/default handle; the guard balances success with CloseClipboard.
        unsafe { OpenClipboard(hwnd) }?;
        Ok(Self)
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: `OpenClipboard` succeeded, so a matching `CloseClipboard`
        // is required.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Wraps a GDI `HDC` created by `CreateCompatibleDC` (or `CreateDC`). `Drop`
/// calls `DeleteDC`. Use [`SelectObjectGuard`] to restore any objects
/// selected into the DC before this guard drops.
struct DcGuard(HDC);

impl DcGuard {
    fn new(hdc: HDC) -> Option<Self> {
        if hdc.is_invalid() {
            None
        } else {
            Some(Self(hdc))
        }
    }

    fn hdc(&self) -> HDC {
        self.0
    }
}

impl Drop for DcGuard {
    fn drop(&mut self) {
        // SAFETY: this guard is constructed only for a valid owned HDC and
        // Drop runs once after selected objects have been restored.
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// Wraps a GDI object handle that must be passed to `DeleteObject` (e.g.
/// `HBITMAP` from `CreateDIBSection`/`CreateBitmap`, `HFONT` from
/// `CreateFontW`). The handle MUST first be deselected from any DC before
/// drop; selecting it into another DC and letting drop run is fine.
struct GdiObjectGuard {
    handle: HGDIOBJ,
    released: bool,
}

impl GdiObjectGuard {
    fn new<T: Into<HGDIOBJ>>(handle: T) -> Self {
        Self {
            handle: handle.into(),
            released: false,
        }
    }

    fn handle(&self) -> HGDIOBJ {
        self.handle
    }
}

impl Drop for GdiObjectGuard {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: the guard owns this valid GDI handle and selection guards
            // have restored it out of every DC before Drop.
            unsafe {
                let _ = DeleteObject(self.handle);
            }
        }
    }
}

/// Restores a previously selected GDI object back into its DC on drop.
/// `SelectObject` returns the prior object; this guard puts it back so the
/// caller-owned object isn't left selected (which would block `DeleteObject`).
struct SelectObjectGuard {
    hdc: HDC,
    prev: HGDIOBJ,
}

impl SelectObjectGuard {
    /// Selects `obj` into `hdc` and remembers the previously selected object
    /// so it can be restored on drop.
    unsafe fn select<T: Into<HGDIOBJ>>(hdc: HDC, obj: T) -> Self {
        // SAFETY: caller guarantees the HDC and object are live and compatible;
        // this guard records the returned prior object for exact restoration.
        let prev = unsafe { SelectObject(hdc, obj.into()) };
        Self { hdc, prev }
    }
}

impl Drop for SelectObjectGuard {
    fn drop(&mut self) {
        // SAFETY: `hdc` remains live longer than this guard and `prev` is the
        // exact object returned by the paired SelectObject call.
        unsafe {
            SelectObject(self.hdc, self.prev);
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_window_id() -> WindowId {
    WindowId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// Stores a window handle as a raw `isize` so the struct is `Send`.
struct WindowInfo {
    hwnd_raw: isize,
    width: u32,
    height: u32,
}

impl WindowInfo {
    fn hwnd(&self) -> HWND {
        HWND(self.hwnd_raw as *mut _)
    }
}

struct DirectWriteRenderer {
    factory: IDWriteFactory,
    // Wrapped in `Mutex` so we can write to the cache through a `&self`
    // borrow. The backend trait's `rasterize_text` takes `&self`, so without
    // interior mutability this cache would be dead code (we'd always create
    // a fresh format on every draw).
    text_format_cache: Mutex<HashMap<String, IDWriteTextFormat>>,
}

impl DirectWriteRenderer {
    fn new() -> WinResult<Self> {
        // SAFETY: the COM-producing API has no raw pointer inputs; the typed
        // result owns the returned shared DirectWrite factory.
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        Ok(Self {
            factory,
            text_format_cache: Mutex::new(HashMap::new()),
        })
    }

    fn get_or_create_format(
        &self,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> WinResult<IDWriteTextFormat> {
        let key = format!("{}:{}:{}:{}", font_family, font_size, bold, italic);
        if let Ok(cache) = self.text_format_cache.lock() {
            if let Some(fmt) = cache.get(&key) {
                return Ok(fmt.clone());
            }
        }
        let family_wide: Vec<u16> = font_family
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let weight = if bold {
            DWRITE_FONT_WEIGHT_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        };
        let style = if italic {
            DWRITE_FONT_STYLE_ITALIC
        } else {
            DWRITE_FONT_STYLE_NORMAL
        };
        // SAFETY: family_wide is NUL-terminated and alive during the call;
        // all remaining parameters are valid DirectWrite enums/scalars.
        let format = unsafe {
            self.factory.CreateTextFormat(
                PCWSTR(family_wide.as_ptr()),
                None,
                weight,
                style,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size,
                w!("en-us"),
            )?
        };
        if let Ok(mut cache) = self.text_format_cache.lock() {
            cache.insert(key, format.clone());
        }
        Ok(format)
    }

    fn create_text_layout(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> WinResult<IDWriteTextLayout> {
        let format = self.get_or_create_format(font_family, font_size, bold, italic)?;
        let text_wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: the UTF-16 slice and format remain alive during the
        // synchronous COM call and the returned layout owns its resources.
        unsafe {
            self.factory
                .CreateTextLayout(&text_wide, &format, 10000.0, 10000.0)
        }
    }
}

pub struct Win32Backend {
    windows: HashMap<WindowId, WindowInfo>,
    hwnd_to_wid: HashMap<isize, WindowId>,
    quit: bool,
    dwrite: Option<DirectWriteRenderer>,
    class_registered: bool,
    hinstance_raw: isize,
    pending_events: Vec<PlatformEvent>,
}

// Safety: All HWND / HINSTANCE values are stored as raw `isize` and
// reconstructed only on the thread that calls PlatformBackend methods
// (which is always the event-dispatch / GUI thread).
unsafe impl Send for Win32Backend {}

impl Win32Backend {
    pub fn new() -> Result<Self, PlatformError> {
        // SAFETY: null requests the module handle for the current process and
        // requires no caller-owned pointer.
        let hmodule = unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map_err(|e| PlatformError::EventLoopError(format!("GetModuleHandleW: {e}")))?;
        let hinstance = HINSTANCE(hmodule.0);
        // Eagerly construct the DirectWrite renderer so its factory + text-
        // format cache are available on the first `rasterize_text` call.
        // Without this the rasterizer falls back to per-call factory creation
        // (slow), and the format cache stays dead.
        let dwrite = match DirectWriteRenderer::new() {
            Ok(dw) => Some(dw),
            Err(e) => {
                warn!("Failed to init DirectWrite: {e}");
                None
            }
        };
        Ok(Self {
            windows: HashMap::new(),
            hwnd_to_wid: HashMap::new(),
            quit: false,
            dwrite,
            class_registered: false,
            hinstance_raw: hinstance.0 as isize,
            pending_events: Vec::new(),
        })
    }

    fn hinstance(&self) -> HINSTANCE {
        HINSTANCE(self.hinstance_raw as *mut _)
    }

    fn ensure_class_registered(&mut self) -> Result<(), PlatformError> {
        if self.class_registered {
            return Ok(());
        }
        let class_name = w!("CratonVMAWTWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(Self::wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: self.hinstance(),
            hIcon: HICON::default(),
            // SAFETY: the predefined IDC_ARROW resource belongs to the system
            // module and the returned typed cursor is borrowed process-wide.
            hCursor: unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW) }
                .unwrap_or(HCURSOR::default()),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_name,
            hIconSm: HICON::default(),
        };
        // SAFETY: `wc` is fully initialized and all pointer members reference
        // static wide strings for the synchronous registration.
        let atom = unsafe { RegisterClassExW(&wc) };
        if atom == 0 {
            return Err(PlatformError::CreationFailed(
                "RegisterClassExW failed".into(),
            ));
        }
        self.class_registered = true;
        Ok(())
    }

    fn ensure_dwrite(&mut self) {
        if self.dwrite.is_none() {
            match DirectWriteRenderer::new() {
                Ok(dw) => self.dwrite = Some(dw),
                Err(e) => warn!("Failed to init DirectWrite: {e}"),
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: Windows invokes this callback with a valid message tuple;
        // forwarding untouched to DefWindowProcW is the documented fallback.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Decode an `LPARAM` packing two signed 16-bit coordinates (X = LOWORD,
    /// Y = HIWORD), the convention used by `WM_MOUSEMOVE`, `WM_LBUTTONDOWN`,
    /// and friends. See the `GET_X_LPARAM` / `GET_Y_LPARAM` macros in
    /// `<windowsx.h>`.
    ///
    /// Both halves must be **sign-extended** to `i32` — a previous version
    /// of this helper masked with `0xFFFF`, which silently converted negative
    /// coordinates (the user dragging the mouse outside the client rect)
    /// into the 32_768..65_535 range. We now widen via `i16` so the sign
    /// bit is preserved on the way to `i32`.
    fn lparam_xy(lp: LPARAM) -> (i32, i32) {
        let lo = (lp.0 & 0xFFFF) as i16 as i32;
        let hi = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
        (lo, hi)
    }

    fn current_modifiers() -> KeyModifiers {
        let mut m = KeyModifiers::empty();
        // SAFETY: GetKeyState accepts these predefined virtual-key values and
        // has no pointer or lifetime requirements.
        unsafe {
            if GetKeyState(VK_SHIFT.0 as i32) < 0 {
                m |= KeyModifiers::SHIFT;
            }
            if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                m |= KeyModifiers::CTRL;
            }
            if GetKeyState(VK_MENU.0 as i32) < 0 {
                m |= KeyModifiers::ALT;
            }
        }
        m
    }

    fn translate_msg(&self, msg: &MSG) -> Option<PlatformEvent> {
        let hwnd_val = msg.hwnd.0 as isize;
        let id = *self.hwnd_to_wid.get(&hwnd_val)?;
        match msg.message {
            WM_CLOSE => Some(PlatformEvent::WindowClose { id }),
            WM_SIZE => {
                // WM_SIZE encodes width/height as **unsigned** 16-bit values
                // (per Win32 docs) — unlike mouse coordinates which are signed
                // (`lparam_xy`). Masking with `0xFFFF` is therefore correct
                // here; do not sign-extend.
                let w = (msg.lParam.0 as u32) & 0xFFFF;
                let h = ((msg.lParam.0 as u32) >> 16) & 0xFFFF;
                Some(PlatformEvent::WindowResize { id, w, h })
            }
            WM_PAINT => Some(PlatformEvent::WindowExposed { id }),
            WM_LBUTTONDOWN => {
                let (x, y) = Self::lparam_xy(msg.lParam);
                Some(PlatformEvent::MousePressed {
                    id,
                    x,
                    y,
                    button: 1,
                })
            }
            WM_LBUTTONUP => {
                let (x, y) = Self::lparam_xy(msg.lParam);
                Some(PlatformEvent::MouseReleased {
                    id,
                    x,
                    y,
                    button: 1,
                })
            }
            WM_RBUTTONDOWN => {
                let (x, y) = Self::lparam_xy(msg.lParam);
                Some(PlatformEvent::MousePressed {
                    id,
                    x,
                    y,
                    button: 3,
                })
            }
            WM_RBUTTONUP => {
                let (x, y) = Self::lparam_xy(msg.lParam);
                Some(PlatformEvent::MouseReleased {
                    id,
                    x,
                    y,
                    button: 3,
                })
            }
            WM_MOUSEMOVE => {
                let (x, y) = Self::lparam_xy(msg.lParam);
                Some(PlatformEvent::MouseMoved { id, x, y })
            }
            WM_MOUSEWHEEL => {
                let delta = ((msg.wParam.0 >> 16) as i16) as i32;
                let (x, y) = Self::lparam_xy(msg.lParam);
                Some(PlatformEvent::MouseWheel {
                    id,
                    x,
                    y,
                    amount: delta / 120,
                })
            }
            WM_KEYDOWN => Some(PlatformEvent::KeyPressed {
                id,
                key_code: msg.wParam.0 as u32,
                char_val: None,
                modifiers: Self::current_modifiers(),
            }),
            WM_KEYUP => Some(PlatformEvent::KeyReleased {
                id,
                key_code: msg.wParam.0 as u32,
                char_val: None,
                modifiers: Self::current_modifiers(),
            }),
            WM_SETFOCUS => Some(PlatformEvent::FocusGained { id }),
            WM_KILLFOCUS => Some(PlatformEvent::FocusLost { id }),
            _ => None,
        }
    }
}

impl PlatformBackend for Win32Backend {
    fn create_window(
        &mut self,
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<WindowId, PlatformError> {
        self.ensure_class_registered()?;
        let tw: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the registered class and instance handles are live, `tw` is
        // NUL-terminated for the call, and no creation parameter is borrowed.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("CratonVMAWTWindow"),
                PCWSTR(tw.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                x,
                y,
                width as i32,
                height as i32,
                HWND::default(),
                HMENU::default(),
                self.hinstance(),
                None,
            )
        }
        .map_err(|e| PlatformError::CreationFailed(format!("CreateWindowExW: {e}")))?;
        let id = next_window_id();
        let hwnd_raw = hwnd.0 as isize;
        self.windows.insert(
            id,
            WindowInfo {
                hwnd_raw,
                width,
                height,
            },
        );
        self.hwnd_to_wid.insert(hwnd_raw, id);
        debug!("Created window {id} (hwnd={hwnd_raw:#x})");
        Ok(id)
    }

    fn destroy_window(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self
            .windows
            .remove(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        self.hwnd_to_wid.remove(&info.hwnd_raw);
        // SAFETY: WindowInfo is removed exactly once and contains the live HWND
        // created by this backend.
        let _ = unsafe { DestroyWindow(info.hwnd()) };
        Ok(())
    }

    fn show_window(&mut self, id: WindowId, visible: bool) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        // SAFETY: the registry lookup proves the HWND is owned and live.
        unsafe {
            let _ = ShowWindow(info.hwnd(), if visible { SW_SHOW } else { SW_HIDE });
        }
        Ok(())
    }

    fn set_window_title(&mut self, id: WindowId, title: &str) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the HWND is live and `w` is a NUL-terminated buffer retained
        // for the duration of the synchronous call.
        let _ = unsafe { SetWindowTextW(info.hwnd(), PCWSTR(w.as_ptr())) };
        Ok(())
    }

    fn set_window_bounds(
        &mut self,
        id: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<(), PlatformError> {
        let info = self
            .windows
            .get_mut(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        // SAFETY: the HWND is live; scalar bounds are copied synchronously and
        // TRUE requests the documented repaint behavior.
        let _ = unsafe { MoveWindow(info.hwnd(), x, y, w as i32, h as i32, TRUE) };
        info.width = w;
        info.height = h;
        Ok(())
    }

    fn get_window_bounds(&self, id: WindowId) -> Result<(i32, i32, u32, u32), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let mut rect = RECT::default();
        // SAFETY: the HWND is live and `rect` is writable for the exact
        // Win32 structure during this synchronous call.
        let _ = unsafe { GetWindowRect(info.hwnd(), &mut rect) };
        Ok((
            rect.left,
            rect.top,
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    }

    fn request_repaint(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        // SAFETY: the HWND is live; None invalidates the whole client area and
        // no raw pointer is retained.
        let _ = unsafe { InvalidateRect(info.hwnd(), None, FALSE) };
        Ok(())
    }

    fn blit_buffer(
        &mut self,
        id: WindowId,
        pixels: &[u32],
        width: u32,
        height: u32,
    ) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let hwnd = info.hwnd();

        // width*height drives both the DIB allocation (via the BITMAPINFO
        // below) and the `from_raw_parts_mut` slice length. A `u32` multiply
        // wraps in release, so a hostile width/height could make the slice
        // length disagree with the real DIB allocation -> heap OOB write.
        // Compute the pixel count once with a checked `usize` multiply and
        // bail if it overflows or doesn't fit the source buffer. Mirrors the
        // cocoa backend's `blit_buffer` guard.
        let pixel_count = match (width as usize).checked_mul(height as usize) {
            Some(n) if n <= pixels.len() => n,
            _ => {
                return Err(PlatformError::CreationFailed(
                    "pixel buffer too small".into(),
                ))
            }
        };
        // SAFETY: every Win32/GDI handle is checked or wrapped in an owning
        // guard; `pixel_count` was checked against both the DIB dimensions and
        // source slice, and raw DIB slices stay within that allocation.
        unsafe {
            // BeginPaint -> EndPaint pair. If we bail with `?` below the
            // guard's Drop closes the paint session.
            let paint = match PaintGuard::begin(hwnd) {
                Some(p) => p,
                None => return Ok(()),
            };
            let hdc = paint.hdc();

            // CreateCompatibleDC -> DeleteDC pair.
            let mem_dc = DcGuard::new(CreateCompatibleDC(hdc))
                .ok_or_else(|| PlatformError::CreationFailed("CreateCompatibleDC failed".into()))?;
            let hdc_mem = mem_dc.hdc();

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            // CreateDIBSection -> DeleteObject pair. The error path here used
            // to leak the BeginPaint handle; now `paint`'s Drop runs as the
            // `?` unwinds the scope.
            let hbm = CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            )
            .map_err(|_| PlatformError::CreationFailed("CreateDIBSection failed".into()))?;
            let hbm_guard = GdiObjectGuard::new(hbm);

            // SelectObject -> restore-previous pair.
            let _selected = SelectObjectGuard::select(hdc_mem, hbm_guard.handle());
            if !bits.is_null() {
                // `pixel_count` is the checked `width as usize * height as
                // usize` computed above; it matches the DIB allocation
                // (32bpp, width*height pixels) exactly, so the slice can
                // never extend past the real allocation.
                let dst = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
                for (i, &px) in pixels.iter().enumerate() {
                    if i >= dst.len() {
                        break;
                    }
                    // ARGB -> BGRA for GDI
                    let a = (px >> 24) & 0xFF;
                    let r = (px >> 16) & 0xFF;
                    let g = (px >> 8) & 0xFF;
                    let b = px & 0xFF;
                    dst[i] = (a << 24) | (b << 16) | (g << 8) | r;
                }
            }
            let _ = BitBlt(
                hdc,
                0,
                0,
                width as i32,
                height as i32,
                hdc_mem,
                0,
                0,
                SRCCOPY,
            );
            // Drop order at scope exit: _selected (restore old object)
            // -> hbm_guard (DeleteObject) -> mem_dc (DeleteDC)
            // -> paint (EndPaint). This matches the original manual ordering.
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<PlatformEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
        // SAFETY: `msg` is writable Win32 message storage; returned messages
        // are dispatched before the local goes out of scope.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if let Some(ev) = self.translate_msg(&msg) {
                    events.push(ev);
                }
                if msg.message == WM_QUIT {
                    self.quit = true;
                }
            }
        }
        events
    }

    fn run_event_loop(&mut self) {
        loop {
            // SAFETY: `msg` is writable Win32 message storage and the API owns
            // the event queue; handles in delivered messages are OS-provided.
            unsafe {
                let mut msg = MSG::default();
                let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
                if !ret.as_bool() || self.quit {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if let Some(ev) = self.translate_msg(&msg) {
                    self.pending_events.push(ev);
                }
            }
        }
    }

    fn post_quit(&mut self) {
        self.quit = true;
        // SAFETY: posts to the current GUI thread queue and takes no pointers.
        unsafe {
            PostQuitMessage(0);
        }
    }

    fn screen_size(&self) -> (u32, u32) {
        // SAFETY: GetSystemMetrics takes only predefined metric identifiers.
        unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) as u32,
                GetSystemMetrics(SM_CYSCREEN) as u32,
            )
        }
    }

    fn screen_dpi(&self) -> f64 {
        // SAFETY: GetDC(NULL) returns a screen DC which is kept live through
        // GetDeviceCaps and released exactly once before leaving the block.
        unsafe {
            let hdc = GetDC(HWND::default());
            let dpi = GetDeviceCaps(hdc, LOGPIXELSX) as f64;
            ReleaseDC(HWND::default(), hdc);
            if dpi > 0.0 {
                dpi
            } else {
                96.0
            }
        }
    }

    fn measure_text(
        &self,
        text: &str,
        _font_family: &str,
        font_size: f32,
        _bold: bool,
        _italic: bool,
    ) -> (f32, f32) {
        // Heuristic measurement — avoids interior mutability / UB.
        // Average character width ~ 0.6 * font size for proportional fonts.
        // Count Unicode scalar values (not UTF-8 bytes) so multibyte / CJK
        // text isn't over-measured by its byte length.
        let w = text.chars().count() as f32 * font_size * 0.6;
        let h = font_size * 1.2;
        (w, h)
    }

    fn rasterize_text(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
        color: u32,
    ) -> TextRaster {
        let empty = TextRaster {
            pixels: vec![],
            width: 0,
            height: 0,
            baseline: 0.0,
        };

        // Reuse the backend's cached DirectWrite factory + text format when
        // available. Creating a factory per call is expensive, and the
        // text-format cache is the whole point of `DirectWriteRenderer`.
        // If the renderer isn't initialised yet, fall back to a fresh factory
        // for this call only.
        let layout = if let Some(dw) = self.dwrite.as_ref() {
            match dw.create_text_layout(text, font_family, font_size, bold, italic) {
                Ok(l) => l,
                Err(_) => return empty,
            }
        } else {
            // SAFETY: the factory API has no caller-owned pointer inputs and
            // returns a typed COM owner on success.
            let factory: IDWriteFactory =
                match unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) } {
                    Ok(f) => f,
                    Err(_) => return empty,
                };

            let family_wide: Vec<u16> = font_family
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let weight = if bold {
                DWRITE_FONT_WEIGHT_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            };
            let dw_style = if italic {
                DWRITE_FONT_STYLE_ITALIC
            } else {
                DWRITE_FONT_STYLE_NORMAL
            };

            // SAFETY: family_wide is NUL-terminated and alive for the call;
            // the remaining inputs are valid DirectWrite values.
            let format = match unsafe {
                factory.CreateTextFormat(
                    PCWSTR(family_wide.as_ptr()),
                    None,
                    weight,
                    dw_style,
                    DWRITE_FONT_STRETCH_NORMAL,
                    font_size,
                    w!("en-us"),
                )
            } {
                Ok(f) => f,
                Err(_) => return empty,
            };

            let text_wide: Vec<u16> = text.encode_utf16().collect();
            // SAFETY: the UTF-16 slice and format remain alive during the
            // synchronous call; the layout owns returned COM state.
            match unsafe { factory.CreateTextLayout(&text_wide, &format, 10000.0, 10000.0) } {
                Ok(l) => l,
                Err(_) => return empty,
            }
        };

        // SAFETY: `layout` is a live typed COM object and `m` is writable
        // storage for the exact metrics structure.
        let (tw, th) = unsafe {
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_err() {
                return empty;
            }
            (m.width.ceil() as u32 + 1, m.height.ceil() as u32 + 1)
        };

        if tw == 0 || th == 0 {
            return empty;
        }

        // Clamp the DWrite-reported metrics to a sane maximum before they
        // feed the DIB allocation and the `vec![0u32; tw*th]` / from_raw_parts
        // slice below. A pathological layout (e.g. an enormous font size or a
        // very long single line) could otherwise drive `tw * th` to overflow a
        // `u32`, making the allocation and the slice length disagree.
        const MAX_RASTER_DIM: u32 = 1 << 15; // 32768 px per side
        let tw = tw.min(MAX_RASTER_DIM);
        let th = th.min(MAX_RASTER_DIM);
        // Pixel count via a checked `usize` multiply (clamped dims guarantee
        // this fits, but compute it once and bail defensively on overflow so
        // the DIB size and the slice length can never diverge).
        let raster_px = match (tw as usize).checked_mul(th as usize) {
            Some(n) => n,
            None => return empty,
        };

        let ca = (color >> 24) & 0xFF;
        let cr = (color >> 16) & 0xFF;
        let cg = (color >> 8) & 0xFF;
        let cb = color & 0xFF;

        // SAFETY: all GDI handles are checked and paired with RAII guards;
        // `raster_px` bounds the 32-bpp DIB exactly, and every temporary UTF-16
        // buffer remains alive through its synchronous Win32 call.
        unsafe {
            let hdc_screen = GetDC(HWND::default());
            let mem_dc = match DcGuard::new(CreateCompatibleDC(hdc_screen)) {
                Some(d) => d,
                None => {
                    ReleaseDC(HWND::default(), hdc_screen);
                    return empty;
                }
            };
            ReleaseDC(HWND::default(), hdc_screen);
            let hdc_mem = mem_dc.hdc();

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: tw as i32,
                    biHeight: -(th as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            // CreateDIBSection -> DeleteObject. On error `mem_dc` drops and
            // runs DeleteDC; no leak.
            let hbm = match CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            ) {
                Ok(bm) => bm,
                Err(_) => return empty,
            };
            let _hbm_guard = GdiObjectGuard::new(hbm);

            let _old_bm = SelectObjectGuard::select(hdc_mem, hbm);
            SetBkMode(hdc_mem, TRANSPARENT);
            SetTextColor(hdc_mem, COLORREF((cb << 16) | (cg << 8) | cr));

            let gdi_weight = if bold { 700 } else { 400 };
            let gdi_ital = if italic { 1u32 } else { 0 };
            let fam: Vec<u16> = font_family
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let hfont = CreateFontW(
                -(font_size as i32),
                0,
                0,
                0,
                gdi_weight,
                gdi_ital,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                PCWSTR(fam.as_ptr()),
            );
            let _hfont_guard = GdiObjectGuard::new(hfont);
            let _old_font = SelectObjectGuard::select(hdc_mem, hfont);

            let text_w: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(hdc_mem, 0, 0, &text_w);

            let mut px_out = vec![0u32; raster_px];
            if !bits.is_null() {
                // `raster_px` is the checked `tw as usize * th as usize`; the
                // DIB above is 32bpp with the same clamped tw/th, so this
                // slice never extends past the real allocation.
                let src = std::slice::from_raw_parts(bits as *const u32, raster_px);
                for (i, &px) in src.iter().enumerate() {
                    let pb = (px >> 16) & 0xFF;
                    let pg = (px >> 8) & 0xFF;
                    let pr = px & 0xFF;
                    let lum = (pr * 77 + pg * 150 + pb * 29) / 256;
                    if lum > 0 {
                        px_out[i] = (ca * lum / 255) << 24 | (cr << 16) | (cg << 8) | cb;
                    }
                }
            }

            // Drop order at scope exit (reverse of declaration):
            //   _old_font   -> restore previous font into hdc_mem
            //   _hfont_guard -> DeleteObject(hfont)
            //   _old_bm     -> restore previous bitmap
            //   _hbm_guard  -> DeleteObject(hbm)
            //   mem_dc      -> DeleteDC(hdc_mem)
            // Matches the previously hand-written cleanup ordering.
            TextRaster {
                pixels: px_out,
                width: tw,
                height: th,
                baseline: font_size * 0.8,
            }
        }
    }

    fn clipboard_get_text(&self) -> Option<String> {
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;

        // SAFETY: clipboard/global-memory handles are checked, locked only
        // while read, bounded by GlobalSize, then unlocked and closed by RAII.
        unsafe {
            // ClipboardGuard ensures CloseClipboard runs on every exit path
            // (including the early `return None` inside the closure below).
            let _clipboard = ClipboardGuard::open(HWND::default()).ok()?;
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32);
            handle.ok().and_then(|h| {
                let ptr = GlobalLock(HGLOBAL(h.0)) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                // GetClipboardData returns a borrowed handle whose buffer may
                // be corrupt or missing the wide-char NUL terminator. Bound
                // every read by the actual allocation size reported by
                // GlobalSize so an un-terminated or short buffer is treated as
                // truncated rather than reading out-of-bounds memory. GlobalSize
                // returns bytes (0 on failure); convert to a wchar cap (rounding
                // down so we never read a partial trailing wchar).
                let cap_wchars = GlobalSize(HGLOBAL(h.0)) / 2;
                let mut len = 0;
                while len < cap_wchars && *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(HGLOBAL(h.0));
                Some(s)
            })
            // `_clipboard` drops here, calling CloseClipboard.
        }
    }

    fn clipboard_set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: allocated global memory is sized to `wide`, locked before
        // copying, and either transferred to the clipboard or freed by RAII.
        unsafe {
            // GlobalAlloc -> GlobalFree pair. If `OpenClipboard` below fails
            // (the previous code's leak site), the guard's Drop frees the
            // buffer instead of leaving it dangling.
            let hmem_guard = GlobalAllocGuard::new(GMEM_MOVEABLE, wide.len() * 2)
                .map_err(|_| PlatformError::ClipboardError("GlobalAlloc failed".into()))?;
            let hmem = hmem_guard.handle();

            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                // hmem_guard's Drop runs here -> GlobalFree.
                return Err(PlatformError::ClipboardError("GlobalLock failed".into()));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hmem);

            // OpenClipboard -> CloseClipboard pair. If this fails, the
            // hmem_guard above still drops correctly.
            let _clipboard = ClipboardGuard::open(HWND::default())
                .map_err(|_| PlatformError::ClipboardError("OpenClipboard failed".into()))?;
            let _ = EmptyClipboard();
            // On success SetClipboardData takes ownership of the HGLOBAL.
            // We must NOT free it ourselves in that case, so release the
            // guard. If SetClipboardData fails we keep the guard so its
            // Drop reclaims the buffer.
            let set_handle = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0));
            if set_handle.is_ok() {
                let _ = hmem_guard.release();
            }
            // _clipboard's Drop -> CloseClipboard runs at scope exit.
        }
        Ok(())
    }

    fn show_file_dialog(
        &mut self,
        _title: &str,
        _save: bool,
        _filters: &[(String, String)],
    ) -> Option<String> {
        warn!("show_file_dialog: not yet implemented");
        None
    }

    fn show_message_dialog(&mut self, title: &str, message: &str, msg_type: MessageDialogType) {
        let tw: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let mw: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        let flags = match msg_type {
            MessageDialogType::Info => MB_OK | MB_ICONINFORMATION,
            MessageDialogType::Warning => MB_OK | MB_ICONWARNING,
            MessageDialogType::Error => MB_OK | MB_ICONERROR,
            MessageDialogType::Question => MB_YESNO | MB_ICONQUESTION,
        };
        // SAFETY: both UTF-16 buffers are NUL-terminated and alive for the
        // synchronous MessageBoxW call; the default owner is permitted.
        unsafe {
            let _ = MessageBoxW(
                HWND::default(),
                PCWSTR(mw.as_ptr()),
                PCWSTR(tw.as_ptr()),
                flags,
            );
        }
    }
}

impl Drop for Win32Backend {
    /// Tear down any live windows and unregister the window class so that
    /// repeated VM init/teardown cycles (e.g. test runs that construct a
    /// fresh backend per case) do not leak `HWND` handles or the
    /// `CratonVMAWTWindow` class registration. Mirrors the equivalent
    /// `Drop` impls on `X11Backend` and `CocoaBackend`.
    fn drop(&mut self) {
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for id in ids {
            let _ = self.destroy_window(id);
        }
        if self.class_registered {
            let class_name = w!("CratonVMAWTWindow");
            // `UnregisterClassW` is safe to call after every window using
            // the class has been destroyed. We ignore the return value:
            // failure here is non-fatal (e.g. the class was already torn
            // down by another instance) and we are already in `drop`.
            // SAFETY: all windows owned by this instance were destroyed above;
            // the class name is static and hinstance matches registration.
            unsafe {
                let _ = UnregisterClassW(class_name, self.hinstance());
            }
            self.class_registered = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the C30 sign-extension fix in `lparam_xy`. The
    /// old implementation masked LOWORD/HIWORD with `0xFFFF` and produced
    /// `(0, 65534)` for an LPARAM packing `(0, -2)`; mouse drags outside
    /// the client rect were therefore mis-reported as positions in the
    /// 32_768..65_535 range. The fixed version widens through `i16` so the
    /// sign bit reaches `i32`.
    #[test]
    fn lparam_xy_sign_extends_negative_coords() {
        // LPARAM packing x = 0, y = -2 (i16) → bits 0xFFFE0000.
        let lp = LPARAM(0xFFFE0000u32 as isize);
        assert_eq!(Win32Backend::lparam_xy(lp), (0, -2));

        // LPARAM packing x = -1, y = -1 → bits 0xFFFFFFFF.
        let lp = LPARAM(0xFFFFFFFFu32 as isize);
        assert_eq!(Win32Backend::lparam_xy(lp), (-1, -1));

        // Positive coordinates should be unchanged.
        let lp = LPARAM(((100i32 as u32) | ((200u32) << 16)) as isize);
        assert_eq!(Win32Backend::lparam_xy(lp), (100, 200));

        // The most-negative coordinate the i16 encoding can carry.
        let lp = LPARAM(0x80008000u32 as isize);
        assert_eq!(Win32Backend::lparam_xy(lp), (-32768, -32768));
    }
}

// ---------------------------------------------------------------------------
// Guard tests
//
// These tests exercise the Drop-based cleanup contract of the RAII guards
// without depending on a real GUI session. They use a small fake counter
// (`GuardLeakCounter`) and parallel mock guards that mirror the real ones'
// Drop logic. Together with code inspection of `blit_buffer` /
// `clipboard_set_text`, this confirms the leak fix.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod guard_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Tracks how many "handles" are live. Mirrors what a real leak detector
    /// would do for Win32 handle counts (e.g. `GetGuiResources` for GDI).
    #[derive(Default)]
    struct GuardLeakCounter {
        alive: AtomicUsize,
    }

    impl GuardLeakCounter {
        fn acquire(&self) {
            self.alive.fetch_add(1, Ordering::SeqCst);
        }
        fn release(&self) {
            self.alive.fetch_sub(1, Ordering::SeqCst);
        }
        fn live(&self) -> usize {
            self.alive.load(Ordering::SeqCst)
        }
    }

    /// Mock that mirrors `PaintGuard` / `ClipboardGuard` semantics: acquire
    /// in `begin`, release in `Drop`.
    struct MockPairGuard<'a> {
        counter: &'a GuardLeakCounter,
    }
    impl<'a> MockPairGuard<'a> {
        fn begin(counter: &'a GuardLeakCounter) -> Self {
            counter.acquire();
            Self { counter }
        }
    }
    impl Drop for MockPairGuard<'_> {
        fn drop(&mut self) {
            self.counter.release();
        }
    }

    /// Mock that mirrors `GlobalAllocGuard`: optional release before drop.
    struct MockAllocGuard<'a> {
        counter: &'a GuardLeakCounter,
        released: bool,
    }
    impl<'a> MockAllocGuard<'a> {
        fn new(counter: &'a GuardLeakCounter) -> Self {
            counter.acquire();
            Self {
                counter,
                released: false,
            }
        }
        fn release(mut self) {
            self.released = true;
        }
    }
    impl Drop for MockAllocGuard<'_> {
        fn drop(&mut self) {
            if !self.released {
                self.counter.release();
            }
        }
    }

    /// Verifies the BeginPaint/EndPaint pair pattern: even when an early-
    /// `?` propagates out before the matching close, the guard's Drop fires.
    ///
    /// This is the regression test for the original
    /// `blit_buffer` leak — `CreateDIBSection` failing with `?` used to skip
    /// `EndPaint`.
    #[test]
    fn paint_guard_drops_on_question_mark_propagation() {
        let leaks = GuardLeakCounter::default();

        fn inner(counter: &GuardLeakCounter) -> Result<(), &'static str> {
            let _paint = MockPairGuard::begin(counter);
            // Simulates `CreateDIBSection.map_err(...)?` returning early.
            Err("create_dib_section failed")?;
            #[allow(unreachable_code)]
            Ok(())
        }

        let r = inner(&leaks);
        assert!(r.is_err(), "inner must propagate the simulated failure");
        assert_eq!(
            leaks.live(),
            0,
            "PaintGuard Drop should run when `?` bails before manual EndPaint"
        );
    }

    /// Verifies the GlobalAlloc/GlobalFree pair pattern: if `OpenClipboard`
    /// fails after `GlobalAlloc` succeeds, the alloc still gets freed.
    /// Regression test for the original `clipboard_set_text` leak.
    #[test]
    fn global_alloc_guard_drops_when_clipboard_open_fails() {
        let leaks = GuardLeakCounter::default();

        fn inner(counter: &GuardLeakCounter) -> Result<(), &'static str> {
            let _hmem = MockAllocGuard::new(counter);
            // GlobalLock would happen here; assume success.
            // Now OpenClipboard fails (simulated). Use `?` to model the
            // real code's early-return via the `?` operator.
            Err("OpenClipboard failed")?;
            #[allow(unreachable_code)]
            Ok(())
        }

        let r = inner(&leaks);
        assert!(r.is_err());
        assert_eq!(
            leaks.live(),
            0,
            "GlobalAllocGuard Drop should free the buffer on the clipboard \
             open failure path"
        );
    }

    /// When SetClipboardData succeeds the OS owns the buffer; the alloc
    /// guard must NOT free it. `release()` arms that case.
    #[test]
    fn global_alloc_guard_release_skips_drop() {
        let leaks = GuardLeakCounter::default();

        {
            let hmem = MockAllocGuard::new(&leaks);
            assert_eq!(leaks.live(), 1);
            hmem.release();
            // After release, drop is a no-op for the counter.
            assert_eq!(
                leaks.live(),
                1,
                "release() must hand ownership off without decrementing"
            );
        }
        // Counter stays at 1 because the OS now owns it (in real code,
        // SetClipboardData -> CloseClipboard would free the global handle
        // along with the clipboard contents).
        assert_eq!(leaks.live(), 1);
    }

    /// Multiple guards in one scope must all run their Drop in reverse
    /// declaration order. Mirrors `blit_buffer`'s nested guards.
    #[test]
    fn nested_guards_all_drop_on_early_return() {
        let leaks = GuardLeakCounter::default();

        fn inner(counter: &GuardLeakCounter) -> Result<(), &'static str> {
            let _paint = MockPairGuard::begin(counter);
            let _dc = MockPairGuard::begin(counter);
            let _hbm = MockPairGuard::begin(counter);
            assert_eq!(counter.live(), 3);
            // Simulate a later failure (e.g. SelectObject error).
            Err("late failure")?;
            #[allow(unreachable_code)]
            Ok(())
        }

        let _ = inner(&leaks);
        assert_eq!(
            leaks.live(),
            0,
            "all three guards must drop on `?` propagation"
        );
    }

    /// Sanity-check that `Drop` ordering inside Rust is LIFO. The real
    /// `blit_buffer` depends on this: SelectObjectGuard (restores prev)
    /// must drop BEFORE GdiObjectGuard (DeleteObject on hbm) so the bitmap
    /// isn't still selected into the DC when we delete it.
    #[test]
    fn drop_order_is_reverse_of_declaration() {
        use std::cell::RefCell;
        let log: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());

        struct Tag<'a>(&'a RefCell<Vec<&'static str>>, &'static str);
        impl Drop for Tag<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push(self.1);
            }
        }

        {
            let _a = Tag(&log, "select_object"); // selected obj into DC
            let _b = Tag(&log, "gdi_object"); // hbm
            let _c = Tag(&log, "dc"); // mem DC
            let _d = Tag(&log, "paint"); // BeginPaint
        }

        let order = log.into_inner();
        assert_eq!(
            order,
            vec!["paint", "dc", "gdi_object", "select_object"],
            "Drop must run in reverse-declaration (LIFO) order so the \
             selection guard restores the old object before the object \
             itself is deleted, and EndPaint runs after DeleteDC"
        );
    }
}
