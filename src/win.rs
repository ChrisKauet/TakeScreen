//! Tudo o que conversa com o Windows: listar janelas e monitores, mover janelas
//! para o datashow e desenhar as prévias ao vivo (miniaturas do DWM).

use std::collections::HashMap;
use std::mem::size_of;

use windows::core::{w, BOOL, PWSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Identificador de janela que pode ser copiado e comparado livremente.
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

    fn intersects(&self, o: &Rect) -> bool {
        self.left < o.right && o.left < self.right && self.top < o.bottom && o.top < self.bottom
    }
}

// ---------------------------------------------------------------------------
// Janelas abertas

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: Hwnd,
    pub title: String,
    pub exe: String,
    pub exe_path: String,
    pub minimized: bool,
    /// Apps da Microsoft Store (Fotos, Filmes e TV) vivem dentro de um "ApplicationFrameWindow".
    pub uwp: bool,
}

/// Lista as janelas que fazem sentido mostrar ao operador, na ordem em que estão empilhadas.
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

/// O processo real de um app da Store é o da janela filha (CoreWindow), não o ApplicationFrameHost.
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

pub fn move_window(hwnd: Hwnd, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd.h(), None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
    }
}

// ---------------------------------------------------------------------------
// Monitores

#[derive(Clone, Debug)]
pub struct Monitor {
    /// Nome GDI, por exemplo `\\.\DISPLAY2`. Pode mudar quando a ordem dos monitores muda.
    pub device: String,
    /// Nome do aparelho, por exemplo "EPSON PJ". Estável entre reinícios.
    pub name: String,
    pub rect: Rect,
    /// Área útil, sem a barra de tarefas.
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

