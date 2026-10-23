//! TakeScreen: operator panel to choose what appears on the external screen (second display).
//!
//! Works like OBS Studio Mode: PREVIEW (what you chose) and ON AIR (what the audience sees)
//! on top; open programs below, each with an icon and a live preview. One click on SHOW,
//! or Enter, swaps what is on air.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("O TakeScreen só funciona no Windows. Compile com --target x86_64-pc-windows-gnu ou no próprio Windows.");

mod clip;
mod queue;
mod win;
mod media;
mod mixer;
mod selftest;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};

use win::{Fit, Hwnd, Monitor, Saved, Thumbnail, WindowInfo};

const REFRESH: Duration = Duration::from_millis(1000);
/// Wake-up interval while the panel is minimized or hidden (the window list keeps being read).
const HIDDEN_REFRESH: Duration = Duration::from_millis(3000);
const PREVIEW_COLOR: egui::Color32 = egui::Color32::from_rgb(46, 160, 67);
const AIR_COLOR: egui::Color32 = egui::Color32::from_rgb(218, 54, 51);
const CARD_W: f32 = 220.0;
const CARD_HEADER: f32 = 30.0;
const QUEUE_CARD_H: f32 = 44.0;
const NEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(214, 150, 24);
const NO_DATASHOW: &str = "O datashow não está conectado (ou é a mesma tela do painel). Nada foi coberto.";

/// What is saved between sessions.
#[derive(Serialize, Deserialize, Default)]
struct Config {
    /// Name of the device chosen as the external screen (e.g. "EPSON PJ").
    datashow_name: Option<String>,
    datashow_device: Option<String>,
    /// External screen protection starts on; this only becomes true if the operator turns it off.
    #[serde(default)]
    protection_off: bool,
    /// Image shown on the external screen when nothing is on air (`None` = black screen).
    #[serde(default)]
    standby_image: Option<String>,
    #[serde(default)]
    queue: queue::Queue,
    /// "Next" in the queue goes straight to SHOW, without stopping at PREVIEW.
    #[serde(default)]
    queue_take: bool,
    #[serde(default)]
    side_closed: bool,
    /// The mixer's "Sem áudio" section is expanded (closed by default).
    #[serde(default)]
    quiet_open: bool,
    /// The operator prefers Maximize over the borderless full screen fit.
    #[serde(default)]
    fit_maximize: bool,
}

/// Master volume ID in mixer controls (audio sessions have Windows IDs).
const MASTER_ID: &str = "";

/// Largest standby image kept in memory: 4K external screen.
const STANDBY_MAX: (u32, u32) = (3840, 2160);

/// Where each Windows thumbnail should be drawn in this frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Preview,
    Air,
    Card(Hwnd),
}

