//! Small click-through "Clip saved" toast in the corner of the game's monitor.
//! (Shows over borderless/windowed games; exclusive fullscreen gets the sound cue only.)

use crate::games;
use std::sync::Mutex;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

static TEXT: Mutex<(String, bool)> = Mutex::new((String::new(), true));
const W: i32 = 380;
const H: i32 = 64;

pub struct Toast(HWND);

impl Toast {
    pub fn new() -> Option<Toast> {
        unsafe {
            let inst = GetModuleHandleW(None).ok()?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: inst.into(),
                lpszClassName: w!("CrispyClipperToast"),
                hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("CrispyClipperToast"),
                PCWSTR::null(),
                WS_POPUP,
                0, 0, W, H,
                None, None, Some(inst.into()), None,
            )
            .ok()?;
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
            Some(Toast(hwnd))
        }
    }

    pub fn show(&self, text: &str, ok: bool) {
        *TEXT.lock().unwrap() = (text.to_string(), ok);
        let m = games::monitor_rect(games::foreground_monitor());
        unsafe {
            let _ = SetWindowPos(
                self.0, Some(HWND_TOPMOST), m.right - W - 24, m.top + 24, W, H,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.0), None, true);
            SetTimer(Some(self.0), 1, 2500, None);
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let (text, ok) = TEXT.lock().unwrap().clone();
                let bg = CreateSolidBrush(COLORREF(0x001e1a18)); // BGR
                let accent = CreateSolidBrush(COLORREF(if ok { 0x004646ff } else { 0x000080ff }));
                FillRect(dc, &RECT { left: 0, top: 0, right: W, bottom: H }, bg);
                FillRect(dc, &RECT { left: 0, top: 0, right: 6, bottom: H }, accent);
                let font = CreateFontW(
                    22, 0, 0, 0, 600, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY, (VARIABLE_PITCH.0 | FF_SWISS.0) as u32, w!("Segoe UI"),
                );
                let old = SelectObject(dc, font.into());
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0x00f0f0f0));
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                if !wide.is_empty() {
                    let mut r = RECT { left: 22, top: 0, right: W - 12, bottom: H };
                    DrawTextW(dc, &mut wide, &mut r, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
                }
                SelectObject(dc, old);
                let _ = DeleteObject(font.into());
                let _ = DeleteObject(bg.into());
                let _ = DeleteObject(accent.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_TIMER => {
                let _ = KillTimer(Some(hwnd), 1);
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
