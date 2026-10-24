use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::core::w;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationSelectionItemPattern, IUIAutomationTreeWalker,
    UIA_CONTROLTYPE_ID, UIA_DocumentControlTypeId, UIA_SelectionItemPatternId, UIA_TabItemControlTypeId,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ, REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APPCOMMAND};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaSession {
    pub id: String,
    pub app: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub can_toggle: bool,
    pub can_next: bool,
    pub can_prev: bool,
}

#[derive(Clone, Copy)]
// Play and Pause are in the contract; the UI uses Toggle because it only knows `can_toggle`.
#[allow(dead_code)]
pub enum MediaCmd {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
}

pub struct Hub {
    sessions: Arc<Mutex<Vec<MediaSession>>>,
    commands: mpsc::Sender<(String, MediaCmd)>,
    /// Browser windows whose tabs are read (set by the panel).
    browsers: Arc<Mutex<Vec<isize>>>,
    tabs: Arc<Mutex<Vec<(isize, Vec<Tab>)>>>,
}

/// Tabs are read every this many session cycles (500 ms each): a paused tab does not need to appear instantly.
const TAB_CYCLES: u32 = 4;

impl Hub {
    pub fn start(on_change: impl Fn() + Send + 'static) -> Hub {
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let (commands, command_rx) = mpsc::channel::<(String, MediaCmd)>();
        let snapshot = Arc::clone(&sessions);
        let browsers = Arc::new(Mutex::new(Vec::new()));
        let tabs = Arc::new(Mutex::new(Vec::new()));
        let (browsers_shared, tabs_shared) = (Arc::clone(&browsers), Arc::clone(&tabs));
        thread::spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                .ok()
                .and_then(|op| op.join().ok());
            let automation = automation();
            let mut current = Vec::new();
            let mut cycle = 0u32;
            loop {
                cycle = cycle.wrapping_add(1);
                if let (Some(a), true) = (&automation, cycle % TAB_CYCLES == 1) {
                    let wins = browsers_shared.lock().map(|b| b.clone()).unwrap_or_default();
                    let next: Vec<(isize, Vec<Tab>)> = wins.iter().map(|&h| (h, read_tabs(a, h))).collect();
                    if let Ok(mut locked) = tabs_shared.lock() {
                        if *locked != next {
                            *locked = next;
                            on_change();
                        }
                    }
                }
                let mut changed = false;
                while let Ok((id, cmd)) = command_rx.try_recv() {
                    if send_command(&current, &id, cmd) {
                        changed = true;
                    }
                }
                let next = manager.as_ref().map(enumerate).unwrap_or_default();
                let values: Vec<MediaSession> = next.iter().map(|(value, _)| value.clone()).collect();
                if values != current.iter().map(|(value, _)| value.clone()).collect::<Vec<_>>() {
                    if let Ok(mut locked) = snapshot.lock() {
                        *locked = values;
                    }
                    changed = true;
                }
                current = next;
                if changed {
                    on_change();
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
        Hub { sessions, commands, browsers, tabs }
    }

    pub fn sessions(&self) -> Vec<MediaSession> {
        self.sessions.lock().ok().map(|s| s.clone()).unwrap_or_default()
    }

    /// Tabs of each browser window, as of the last read (up to 2 s old).
    pub fn tabs(&self) -> Vec<(isize, Vec<Tab>)> {
        self.tabs.lock().ok().map(|t| t.clone()).unwrap_or_default()
    }

    /// Browser windows whose tab strip should be read.
    pub fn set_browsers(&self, hwnds: &[isize]) {
        if let Ok(mut b) = self.browsers.lock() {
            if *b != hwnds {
                *b = hwnds.to_vec();
            }
        }
    }

    pub fn send(&self, id: &str, cmd: MediaCmd) {
        let _ = self.commands.send((id.to_owned(), cmd));
    }
}

fn enumerate(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Vec<(MediaSession, GlobalSystemMediaTransportControlsSession)> {
    let Ok(view) = manager.GetSessions() else { return Vec::new() };
    let Ok(size) = view.Size() else { return Vec::new() };
    let mut out = Vec::new();
    // Every browser window or tab group is its own session with the same source id: they are told apart by order.
    let sessions: Vec<_> = (0..size).filter_map(|index| view.GetAt(index).ok()).collect();
    let sources: Vec<String> = sessions.iter().map(|s| s.SourceAppUserModelId().map(|v| v.to_string()).unwrap_or_default()).collect();
    for ((session, source), id) in sessions.into_iter().zip(&sources).zip(session_ids(&sources)) {
        if source.is_empty() {
            continue;
        }
        let app = friendly_app_name(source);
        let Ok(info) = session.GetPlaybackInfo() else { continue };
        let status = info.PlaybackStatus().unwrap_or(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed);
        let controls = info.Controls().ok();
        let props = session.TryGetMediaPropertiesAsync().ok().and_then(|op| op.join().ok());
        let title = props.as_ref().and_then(|p| p.Title().ok()).map(|v| v.to_string()).unwrap_or_default();
        let artist = props.as_ref().and_then(|p| p.Artist().ok()).map(|v| v.to_string()).unwrap_or_default();
        out.push((
            MediaSession {
                id,
                app,
                title,
                artist,
                playing: status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing,
                can_toggle: controls.as_ref().and_then(|c| c.IsPlayPauseToggleEnabled().ok()).unwrap_or(false),
                can_next: controls.as_ref().and_then(|c| c.IsNextEnabled().ok()).unwrap_or(false),
                can_prev: controls.as_ref().and_then(|c| c.IsPreviousEnabled().ok()).unwrap_or(false),
            },
            session,
        ));
    }
    out
}

fn send_command(
    sessions: &[(MediaSession, GlobalSystemMediaTransportControlsSession)],
    id: &str,
    cmd: MediaCmd,
) -> bool {
    let Some((_, session)) = sessions.iter().find(|(value, _)| value.id == id) else { return false };
    let result = match cmd {
        MediaCmd::Play => session.TryPlayAsync(),
        MediaCmd::Pause => session.TryPauseAsync(),
        MediaCmd::Toggle => session.TryTogglePlayPauseAsync(),
        MediaCmd::Next => session.TrySkipNextAsync(),
        MediaCmd::Prev => session.TrySkipPreviousAsync(),
    };
    result.ok().and_then(|op| op.join().ok()).unwrap_or(false)
}

/// Unique id of each session: the source id, plus `#n` from the second session of the same source on.
fn session_ids(sources: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashMap::<&str, usize>::new();
    sources
        .iter()
        .map(|source| {
            let count = seen.entry(source).or_default();
            *count += 1;
            if *count == 1 { source.clone() } else { format!("{source}#{count}") }
        })
        .collect()
}

/// Players without a media session that still obey the media keys sent to their window
/// (WM_APPCOMMAND): lowercase exe name without extension, and the name shown to the operator.
const WINDOW_PLAYERS: &[(&str, &str)] = &[
    ("vlc", "VLC"),
    ("wmplayer", "Windows Media Player"),
    ("mpc-hc", "MPC-HC"),
    ("mpc-hc64", "MPC-HC"),
    ("mpc-be", "MPC-BE"),
    ("mpc-be64", "MPC-BE"),
    ("potplayer", "PotPlayer"),
    ("potplayer64", "PotPlayer"),
    ("potplayermini", "PotPlayer"),
    ("potplayermini64", "PotPlayer"),
    ("mpv", "mpv"),
    ("mpvnet", "mpv.net"),
];

/// Friendly names of apps that publish a media session, matched on the session source id.
const SESSION_APPS: &[(&str, &str)] = &[
    ("msedge", "Microsoft Edge"),
    ("chrome", "Google Chrome"),
    ("firefox", "Firefox"),
    ("brave", "Brave"),
    ("opera", "Opera"),
    ("vivaldi", "Vivaldi"),
    ("spotify", "Spotify"),
    ("zunevideo", "Filmes e TV"),
    ("zunemusic", "Media Player"),
    ("windows.photos", "Fotos"),
    ("vlc", "VLC"),
];

/// Whether a browser window title ("Tab - Profile - Browser") shows the tab of a media session.
pub fn title_in_window(session_title: &str, window_title: &str) -> bool {
    !session_title.is_empty() && window_title.contains(session_title)
}

/// Browsers whose windows can host a video without a media session of their own.
const BROWSERS: &[&str] = &["brave", "chrome", "msedge", "firefox", "opera", "vivaldi"];

/// Title fragments of sites that play video; a browser window showing one is a media source.
const VIDEO_SITES: &[&str] = &["youtube", "twitch", "netflix", "vimeo", "prime video", "disney+", "globoplay", "dailymotion"];

pub fn is_browser(exe: &str) -> bool {
    let file = exe.rsplit(['\\', '/']).next().unwrap_or_default().to_ascii_lowercase();
    file.strip_suffix(".exe").is_some_and(|stem| BROWSERS.contains(&stem))
}

/// Whether a browser window title ("(2) Video - YouTube - Brave") shows a video site.
pub fn shows_video(window_title: &str) -> bool {
    let lower = window_title.to_lowercase();
    VIDEO_SITES.iter().any(|site| lower.contains(site))
}

/// Tab title without the notification badge "(2) " and the trailing browser/profile names.
pub fn tab_title(window_title: &str) -> String {
    let mut text = window_title.trim();
    if let Some(rest) = text.strip_prefix('(').and_then(|r| r.split_once(") ")) {
        if rest.0.chars().all(|c| c.is_ascii_digit()) {
            text = rest.1;
        }
    }
    let mut parts: Vec<&str> = text.split(" - ").collect();
    let browser_name = |p: &str| {
        let p = p.replace('\u{200b}', "").to_lowercase();
        ["brave", "google chrome", "microsoft edge", "mozilla firefox", "opera", "vivaldi"].iter().any(|b| p == *b)
    };
    if parts.len() > 1 && browser_name(parts[parts.len() - 1]) {
        parts.pop();
        // Edge adds the profile name before its own name.
        if parts.len() > 1 && parts[parts.len() - 1].starts_with("Profile ") {
            parts.pop();
        }
    }
    parts.join(" - ")
}

/// A browser window as a candidate media source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserWin {
    pub id: isize,
    pub exe: String,
    pub title: String,
    /// Its page is known to be a media page through a signal other than the title (see `remember_media`).
    pub media: bool,
}

/// Browser windows that play media but match no media session, so they need a card of their own.
/// A session claims at most one window (the first whose title contains the session title);
/// a window claimed by a session is shown through that session, with its state and buttons.
pub fn unmatched_video_windows<'a>(sessions: &[MediaSession], wins: &'a [BrowserWin]) -> Vec<&'a BrowserWin> {
    let mut claimed = vec![false; wins.len()];
    for s in sessions {
        let hit = wins.iter().enumerate().position(|(i, w)| !claimed[i] && same_app(&s.app, &w.exe) && title_in_window(&s.title, &w.title));
        if let Some(i) = hit {
            claimed[i] = true;
        }
    }
    wins.iter().zip(claimed).filter(|(w, claimed)| !claimed && (w.media || shows_video(&w.title))).map(|(w, _)| w).collect()
}

