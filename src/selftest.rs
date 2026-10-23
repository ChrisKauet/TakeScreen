//! Click-free self-test that verifies window switching on the real projector:
//!
//! `TakeScreen.exe --selftest "<title fragment>" [--fit borderless|maximize]`
//! `TakeScreen.exe --seq "A|B|C" [--fit ...]` (switches the live window A -> B -> C, as the interface does)
//! `TakeScreen.exe --inspect "<title fragment>"` (only describes, moves nothing; "*" for all windows)
//! `--press f` (or `f11`): with the last window live, presses the key twice through `press_key_on`.
//! If the title contains "FS-OFF" (test page), verifies that it becomes "FS-ON" and then reverts.
//!
//! Prints one line per check and exits with a nonzero code if any check fails.

use std::time::{Duration, Instant};

use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

use crate::win::{self, Fit, Hwnd, Monitor, Rect, Saved, WinState, WindowInfo, VIRTUAL_KEY};

const TOL: i32 = 2;

pub fn run_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let val = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if let Some(t) = val("--inspect") {
        return Some(inspect(&t));
    }
    let seq: Vec<String> = match (val("--seq"), val("--selftest")) {
        (Some(s), _) => s.split('|').map(str::to_string).collect(),
        (None, Some(t)) => vec![t],
        _ => return None,
    };
    let fit = match val("--fit").as_deref() {
        Some("maximize") => Some(Fit::Maximize),
        Some("borderless") => Some(Fit::Borderless),
        _ => None,
    };
    let press = val("--press").map(|k| key_of(&k));
    Some(selftest(&seq, fit, press))
}

fn key_of(k: &str) -> VIRTUAL_KEY {
    match k.to_lowercase().as_str() {
        "f" => win::VK_VIDEO_FULLSCREEN,
        "f11" => win::VK_FULLSCREEN,
        k => VIRTUAL_KEY(k.to_uppercase().chars().next().unwrap_or('F') as u16),
    }
}

fn find(part: &str) -> Option<WindowInfo> {
    let part = part.to_lowercase();
    win::list_windows().into_iter().find(|w| w.title.to_lowercase().contains(&part))
}

fn inspect(part: &str) -> i32 {
    win::dpi_aware();
    let part = part.to_lowercase();
    let mut n = 0;
    for w in win::list_windows() {
        if part == "*" || w.title.to_lowercase().contains(&part) {
            println!("\"{}\" uwp={} {}", w.title, w.uwp, win::describe(w.hwnd));
            n += 1;
        }
    }
    if n == 0 {
        println!("FALHA nenhuma janela com \"{part}\"");
        return 1;
    }
    0
}

