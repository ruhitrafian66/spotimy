//! Link to Spotify Soloist, the official headless Spotify player.
//!
//! Soloist plays the audio. This app starts it as a child process and controls it
//! through its local WebSocket API (`--ws`). Spotify does not permit redistribution of
//! Soloist, thus the user downloads it from developer.spotify.com.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::mpsc::{UnboundedSender, unbounded_channel},
};
use tokio_tungstenite::tungstenite::Message;

use crate::model::Track;

/// The name of the Spotify Connect device that Soloist shows.
pub const DEVICE_NAME: &str = "Spotimy";
pub const DOWNLOAD_PAGE: &str =
    "https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates";
pub const KEY_PAGE: &str = "https://developer.spotify.com/dashboard/soloist";

/// The limit for the Soloist audio cache on disk, in MB.
const CACHE_MB: &str = "512";
const EXE: &str = if cfg!(windows) { "soloist.exe" } else { "soloist" };

pub enum SoloistMsg {
    /// A JSON message from Soloist.
    Json(Value),
    /// The link closed. `generation` identifies the link.
    Closed { generation: u64, reason: String },
}

pub struct Soloist {
    child: Option<Child>,
    tx: UnboundedSender<String>,
}

/// The data directory that Soloist uses by default.
fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        dirs::data_local_dir().map(|d| d.join("soloist").join("data"))
    } else {
        dirs::data_dir().map(|d| d.join("soloist"))
    }
}

/// Finds the Soloist executable. `configured` is the path from the settings.
pub fn find_exe(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim().trim_matches('"');
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        // The user can give the folder or the file.
        let p = if p.is_dir() { p.join(EXE) } else { p };
        return p.is_file().then_some(p);
    }
    let mut dirs_to_try: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs_to_try.push(d.to_owned());
        }
    }
    dirs_to_try.push(crate::auth::data_dir());
    if let Some(h) = dirs::home_dir() {
        dirs_to_try.push(h.join("soloist"));
        dirs_to_try.push(h.join(".local").join("bin"));
    }
    if let Some(d) = dirs::download_dir() {
        dirs_to_try.push(d.clone());
        dirs_to_try.push(d.join("soloist"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs_to_try.extend(std::env::split_paths(&path));
    }
    dirs_to_try
        .into_iter()
        .map(|d| d.join(EXE))
        .find(|p| p.is_file())
}

fn command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // Do not show a console window.
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Gives the API key to Soloist through stdin. Soloist keeps it for later starts.
pub async fn save_api_key(exe: &Path, key: &str) -> Result<(), String> {
    let mut child = command(exe)
        .arg("setup-api-key")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Soloist did not start: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{}\n", key.trim()).as_bytes()).await;
    }
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .map_err(|_| "Soloist did not accept the API key (timeout)".to_owned())?
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Soloist rejected the API key. Soloist 1.4.0 or newer is necessary.".into())
    }
}

async fn read_port(dir: &Path) -> Option<(String, u16)> {
    let port = tokio::fs::read_to_string(dir.join("ws.port")).await.ok()?;
    let port: u16 = port.trim().parse().ok()?;
    let addr = tokio::fs::read_to_string(dir.join("ws.addr"))
        .await
        .map(|a| a.trim().to_owned())
        .unwrap_or_default();
    let addr = if addr.is_empty() { "127.0.0.1".to_owned() } else { addr };
    Some((addr, port))
}

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn connect(dir: &Path) -> Option<Ws> {
    let (addr, port) = read_port(dir).await?;
    let url = format!("ws://{addr}:{port}");
    let fut = tokio_tungstenite::connect_async(url);
    match tokio::time::timeout(Duration::from_secs(3), fut).await {
        Ok(Ok((ws, _))) => Some(ws),
        _ => None,
    }
}

impl Soloist {
    /// Connects to a Soloist that runs already, or starts one.
    pub async fn start(
        exe: &Path,
        generation: u64,
        events: UnboundedSender<SoloistMsg>,
    ) -> Result<Self, String> {
        let dir = data_dir().ok_or("The Soloist data folder is not known")?;
        let mut child = None;
        let mut ws = connect(&dir).await;

        if ws.is_none() {
            // Stale port files are from a Soloist that stopped incorrectly.
            let _ = tokio::fs::remove_file(dir.join("ws.port")).await;
            let mut c = command(exe)
                .args(["--device-name", DEVICE_NAME])
                .args(["--ws", "127.0.0.1:0"])
                .args(["--cache-size", CACHE_MB])
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("Soloist did not start: {e}"))?;

            let deadline = Instant::now() + Duration::from_secs(15);
            while ws.is_none() {
                if let Ok(Some(status)) = c.try_wait() {
                    return Err(match status.code() {
                        Some(10) => "This Soloist build is expired. Download a new build.".into(),
                        _ => "Soloist stopped at startup. Make sure that the API key is saved \
                              and that no other Soloist runs."
                            .into(),
                    });
                }
                if Instant::now() > deadline {
                    return Err("Soloist did not open its control port".into());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                ws = connect(&dir).await;
            }
            child = Some(c);
        }

        let (mut sink, mut stream) = ws.expect("connected").split();
        let (tx, mut rx) = unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            let reason = loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(text.as_str()) {
                            if events.send(SoloistMsg::Json(v)).is_err() {
                                return;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break e.to_string(),
                    None => break "Soloist closed the link".to_owned(),
                }
            };
            let _ = events.send(SoloistMsg::Closed { generation, reason });
        });

        Ok(Self { child, tx })
    }

    pub fn send(&self, command: &str, mut fields: Value) {
        if !fields.is_object() {
            fields = json!({});
        }
        fields["type"] = "command".into();
        fields["command"] = command.into();
        let _ = self.tx.send(fields.to_string());
    }

    pub fn cmd(&self, command: &str) {
        self.send(command, Value::Null);
    }

    /// Stops the Soloist process, if this app started it.
    pub async fn stop(mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill().await;
        }
    }
}