/// A browser tab read from the window's tab strip through UI Automation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    /// Tab title without the browser status notes ("Memory usage", "Audio playing").
    pub title: String,
    /// The browser marks the tab as playing sound.
    pub audible: bool,
    /// The browser shows a sound indicator on the tab (playing or muted): the page is a player, whatever the site.
    pub noted: bool,
}

/// Splits the accessible name of a tab ("Video - YouTube - Audio playing - Memory usage - 404 MB")
/// into its title and the sound indicators the browser appends to it.
pub fn parse_tab(name: &str) -> Tab {
    const NOTES: &[&str] = &["memory usage", "uso de memória"];
    const PLAYING: &[&str] = &["audio playing", "playing audio", "reproduzindo áudio"];
    const MUTED: &[&str] = &["audio muted", "tab muted", "site muted", "áudio desativado", "som desativado"];
    let mut parts: Vec<&str> = name.trim().split(" - ").collect();
    let (mut audible, mut noted) = (false, false);
    while parts.len() > 1 {
        let last = parts[parts.len() - 1].trim().to_lowercase();
        let before = parts[parts.len() - 2].trim().to_lowercase();
        let starts = |list: &[&str]| list.iter().any(|n| last.starts_with(n));
        // "Memory usage - 404 MB": the size is its own segment, removed together with the note before it.
        let size = last.ends_with(" mb") || last.ends_with(" gb");
        if size && NOTES.iter().any(|n| before.starts_with(n)) {
            parts.pop();
        } else if starts(NOTES) || starts(PLAYING) || starts(MUTED) {
            audible |= starts(PLAYING);
            noted |= starts(PLAYING) || starts(MUTED);
            parts.pop();
        } else {
            break;
        }
    }
    Tab { title: parts.join(" - "), audible, noted }
}

