use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE, eConsole, eRender,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

/// System sound group key (Windows notification sounds).
const SYSTEM_KEY: &str = "sistema";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sound {
    Playing,
    /// Has audio session, but not playing.
    Idle,
    /// Open program that hasn't created an audio session yet.
    None,
}

/// A program in the mixer list. `id` is the program key (lowercase exe path):
/// volume and mute apply to all its sessions, on all output devices.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioApp {
    pub id: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
    pub sound: Sound,
}

/// An audio session already linked to its owning program.
#[derive(Clone, Debug, PartialEq)]
struct Sess {
    key: String,
    name: String,
    volume: f32,
    muted: bool,
    peak: f32,
    active: bool,
}

#[derive(Clone)]
struct Snapshot {
    apps: Vec<AudioApp>,
    master: (f32, bool),
}

enum Command {
    SetVolume(String, f32),
    SetMute(String, bool),
    SetMaster(f32),
    SetMasterMute(bool),
}

pub struct Hub {
    snapshot: Arc<Mutex<Snapshot>>,
    commands: mpsc::Sender<Command>,
    known: Arc<Mutex<HashSet<String>>>,
}

impl Hub {
    pub fn start(on_change: impl Fn() + Send + 'static) -> Hub {
        let snapshot = Arc::new(Mutex::new(Snapshot { apps: Vec::new(), master: (1.0, false) }));
        let known = Arc::new(Mutex::new(HashSet::new()));
        let (commands, command_rx) = mpsc::channel();
        let shared = Arc::clone(&snapshot);
        let known_shared = Arc::clone(&known);
        thread::spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            loop {
                let commands: Vec<_> = command_rx.try_iter().collect();
                let known = known_shared.lock().map(|k| k.clone()).unwrap_or_default();
                let next = tick(&commands, &known);
                if let Ok(mut current) = shared.lock() {
                    *current = next;
                }
                on_change();
                thread::sleep(Duration::from_millis(250));
            }
        });
        Hub { snapshot, commands, known }
    }

    /// Programs with audio sessions (those opened without sound go in `rows`).
    pub fn apps(&self) -> Vec<AudioApp> {
        self.snapshot.lock().ok().map(|s| s.apps.clone()).unwrap_or_default()
    }

    pub fn master(&self) -> (f32, bool) {
        self.snapshot.lock().ok().map(|s| s.master).unwrap_or((0.0, false))
    }

    /// Exe paths with open windows: audio from a child process is linked to the ancestor in this list.
    pub fn set_known(&self, exes: &[String]) {
        let set: HashSet<String> = exes.iter().map(|e| e.to_lowercase()).filter(|e| !e.is_empty()).collect();
        if let Ok(mut known) = self.known.lock() {
            if *known != set {
                *known = set;
            }
        }
    }

    pub fn set_volume(&self, id: &str, v: f32) {
        let _ = self.commands.send(Command::SetVolume(id.to_owned(), volume(v)));
    }

    pub fn set_mute(&self, id: &str, m: bool) {
        let _ = self.commands.send(Command::SetMute(id.to_owned(), m));
    }

    pub fn set_master(&self, v: f32) {
        let _ = self.commands.send(Command::SetMaster(volume(v)));
    }

    pub fn set_master_mute(&self, m: bool) {
        let _ = self.commands.send(Command::SetMasterMute(m));
    }
}

/// Merges sessions by program: highest volume and peak, mute only if all are muted.
fn group(sessions: Vec<Sess>) -> Vec<AudioApp> {
    let mut apps: Vec<AudioApp> = Vec::new();
    for s in sessions {
        let sound = if s.active { Sound::Playing } else { Sound::Idle };
        match apps.iter_mut().find(|a| a.id == s.key) {
            Some(a) => {
                a.volume = a.volume.max(s.volume);
                a.muted &= s.muted;
                a.peak = a.peak.max(s.peak);
                if sound == Sound::Playing {
                    a.sound = Sound::Playing;
                }
            }
            None => apps.push(AudioApp { id: s.key, name: s.name, volume: s.volume, muted: s.muted, peak: s.peak, sound }),
        }
    }
    apps
}

