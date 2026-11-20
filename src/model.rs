//! Plain data types shared between the backend and the UI.

use std::time::Instant;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub artists: String,
    pub artist_id: String,
    pub album: String,
    pub album_id: String,
    pub duration_ms: u32,
    /// Small cover (about 64 px).
    pub img_s: String,
    /// Medium cover (about 300 px).
    pub img_l: String,
    pub playable: bool,
}

impl Track {
    pub fn uri(&self) -> String {
        format!("spotify:track:{}", self.id)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Card {
    pub kind: CardKind,
    pub id: String,
    pub name: String,
    pub sub: String,
    pub img_s: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CardKind {
    #[default]
    Album,
    Artist,
    Playlist,
}

/// Identifies one screen of content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageKey {
    Home,
    Liked,
    Albums,
    Playlist(String),
    Album(String),
    Artist(String),
    Search(String),
}

/// One batch of content for a page. Pages load in batches to keep memory low.
#[derive(Clone, Debug, Default)]
pub struct PageData {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub img_l: Option<String>,
    pub tracks: Vec<Track>,
    pub cards: Vec<Card>,
    /// Offset of the next batch, if there is one.
    pub next: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Debug)]
pub struct Playback {
    pub track: Option<Track>,
    pub playing: bool,
    pub loading: bool,
    pub position_ms: u32,
    /// The time at which `position_ms` was correct.
    pub at: Instant,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub liked: Option<bool>,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            track: None,
            playing: false,
            loading: false,
            position_ms: 0,
            at: Instant::now(),
            shuffle: false,
            repeat: Repeat::Off,
            liked: None,
        }
    }
}

impl Playback {
    /// The current position, extrapolated from the last player event.
    pub fn now_ms(&self) -> u32 {
        let dur = self.track.as_ref().map(|t| t.duration_ms).unwrap_or(0);
        let pos = if self.playing && !self.loading {
            self.position_ms
                .saturating_add(self.at.elapsed().as_millis() as u32)
        } else {
            self.position_ms
        };
        pos.min(dur)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AuthState {
    /// The app checks for a saved login.
    Starting,
    LoggedOut,
    /// The browser is open; the app waits for the user.
    WaitingForBrowser,
    Connecting,
    LoggedIn { name: String },
}

/// The state of the Soloist player process.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerStatus {
    Off,
    Starting,
    /// Soloist runs, but no Spotify account is paired with it.
    NeedsPairing,
    Ready,
    Failed(String),
}

pub struct Setup {
    pub client_id: String,
    pub soloist_path: String,
    /// Optional. The app gives it to `soloist setup-api-key` and does not keep it.
    pub api_key: String,
}

pub enum Command {
    Login(Setup),
    /// Stops a login that waits for the browser.
    CancelLogin,
    Logout,
    RestartPlayer,
    Load { key: PageKey, offset: u32 },
    LoadPlaylists,
    Image { url: String, px: u32 },
    /// Plays `tracks[index]`. With a `context` (album or playlist URI), Spotify continues in it.
    Play { context: Option<String>, tracks: Vec<Track>, index: usize },
    Enqueue(Track),
    Toggle,
    Next,
    Prev,
    Seek(u32),
    Volume(f32),
    Shuffle(bool),
    Repeat(Repeat),
    Like { id: String, liked: bool },
}

pub enum Event {
    Auth(AuthState),
    Player(PlayerStatus),
    Page { key: PageKey, offset: u32, data: PageData },
    PageFailed { key: PageKey, error: String },
    Playlists(Vec<Card>),
    Image { url: String, size: [usize; 2], rgba: Vec<u8> },
    Playback(Playback),
    Volume(f32),
    Error(String),
}