/// Whether a page is a media source: a known video site in its title, a sound indicator on its tab,
/// or a page already seen playing in this session (`seen`), which keeps paused players of any site listed.
fn media_page(title: &str, noted: bool, seen: bool) -> bool {
    noted || seen || shows_video(title)
}

/// Pages seen as players, by window and tab title (`tab_title`): a tab with a sound indicator, or a
/// tab or window whose title holds the title of a media session of the same browser. Entries of
/// windows that closed are dropped.
pub fn remember_media(seen: &mut HashSet<(isize, String)>, sessions: &[MediaSession], wins: &[BrowserWin], tabs: &[(isize, Vec<Tab>)]) {
    seen.retain(|(h, _)| wins.iter().any(|w| w.id == *h));
    for w in wins {
        let played = |title: &str| sessions.iter().any(|s| same_app(&s.app, &w.exe) && title_in_window(&s.title, title));
        if played(&w.title) {
            seen.insert((w.id, tab_title(&w.title)));
        }
        let list = tabs.iter().find(|(h, _)| *h == w.id).map(|(_, l)| l.as_slice()).unwrap_or_default();
        for t in list.iter().filter(|t| t.noted || played(&t.title)) {
            seen.insert((w.id, tab_title(&t.title)));
        }
    }
}

/// Background tabs that are media pages and have no media session: they have no window of their
/// own (the window card shows the active tab), so each one needs a card. `tabs` pairs a window id
/// with its tabs; windows that are no longer in `wins` are ignored.
pub fn background_video_tabs<'a>(sessions: &[MediaSession], wins: &[BrowserWin], tabs: &'a [(isize, Vec<Tab>)], seen: &HashSet<(isize, String)>) -> Vec<(isize, &'a Tab)> {
    let mut out = Vec::new();
    for (hwnd, list) in tabs {
        let Some(w) = wins.iter().find(|w| w.id == *hwnd) else { continue };
        let active = tab_title(&w.title);
        for t in list {
            let title = tab_title(&t.title);
            let session = sessions.iter().any(|s| same_app(&s.app, &w.exe) && title_in_window(&s.title, &t.title));
            if title != active && !session && media_page(&t.title, t.noted, seen.contains(&(*hwnd, title.clone()))) {
                out.push((*hwnd, t));
            }
        }
    }
    out
}

