//! Everything that talks to Windows: listing windows and monitors, moving windows
//! to the projector and drawing the live previews (DWM thumbnails).

use std::collections::HashMap;
use std::mem::size_of;

use windows::core::{w, BOOL, PWSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Window handle that can be freely copied and compared.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hwnd(pub isize);

impl Hwnd {
    fn h(self) -> HWND {
        HWND(self.0 as *mut _)
    }

    fn from(h: HWND) -> Self {
        Hwnd(h.0 as isize)
    }

    pub fn exists(self) -> bool {
        unsafe { IsWindow(Some(self.h())).as_bool() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    fn from_win(r: RECT) -> Self {
        Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
    }

    /// The center of this rectangle falls inside `o`.
    pub fn center_in(&self, o: &Rect) -> bool {
        let (x, y) = ((self.left + self.right) / 2, (self.top + self.bottom) / 2);
        o.left <= x && x < o.right && o.top <= y && y < o.bottom
    }

    pub fn covers(&self, o: &Rect) -> bool {
        self.left <= o.left && self.top <= o.top && self.right >= o.right && self.bottom >= o.bottom
    }

    /// Area shared with `o`, in pixels.
    pub fn overlap_area(&self, o: &Rect) -> i32 {
        let w = self.right.min(o.right) - self.left.max(o.left);
        let h = self.bottom.min(o.bottom) - self.top.max(o.top);
        if w > 0 && h > 0 {
            w * h
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Open windows

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: Hwnd,
    pub title: String,
    pub exe: String,
    pub exe_path: String,
    pub minimized: bool,
    /// Microsoft Store apps (Photos, Movies & TV) live inside an "ApplicationFrameWindow".
    pub uwp: bool,
}

/// Lists the windows worth showing to the operator, in their stacking order.
pub fn list_windows() -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_windows_cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<WindowInfo>);
    if let Some(info) = window_info(hwnd) {
        out.push(info);
    }
    TRUE
}

fn window_info(hwnd: HWND) -> Option<WindowInfo> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) {
            return None;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return None;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let title = window_text(hwnd);
        let class = class_name(hwnd);
        if title.is_empty() || class == "Progman" || class == "WorkerW" {
            return None;
        }
        let pid = window_pid(hwnd);
        if pid == GetCurrentProcessId() {
            return None;
        }
        let uwp = class == "ApplicationFrameWindow";
        let exe_path = if uwp { uwp_exe(hwnd, pid) } else { process_path(pid) };
        let exe = exe_path.rsplit('\\').next().unwrap_or_default().to_string();
        let minimized = IsIconic(hwnd).as_bool();
        Some(WindowInfo { hwnd: Hwnd::from(hwnd), title, exe, exe_path, minimized, uwp })
    }
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok()
            && cloaked != 0
    }
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn process_path(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// The real process of a Store app belongs to the child window (CoreWindow), not to ApplicationFrameHost.
fn uwp_exe(frame: HWND, frame_pid: u32) -> String {
    struct Search {
        frame_pid: u32,
        found: u32,
    }
    unsafe extern "system" fn cb(child: HWND, lp: LPARAM) -> BOOL {
        let s = &mut *(lp.0 as *mut Search);
        let pid = window_pid(child);
        if pid != s.frame_pid {
            s.found = pid;
            return FALSE;
        }
        TRUE
    }
    let mut s = Search { frame_pid, found: 0 };
    unsafe {
        let _ = EnumChildWindows(Some(frame), Some(cb), LPARAM(&mut s as *mut _ as isize));
    }
    process_path(if s.found != 0 { s.found } else { frame_pid })
}

pub fn window_rect(hwnd: Hwnd) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd.h(), &mut r).ok()? };
    Some(Rect::from_win(r))
}

/// What actually appears on screen. `GetWindowRect` includes an invisible border of about
/// 8 px: a window maximized on screen 1 would otherwise "spill" onto the adjacent screen 2.
pub fn visible_rect(hwnd: Hwnd) -> Option<Rect> {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(hwnd.h(), DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, size_of::<RECT>() as u32)
    };
    if ok.is_ok() && r.right > r.left {
        Some(Rect::from_win(r))
    } else {
        window_rect(hwnd)
    }
}

/// The current state of the window, used to verify whether a switch succeeded.
#[derive(Clone, Copy, Debug)]
pub struct WinState {
    pub rect: Option<Rect>,
    pub iconic: bool,
    pub zoomed: bool,
    pub visible: bool,
    pub cloaked: bool,
    pub style: isize,
    pub exstyle: isize,
}

pub fn state(hwnd: Hwnd) -> WinState {
    let h = hwnd.h();
    unsafe {
        WinState {
            rect: visible_rect(hwnd),
            iconic: IsIconic(h).as_bool(),
            zoomed: IsZoomed(h).as_bool(),
            visible: IsWindowVisible(h).as_bool(),
            cloaked: is_cloaked(h),
            style: GetWindowLongPtrW(h, GWL_STYLE),
            exstyle: GetWindowLongPtrW(h, GWL_EXSTYLE),
        }
    }
}

impl WinState {
    /// The audience sees the window: visible, not minimized and not cloaked by DWM.
    pub fn shown(&self) -> bool {
        self.visible && !self.iconic && !self.cloaked
    }
}

/// `r` covers `mon`, allowing `tol` pixels of slack on each edge.
pub fn covers_within(r: &Rect, mon: &Rect, tol: i32) -> bool {
    r.left <= mon.left + tol && r.top <= mon.top + tol && r.right >= mon.right - tol && r.bottom >= mon.bottom - tol
}

/// user32 functions that the `windows` crate only exposes with the HiDpi feature.
fn user32(name: windows::core::PCSTR) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe { GetProcAddress(GetModuleHandleW(w!("user32.dll")).ok()?, name) }
}

/// The same DPI mode as the panel (eframe enables "per monitor v2"): everything in physical pixels.
pub fn dpi_aware() {
    if let Some(f) = user32(windows::core::s!("SetProcessDpiAwarenessContext")) {
        let f: unsafe extern "system" fn(isize) -> BOOL = unsafe { std::mem::transmute(f) };
        unsafe {
            let _ = f(-4);
        }
    }
}

