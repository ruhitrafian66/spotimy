# Spotimy

A small native Spotify client for macOS (Apple Silicon) and Windows (x64), built to use far less
memory than the official desktop app.

The official app is a Chromium (CEF) application: several processes, around 1 GB in total.
Spotimy is one native process with a GPU-drawn UI, plus Spotify's own headless player for audio.

## How it works

| Part | What does it |
| --- | --- |
| Window and UI | Rust + [egui](https://github.com/emilk/egui) (OpenGL). No webview, no Chromium. |
| Library, search, playlists | [Spotify Web API](https://developer.spotify.com/documentation/web-api) with your own client ID (OAuth PKCE). |
| Audio playback | [Spotify Soloist](https://developer.spotify.com/documentation/soloist), Spotify's official headless player, started as a child process and controlled over its local WebSocket API. |

Spotimy does not decrypt or download audio itself and does not use reverse-engineered protocols.
Spotify closed those routes for third-party client IDs in 2026; Soloist is the supported one.

## Requirements

- Spotify **Premium**
- macOS 13+ on Apple Silicon, or Windows 10/11 x64
- Soloist 1.4.0 or newer (you download it yourself; Spotify does not allow redistribution)

## Setup (once)

1. **Client ID.** In the [Spotify developer dashboard](https://developer.spotify.com/dashboard)
   create an app, select *Web API*, and add the redirect URI `http://127.0.0.1:8898/login`.
   Copy the client ID.
2. **Soloist.** Download it from the
   [Soloist downloads page](https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates)
   and extract it. Spotimy looks next to its own executable, in `~/Downloads`, `~/soloist`, and on
   `PATH`; or give the path on the setup screen.
3. **Soloist API key.** Create one at <https://developer.spotify.com/dashboard/soloist>. Paste it on
   the setup screen (Spotimy passes it to `soloist setup-api-key` over stdin and does not store it),
   or run `soloist setup-api-key` yourself.
4. Start Spotimy, fill in the setup screen, and approve the login in the browser.
5. **Pair the player.** The first time, open the Spotify app on a phone or computer on the same
   network, open the device list, and pick **Spotimy**. Soloist remembers the session after that.

Soloist builds expire 90 days after their build date. When that happens Spotimy says so; download
a new build.

## Using it

- Double-click a track to play it. In a playlist or album, playback continues through it.
- Right-click a track: add to queue, go to album, go to artist.
- `Space` toggles play/pause. The heart saves the current track to Liked Songs.

## Build

```sh
# macOS: creates dist/Spotimy.app
scripts/bundle-macos.sh

# Windows, cross-compiled on macOS: creates dist/Spotimy.exe
scripts/build-windows.sh

# Windows, on a Windows PC: creates target\release\spotimy.exe
cargo build --release
```

`.github/workflows/build.yml` builds both on GitHub Actions and uploads them as artifacts.

Demo mode shows sample data without an account, for UI work and memory measurements:

```sh
SPOTIMY_DEMO=1 cargo run --release
```

## Memory

Measured on an 8 GB M-series Mac (`footprint`, release build, 1080×700 window on a Retina display):

| | Footprint |
| --- | --- |
| Official Spotify app (7 processes, idle) | ~1100 MB |
| Spotimy UI, demo data, 3000-track playlist open | ~100 MB |

About 58 MB of Spotimy's figure is the window's GPU surfaces; the app's own heap is about 15 MB.
Soloist's memory is extra and has not been measured here.

What keeps it low:

- no browser engine;
- the UI redraws only on input or player events (once a second while playing), so idle CPU is ~0%;
- track lists load 50 at a time and only visible rows are drawn;
- cover art is downscaled on arrival and capped at 240 small textures (least recently used go first);
- audio caching is on disk, inside Soloist, capped at 512 MB.

## Login security

The Web API login is the OAuth 2.0 authorization code flow with PKCE (S256), implemented in
`src/auth.rs`:

- no client secret exists anywhere; a fresh random `code_verifier` is made for each login and only
  its SHA-256 hash goes through the browser;
- the callback listener binds `127.0.0.1` only, accepts only `GET /login` with the matching random
  `state`, ignores everything else, and gives up after 5 minutes;
- the refresh token is stored in `session.json` (mode 0600 on macOS), is replaced when Spotify
  rotates it, and is deleted when Spotify reports it invalid. Spotify refresh tokens last 6 months.

PKCE removes the need for a secret, but not the need for your own client ID: a Development Mode
client ID works for at most 5 allowlisted users, so one shared ID cannot ship with the app.

## Limits

- Spotify's Development Mode rules (2026) apply to your client ID: search returns 10 results per
  request, artist "top tracks" is gone, and track lists of playlists you do not own may be
  refused. The **Play** button still plays those playlists.
- No podcasts, lyrics, or playlist editing yet.
- Files: `config.json` and `session.json` (the Web API refresh token, mode 0600 on macOS) live in
  the OS config folder under `spotimy`.