/// Walks the browser interface of `hwnd` (the page itself is skipped) and calls `f` with every
/// tab and its accessible name until `f` returns `true`.
fn for_each_tab(automation: &IUIAutomation, hwnd: isize, mut f: impl FnMut(&IUIAutomationElement, &str) -> bool) {
    fn walk(walker: &IUIAutomationTreeWalker, parent: &IUIAutomationElement, depth: u32, f: &mut dyn FnMut(&IUIAutomationElement, &str) -> bool) -> bool {
        let mut child = unsafe { walker.GetFirstChildElement(parent) }.ok();
        while let Some(c) = child {
            let kind = unsafe { c.CurrentControlType() }.unwrap_or(UIA_CONTROLTYPE_ID(0));
            if kind == UIA_TabItemControlTypeId {
                let name = unsafe { c.CurrentName() }.map(|n| n.to_string()).unwrap_or_default();
                if f(&c, &name) {
                    return true;
                }
            } else if kind != UIA_DocumentControlTypeId && depth < 12 && walk(walker, &c, depth + 1, f) {
                // The page (Document) is never entered: reading it would make the browser build its whole accessibility tree.
                return true;
            }
            child = unsafe { walker.GetNextSiblingElement(&c) }.ok();
        }
        false
    }
    unsafe {
        let Ok(root) = automation.ElementFromHandle(HWND(hwnd as *mut _)) else { return };
        let Ok(walker) = automation.ControlViewWalker() else { return };
        walk(&walker, &root, 0, &mut f);
    }
}

fn automation() -> Option<IUIAutomation> {
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() }
}