/// Describes the window without touching it (self-test `--inspect`).
pub fn describe(hwnd: Hwnd) -> String {
    let h = hwnd.h();
    unsafe {
        let s = state(hwnd);
        let st = s.style as u32;
        let flag = |bit: WINDOW_STYLE| if st & bit.0 != 0 { 1 } else { 0 };
        let mut wi = WINDOWINFO { cbSize: size_of::<WINDOWINFO>() as u32, ..Default::default() };
        let _ = GetWindowInfo(h, &mut wi);
        let mut pl = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(h, &mut pl);
        let pid = window_pid(h);
        let dpi_ctx = user32(windows::core::s!("GetWindowDpiAwarenessContext")).map(|f| {
            let f: unsafe extern "system" fn(HWND) -> isize = std::mem::transmute(f);
            f(h)
        });
        let awareness = match (dpi_ctx, user32(windows::core::s!("GetAwarenessFromDpiAwarenessContext"))) {
            (Some(c), Some(f)) => {
                let f: unsafe extern "system" fn(isize) -> i32 = std::mem::transmute(f);
                match f(c) {
                    0 => "unaware",
                    1 => "system",
                    2 => "per-monitor",
                    _ => "?",
                }
            }
            _ => "?",
        };
        let dpi = user32(windows::core::s!("GetDpiForWindow")).map_or(0, |f| {
            let f: unsafe extern "system" fn(HWND) -> u32 = std::mem::transmute(f);
            f(h)
        });
        // A non-administrator can obtain the limited query, but not the full one, for an elevated process.
        let elevated = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid).map(|p| CloseHandle(p)).is_err()
            && OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map(|p| CloseHandle(p)).is_ok();
        let r = |r: RECT| format!("{},{},{},{}", r.left, r.top, r.right, r.bottom);
        format!(
            "classe={} pid={} exe={} provavel_admin={} dpi={} dpi_modo={} visivel={} minimizada={} maximizada={} cloaked={} \
             caption={} thickframe={} maxbox={} popup={} topmost={} showCmd={} normal={} janela={} cliente={} visivel_dwm={:?} bordas={}x{}",
            class_name(h),
            pid,
            process_path(pid).rsplit('\\').next().unwrap_or_default(),
            elevated,
            dpi,
            awareness,
            s.visible,
            s.iconic,
            s.zoomed,
            s.cloaked,
            flag(WS_CAPTION),
            flag(WS_THICKFRAME),
            flag(WS_MAXIMIZEBOX),
            flag(WS_POPUP),
            s.exstyle as u32 & WS_EX_TOPMOST.0 != 0,
            pl.showCmd,
            r(pl.rcNormalPosition),
            r(wi.rcWindow),
            r(wi.rcClient),
            s.rect.map(|r| (r.left, r.top, r.right, r.bottom)),
            wi.cxWindowBorders,
            wi.cyWindowBorders,
        )
    }
}

/// Opens the native file picker for a standby image.
pub fn pick_image_file(owner: Option<Hwnd>) -> Option<String> {
    pick_file(owner, w!("Imagens (*.png;*.jpg;*.jpeg)\0*.png;*.jpg;*.jpeg\0\0"))
}

pub fn pick_media_file(owner: Option<Hwnd>) -> Option<String> {
    pick_file(
        owner,
        w!("Vídeo e áudio (*.mp4;*.mkv;*.avi;*.mov;*.wmv;*.mp3;*.wav;*.flac)\0*.mp4;*.mkv;*.avi;*.mov;*.wmv;*.mp3;*.wav;*.flac\0\0"),
    )
}

pub fn pick_any_file(owner: Option<Hwnd>) -> Option<String> {
    pick_file(owner, w!("Todos os arquivos (*.*)\0*.*\0\0"))
}

/// Opens a file or link with the default Windows program.
pub fn open_path(path: &str) -> bool {
    let path = windows::core::HSTRING::from(path);
    let r = unsafe { ShellExecuteW(None, w!("open"), &path, None, None, SW_SHOWNORMAL) };
    r.0 as isize > 32
}

fn pick_file(owner: Option<Hwnd>, filter: windows::core::PCWSTR) -> Option<String> {
    let mut file = [0u16; 32_768];
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner.map(Hwnd::h).unwrap_or_default(),
        lpstrFilter: filter,
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    unsafe {
        if !GetOpenFileNameW(&mut dialog).as_bool() {
            return None;
        }
    }
    let end = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(String::from_utf16_lossy(&file[..end]))
}

/// The operator is currently dragging or resizing the window.
fn in_move_size(hwnd: HWND) -> bool {
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd, None);
        let mut gi = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(tid, &mut gi).is_ok() && (gi.flags.0 & GUI_INMOVESIZE.0) != 0
    }
}

pub fn move_window(hwnd: Hwnd, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd.h(), None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
    }
}

// ---------------------------------------------------------------------------
// Monitors

#[derive(Clone, Debug)]
pub struct Monitor {
    /// GDI name, for example `\\.\DISPLAY2`. It may change when the monitor order changes.
    pub device: String,
    /// Device name, for example "EPSON PJ". Stable across restarts.
    pub name: String,
    pub rect: Rect,
    /// Work area, excluding the taskbar.
    pub work: Rect,
    pub primary: bool,
}

impl Monitor {
    pub fn label(&self) -> String {
        let n = if self.name.is_empty() { "Monitor" } else { &self.name };
        let num = self.device.trim_start_matches(r"\\.\DISPLAY");
        let principal = if self.primary { ", principal" } else { "" };
        format!("{n} (tela {num}{principal}, {}x{})", self.rect.width(), self.rect.height())
    }

    /// The window (visible rectangle) is on this monitor: its center falls inside it.
    pub fn holds(&self, r: &Rect) -> bool {
        r.center_in(&self.rect)
    }
}

pub fn list_monitors() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_monitors_cb), LPARAM(&mut out as *mut _ as isize));
    }
    let names = friendly_names();
    for m in &mut out {
        if let Some(n) = names.get(&m.device) {
            m.name = n.clone();
        }
    }
    out.sort_by_key(|m| (m.rect.left, m.rect.top));
    out
}

unsafe extern "system" fn enum_monitors_cb(hmon: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
        out.push(Monitor {
            device: utf16_z(&info.szDevice),
            name: String::new(),
            rect: Rect::from_win(info.monitorInfo.rcMonitor),
            work: Rect::from_win(info.monitorInfo.rcWork),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    TRUE
}

/// Maps the GDI name of each monitor to the device name (EDID).
fn friendly_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return map;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return map;
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) == 0
                && DisplayConfigGetDeviceInfo(&mut tgt.header) == 0
            {
                map.insert(utf16_z(&src.viewGdiDeviceName), utf16_z(&tgt.monitorFriendlyDeviceName));
            }
        }
    }
    map
}

fn utf16_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---------------------------------------------------------------------------
// Curtain: a borderless black window (or one showing the standby image) that covers the
// projector beneath whatever is live.

