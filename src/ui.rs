//! The window. It draws only after input or a backend event, and one time each second
//! during playback. It holds a small, limited set of cover images.

use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Rect, RichText, Sense,
    Stroke, TextureHandle, TextureOptions, Ui, UiBuilder, pos2, text::LayoutJob,
    text::TextWrapping, vec2,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    auth::{Config, REDIRECT_URI, clean_client_id},
    model::{
        AuthState, Card, CardKind, Command, Event, PageData, PageKey, Playback, PlayerStatus,
        Repeat, Setup, Track,
    },
    soloist,
};

const BG: Color32 = Color32::from_rgb(16, 17, 20);
const PANEL: Color32 = Color32::from_rgb(23, 24, 29);
const HOVER: Color32 = Color32::from_rgb(38, 40, 48);
const TEXT: Color32 = Color32::from_rgb(232, 233, 238);
const DIM: Color32 = Color32::from_rgb(150, 153, 165);
const ACCENT: Color32 = Color32::from_rgb(255, 176, 66);
const OK: Color32 = Color32::from_rgb(120, 200, 130);
const BAD: Color32 = Color32::from_rgb(240, 120, 110);

const ROW_H: f32 = 52.0;
/// The maximum number of cover textures in memory.
const MAX_TEXTURES: usize = 240;
const THUMB_PX: u32 = 96;
const COVER_PX: u32 = 360;

struct Tex {
    handle: TextureHandle,
    used: u64,
}

struct View {
    key: PageKey,
    data: PageData,
    loading: bool,
    error: Option<String>,
}

impl View {
    fn new(key: PageKey) -> Self {
        Self {
            key,
            data: PageData::default(),
            loading: true,
            error: None,
        }
    }

    /// The Spotify URI that continues playback after the selected track.
    fn context(&self) -> Option<String> {
        match &self.key {
            PageKey::Playlist(id) => Some(format!("spotify:playlist:{id}")),
            PageKey::Album(id) => Some(format!("spotify:album:{id}")),
            _ => None,
        }
    }
}

pub struct App {
    tx: UnboundedSender<Command>,
    rx: Receiver<Event>,

    auth: AuthState,
    player: PlayerStatus,
    setup_client_id: String,
    setup_path: String,
    setup_key: String,
    /// The result of the last search for Soloist, and its time and input.
    located: (soloist::Located, Instant, String),

    playlists: Vec<Card>,
    view: View,
    history: Vec<PageKey>,
    search: String,

    pb: Playback,
    volume: f32,
    seek_drag: Option<f32>,

    textures: HashMap<String, Tex>,
    pending: HashSet<String>,
    frame_no: u64,
    toast: Option<(String, Instant)>,
}

fn fmt_time(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Draws one line of text. Text that is too long ends with an ellipsis.
fn line(ui: &Ui, rect: Rect, text: &str, size: f32, color: Color32, align: Align2) {
    if rect.width() <= 4.0 || text.is_empty() {
        return;
    }
    let mut job = LayoutJob::simple_singleline(text.to_owned(), FontId::proportional(size), color);
    job.wrap = TextWrapping::truncate_at_width(rect.width());
    let galley = ui.painter().layout_job(job);
    let pos = align.align_size_within_rect(galley.size(), rect).min;
    ui.painter().galley(pos, galley, color);
}

fn icon_button(ui: &mut Ui, text: &str, size: f32, color: Color32, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(size + 14.0, size + 14.0), Sense::click());
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(size),
        color,
    );
    if color == ACCENT {
        // A dot shows the "on" state. The icon font does not always take the color.
        ui.painter()
            .circle_filled(pos2(rect.center().x, rect.bottom() - 1.0), 2.0, ACCENT);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip)
        .clicked()
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Play,
    Pause,
    Prev,
    Next,
}