struct App {
    cfg: Config,
    me: Option<Hwnd>,
    curtain: Option<Hwnd>,
    monitors: Vec<Monitor>,
    datashow: usize,
    windows: Vec<WindowInfo>,
    /// Windows in the order they appeared: cards don't swap when focus changes.
    order: Vec<Hwnd>,
    icons: HashMap<Hwnd, Option<egui::TextureHandle>>,
    last_refresh: Instant,
    preview: Option<Hwnd>,
    fit: Fit,
    on_air: Option<Saved>,
    thumbs: HashMap<Slot, Thumbnail>,
    /// Areas (in physical pixels) and crop of each thumbnail, built during screen drawing.
    placed: Vec<(Slot, win::Rect, win::Rect)>,
    status: String,
    /// Title and program of the card under the mouse, shown in the status bar.
    hovered_card: Option<String>,
    /// A tooltip is open: thumbnails, which stay on top of everything, hide.
    tip_open: bool,
    media: media::Hub,
    /// Media source chosen in the panel (kept for the session only).
    media_sel: Option<media::SourceKey>,
    mixer: mixer::Hub,
    /// Mixer slider the operator is dragging: the value read from Windows doesn't overwrite it.
    vol_edit: Option<(String, f32, Instant)>,
    url_input: String,
    /// Ctrl+V was down on the previous frame (see `clip::paste_pressed`).
    paste_was_down: bool,
    /// Queue card selected with a click; Del removes it.
    queue_sel: Option<usize>,
    /// Text typed in the search box above the window cards.
    filter: String,
    /// Full screen keys pressed an odd number of times on the ON AIR window.
    toggled: Vec<win::VIRTUAL_KEY>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        win::guard_install(move || ctx.request_repaint());
        let cfg: Config = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let fit = if cfg.fit_maximize { Fit::Maximize } else { Fit::Borderless };
        let mut app = App {
            cfg,
            me: None,
            curtain: None,
            monitors: Vec::new(),
            datashow: 0,
            windows: Vec::new(),
            order: Vec::new(),
            icons: HashMap::new(),
            last_refresh: Instant::now() - REFRESH,
            preview: None,
            fit,
            on_air: None,
            thumbs: HashMap::new(),
            placed: Vec::new(),
            status: String::new(),
            hovered_card: None,
            tip_open: false,
            media: media::Hub::start({
                let ctx = cc.egui_ctx.clone();
                move || ctx.request_repaint()
            }),
            media_sel: None,
            mixer: mixer::Hub::start({
                let ctx = cc.egui_ctx.clone();
                move || ctx.request_repaint()
            }),
            vol_edit: None,
            url_input: String::new(),
            paste_was_down: false,
            queue_sel: None,
            filter: String::new(),
            toggled: Vec::new(),
        };
        if let Some(title) = win::recover_previous() {
            app.status = format!("\"{title}\" tinha ficado no datashow quando o TakeScreen fechou e foi devolvida para a tela 1.");
        }
        app.refresh_monitors();
        app.apply_standby();
        app
    }

    /// Loads the chosen standby image; if it fails, warns and leaves black screen.
    fn apply_standby(&mut self) {
        let Some(path) = self.cfg.standby_image.clone() else {
            win::set_standby(self.curtain, None);
            return;
        };
        match load_standby(&path) {
            Ok(img) => win::set_standby(self.curtain, Some(img)),
            Err(e) => {
                win::set_standby(self.curtain, None);
                let aviso = format!("Não consegui abrir a imagem de espera ({e}). O datashow fica com a tela preta.");
                self.status = if self.status.is_empty() { aviso } else { format!("{} {aviso}", self.status) };
            }
        }
    }

    fn choose_standby(&mut self) {
        let Some(path) = win::pick_image_file(self.me) else { return };
        self.cfg.standby_image = Some(path);
        self.status.clear();
        self.apply_standby();
        if self.status.is_empty() {
            self.status = "Imagem de espera trocada.".into();
        } else {
            self.cfg.standby_image = None;
        }
    }

    fn black_standby(&mut self) {
        self.cfg.standby_image = None;
        self.apply_standby();
        self.status = "Imagem de espera: tela preta.".into();
    }

    /// The external screen is separate: there are 2 or more monitors and the panel isn't on it.
    fn datashow_separate(&self) -> bool {
        let Some(ds) = self.datashow_monitor() else { return false };
        self.monitors.len() >= 2 && !self.me.and_then(win::visible_rect).is_some_and(|r| ds.holds(&r))
    }

    fn datashow_monitor(&self) -> Option<&Monitor> {
        self.monitors.get(self.datashow)
    }

    fn on_air_hwnd(&self) -> Option<Hwnd> {
        self.on_air.map(|s| s.hwnd)
    }

    /// Re-finds the external screen by device name, even if monitor order changed.
    fn refresh_monitors(&mut self) {
        let before = self.datashow_monitor().map(|m| m.rect);
        self.monitors = win::list_monitors();
        let by_name = self.cfg.datashow_name.as_ref().and_then(|n| {
            let same: Vec<usize> = (0..self.monitors.len()).filter(|&i| &self.monitors[i].name == n).collect();
            match same.as_slice() {
                [i] => Some(*i),
                // Dois aparelhos iguais: desempata pelo nome GDI.
                _ => same.into_iter().find(|&i| Some(&self.monitors[i].device) == self.cfg.datashow_device.as_ref()),
            }
        });
        let by_device = || {
            let d = self.cfg.datashow_device.as_ref()?;
            self.monitors.iter().position(|m| &m.device == d)
        };
        let first_secondary = || self.monitors.iter().position(|m| !m.primary);
        self.datashow = by_name.or_else(by_device).or_else(first_secondary).unwrap_or(0);
        if self.datashow_monitor().map(|m| m.rect) != before {
            self.reposition_curtain();
        }
    }

    fn set_datashow(&mut self, i: usize) {
        if i == self.datashow {
            return;
        }
        self.clear();
        self.datashow = i;
        if let Some(m) = self.monitors.get(i).cloned() {
            self.cfg.datashow_name = Some(m.name.clone());
            self.cfg.datashow_device = Some(m.device.clone());
        }
        self.reposition_curtain();
        self.keep_me_off_datashow();
    }

    /// The monitor windows return to: the panel's, or the primary.
    fn home_monitor(&self) -> Option<&Monitor> {
        let ds = self.datashow_monitor()?;
        let mine = self.me.and_then(win::visible_rect);
        let others = || self.monitors.iter().filter(|m| m.device != ds.device);
        others()
            .find(|m| mine.is_some_and(|r| m.holds(&r)))
            .or_else(|| others().find(|m| m.primary))
            .or_else(|| others().next())
    }

    /// Tells the guard which is the external screen and which windows can be there.
    fn sync_guard(&self, entering: Option<Hwnd>) {
        let (Some(ds), Some(home)) = (self.datashow_monitor(), self.home_monitor()) else {
            win::guard_configure(false, win::Rect::default(), win::Rect::default(), &[], None);
            return;
        };
        let allowed: Vec<Hwnd> = self.on_air_hwnd().into_iter().chain(entering).collect();
        win::guard_configure(!self.cfg.protection_off, ds.rect, home.work, &allowed, self.curtain);
    }

    /// If the window that left ON AIR was opened on the external screen, it doesn't go back there.
    fn send_off_datashow(&self, hwnd: Hwnd) {
        let (Some(ds), Some(home)) = (self.datashow_monitor(), self.home_monitor()) else { return };
        if win::visible_rect(hwnd).is_some_and(|r| ds.holds(&r)) {
            win::send_home(hwnd, home.work);
        }
    }

    fn reposition_curtain(&mut self) {
        let Some(m) = self.datashow_monitor().cloned() else { return };
        match self.curtain {
            Some(c) => win::place_curtain(c, m.rect, false),
            None => self.curtain = win::create_curtain(m.rect),
        }
    }

    /// The panel must never appear on the external screen.
    fn keep_me_off_datashow(&self) {
        let (Some(me), Some(ds)) = (self.me, self.datashow_monitor()) else { return };
        let Some(r) = win::visible_rect(me) else { return };
        if ds.holds(&r) {
            if let Some(other) = self.monitors.iter().find(|m| m.device != ds.device) {
                win::move_window(me, other.rect.left + 40, other.rect.top + 40);
            }
        }
    }

    fn refresh_windows(&mut self, ctx: &egui::Context) {
        self.windows = win::list_windows();
        let alive: Vec<Hwnd> = self.windows.iter().map(|w| w.hwnd).collect();
        track_order(&mut self.order, &alive);
        let order = &self.order;
        self.windows.sort_by_key(|w| order.iter().position(|h| *h == w.hwnd));
        if let Some(s) = self.on_air {
            if !s.hwnd.exists() {
                // The ON AIR window was closed: the black curtain is already right behind it.
                self.on_air = None;
                self.toggled.clear();
                win::remember_on_air(None);
                self.status = "A janela que estava no ar foi fechada. O datashow está preto.".into();
            }
        }
        if self.preview.is_some_and(|p| !p.exists()) {
            self.preview = None;
        }

        self.icons.retain(|h, _| alive.contains(h));
        for w in &self.windows {
            self.icons.entry(w.hwnd).or_insert_with(|| {
                win::window_icon(w).map(|i| {
                    let img = egui::ColorImage::from_rgba_unmultiplied(i.size, &i.rgba);
                    ctx.load_texture(format!("icone-{}", w.hwnd.0), img, egui::TextureOptions::LINEAR)
                })
            });
        }
        self.last_refresh = Instant::now();
    }

    fn select(&mut self, hwnd: Hwnd) {
        self.preview = Some(hwnd);
        let uwp = self.windows.iter().any(|w| w.hwnd == hwnd && w.uwp);
        self.fit = if uwp || self.cfg.fit_maximize { Fit::Maximize } else { Fit::Borderless };
    }

    fn title_of(&self, hwnd: Hwnd) -> String {
        self.windows.iter().find(|w| w.hwnd == hwnd).map(|w| w.title.clone()).unwrap_or_default()
    }

    /// SHOW: preview goes on air and the window that was on air returns to screen 1.
    fn take(&mut self) {
        let (Some(target), Some(curtain), Some(ds)) = (self.preview, self.curtain, self.datashow_monitor().cloned()) else {
            return;
        };
        if self.on_air_hwnd() == Some(target) {
            return;
        }
        if !self.datashow_separate() {
            self.status = NO_DATASHOW.into();
            return;
        }
        self.leave_fullscreen();
        self.sync_guard(Some(target));
        let cover = self.on_air_hwnd().unwrap_or(curtain);
        let (fit, key) = self.effective_fit(target, self.fit);
        if let (true, Some(me)) = (win::frameless_vlc(target), self.me) {
            // Before the take: leaving its own full screen moves VLC back to its old place.
            // Esc is not recorded in `toggled`: it only brings VLC out of its own full screen.
            let _ = win::press_key_on(target, win::undo_key(win::VK_VIDEO_FULLSCREEN), me);
        }
        let saved = match win::take(target, cover, curtain, ds.rect, fit) {
            Ok(s) => s,
            Err(e) => {
                self.status = e;
                self.sync_guard(None);
                return;
            }
        };
        win::remember_on_air(Some(&saved));
        if let Some(old) = self.on_air.replace(saved) {
            win::restore(&old);
            self.send_off_datashow(old.hwnd);
        }
        self.sync_guard(None);
        self.status = format!("No ar: {}", self.title_of(target));
        if let Some(k) = key {
            self.press_on_air(k, None);
        }
        if let Some(me) = self.me {
            win::focus(me);
        }
    }

    fn effective_fit(&self, hwnd: Hwnd, chosen: Fit) -> (Fit, Option<win::VIRTUAL_KEY>) {
        let exe = self.windows.iter().find(|w| w.hwnd == hwnd).map(|w| w.exe.as_str()).unwrap_or_default();
        win::effective_fit(chosen, &win::window_class(hwnd), exe)
    }

    /// The operator changed the fit: it is saved and, if a window is on air, applied to it now.
    fn change_fit(&mut self, fit: Fit) {
        if !self.preview.is_some_and(|p| self.windows.iter().any(|w| w.hwnd == p && w.uwp)) {
            self.cfg.fit_maximize = fit == Fit::Maximize;
        }
        let (Some(saved), Some(curtain), Some(ds)) = (self.on_air, self.curtain, self.datashow_monitor().cloned()) else {
            return;
        };
        self.leave_fullscreen();
        let (fit, key) = self.effective_fit(saved.hwnd, fit);
        match win::refit(&saved, curtain, ds.rect, fit) {
            Ok(()) => {
                self.status = format!("No ar: {}", self.title_of(saved.hwnd));
                if let Some(k) = key {
                    self.press_on_air(k, None);
                }
            }
            Err(e) => {
                // The window went back to screen 1: the black curtain is on the projector.
                self.on_air = None;
                win::remember_on_air(None);
                self.status = e;
                self.sync_guard(None);
            }
        }
        if let Some(me) = self.me {
            win::focus(me);
        }
    }

    /// LIMPAR: cortina preta na frente e a janela volta para a tela 1.
    fn clear(&mut self) {
        let separate = self.datashow_separate();
        // Before the curtain comes up, so the window does not jump in front of it.
        self.leave_fullscreen();
        // With a single monitor, the curtain would cover the panel itself.
        if let (true, Some(c), Some(ds)) = (separate, self.curtain, self.datashow_monitor()) {
            win::place_curtain(c, ds.rect, true);
        }
        if let Some(old) = self.on_air.take() {
            win::remember_on_air(None);
            win::restore(&old);
            self.send_off_datashow(old.hwnd);
            self.status = "Datashow limpo.".into();
        }
        if !separate {
            self.status = NO_DATASHOW.into();
        }
        self.sync_guard(None);
        if let Some(me) = self.me {
            win::focus(me);
        }
    }

    /// Presses `vk` on the ON AIR window; `hint` goes to status bar if key was sent.
    fn press_on_air(&mut self, vk: win::VIRTUAL_KEY, hint: Option<&str>) {
        if let (Some(s), Some(me)) = (self.on_air, self.me) {
            match win::press_key_on(s.hwnd, vk, me) {
                Ok(()) => {
                    win::toggle_key(&mut self.toggled, vk);
                    if let Some(h) = hint {
                        self.status = h.into();
                    }
                }
                Err(e) => self.status = e,
            }
        }
    }

    /// Next in queue: window goes to PREVIEW (and to air if marked); file and link open in default program.
    /// Undoes the full screen keys before the window leaves ON AIR: a browser left in full screen
    /// keeps re-entering it wherever it goes, covering screen 1 and hiding the next window.
    fn leave_fullscreen(&mut self) {
        let keys = std::mem::take(&mut self.toggled);
        if let (Some(s), Some(me)) = (self.on_air, self.me) {
            win::undo_keys(s.hwnd, keys, me);
        }
    }

    fn queue_next(&mut self) {
        let Some(item) = self.cfg.queue.next().cloned() else {
            self.status = if self.cfg.queue.items.is_empty() {
                "A fila está vazia.".into()
            } else {
                "Fim da fila. Clique em Reiniciar para começar de novo.".into()
            };
            return;
        };
        match &item.kind {
            queue::Kind::Window(_) => match queue::find_window(&item, &self.windows).map(|w| w.hwnd) {
                Some(h) => {
                    self.select(h);
                    self.status = format!("Na PRÉVIA: {}", item.label);
                    if self.cfg.queue_take {
                        self.take();
                    }
                }
                None => self.status = format!("Não achei a janela \"{}\". Ela está aberta?", item.label),
            },
            queue::Kind::File(p) | queue::Kind::Url(p) => {
                self.status = if win::open_path(p) {
                    format!("Abrindo {}. Quando a janela aparecer, escolha e aperte EXIBIR.", item.label)
                } else {
                    format!("Não consegui abrir {}.", item.label)
                };
            }
        }
    }

    fn queue_add(&mut self, label: String, kind: queue::Kind) {
        self.status = format!("Na fila: {label}");
        self.cfg.queue.add(queue::Item { label, kind });
    }

    fn open_vlc(&mut self) {
        if media::find_vlc().is_none() {
            self.status = "Não achei o VLC neste computador. Instale pelo site videolan.org.".into();
            return;
        }
        let Some(file) = win::pick_media_file(self.me) else { return };
        // Maximize keeps VLC as an ordinary window; only "Tela inteira" opens its own full screen on the projector.
        let fullscreen = (self.fit == Fit::Borderless).then_some(self.datashow as u32);
        self.status = match media::open_in_vlc(&file, fullscreen) {
            Ok(()) if fullscreen.is_none() => "VLC aberto em janela. Escolha a janela dele e aperte EXIBIR.".into(),
            Ok(()) => "VLC aberto sem os controles na tela. Escolha a janela dele e aperte EXIBIR.".into(),
            Err(e) => format!("Não consegui abrir o VLC: {e}"),
        };
    }

    // -----------------------------------------------------------------------
    // Live thumbnails

    fn source_of(&self, slot: Slot) -> Option<Hwnd> {
        match slot {
            Slot::Preview => self.preview.filter(|p| Some(*p) != self.on_air_hwnd()),
            Slot::Air => self.on_air_hwnd(),
            Slot::Card(h) => Some(h),
        }
    }

    /// Shows thumbnails in the places drawn this frame and hides the rest.
    fn apply_thumbnails(&mut self, popup_open: bool) {
        let Some(me) = self.me else { return };
        let placed = std::mem::take(&mut self.placed);
        let wanted: HashMap<Slot, Option<Hwnd>> = placed.iter().map(|p| (p.0, self.source_of(p.0))).collect();
        self.thumbs.retain(|slot, t| wanted.get(slot).is_some_and(|src| *src == Some(t.src)));
        for (slot, area, clip) in placed {
            let Some(src) = self.source_of(slot) else { continue };
            if !self.thumbs.contains_key(&slot) {
                match Thumbnail::new(me, src) {
                    Some(t) => self.thumbs.insert(slot, t),
                    None => continue,
                };
            }
            let t = &self.thumbs[&slot];
            // Windows draws thumbnails on top of everything, including open menus.
            if popup_open {
                t.hide();
            } else {
                t.show(area, clip);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Interface

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("TakeScreen").size(20.0).strong());
            ui.add_space(16.0);
            ui.label("Datashow:");
            let mut chosen = self.datashow;
            let current = self.datashow_monitor().map(|m| m.label()).unwrap_or_else(|| "nenhum monitor".into());
            egui::ComboBox::from_id_salt("datashow").width(300.0).selected_text(current).show_ui(ui, |ui| {
                for (i, m) in self.monitors.iter().enumerate() {
                    ui.selectable_value(&mut chosen, i, m.label());
                }
            });
            self.set_datashow(chosen);
            ui.add_space(16.0);
            let mut protect = !self.cfg.protection_off;
            let r = ui.checkbox(&mut protect, "Proteger datashow").on_hover_text(
                "Qualquer janela que abrir na tela 2 sem passar pelo TakeScreen volta na hora para a tela 1. \
                 Apresentações do PowerPoint vão para a PRÉVIA.",
            );
            self.tip_open |= r.hovered() || r.is_tooltip_open();
            self.cfg.protection_off = !protect;
            ui.add_space(16.0);
            ui.label("Espera:");
            if ui.button("Escolher imagem").clicked() {
                self.choose_standby();
            }
            if ui.add_enabled(self.cfg.standby_image.is_some(), egui::Button::new("Usar tela preta")).clicked() {
                self.black_standby();
            }
            ui.add_space(16.0);
            let mut side = !self.cfg.side_closed;
            ui.toggle_value(&mut side, "☰ Fila, mídia e mixer");
            self.cfg.side_closed = !side;
            if self.monitors.len() < 2 {
                ui.colored_label(AIR_COLOR, "Só um monitor conectado. Ligue o datashow.");
            }
        });
    }

    /// Draws a frame (PREVIEW or ON AIR) with the thumbnail inside.
    fn screen_box(&mut self, ui: &mut egui::Ui, slot: Slot, title: &str, color: egui::Color32, caption: &str, width: f32) {
        ui.label(egui::RichText::new(title).strong().color(color).size(16.0));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, width * 9.0 / 16.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 4.0, egui::Color32::BLACK);
        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(3.0, color), egui::StrokeKind::Outside);
        ui.add(egui::Label::new(egui::RichText::new(caption).weak()).truncate());
        // Crops to the central panel: if the window is narrow, the thumbnail doesn't invade the sidebar.
        let clip = to_px(ui.clip_rect(), ui.ctx());
        self.placed.push((slot, to_px(rect.shrink(1.0), ui.ctx()), clip));
    }

    fn studio(&mut self, ui: &mut egui::Ui, max_height: f32) {
        let gap = 140.0;
        let spacing = ui.spacing().item_spacing.x;
        // Two screens, the button column, the spaces between them and the frame borders.
        let by_width = (ui.available_width() - gap - 2.0 * spacing - 16.0) / 2.0;
        // Room below the frames for the caption, the button row and the small hint line.
        let by_height = (max_height - 96.0) * 16.0 / 9.0;
        let width = by_width.min(by_height).max(120.0);

        let air = self.on_air_hwnd();
        let preview_caption = match self.preview {
            Some(p) if air == Some(p) => "Essa janela já está no ar.".to_string(),
            Some(p) => self.title_of(p),
            None => "Clique em um programa abaixo.".into(),
        };
        let air_caption = match air {
            Some(h) => self.title_of(h),
            None => "Nada no ar (datashow preto ou livre).".into(),
        };

        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(width);
                self.screen_box(ui, Slot::Preview, "PRÉVIA", PREVIEW_COLOR, &preview_caption, width);
                ui.horizontal(|ui| {
                    ui.label("Ajuste:");
                    let before = self.fit;
                    ui.radio_value(&mut self.fit, Fit::Borderless, "Tela inteira sem bordas");
                    ui.radio_value(&mut self.fit, Fit::Maximize, "Maximizar");
                    if self.fit != before {
                        self.change_fit(self.fit);
                    }
                });
            });
            ui.allocate_ui_with_layout(egui::vec2(gap, width * 9.0 / 16.0 + 30.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.add_space(width * 9.0 / 32.0 - 10.0);
                let ready = self.preview.is_some() && self.preview != air;
                let button = egui::Button::new(egui::RichText::new("EXIBIR ▶").size(20.0).strong().color(egui::Color32::WHITE))
                    .fill(if ready { AIR_COLOR } else { egui::Color32::from_gray(60) })
                    .min_size(egui::vec2(gap - 16.0, 56.0));
                if ui.add_enabled(ready, button).clicked() {
                    self.take();
                }
                ui.label(egui::RichText::new("Enter").small().weak());
            });
            ui.vertical(|ui| {
                ui.set_width(width);
                self.screen_box(ui, Slot::Air, "NO AR", AIR_COLOR, &air_caption, width);
                ui.horizontal(|ui| {
                    if ui.add_enabled(air.is_some(), egui::Button::new("Tela cheia do programa (F11)")).clicked() {
                        self.press_on_air(win::VK_FULLSCREEN, None);
                    }
                    if ui.add_enabled(air.is_some(), egui::Button::new("Vídeo em tela cheia (F)")).clicked() {
                        self.press_on_air(win::VK_VIDEO_FULLSCREEN, Some("Se o vídeo não mudou, clique uma vez no vídeo na tela dele e tente de novo."));
                    }
                    if ui.button("Limpar datashow (Esc)").clicked() {
                        self.clear();
                    }
                });
                let hz = self.datashow_monitor().map(|m| m.hz).filter(|&hz| hz > 1);
                let tip = match hz {
                    Some(hz) => format!("Datashow a {hz} Hz. Vídeo do navegador roda mais leve em tela cheia do vídeo (F)."),
                    None => "Vídeo do navegador roda mais leve em tela cheia do vídeo (F).".into(),
                };
                ui.label(egui::RichText::new(tip).small().weak());
            });
        });
    }

    fn programs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Programas abertos");
            ui.label(egui::RichText::new("Clique para ver na prévia. Clique duplo exibe direto.").weak());
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Buscar...").desired_width(140.0));
            if !self.filter.is_empty() && ui.small_button("✖").on_hover_text("Limpar a busca").clicked() {
                self.filter.clear();
            }
        });
        ui.add_space(4.0);

        let mut clicked = None;
        let mut double = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let spacing = 10.0;
            ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
            let cols = (((ui.available_width() + spacing) / (CARD_W + spacing)).floor() as usize).max(1);
            let clip = to_px(ui.clip_rect(), ui.ctx());
            let needle = self.filter.trim().to_lowercase();
            let windows: Vec<WindowInfo> = self
                .windows
                .iter()
                .filter(|w| needle.is_empty() || w.title.to_lowercase().contains(&needle) || w.exe.to_lowercase().contains(&needle))
                .cloned()
                .collect();
            if windows.is_empty() && !needle.is_empty() {
                ui.label(egui::RichText::new("Nenhum programa com esse texto.").weak());
            }
            for row in windows.chunks(cols) {
                ui.horizontal(|ui| {
                    for w in row {
                        let r = self.card(ui, w, clip);
                        if r.hovered() {
                            self.hovered_card = Some(format!("{}  ({})", w.title, w.exe));
                        }
                        if r.clicked() {
                            clicked = Some(w.hwnd);
                        }
                        if r.double_clicked() {
                            double = Some(w.hwnd);
                        }
                    }
                });
            }
        });
        if let Some(h) = clicked.or(double) {
            self.select(h);
        }
        if double.is_some() {
            self.take();
        }
    }

    /// A card: icon and program name on top, live preview below.
    fn card(&mut self, ui: &mut egui::Ui, w: &WindowInfo, clip: win::Rect) -> egui::Response {
        let thumb_h = CARD_W * 9.0 / 16.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(CARD_W, CARD_HEADER + thumb_h + 6.0), egui::Sense::click());
        let on_air = self.on_air_hwnd() == Some(w.hwnd);
        let in_preview = self.preview == Some(w.hwnd);
        let visuals = ui.visuals();
        let fill = if resp.hovered() { visuals.widgets.hovered.weak_bg_fill } else { visuals.faint_bg_color };
        let stroke = if on_air {
            egui::Stroke::new(3.0, AIR_COLOR)
        } else if in_preview {
            egui::Stroke::new(3.0, PREVIEW_COLOR)
        } else {
            egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
        };
        let painter = ui.painter_at(rect.expand(3.0));
        painter.rect_filled(rect, 6.0, fill);
        painter.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);

        let icon = self.icons.get(&w.hwnd).and_then(|t| t.as_ref());
        let icon_rect = egui::Rect::from_min_size(rect.min + egui::vec2(6.0, 5.0), egui::vec2(20.0, 20.0));
        match icon {
            Some(t) => {
                painter.image(t.id(), icon_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
            None => {
                painter.rect_filled(icon_rect, 4.0, egui::Color32::from_gray(80));
                let letter = w.exe.chars().next().unwrap_or('?').to_uppercase().to_string();
                painter.text(icon_rect.center(), egui::Align2::CENTER_CENTER, letter, egui::FontId::proportional(13.0), egui::Color32::WHITE);
            }
        }
        let badge_w = if on_air { 52.0 } else { 0.0 };
        let mut job = egui::text::LayoutJob::single_section(
            w.title.clone(),
            egui::TextFormat { font_id: egui::FontId::proportional(13.0), color: visuals.strong_text_color(), ..Default::default() },
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(CARD_W - 40.0 - badge_w);
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        painter.galley(rect.min + egui::vec2(32.0, (CARD_HEADER - galley.size().y) / 2.0), galley, egui::Color32::WHITE);
        if on_air {
            let badge = egui::Rect::from_min_size(egui::pos2(rect.right() - badge_w - 4.0, rect.top() + 6.0), egui::vec2(badge_w, 18.0));
            painter.rect_filled(badge, 4.0, AIR_COLOR);
            painter.text(badge.center(), egui::Align2::CENTER_CENTER, "NO AR", egui::FontId::proportional(11.0), egui::Color32::WHITE);
        }

        let thumb = egui::Rect::from_min_size(rect.min + egui::vec2(6.0, CARD_HEADER), egui::vec2(CARD_W - 12.0, thumb_h - 6.0));
        painter.rect_filled(thumb, 3.0, egui::Color32::BLACK);
        // A minimized window has no image to show, and the ON AIR window is already live in the
        // ON AIR frame: a second live copy would make DWM rescale the same video twice. Both show the large icon.
        if w.minimized || on_air {
            let big = egui::Rect::from_center_size(thumb.center() - egui::vec2(0.0, 8.0), egui::vec2(48.0, 48.0));
            if let Some(t) = icon {
                painter.image(t.id(), big, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
            let note = if on_air { "no ar (imagem no quadro NO AR)" } else { "minimizada" };
            painter.text(thumb.center_bottom() - egui::vec2(0.0, 12.0), egui::Align2::CENTER_CENTER, note, egui::FontId::proportional(11.0), egui::Color32::GRAY);
        } else {
            self.placed.push((Slot::Card(w.hwnd), to_px(thumb, ui.ctx()), clip));
        }
        // No floating tooltip: Windows thumbnail would be on top of it. Title and program go to status bar.
        resp
    }

    // -----------------------------------------------------------------------
    // Sidebar: queue, media and mixer

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::CollapsingHeader::new(egui::RichText::new("Fila").strong()).default_open(true).show(ui, |ui| self.queue_ui(ui));
            egui::CollapsingHeader::new(egui::RichText::new("Mídia").strong()).default_open(true).show(ui, |ui| self.media_ui(ui));
            egui::CollapsingHeader::new(egui::RichText::new("Mixer").strong()).default_open(true).show(ui, |ui| self.mixer_ui(ui));
        });
    }

    /// Adds a tooltip that also hides the Windows thumbnails while it is open.
    fn tip(&mut self, r: egui::Response, text: &str) -> egui::Response {
        let r = r.on_hover_text(text);
        self.tip_open |= r.is_tooltip_open();
        r
    }

    fn queue_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let ready = !self.cfg.queue.items.is_empty();
            let next = egui::Button::new(egui::RichText::new("Próximo ▶").strong().color(egui::Color32::WHITE))
                .fill(if ready { PREVIEW_COLOR } else { egui::Color32::from_gray(60) });
            let r = ui.add_enabled(ready, next);
            if self.tip(r, "Mostra o próximo item da fila (PageDown)").clicked() {
                self.queue_next();
            }
            let r = ui.add_enabled(self.cfg.queue.pos.is_some(), egui::Button::new("Reiniciar"));
            if self.tip(r, "O próximo volta a ser o primeiro item").clicked() {
                self.cfg.queue.reset();
                self.status = "Fila reiniciada: o Próximo volta ao primeiro item.".into();
            }
            let r = ui.add_enabled(ready, egui::Button::new("Limpar"));
            if self.tip(r, "Remove todos os itens da fila").clicked() {
                self.cfg.queue.clear();
                self.queue_sel = None;
                self.status = "Fila limpa.".into();
            }
        });
        ui.checkbox(&mut self.cfg.queue_take, "Próximo já exibe (sem parar na PRÉVIA)");

        if self.cfg.queue.items.is_empty() {
            ui.label(egui::RichText::new("Fila vazia. Adicione a janela da PRÉVIA, um arquivo ou um link.").weak());
        } else {
            ui.label(egui::RichText::new("Arraste para reordenar. Clique e aperte Del para remover.").small().weak());
        }
        self.queue_cards(ui);

        ui.horizontal_wrapped(|ui| {
            let r = ui.add_enabled(self.preview.is_some(), egui::Button::new("+ Janela da PRÉVIA"));
            if self.tip(r, "Põe na fila a janela que está na PRÉVIA").clicked() {
                if let Some(p) = self.preview {
                    let title = self.title_of(p);
                    self.queue_add(title.clone(), queue::Kind::Window(title));
                }
            }
            if ui.button("+ Arquivo").clicked() {
                if let Some(path) = win::pick_any_file(self.me) {
                    let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
                    self.queue_add(name, queue::Kind::File(path));
                }
            }
        });
        self.link_row(ui);
    }

    /// Link field with paste support. eframe has no clipboard here, so Ctrl+V is read from `clip`.
    fn link_row(&mut self, ui: &mut egui::Ui) {
        let id = ui.make_persistent_id("link_field");
        let pressed = clip::paste_pressed(&mut self.paste_was_down);
        let window_focused = ui.input(|i| i.viewport().focused.unwrap_or(true));
        if pressed && window_focused && ui.memory(|m| m.has_focus(id)) {
            if let Some(text) = clip::read_text() {
                ui.ctx().input_mut(|i| i.events.push(egui::Event::Paste(clip::clean_link(&text))));
            }
        }
        let field = ui.add(egui::TextEdit::singleline(&mut self.url_input).id(id).hint_text("https://...").desired_width(f32::INFINITY));
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.horizontal_wrapped(|ui| {
            let r = ui.button("Colar");
            if self.tip(r, "Cola o link copiado, sem espaços nem quebras de linha").clicked() {
                match clip::read_text().map(|t| clip::clean_link(&t)).filter(|t| !t.is_empty()) {
                    Some(text) => self.url_input = text,
                    None => self.status = "Não há texto copiado para colar.".into(),
                }
            }
            if ui.button("+ Link").clicked() || enter {
                match normalize_url(&self.url_input) {
                    Some(url) => {
                        self.queue_add(url.clone(), queue::Kind::Url(url));
                        self.url_input.clear();
                    }
                    None => self.status = "Escreva um link sem espaços, como youtube.com/watch?v=...".into(),
                }
            }
        });
    }

    fn queue_remove(&mut self, i: usize) {
        if let Some(item) = self.cfg.queue.items.get(i) {
            self.status = format!("Removido da fila: {}", item.label);
        }
        self.cfg.queue.remove(i);
        self.queue_sel = None;
    }

    /// One small card per queue item: icon, short title, type, current/next highlight,
    /// drag to reorder and up/down/remove buttons.
    fn queue_cards(&mut self, ui: &mut egui::Ui) {
        let (pos, upcoming) = (self.cfg.queue.pos, self.cfg.queue.upcoming());
        let n = self.cfg.queue.items.len();
        let mut action: Option<(usize, i8)> = None;
        let mut dropped: Option<(usize, usize)> = None;
        ui.spacing_mut().item_spacing.y = 4.0;
        for i in 0..n {
            let item = self.cfg.queue.items[i].clone();
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), QUEUE_CARD_H), egui::Sense::click_and_drag());
            if resp.drag_started() {
                resp.dnd_set_drag_payload(i);
            }
            if resp.clicked() {
                self.queue_sel = Some(i);
            }
            if let Some(from) = resp.dnd_release_payload::<usize>() {
                dropped = Some((*from, i));
            }
            let (state, color) = if pos == Some(i) {
                ("atual", Some(PREVIEW_COLOR))
            } else if upcoming == Some(i) {
                ("próximo", Some(NEXT_COLOR))
            } else {
                ("", None)
            };
            let visuals = ui.visuals();
            let selected = self.queue_sel == Some(i);
            let fill = if selected || resp.hovered() { visuals.widgets.hovered.weak_bg_fill } else { visuals.faint_bg_color };
            let stroke = match color {
                Some(c) => egui::Stroke::new(2.0, c),
                None if selected => egui::Stroke::new(1.5, visuals.selection.stroke.color),
                None => egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
            };
            let (strong, weak) = (visuals.strong_text_color(), visuals.weak_text_color());
            let painter = ui.painter();
            painter.rect_filled(rect, 6.0, fill);
            painter.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);

            let icon_rect = egui::Rect::from_min_size(rect.min + egui::vec2(6.0, (QUEUE_CARD_H - 28.0) / 2.0), egui::vec2(28.0, 28.0));
            let texture = match &item.kind {
                queue::Kind::Window(_) => queue::find_window(&item, &self.windows).and_then(|w| self.icons.get(&w.hwnd)).and_then(|t| t.as_ref()),
                _ => None,
            };
            match texture {
                Some(t) => {
                    painter.image(t.id(), icon_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                }
                None => {
                    painter.rect_filled(icon_rect, 5.0, egui::Color32::from_gray(70));
                    let glyph = match queue::kind_label(&item.kind) {
                        "janela" => "🗔",
                        "vídeo" => "🎬",
                        "link" => "🌐",
                        _ => "📄",
                    };
                    painter.text(icon_rect.center(), egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(16.0), egui::Color32::WHITE);
                }
            }
            let text_x = icon_rect.right() + 8.0;
            let buttons_w = 3.0 * 22.0 + 4.0;
            let text_w = (rect.right() - buttons_w - text_x - 4.0).max(30.0);
            let mut job = egui::text::LayoutJob::single_section(
                item.label.clone(),
                egui::TextFormat { font_id: egui::FontId::proportional(13.0), color: strong, ..Default::default() },
            );
            job.wrap = egui::text::TextWrapping::truncate_at_width(text_w);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            let painter = ui.painter();
            painter.galley(egui::pos2(text_x, rect.top() + 5.0), galley, strong);
            let kind = queue::kind_label(&item.kind);
            let sub = if state.is_empty() { format!("{}. {kind}", i + 1) } else { format!("{}. {kind} · {}", i + 1, state.to_uppercase()) };
            painter.text(egui::pos2(text_x, rect.bottom() - 6.0), egui::Align2::LEFT_BOTTOM, sub, egui::FontId::proportional(11.0), color.unwrap_or(weak));

            let button = |k: usize| egui::Rect::from_min_size(egui::pos2(rect.right() - buttons_w + 22.0 * k as f32, rect.center().y - 10.0), egui::vec2(20.0, 20.0));
            if ui.put(button(0), egui::Button::new("⏶").small().frame(false)).on_hover_text("Subir").clicked() && i > 0 {
                action = Some((i, -1));
            }
            if ui.put(button(1), egui::Button::new("⏷").small().frame(false)).on_hover_text("Descer").clicked() && i + 1 < n {
                action = Some((i, 1));
            }
            if ui.put(button(2), egui::Button::new("✖").small().frame(false)).on_hover_text("Remover da fila").clicked() {
                action = Some((i, 0));
            }
            // Insertion line while another card is dragged over this one.
            if let Some(from) = resp.dnd_hover_payload::<usize>().filter(|from| **from != i) {
                let y = if *from > i { rect.top() - 2.0 } else { rect.bottom() + 2.0 };
                ui.painter().hline(rect.x_range(), y, egui::Stroke::new(3.0, PREVIEW_COLOR));
            }
            if resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        if let Some((from, to)) = dropped {
            self.cfg.queue.move_to(from, to);
            self.queue_sel = Some(to);
        }
        match action {
            Some((i, -1)) => {
                self.cfg.queue.move_up(i);
                self.queue_sel = Some(i - 1);
            }
            Some((i, 1)) => {
                self.cfg.queue.move_down(i);
                self.queue_sel = Some(i + 1);
            }
            Some((i, _)) => self.queue_remove(i),
            None => {}
        }
    }

    fn media_ui(&mut self, ui: &mut egui::Ui) {
        struct Src {
            key: media::SourceKey,
            label: String,
            info: String,
            playing: Option<bool>,
            on_air: bool,
            icon: Option<egui::TextureHandle>,
            hwnd: Option<Hwnd>,
            can: (bool, bool, bool),
        }
        let air_exe = self.on_air_hwnd().and_then(|h| self.windows.iter().find(|w| w.hwnd == h)).map(|w| w.exe.clone());
        let icon_of = |app: &Self, exe: &str| {
            let w = app.windows.iter().find(|w| media::same_app(exe, &w.exe))?;
            app.icons.get(&w.hwnd)?.clone()
        };
        let air_title = self.on_air_hwnd().and_then(|h| self.windows.iter().find(|w| w.hwnd == h)).map(|w| w.title.clone()).unwrap_or_default();
        let mut srcs: Vec<Src> = Vec::new();
        let sessions = self.media.sessions();
        for s in &sessions {
            let info = if s.artist.is_empty() { s.title.clone() } else { format!("{} · {}", s.title, s.artist) };
            // Several sessions of one app (browser windows): only the one whose tab is the window title is on air.
            let siblings = sessions.iter().filter(|o| o.app == s.app).count();
            let same = air_exe.as_deref().is_some_and(|e| media::same_app(&s.app, e));
            srcs.push(Src {
                key: media::SourceKey::Session(s.id.clone()),
                on_air: same && (siblings == 1 || media::title_in_window(&s.title, &air_title)),
                icon: icon_of(self, &s.app),
                label: media::display_name(&s.app),
                info,
                playing: Some(s.playing),
                hwnd: None,
                can: (s.can_prev, s.can_toggle, s.can_next),
            });
        }
        // Players without a media session (VLC, MPC, PotPlayer, mpv, WMP): one source per window, driven by WM_APPCOMMAND.
        for (w, name) in self.windows.iter().filter_map(|w| Some((w, media::window_player(&w.exe)?))) {
            if sessions.iter().any(|s| media::same_app(&s.app, &w.exe)) {
                continue;
            }
            srcs.push(Src {
                key: media::SourceKey::Window(w.hwnd.0),
                label: name.into(),
                info: w.title.clone(),
                playing: None,
                on_air: self.on_air_hwnd() == Some(w.hwnd),
                icon: self.icons.get(&w.hwnd).cloned().flatten(),
                hwnd: Some(w.hwnd),
                can: (true, true, true),
            });
        }
        if srcs.is_empty() {
            ui.label(egui::RichText::new("Nenhuma mídia encontrada").weak());
        }
        let keys: Vec<_> = srcs.iter().map(|s| s.key.clone()).collect();
        let playing: Vec<_> = srcs.iter().map(|s| s.playing == Some(true)).collect();
        let (chosen, lost) = media::resolve(self.media_sel.as_ref(), &keys, &playing);
        if lost {
            self.media_sel = None;
            self.status = "A fonte de mídia escolhida sumiu. Voltei para a automática.".into();
        }
        let mut pick = None;
        let mut action: Option<(usize, media::MediaCmd)> = None;
        for (i, s) in srcs.iter().enumerate() {
            let selected = self.media_sel.is_some() && chosen == Some(i);
            let stroke = if selected { egui::Stroke::new(1.5, ui.visuals().selection.bg_fill) } else { ui.visuals().widgets.noninteractive.bg_stroke };
            egui::Frame::group(ui.style()).stroke(stroke).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    if let Some(t) = &s.icon {
                        ui.image((t.id(), egui::vec2(18.0, 18.0)));
                    }
                    if ui.selectable_label(selected, egui::RichText::new(&s.label).strong()).clicked() {
                        pick = Some(s.key.clone());
                    }
                    match s.playing {
                        Some(true) => { ui.label(egui::RichText::new("tocando").small().color(PREVIEW_COLOR)); }
                        Some(false) => { ui.label(egui::RichText::new("pausado").small().weak()); }
                        None => {}
                    }
                    if s.on_air {
                        ui.label(egui::RichText::new("NO AR").small().color(egui::Color32::from_rgb(230, 80, 60)));
                    }
                    if ui.add_enabled(s.can.1, egui::Button::new("⏯")).clicked() {
                        action = Some((i, media::MediaCmd::Toggle));
                    }
                });
                if !s.info.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(&s.info).weak()).truncate());
                }
            });
        }
        if let Some(k) = pick {
            self.media_sel = Some(k);
        }
        // Transport buttons act on the chosen source, not on whichever Windows considers current.
        if let Some(i) = chosen {
            let s = &srcs[i];
            let mode = if self.media_sel.is_some() { "escolhida" } else { "automática" };
            ui.label(egui::RichText::new(format!("Controles: {} ({mode})", s.label)).small().weak());
            ui.horizontal(|ui| {
                let cmds = [(s.can.0, "⏮", media::MediaCmd::Prev), (s.can.1, "⏯", media::MediaCmd::Toggle), (s.can.2, "⏭", media::MediaCmd::Next)];
                for (can, icon, cmd) in cmds {
                    if ui.add_enabled(can, egui::Button::new(egui::RichText::new(icon).size(16.0)).min_size(egui::vec2(36.0, 24.0))).clicked() {
                        action = Some((i, cmd));
                    }
                }
            });
        }
        if let Some((i, cmd)) = action {
            match (&srcs[i].key, srcs[i].hwnd) {
                (media::SourceKey::Session(id), _) => self.media.send(id, cmd),
                (_, Some(h)) => {
                    if !media::send_to_window(h, cmd) {
                        self.status = "Não consegui mandar o comando para o reprodutor.".into();
                    }
                }
                _ => {}
            }
        }
        ui.separator();
        // Players without Windows media control still understand the play/pause key sent to the window.
        if let Some(h) = self.on_air_hwnd() {
            if ui.button("⏯ Tocar/pausar a janela NO AR").clicked() && !media::send_to_window(h, media::MediaCmd::Toggle) {
                self.status = "Não consegui mandar tocar/pausar para a janela no ar.".into();
            }
        }
        if ui.button("Abrir vídeo no VLC (sem HUD)").clicked() {
            self.open_vlc();
        }
    }

    fn mixer_ui(&mut self, ui: &mut egui::Ui) {
        if self.vol_edit.as_ref().is_some_and(|e| e.2.elapsed() > Duration::from_millis(700)) {
            self.vol_edit = None;
        }
        let (master, master_muted) = self.mixer.master();
        self.volume_row(ui, MASTER_ID, "Volume geral", master, master_muted, None, true, None);
        ui.separator();
        // One program per exe, with the name of its first card.
        let mut programs: Vec<(String, String)> = Vec::new();
        // Window icon of each program, reused from the window cards.
        let mut exe_icons: HashMap<String, egui::TextureHandle> = HashMap::new();
        // Browsers started more than once (one process per profile) get one mixer row per process, named by its window.
        let mut instances: Vec<(String, String)> = Vec::new();
        for w in &self.windows {
            if !programs.iter().any(|(p, _)| p.eq_ignore_ascii_case(&w.exe_path)) {
                programs.push((w.exe_path.clone(), w.title.clone()));
            }
            let instance = format!("{}|{}", w.exe_path, w.pid);
            if !instances.iter().any(|(p, _)| p.eq_ignore_ascii_case(&instance)) {
                instances.push((instance, w.title.clone()));
            }
            if let Some(Some(t)) = self.icons.get(&w.hwnd) {
                exe_icons.entry(w.exe_path.to_lowercase()).or_insert_with(|| t.clone());
            }
        }
        self.mixer.set_known(&programs.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>());
        self.mixer.watching();
        programs.extend(instances);
        let (playing, quiet) = mixer::split_quiet(mixer::rows(self.mixer.apps(), &programs));
        if playing.is_empty() {
            ui.label(egui::RichText::new("Nenhum programa tocando agora.").weak());
        }
        for a in &playing {
            self.app_row(ui, a, &exe_icons);
        }
        if !quiet.is_empty() {
            let title = egui::RichText::new(format!("Sem áudio ({})", quiet.len())).strong();
            let section = egui::CollapsingHeader::new(title).id_salt("mixer_quiet").open(Some(self.cfg.quiet_open)).show(ui, |ui| {
                for a in &quiet {
                    self.app_row(ui, a, &exe_icons);
                }
            });
            if section.header_response.clicked() {
                self.cfg.quiet_open = !self.cfg.quiet_open;
            }
        }
        ui.label(egui::RichText::new("O Windows não separa o volume por aba: janelas de um mesmo navegador e perfil dividem um controle. Janelas de perfis ou navegadores diferentes têm controles próprios.").small().weak());
    }

    fn app_row(&mut self, ui: &mut egui::Ui, a: &mixer::AudioApp, exe_icons: &HashMap<String, egui::TextureHandle>) {
        let name = match a.sound {
            mixer::Sound::Playing => a.name.clone(),
            mixer::Sound::Idle => format!("{} (parado)", a.name),
            mixer::Sound::None => format!("{} (sem áudio no momento)", a.name),
        };
        let lead = (exe_icons.get(a.id.split('|').next().unwrap_or_default()),a.name.chars().next().unwrap_or('?'));
        self.volume_row(ui, &a.id, &name, a.volume, a.muted, Some(a.peak), a.sound != mixer::Sound::None, Some(lead));
    }

    /// Name, mute button, 0-100 slider, and for programs, the level bar.
    /// `lead` is the program icon (or, without one, the initial letter) shown before the name.
    fn volume_row(&mut self, ui: &mut egui::Ui, id: &str, name: &str, volume: f32, muted: bool, peak: Option<f32>, enabled: bool, lead: Option<(Option<&egui::TextureHandle>, char)>) {
        ui.horizontal(|ui| {
            if let Some((icon, letter)) = lead {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                match icon {
                    Some(t) => {
                        ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                    }
                    None => {
                        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_gray(80));
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, letter.to_uppercase().to_string(), egui::FontId::proportional(11.0), egui::Color32::WHITE);
                    }
                }
            }
            ui.add(egui::Label::new(name).truncate());
        });
        ui.add_enabled_ui(enabled, |ui| ui.horizontal(|ui| {
            let icon = if muted { "🔇" } else { "🔊" };
            if ui.add(egui::Button::new(icon).selected(muted)).clicked() {
                if id == MASTER_ID {
                    self.mixer.set_master_mute(!muted);
                } else {
                    self.mixer.set_mute(id, !muted);
                }
            }
            let mut v = match &self.vol_edit {
                Some((e, v, _)) if e == id => *v,
                _ => volume * 100.0,
            };
            ui.spacing_mut().slider_width = (ui.available_width() - 50.0).max(60.0);
            let r = ui.add(egui::Slider::new(&mut v, 0.0..=100.0).fixed_decimals(0));
            if r.changed() || r.dragged() {
                self.vol_edit = Some((id.to_owned(), v, Instant::now()));
            }
            if r.changed() {
                if id == MASTER_ID {
                    self.mixer.set_master(v / 100.0);
                } else {
                    self.mixer.set_volume(id, v / 100.0);
                }
            }
        }));
        if let Some(p) = peak {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
            let level = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * p.clamp(0.0, 1.0), rect.height()));
            ui.painter().rect_filled(level, 2.0, PREVIEW_COLOR);
        }
        ui.add_space(4.0);
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
            // With a menu open (monitor list), Esc and Enter belong to it, not the external screen.
            // `ctx.any_popup_open()` only knows menus after the frame is drawn, and the
            // keyboard is read before that. egui's memory keeps the open menu between frames.
            // Typing a link in the queue: keys belong to the text field.
        if egui::Popup::is_any_open(ctx) || ctx.text_edit_focused() {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            if let Some(i) = self.queue_sel {
                self.queue_remove(i);
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::PageDown)) {
            self.queue_next();
        }
        let (enter, esc, back, next) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowRight),
            )
        });
        if enter {
            self.take();
        }
        if esc {
            self.clear();
        }
        if back || next {
            let pos = self.preview.and_then(|p| self.windows.iter().position(|w| w.hwnd == p));
            let n = self.windows.len();
            let target = match (pos, next) {
                _ if n == 0 => None,
                (None, _) => Some(0),
                (Some(i), true) => Some((i + 1).min(n - 1)),
                (Some(i), false) => Some(i.saturating_sub(1)),
            };
            if let Some(i) = target {
                self.select(self.windows[i].hwnd);
            }
        }
    }
}

