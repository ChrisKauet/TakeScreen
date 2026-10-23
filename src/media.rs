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
    let mut app_counts = std::collections::HashMap::<String, usize>::new();
    for index in 0..size {
        let Ok(session) = view.GetAt(index) else { continue };
        let Ok(source) = session.SourceAppUserModelId() else { continue };
        let source = source.to_string();
        let app = friendly_app_name(&source);
        let count = app_counts.entry(source.clone()).or_default();
        *count += 1;
        let id = if *count == 1 { source } else { format!("{source}#{count}") };
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

/// Extracts a friendly app name from the source string.
fn friendly_app_name(source: &str) -> String {
    let name = source.rsplit(['\\', '/']).next().unwrap_or(source);
    let name = name.split('!').next().unwrap_or(name);
    name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name).to_owned()
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

pub fn vlc_args(file: &str, screen: u32) -> Vec<String> {
    vec![
        "--fullscreen".into(),
        "--no-video-title-show".into(),
        "--no-osd".into(),
        "--no-qt-fs-controller".into(),
        format!("--qt-fullscreen-screennumber={screen}"),
        file.into(),
    ]
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

pub fn open_in_vlc(file: &str, screen: u32) -> Result<(), String> {
    let path = find_vlc().ok_or_else(|| "VLC não foi encontrado".to_owned())?;
    std::process::Command::new(path)
        .args(vlc_args(file, screen))
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
    fn monta_argumentos_do_vlc() {
        assert_eq!(vlc_args("video.mp4", 2), [
            "--fullscreen", "--no-video-title-show", "--no-osd", "--no-qt-fs-controller",
            "--qt-fullscreen-screennumber=2", "video.mp4",
        ]);
    }
}
