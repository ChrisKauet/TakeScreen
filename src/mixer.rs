use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

#[derive(Clone, Debug, PartialEq)]
pub struct AudioApp {
    pub id: String,
    pub pid: u32,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
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
}

impl Hub {
    pub fn start(on_change: impl Fn() + Send + 'static) -> Hub {
        let snapshot = Arc::new(Mutex::new(Snapshot { apps: Vec::new(), master: (1.0, false) }));
        let (commands, command_rx) = mpsc::channel();
        let shared = Arc::clone(&snapshot);
        thread::spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            loop {
                let device = default_device();
                let commands: Vec<_> = command_rx.try_iter().collect();
                if let Some(device) = device.as_ref() {
                    apply_commands(device, &commands);
                }
                let next = device.as_ref().and_then(read_snapshot).unwrap_or_else(|| Snapshot { apps: Vec::new(), master: (0.0, false) });
                if let Ok(mut current) = shared.lock() {
                    *current = next;
                }
                on_change();
                thread::sleep(Duration::from_millis(250));
            }
        });
        Hub { snapshot, commands }
    }

    pub fn apps(&self) -> Vec<AudioApp> {
        self.snapshot.lock().ok().map(|s| s.apps.clone()).unwrap_or_default()
    }

    pub fn master(&self) -> (f32, bool) {
        self.snapshot.lock().ok().map(|s| s.master).unwrap_or((0.0, false))
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

fn default_device() -> Option<IMMDevice> {
    unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?.GetDefaultAudioEndpoint(eRender, eConsole).ok() }
}

fn read_snapshot(device: &IMMDevice) -> Option<Snapshot> {
    unsafe {
        let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).ok()?;
        let apps = read_apps(&manager);
        let master = (endpoint.GetMasterVolumeLevelScalar().unwrap_or(0.0).clamp(0.0, 1.0), endpoint.GetMute().unwrap_or_default().as_bool());
        Some(Snapshot { apps, master })
    }
}

fn read_apps(manager: &IAudioSessionManager2) -> Vec<AudioApp> {
    let Ok(enumerator) = (unsafe { manager.GetSessionEnumerator() }) else { return Vec::new() };
    let Ok(count) = (unsafe { enumerator.GetCount() }) else { return Vec::new() };
    let mut apps = Vec::new();
    for index in 0..count {
        let Ok(control) = (unsafe { enumerator.GetSession(index) }) else { continue };
        let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
        let Ok(volume) = control.cast::<windows::Win32::Media::Audio::ISimpleAudioVolume>() else { continue };
        let level = unsafe { volume.GetMasterVolume() }.unwrap_or(0.0).clamp(0.0, 1.0);
        let state = unsafe { control.GetState() }.unwrap_or_default();
        if state != AudioSessionStateActive && level <= 0.0 { continue; }
        let pid = unsafe { control2.GetProcessId() }.unwrap_or(0);
        let system = pid == 0 || unsafe { control2.IsSystemSoundsSession() } == windows::core::HRESULT(0);
        let name = if system { "Sons do sistema".to_owned() } else { process_name(pid) };
        let Ok(id) = (unsafe { control2.GetSessionInstanceIdentifier() }) else { continue };
        let id = pwstr_to_string(id);
        let peak = control.cast::<IAudioMeterInformation>().ok().and_then(|meter| unsafe { meter.GetPeakValue().ok() }).unwrap_or(0.0).clamp(0.0, 1.0);
        apps.push(AudioApp {
            id,
            pid,
            name,
            volume: level,
            muted: unsafe { volume.GetMute() }.map(|value| value.as_bool()).unwrap_or(false),
            peak,
        });
    }
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps
}

fn apply_commands(device: &IMMDevice, commands: &[Command]) {
    unsafe {
        let Ok(endpoint): Result<IAudioEndpointVolume, _> = device.Activate(CLSCTX_ALL, None) else { return };
        let Ok(manager): Result<IAudioSessionManager2, _> = device.Activate(CLSCTX_ALL, None) else { return };
        for command in commands {
            match command {
                Command::SetMaster(level) => { let _ = endpoint.SetMasterVolumeLevelScalar(*level, std::ptr::null()); }
                Command::SetMasterMute(muted) => { let _ = endpoint.SetMute(*muted, std::ptr::null()); }
                Command::SetVolume(id, level) => {
                    let Ok(enumerator) = manager.GetSessionEnumerator() else { continue };
                    let Ok(count) = enumerator.GetCount() else { continue };
                    for index in 0..count {
                        let Ok(control) = enumerator.GetSession(index) else { continue };
                        let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                        let Ok(session_id) = control2.GetSessionInstanceIdentifier() else { continue };
                        let session_id = pwstr_to_string(session_id);
                        if session_id != *id { continue; }
                        let Ok(volume) = control.cast::<windows::Win32::Media::Audio::ISimpleAudioVolume>() else { break };
                        let _ = volume.SetMasterVolume(*level, std::ptr::null());
                        break;
                    }
                }
                Command::SetMute(id, muted) => {
                    let Ok(enumerator) = manager.GetSessionEnumerator() else { continue };
                    let Ok(count) = enumerator.GetCount() else { continue };
                    for index in 0..count {
                        let Ok(control) = enumerator.GetSession(index) else { continue };
                        let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                        let Ok(session_id) = control2.GetSessionInstanceIdentifier() else { continue };
                        let session_id = pwstr_to_string(session_id);
                        if session_id != *id { continue; }
                        let Ok(volume) = control.cast::<windows::Win32::Media::Audio::ISimpleAudioVolume>() else { break };
                        let _ = volume.SetMute(*muted, std::ptr::null());
                        break;
                    }
                }
            }
        }
    }
}

fn pwstr_to_string(value: windows::core::PWSTR) -> String {
    let text = unsafe { value.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(value.0.cast())); }
    text
}

fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return "Desconhecido".into() };
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, windows::core::PWSTR(buffer.as_mut_ptr()), &mut length);
        let _ = CloseHandle(handle);
        let path = result.ok().map(|_| String::from_utf16_lossy(&buffer[..length as usize])).unwrap_or_default();
        let name = path.rsplit(['\\', '/']).next().unwrap_or("Desconhecido").trim_end_matches(".exe");
        capitalize(name)
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
}