/// Final mixer list: one item per open program (`programs` = exe path and card name)
/// plus those with only sessions. Order: playing, idle with session, no audio; then by name.
pub fn rows(groups: Vec<AudioApp>, programs: &[(String, String)]) -> Vec<AudioApp> {
    let mut out = groups;
    for (path, label) in programs {
        let key = path.to_lowercase();
        if key.is_empty() {
            continue;
        }
        match out.iter_mut().find(|a| a.id == key) {
            Some(a) => a.name = label.clone(),
            None => out.push(AudioApp { id: key, name: label.clone(), volume: 1.0, muted: false, peak: 0.0, sound: Sound::None }),
        }
    }
    let rank = |s: Sound| match s {
        Sound::Playing => 0,
        Sound::Idle => 1,
        Sound::None => 2,
    };
    out.sort_by_cached_key(|a| (rank(a.sound), a.name.to_lowercase()));
    out
}

/// Splits the sorted list into programs playing now and the quiet ones (idle session or none).
pub fn split_quiet(rows: Vec<AudioApp>) -> (Vec<AudioApp>, Vec<AudioApp>) {
    rows.into_iter().partition(|a| a.sound == Sound::Playing)
}

/// Exe owning the audio of `pid`: itself if it has a window, else the first ancestor with a window
/// (browsers and Electron play in a child process). If no ancestor has a window, the inverse:
/// a descendant with a window (in Steam audio is from steam.exe, window is from steamwebhelper).
/// If neither, the own exe.
fn resolve(pid: u32, parent: &HashMap<u32, u32>, names: &HashMap<u32, String>, exe_of: &mut dyn FnMut(u32) -> String, known: &HashSet<String>) -> String {
    let own = exe_of(pid);
    if own.is_empty() {
        return format!("pid:{pid}");
    }
    if known.contains(&own) {
        return own;
    }
    let mut at = pid;
    for _ in 0..16 {
        let Some(&up) = parent.get(&at) else { break };
        if up == 0 || up == at {
            break;
        }
        let exe = exe_of(up);
        // Explorer opens almost everything: linking audio from any app to it would be wrong.
        if known.contains(&exe) && !exe.ends_with("\\explorer.exe") {
            return exe;
        }
        at = up;
    }
    // Only queries the path of processes whose file name matches some exe with a window.
    let files: HashSet<&str> = known.iter().filter_map(|k| k.rsplit('\\').next()).collect();
    for (&child, name) in names {
        if child == pid || !files.contains(name.to_lowercase().as_str()) {
            continue;
        }
        let exe = exe_of(child);
        if !known.contains(&exe) || exe.ends_with("\\explorer.exe") {
            continue;
        }
        let mut at = child;
        for _ in 0..16 {
            match parent.get(&at) {
                Some(&up) if up == pid => return exe,
                Some(&up) if up != 0 && up != at => at = up,
                _ => break,
            }
        }
    }
    own
}

/// Parent and exe name of each process (the name helps when full path can't be read).
fn process_tree() -> (HashMap<u32, u32>, HashMap<u32, String>) {
    let mut map = HashMap::new();
    let mut names = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return (map, names) };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                names.insert(entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile).trim_end_matches(' ').to_owned());
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    (map, names)
}

fn enumerator() -> Option<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() }
}

fn render_devices() -> Vec<IMMDevice> {
    let Some(enumerator) = enumerator() else { return Vec::new() };
    unsafe {
        let Ok(list) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let count = list.GetCount().unwrap_or(0);
        (0..count).filter_map(|i| list.Item(i).ok()).collect()
    }
}

fn default_device() -> Option<IMMDevice> {
    unsafe { enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole).ok() }
}

