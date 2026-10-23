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
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
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
}

impl Hub {
    pub fn start(on_change: impl Fn() + Send + 'static) -> Hub {
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let (commands, command_rx) = mpsc::channel::<(String, MediaCmd)>();
        let snapshot = Arc::clone(&sessions);
        thread::spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                .ok()
                .and_then(|op| op.join().ok());
            let mut current = Vec::new();
            loop {
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
        Hub { sessions, commands }
    }

    pub fn sessions(&self) -> Vec<MediaSession> {
        self.sessions.lock().ok().map(|s| s.clone()).unwrap_or_default()
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
