//! TakeScreen: painel do operador para escolher o que aparece no datashow (segunda tela).
//!
//! Funciona como o Modo Estúdio do OBS: em cima a PRÉVIA (o que você escolheu) e o
//! NO AR (o que o público está vendo); embaixo os programas abertos, cada um com o
//! ícone e uma prévia ao vivo. Um clique em EXIBIR, ou Enter, troca o que está no ar.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("O TakeScreen só funciona no Windows. Compile com --target x86_64-pc-windows-gnu ou no próprio Windows.");

mod win;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};

use win::{Fit, Hwnd, Monitor, Saved, Thumbnail, WindowInfo};

const REFRESH: Duration = Duration::from_millis(1000);
const PREVIEW_COLOR: egui::Color32 = egui::Color32::from_rgb(46, 160, 67);
const AIR_COLOR: egui::Color32 = egui::Color32::from_rgb(218, 54, 51);
const CARD_W: f32 = 220.0;
const CARD_HEADER: f32 = 30.0;

/// O que fica salvo entre uma abertura e outra.
#[derive(Serialize, Deserialize, Default)]
struct Config {
    /// Nome do aparelho escolhido como datashow (por exemplo "EPSON PJ").
    datashow_name: Option<String>,
    datashow_device: Option<String>,
    /// A proteção do datashow vem ligada; isto só fica true se o operador desligar.
    #[serde(default)]
    protection_off: bool,
}