/// All sessions from all active output devices, already linked to owning program.
/// Volume and mute commands are applied in the same pass.
fn read_sessions(commands: &[Command], known: &HashSet<String>) -> Vec<Sess> {
    let (parent, names) = process_tree();
    let mut paths: HashMap<u32, String> = HashMap::new();
    let mut exe_of = |pid: u32| paths.entry(pid).or_insert_with(|| process_path(pid).to_lowercase()).clone();
    let mut sessions = Vec::new();
    for device in render_devices() {
        let Some(manager) = (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok() }) else { continue };
        let Ok(list) = (unsafe { manager.GetSessionEnumerator() }) else { continue };
        let count = unsafe { list.GetCount() }.unwrap_or(0);
        for index in 0..count {
            let Ok(control) = (unsafe { list.GetSession(index) }) else { continue };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
            let Ok(vol) = control.cast::<ISimpleAudioVolume>() else { continue };
            let pid = unsafe { control2.GetProcessId() }.unwrap_or(0);
            let system = pid == 0 || unsafe { control2.IsSystemSoundsSession() } == windows::core::HRESULT(0);
            let (key, name) = if system {
                (SYSTEM_KEY.to_owned(), "Sons do sistema".to_owned())
            } else {
                let key = resolve(pid, &parent, &names, &mut exe_of, known);
                let name = match key.rsplit(['\\', '/']).next() {
                    Some(file) if !key.starts_with("pid:") => capitalize(file.trim_end_matches(".exe")),
                    _ => names.get(&pid).map(|n| capitalize(n.trim_end_matches(".exe"))).unwrap_or_else(|| "Desconhecido".into()),
                };
                (key, name)
            };
            for command in commands {
                match command {
                    Command::SetVolume(id, level) if *id == key => {
                        let _ = unsafe { vol.SetMasterVolume(*level, std::ptr::null()) };
                    }
                    Command::SetMute(id, muted) if *id == key => {
                        let _ = unsafe { vol.SetMute(*muted, std::ptr::null()) };
                    }
                    _ => {}
                }
            }
            sessions.push(Sess {
                key,
                name,
                volume: unsafe { vol.GetMasterVolume() }.unwrap_or(0.0).clamp(0.0, 1.0),
                muted: unsafe { vol.GetMute() }.map(|m| m.as_bool()).unwrap_or(false),
                peak: control.cast::<IAudioMeterInformation>().ok().and_then(|m| unsafe { m.GetPeakValue().ok() }).unwrap_or(0.0).clamp(0.0, 1.0),
                active: unsafe { control.GetState() }.unwrap_or_default() == AudioSessionStateActive,
            });
        }
    }
    sessions
}

/// One cycle: applies commands (on all devices) and reads state.
fn tick(commands: &[Command], known: &HashSet<String>) -> Snapshot {
    let sessions = read_sessions(commands, known);
    // Master volume is always from the default device.
    let master = default_device()
        .and_then(|d| unsafe { d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok() })
        .map(|endpoint| unsafe {
            for command in commands {
                match command {
                    Command::SetMaster(level) => {
                        let _ = endpoint.SetMasterVolumeLevelScalar(*level, std::ptr::null());
                    }
                    Command::SetMasterMute(muted) => {
                        let _ = endpoint.SetMute(*muted, std::ptr::null());
                    }
                    _ => {}
                }
            }
            (endpoint.GetMasterVolumeLevelScalar().unwrap_or(0.0).clamp(0.0, 1.0), endpoint.GetMute().unwrap_or_default().as_bool())
        })
        .unwrap_or((0.0, false));
    Snapshot { apps: group(sessions), master }
}

fn process_path(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, windows::core::PWSTR(buffer.as_mut_ptr()), &mut length);
        let _ = CloseHandle(handle);
        result.ok().map(|_| String::from_utf16_lossy(&buffer[..length as usize])).unwrap_or_default()
    }
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return "Desconhecido".into() };
    first.to_uppercase().collect::<String>() + chars.as_str()
}