// ---- Conversion of Soloist JSON ("entity" objects) ----

fn name(entity: &Value) -> String {
    entity
        .pointer("/decorations/identity/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn uri_id(entity: &Value) -> String {
    entity
        .get("uri")
        .and_then(Value::as_str)
        .and_then(|u| u.rsplit(':').next())
        .unwrap_or_default()
        .to_owned()
}

fn cover(entity: &Value, wanted: &[&str]) -> Option<String> {
    let covers = entity
        .pointer("/decorations/visual_identity/cover")
        .and_then(Value::as_array)?;
    let url = |c: &Value| c.get("url").and_then(Value::as_str).map(str::to_owned);
    for size in wanted {
        let hit = covers
            .iter()
            .find(|c| c.get("size").and_then(Value::as_str) == Some(size));
        if let Some(u) = hit.and_then(url) {
            return Some(u);
        }
    }
    covers.first().and_then(url)
}

/// Converts the `item` entity of a playback state to a track.
pub fn track(item: &Value) -> Option<Track> {
    let uri = item.get("uri").and_then(Value::as_str).unwrap_or_default();
    if uri.is_empty() {
        return None;
    }
    let null = Value::Null;
    let album = item.pointer("/decorations/parent/entity").unwrap_or(&null);
    let creators = item
        .pointer("/decorations/creators")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let pick = |sizes: &[&str]| {
        cover(item, sizes)
            .or_else(|| cover(album, sizes))
            .unwrap_or_default()
    };
    Some(Track {
        id: uri_id(item),
        name: name(item),
        artists: creators.iter().map(name).collect::<Vec<_>>().join(", "),
        artist_id: creators.first().map(uri_id).unwrap_or_default(),
        album: name(album),
        album_id: uri_id(album),
        duration_ms: item
            .pointer("/decorations/playback/duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        img_s: pick(&["small", "default"]),
        img_l: pick(&["default", "large", "small"]),
        playable: true,
    })
}

/// Calculates the position now from a Soloist `position` object.
pub fn position_now(position: &Value) -> Option<u32> {
    let pos = position.get("position_ms")?.as_f64()?;
    let stamp = position.get("timestamp_ms").and_then(Value::as_f64);
    let speed = position.get("speed").and_then(Value::as_f64).unwrap_or(0.0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    let extra = match stamp {
        // Ignore a clock difference that is not plausible.
        Some(t) if now >= t && now - t < 3_600_000.0 => (now - t) * speed,
        _ => 0.0,
    };
    Some((pos + extra).max(0.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(uri: &str, name: &str) -> Value {
        json!({ "uri": uri, "entity_type": "x", "decorations": { "identity": { "name": name } } })
    }

    #[test]
    fn converts_a_playback_item_to_a_track() {
        let mut item = entity("spotify:track:2fFlmlePz9hrVMv4LvdQxN", "Song");
        let mut album = entity("spotify:album:ALBUM1", "Record");
        album["decorations"]["visual_identity"] = json!({ "cover": [
            { "url": "https://img/large", "size": "large" },
            { "url": "https://img/small", "size": "small" },
            { "url": "https://img/default", "size": "default" },
        ]});
        item["decorations"]["parent"] = json!({ "entity": album });
        item["decorations"]["creators"] = json!([
            entity("spotify:artist:ART1", "First"),
            entity("spotify:artist:ART2", "Second"),
        ]);
        item["decorations"]["playback"] = json!({ "duration_ms": 201000 });

        let t = track(&item).expect("track");
        assert_eq!(t.id, "2fFlmlePz9hrVMv4LvdQxN");
        assert_eq!(t.uri(), "spotify:track:2fFlmlePz9hrVMv4LvdQxN");
        assert_eq!(t.name, "Song");
        assert_eq!(t.artists, "First, Second");
        assert_eq!(t.artist_id, "ART1");
        assert_eq!(t.album, "Record");
        assert_eq!(t.album_id, "ALBUM1");
        assert_eq!(t.duration_ms, 201000);
        assert_eq!(t.img_s, "https://img/small");
        assert_eq!(t.img_l, "https://img/default");
    }

    #[test]
    fn an_empty_item_is_not_a_track() {
        assert!(track(&json!({ "uri": "" })).is_none());
        assert!(track(&Value::Null).is_none());
    }

    #[test]
    fn position_uses_the_timestamp_and_the_speed() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64;
        let playing = json!({ "position_ms": 10_000, "timestamp_ms": now - 2_000.0, "speed": 1.0 });
        let p = position_now(&playing).unwrap();
        assert!((12_000..12_500).contains(&p), "{p}");

        let paused = json!({ "position_ms": 10_000, "timestamp_ms": now - 2_000.0, "speed": 0.0 });
        assert_eq!(position_now(&paused), Some(10_000));

        // A timestamp in the future (clock difference) adds nothing.
        let skew = json!({ "position_ms": 10_000, "timestamp_ms": now + 60_000.0, "speed": 1.0 });
        assert_eq!(position_now(&skew), Some(10_000));
    }

    #[test]
    fn commands_have_the_documented_shape() {
        let (tx, mut rx) = unbounded_channel();
        let s = Soloist { child: None, tx };
        s.cmd("pause");
        s.send("seek", json!({ "position_ms": 5 }));
        let a: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        let b: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(a, json!({ "type": "command", "command": "pause" }));
        assert_eq!(b, json!({ "type": "command", "command": "seek", "position_ms": 5 }));
    }
}