fn read_tabs(automation: &IUIAutomation, hwnd: isize) -> Vec<Tab> {
    let mut tabs = Vec::new();
    for_each_tab(automation, hwnd, |_, name| {
        tabs.push(parse_tab(name));
        false
    });
    tabs
}

/// Makes the tab titled `title` the active tab of browser window `hwnd`, as a click on it would.
pub fn select_tab(hwnd: isize, title: &str) -> bool {
    // The panel thread may already have COM; a second call with the same mode only adds a reference.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let Some(automation) = automation() else { return false };
    let mut selected = false;
    for_each_tab(&automation, hwnd, |tab, name| {
        if parse_tab(name).title != title {
            return false;
        }
        selected = unsafe { tab.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId).and_then(|p| p.Select()) }.is_ok();
        true
    });
    selected
}

/// Key that toggles playback in the page: `k` on YouTube (it works with focus anywhere in the page), else space.
pub fn play_key_is_k(window_title: &str) -> bool {
    window_title.to_lowercase().contains("youtube")
}

/// Name of the player when `exe` is one driven through its window, else `None`.
pub fn window_player(exe: &str) -> Option<&'static str> {
    let file = exe.rsplit(['\\', '/']).next()?.to_ascii_lowercase();
    let stem = file.strip_suffix(".exe")?;
    WINDOW_PLAYERS.iter().find(|(name, _)| *name == stem).map(|(_, label)| *label)
}

/// Name shown for a media session app (`app` is the cleaned source id).
pub fn display_name(app: &str) -> String {
    let lower = app.to_ascii_lowercase();
    SESSION_APPS.iter().find(|(key, _)| lower.contains(key)).map(|(_, label)| (*label).to_owned()).unwrap_or_else(|| app.to_owned())
}

/// Extracts a friendly app name from the source string.
fn friendly_app_name(source: &str) -> String {
    let name = source.rsplit(['\\', '/']).next().unwrap_or(source);
    let name = name.split('!').next().unwrap_or(name);
    name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name).to_owned()
}

/// Identity of a controllable media source: a Windows media session or a plain window
/// (VLC does not register a session, so it is driven through WM_APPCOMMAND).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKey {
    Session(String),
    Window(isize),
    /// A background tab of a browser window, by window and tab title.
    Tab(isize, String),
}

/// Picks the source the transport buttons act on. Returns the index and whether the
/// remembered choice vanished. Without a valid choice, the first playing source wins, then the first.
pub fn resolve(selected: Option<&SourceKey>, keys: &[SourceKey], playing: &[bool]) -> (Option<usize>, bool) {
    if let Some(i) = selected.and_then(|s| keys.iter().position(|k| k == s)) {
        return (Some(i), false);
    }
    let auto = playing.iter().position(|&p| p).or(if keys.is_empty() { None } else { Some(0) });
    (auto, selected.is_some())
}

/// Whether a window process belongs to the app named by a media session ("msedge" and "msedge.exe").
pub fn same_app(session_app: &str, exe: &str) -> bool {
    let stem = |s: &str| {
        let s = s.rsplit(['\\', '/']).next().unwrap_or(s).to_ascii_lowercase();
        s.strip_suffix(".exe").map(str::to_owned).unwrap_or(s)
    };
    !session_app.is_empty() && stem(session_app) == stem(exe)
}

pub fn send_to_window(h: crate::win::Hwnd, cmd: MediaCmd) -> bool {
    let command = match cmd {
        MediaCmd::Play => 46,
        MediaCmd::Pause => 47,
        MediaCmd::Toggle => 14,
        MediaCmd::Next => 11,
        MediaCmd::Prev => 12,
    };
    let hwnd = HWND(h.0 as *mut _);
    let lparam = LPARAM((command as isize) << 16);
    unsafe { PostMessageW(Some(hwnd), WM_APPCOMMAND, WPARAM(0), lparam).is_ok() }
}

/// `fullscreen_on`: VLC's own full screen on that monitor; `None` opens an ordinary window,
/// so the Maximize fit keeps the Windows title bar and buttons.
pub fn vlc_args(file: &str, fullscreen_on: Option<u32>) -> Vec<String> {
    let mut args: Vec<String> = vec!["--no-video-title-show".into(), "--no-osd".into()];
    if let Some(screen) = fullscreen_on {
        args.splice(0..0, ["--fullscreen".into()]);
        args.extend(["--no-qt-fs-controller".into(), format!("--qt-fullscreen-screennumber={screen}")]);
    }
    args.push(file.into());
    args
}