/// Standby image already converted to the GDI format (BGRA, top-down).
struct Picture {
    w: i32,
    h: i32,
    bgra: Vec<u8>,
}

static STANDBY: Mutex<Option<Picture>> = Mutex::new(None);

/// Replaces the standby image (`None` reverts to the black screen). `rgba` is top-down.
pub fn set_standby(curtain: Option<Hwnd>, image: Option<([usize; 2], Vec<u8>)>) {
    let pic = image.map(|([w, h], mut px)| {
        for p in px.chunks_mut(4) {
            p.swap(0, 2);
        }
        Picture { w: w as i32, h: h as i32, bgra: px }
    });
    *STANDBY.lock().unwrap() = pic;
    if let Some(c) = curtain {
        unsafe {
            let _ = InvalidateRect(Some(c.h()), None, true);
        }
    }
}

/// Where the image sits within the screen, preserving the aspect ratio (black margins at the edges).
pub fn letterbox(img: (i32, i32), screen: (i32, i32)) -> Rect {
    let scale = (screen.0 as f32 / img.0.max(1) as f32).min(screen.1 as f32 / img.1.max(1) as f32);
    let (w, h) = ((img.0 as f32 * scale).round() as i32, (img.1 as f32 * scale).round() as i32);
    let (left, top) = ((screen.0 - w) / 2, (screen.1 - h) / 2);
    Rect { left, top, right: left + w, bottom: top + h }
}