/// A transport button that is drawn with shapes. It does not depend on a font.
fn shape_button(ui: &mut Ui, shape: Shape, size: f32, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
    let color = if resp.hovered() { ACCENT } else { TEXT };
    let c = rect.center();
    let h = size / 2.0;
    let p = ui.painter();
    let tri = |x0: f32, x1: f32| {
        egui::Shape::convex_polygon(
            vec![pos2(x0, c.y - h), pos2(x1, c.y), pos2(x0, c.y + h)],
            color,
            Stroke::NONE,
        )
    };
    let bar = |x: f32, w: f32| {
        p.rect_filled(
            Rect::from_min_max(pos2(x, c.y - h), pos2(x + w, c.y + h)),
            1.0,
            color,
        );
    };
    match shape {
        Shape::Play => {
            p.add(tri(c.x - h * 0.7, c.x + h));
        }
        Shape::Pause => {
            bar(c.x - h * 0.75, h * 0.5);
            bar(c.x + h * 0.25, h * 0.5);
        }
        Shape::Next => {
            p.add(tri(c.x - h, c.x + h * 0.5));
            bar(c.x + h * 0.55, h * 0.35);
        }
        Shape::Prev => {
            p.add(tri(c.x + h, c.x - h * 0.5));
            bar(c.x - h * 0.9, h * 0.35);
        }
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip)
        .clicked()
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tx: UnboundedSender<Command>,
        rx: Receiver<Event>,
    ) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_visuals(egui::Visuals::dark());
        ctx.global_style_mut(|s| {
            let v = &mut s.visuals;
            v.panel_fill = BG;
            v.window_fill = PANEL;
            v.extreme_bg_color = Color32::from_rgb(10, 10, 12);
            v.override_text_color = Some(TEXT);
            v.selection.bg_fill = ACCENT.linear_multiply(0.35);
            v.selection.stroke = Stroke::new(1.0, ACCENT);
            v.hyperlink_color = ACCENT;
            v.widgets.hovered.weak_bg_fill = HOVER;
            v.widgets.inactive.weak_bg_fill = Color32::from_rgb(32, 34, 41);
            s.spacing.item_spacing = vec2(8.0, 6.0);
            s.spacing.button_padding = vec2(10.0, 5.0);
        });

        let cfg = Config::load();
        Self {
            tx,
            rx,
            auth: AuthState::Starting,
            player: PlayerStatus::Off,
            setup_client_id: cfg.client_id,
            setup_path: cfg.soloist_path,
            setup_key: String::new(),
            located: (soloist::Located::Missing, Instant::now() - Duration::from_secs(60), String::new()),
            playlists: Vec::new(),
            view: View::new(PageKey::Home),
            history: Vec::new(),
            search: String::new(),
            pb: Playback::default(),
            volume: 0.8,
            seek_drag: None,
            textures: HashMap::new(),
            pending: HashSet::new(),
            frame_no: 0,
            toast: None,
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    fn toast(&mut self, text: String) {
        self.toast = Some((text, Instant::now()));
    }

    fn open(&mut self, key: PageKey) {
        if self.view.key == key && self.view.error.is_none() {
            return;
        }
        let old = std::mem::replace(&mut self.view, View::new(key.clone()));
        self.history.push(old.key);
        if self.history.len() > 30 {
            self.history.remove(0);
        }
        self.load(key);
    }

    fn load(&mut self, key: PageKey) {
        if matches!(&key, PageKey::Search(q) if q.is_empty()) {
            self.view.loading = false;
            return;
        }
        self.send(Command::Load { key, offset: 0 });
    }

    fn back(&mut self) {
        if let Some(key) = self.history.pop() {
            if let PageKey::Search(q) = &key {
                self.search = q.clone();
            }
            self.view = View::new(key.clone());
            self.load(key);
        }
    }

    fn open_card(&mut self, card: &Card) {
        self.open(match card.kind {
            CardKind::Album => PageKey::Album(card.id.clone()),
            CardKind::Artist => PageKey::Artist(card.id.clone()),
            CardKind::Playlist => PageKey::Playlist(card.id.clone()),
        });
    }

    fn events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Auth(state) => {
                    if matches!(state, AuthState::LoggedIn { .. }) {
                        self.send(Command::LoadPlaylists);
                        self.view = View::new(PageKey::Home);
                        self.history.clear();
                        self.load(PageKey::Home);
                    }
                    if state == AuthState::LoggedOut {
                        self.playlists.clear();
                        self.textures.clear();
                        self.pending.clear();
                    }
                    self.auth = state;
                }
                Event::Player(status) => self.player = status,
                Event::Page { key, offset, data } => {
                    if key != self.view.key {
                        continue; // The user opened a different page.
                    }
                    let v = &mut self.view;
                    v.loading = false;
                    v.error = None;
                    if offset == 0 {
                        v.data = data;
                    } else {
                        v.data.tracks.extend(data.tracks);
                        v.data.cards.extend(data.cards);
                        v.data.next = data.next;
                    }
                }
                Event::PageFailed { key, error } => {
                    if key == self.view.key {
                        self.view.loading = false;
                        self.view.data.next = None;
                        self.view.error = Some(error);
                    }
                }
                Event::Playlists(list) => self.playlists = list,
                Event::Image { url, size, rgba } => {
                    self.pending.remove(&url);
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    let handle = ctx.load_texture(&url, img, TextureOptions::LINEAR);
                    self.textures.insert(
                        url,
                        Tex {
                            handle,
                            used: self.frame_no,
                        },
                    );
                    self.evict_textures();
                }
                Event::Playback(pb) => self.pb = pb,
                Event::Volume(v) => self.volume = v,
                Event::Error(e) => self.toast(e),
            }
        }
    }

    /// Removes the textures that were not used for the longest time.
    fn evict_textures(&mut self) {
        if self.textures.len() <= MAX_TEXTURES {
            return;
        }
        let mut ages: Vec<(u64, String)> = self
            .textures
            .iter()
            .map(|(k, t)| (t.used, k.clone()))
            .collect();
        ages.sort_unstable();
        let remove = self.textures.len() - MAX_TEXTURES * 3 / 4;
        for (_, key) in ages.into_iter().take(remove) {
            self.textures.remove(&key);
        }
    }

    /// Draws a cover image, or a placeholder while the image loads.
    fn cover(&mut self, ui: &Ui, rect: Rect, url: &str, px: u32, radius: u8) {
        let radius = CornerRadius::same(radius);
        if let Some(tex) = self.textures.get_mut(url) {
            tex.used = self.frame_no;
            egui::Image::from_texture(&tex.handle)
                .corner_radius(radius)
                .paint_at(ui, rect);
            return;
        }
        ui.painter()
            .rect_filled(rect, radius, Color32::from_rgb(40, 42, 50));
        if !url.is_empty() && !self.pending.contains(url) && self.pending.len() < 64 {
            self.pending.insert(url.to_owned());
            self.send(Command::Image {
                url: url.to_owned(),
                px,
            });
        }
    }

    // ---------------------------------------------------------------- setup

    fn setup_screen(&mut self, ui: &mut Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.set_max_width(560.0);
                    ui.add_space(28.0);
                    ui.label(RichText::new("Spotimy").size(34.0).strong().color(ACCENT));
                    ui.label(RichText::new("A small native Spotify client").color(DIM));
                    ui.add_space(18.0);

                    match &self.auth {
                        AuthState::Starting | AuthState::Connecting => {
                            ui.spinner();
                            ui.label("Connecting…");
                            return;
                        }
                        AuthState::WaitingForBrowser => {
                            ui.spinner();
                            ui.label("Approve the login in your browser.");
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(
                                    "If the browser shows \"INVALID_CLIENT: Invalid redirect URI\", the \
                                     redirect URI is not in your Spotify app. Add it in the dashboard, \
                                     save, and try again:",
                                )
                                .color(DIM),
                            );
                            ui.horizontal(|ui| {
                                ui.monospace(REDIRECT_URI);
                                if ui.small_button("Copy").clicked() {
                                    ui.ctx().copy_text(REDIRECT_URI.to_owned());
                                }
                            });
                            ui.label(
                                RichText::new(
                                    "If it shows \"INVALID_CLIENT: Invalid client\", the client ID is not correct.",
                                )
                                .color(DIM),
                            );
                            ui.add_space(8.0);
                            if ui.button("Cancel").clicked() {
                                self.send(Command::CancelLogin);
                            }
                            return;
                        }
                        _ => {}
                    }

                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        ui.label(RichText::new("1. Spotify client ID").strong());
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.label(RichText::new("Create an app in the ").color(DIM));
                            ui.hyperlink_to(
                                "Spotify developer dashboard",
                                "https://developer.spotify.com/dashboard",
                            );
                            ui.label(RichText::new(". Select \"Web API\". Add this redirect URI:").color(DIM));
                        });
                        ui.horizontal(|ui| {
                            ui.monospace(REDIRECT_URI);
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(REDIRECT_URI.to_owned());
                            }
                        });
                        ui.add(
                            egui::TextEdit::singleline(&mut self.setup_client_id)
                                .hint_text("Client ID (32 characters)")
                                .desired_width(f32::INFINITY),
                        );
                        let client_id = clean_client_id(&self.setup_client_id);
                        if client_id.is_some() {
                            ui.label(RichText::new("✔ The client ID has the correct format.").color(OK).small());
                        } else if !self.setup_client_id.trim().is_empty() {
                            ui.label(
                                RichText::new(
                                    "A client ID has 32 characters: the digits 0 to 9 and the letters a to f. \
                                     Do not use the client secret.",
                                )
                                .color(BAD)
                                .small(),
                            );
                        }
                        ui.add_space(12.0);

                        ui.label(RichText::new("2. Spotify Soloist (the audio player)").strong());
                        // Search again each second, to see a download when it completes.
                        if self.located.1.elapsed() > Duration::from_secs(1)
                            || self.located.2 != self.setup_path
                        {
                            self.located = (
                                soloist::locate(&self.setup_path),
                                Instant::now(),
                                self.setup_path.clone(),
                            );
                        }
                        ui.ctx().request_repaint_after(Duration::from_secs(1));
                        match self.located.0.clone() {
                            soloist::Located::Exe(p) => {
                                ui.label(
                                    RichText::new(format!("✔ Soloist is ready: {}", p.display()))
                                        .color(OK)
                                        .small(),
                                );
                            }
                            soloist::Located::Archive(p) => {
                                ui.label(
                                    RichText::new(format!(
                                        "✔ The download is found: {}. Spotimy extracts it at the login.",
                                        p.display()
                                    ))
                                    .color(OK)
                                    .small(),
                                );
                            }
                            soloist::Located::Missing => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    ui.hyperlink_to("Download Soloist", soloist::DOWNLOAD_PAGE);
                                    ui.label(
                                        RichText::new(
                                            " from Spotify into your Downloads folder. Spotimy finds it \
                                             there and extracts it. No other step is necessary.",
                                        )
                                        .color(DIM),
                                    );
                                });
                                ui.label(
                                    RichText::new("Spotimy waits for the download…").color(BAD).small(),
                                );
                            }
                        }
                        egui::CollapsingHeader::new(RichText::new("Soloist is in a different folder").small())
                            .id_salt("soloist_path")
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.setup_path)
                                        .hint_text("Path of the soloist file or its folder")
                                        .desired_width(f32::INFINITY),
                                );
                            });
                        ui.add_space(12.0);

                        ui.label(RichText::new("3. Soloist API key").strong());
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.hyperlink_to("Create a Soloist API key", soloist::KEY_PAGE);
                            ui.label(
                                RichText::new(
                                    ". Spotimy gives it to Soloist one time and does not keep it. \
                                     Leave the field empty if Soloist has the key already.",
                                )
                                .color(DIM),
                            );
                        });
                        ui.add(
                            egui::TextEdit::singleline(&mut self.setup_key)
                                .password(true)
                                .hint_text("Soloist API key (optional)")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(16.0);

                        let ready = client_id.is_some();
                        let button = egui::Button::new(
                            RichText::new("Log in with Spotify").color(Color32::BLACK).strong(),
                        )
                        .fill(ACCENT)
                        .min_size(vec2(200.0, 36.0));
                        if ui.add_enabled(ready, button).clicked() {
                            let api_key = std::mem::take(&mut self.setup_key);
                            self.send(Command::Login(Setup {
                                client_id: client_id.clone().unwrap_or_default(),
                                soloist_path: self.setup_path.clone(),
                                api_key,
                            }));
                        }
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("A Spotify Premium account is necessary.")
                                .color(DIM)
                                .small(),
                        );
                    });
                });
            });
        });
    }

    // ------------------------------------------------------------- sidebar

    fn nav_item(&mut self, ui: &mut Ui, label: &str, key: PageKey) {
        let selected = std::mem::discriminant(&self.view.key) == std::mem::discriminant(&key)
            && !matches!(key, PageKey::Playlist(_));
        let (rect, resp) =
            ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
        if resp.hovered() || selected {
            ui.painter().rect_filled(rect, 6.0, HOVER);
        }
        let color = if selected { ACCENT } else { TEXT };
        line(ui, rect.shrink2(vec2(10.0, 0.0)), label, 14.5, color, Align2::LEFT_CENTER);
        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            if let PageKey::Search(_) = key {
                self.open(PageKey::Search(self.search.clone()));
            } else {
                self.open(key);
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("Spotimy").size(20.0).strong().color(ACCENT));
        });
        ui.add_space(8.0);
        self.nav_item(ui, "Home", PageKey::Home);
        self.nav_item(ui, "Search", PageKey::Search(String::new()));
        self.nav_item(ui, "Liked Songs", PageKey::Liked);
        self.nav_item(ui, "Albums", PageKey::Albums);
        ui.add_space(6.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("PLAYLISTS").size(11.0).color(DIM));
        });

        let mut clicked: Option<String> = None;
        let current = match &self.view.key {
            PageKey::Playlist(id) => Some(id.as_str()),
            _ => None,
        };
        let bottom = 34.0;
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - bottom)
            .auto_shrink([false, false])
            .show_rows(ui, 28.0, self.playlists.len(), |ui, range| {
                for p in &self.playlists[range] {
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                    let selected = current == Some(p.id.as_str());
                    if resp.hovered() || selected {
                        ui.painter().rect_filled(rect, 6.0, HOVER);
                    }
                    let color = if selected { ACCENT } else { DIM };
                    line(ui, rect.shrink2(vec2(10.0, 0.0)), &p.name, 13.5, color, Align2::LEFT_CENTER);
                    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        clicked = Some(p.id.clone());
                    }
                }
            });
        if let Some(id) = clicked {
            self.open(PageKey::Playlist(id));
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                let name = match &self.auth {
                    AuthState::LoggedIn { name } => name.clone(),
                    _ => String::new(),
                };
                if ui
                    .small_button("Log out")
                    .on_hover_text("Log out and show the setup page")
                    .clicked()
                {
                    self.send(Command::Logout);
                }
                ui.label(RichText::new(name).size(12.0).color(DIM));
            });
        });
    }

    // ----------------------------------------------------------- main page

    fn page(&mut self, ui: &mut Ui) {
        let is_search = matches!(self.view.key, PageKey::Search(_));

        ui.horizontal(|ui| {
            let can_back = !self.history.is_empty();
            if ui
                .add_enabled(can_back, egui::Button::new("←").frame(false))
                .on_hover_text("Back")
                .clicked()
            {
                self.back();
            }
            if is_search {
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search for songs, albums, artists, playlists")
                        .desired_width(ui.available_width().min(520.0)),
                );
                if self.search.is_empty() && !edit.has_focus() && self.view.data.tracks.is_empty() {
                    edit.request_focus();
                }
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && !self.search.trim().is_empty() {
                    let key = PageKey::Search(self.search.trim().to_owned());
                    self.view = View::new(key.clone());
                    self.load(key);
                }
            }
        });

        let mut play: Option<usize> = None;
        let mut play_context = false;
        let mut enqueue: Option<Track> = None;
        let mut go: Option<PageKey> = None;
        let mut card_click: Option<usize> = None;
        let mut want_more = false;

        let current_id = self.pb.track.as_ref().map(|t| t.id.clone());
        let scroll_id = format!("{:?}", self.view.key);

        egui::ScrollArea::vertical()
            .id_salt(scroll_id)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // ---- header
                let title = self.view.data.title.clone();
                let subtitle = self.view.data.subtitle.clone();
                let art = self.view.data.img_l.clone().filter(|u| !u.is_empty());
                if title.is_some() && !is_search {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if let Some(url) = &art {
                            let (rect, _) = ui.allocate_exact_size(vec2(132.0, 132.0), Sense::hover());
                            self.cover(ui, rect, url, COVER_PX, 8);
                            ui.add_space(8.0);
                        }
                        ui.vertical(|ui| {
                            if art.is_some() {
                                ui.add_space(34.0);
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(title.unwrap_or_default()).size(28.0).strong(),
                                )
                                .truncate(),
                            );
                            if let Some(sub) = subtitle {
                                ui.label(RichText::new(sub).color(DIM));
                            }
                            if self.view.context().is_some() {
                                ui.add_space(4.0);
                                let b = egui::Button::new(
                                    RichText::new("▶  Play").color(Color32::BLACK).strong(),
                                )
                                .fill(ACCENT);
                                if ui.add(b).clicked() {
                                    play_context = true;
                                }
                            }
                        });
                    });
                    ui.add_space(10.0);
                }

                if let Some(err) = &self.view.error {
                    ui.label(RichText::new(err).color(Color32::from_rgb(240, 120, 110)));
                    if self.view.context().is_some() {
                        ui.label(
                            RichText::new(
                                "Spotify does not give the track list of this item to a \
                                 Development Mode app. The Play button works.",
                            )
                            .color(DIM),
                        );
                    }
                }

                // ---- cards (albums, artists, playlists)
                let n_cards = self.view.data.cards.len();
                if n_cards > 0 {
                    let w = ui.available_width();
                    let cols = ((w / 250.0).floor() as usize).max(1);
                    let card_w = (w - (cols as f32 - 1.0) * 8.0) / cols as f32;
                    let rows = n_cards.div_ceil(cols);
                    let card_h = 60.0;
                    let (area, _) = ui.allocate_exact_size(
                        vec2(w, rows as f32 * (card_h + 8.0)),
                        Sense::hover(),
                    );
                    let clip = ui.clip_rect();
                    for i in 0..n_cards {
                        let (r, c) = (i / cols, i % cols);
                        let rect = Rect::from_min_size(
                            pos2(
                                area.left() + c as f32 * (card_w + 8.0),
                                area.top() + r as f32 * (card_h + 8.0),
                            ),
                            vec2(card_w, card_h),
                        );
                        if !clip.intersects(rect) {
                            continue;
                        }
                        let resp = ui.interact(rect, ui.scope_id().with(("card", i)), Sense::click());
                        let fill = if resp.hovered() { HOVER } else { PANEL };
                        ui.painter().rect_filled(rect, 8.0, fill);
                        let card = self.view.data.cards[i].clone();
                        let img = Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(48.0, 48.0));
                        let round = if card.kind == CardKind::Artist { 24 } else { 6 };
                        self.cover(ui, img, &card.img_s, THUMB_PX, round);
                        let text = Rect::from_min_max(
                            pos2(img.right() + 10.0, rect.top() + 9.0),
                            pos2(rect.right() - 8.0, rect.bottom() - 9.0),
                        );
                        let (top, bot) = text.split_top_bottom_at_fraction(0.5);
                        line(ui, top, &card.name, 14.0, TEXT, Align2::LEFT_CENTER);
                        line(ui, bot, &card.sub, 12.0, DIM, Align2::LEFT_CENTER);
                        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            card_click = Some(i);
                        }
                    }
                    ui.add_space(8.0);
                }

                // ---- track rows. Only the rows in view are drawn.
                let n = self.view.data.tracks.len();
                if n > 0 {
                    let w = ui.available_width();
                    let (area, _) = ui.allocate_exact_size(vec2(w, n as f32 * ROW_H), Sense::hover());
                    let clip = ui.clip_rect();
                    let first = (((clip.top() - area.top()) / ROW_H).floor().max(0.0)) as usize;
                    let last = ((((clip.bottom() - area.top()) / ROW_H).ceil()).max(0.0) as usize).min(n);
                    if last + 15 >= n {
                        want_more = true;
                    }
                    let show_album = w > 620.0 && !matches!(self.view.key, PageKey::Album(_));
                    for i in first..last {
                        let rect = Rect::from_min_size(
                            pos2(area.left(), area.top() + i as f32 * ROW_H),
                            vec2(w, ROW_H),
                        );
                        let resp = ui.interact(rect, ui.scope_id().with(("row", i)), Sense::click());
                        let t = self.view.data.tracks[i].clone();
                        let is_current = current_id.as_deref() == Some(t.id.as_str());
                        if resp.hovered() {
                            ui.painter().rect_filled(rect, 6.0, HOVER);
                        }
                        let title_color = if !t.playable {
                            DIM.linear_multiply(0.6)
                        } else if is_current {
                            ACCENT
                        } else {
                            TEXT
                        };

                        let num = Rect::from_min_size(rect.min, vec2(40.0, ROW_H));
                        let label = if resp.hovered() { "▶".to_owned() } else { (i + 1).to_string() };
                        ui.painter().text(
                            num.center(),
                            Align2::CENTER_CENTER,
                            label,
                            FontId::proportional(13.0),
                            if is_current { ACCENT } else { DIM },
                        );

                        let img = Rect::from_min_size(
                            pos2(num.right(), rect.top() + 6.0),
                            vec2(40.0, 40.0),
                        );
                        self.cover(ui, img, &t.img_s, THUMB_PX, 4);

                        let right = rect.right() - 12.0;
                        let dur = Rect::from_min_max(pos2(right - 48.0, rect.top()), pos2(right, rect.bottom()));
                        line(ui, dur, &fmt_time(t.duration_ms), 13.0, DIM, Align2::RIGHT_CENTER);

                        let text_left = img.right() + 12.0;
                        let text_right = dur.left() - 12.0;
                        let split = if show_album {
                            text_left + (text_right - text_left) * 0.58
                        } else {
                            text_right
                        };
                        let main = Rect::from_min_max(
                            pos2(text_left, rect.top() + 8.0),
                            pos2(split - 12.0, rect.bottom() - 8.0),
                        );
                        let (top, bot) = main.split_top_bottom_at_fraction(0.5);
                        line(ui, top, &t.name, 14.5, title_color, Align2::LEFT_CENTER);
                        line(ui, bot, &t.artists, 12.5, DIM, Align2::LEFT_CENTER);
                        if show_album {
                            let album = Rect::from_min_max(
                                pos2(split, rect.top()),
                                pos2(text_right, rect.bottom()),
                            );
                            line(ui, album, &t.album, 13.0, DIM, Align2::LEFT_CENTER);
                        }

                        if resp.double_clicked() && t.playable {
                            play = Some(i);
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Play").clicked() {
                                play = Some(i);
                                ui.close();
                            }
                            if ui.button("Add to queue").clicked() {
                                enqueue = Some(t.clone());
                                ui.close();
                            }
                            if !t.album_id.is_empty() && ui.button("Go to album").clicked() {
                                go = Some(PageKey::Album(t.album_id.clone()));
                                ui.close();
                            }
                            if !t.artist_id.is_empty() && ui.button("Go to artist").clicked() {
                                go = Some(PageKey::Artist(t.artist_id.clone()));
                                ui.close();
                            }
                        });
                    }
                } else if n_cards > 0 {
                    want_more = true;
                }

                if self.view.loading {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.spinner();
                    });
                } else if n == 0 && n_cards == 0 && self.view.error.is_none() {
                    ui.add_space(12.0);
                    let hint = if is_search { "Type a search and press Enter." } else { "Nothing is here." };
                    ui.label(RichText::new(hint).color(DIM));
                }
                ui.add_space(12.0);
            });

        // ---- actions (after the borrow of the page data ends)
        if want_more && !self.view.loading {
            if let Some(offset) = self.view.data.next {
                self.view.loading = true;
                self.send(Command::Load {
                    key: self.view.key.clone(),
                    offset,
                });
            }
        }
        if let Some(index) = play {
            self.send(Command::Play {
                context: self.view.context(),
                tracks: self.view.data.tracks.clone(),
                index,
            });
        }
        if play_context {
            // An index out of range starts the context at its first track.
            self.send(Command::Play {
                context: self.view.context(),
                tracks: Vec::new(),
                index: 0,
            });
        }
        if let Some(t) = enqueue {
            self.toast(format!("Added to queue: {}", t.name));
            self.send(Command::Enqueue(t));
        }
        if let Some(i) = card_click {
            let card = self.view.data.cards[i].clone();
            self.open_card(&card);
        }
        if let Some(key) = go {
            self.open(key);
        }
    }

    // ---------------------------------------------------------- player bar

    fn player_bar(&mut self, ui: &mut Ui) {
        let full = ui.max_rect();
        let side = (full.width() * 0.28).clamp(180.0, 340.0);
        let left = Rect::from_min_max(full.min, pos2(full.left() + side, full.bottom()));
        let right = Rect::from_min_max(pos2(full.right() - side, full.top()), full.max);
        let center = Rect::from_min_max(
            pos2(left.right() + 8.0, full.top()),
            pos2(right.left() - 8.0, full.bottom()),
        );

        // ---- left: the current track
        let track = self.pb.track.clone();
        if let Some(t) = &track {
            let img = Rect::from_min_size(pos2(left.left() + 4.0, left.center().y - 26.0), vec2(52.0, 52.0));
            self.cover(ui, img, &t.img_s, THUMB_PX, 6);
            let heart = Rect::from_min_size(pos2(left.right() - 30.0, left.center().y - 14.0), vec2(28.0, 28.0));
            let text = Rect::from_min_max(
                pos2(img.right() + 10.0, img.top() + 5.0),
                pos2(heart.left() - 4.0, img.bottom() - 5.0),
            );
            let (top, bot) = text.split_top_bottom_at_fraction(0.5);
            line(ui, top, &t.name, 14.0, TEXT, Align2::LEFT_CENTER);
            line(ui, bot, &t.artists, 12.0, DIM, Align2::LEFT_CENTER);

            let name_resp = ui
                .interact(text, ui.scope_id().with("np"), Sense::click())
                .on_hover_text("Go to album");
            if name_resp.clicked() && !t.album_id.is_empty() {
                self.open(PageKey::Album(t.album_id.clone()));
            }

            if let Some(liked) = self.pb.liked {
                let resp = ui
                    .interact(heart, ui.scope_id().with("heart"), Sense::click())
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text(if liked { "Remove from Liked Songs" } else { "Save to Liked Songs" });
                ui.painter().text(
                    heart.center(),
                    Align2::CENTER_CENTER,
                    if liked { "♥" } else { "♡" },
                    FontId::proportional(18.0),
                    if liked { ACCENT } else { DIM },
                );
                if resp.clicked() {
                    self.pb.liked = Some(!liked);
                    self.send(Command::Like {
                        id: t.id.clone(),
                        liked: !liked,
                    });
                }
            }
        }

        // ---- center: transport buttons and the seek bar
        let buttons_w = 3.0 * 36.0 + 2.0 * 29.0 + 4.0 * 6.0;
        let buttons = Rect::from_min_size(
            pos2(center.center().x - buttons_w / 2.0, center.top() + 6.0),
            vec2(buttons_w, 36.0),
        );
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(buttons)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let on = |b: bool| if b { ACCENT } else { DIM };
                if icon_button(ui, "🔀", 15.0, on(self.pb.shuffle), "Shuffle") {
                    self.send(Command::Shuffle(!self.pb.shuffle));
                }
                if shape_button(ui, Shape::Prev, 14.0, "Previous") {
                    self.send(Command::Prev);
                }
                let shape = if self.pb.playing { Shape::Pause } else { Shape::Play };
                if shape_button(ui, shape, 18.0, "Play or pause (Space)") {
                    self.send(Command::Toggle);
                }
                if shape_button(ui, Shape::Next, 14.0, "Next") {
                    self.send(Command::Next);
                }
                let (icon, tip, next) = match self.pb.repeat {
                    Repeat::Off => ("🔁", "Repeat: off", Repeat::All),
                    Repeat::All => ("🔁", "Repeat: all", Repeat::One),
                    Repeat::One => ("🔂", "Repeat: one track", Repeat::Off),
                };
                if icon_button(ui, icon, 15.0, on(self.pb.repeat != Repeat::Off), tip) {
                    self.send(Command::Repeat(next));
                }
            },
        );

        let dur = track.as_ref().map(|t| t.duration_ms).unwrap_or(0);
        let pos = self.pb.now_ms();
        let frac = match self.seek_drag {
            Some(f) => f,
            None if dur > 0 => pos as f32 / dur as f32,
            None => 0.0,
        };
        let bar_y = center.bottom() - 16.0;
        let bar = Rect::from_min_max(
            pos2(center.left() + 44.0, bar_y - 8.0),
            pos2(center.right() - 44.0, bar_y + 8.0),
        );
        let shown = (frac * dur as f32) as u32;
        line(
            ui,
            Rect::from_min_max(pos2(center.left(), bar.top()), pos2(bar.left() - 8.0, bar.bottom())),
            &fmt_time(shown),
            11.5,
            DIM,
            Align2::RIGHT_CENTER,
        );
        line(
            ui,
            Rect::from_min_max(pos2(bar.right() + 8.0, bar.top()), pos2(center.right(), bar.bottom())),
            &fmt_time(dur),
            11.5,
            DIM,
            Align2::LEFT_CENTER,
        );
        if bar.width() > 20.0 {
            let resp = ui.interact(bar, ui.scope_id().with("seek"), Sense::click_and_drag());
            let track_rect = Rect::from_min_max(pos2(bar.left(), bar_y - 2.0), pos2(bar.right(), bar_y + 2.0));
            ui.painter().rect_filled(track_rect, 2.0, Color32::from_rgb(60, 62, 72));
            let x = bar.left() + bar.width() * frac.clamp(0.0, 1.0);
            let active = resp.hovered() || resp.dragged();
            ui.painter().rect_filled(
                Rect::from_min_max(track_rect.min, pos2(x, track_rect.bottom())),
                2.0,
                if active { ACCENT } else { TEXT },
            );
            if active {
                ui.painter().circle_filled(pos2(x, bar_y), 6.0, TEXT);
            }
            if dur > 0 {
                if let Some(p) = resp.interact_pointer_pos() {
                    let f = ((p.x - bar.left()) / bar.width()).clamp(0.0, 1.0);
                    if resp.dragged() {
                        self.seek_drag = Some(f);
                    }
                    if resp.clicked() || resp.drag_stopped() {
                        self.seek_drag = None;
                        self.send(Command::Seek((f * dur as f32) as u32));
                    }
                } else if resp.drag_stopped() {
                    if let Some(f) = self.seek_drag.take() {
                        self.send(Command::Seek((f * dur as f32) as u32));
                    }
                }
            }
        }

        // ---- right: the volume and the player status
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(right.shrink2(vec2(8.0, 0.0)))
                .layout(Layout::right_to_left(Align::Center)),
            |ui| match self.player.clone() {
                PlayerStatus::Ready => {
                    ui.spacing_mut().slider_width = (right.width() - 80.0).clamp(60.0, 120.0);
                    let slider = egui::Slider::new(&mut self.volume, 0.0..=1.0)
                        .show_value(false)
                        .trailing_fill(true);
                    if ui.add(slider).changed() {
                        self.send(Command::Volume(self.volume));
                    }
                    ui.label(RichText::new("🔊").color(DIM));
                }
                PlayerStatus::Off => {}
                PlayerStatus::Starting => {
                    ui.label(RichText::new("The player starts…").color(DIM).small());
                    ui.spinner();
                }
                PlayerStatus::NeedsPairing => {
                    ui.label(RichText::new("Pair the player ⓘ").color(ACCENT).small())
                        .on_hover_text(format!(
                            "Do this one time: open the Spotify app on a phone or computer on the \
                             same network. Open the device list and select \"{}\".",
                            soloist::DEVICE_NAME
                        ));
                }
                PlayerStatus::Failed(why) => {
                    if ui.small_button("Retry").clicked() {
                        self.send(Command::RestartPlayer);
                    }
                    ui.label(
                        RichText::new("Player error ⓘ")
                            .color(Color32::from_rgb(240, 120, 110))
                            .small(),
                    )
                    .on_hover_text(why);
                }
            },
        );
    }
}

