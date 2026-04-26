//! Win32 platform backend -- Windows windowing via the `windows` crate,
//! GDI for buffer blitting, and DirectWrite for text rasterization.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, warn};

use windows::core::{w, PCWSTR};
type WinResult<T> = windows::core::Result<T>;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::backend::*;

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
    text_format_cache: HashMap<String, IDWriteTextFormat>,
}

impl DirectWriteRenderer {
    fn new() -> WinResult<Self> {
        let factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        Ok(Self {
            factory,
            text_format_cache: HashMap::new(),
        })
    }

    fn get_or_create_format(
        &mut self,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> WinResult<IDWriteTextFormat> {
        let key = format!("{}:{}:{}:{}", font_family, font_size, bold, italic);
        if let Some(fmt) = self.text_format_cache.get(&key) {
            return Ok(fmt.clone());
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
        self.text_format_cache.insert(key, format.clone());
        Ok(format)
    }

    fn create_text_layout(
        &mut self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> WinResult<IDWriteTextLayout> {
        let format =
            self.get_or_create_format(font_family, font_size, bold, italic)?;
        let text_wide: Vec<u16> = text.encode_utf16().collect();
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
        let hmodule = unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map_err(|e| PlatformError::EventLoopError(format!("GetModuleHandleW: {e}")))?;
        let hinstance = HINSTANCE(hmodule.0);
        Ok(Self {
            windows: HashMap::new(),
            hwnd_to_wid: HashMap::new(),
            quit: false,
            dwrite: None,
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
        let class_name = w!("RustJVMAWTWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(Self::wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: self.hinstance(),
            hIcon: HICON::default(),
            hCursor: unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW) }
                .unwrap_or(HCURSOR::default()),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_name,
            hIconSm: HICON::default(),
        };
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
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn lparam_xy(lp: LPARAM) -> (i32, i32) {
        ((lp.0 as i32) & 0xFFFF, ((lp.0 as i32) >> 16) & 0xFFFF)
    }

    fn current_modifiers() -> KeyModifiers {
        let mut m = KeyModifiers::empty();
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
        let tw: Vec<u16> = title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("RustJVMAWTWindow"),
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
        let _ = unsafe { DestroyWindow(info.hwnd()) };
        Ok(())
    }

    fn show_window(&mut self, id: WindowId, visible: bool) -> Result<(), PlatformError> {
        let info = self
            .windows
            .get(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        unsafe {
            let _ = ShowWindow(
                info.hwnd(),
                if visible { SW_SHOW } else { SW_HIDE },
            );
        }
        Ok(())
    }

    fn set_window_title(&mut self, id: WindowId, title: &str) -> Result<(), PlatformError> {
        let info = self
            .windows
            .get(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        let w: Vec<u16> = title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
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
        let _ = unsafe { MoveWindow(info.hwnd(), x, y, w as i32, h as i32, TRUE) };
        info.width = w;
        info.height = h;
        Ok(())
    }

    fn get_window_bounds(&self, id: WindowId) -> Result<(i32, i32, u32, u32), PlatformError> {
        let info = self
            .windows
            .get(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(info.hwnd(), &mut rect) };
        Ok((
            rect.left,
            rect.top,
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    }

    fn request_repaint(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self
            .windows
            .get(&id)
            .ok_or(PlatformError::WindowNotFound)?;
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
        let info = self
            .windows
            .get(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        let hwnd = info.hwnd();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if hdc.is_invalid() {
                return Ok(());
            }
            let hdc_mem = CreateCompatibleDC(hdc);
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
            let hbm = CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            )
            .map_err(|_| {
                PlatformError::CreationFailed("CreateDIBSection failed".into())
            })?;
            let old = SelectObject(hdc_mem, hbm);
            if !bits.is_null() {
                let dst = std::slice::from_raw_parts_mut(
                    bits as *mut u32,
                    (width * height) as usize,
                );
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
            SelectObject(hdc_mem, old);
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(hdc_mem);
            let _ = EndPaint(hwnd, &ps);
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<PlatformEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
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
        unsafe {
            PostQuitMessage(0);
        }
    }

    fn screen_size(&self) -> (u32, u32) {
        unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) as u32,
                GetSystemMetrics(SM_CYSCREEN) as u32,
            )
        }
    }

    fn screen_dpi(&self) -> f64 {
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
        let w = text.len() as f32 * font_size * 0.6;
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

        // Create a fresh DirectWrite factory per-call to avoid &mut self UB.
        let factory: IDWriteFactory = match unsafe {
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
        } {
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
        let layout = match unsafe {
            factory.CreateTextLayout(&text_wide, &format, 10000.0, 10000.0)
        } {
            Ok(l) => l,
            Err(_) => return empty,
        };

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

        let ca = (color >> 24) & 0xFF;
        let cr = (color >> 16) & 0xFF;
        let cg = (color >> 8) & 0xFF;
        let cb = color & 0xFF;

        unsafe {
            let hdc_screen = GetDC(HWND::default());
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            ReleaseDC(HWND::default(), hdc_screen);

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
            let hbm = match CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            ) {
                Ok(bm) => bm,
                Err(_) => {
                    let _ = DeleteDC(hdc_mem);
                    return empty;
                }
            };

            let old_bm = SelectObject(hdc_mem, hbm);
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
            let old_font = SelectObject(hdc_mem, hfont);

            let text_w: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(hdc_mem, 0, 0, &text_w);

            let mut px_out = vec![0u32; (tw * th) as usize];
            if !bits.is_null() {
                let src =
                    std::slice::from_raw_parts(bits as *const u32, (tw * th) as usize);
                for (i, &px) in src.iter().enumerate() {
                    let pb = (px >> 16) & 0xFF;
                    let pg = (px >> 8) & 0xFF;
                    let pr = px & 0xFF;
                    let lum = (pr * 77 + pg * 150 + pb * 29) / 256;
                    if lum > 0 {
                        px_out[i] =
                            (ca * lum / 255) << 24 | (cr << 16) | (cg << 8) | cb;
                    }
                }
            }

            SelectObject(hdc_mem, old_font);
            let _ = DeleteObject(hfont);
            SelectObject(hdc_mem, old_bm);
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(hdc_mem);

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

        unsafe {
            if OpenClipboard(HWND::default()).is_err() {
                return None;
            }
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32);
            let result = handle.ok().and_then(|h| {
                let ptr = GlobalLock(HGLOBAL(h.0)) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(HGLOBAL(h.0));
                Some(s)
            });
            let _ = CloseClipboard();
            result
        }
    }

    fn clipboard_set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;

        let wide: Vec<u16> = text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let hmem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)
                .map_err(|_| PlatformError::ClipboardError("GlobalAlloc failed".into()))?;
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(hmem);
                return Err(PlatformError::ClipboardError(
                    "GlobalLock failed".into(),
                ));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hmem);
            OpenClipboard(HWND::default())
                .map_err(|_| PlatformError::ClipboardError("OpenClipboard failed".into()))?;
            let _ = EmptyClipboard();
            let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0));
            let _ = CloseClipboard();
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

    fn show_message_dialog(
        &mut self,
        title: &str,
        message: &str,
        msg_type: MessageDialogType,
    ) {
        let tw: Vec<u16> = title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mw: Vec<u16> = message
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let flags = match msg_type {
            MessageDialogType::Info => MB_OK | MB_ICONINFORMATION,
            MessageDialogType::Warning => MB_OK | MB_ICONWARNING,
            MessageDialogType::Error => MB_OK | MB_ICONERROR,
            MessageDialogType::Question => MB_YESNO | MB_ICONQUESTION,
        };
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
