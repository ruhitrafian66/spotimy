//! Spotify Web API client.
//!
//! The paths follow the February 2026 Development Mode rules:
//! `/playlists/{id}/items`, `/me/library`, search `limit` <= 10, no batch endpoints.

use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    auth::Auth,
    model::{Card, CardKind, PageData, PageKey, Track},
};

const BASE: &str = "https://api.spotify.com/v1";
const PAGE: u32 = 50;

#[derive(Clone)]
pub struct Api {
    pub http: reqwest::Client,
    pub auth: Arc<Mutex<Auth>>,
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Picks the smallest image that is at least `min` pixels wide.
/// Spotify sorts images from the widest to the narrowest.
fn pick_image(v: &Value, min: u64) -> String {
    let Some(images) = v.get("images").and_then(Value::as_array) else {
        return String::new();
    };
    let mut best: Option<&Value> = None;
    for img in images {
        let w = img.get("width").and_then(Value::as_u64).unwrap_or(0);
        if w >= min || best.is_none() {
            best = Some(img);
        }
    }
    best.map(|i| s(i, "url")).unwrap_or_default()
}

fn artist_names(v: &Value) -> (String, String) {
    let Some(arr) = v.get("artists").and_then(Value::as_array) else {
        return (String::new(), String::new());
    };
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .collect();
    let first_id = arr.first().map(|a| s(a, "id")).unwrap_or_default();
    (names.join(", "), first_id)
}

/// Converts a track object. `album` supplies the album fields when the track has none.
fn track(v: &Value, album: Option<&Value>) -> Option<Track> {
    if v.get("type").and_then(Value::as_str).is_some_and(|t| t != "track") {
        return None; // Podcast episodes are not supported.
    }
    let id = s(v, "id");
    if id.is_empty() {
        return None; // Local files have no ID.
    }
    let album = v.get("album").or(album);
    let (artists, artist_id) = artist_names(v);
    Some(Track {
        id,
        name: s(v, "name"),
        artists,
        artist_id,
        album: album.map(|a| s(a, "name")).unwrap_or_default(),
        album_id: album.map(|a| s(a, "id")).unwrap_or_default(),
        duration_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
        img_s: album.map(|a| pick_image(a, 64)).unwrap_or_default(),
        img_l: album.map(|a| pick_image(a, 300)).unwrap_or_default(),
        playable: v.get("is_playable").and_then(Value::as_bool).unwrap_or(true),
    })
}

fn card(v: &Value, kind: CardKind) -> Option<Card> {
    let id = s(v, "id");
    if id.is_empty() {
        return None;
    }
    let sub = match kind {
        CardKind::Album => {
            let year: String = s(v, "release_date").chars().take(4).collect();
            let (artists, _) = artist_names(v);
            if year.is_empty() { artists } else { format!("{year} · {artists}") }
        }
        CardKind::Artist => "Artist".to_owned(),
        CardKind::Playlist => v
            .get("owner")
            .map(|o| s(o, "display_name"))
            .unwrap_or_default(),
    };
    Some(Card {
        kind,
        id,
        name: s(v, "name"),
        sub,
        img_s: pick_image(v, 64),
    })
}

fn items(v: &Value) -> &[Value] {
    v.get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn next_offset(v: &Value, offset: u32, got: usize) -> Option<u32> {
    let has_next = v.get("next").is_some_and(|n| !n.is_null());
    (has_next && got > 0).then_some(offset + got as u32)
}

impl Api {
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), String> {
        let mut attempt = 0;
        loop {
            let token = self.auth.lock().await.token().await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{BASE}{path}"))
                .bearer_auth(token)
                .query(query);
            req = match body {
                Some(b) => req.json(b),
                None if method != Method::GET => req.header("Content-Length", "0"),
                None => req,
            };
            let resp = req
                .send()
                .await
                .map_err(|e| format!("Network error: {e}"))?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < 2 {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| h.parse::<u64>().ok())
                    .unwrap_or(2);
                if wait <= 10 {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    continue;
                }
            }
            let text = resp.text().await.unwrap_or_default();
            let body = serde_json::from_str(&text).unwrap_or(Value::Null);
            return Ok((status, body));
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let (status, body) = self.request(Method::GET, path, query, None).await?;
        if status.is_success() {
            return Ok(body);
        }
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        Err(format!("Spotify API {}: {msg} ({path})", status.as_u16()))
    }

    pub async fn display_name(&self) -> Result<String, String> {
        let me = self.get("/me", &[]).await?;
        let name = s(&me, "display_name");
        Ok(if name.is_empty() { s(&me, "id") } else { name })
    }

    pub async fn playlists(&self) -> Result<Vec<Card>, String> {
        let mut out = Vec::new();
        let mut offset = 0;
        // 10 batches maximum: 500 playlists.
        for _ in 0..10 {
            let v = self
                .get(
                    "/me/playlists",
                    &[("limit", PAGE.to_string()), ("offset", offset.to_string())],
                )
                .await?;
            let batch = items(&v);
            out.extend(batch.iter().filter_map(|p| card(p, CardKind::Playlist)));
            match next_offset(&v, offset, batch.len()) {
                Some(n) => offset = n,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn page(&self, key: &PageKey, offset: u32) -> Result<PageData, String> {
        let paging = [("limit", PAGE.to_string()), ("offset", offset.to_string())];
        match key {
            PageKey::Home => {
                let v = self
                    .get("/me/player/recently-played", &[("limit", PAGE.to_string())])
                    .await?;
                let mut tracks: Vec<Track> = Vec::new();
                for it in items(&v) {
                    if let Some(t) = it.get("track").and_then(|t| track(t, None)) {
                        if !tracks.iter().any(|x| x.id == t.id) {
                            tracks.push(t);
                        }
                    }
                }
                Ok(PageData {
                    title: Some("Recently played".into()),
                    tracks,
                    ..Default::default()
                })
            }
            PageKey::Liked => {
                let v = self.get("/me/tracks", &paging).await?;
                let batch = items(&v);
                Ok(PageData {
                    title: Some("Liked Songs".into()),
                    subtitle: v
                        .get("total")
                        .and_then(Value::as_u64)
                        .map(|n| format!("{n} songs")),
                    tracks: batch
                        .iter()
                        .filter_map(|it| it.get("track").and_then(|t| track(t, None)))
                        .collect(),
                    next: next_offset(&v, offset, batch.len()),
                    ..Default::default()
                })
            }
            PageKey::Albums => {
                let v = self.get("/me/albums", &paging).await?;
                let batch = items(&v);
                Ok(PageData {
                    title: Some("Albums".into()),
                    cards: batch
                        .iter()
                        .filter_map(|it| it.get("album").and_then(|a| card(a, CardKind::Album)))
                        .collect(),
                    next: next_offset(&v, offset, batch.len()),
                    ..Default::default()
                })
            }
            PageKey::Playlist(id) => {
                let mut data = PageData::default();
                if offset == 0 {
                    let head = self
                        .get(
                            &format!("/playlists/{id}"),
                            &[("fields", "name,description,images,owner(display_name)".into())],
                        )
                        .await?;
                    data.title = Some(s(&head, "name"));
                    data.subtitle = head.get("owner").map(|o| s(o, "display_name"));
                    data.img_l = Some(pick_image(&head, 300));
                }
                let v = self.get(&format!("/playlists/{id}/items"), &paging).await?;
                let batch = items(&v);
                data.tracks = batch
                    .iter()
                    // "item" is the 2026 name of the field; "track" is the old name.
                    .filter_map(|it| it.get("item").or_else(|| it.get("track")))
                    .filter_map(|t| track(t, None))
                    .collect();
                data.next = next_offset(&v, offset, batch.len());
                Ok(data)
            }
            PageKey::Album(id) => {
                let mut data = PageData::default();
                let head = self.get(&format!("/albums/{id}"), &[]).await?;
                if offset == 0 {
                    let (artists, _) = artist_names(&head);
                    let year: String = s(&head, "release_date").chars().take(4).collect();
                    data.title = Some(s(&head, "name"));
                    data.subtitle = Some(format!("{artists} · {year}"));
                    data.img_l = Some(pick_image(&head, 300));
                }
                let v = if offset == 0 {
                    head.get("tracks").cloned().unwrap_or(Value::Null)
                } else {
                    self.get(&format!("/albums/{id}/tracks"), &paging).await?
                };
                let batch = items(&v);
                data.tracks = batch.iter().filter_map(|t| track(t, Some(&head))).collect();
                data.next = next_offset(&v, offset, batch.len());
                Ok(data)
            }
            PageKey::Artist(id) => {
                let mut data = PageData::default();
                if offset == 0 {
                    let head = self.get(&format!("/artists/{id}"), &[]).await?;
                    data.title = Some(s(&head, "name"));
                    data.subtitle = Some("Artist".into());
                    data.img_l = Some(pick_image(&head, 300));
                }
                let v = self
                    .get(
                        &format!("/artists/{id}/albums"),
                        &[
                            ("include_groups", "album,single".into()),
                            ("limit", "20".into()),
                            ("offset", offset.to_string()),
                        ],
                    )
                    .await?;
                let batch = items(&v);
                data.cards = batch.iter().filter_map(|a| card(a, CardKind::Album)).collect();
                data.next = next_offset(&v, offset, batch.len());
                Ok(data)
            }
            PageKey::Search(q) => {
                let v = self
                    .get(
                        "/search",
                        &[
                            ("q", q.clone()),
                            ("type", "track,album,artist,playlist".into()),
                            ("limit", "10".into()),
                            ("offset", offset.to_string()),
                        ],
                    )
                    .await?;
                let section = |name: &str| v.get(name).cloned().unwrap_or(Value::Null);
                let tracks_v = section("tracks");
                let batch = items(&tracks_v);
                let mut data = PageData {
                    tracks: batch.iter().filter_map(|t| track(t, None)).collect(),
                    // More batches add tracks only. 50 search results maximum.
                    next: next_offset(&tracks_v, offset, batch.len()).filter(|n| *n < 50),
                    ..Default::default()
                };
                if offset == 0 {
                    for (name, kind) in [
                        ("artists", CardKind::Artist),
                        ("albums", CardKind::Album),
                        ("playlists", CardKind::Playlist),
                    ] {
                        let sec = section(name);
                        data.cards
                            .extend(items(&sec).iter().take(6).filter_map(|c| card(c, kind)));
                    }
                }
                Ok(data)
            }
        }
    }

    /// Finds the Spotify Connect device ID of a device by its name.
    pub async fn device_id(&self, name: &str) -> Result<Option<String>, String> {
        let v = self.get("/me/player/devices", &[]).await?;
        Ok(v.get("devices")
            .and_then(Value::as_array)
            .and_then(|d| d.iter().find(|d| s(d, "name") == name))
            .map(|d| s(d, "id")))
    }

    /// Starts playback on a device. `body` is the Web API "start playback" body.
    pub async fn play(&self, device_id: &str, body: &Value) -> Result<(), String> {
        let (status, resp) = self
            .request(
                Method::PUT,
                "/me/player/play",
                &[("device_id", device_id.to_owned())],
                Some(body),
            )
            .await?;
        if status.is_success() {
            return Ok(());
        }
        let msg = resp
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        Err(format!("Spotify API {}: {msg}", status.as_u16()))
    }

    pub async fn is_liked(&self, id: &str) -> Option<bool> {
        let uri = format!("spotify:track:{id}");
        let v = match self.get("/me/library/contains", &[("uris", uri)]).await {
            Ok(v) => v,
            // The old path. It is available for client IDs that the 2026 rules do not apply to.
            Err(_) => self
                .get("/me/tracks/contains", &[("ids", id.to_owned())])
                .await
                .ok()?,
        };
        v.as_array()?.first()?.as_bool()
    }

    pub async fn set_liked(&self, id: &str, liked: bool) -> Result<(), String> {
        let method = if liked { Method::PUT } else { Method::DELETE };
        let uri = format!("spotify:track:{id}");
        let (status, _) = self
            .request(method.clone(), "/me/library", &[("uris", uri)], None)
            .await?;
        if status.is_success() {
            return Ok(());
        }
        let (status, _) = self
            .request(method, "/me/tracks", &[("ids", id.to_owned())], None)
            .await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("Spotify API {}: the like change failed", status.as_u16()))
        }
    }

    /// Downloads an image and scales it down. Returns RGBA pixels.
    pub async fn image(&self, url: &str, px: u32) -> Result<([usize; 2], Vec<u8>), String> {
        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
        tokio::task::spawn_blocking(move || {
            let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
            let img = if img.width() > px || img.height() > px {
                img.resize_to_fill(px, px, image::imageops::FilterType::Triangle)
            } else {
                img
            };
            let rgba = img.to_rgba8();
            Ok((
                [rgba.width() as usize, rgba.height() as usize],
                rgba.into_raw(),
            ))
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn images() -> Value {
        json!([
            { "url": "big", "width": 640 },
            { "url": "mid", "width": 300 },
            { "url": "small", "width": 64 },
        ])
    }

    #[test]
    fn picks_the_smallest_image_that_is_large_enough() {
        let v = json!({ "images": images() });
        assert_eq!(pick_image(&v, 64), "small");
        assert_eq!(pick_image(&v, 300), "mid");
        assert_eq!(pick_image(&v, 2000), "big");
        assert_eq!(pick_image(&json!({}), 64), "");
    }

    #[test]
    fn converts_a_track_and_uses_the_album_fallback() {
        let album = json!({ "id": "AL", "name": "Record", "images": images() });
        let v = json!({
            "type": "track", "id": "T1", "name": "Song", "duration_ms": 1234,
            "artists": [{ "id": "A1", "name": "One" }, { "id": "A2", "name": "Two" }],
        });
        let t = track(&v, Some(&album)).unwrap();
        assert_eq!((t.id.as_str(), t.artists.as_str(), t.artist_id.as_str()), ("T1", "One, Two", "A1"));
        assert_eq!((t.album.as_str(), t.album_id.as_str()), ("Record", "AL"));
        assert_eq!((t.img_s.as_str(), t.img_l.as_str()), ("small", "mid"));
        assert!(t.playable);
    }

    #[test]
    fn ignores_episodes_and_local_files() {
        assert!(track(&json!({ "type": "episode", "id": "E1" }), None).is_none());
        assert!(track(&json!({ "type": "track", "id": null, "name": "local" }), None).is_none());
    }

    #[test]
    fn calculates_the_next_offset() {
        assert_eq!(next_offset(&json!({ "next": "https://x" }), 50, 50), Some(100));
        assert_eq!(next_offset(&json!({ "next": null }), 50, 50), None);
        assert_eq!(next_offset(&json!({ "next": "https://x" }), 50, 0), None);
    }
}