unsafe fn paint_curtain(h: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(h, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(h, &mut rc);
    FillRect(dc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
    if let Some(p) = STANDBY.lock().unwrap().as_ref() {
        let r = letterbox((p.w, p.h), (rc.right - rc.left, rc.bottom - rc.top));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: p.w,
                biHeight: -p.h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        SetStretchBltMode(dc, HALFTONE);
        let _ = SetBrushOrgEx(dc, 0, 0, None);
        StretchDIBits(
            dc,
            r.left,
            r.top,
            r.width(),
            r.height(),
            0,
            0,
            p.w,
            p.h,
            Some(p.bgra.as_ptr() as *const _),
            &bi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
    let _ = EndPaint(h, &ps);
}

pub fn create_curtain(r: Rect) -> Option<Hwnd> {
    unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                paint_curtain(h);
                LRESULT(0)
            }
            // WM_PAINT already paints everything: erasing first would only make the image flicker.
            WM_ERASEBKGND => LRESULT(1),
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: w!("TakeScreenCortina"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let h = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("TakeScreenCortina"),
            w!("TakeScreen - cortina"),
            WS_POPUP,
            r.left,
            r.top,
            r.width(),
            r.height(),
            None,
            None,
            Some(hinst),
            None,
        )
        .ok()?;
        // Starts beneath everything, so it does not hide what is already on the projector when the program opens.
        let _ = SetWindowPos(h, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        Some(Hwnd::from(h))
    }
}

pub fn place_curtain(curtain: Hwnd, r: Rect, on_top: bool) {
    let after = if on_top { HWND_TOP } else { HWND_BOTTOM };
    unsafe {
        let _ = SetWindowPos(curtain.h(), Some(after), r.left, r.top, r.width(), r.height(), SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

pub fn destroy(hwnd: Hwnd) {
    unsafe {
        let _ = DestroyWindow(hwnd.h());
    }
}

// ---------------------------------------------------------------------------
// Show on the projector and return to screen 1

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fit {
    /// Removes the border and title bar and fills the entire monitor. Best for ordinary programs.
    Borderless,
    /// Only maximizes on the monitor. Required for Store apps, which draw their own frame.
    Maximize,
}

/// The window state before it went to the projector, so it can be restored exactly.
#[derive(Clone, Copy)]
pub struct Saved {
    pub hwnd: Hwnd,
    placement: WINDOWPLACEMENT,
    style: isize,
    exstyle: isize,
}

impl Saved {
    /// The style `restore` gives back; differs from the original only for a VLC stuck without a frame.
    pub fn style(&self) -> isize {
        self.style
    }
}

const CLOSED: &str = "Não consegui mover essa janela. Ela ainda está aberta?";
const ELEVATED: &str =
    "Esta janela é de um programa em modo administrador: execute o TakeScreen como administrador para exibi-la.";
const LOST: &str = "Essa janela não apareceu no datashow e voltou para a tela 1.";
/// State bits that must not be restored with the saved frame: the window would believe
/// it is maximized or minimized when it is not, and Windows would ignore the next request.
const STATE_BITS: isize = (WS_MAXIMIZE.0 | WS_MINIMIZE.0) as isize;

/// Moves `target` to monitor `mon`. The window is assembled beneath `cover`
/// (what is live now) and only comes to the front at the end, all at once,
/// so the audience never sees it being dragged or resized.
/// Verifies the result: if the requested fit does not cover the projector, retries and then maximizes.
pub fn take(target: Hwnd, cover: Hwnd, curtain: Hwnd, mon: Rect, fit: Fit) -> Result<Saved, String> {
    let h = target.h();
    let pl = placement(target).ok_or(CLOSED)?;
    let mut saved = unsafe { Saved { hwnd: target, placement: pl, style: GetWindowLongPtrW(h, GWL_STYLE), exstyle: GetWindowLongPtrW(h, GWL_EXSTYLE) } };
    // A frameless VLC is stuck in its own full screen: it goes back to screen 1 with the Windows buttons.
    if needs_frame(h, saved.style) {
        saved.style = framed(saved.style);
    }
    place(&saved, pl, cover, curtain, mon, fit)?;
    Ok(saved)
}

/// Switches the live window between the two fits in place. `saved` stays as it was before the
/// first take, so `restore` still returns the window to its original state.
pub fn refit(saved: &Saved, curtain: Hwnd, mon: Rect, fit: Fit) -> Result<(), String> {
    // The current placement: when maximized on the projector, its normal rectangle is there too.
    let pl = placement(saved.hwnd).ok_or(CLOSED)?;
    place(saved, pl, curtain, curtain, mon, fit)
}

fn placement(hwnd: Hwnd) -> Option<WINDOWPLACEMENT> {
    let mut pl = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { (IsWindow(Some(hwnd.h())).as_bool() && GetWindowPlacement(hwnd.h(), &mut pl).is_ok()).then_some(pl) }
}

/// Fits `saved.hwnd` to `mon` beneath `cover`, then brings it to the front. `pl` is used to leave
/// the minimized/maximized state. On failure the window goes back to its original state.
fn place(saved: &Saved, pl: WINDOWPLACEMENT, cover: Hwnd, curtain: Hwnd, mon: Rect, fit: Fit) -> Result<(), String> {
    let target = saved.hwnd;
    let h = target.h();
    unsafe {
        // 0. Program running as administrator: Windows silently rejects any adjustment coming
        //    from an ordinary program. A SetWindowPos that changes nothing reveals this before anything moves.
        if let Err(e) = SetWindowPos(h, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) {
            return Err(if e.code() == E_ACCESSDENIED { ELEVATED } else { CLOSED }.into());
        }

        // 1. Leaves the minimized/maximized state without stealing the focus of the operator.
        if IsIconic(h).as_bool() || IsZoomed(h).as_bool() {
            let mut normal = pl;
            normal.flags = WINDOWPLACEMENT_FLAGS(0);
            normal.showCmd = SW_SHOWNOACTIVATE.0 as u32;
            let _ = SetWindowPlacement(h, &normal);
        }

        // 2. First moves it to the projector at its current size: if the DPI there differs, the
        //    program readjusts now (WM_DPICHANGED) and does not undo the final size later.
        let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
        let _ = SetWindowPos(h, Some(cover.h()), mon.left + 40, mon.top + 40, 0, 0, flags | SWP_NOSIZE);

        // 3. Adjusts, verifies what actually appears and falls back to the next method if it does not cover.
        let monitors = list_monitors();
        let work = monitors.iter().find(|m| m.rect == mon).map_or(mon, |m| m.work);
        let mut done = false;
        if fit == Fit::Borderless {
            let caption = (WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as isize;
            let edges = (WS_EX_DLGMODALFRAME | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_WINDOWEDGE).0 as isize;
            // Twice: Chromium and Electron sometimes undo the first adjustment.
            for _ in 0..2 {
                SetWindowLongPtrW(h, GWL_STYLE, saved.style & !caption & !STATE_BITS);
                SetWindowLongPtrW(h, GWL_EXSTYLE, saved.exstyle & !edges);
                let _ = SetWindowPos(h, Some(cover.h()), mon.left, mon.top, mon.width(), mon.height(), flags | SWP_FRAMECHANGED);
                if settled(target, &mon) {
                    done = true;
                    break;
                }
            }
            if !done {
                // About to maximize: restore the frame first, so the window is not left without a title bar.
                SetWindowLongPtrW(h, GWL_STYLE, saved.style & !STATE_BITS);
                SetWindowLongPtrW(h, GWL_EXSTYLE, saved.exstyle);
            }
        }
        if !done {
            // Windows has no "maximize without activating": SW_MAXIMIZE always brings the window
            // to the front. During the switch, the cover stays "always on top" above it,
            // so the audience keeps seeing only what was live.
            let was_topmost = GetWindowLongPtrW(cover.h(), GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
            let still = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            let _ = SetWindowPos(cover.h(), Some(HWND_TOPMOST), 0, 0, 0, 0, still);
            // Maximize means the native maximized window, with title bar and buttons.
            let style = GetWindowLongPtrW(h, GWL_STYLE);
            if needs_frame(h, style) {
                SetWindowLongPtrW(h, GWL_STYLE, framed(style) & !STATE_BITS);
            }
            let _ = SetWindowPos(h, Some(cover.h()), mon.left + 40, mon.top + 40, mon.width() - 80, mon.height() - 80, flags | SWP_FRAMECHANGED);
            let _ = ShowWindow(h, SW_MAXIMIZE);
            // A maximized window fills the work area: if the projector has a taskbar, it remains visible.
            done = settled(target, &work);
            if !was_topmost {
                // Leaves "always on top" and sits just above ordinary windows, still above the new one.
                let _ = SetWindowPos(cover.h(), Some(HWND_NOTOPMOST), 0, 0, 0, 0, still);
            }
        }

        let now = state(target);
        let others: Vec<Rect> = monitors.iter().filter(|m| m.rect != mon).map(|m| m.rect).collect();
        match now.rect {
            // Minimum size larger than the projector: the excess goes to the side with no other screen.
            Some(r) if done && (r.width() > mon.width() || r.height() > mon.height()) => {
                let (x, y) = spill_origin((r.width(), r.height()), &mon, &others);
                let wr = window_rect(target).unwrap_or(r);
                let _ = SetWindowPos(h, None, x - (r.left - wr.left), y - (r.top - wr.top), 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            // Does not cover (fixed size), but is visible on the projector: it stays live, with the black curtain around it.
            Some(r) if now.shown() && r.overlap_area(&mon) > 0 => {}
            _ if !done => {
                restore(saved);
                return Err(LOST.into());
            }
            _ => {}
        }

        // 4. The switch: brings it to the front and keeps the curtain right behind, in case the window closes.
        let _ = SetWindowPos(h, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(curtain.h(), Some(h), mon.left, mon.top, mon.width(), mon.height(), SWP_NOACTIVATE | SWP_SHOWWINDOW);
        Ok(())
    }
}

/// Programs whose borderless fit looks broken but whose own full screen is clean: the fit becomes
/// Maximize and the key is pressed once the window is on air.
/// Explorer draws its title strip itself (a white block remains without the frame) and closes the
/// window if F11 arrives while it is borderless; VLC keeps its menu and control bars.
pub fn native_fullscreen(class: &str, exe: &str) -> Option<VIRTUAL_KEY> {
    match (class, exe.to_lowercase().as_str()) {
        ("CabinetWClass", _) => Some(VK_FULLSCREEN),
        // shortcut: VLC is matched by exe name because its Qt window class is shared by every Qt program.
        (_, "vlc.exe") => Some(VK_VIDEO_FULLSCREEN),
        _ => None,
    }
}

/// VLC remembers its full screen state across launches (vlc-qt-interface.ini) and then reopens as a
/// popup with no title bar and no buttons, which neither F nor Esc clears.
fn needs_frame(h: HWND, style: isize) -> bool {
    style as u32 & WS_CAPTION.0 != WS_CAPTION.0 && process_path(window_pid(h)).to_lowercase().ends_with("\\vlc.exe")
}

/// A frameless VLC before a take: either in its own full screen, where the F key would turn it off,
/// or stuck without a frame. Esc (`undo_key` of F) leaves the first and does nothing to the second.
pub fn frameless_vlc(hwnd: Hwnd) -> bool {
    unsafe { needs_frame(hwnd.h(), GetWindowLongPtrW(hwnd.h(), GWL_STYLE)) }
}

/// `style` as an ordinary window with title bar, frame and the minimize, maximize and close buttons.
fn framed(style: isize) -> isize {
    (style & !(WS_POPUP.0 as isize)) | WS_OVERLAPPEDWINDOW.0 as isize
}

/// The fit to apply and the key to press afterwards, for the fit the operator chose.
pub fn effective_fit(chosen: Fit, class: &str, exe: &str) -> (Fit, Option<VIRTUAL_KEY>) {
    match (chosen, native_fullscreen(class, exe)) {
        (Fit::Borderless, Some(key)) => (Fit::Maximize, Some(key)),
        (fit, _) => (fit, None),
    }
}

pub fn window_class(hwnd: Hwnd) -> String {
    class_name(hwnd.h())
}

/// Only one panel may run: two projector guards would keep sending each other's windows back.
/// Returns false if another TakeScreen is already open.
pub fn single_instance() -> bool {
    unsafe {
        match CreateMutexW(None, true, w!(r"Local\TakeScreen-panel")) {
            // The handle is kept open for the lifetime of the process on purpose.
            Ok(_) => GetLastError() != ERROR_ALREADY_EXISTS,
            Err(_) => true,
        }
    }
}

pub fn alert(text: &str) {
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(None, windows::core::PCWSTR(text.as_ptr()), w!("TakeScreen"), MB_OK | MB_ICONINFORMATION);
    }
}

/// Gives the program time to finish adjusting and verifies whether the window covers `want`.
fn settled(target: Hwnd, want: &Rect) -> bool {
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Handles messages sent to the panel (activation, focus): the other program waits for them.
        unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
        }
    }
    let s = state(target);
    s.shown() && s.rect.is_some_and(|r| covers_within(&r, want, 2))
}

/// Where to place a window of size `size` larger than `mon`, covering it while intruding as little as possible on `others`.
pub fn spill_origin(size: (i32, i32), mon: &Rect, others: &[Rect]) -> (i32, i32) {
    let xs = [mon.left, mon.right - size.0, mon.left - (size.0 - mon.width()) / 2];
    let ys = [mon.top, mon.bottom - size.1, mon.top - (size.1 - mon.height()) / 2];
    let spill = |&(x, y): &(i32, i32)| {
        let r = Rect { left: x, top: y, right: x + size.0, bottom: y + size.1 };
        others.iter().map(|o| r.overlap_area(o) as i64).sum::<i64>()
    };
    let mut best = (xs[0], ys[0]);
    for x in xs {
        for y in ys {
            if spill(&(x, y)) < spill(&best) {
                best = (x, y);
            }
        }
    }
    best
}

/// Returns the window to the position, size and frame it had before going to the projector.
pub fn restore(s: &Saved) {
    let h = s.hwnd.h();
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return;
        }
        let mut pl = s.placement;
        pl.flags = WINDOWPLACEMENT_FLAGS(0);
        // Maximized on the projector: leave the maximized state first, otherwise Windows keeps it there.
        if IsZoomed(h).as_bool() {
            let mut normal = pl;
            normal.showCmd = SW_SHOWNOACTIVATE.0 as u32;
            let _ = SetWindowPlacement(h, &normal);
        }
        SetWindowLongPtrW(h, GWL_STYLE, s.style & !STATE_BITS);
        SetWindowLongPtrW(h, GWL_EXSTYLE, s.exstyle);
        pl.showCmd = match SHOW_WINDOW_CMD(pl.showCmd as i32) {
            SW_SHOWMINIMIZED => SW_SHOWMINNOACTIVE.0 as u32,
            SW_SHOWMAXIMIZED => SW_SHOWMAXIMIZED.0 as u32,
            _ => SW_SHOWNOACTIVATE.0 as u32,
        };
        let _ = SetWindowPlacement(h, &pl);
        let _ = SetWindowPos(h, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        // A program's own full screen (Explorer F11) may leave "always on top", which only SetWindowPos clears.
        if s.exstyle as u32 & WS_EX_TOPMOST.0 == 0 && GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 {
            let _ = SetWindowPos(h, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        // Never disappears: if it ended up visible on no screen, it moves to the center of the primary one.
        let st = state(s.hwnd);
        let monitors = list_monitors();
        if !st.iconic && !st.rect.is_some_and(|r| monitors.iter().any(|m| r.center_in(&m.rect))) {
            if let Some(m) = monitors.iter().find(|m| m.primary) {
                send_home(s.hwnd, m.work);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery: if TakeScreen closes abruptly (error, killed process), the window
// that was live would remain frameless on the projector. It is recorded in a file and
// restored on the next start, or immediately if the error occurs inside the program.

static ON_AIR: Mutex<Option<Saved>> = Mutex::new(None);

fn recovery_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::Path::new(&base).join("TakeScreen").join("no-ar.txt"))
}

/// Records (or erases, with `None`) the window that is live.
pub fn remember_on_air(s: Option<&Saved>) {
    if let Ok(mut g) = ON_AIR.lock() {
        *g = s.copied();
    }
    let Some(path) = recovery_file() else { return };
    match s {
        None => {
            let _ = std::fs::remove_file(path);
        }
        Some(s) => {
            let line = recovery_text(s, window_pid(s.hwnd.h()));
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, line);
        }
    }
}

fn recovery_text(s: &Saved, pid: u32) -> String {
    let p = &s.placement;
    let n = &p.rcNormalPosition;
    format!(
        "{} {} {} {} {} {} {} {} {} {} {}",
        s.hwnd.0,
        pid,
        s.style,
        s.exstyle,
        p.showCmd,
        p.ptMinPosition.x,
        p.ptMinPosition.y,
        n.left,
        n.top,
        n.right,
        n.bottom
    )
}

fn parse_recovery(text: &str) -> Option<(Saved, u32)> {
    let v: Vec<i64> = text.split_whitespace().map(|t| t.parse().ok()).collect::<Option<_>>()?;
    let [hwnd, pid, style, exstyle, show, mx, my, l, t, r, b] = v[..] else { return None };
    let placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: show as u32,
        ptMinPosition: POINT { x: mx as i32, y: my as i32 },
        ptMaxPosition: POINT { x: -1, y: -1 },
        rcNormalPosition: RECT { left: l as i32, top: t as i32, right: r as i32, bottom: b as i32 },
        ..Default::default()
    };
    Some((Saved { hwnd: Hwnd(hwnd as isize), placement, style: style as isize, exstyle: exstyle as isize }, pid as u32))
}

/// On startup: if the last session left a window live, restores it to its previous state.
/// Returns the title of the restored window.
pub fn recover_previous() -> Option<String> {
    let path = recovery_file()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let (saved, pid) = parse_recovery(&text)?;
    // The window handle may have been reused by another program.
    if !saved.hwnd.exists() || window_pid(saved.hwnd.h()) != pid {
        return None;
    }
    restore(&saved);
    Some(window_text(saved.hwnd.h()))
}

/// For errors inside the program: restores the live window immediately.
pub fn recover_now() {
    if let Ok(mut g) = ON_AIR.try_lock() {
        if let Some(s) = g.take() {
            restore(&s);
        }
    }
    if let Some(path) = recovery_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// Brings the panel to the front and returns keyboard focus to it. After moving the projector
/// windows, the panel may stay in front without keyboard focus, and Esc would be lost.
pub fn focus(hwnd: Hwnd) {
    unsafe {
        let _ = SetForegroundWindow(hwnd.h());
        let _ = SetFocus(Some(hwnd.h()));
    }
}

/// Presses a key on the live window and returns focus to the panel (F11 puts the browser
/// in full screen; F does the same for YouTube and most players). Fails if the window does not activate.
pub fn press_key_on(target: Hwnd, vk: VIRTUAL_KEY, back_to: Hwnd) -> Result<(), String> {
    if !activate(target) {
        activate(back_to);
        return Err("Não consegui ativar a janela no ar para apertar a tecla. Tente de novo.".into());
    }
    unsafe {
        // Sending the scancode as well makes the page receive the key as if it came from the keyboard (event.code).
        let scan = MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) as u16;
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, ..Default::default() } },
        };
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    activate(back_to);
    focus(back_to);
    Ok(())
}

/// Brings the window to the front with keyboard focus. Windows only lets the foreground process
/// change focus; attaching our input queue to that of the foreground window works around this.
fn activate(hwnd: Hwnd) -> bool {
    let h = hwnd.h();
    unsafe {
        let me = GetCurrentThreadId();
        let front = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = front != 0 && front != me && AttachThreadInput(me, front, true).as_bool();
        let _ = BringWindowToTop(h);
        let _ = SetForegroundWindow(h);
        if attached {
            let _ = AttachThreadInput(me, front, false);
        }
        for _ in 0..10 {
            if GetForegroundWindow() == h {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        false
    }
}

/// The window that currently holds the keyboard.
pub fn foreground() -> Hwnd {
    Hwnd::from(unsafe { GetForegroundWindow() })
}

pub fn title(hwnd: Hwnd) -> String {
    window_text(hwnd.h())
}

pub use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;

pub const VK_FULLSCREEN: VIRTUAL_KEY = VK_F11;
/// Video full screen on YouTube, Vimeo, Twitch, Netflix and most HTML5 players.
pub const VK_VIDEO_FULLSCREEN: VIRTUAL_KEY = VK_F;

/// The key that undoes `vk`: F11 toggles itself, while Esc always leaves HTML5 full screen
/// (pressing F again would re-enter it if the operator had already left it on the page).
pub fn undo_key(vk: VIRTUAL_KEY) -> VIRTUAL_KEY {
    if vk == VK_VIDEO_FULLSCREEN { VK_ESCAPE } else { vk }
}

/// Records one press of `vk`: a second press undoes the first.
pub fn toggle_key(keys: &mut Vec<VIRTUAL_KEY>, vk: VIRTUAL_KEY) {
    match keys.iter().position(|k| *k == vk) {
        Some(i) => {
            keys.remove(i);
        }
        None => keys.push(vk),
    }
}

/// Presses the undo key of each entry in `keys` on `target`, latest first, and returns focus to `back_to`.
pub fn undo_keys(target: Hwnd, keys: Vec<VIRTUAL_KEY>, back_to: Hwnd) {
    if keys.is_empty() || !target.exists() {
        return;
    }
    for vk in keys.into_iter().rev() {
        let _ = press_key_on(target, undo_key(vk), back_to);
    }
    // shortcut: fixed wait for the program to leave full screen; poll its rect if 300 ms proves short.
    std::thread::sleep(std::time::Duration::from_millis(300));
}

// ---------------------------------------------------------------------------
// Live previews: Windows itself (DWM) draws the source window inside
// our panel, at no CPU cost to the program.

pub struct Thumbnail {
    pub src: Hwnd,
    id: isize,
}

impl Thumbnail {
    pub fn new(dest: Hwnd, src: Hwnd) -> Option<Self> {
        let id = unsafe { DwmRegisterThumbnail(dest.h(), src.h()).ok()? };
        Some(Thumbnail { src, id })
    }

    /// `area` in physical pixels, relative to the panel client area. The image is
    /// centered preserving the aspect ratio of the source window, and clipped to `clip`
    /// (Windows draws over the panel and does not know where a scrolling area ends).
    pub fn show(&self, area: Rect, clip: Rect) {
        unsafe {
            let Ok(size) = DwmQueryThumbnailSourceSize(self.id) else { return };
            let (sw, sh) = (size.cx.max(1) as f32, size.cy.max(1) as f32);
            let scale = (area.width() as f32 / sw).min(area.height() as f32 / sh);
            let (w, h) = ((sw * scale) as i32, (sh * scale) as i32);
            let left = area.left + (area.width() - w) / 2;
            let top = area.top + (area.height() - h) / 2;
            let dest = Rect { left, top, right: left + w, bottom: top + h };
            let vis = Rect {
                left: dest.left.max(clip.left),
                top: dest.top.max(clip.top),
                right: dest.right.min(clip.right),
                bottom: dest.bottom.min(clip.bottom),
            };
            if vis.width() <= 0 || vis.height() <= 0 || scale <= 0.0 {
                self.hide();
                return;
            }
            let src = |d: i32, origin: i32| ((d - origin) as f32 / scale).round() as i32;
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_RECTSOURCE
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY
                    | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT { left: vis.left, top: vis.top, right: vis.right, bottom: vis.bottom },
                rcSource: RECT {
                    left: src(vis.left, dest.left),
                    top: src(vis.top, dest.top),
                    right: src(vis.right, dest.left),
                    bottom: src(vis.bottom, dest.top),
                },
                opacity: 255,
                fVisible: TRUE,
                fSourceClientAreaOnly: FALSE,
                ..Default::default()
            };
            let _ = DwmUpdateThumbnailProperties(self.id, &props);
        }
    }

    pub fn hide(&self) {
        let props = DWM_THUMBNAIL_PROPERTIES { dwFlags: DWM_TNP_VISIBLE, fVisible: FALSE, ..Default::default() };
        unsafe {
            let _ = DwmUpdateThumbnailProperties(self.id, &props);
        }
    }
}

impl Drop for Thumbnail {
    fn drop(&mut self) {
        unsafe {
            let _ = DwmUnregisterThumbnail(self.id);
        }
    }
}

// ---------------------------------------------------------------------------
// Program icons

pub struct Icon {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

/// The icon the window shows on the taskbar; if it provides none, the icon of the .exe.
pub fn window_icon(w: &WindowInfo) -> Option<Icon> {
    let h = w.hwnd.h();
    unsafe {
        if !w.uwp {
            for kind in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
                let mut res = 0usize;
                let ok = SendMessageTimeoutW(h, WM_GETICON, WPARAM(kind as usize), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, 100, Some(&mut res));
                if ok.0 != 0 && res != 0 {
                    if let Some(i) = icon_to_rgba(HICON(res as *mut _)) {
                        return Some(i);
                    }
                }
            }
            let class_icon = GetClassLongPtrW(h, GCLP_HICON);
            if class_icon != 0 {
                if let Some(i) = icon_to_rgba(HICON(class_icon as *mut _)) {
                    return Some(i);
                }
            }
        }
        exe_icon(&w.exe_path)
    }
}

fn exe_icon(path: &str) -> Option<Icon> {
    if path.is_empty() {
        return None;
    }
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut large = HICON::default();
    unsafe {
        let n = windows::Win32::UI::Shell::ExtractIconExW(windows::core::PCWSTR(wide.as_ptr()), 0, Some(&mut large), None, 1);
        if n == 0 || large.is_invalid() {
            return None;
        }
        let icon = icon_to_rgba(large);
        let _ = DestroyIcon(large);
        icon
    }
}

unsafe fn icon_to_rgba(icon: HICON) -> Option<Icon> {
    let mut ii = ICONINFO::default();
    GetIconInfo(icon, &mut ii).ok()?;
    let cleanup = |ii: &ICONINFO| {
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor.into());
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask.into());
        }
    };
    if ii.hbmColor.is_invalid() {
        cleanup(&ii);
        return None;
    }
    let mut bm = BITMAP::default();
    GetObjectW(ii.hbmColor.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    if w <= 0 || h <= 0 {
        cleanup(&ii);
        return None;
    }
    let read = |bmp: HBITMAP| -> Vec<u8> {
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        buf
    };
    let mut px = read(ii.hbmColor);
    // Legacy icons have no transparency in the alpha channel, only in the mask.
    if px.chunks(4).all(|p| p[3] == 0) {
        let mask = read(ii.hbmMask);
        for (p, m) in px.chunks_mut(4).zip(mask.chunks(4)) {
            p[3] = if m[0] == 0 { 255 } else { 0 };
        }
    }
    for p in px.chunks_mut(4) {
        p.swap(0, 2);
    }
    cleanup(&ii);
    Some(Icon { size: [w as usize, h as usize], rgba: px })
}

// ---------------------------------------------------------------------------
// Projector protection: any window that appears on screen 2 without going through
// TakeScreen (a program opened by accident, a window that remembered its last position)
// is sent back to screen 1 immediately.

use std::sync::{Mutex, OnceLock};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};

struct Guard {
    enabled: bool,
    datashow: Rect,
    home: Rect,
    /// Windows allowed to stay on the projector: the live one and the incoming one.
    allowed: Vec<isize>,
    /// Titles sent back since the panel last checked.
    returned: Vec<String>,
    /// Presentations that reached the projector since the panel last read them.
    arrived: Vec<isize>,
    curtain: Option<isize>,
}

static GUARD: Mutex<Guard> = Mutex::new(Guard {
    enabled: false,
    datashow: Rect { left: 0, top: 0, right: 0, bottom: 0 },
    home: Rect { left: 0, top: 0, right: 0, bottom: 0 },
    allowed: Vec::new(),
    returned: Vec::new(),
    arrived: Vec::new(),
    curtain: None,
});
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Enables Windows notifications about windows appearing and moving. `wake` wakes the panel.
pub fn guard_install(wake: impl Fn() + Send + Sync + 'static) {
    let _ = WAKE.set(Box::new(wake));
    let events = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
    ];
    for (min, max) in events {
        unsafe {
            SetWinEventHook(min, max, None, Some(guard_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        }
    }
}

pub fn guard_configure(enabled: bool, datashow: Rect, home: Rect, allowed: &[Hwnd], curtain: Option<Hwnd>) {
    let mut g = GUARD.lock().unwrap();
    g.enabled = enabled;
    g.datashow = datashow;
    g.home = home;
    g.allowed = allowed.iter().map(|h| h.0).collect();
    g.curtain = curtain.map(|h| h.0);
}

pub fn guard_take_returned() -> Vec<String> {
    std::mem::take(&mut GUARD.lock().unwrap().returned)
}

pub fn guard_take_arrived() -> Vec<Hwnd> {
    std::mem::take(&mut GUARD.lock().unwrap().arrived)
        .into_iter()
        .map(Hwnd)
        .collect()
}

unsafe extern "system" fn guard_event(_: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    if id_object != OBJID_WINDOW.0 || hwnd.is_invalid() {
        return;
    }
    let (datashow, home, curtain) = {
        let g = GUARD.lock().unwrap();
        if !g.enabled || g.allowed.contains(&(hwnd.0 as isize)) {
            return;
        }
        (g.datashow, g.home, g.curtain.map(Hwnd))
    };
    if in_move_size(hwnd) {
        return;
    }
    // Only "real" windows (the same ones shown on the cards); menus and dialogs stay with their owner.
    let Some(info) = window_info(hwnd) else { return };
    if IsIconic(hwnd).as_bool() {
        return;
    }
    let Some(r) = visible_rect(info.hwnd) else { return };
    if !r.center_in(&datashow) {
        return;
    }
    let presentation = class_name(hwnd) == "screenClass" || r.covers(&datashow);
    if presentation {
        let accepted = {
            let mut g = GUARD.lock().unwrap();
            if !g.enabled || g.allowed.contains(&(hwnd.0 as isize)) || g.arrived.contains(&(hwnd.0 as isize)) {
                false
            } else {
                g.arrived.push(hwnd.0 as isize);
                true
            }
        };
        if !accepted {
            return;
        }
        if let Some(curtain) = curtain {
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            let _ = SetWindowPos(hwnd, Some(curtain.h()), 0, 0, 0, 0, flags);
        }
    } else {
        send_home(info.hwnd, home);
        GUARD.lock().unwrap().returned.push(info.title);
    }
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// Places the window in the center of screen 1, without activating it, fitting the work area.
pub fn send_home(hwnd: Hwnd, home: Rect) {
    let h = hwnd.h();
    unsafe {
        if IsZoomed(h).as_bool() {
            let mut pl = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if GetWindowPlacement(h, &mut pl).is_ok() {
                pl.flags = WINDOWPLACEMENT_FLAGS(0);
                pl.showCmd = SW_SHOWNOACTIVATE.0 as u32;
                let _ = SetWindowPlacement(h, &pl);
            }
        }
        let (Some(r), Some(wr)) = (visible_rect(hwnd), window_rect(hwnd)) else { return };
        let w = r.width().min(home.width() * 9 / 10).max(200);
        let ht = r.height().min(home.height() * 9 / 10).max(150);
        // SetWindowPos counts the invisible border (about 8 px): without it, the window would shrink on every return.
        let (bl, bt) = (r.left - wr.left, r.top - wr.top);
        let (bw, bh) = (wr.width() - r.width(), wr.height() - r.height());
        let x = home.left + (home.width() - w) / 2 - bl;
        let y = home.top + (home.height() - ht) / 2 - bt;
        let _ = SetWindowPos(h, None, x, y, w + bw, ht + bh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlc_preso_ganha_moldura_e_botoes() {
        let stuck = (WS_POPUP | WS_VISIBLE | WS_SYSMENU).0 as isize;
        let f = framed(stuck) as u32;
        assert_eq!(f & WS_POPUP.0, 0);
        assert_eq!(f & WS_OVERLAPPEDWINDOW.0, WS_OVERLAPPEDWINDOW.0);
        assert_ne!(f & WS_VISIBLE.0, 0);
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    #[test]
    fn explorer_e_vlc_usam_a_tela_cheia_do_proprio_programa() {
        assert_eq!(effective_fit(Fit::Borderless, "CabinetWClass", "explorer.exe"), (Fit::Maximize, Some(VK_FULLSCREEN)));
        assert_eq!(effective_fit(Fit::Borderless, "Qt5QWindowIcon", "VLC.EXE"), (Fit::Maximize, Some(VK_VIDEO_FULLSCREEN)));
        // Maximize keeps the native maximize; the video full screen button stays available.
        assert_eq!(effective_fit(Fit::Maximize, "Qt5QWindowIcon", "vlc.exe"), (Fit::Maximize, None));
        assert_eq!(effective_fit(Fit::Borderless, "Chrome_WidgetWin_1", "msedge.exe"), (Fit::Borderless, None));
        assert_eq!(effective_fit(Fit::Borderless, "Qt5QWindowIcon", "obs64.exe"), (Fit::Borderless, None));
    }

    #[test]
    fn maximized_window_on_primary_is_not_on_datashow() {
        let maximized = rect(-8, -8, 1928, 1048);
        let datashow = rect(-1600, 0, 0, 900);
        assert!(!maximized.center_in(&datashow));
    }

    #[test]
    fn center_in_uses_the_visible_window_center() {
        let datashow = rect(-1600, 0, 0, 900);
        assert!(!rect(100, 100, 500, 500).center_in(&datashow));
        assert!(rect(-1500, 100, -200, 800).center_in(&datashow));
    }

    #[test]
    fn covers_accepts_a_fullscreen_window() {
        let datashow = rect(-1600, 0, 0, 900);
        assert!(rect(-1600, 0, 0, 900).covers(&datashow));
        assert!(rect(-1700, -20, 20, 920).covers(&datashow));
        assert!(!rect(-1500, 0, 0, 900).covers(&datashow));
    }

    #[test]
    fn covers_within_accepts_small_gaps_only() {
        let datashow = rect(-1600, 0, 0, 900);
        assert!(covers_within(&rect(-1598, 2, -2, 898), &datashow, 2));
        assert!(!covers_within(&rect(-1597, 0, 0, 900), &datashow, 2));
        assert!(!covers_within(&rect(-1600, 0, 0, 860), &datashow, 2));
        assert!(covers_within(&rect(-1700, -50, 100, 950), &datashow, 2));
    }

    #[test]
    fn overlap_area_counts_shared_pixels() {
        assert_eq!(rect(0, 0, 10, 10).overlap_area(&rect(5, 5, 20, 20)), 25);
        assert_eq!(rect(-1600, 0, 0, 900).overlap_area(&rect(0, 0, 1920, 1080)), 0);
    }

    #[test]
    fn spill_origin_pushes_overflow_away_from_other_screens() {
        // Projector to the left of the primary screen: the excess goes left, where there is no screen.
        let datashow = rect(-1600, 0, 0, 900);
        let primary = rect(0, 0, 1920, 1080);
        assert_eq!(spill_origin((1700, 1000), &datashow, &[primary]), (-1700, 0));
        // Projector to the right: the excess goes right.
        let right = rect(1920, 0, 3520, 900);
        assert_eq!(spill_origin((1700, 1000), &right, &[primary]), (1920, 0));
        // No neighbors: cover starting from the corner.
        assert_eq!(spill_origin((1700, 1000), &datashow, &[]), (-1600, 0));
    }

    #[test]
    fn letterbox_preserves_aspect_ratio() {
        assert_eq!(letterbox((1920, 1080), (1600, 900)), rect(0, 0, 1600, 900));
        assert_eq!(letterbox((1000, 1000), (1600, 900)), rect(350, 0, 1250, 900));
    }

    #[test]
    fn parse_recovery_round_trip() {
        let saved = Saved {
            hwnd: Hwnd(-123),
            placement: WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                showCmd: SW_SHOWMAXIMIZED.0 as u32,
                ptMinPosition: POINT { x: -4, y: 7 },
                ptMaxPosition: POINT { x: -1, y: -1 },
                rcNormalPosition: RECT { left: -800, top: 10, right: 900, bottom: 700 },
                ..Default::default()
            },
            style: -55,
            exstyle: 66,
        };
        let text = recovery_text(&saved, 4242);
        let (parsed, pid) = parse_recovery(&text).expect("formato de recuperacao valido");
        assert_eq!(pid, 4242);
        assert_eq!(parsed.hwnd, saved.hwnd);
        assert_eq!(parsed.style, saved.style);
        assert_eq!(parsed.exstyle, saved.exstyle);
        assert_eq!(parsed.placement.showCmd, saved.placement.showCmd);
        assert_eq!(parsed.placement.ptMinPosition.x, saved.placement.ptMinPosition.x);
        assert_eq!(parsed.placement.ptMinPosition.y, saved.placement.ptMinPosition.y);
        assert_eq!(parsed.placement.rcNormalPosition.left, saved.placement.rcNormalPosition.left);
        assert_eq!(parsed.placement.rcNormalPosition.top, saved.placement.rcNormalPosition.top);
        assert_eq!(parsed.placement.rcNormalPosition.right, saved.placement.rcNormalPosition.right);
        assert_eq!(parsed.placement.rcNormalPosition.bottom, saved.placement.rcNormalPosition.bottom);
    }
}