fn to_px(r: egui::Rect, ctx: &egui::Context) -> win::Rect {
    let ppp = ctx.pixels_per_point();
    win::Rect {
        left: (r.left() * ppp).round() as i32,
        top: (r.top() * ppp).round() as i32,
        right: (r.right() * ppp).round() as i32,
        bottom: (r.bottom() * ppp).round() as i32,
    }
}

/// Uses the Windows font (Segoe UI), with symbols and emojis as fallback,
/// so window titles appear correctly.
fn install_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontData, FontInsert, FontPriority, InsertFontFamily};
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let fonts = [
        ("segoeui", FontPriority::Highest),
        ("seguisym", FontPriority::Lowest),
        ("seguiemj", FontPriority::Lowest),
    ];
    for (name, priority) in fonts {
        let Ok(bytes) = std::fs::read(format!(r"{dir}\Fonts\{name}.ttf")) else { continue };
        let families = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
            .into_iter()
            .filter(|f| *f == egui::FontFamily::Proportional || priority == FontPriority::Lowest)
            .map(|family| InsertFontFamily { family, priority })
            .collect();
        ctx.add_font(FontInsert::new(name, FontData::from_owned(bytes), families));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.me.is_none() {
            if let Ok(RawWindowHandle::Win32(h)) = frame.window_handle().map(|h| h.as_raw()) {
                self.me = Some(Hwnd(h.hwnd.get()));
                self.keep_me_off_datashow();
            }
        }
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh_windows(&ctx);
            self.refresh_monitors();
        }
        self.keyboard(&ctx);
        self.sync_guard(None);
        if let Some(title) = win::guard_take_returned().pop() {
            self.status = format!("\"{title}\" abriu no datashow e foi devolvida para a tela 1.");
        }
        for h in win::guard_take_arrived() {
            if !h.exists() {
                continue;
            }
            if !self.windows.iter().any(|w| w.hwnd == h) {
                self.refresh_windows(&ctx);
            }
            self.select(h);
            self.status = format!("A apresentação {} abriu no datashow e está na PRÉVIA. Aperte EXIBIR para mostrar.", self.title_of(h));
        }
        // Minimized or hidden panel: nobody sees the thumbnails, so they are released (DWM stops
        // copying those windows, the live video included) and the interface is not built.
        if panel_hidden(&ctx.input(|i| i.viewport().clone())) {
            self.thumbs.clear();
            self.placed.clear();
            ctx.request_repaint_after(HIDDEN_REFRESH);
            return;
        }
        // The card under the pointer is measured while drawing; this shows the previous frame's.
        let hovered = self.hovered_card.take();
        self.tip_open = false;

        egui::Panel::top("barra").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            let text = match &hovered {
                Some(h) => h.as_str(),
                None if self.status.is_empty() => "Pronto.",
                None => &self.status,
            };
            // Version is laid out first from the right, so the status text truncates before overlapping it.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(concat!("TakeScreen v", env!("CARGO_PKG_VERSION")));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(text).truncate());
                });
            });
        });
        // A barra lateral vem antes do painel central, que encolhe: as miniaturas do Windows nunca ficam por cima dela.
        if !self.cfg.side_closed {
            egui::Panel::right("lateral").resizable(true).default_size(260.0).size_range(220.0..=420.0).show(ui, |ui| {
                self.side_panel(ui);
            });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            let studio_h = (ui.available_height() * 0.55).max(220.0);
            self.studio(ui, studio_h);
            ui.add_space(6.0);
            ui.separator();
            self.programs(ui);
        });

        self.apply_thumbnails(ctx.any_popup_open() || self.tip_open);
        ctx.request_repaint_after(REFRESH);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.cfg);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.thumbs.clear();
        win::guard_configure(false, win::Rect::default(), win::Rect::default(), &[], None);
        if let Some(old) = self.on_air.take() {
            win::restore(&old);
        }
        win::remember_on_air(None);
        if let Some(c) = self.curtain.take() {
            win::destroy(c);
        }
    }
}

