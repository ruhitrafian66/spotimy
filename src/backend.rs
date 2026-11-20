//! The background thread: login, Web API calls, and control of the Soloist player.

use std::{
    sync::{
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use eframe::egui;
use serde_json::{Value, json};
use tokio::sync::{
    Mutex, Semaphore,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::{
    api::Api,
    auth::{self, Auth, Config},
    demo,
    model::{AuthState, Command, Event, Playback, PlayerStatus, Repeat, Setup, Track},
    soloist::{self, DEVICE_NAME, Soloist, SoloistMsg},
};

/// The maximum number of track URIs in one "start playback" request.
const MAX_URIS: usize = 100;

#[derive(Clone)]
struct Out {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Out {
    fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
}

/// Results of tasks that must change the core state.
enum Internal {
    Login(Result<Auth, String>),
    Liked { id: String, liked: Option<bool> },
    PlayFailed { error: String, fallback_uri: Option<String> },
}

pub fn spawn(ctx: egui::Context) -> (UnboundedSender<Command>, Receiver<Event>, JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (ev_tx, ev_rx) = channel();
    let out = Out { tx: ev_tx, ctx };
    let handle = std::thread::Builder::new()
        .name("backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(4)
                .thread_stack_size(1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(Core::new(out).run(cmd_rx));
        })
        .expect("backend thread");
    (cmd_tx, ev_rx, handle)
}

struct Core {
    out: Out,
    http: reqwest::Client,
    cfg: Config,
    api: Option<Api>,
    images: Arc<Semaphore>,
    int_tx: UnboundedSender<Internal>,
    sol_tx: UnboundedSender<SoloistMsg>,

    soloist: Option<Soloist>,
    generation: u64,
    paired: bool,
    active: bool,
    /// The Spotify Connect ID of the Soloist device.
    device_id: Arc<Mutex<Option<String>>>,
    pb: Playback,
}

impl Core {
    fn new(out: Out) -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        // The two channels are replaced in `run`.
        let (int_tx, _) = unbounded_channel();
        let (sol_tx, _) = unbounded_channel();
        Self {
            out,
            http,
            cfg: Config::load(),
            api: None,
            images: Arc::new(Semaphore::new(6)),
            int_tx,
            sol_tx,
            soloist: None,
            generation: 0,
            paired: false,
            active: false,
            device_id: Arc::new(Mutex::new(None)),
            pb: Playback::default(),
        }
    }

    async fn run(mut self, mut cmds: UnboundedReceiver<Command>) {
        let (int_tx, mut int_rx) = unbounded_channel();
        let (sol_tx, mut sol_rx) = unbounded_channel();
        self.int_tx = int_tx;
        self.sol_tx = sol_tx;
        self.startup().await;
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => break, // The UI closed.
                },
                Some(msg) = int_rx.recv() => self.internal(msg).await,
                Some(msg) = sol_rx.recv() => self.soloist_msg(msg),
            }
        }
        if let Some(s) = self.soloist.take() {
            s.stop().await;
        }
    }

    async fn startup(&mut self) {
        if demo::enabled() {
            self.out.send(Event::Auth(AuthState::LoggedIn { name: "Demo user".into() }));
            self.out.send(Event::Player(PlayerStatus::Ready));
            self.pb.track = Some(demo::track(6));
            self.pb.liked = Some(true);
            self.set_position(42_000);
            self.emit();
            return;
        }
        if self.cfg.client_id.is_empty() {
            self.out.send(Event::Auth(AuthState::LoggedOut));
            return;
        }
        self.out.send(Event::Auth(AuthState::Starting));
        match Auth::restore(self.http.clone(), &self.cfg.client_id).await {
            Ok(Some(auth)) => self.logged_in(auth).await,
            Ok(None) => self.out.send(Event::Auth(AuthState::LoggedOut)),
            Err(e) => {
                log::warn!("saved login failed: {e}");
                self.out.send(Event::Auth(AuthState::LoggedOut));
            }
        }
    }

    async fn logged_in(&mut self, auth: Auth) {
        self.out.send(Event::Auth(AuthState::Connecting));
        let api = Api {
            http: self.http.clone(),
            auth: Arc::new(Mutex::new(auth)),
        };
        let name = match api.display_name().await {
            Ok(n) => n,
            Err(e) => {
                self.out.send(Event::Error(e));
                self.out.send(Event::Auth(AuthState::LoggedOut));
                return;
            }
        };
        self.api = Some(api);
        self.out.send(Event::Auth(AuthState::LoggedIn { name }));
        self.start_player().await;
    }

    async fn start_player(&mut self) {
        if let Some(old) = self.soloist.take() {
            old.stop().await;
        }
        self.paired = false;
        self.active = false;
        *self.device_id.lock().await = None;
        let Some(exe) = soloist::find_exe(&self.cfg.soloist_path) else {
            self.out.send(Event::Player(PlayerStatus::Failed(
                "Soloist was not found. Download it and set its path.".into(),
            )));
            return;
        };
        self.out.send(Event::Player(PlayerStatus::Starting));
        self.generation += 1;
        match Soloist::start(&exe, self.generation, self.sol_tx.clone()).await {
            // Soloist sends its auth state immediately. That message sets the status.
            Ok(s) => self.soloist = Some(s),
            Err(e) => self.out.send(Event::Player(PlayerStatus::Failed(e))),
        }
    }

    fn emit(&self) {
        self.out.send(Event::Playback(self.pb.clone()));
    }

    fn set_position(&mut self, ms: u32) {
        self.pb.position_ms = ms;
        self.pb.at = Instant::now();
    }

    fn sol(&self) -> Option<&Soloist> {
        if self.soloist.is_none() || !self.paired {
            self.out.send(Event::Error(
                "The player is not ready. See the status at the bottom.".into(),
            ));
            return None;
        }
        self.soloist.as_ref()
    }

    fn check_liked(&self, id: String) {
        let Some(api) = self.api.clone() else { return };
        let tx = self.int_tx.clone();
        tokio::spawn(async move {
            let liked = api.is_liked(&id).await;
            let _ = tx.send(Internal::Liked { id, liked });
        });
    }

    fn set_track(&mut self, item: &Value) {
        let new = soloist::track(item);
        let changed = new.as_ref().map(|t| &t.id) != self.pb.track.as_ref().map(|t| &t.id);
        if changed {
            self.pb.liked = None;
            if let Some(t) = &new {
                self.check_liked(t.id.clone());
            }
        }
        // Keep the old track on an empty item: Soloist sends one between two tracks.
        if new.is_some() {
            self.pb.track = new;
        }
    }

    fn set_status(&mut self, status: &str) {
        self.pb.playing = matches!(status, "playing" | "buffering");
        self.pb.loading = status == "buffering";
    }

    fn set_options(&mut self, options: &Value) {
        if let Some(s) = options.get("shuffle").and_then(Value::as_bool) {
            self.pb.shuffle = s;
        }
        self.pb.repeat = match options.get("repeat").and_then(Value::as_str) {
            Some("context") => Repeat::All,
            Some("track") => Repeat::One,
            Some(_) => Repeat::Off,
            None => self.pb.repeat,
        };
    }

    fn set_auth(&mut self, v: &Value) {
        if let Some(a) = v.get("is_active").and_then(Value::as_bool) {
            self.active = a;
        }
        let Some(logged_in) = v.get("logged_in").and_then(Value::as_bool) else {
            return;
        };
        let was = self.paired;
        self.paired = logged_in;
        self.out.send(Event::Player(if logged_in {
            PlayerStatus::Ready
        } else {
            PlayerStatus::NeedsPairing
        }));
        if logged_in && !was {
            if let Some(s) = &self.soloist {
                s.cmd("get_state");
            }
        }
    }

    fn soloist_msg(&mut self, msg: SoloistMsg) {
        let v = match msg {
            SoloistMsg::Json(v) => v,
            SoloistMsg::Closed { generation, reason } => {
                if generation == self.generation {
                    self.soloist = None;
                    self.paired = false;
                    self.pb.playing = false;
                    self.pb.loading = false;
                    self.emit();
                    self.out
                        .send(Event::Player(PlayerStatus::Failed(format!("Soloist stopped: {reason}"))));
                }
                return;
            }
        };
        let null = Value::Null;
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "auth_state" => self.set_auth(&v),
            "device_changed" => {
                if let Some(a) = v.get("is_active").and_then(Value::as_bool) {
                    self.active = a;
                }
            }
            "playback_state" => {
                if let Some(a) = v.get("is_active").and_then(Value::as_bool) {
                    self.active = a;
                }
                self.set_track(v.get("item").unwrap_or(&null));
                self.set_status(v.get("status").and_then(Value::as_str).unwrap_or("idle"));
                self.set_options(v.get("options").unwrap_or(&null));
                if let Some(ms) = v.get("position").and_then(soloist::position_now) {
                    self.set_position(ms);
                }
                if let Some(vol) = v.get("volume").and_then(Value::as_f64) {
                    self.out.send(Event::Volume((vol / 100.0) as f32));
                }
                self.emit();
            }
            "track_changed" => {
                self.set_track(v.get("item").unwrap_or(&null));
                self.set_position(0);
                self.emit();
            }
            "playback_changed" => {
                // Freeze the position before the state changes.
                let now = self.pb.now_ms();
                self.set_position(now);
                self.set_status(v.get("status").and_then(Value::as_str).unwrap_or("idle"));
                self.emit();
            }
            "options_changed" => {
                self.set_options(v.get("options").unwrap_or(&null));
                self.emit();
            }
            "position_sync" => {
                if let Some(ms) = v.get("position").and_then(soloist::position_now) {
                    self.set_position(ms);
                    self.emit();
                }
            }
            "volume_changed" => {
                if let Some(vol) = v.get("volume").and_then(Value::as_f64) {
                    self.out.send(Event::Volume((vol / 100.0) as f32));
                }
            }
            "error" => {
                let m = v.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                self.out.send(Event::Error(format!("Player: {m}")));
            }
            _ => {}
        }
    }

    async fn login(&mut self, setup: Setup) {
        self.cfg.client_id = setup.client_id.trim().to_owned();
        self.cfg.soloist_path = setup.soloist_path.trim().to_owned();
        self.cfg.save();

        let key = setup.api_key.trim();
        if !key.is_empty() {
            match soloist::find_exe(&self.cfg.soloist_path) {
                Some(exe) => {
                    if let Err(e) = soloist::save_api_key(&exe, key).await {
                        self.out.send(Event::Error(e));
                    }
                }
                None => self.out.send(Event::Error(
                    "Soloist was not found, thus the API key was not saved.".into(),
                )),
            }
        }

        self.out.send(Event::Auth(AuthState::WaitingForBrowser));
        let id = self.cfg.client_id.clone();
        let tx = self.int_tx.clone();
        let http = self.http.clone();
        tokio::spawn(async move {
            let _ = tx.send(Internal::Login(Auth::interactive(http, &id).await));
        });
    }

    fn play(&mut self, context: Option<String>, tracks: Vec<Track>, index: usize) {
        let Some(api) = self.api.clone() else { return };
        if self.sol().is_none() {
            return;
        }
        let first = tracks.get(index).cloned();
        let body = match (&context, &first) {
            (Some(c), Some(t)) => json!({ "context_uri": c, "offset": { "uri": t.uri() } }),
            (Some(c), None) => json!({ "context_uri": c }),
            (None, Some(_)) => {
                let uris: Vec<String> = tracks
                    .iter()
                    .skip(index)
                    .filter(|t| t.playable)
                    .take(MAX_URIS)
                    .map(Track::uri)
                    .collect();
                json!({ "uris": uris })
            }
            (None, None) => return,
        };

        // Show the new track immediately.
        if let Some(t) = &first {
            self.pb.track = Some(t.clone());
            self.pb.liked = None;
            self.check_liked(t.id.clone());
        }
        self.pb.playing = true;
        self.pb.loading = true;
        self.set_position(0);
        self.emit();

        let fallback_uri = first.map(|t| t.uri()).or(context);
        let device = self.device_id.clone();
        let tx = self.int_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut dev = device.lock().await;
                if dev.is_none() {
                    *dev = api.device_id(DEVICE_NAME).await?;
                }
                let id = dev
                    .clone()
                    .ok_or("Spotify does not list the Spotimy device yet")?;
                drop(dev);
                if let Err(e) = api.play(&id, &body).await {
                    // The device ID can change after a new pairing.
                    *device.lock().await = None;
                    return Err(e);
                }
                Ok::<(), String>(())
            }
            .await;
            if let Err(error) = result {
                let _ = tx.send(Internal::PlayFailed { error, fallback_uri });
            }
        });
    }

    /// Answers a command with sample data.
    fn demo_command(&mut self, cmd: Command) {
        match cmd {
            Command::Load { key, offset } => {
                let data = demo::page(&key, offset);
                self.out.send(Event::Page { key, offset, data });
            }
            Command::LoadPlaylists => self.out.send(Event::Playlists(demo::playlists())),
            Command::Image { url, px } => {
                let (size, rgba) = demo::image(&url, px);
                self.out.send(Event::Image { url, size, rgba });
            }
            Command::Play { tracks, index, .. } => {
                if let Some(t) = tracks.get(index) {
                    self.pb.track = Some(t.clone());
                    self.pb.playing = true;
                    self.set_position(0);
                    self.emit();
                }
            }
            Command::Toggle => {
                let now = self.pb.now_ms();
                self.set_position(now);
                self.pb.playing = !self.pb.playing;
                self.emit();
            }
            Command::Seek(ms) => {
                self.set_position(ms);
                self.emit();
            }
            _ => {}
        }
    }

    async fn command(&mut self, cmd: Command) {
        if demo::enabled() {
            return self.demo_command(cmd);
        }
        match cmd {
            Command::Login(setup) => self.login(setup).await,
            Command::Logout => {
                auth::forget();
                if let Some(s) = self.soloist.take() {
                    s.cmd("pause");
                    s.stop().await;
                }
                self.api = None;
                self.paired = false;
                self.pb = Playback::default();
                self.emit();
                self.out.send(Event::Player(PlayerStatus::Off));
                self.out.send(Event::Auth(AuthState::LoggedOut));
            }
            Command::RestartPlayer => self.start_player().await,
            Command::Load { key, offset } => {
                let Some(api) = self.api.clone() else { return };
                let out = self.out.clone();
                tokio::spawn(async move {
                    match api.page(&key, offset).await {
                        Ok(data) => out.send(Event::Page { key, offset, data }),
                        Err(error) => out.send(Event::PageFailed { key, error }),
                    }
                });
            }
            Command::LoadPlaylists => {
                let Some(api) = self.api.clone() else { return };
                let out = self.out.clone();
                tokio::spawn(async move {
                    match api.playlists().await {
                        Ok(list) => out.send(Event::Playlists(list)),
                        Err(e) => out.send(Event::Error(e)),
                    }
                });
            }
            Command::Image { url, px } => {
                let Some(api) = self.api.clone() else { return };
                let out = self.out.clone();
                let sem = self.images.clone();
                tokio::spawn(async move {
                    let _permit = sem.acquire().await;
                    match api.image(&url, px).await {
                        Ok((size, rgba)) => out.send(Event::Image { url, size, rgba }),
                        Err(e) => log::debug!("image {url}: {e}"),
                    }
                });
            }
            Command::Play { context, tracks, index } => self.play(context, tracks, index),
            Command::Enqueue(track) => {
                if let Some(s) = self.sol() {
                    s.send("add_to_queue", json!({ "uri": track.uri() }));
                }
            }
            Command::Toggle => {
                let Some(s) = self.sol() else { return };
                if self.pb.playing {
                    s.cmd("pause");
                } else {
                    if !self.active {
                        s.cmd("activate");
                    }
                    s.cmd("play");
                }
            }
            Command::Next => {
                if let Some(s) = self.sol() {
                    s.cmd("skip_next");
                }
            }
            Command::Prev => {
                if let Some(s) = self.sol() {
                    s.cmd("skip_prev");
                }
            }
            Command::Seek(ms) => {
                if let Some(s) = self.sol() {
                    s.send("seek", json!({ "position_ms": ms }));
                    self.set_position(ms);
                    self.emit();
                }
            }
            Command::Volume(v) => {
                if let Some(s) = &self.soloist {
                    let vol = (v.clamp(0.0, 1.0) * 100.0).round() as u32;
                    s.send("set_volume", json!({ "volume": vol }));
                }
            }
            Command::Shuffle(on) => {
                if let Some(s) = self.sol() {
                    s.send("set_shuffle", json!({ "enabled": on }));
                }
            }
            Command::Repeat(mode) => {
                if let Some(s) = self.sol() {
                    let set = |cmd: &str, on: bool| s.send(cmd, json!({ "enabled": on }));
                    // The order is from the Soloist documentation.
                    match mode {
                        Repeat::Off => {
                            set("set_repeat_track", false);
                            set("set_repeat_context", false);
                        }
                        Repeat::All => {
                            set("set_repeat_track", false);
                            set("set_repeat_context", true);
                        }
                        Repeat::One => {
                            set("set_repeat_context", false);
                            set("set_repeat_track", true);
                        }
                    }
                }
            }
            Command::Like { id, liked } => {
                let Some(api) = self.api.clone() else { return };
                let tx = self.int_tx.clone();
                let out = self.out.clone();
                tokio::spawn(async move {
                    match api.set_liked(&id, liked).await {
                        Ok(()) => {
                            let _ = tx.send(Internal::Liked { id, liked: Some(liked) });
                        }
                        Err(e) => out.send(Event::Error(e)),
                    }
                });
            }
        }
    }

    async fn internal(&mut self, msg: Internal) {
        match msg {
            Internal::Login(Ok(auth)) => self.logged_in(auth).await,
            Internal::Login(Err(e)) => {
                self.out.send(Event::Error(format!("Login failed: {e}")));
                self.out.send(Event::Auth(AuthState::LoggedOut));
            }
            Internal::Liked { id, liked } => {
                if self.pb.track.as_ref().is_some_and(|t| t.id == id) {
                    self.pb.liked = liked;
                    self.emit();
                }
            }
            Internal::PlayFailed { error, fallback_uri } => {
                // The Web API did not start playback. Ask Soloist directly.
                // Soloist accepts one URI and no start position in a list.
                log::warn!("web api play failed: {error}");
                match (&self.soloist, fallback_uri) {
                    (Some(s), Some(uri)) if self.paired => {
                        if !self.active {
                            s.cmd("activate");
                        }
                        s.send("play", json!({ "uri": uri }));
                    }
                    _ => {
                        self.pb.playing = false;
                        self.pb.loading = false;
                        self.emit();
                        self.out.send(Event::Error(error));
                    }
                }
            }
        }
    }
}