pub fn find_vlc() -> Option<PathBuf> {
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for key in [w!("SOFTWARE\\VideoLAN\\VLC"), w!("SOFTWARE\\WOW6432Node\\VideoLAN\\VLC")] {
            if let Some(dir) = registry_string(root, key, w!("InstallDir")) {
                let path = PathBuf::from(dir).join("vlc.exe");
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    let mut dirs = Vec::new();
    if let Some(value) = std::env::var_os("ProgramFiles") { dirs.push(PathBuf::from(value)); }
    if let Some(value) = std::env::var_os("ProgramFiles(x86)") { dirs.push(PathBuf::from(value)); }
    dirs.into_iter().map(|dir| dir.join("VideoLAN\\VLC\\vlc.exe")).find(|path| path.is_file())
}

fn registry_string(root: HKEY, key: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<String> {
    unsafe {
        let mut handle = HKEY::default();
        RegOpenKeyExW(root, key, None, KEY_READ, &mut handle).ok().ok()?;
        let mut kind = REG_SZ;
        let mut bytes = 0u32;
        let query = RegQueryValueExW(handle, value, None, Some(&mut kind), None, Some(&mut bytes));
        if query.is_err() || kind != REG_SZ || bytes < 2 { let _ = RegCloseKey(handle); return None; }
        let mut data = vec![0u16; (bytes as usize + 1) / 2];
        let query = RegQueryValueExW(handle, value, None, Some(&mut kind), Some(data.as_mut_ptr().cast()), Some(&mut bytes));
        let _ = RegCloseKey(handle);
        if query.is_err() { return None; }
        let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
        Some(String::from_utf16_lossy(&data[..len]))
    }
}

pub fn open_in_vlc(file: &str, fullscreen_on: Option<u32>) -> Result<(), String> {
    let path = find_vlc().ok_or_else(|| "VLC não foi encontrado".to_owned())?;
    std::process::Command::new(path)
        .args(vlc_args(file, fullscreen_on))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limpa_nome_do_app() {
        assert_eq!(friendly_app_name(r"Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"), "Microsoft.ZuneMusic_8wekyb3d8bbwe");
        assert_eq!(friendly_app_name("VLC.exe!App"), "VLC");
        assert_eq!(friendly_app_name(r"C:\Apps\Chrome.exe"), "Chrome");
    }

    #[test]
    fn escolhe_fonte_e_volta_para_automatica() {
        let a = SourceKey::Session("a".into());
        let b = SourceKey::Window(7);
        let keys = [a.clone(), b.clone()];
        assert_eq!(resolve(Some(&b), &keys, &[true, false]), (Some(1), false));
        assert_eq!(resolve(None, &keys, &[false, true]), (Some(1), false));
        assert_eq!(resolve(None, &keys, &[false, false]), (Some(0), false));
        assert_eq!(resolve(Some(&SourceKey::Window(9)), &keys, &[false, true]), (Some(1), true));
        assert_eq!(resolve(Some(&b), &[], &[]), (None, true));
        assert_eq!(resolve(None, &[], &[]), (None, false));
    }

    #[test]
    fn reconhece_app_e_vlc() {
        assert!(same_app("msedge", r"C:\Edge\MSEdge.exe"));
        assert!(!same_app("", ""));
        assert!(!same_app("chrome", "vlc.exe"));
        assert_eq!(window_player(r"C:\Program Files\VideoLAN\VLC\vlc.exe"), Some("VLC"));
        assert_eq!(window_player("vlcx.exe"), None);
    }

    #[test]
    fn reconhece_reprodutores_comuns_do_windows() {
        for (exe, name) in [("wmplayer.exe", "Windows Media Player"), ("MPC-HC64.exe", "MPC-HC"), ("mpc-be64.exe", "MPC-BE"), ("PotPlayerMini64.exe", "PotPlayer"), ("mpv.exe", "mpv")] {
            assert_eq!(window_player(exe), Some(name), "{exe}");
        }
        // Browsers and Store apps use their media session, not the window.
        assert_eq!(window_player("msedge.exe"), None);
        assert_eq!(display_name("msedge"), "Microsoft Edge");
        assert_eq!(display_name("Microsoft.ZuneVideo_8wekyb3d8bbwe"), "Filmes e TV");
        assert_eq!(display_name("Microsoft.ZuneMusic_8wekyb3d8bbwe"), "Media Player");
        assert_eq!(display_name("Microsoft.Windows.Photos_8wekyb3d8bbwe"), "Fotos");
        assert_eq!(display_name("Spotify"), "Spotify");
        assert_eq!(display_name("Outro"), "Outro");
    }

    fn session(app: &str, title: &str) -> MediaSession {
        MediaSession { id: app.into(), app: app.into(), title: title.into(), artist: String::new(), playing: false, can_toggle: true, can_next: false, can_prev: false }
    }

    fn win(id: isize, title: &str) -> BrowserWin {
        BrowserWin { id, exe: r"C:\Brave\brave.exe".into(), title: title.into(), media: false }
    }

    #[test]
    fn titulo_da_aba_sem_navegador_nem_contador() {
        assert_eq!(tab_title("(2) Fiz um jogo - YouTube - Brave"), "Fiz um jogo - YouTube");
        assert_eq!(tab_title("Teste Apolo 1 - Profile 1 - Microsoft\u{200b} Edge"), "Teste Apolo 1");
        assert_eq!(tab_title("Sem navegador"), "Sem navegador");
        assert_eq!(tab_title("(rascunho) Nota - Brave"), "(rascunho) Nota");
    }

    #[test]
    fn so_navegadores_com_site_de_video() {
        assert!(is_browser(r"C:\Brave\brave.exe") && is_browser("msedge.exe") && !is_browser("vlc.exe"));
        assert!(shows_video("(2) Zelda - YouTube - Brave"));
        assert!(!shows_video("Planilha - Brave"));
        assert!(play_key_is_k("x - YouTube - Brave") && !play_key_is_k("Filme - Netflix - Brave"));
    }

    #[test]
    fn duas_janelas_com_uma_sessao_viram_duas_fontes() {
        let wins = [win(1, "(2) Jogo com IA - YouTube - Brave"), win(2, "(2) Zelda - YouTube - Brave"), win(3, "Planilha - Brave")];
        // One session only (Chromium shows the active tab): window 1 is covered by it, window 2 stays alone.
        let left = unmatched_video_windows(&[session("brave", "Jogo com IA")], &wins);
        assert_eq!(left.iter().map(|w| w.id).collect::<Vec<_>>(), [2]);
        // No session at all: both video windows appear, the spreadsheet does not.
        assert_eq!(unmatched_video_windows(&[], &wins).len(), 2);
        // A session claims a single window, even if two titles contain its text.
        let twins = [win(1, "Aula - YouTube - Brave"), win(2, "Aula - YouTube - Brave")];
        assert_eq!(unmatched_video_windows(&[session("brave", "Aula")], &twins).iter().map(|w| w.id).collect::<Vec<_>>(), [2]);
        // A session of another app does not claim the window.
        assert_eq!(unmatched_video_windows(&[session("chrome", "Jogo com IA")], &wins[..1]).len(), 1);
    }

    #[test]
    fn nome_da_aba_sem_avisos_do_navegador() {
        let parsed = |name: &str| {
            let t = parse_tab(name);
            (t.title, t.audible, t.noted)
        };
        // Names read from the Brave and Chrome tab strips on 2026-10-23.
        assert_eq!(parsed("(2) Zelda - YouTube - Memory usage - 404 MB"), ("(2) Zelda - YouTube".to_owned(), false, false));
        assert_eq!(parsed("TV Globo Ao Vivo | Globoplay - Audio playing - Memory usage - 477 MB"), ("TV Globo Ao Vivo | Globoplay".to_owned(), true, true));
        assert_eq!(parsed("Avicii - Wake Me Up (Official Video) - YouTube - Memory usage - 264 MB"), ("Avicii - Wake Me Up (Official Video) - YouTube".to_owned(), false, false));
        assert_eq!(parsed("Top Agent Skill Repositories on GitHub · Linkly AI"), ("Top Agent Skill Repositories on GitHub · Linkly AI".to_owned(), false, false));
        assert_eq!(parsed("Aula - YouTube - Reproduzindo áudio"), ("Aula - YouTube".to_owned(), true, true));
        // A muted tab is still a player, though it is not audible.
        assert_eq!(parsed("Radio - Audio muted"), ("Radio".to_owned(), false, true));
        // A title that merely ends in a size is kept.
        assert_eq!(parsed("Download 300 MB"), ("Download 300 MB".to_owned(), false, false));
    }

    fn tab(title: &str) -> Tab {
        Tab { title: title.into(), audible: false, noted: false }
    }

    #[test]
    fn qualquer_site_que_tocou_continua_na_lista() {
        let wins = [win(1, "Alarm01.wav - Brave"), win(2, "Podcast X - Brave"), win(3, "Planilha - Brave")];
        let mut noted = tab("Radio Gaucha");
        noted.noted = true;
        let tabs = vec![(1, vec![tab("Alarm01.wav")]), (2, vec![tab("Podcast X"), noted]), (3, vec![tab("Planilha")])];
        let mut seen = HashSet::new();
        // Window 1 had a media session; window 2 has a tab with a sound indicator; window 3 never played.
        remember_media(&mut seen, &[session("brave", "Alarm01.wav")], &wins, &tabs);
        assert!(seen.contains(&(1, "Alarm01.wav".to_owned())) && seen.contains(&(2, "Radio Gaucha".to_owned())));
        assert_eq!(seen.len(), 2);
        // The session is gone (paused for a long time): the remembered window still has a card.
        let mut known = wins.clone();
        for w in &mut known {
            w.media = seen.contains(&(w.id, tab_title(&w.title)));
        }
        assert_eq!(unmatched_video_windows(&[], &known).iter().map(|w| w.id).collect::<Vec<_>>(), [1]);
        // The background tab with a sound indicator gets a card, whatever the site.
        let found: Vec<_> = background_video_tabs(&[], &wins, &tabs, &seen).into_iter().map(|(h, t)| (h, t.title.clone())).collect();
        assert_eq!(found, [(2, "Radio Gaucha".to_owned())]);
        // Closing window 2 forgets its pages.
        remember_media(&mut seen, &[], &wins[..1], &tabs);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn abas_de_video_em_segundo_plano_viram_fontes() {
        let wins = [win(1, "affaan-m/ECC - Brave"), win(2, "(2) Zelda - YouTube - Brave")];
        let tabs = vec![
            (1, vec![tab("(2) Fiz um jogo - YouTube"), tab("Pesquisa Google"), tab("affaan-m/ECC")]),
            (2, vec![tab("Release notes"), tab("(2) Zelda - YouTube")]),
        ];
        // Only the hidden YouTube tab of window 1: the active tabs already have a window card, and other sites are not video.
        let none = HashSet::new();
        let found: Vec<_> = background_video_tabs(&[], &wins, &tabs, &none).into_iter().map(|(h, t)| (h, t.title.clone())).collect();
        assert_eq!(found, [(1, "(2) Fiz um jogo - YouTube".to_owned())]);
        // A tab that has a media session is shown through the session.
        assert!(background_video_tabs(&[session("brave", "Fiz um jogo")], &wins, &tabs, &none).is_empty());
        // Tabs of a window that closed are ignored.
        assert!(background_video_tabs(&[], &wins[1..], &tabs[..1], &none).is_empty());
    }

    #[test]
    fn tres_abas_do_mesmo_navegador_viram_tres_fontes() {
        let sources: Vec<String> = ["msedge.exe", "Spotify.exe", "msedge.exe", "msedge.exe"].map(String::from).to_vec();
        assert_eq!(session_ids(&sources), ["msedge.exe", "Spotify.exe", "msedge.exe#2", "msedge.exe#3"]);
    }

    #[test]
    fn monta_argumentos_do_vlc() {
        assert_eq!(vlc_args("video.mp4", Some(2)), [
            "--fullscreen", "--no-video-title-show", "--no-osd", "--no-qt-fs-controller",
            "--qt-fullscreen-screennumber=2", "video.mp4",
        ]);
        assert_eq!(vlc_args("video.mp4", None), ["--no-video-title-show", "--no-osd", "video.mp4"]);
    }
}