/// Waits without blocking the curtain (it belongs to this process and must paint itself).
fn wait(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn fmt(s: &WinState) -> String {
    let r = s.rect.map_or("?".into(), |r| format!("{},{},{},{}", r.left, r.top, r.right, r.bottom));
    format!("rect={r} minimizada={} maximizada={} visivel={} cloaked={}", s.iconic, s.zoomed, s.visible, s.cloaked)
}

fn selftest(seq: &[String], fit: Option<Fit>, press: Option<VIRTUAL_KEY>) -> i32 {
    win::dpi_aware();
    // The window that currently holds the keyboard plays the role of the panel: focus must return to it.
    let panel = win::foreground();
    let mons = win::list_monitors();
    let (Some(ds), Some(home)) = (mons.iter().find(|m| !m.primary).cloned(), mons.iter().find(|m| m.primary).cloned()) else {
        println!("FALHA o autoteste precisa de 2 monitores");
        return 2;
    };
    println!("datashow={} casa={}", ds.label(), home.label());
    let Some(curtain) = win::create_curtain(ds.rect) else {
        println!("FALHA não consegui criar a cortina");
        return 2;
    };
    let mut fails = 0;
    let mut on_air: Option<(Saved, String, WinState)> = None;
    let mut toggled = Vec::new();
    let mut air_exe = String::new();
    for part in seq {
        // A "fit:maximize|borderless" step switches the live window in place, as the panel selector does.
        if let Some(mode) = part.strip_prefix("fit:") {
            if let Some((s, title, _)) = on_air.as_ref() {
                let chosen = if mode == "maximize" { Fit::Maximize } else { Fit::Borderless };
                let (applied, key) = win::effective_fit(chosen, &win::window_class(s.hwnd), &air_exe);
                win::undo_keys(s.hwnd, std::mem::take(&mut toggled), panel);
                println!("FIT {chosen:?} (aplicado {applied:?}, tecla {key:?}) em \"{title}\"");
                match win::refit(s, curtain, ds.rect, applied) {
                    Ok(()) => {
                        press_own_key(s.hwnd, key, panel, &mut toggled);
                        wait(1500);
                        fails += check_air(s.hwnd, title, &ds, &mons);
                    }
                    Err(e) => {
                        println!("FALHA refit: {e}");
                        fails += 1;
                        on_air = None;
                    }
                }
            }
            continue;
        }
        // A "wait:<ms>" step keeps the live window on air, for example to capture the projector.
        if let Some(ms) = part.strip_prefix("wait:") {
            wait(ms.parse().unwrap_or(1000));
            continue;
        }
        // A "key:<k>" step presses the key on the live window, as the panel buttons do.
        if let Some(k) = part.strip_prefix("key:") {
            if let Some((s, _, _)) = on_air.as_ref() {
                let sent = win::press_key_on(s.hwnd, key_of(k), panel);
                if sent.is_ok() {
                    win::toggle_key(&mut toggled, key_of(k));
                }
                wait(1500);
                println!("{} tecla \"{k}\": enviada={sent:?} titulo=\"{}\"", tag(sent.is_ok()), win::title(s.hwnd));
                fails += sent.is_err() as i32;
            }
            continue;
        }
        let Some(w) = find(part) else {
            println!("FALHA \"{part}\": janela não encontrada");
            fails += 1;
            continue;
        };
        let before = win::state(w.hwnd);
        let chosen = fit.unwrap_or(if w.uwp { Fit::Maximize } else { Fit::Borderless });
        let (fit, key) = win::effective_fit(chosen, &win::window_class(w.hwnd), &w.exe);
        println!("TAKE \"{}\" fit={chosen:?} (aplicado {fit:?}, tecla {key:?}) antes: {}", w.title, fmt(&before));
        // Same as the panel: undo the full screen keys before the live window leaves the air.
        if let Some((s, _, _)) = on_air.as_ref() {
            win::undo_keys(s.hwnd, std::mem::take(&mut toggled), panel);
        }
        let cover = on_air.as_ref().map_or(curtain, |(s, _, _)| s.hwnd);
        let t0 = Instant::now();
        match win::take(w.hwnd, cover, curtain, ds.rect, fit) {
            Err(e) => {
                println!("ERRO take \"{}\" ({} ms): {e}", w.title, t0.elapsed().as_millis());
                wait(800);
                let now = win::state(w.hwnd);
                // Even on error, the window must not disappear.
                let ok = !before.shown() || (now.shown() && now.rect.is_some_and(|r| on_some(&r, &mons)));
                println!("{} depois do erro a janela continua à vista: {}", tag(ok), fmt(&now));
                fails += !ok as i32 + 1;
            }
            Ok(saved) => {
                println!("take levou {} ms", t0.elapsed().as_millis());
                if let Some((old, title, b)) = on_air.replace((saved, w.title.clone(), before)) {
                    fails += give_back(&old, &title, &b, &ds, &home, &mons);
                }
                air_exe = w.exe.clone();
                press_own_key(w.hwnd, key, panel, &mut toggled);
                wait(1500);
                fails += check_air(w.hwnd, &w.title, &ds, &mons);
            }
        }
    }
    if let (Some(vk), Some((s, _, _))) = (press, on_air.as_ref()) {
        fails += press_test(s.hwnd, vk, panel);
    }
    if let Some((old, title, b)) = on_air {
        win::undo_keys(old.hwnd, toggled, panel);
        fails += give_back(&old, &title, &b, &ds, &home, &mons);
    }
    win::destroy(curtain);
    println!("RESULTADO {} ({fails} falha(s))", if fails == 0 { "OK" } else { "FALHA" });
    (fails > 0) as i32
}

fn tag(ok: bool) -> &'static str {
    if ok {
        "OK   "
    } else {
        "FALHA"
    }
}