/// The panel cannot be seen: minimized, or reported as fully covered by Windows.
fn panel_hidden(info: &egui::ViewportInfo) -> bool {
    info.minimized == Some(true) || info.occluded == Some(true)
}

/// Adds to `order` the new windows (at the end) and removes the ones that closed.
fn track_order(order: &mut Vec<Hwnd>, alive: &[Hwnd]) {
    order.retain(|h| alive.contains(h));
    for h in alive {
        if !order.contains(h) {
            order.push(*h);
        }
    }
}

/// Link typed in queue: no spaces, and only http/https (Windows would open any program).
fn normalize_url(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    let lower = s.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Some(s.to_owned())
    } else if s.contains("://") {
        None
    } else {
        Some(format!("https://{s}"))
    }
}

/// Opens the standby image (PNG or JPEG), resizes to fit 3840x2160 preserving
/// aspect ratio, and returns the size and pixels in RGBA, top to bottom.
fn load_standby(path: &str) -> Result<([usize; 2], Vec<u8>), String> {
    let img = image::open(path).map_err(|e| e.to_string())?;
    let (max_w, max_h) = STANDBY_MAX;
    let img = if img.width() > max_w || img.height() > max_h {
        img.resize(max_w, max_h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    Ok(([rgba.width() as usize, rgba.height() as usize], rgba.into_raw()))
}

fn main() -> eframe::Result {
    if let Some(code) = selftest::run_from_args() {
        std::process::exit(code);
    }
    if !win::single_instance() {
        win::alert("O TakeScreen já está aberto. Use a janela que já está aberta.");
        return Ok(());
    }
    // If the program crashes, the ON AIR window returns to screen 1 before closing.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        win::recover_now();
        default_hook(info);
    }));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TakeScreen")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("TakeScreen", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teclas_de_tela_cheia_sao_desfeitas_ao_sair_do_ar() {
        let mut keys = Vec::new();
        win::toggle_key(&mut keys, win::VK_VIDEO_FULLSCREEN);
        win::toggle_key(&mut keys, win::VK_FULLSCREEN);
        win::toggle_key(&mut keys, win::VK_FULLSCREEN);
        assert_eq!(keys, [win::VK_VIDEO_FULLSCREEN]);
        assert_ne!(win::undo_key(win::VK_VIDEO_FULLSCREEN), win::VK_VIDEO_FULLSCREEN);
        assert_eq!(win::undo_key(win::VK_FULLSCREEN), win::VK_FULLSCREEN);
    }

    #[test]
    fn painel_oculto_so_quando_minimizado_ou_coberto() {
        let mut info = egui::ViewportInfo::default();
        // Unknown state (first frames) counts as visible: the panel must never start blank.
        assert!(!panel_hidden(&info));
        info.focused = Some(false);
        assert!(!panel_hidden(&info), "sem foco o operador ainda ve o painel");
        info.minimized = Some(true);
        assert!(panel_hidden(&info));
        info.minimized = Some(false);
        info.occluded = Some(true);
        assert!(panel_hidden(&info));
    }

    #[test]
    fn cartoes_ficam_na_ordem_de_chegada() {
        let (a, b, c, d) = (Hwnd(1), Hwnd(2), Hwnd(3), Hwnd(4));
        let mut order = Vec::new();
        track_order(&mut order, &[b, a, c]);
        assert_eq!(order, [b, a, c]);
        // O foco mudou (a lista do Windows vem em outra ordem), b fechou e d abriu.
        track_order(&mut order, &[d, c, a]);
        assert_eq!(order, [a, c, d]);
    }

    #[test]
    fn imagem_de_espera_grande_e_reduzida_mantendo_a_proporcao() {
        let path = std::env::temp_dir().join(format!("takescreen-teste-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(5000, 1000, image::Rgba([10, 20, 30, 255])).save(&path).unwrap();
        let loaded = load_standby(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let ([w, h], rgba) = loaded.unwrap();
        assert_eq!((w, h), (3840, 768));
        assert_eq!(rgba.len(), w * h * 4);
        assert_eq!(&rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn link_da_fila_vira_http() {
        assert_eq!(normalize_url("  youtube.com/x "), Some("https://youtube.com/x".into()));
        assert_eq!(normalize_url("HTTP://a.com"), Some("HTTP://a.com".into()));
        assert_eq!(normalize_url("file://C:/x.exe"), None);
        assert_eq!(normalize_url("a b.com"), None);
        assert_eq!(normalize_url(""), None);
    }

    #[test]
    fn imagem_de_espera_invalida_vira_erro() {
        assert!(load_standby(r"C:\nao\existe\takescreen.png").is_err());
    }
}