fn volume(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_fica_entre_zero_e_um() {
        assert_eq!(volume(-0.5), 0.0);
        assert_eq!(volume(0.35), 0.35);
        assert_eq!(volume(2.0), 1.0);
    }

    fn sess(key: &str, volume: f32, muted: bool, peak: f32, active: bool) -> Sess {
        Sess { key: key.into(), name: key.into(), volume, muted, peak, active }
    }

    #[test]
    fn agrupa_sessoes_do_mesmo_programa() {
        let apps = group(vec![sess("edge", 0.4, true, 0.1, false), sess("edge", 0.8, false, 0.5, true), sess("vlc", 0.2, true, 0.0, false)]);
        assert_eq!(apps.len(), 2);
        let edge = &apps[0];
        assert_eq!((edge.volume, edge.muted, edge.peak, edge.sound), (0.8, false, 0.5, Sound::Playing));
        assert_eq!((apps[1].muted, apps[1].sound), (true, Sound::Idle));
    }

    /// Builds a fake tree: (pid, parent, exe path).
    fn fake(procs: &[(u32, u32, &str)]) -> (HashMap<u32, u32>, HashMap<u32, String>, HashMap<u32, String>) {
        let parent = procs.iter().map(|p| (p.0, p.1)).collect();
        let names = procs.iter().map(|p| (p.0, p.2.rsplit('\\').next().unwrap().to_owned())).collect();
        let exes = procs.iter().map(|p| (p.0, p.2.to_owned())).collect();
        (parent, names, exes)
    }

    fn owner(procs: &[(u32, u32, &str)], windows: &[&str], pid: u32) -> String {
        let (parent, names, exes) = fake(procs);
        let known: HashSet<String> = windows.iter().map(|w| w.to_string()).collect();
        let mut exe_of = |p: u32| exes.get(&p).cloned().unwrap_or_default();
        resolve(pid, &parent, &names, &mut exe_of, &known)
    }

    #[test]
    fn sobe_a_arvore_ate_o_programa_com_janela() {
        let procs = [(1, 0, "c:\\w\\explorer.exe"), (10, 1, "c:\\e\\edge.exe"), (11, 10, "c:\\e\\edge.exe"), (12, 11, "c:\\e\\gpu.exe"), (40, 1, "c:\\x\\solto.exe")];
        let win = ["c:\\e\\edge.exe", "c:\\w\\explorer.exe"];
        assert_eq!(owner(&procs, &win, 12), "c:\\e\\edge.exe");
        assert_eq!(owner(&procs, &win, 11), "c:\\e\\edge.exe");
        // Explorer child without its own window doesn't become "Explorer"
        assert_eq!(owner(&procs, &win, 40), "c:\\x\\solto.exe");
        assert_eq!(owner(&procs, &win, 99), "pid:99");
    }

    #[test]
    fn apps_com_helper_agrupam_no_dono_da_janela() {
        // Helpers (Brave, Discord, Obsidian, Telegram) play in children of the window exe.
        let procs = [
            (1, 0, "c:\\w\\explorer.exe"),
            (10, 1, "c:\\b\\brave.exe"), (11, 10, "c:\\b\\brave.exe"),
            (20, 1, "c:\\d\\discord.exe"), (21, 20, "c:\\d\\discord.exe"),
            (30, 1, "c:\\o\\obsidian.exe"), (31, 30, "c:\\o\\obsidian.exe"),
            (40, 1, "c:\\t\\telegram.exe"), (41, 40, "c:\\t\\telegram.exe"),
            // Steam: audio is from steam.exe, window is from steamwebhelper (child).
            (50, 1, "c:\\s\\steam.exe"), (51, 50, "c:\\s\\steamwebhelper.exe"), (52, 51, "c:\\s\\steamwebhelper.exe"),
        ];
        let win = ["c:\\b\\brave.exe", "c:\\d\\discord.exe", "c:\\o\\obsidian.exe", "c:\\t\\telegram.exe", "c:\\s\\steamwebhelper.exe", "c:\\w\\explorer.exe"];
        for (pid, want) in [(11, win[0]), (21, win[1]), (31, win[2]), (41, win[3]), (50, win[4]), (52, win[4])] {
            assert_eq!(owner(&procs, &win, pid), want, "pid {pid}");
        }
    }

    #[test]
    fn lista_ordena_e_inclui_programas_sem_audio() {
        let app = |id: &str, peak, sound| AudioApp { id: id.into(), name: id.into(), volume: 1.0, muted: false, peak, sound };
        let groups = vec![app("b", 0.0, Sound::Idle), app("a", 0.3, Sound::Playing)];
        let programs = vec![("A".to_string(), "Janela do A".to_string()), ("c".to_string(), "Janela C".to_string())];
        let seen: Vec<_> = rows(groups, &programs).into_iter().map(|a| (a.name, a.sound)).collect();
        assert_eq!(seen, [("Janela do A".to_string(), Sound::Playing), ("b".to_string(), Sound::Idle), ("Janela C".to_string(), Sound::None)]);
    }

    #[test]
    fn separa_os_que_nao_tocam() {
        let app = |id: &str, sound| AudioApp { id: id.into(), name: id.into(), volume: 1.0, muted: false, peak: 0.0, sound };
        let sorted = rows(vec![app("c", Sound::Idle), app("a", Sound::Playing), app("b", Sound::Playing)], &[("d".to_string(), "d".to_string())]);
        let (playing, quiet) = split_quiet(sorted);
        assert_eq!(playing.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(quiet.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["c", "d"]);
    }
}