    pub fn contains(&self, r: &Rect) -> bool {
        self.rect.intersects(r)
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

/// Mapeia o nome GDI de cada monitor para o nome do aparelho (EDID).
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
// Cortina: uma janela preta, sem borda, que cobre o datashow por baixo do que está no ar.

pub fn create_curtain(r: Rect) -> Option<Hwnd> {
    unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        DefWindowProcW(h, msg, wp, lp)
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
        // Começa por baixo de tudo, para não apagar o que já estiver no datashow ao abrir o programa.
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
// Exibir no datashow e devolver à tela 1

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fit {
    /// Tira a borda e a barra de título e ocupa o monitor inteiro. Melhor para programas comuns.
    Borderless,
    /// Só maximiza no monitor. Necessário para apps da Store, que desenham a própria moldura.
    Maximize,
}

/// Como a janela estava antes de ir para o datashow, para devolvê-la igualzinha.
#[derive(Clone, Copy)]
pub struct Saved {
    pub hwnd: Hwnd,
    placement: WINDOWPLACEMENT,
    style: isize,
    exstyle: isize,
}

/// Leva `target` para o monitor `mon`. A janela é montada por baixo de `cover`
/// (o que está no ar agora) e só no fim vem para a frente, de uma vez,
/// então o público não vê ela sendo arrastada ou redimensionada.
pub fn take(target: Hwnd, cover: Hwnd, curtain: Hwnd, mon: Rect, fit: Fit) -> Option<Saved> {
    let h = target.h();
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut pl = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        GetWindowPlacement(h, &mut pl).ok()?;
        let saved = Saved {
            hwnd: target,
            placement: pl,
            style: GetWindowLongPtrW(h, GWL_STYLE),
            exstyle: GetWindowLongPtrW(h, GWL_EXSTYLE),
        };

        // 1. Sai de minimizado/maximizado sem roubar o foco do operador.
        if IsIconic(h).as_bool() || IsZoomed(h).as_bool() {
            let mut normal = pl;
            normal.flags = WINDOWPLACEMENT_FLAGS(0);
            normal.showCmd = SW_SHOWNOACTIVATE.0 as u32;
            let _ = SetWindowPlacement(h, &normal);
        }

        // 2. Monta a janela no datashow, por baixo do que está no ar.
        let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
        match fit {
            Fit::Borderless => {
                let caption = (WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as isize;
                let edges = (WS_EX_DLGMODALFRAME | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_WINDOWEDGE).0 as isize;
                SetWindowLongPtrW(h, GWL_STYLE, saved.style & !caption);
                SetWindowLongPtrW(h, GWL_EXSTYLE, saved.exstyle & !edges);
                let _ = SetWindowPos(h, Some(cover.h()), mon.left, mon.top, mon.width(), mon.height(), flags | SWP_FRAMECHANGED);
            }
            Fit::Maximize => {
                let _ = SetWindowPos(h, Some(cover.h()), mon.left + 40, mon.top + 40, mon.width() - 80, mon.height() - 80, flags);
                let _ = ShowWindow(h, SW_MAXIMIZE);
            }
        }

        // 3. A troca: traz para a frente e deixa a cortina logo atrás, para o caso de a janela fechar.
        let _ = SetWindowPos(h, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(curtain.h(), Some(h), mon.left, mon.top, mon.width(), mon.height(), SWP_NOACTIVATE | SWP_SHOWWINDOW);
        Some(saved)
    }
}

/// Devolve a janela ao lugar, tamanho e moldura que tinha antes de ir para o datashow.
pub fn restore(s: &Saved) {
    let h = s.hwnd.h();
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return;
        }
        SetWindowLongPtrW(h, GWL_STYLE, s.style);
        SetWindowLongPtrW(h, GWL_EXSTYLE, s.exstyle);
        let mut pl = s.placement;
        pl.flags = WINDOWPLACEMENT_FLAGS(0);
        pl.showCmd = match SHOW_WINDOW_CMD(pl.showCmd as i32) {
            SW_SHOWMINIMIZED => SW_SHOWMINNOACTIVE.0 as u32,
            SW_SHOWMAXIMIZED => SW_SHOWMAXIMIZED.0 as u32,
            _ => SW_SHOWNOACTIVATE.0 as u32,
        };
        let _ = SetWindowPlacement(h, &pl);
        let _ = SetWindowPos(h, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    }
}

pub fn focus(hwnd: Hwnd) {
    unsafe {
        let _ = SetForegroundWindow(hwnd.h());
    }
}

/// Aperta uma tecla na janela que está no ar (F11 no Chrome e no Fotos põe em tela cheia)
/// e devolve o foco ao painel.
pub fn send_key(target: Hwnd, vk: VIRTUAL_KEY, back_to: Hwnd) {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    };
    unsafe {
        let _ = SetForegroundWindow(target.h());
        std::thread::sleep(std::time::Duration::from_millis(80));
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
        std::thread::sleep(std::time::Duration::from_millis(80));
        let _ = SetForegroundWindow(back_to.h());
    }
}

pub const VK_FULLSCREEN: VIRTUAL_KEY = VK_F11;

// ---------------------------------------------------------------------------
// Prévias ao vivo: o próprio Windows (DWM) desenha a janela de origem dentro do
// nosso painel, sem custo de CPU para o programa.

pub struct Thumbnail {
    pub src: Hwnd,
    id: isize,
}

impl Thumbnail {
    pub fn new(dest: Hwnd, src: Hwnd) -> Option<Self> {
        let id = unsafe { DwmRegisterThumbnail(dest.h(), src.h()).ok()? };
        Some(Thumbnail { src, id })
    }

    /// `area` em pixels físicos, relativos à área interna do painel. A imagem é
    /// centralizada mantendo a proporção da janela de origem, e cortada em `clip`
    /// (o Windows desenha por cima do painel e não sabe onde termina uma área com rolagem).
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
// Ícones dos programas

pub struct Icon {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

/// O ícone que a janela mostra na barra de tarefas; se ela não informar, o ícone do .exe.
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
    // Ícones antigos não têm transparência no canal alfa, só na máscara.
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
// Proteção do datashow: qualquer janela que aparecer na tela 2 sem passar pelo
// TakeScreen (um programa aberto sem querer, uma janela que lembrou da última posição)
// é devolvida na hora para a tela 1.

use std::sync::{Mutex, OnceLock};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};

struct Guard {
    enabled: bool,
    datashow: Rect,
    home: Rect,
    /// Janelas que podem ficar no datashow: a que está no ar e a que está entrando.
    allowed: Vec<isize>,
    /// Títulos devolvidos desde a última vez que o painel olhou.
    returned: Vec<String>,
}

static GUARD: Mutex<Guard> = Mutex::new(Guard {
    enabled: false,
    datashow: Rect { left: 0, top: 0, right: 0, bottom: 0 },
    home: Rect { left: 0, top: 0, right: 0, bottom: 0 },
    allowed: Vec::new(),
    returned: Vec::new(),
});
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Liga os avisos do Windows sobre janelas aparecendo e se movendo. `wake` acorda o painel.
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

pub fn guard_configure(enabled: bool, datashow: Rect, home: Rect, allowed: &[Hwnd]) {
    let mut g = GUARD.lock().unwrap();
    g.enabled = enabled;
    g.datashow = datashow;
    g.home = home;
    g.allowed = allowed.iter().map(|h| h.0).collect();
}

pub fn guard_take_returned() -> Vec<String> {
    std::mem::take(&mut GUARD.lock().unwrap().returned)
}

unsafe extern "system" fn guard_event(_: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    if id_object != OBJID_WINDOW.0 || hwnd.is_invalid() {
        return;
    }
    let (datashow, home) = {
        let g = GUARD.lock().unwrap();
        if !g.enabled || g.allowed.contains(&(hwnd.0 as isize)) {
            return;
        }
        (g.datashow, g.home)
    };
    // Só janelas "de verdade" (as mesmas que aparecem nos cartões); menus e diálogos ficam com o dono.
    let Some(info) = window_info(hwnd) else { return };
    if IsIconic(hwnd).as_bool() {
        return;
    }
    let Some(r) = window_rect(info.hwnd) else { return };
    if !datashow.intersects(&r) {
        return;
    }
    send_home(info.hwnd, home);
    GUARD.lock().unwrap().returned.push(info.title);
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// Coloca a janela no meio da tela 1, sem ativá-la, cabendo na área útil.
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
        let Some(r) = window_rect(hwnd) else { return };
        let w = r.width().min(home.width() * 9 / 10).max(200);
        let ht = r.height().min(home.height() * 9 / 10).max(150);
        let x = home.left + (home.width() - w) / 2;
        let y = home.top + (home.height() - ht) / 2;
        let _ = SetWindowPos(h, None, x, y, w, ht, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}
