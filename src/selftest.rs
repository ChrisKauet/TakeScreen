//! Autoteste sem cliques, para conferir a troca de janelas no datashow de verdade:
//!
//! `TakeScreen.exe --selftest "<trecho do título>" [--fit borderless|maximize]`
//! `TakeScreen.exe --seq "A|B|C" [--fit ...]` (troca no ar A -> B -> C, como na interface)
//! `TakeScreen.exe --inspect "<trecho do título>"` (só descreve, não move nada; "*" para todas)
//!
//! Imprime uma linha por verificação e sai com código diferente de zero se alguma falhar.

use std::time::{Duration, Instant};

use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

use crate::win::{self, Fit, Hwnd, Monitor, Rect, Saved, WinState, WindowInfo};

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
    Some(selftest(&seq, fit))
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

/// Espera sem travar a cortina (ela é deste processo e precisa se pintar).
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

fn selftest(seq: &[String], fit: Option<Fit>) -> i32 {
    win::dpi_aware();
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
    for part in seq {
        let Some(w) = find(part) else {
            println!("FALHA \"{part}\": janela não encontrada");
            fails += 1;
            continue;
        };
        let before = win::state(w.hwnd);
        let fit = fit.unwrap_or(if w.uwp { Fit::Maximize } else { Fit::Borderless });
        println!("TAKE \"{}\" fit={fit:?} antes: {}", w.title, fmt(&before));
        let cover = on_air.as_ref().map_or(curtain, |(s, _, _)| s.hwnd);
        let t0 = Instant::now();
        match win::take(w.hwnd, cover, curtain, ds.rect, fit) {
            Err(e) => {
                println!("ERRO take \"{}\" ({} ms): {e}", w.title, t0.elapsed().as_millis());
                wait(800);
                let now = win::state(w.hwnd);
                // Mesmo com erro, a janela não pode sumir.
                let ok = !before.shown() || (now.shown() && now.rect.is_some_and(|r| on_some(&r, &mons)));
                println!("{} depois do erro a janela continua à vista: {}", tag(ok), fmt(&now));
                fails += !ok as i32 + 1;
            }
            Ok(saved) => {
                println!("take levou {} ms", t0.elapsed().as_millis());
                if let Some((old, title, b)) = on_air.replace((saved, w.title.clone(), before)) {
                    fails += give_back(&old, &title, &b, &ds, &home, &mons);
                }
                wait(1500);
                fails += check_air(w.hwnd, &w.title, &ds, &mons);
            }
        }
    }
    if let Some((old, title, b)) = on_air {
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
    // Maximizada só alcança a área útil; a barra de tarefas do datashow, se houver, fica à vista.
    let covers_work = s.rect.is_some_and(|r| win::covers_within(&r, &ds.work, TOL));
    let ok = s.shown() && (covers || (s.zoomed && covers_work));
    let note = if ok && !covers { " AVISO barra de tarefas do datashow visivel" } else { "" };
    println!("{} no ar \"{title}\": cobre={covers} vaza_px2={spill} {}{note}", tag(ok), fmt(&s));
    !ok as i32
}

/// O que a interface faz com a janela que sai do ar: `restore` e, se ficou no datashow, manda para casa.
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
    !ok as i32
}