fn on_some(r: &Rect, mons: &[Monitor]) -> bool {
    mons.iter().any(|m| r.center_in(&m.rect))
}

fn check_air(h: Hwnd, title: &str, ds: &Monitor, mons: &[Monitor]) -> i32 {
    let s = win::state(h);
    let covers = s.rect.is_some_and(|r| win::covers_within(&r, &ds.rect, TOL));
    let spill: i32 = s.rect.map_or(0, |r| {
        mons.iter().filter(|m| m.device != ds.device).map(|m| r.overlap_area(&m.rect)).sum()
    });
    // A maximized window only reaches the work area; the projector taskbar, if any, remains visible.
    let covers_work = s.rect.is_some_and(|r| win::covers_within(&r, &ds.work, TOL));
    let ok = s.shown() && (covers || (s.zoomed && covers_work));
    let note = if ok && !covers { " AVISO barra de tarefas do datashow visivel" } else { "" };
    println!("{} no ar \"{title}\": cobre={covers} vaza_px2={spill} {}{note}", tag(ok), fmt(&s));
    !ok as i32
}

/// What the interface does with the window going off air: `restore` and, if it stayed on the projector, send it home.
fn give_back(old: &Saved, title: &str, before: &WinState, ds: &Monitor, home: &Monitor, mons: &[Monitor]) -> i32 {
    win::restore(old);
    let right_after = win::state(old.hwnd);
    if right_after.rect.is_some_and(|r| ds.holds(&r)) {
        println!("      \"{title}\" ainda no datashow logo após o restore, mandando para casa: {}", fmt(&right_after));
        win::send_home(old.hwnd, home.work);
    }
    wait(800);
    let s = win::state(old.hwnd);
    let same_rect = match (s.rect, before.rect) {
        (Some(a), Some(b)) => (a.left - b.left).abs() <= TOL && (a.top - b.top).abs() <= TOL && (a.right - b.right).abs() <= TOL && (a.bottom - b.bottom).abs() <= TOL,
        _ => false,
    };
    let off_ds = !s.rect.is_some_and(|r| ds.holds(&r));
    let seen = s.shown() == before.shown() && (!s.shown() || s.rect.is_some_and(|r| on_some(&r, mons)));
    let same_frame = s.style == before.style && s.exstyle == before.exstyle;
    let same_state = s.iconic == before.iconic && s.zoomed == before.zoomed;
    let ok = old.hwnd.exists() && off_ds && seen && same_rect && same_frame && same_state;
    println!(
        "{} voltou \"{title}\": fora_do_datashow={off_ds} a_vista={seen} mesmo_tamanho={same_rect} mesma_moldura={same_frame} mesmo_estado={same_state} {}",
        tag(ok),
        fmt(&s)
    );
    if !same_frame {
        println!("      estilo agora={:#x}/{:#x} antes={:#x}/{:#x}", s.style, s.exstyle, before.style, before.exstyle);
    }
    !ok as i32
}

/// Presses the key twice on the live window: toggles full screen on and off on the test page.
/// Presses the program's own full screen key, as the panel does right after a take.
fn press_own_key(h: Hwnd, key: Option<VIRTUAL_KEY>, panel: Hwnd, toggled: &mut Vec<VIRTUAL_KEY>) {
    let Some(k) = key else { return };
    let sent = win::press_key_on(h, k, panel);
    println!("      tecla de tela cheia do programa: {sent:?}");
    if sent.is_ok() {
        win::toggle_key(toggled, k);
    }
}

fn press_test(h: Hwnd, vk: VIRTUAL_KEY, panel: Hwnd) -> i32 {
    let marked = win::title(h).contains("FS-OFF");
    let mut fails = 0;
    for expect in ["FS-ON", "FS-OFF"] {
        let t0 = Instant::now();
        let sent = win::press_key_on(h, vk, panel);
        let ms = t0.elapsed().as_millis();
        wait(800);
        let title = win::title(h);
        let back = win::foreground() == panel;
        let ok = sent.is_ok() && back && (!marked || title.contains(expect));
        let want = if marked { expect } else { "-" };
        println!("{} tecla ({ms} ms): enviada={sent:?} titulo=\"{title}\" esperado={want} foco_voltou={back}", tag(ok));
        fails += !ok as i32;
    }
    fails
}
