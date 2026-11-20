# Spotimy

A lightweight native Spotify client for macOS (Apple Silicon) and Windows (x64). The purpose of
the project is low memory use: one Rust process with an egui/OpenGL window, no webview.

## Commands

Rust is not on PATH by default on the owner's Mac. Run `source ~/.cargo/env` first.

```sh
cargo test --release             # unit tests (16)
cargo build --release            # macOS binary: target/release/spotimy
scripts/bundle-macos.sh          # dist/Spotimy.app (ad-hoc signed)
scripts/build-windows.sh         # dist/Spotimy.exe, cross-compiled; needs mingw-w64
SPOTIMY_DEMO=1 target/release/spotimy   # sample data, no account necessary
```

- Use `--release` for everything. A debug build adds about 1 GB to `target/`, and the owner's disk
  is nearly full. Check `df -h` before a large build or install.
- After a Windows cross-build, delete `target/x86_64-pc-windows-gnu` once the `.exe` is in `dist/`.
- `.github/workflows/build.yml` builds and tests both platforms natively on push.

## Architecture

Two threads that talk through channels. The UI never blocks on the network.

| File | Role |
| --- | --- |
| `src/main.rs` | Window setup. Joins the backend thread on exit so that Soloist is stopped. |
| `src/ui.rs` | The egui `App`. Sends `Command`, receives `Event`. All drawing is here. |
| `src/backend.rs` | `Core`: a tokio runtime on its own thread. Handles commands, owns login state and the Soloist link. |
| `src/api.rs` | Spotify Web API client. Converts JSON to `model` types. |
| `src/auth.rs` | OAuth 2.0 authorization code flow with PKCE, token refresh, config and token files. |
| `src/soloist.rs` | Starts Spotify Soloist as a child process and speaks its local WebSocket API. |
| `src/model.rs` | Plain data types shared by UI and backend: `Track`, `PageKey`, `PageData`, `Playback`, `Command`, `Event`. |
| `src/demo.rs` | Sample data for `SPOTIMY_DEMO=1`. |

Playback has two paths, on purpose:

- **Start playback:** Web API `PUT /me/player/play` with the Soloist device ID, because it accepts a
  context URI plus a start track, or a list of track URIs. Fallback: Soloist `play` with one URI.
- **Everything else** (pause, next, previous, seek, volume, shuffle, repeat, queue add, and all
  state): the Soloist WebSocket. Soloist pushes state events, so the app never polls.

## Rules that are not obvious from the code

- **Audio comes only from Spotify Soloist**, Spotify's official headless player. Do not add
  librespot or any other reverse-engineered playback, and do not use Spotify's own client IDs.
  Spotify blocked those routes on purpose in 2026; working around the block is out of scope.
- **Do not bundle or redistribute Soloist.** Spotify's terms forbid it, and ask that users be sent
  to the download page. The user downloads it; the app only finds it (also as an archive in
  Downloads, which it extracts) and starts it. Do not make the app fetch Soloist by itself. Do not store the Soloist API key: pass it once to
  `soloist setup-api-key` over stdin.
- **The Web API follows the February 2026 Development Mode rules.** Use `/playlists/{id}/items`
  (field `item`, with `track` as the old fallback), `/me/library` for save/remove/contains, and a
  search `limit` of 10 or less. Batch endpoints, `/artists/{id}/top-tracks` and `/browse/*` are
  gone. Check Spotify's changelog before adding an endpoint.
- **The redirect URI is fixed:** `http://127.0.0.1:8898/login`. Users register exactly this string,
  so a change breaks every existing setup. Spotify does not accept `localhost`.
- **The OAuth callback must keep its `state` check**, its `GET /login`-only rule and its timeout.
  Tests in `auth.rs` cover them.
- **Tolerate missing JSON fields.** Both Spotify APIs changed field names in 2026, so parsing uses
  `serde_json::Value` with defaults, not strict structs.

## Memory rules

Memory use is the product's main feature. Keep these properties when changing the UI:

- Draw only on input or on a backend event. The only timer is one repaint per second while a track
  plays. Do not add continuous repaints or animations that run when idle.
- Long lists load 50 items at a time and draw only the visible rows (manual culling in
  `App::page`). Do not lay out every row.
- Cover images are downscaled in the backend before they reach the UI, and the texture cache is
  capped (`MAX_TEXTURES`, least recently used are evicted first).
- Check the effect of a UI change with `footprint $(pgrep -x spotimy)` in demo mode. The baseline
  is about 100 MB, of which about 58 MB is the window's GPU surfaces.

## Verifying changes

- No Spotify account is available in automated work. The login, the Web API calls and the Soloist
  link have unit tests for their parsing and protocol shapes only. Say clearly when a change was
  not tested against the real service.
- To check the UI without screen-recording permission, let the app save its own screenshot:

  ```sh
  SPOTIMY_DEMO=1 SPOTIMY_PAGE=playlist SPOTIMY_SHOT=/path/out.png target/release/spotimy
  ```

  `SPOTIMY_PAGE` accepts `playlist`, `albums` or `search`. The app exits after it saves the image.
- egui is version 0.37, whose API differs from older examples: `App::ui(&mut self, ui, frame)`,
  `Panel::left/bottom(...).show(ui, ...)`, `Ui::scope_id()`, `Frame::new()`. Read the crate source
  in `~/.cargo/registry` when unsure.
- The Windows binary cannot be run on the owner's Mac. `cargo check --release --target
  x86_64-pc-windows-gnu` type-checks the Windows-only code without the mingw toolchain.

## Files outside the repo

- Config: `~/Library/Application Support/spotimy/config.json` (macOS), `%APPDATA%\spotimy` (Windows).
- Web API refresh token: `session.json` in the same folder. Never print or commit its content.
- Soloist uses its own default data folder (`~/Library/Application Support/soloist`,
  `%LOCALAPPDATA%\soloist\data`). The app reads `ws.port` and `ws.addr` there to find the socket.