/// Onde cada miniatura do Windows deve ser desenhada neste quadro.
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
    icons: HashMap<Hwnd, Option<egui::TextureHandle>>,
    last_refresh: Instant,
    preview: Option<Hwnd>,
    fit: Fit,
    on_air: Option<Saved>,
    thumbs: HashMap<Slot, Thumbnail>,
    /// Áreas (em pixels físicos) e recorte de cada miniatura, montados durante o desenho da tela.
    placed: Vec<(Slot, win::Rect, win::Rect)>,
    status: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        win::guard_install(move || ctx.request_repaint());
        let cfg: Config = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let mut app = App {
            cfg,
            me: None,
            curtain: None,
            monitors: Vec::new(),
            datashow: 0,
            windows: Vec::new(),
            icons: HashMap::new(),
            last_refresh: Instant::now() - REFRESH,
            preview: None,
            fit: Fit::Borderless,
            on_air: None,
            thumbs: HashMap::new(),
            placed: Vec::new(),
            status: String::new(),
        };
        app.refresh_monitors();
        app
    }

    fn datashow_monitor(&self) -> Option<&Monitor> {
        self.monitors.get(self.datashow)
    }

    fn on_air_hwnd(&self) -> Option<Hwnd> {
        self.on_air.map(|s| s.hwnd)
    }

    /// Reencontra o datashow pelo nome do aparelho, mesmo que a ordem dos monitores tenha mudado.
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

    /// O monitor para onde as janelas voltam: o do painel, ou o principal.
    fn home_monitor(&self) -> Option<&Monitor> {
        let ds = self.datashow_monitor()?;
        let mine = self.me.and_then(win::window_rect);
        let others = || self.monitors.iter().filter(|m| m.device != ds.device);
        others()
            .find(|m| mine.is_some_and(|r| m.contains(&r)))
            .or_else(|| others().find(|m| m.primary))
            .or_else(|| others().next())
    }

    /// Diz à proteção qual é o datashow e quais janelas podem estar nele.
    fn sync_guard(&self, entering: Option<Hwnd>) {
        let (Some(ds), Some(home)) = (self.datashow_monitor(), self.home_monitor()) else {
            win::guard_configure(false, win::Rect::default(), win::Rect::default(), &[]);
            return;
        };
        let allowed: Vec<Hwnd> = self.on_air_hwnd().into_iter().chain(entering).collect();
        win::guard_configure(!self.cfg.protection_off, ds.rect, home.work, &allowed);
    }

    /// Se a janela que saiu do ar tinha sido aberta no datashow, ela não volta para lá.
    fn send_off_datashow(&self, hwnd: Hwnd) {
        let (Some(ds), Some(home)) = (self.datashow_monitor(), self.home_monitor()) else { return };
        if win::window_rect(hwnd).is_some_and(|r| ds.contains(&r)) {
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

    /// O painel nunca deve aparecer no datashow.
    fn keep_me_off_datashow(&self) {
        let (Some(me), Some(ds)) = (self.me, self.datashow_monitor()) else { return };
        let Some(r) = win::window_rect(me) else { return };
        if ds.contains(&r) {
            if let Some(other) = self.monitors.iter().find(|m| m.device != ds.device) {
                win::move_window(me, other.rect.left + 40, other.rect.top + 40);
            }
        }
    }

    fn refresh_windows(&mut self, ctx: &egui::Context) {
        self.windows = win::list_windows();
        if let Some(s) = self.on_air {
            if !s.hwnd.exists() {
                // A janela no ar foi fechada: a cortina preta já está logo atrás dela.
                self.on_air = None;
                self.status = "A janela que estava no ar foi fechada. O datashow está preto.".into();
            }
        }
        if self.preview.is_some_and(|p| !p.exists()) {
            self.preview = None;
        }

        let alive: Vec<Hwnd> = self.windows.iter().map(|w| w.hwnd).collect();
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
        self.fit = if uwp { Fit::Maximize } else { Fit::Borderless };
    }

    fn title_of(&self, hwnd: Hwnd) -> String {
        self.windows.iter().find(|w| w.hwnd == hwnd).map(|w| w.title.clone()).unwrap_or_default()
    }

    /// EXIBIR: a prévia vai para o ar e a janela que estava no ar volta para a tela 1.
    fn take(&mut self) {
        let (Some(target), Some(curtain), Some(ds)) = (self.preview, self.curtain, self.datashow_monitor().cloned()) else {
            return;
        };
        if self.on_air_hwnd() == Some(target) {
            return;
        }
        let me_there = self.me.and_then(win::window_rect).is_some_and(|r| ds.contains(&r));
        if self.monitors.len() < 2 || me_there {
            self.status = "O datashow não está conectado (ou é a mesma tela do painel).".into();
            return;
        }
        self.sync_guard(Some(target));
        let cover = self.on_air_hwnd().unwrap_or(curtain);
        let Some(saved) = win::take(target, cover, curtain, ds.rect, self.fit) else {
            self.status = "Não consegui mover essa janela. Ela ainda está aberta?".into();
            return;
        };
        if let Some(old) = self.on_air.replace(saved) {
            win::restore(&old);
            self.send_off_datashow(old.hwnd);
        }
        self.sync_guard(None);
        self.status = format!("No ar: {}", self.title_of(target));
        if let Some(me) = self.me {
            win::focus(me);
        }
    }

    /// LIMPAR: cortina preta na frente e a janela volta para a tela 1.
    fn clear(&mut self) {
        if let (Some(c), Some(ds)) = (self.curtain, self.datashow_monitor()) {
            win::place_curtain(c, ds.rect, true);
        }
        if let Some(old) = self.on_air.take() {
            win::restore(&old);
            self.send_off_datashow(old.hwnd);
            self.status = "Datashow limpo (tela preta).".into();
        }
        self.sync_guard(None);
        if let Some(me) = self.me {
            win::focus(me);
        }
    }

    fn fullscreen_key(&mut self) {
        if let (Some(s), Some(me)) = (self.on_air, self.me) {
            win::send_key(s.hwnd, win::VK_FULLSCREEN, me);
        }
    }

    // -----------------------------------------------------------------------
    // Miniaturas ao vivo

    fn source_of(&self, slot: Slot) -> Option<Hwnd> {
        match slot {
            Slot::Preview => self.preview.filter(|p| Some(*p) != self.on_air_hwnd()),
            Slot::Air => self.on_air_hwnd(),
            Slot::Card(h) => Some(h),
        }
    }

    /// Mostra as miniaturas nos lugares desenhados neste quadro e esconde as demais.
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
            // O Windows desenha as miniaturas por cima de tudo, inclusive de menus abertos.
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
            ui.checkbox(&mut protect, "Proteger datashow")
                .on_hover_text("Qualquer janela que abrir na tela 2 sem passar pelo TakeScreen volta na hora para a tela 1.");
            self.cfg.protection_off = !protect;
            if self.monitors.len() < 2 {
                ui.colored_label(AIR_COLOR, "Só um monitor conectado. Ligue o datashow.");
            }
        });
    }

    /// Desenha um quadro (PRÉVIA ou NO AR) com a miniatura dentro.
    fn screen_box(&mut self, ui: &mut egui::Ui, slot: Slot, title: &str, color: egui::Color32, caption: &str, width: f32) {
        ui.label(egui::RichText::new(title).strong().color(color).size(16.0));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, width * 9.0 / 16.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 4.0, egui::Color32::BLACK);
        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(3.0, color), egui::StrokeKind::Outside);
        ui.add(egui::Label::new(egui::RichText::new(caption).weak()).truncate());
        let full = to_px(ui.ctx().content_rect(), ui.ctx());
        self.placed.push((slot, to_px(rect.shrink(1.0), ui.ctx()), full));
    }

    fn studio(&mut self, ui: &mut egui::Ui, max_height: f32) {
        let gap = 140.0;
        let spacing = ui.spacing().item_spacing.x;
        // Duas telas, a coluna do botão, os espaços entre elas e a borda das molduras.
        let by_width = (ui.available_width() - gap - 2.0 * spacing - 16.0) / 2.0;
        let by_height = (max_height - 80.0) * 16.0 / 9.0;
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
                    ui.radio_value(&mut self.fit, Fit::Borderless, "Tela inteira sem bordas");
                    ui.radio_value(&mut self.fit, Fit::Maximize, "Maximizar");
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
                        self.fullscreen_key();
                    }
                    if ui.button("Limpar datashow (Esc)").clicked() {
                        self.clear();
                    }
                });
            });
        });
    }

    fn programs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Programas abertos");
            ui.label(egui::RichText::new("Clique para ver na prévia. Clique duplo exibe direto.").weak());
        });
        ui.add_space(4.0);

        let mut clicked = None;
        let mut double = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let spacing = 10.0;
            ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
            let cols = (((ui.available_width() + spacing) / (CARD_W + spacing)).floor() as usize).max(1);
            let clip = to_px(ui.clip_rect(), ui.ctx());
            let windows = self.windows.clone();
            for row in windows.chunks(cols) {
                ui.horizontal(|ui| {
                    for w in row {
                        let r = self.card(ui, w, clip);
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

    /// Um cartão: ícone e nome do programa em cima, prévia ao vivo embaixo.
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
        if w.minimized {
            // Janela minimizada não tem imagem para mostrar: fica o ícone grande.
            let big = egui::Rect::from_center_size(thumb.center() - egui::vec2(0.0, 8.0), egui::vec2(48.0, 48.0));
            if let Some(t) = icon {
                painter.image(t.id(), big, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
            painter.text(thumb.center_bottom() - egui::vec2(0.0, 12.0), egui::Align2::CENTER_CENTER, "minimizada", egui::FontId::proportional(11.0), egui::Color32::GRAY);
        } else {
            self.placed.push((Slot::Card(w.hwnd), to_px(thumb, ui.ctx()), clip));
        }
        resp.on_hover_text(format!("{}\n{}", w.title, w.exe))
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
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

/// Usa a fonte do Windows (Segoe UI), com os símbolos e emojis como reserva,
/// para os títulos das janelas aparecerem certos.
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

        egui::Panel::top("barra").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.label(if self.status.is_empty() { "Pronto." } else { &self.status });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let studio_h = (ui.available_height() * 0.55).max(220.0);
            self.studio(ui, studio_h);
            ui.add_space(6.0);
            ui.separator();
            self.programs(ui);
        });

        self.apply_thumbnails(ctx.any_popup_open());
        ctx.request_repaint_after(REFRESH);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.cfg);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.thumbs.clear();
        win::guard_configure(false, win::Rect::default(), win::Rect::default(), &[]);
        if let Some(old) = self.on_air.take() {
            win::restore(&old);
        }
        if let Some(c) = self.curtain.take() {
            win::destroy(c);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TakeScreen")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("TakeScreen", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