impl App {
    /// Shows what the user must do when the player is not ready.
    fn player_banner(&mut self, ui: &mut Ui) {
        let status = self.player.clone();
        if !matches!(status, PlayerStatus::NeedsPairing | PlayerStatus::Failed(_)) {
            return;
        }
        egui::Frame::new()
            .fill(Color32::from_rgb(46, 38, 22))
            .stroke(Stroke::new(1.0, ACCENT.linear_multiply(0.5)))
            .corner_radius(8.0)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match status {
                    PlayerStatus::NeedsPairing => {
                        ui.label(
                            RichText::new("One more step: pair the player with your Spotify account")
                                .strong()
                                .color(ACCENT),
                        );
                        ui.label(RichText::new("You do this one time. Keep Spotimy open.").color(DIM));
                        ui.add_space(4.0);
                        for step in [
                            "1. Connect your phone to the same Wi-Fi network as this computer.".to_owned(),
                            "2. Open the Spotify app on the phone and start a song.".to_owned(),
                            "3. Tap the device icon at the bottom of the Now Playing screen.".to_owned(),
                            format!("4. Select \"{}\" in the device list.", soloist::DEVICE_NAME),
                        ] {
                            ui.label(step);
                        }
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "This message goes away when the pairing is complete. If \"{}\" is not \
                                 in the list: wait 10 seconds, set VPN to off, and permit local network \
                                 access if the system asks. The Spotify app on this computer also works.",
                                soloist::DEVICE_NAME
                            ))
                            .color(DIM)
                            .small(),
                        );
                    }
                    PlayerStatus::Failed(why) => {
                        ui.label(
                            RichText::new("The audio player has a problem")
                                .strong()
                                .color(Color32::from_rgb(240, 120, 110)),
                        );
                        ui.label(why);
                        if ui.button("Try again").clicked() {
                            self.send(Command::RestartPlayer);
                        }
                    }
                    _ => {}
                }
            });
        ui.add_space(8.0);
    }

    /// Test hooks. `SPOTIMY_PAGE` opens a page at startup.
    /// `SPOTIMY_SHOT` saves a screenshot to the given path and stops the app.
    fn test_hooks(&mut self, ctx: &egui::Context) {
        if self.frame_no == 5 {
            match std::env::var("SPOTIMY_PAGE").as_deref() {
                Ok("playlist") => self.open(PageKey::Playlist("demo".into())),
                Ok("albums") => self.open(PageKey::Albums),
                Ok("search") => {
                    self.search = "night".into();
                    self.open(PageKey::Search("night".into()));
                }
                _ => {}
            }
        }
        let Some(path) = std::env::var_os("SPOTIMY_SHOT") else {
            return;
        };
        if self.frame_no < 40 {
            ctx.request_repaint();
        } else if self.frame_no == 40 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::ScreenshotCallback::new(move |img| {
                    let [w, h] = img.size;
                    let _ = image::save_buffer(
                        &path,
                        img.as_raw(),
                        w as u32,
                        h as u32,
                        image::ColorType::Rgba8,
                    );
                    std::process::exit(0);
                }),
            ));
            ctx.request_repaint();
        } else {
            ctx.request_repaint();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame_no += 1;
        let ctx = ui.ctx().clone();
        self.events(&ctx);
        self.test_hooks(&ctx);

        if !matches!(self.auth, AuthState::LoggedIn { .. }) {
            self.setup_screen(ui);
        } else {
            // Space toggles playback when no text field has the focus.
            if !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                self.send(Command::Toggle);
            }

            egui::Panel::bottom("player")
                .exact_size(84.0)
                .resizable(false)
                .frame(egui::Frame::new().fill(PANEL).inner_margin(8.0))
                .show(ui, |ui| self.player_bar(ui));
            egui::Panel::left("nav")
                .exact_size(220.0)
                .resizable(false)
                .frame(egui::Frame::new().fill(PANEL).inner_margin(6.0))
                .show(ui, |ui| self.sidebar(ui));
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
                .show(ui, |ui| {
                    self.player_banner(ui);
                    self.page(ui);
                });
        }

        // ---- toast message
        if let Some((text, at)) = &self.toast {
            let age = at.elapsed();
            if age > Duration::from_secs(6) {
                self.toast = None;
            } else {
                egui::Area::new(egui::Id::unique("toast"))
                    .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -100.0))
                    .show(&ctx, |ui| {
                        egui::Frame::new()
                            .fill(Color32::from_rgb(52, 54, 66))
                            .corner_radius(8.0)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.set_max_width(520.0);
                                ui.label(text.as_str());
                            });
                    });
                ctx.request_repaint_after(Duration::from_secs(6) - age);
            }
        }

        // Draw one time each second during playback, for the seek bar.
        // In all other conditions the app draws only after input or an event.
        if self.pb.playing && !self.pb.loading {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}
