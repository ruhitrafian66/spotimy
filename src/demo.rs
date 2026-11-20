//! Demo mode (`SPOTIMY_DEMO=1`): sample data in place of Spotify.
//! It is for UI tests and memory measurements without an account.

use crate::model::{Card, CardKind, PageData, PageKey, Track};

pub fn enabled() -> bool {
    std::env::var_os("SPOTIMY_DEMO").is_some()
}

const WORDS: &[&str] = &[
    "Amber", "River", "Night", "Signal", "Paper", "Garden", "Static", "Harbor", "Velvet", "North",
    "Echo", "Glass", "Summer", "Orbit", "Quiet", "Copper", "Meadow", "Neon", "Winter", "Lantern",
];

fn words(seed: usize, n: usize) -> String {
    (0..n)
        .map(|i| WORDS[(seed * 7 + i * 13 + seed / 3) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn track(i: usize) -> Track {
    Track {
        id: format!("demo{i:018}"),
        name: words(i, 2 + i % 3),
        artists: words(i + 5, 1 + i % 2),
        artist_id: format!("artist{}", i % 9),
        album: words(i / 4 + 11, 2),
        album_id: format!("album{}", i / 4),
        duration_ms: 150_000 + ((i * 7919) % 140_000) as u32,
        img_s: format!("demo://cover/{}", i / 4),
        img_l: format!("demo://cover/{}", i / 4),
        playable: i % 23 != 22,
    }
}

fn card(kind: CardKind, i: usize) -> Card {
    Card {
        kind,
        id: format!("card{i}"),
        name: words(i + 3, 2),
        sub: match kind {
            CardKind::Album => format!("{} · {}", 2001 + i % 24, words(i, 1)),
            CardKind::Artist => "Artist".into(),
            CardKind::Playlist => "Demo user".into(),
        },
        img_s: format!("demo://cover/{}", 100 + i),
    }
}

pub fn playlists() -> Vec<Card> {
    (0..40).map(|i| card(CardKind::Playlist, i)).collect()
}

pub fn page(key: &PageKey, offset: u32) -> PageData {
    let batch = |total: u32| -> (Vec<Track>, Option<u32>) {
        let end = (offset + 50).min(total);
        let tracks = (offset..end).map(|i| track(i as usize)).collect();
        (tracks, (end < total).then_some(end))
    };
    match key {
        PageKey::Albums => PageData {
            title: Some("Albums".into()),
            cards: (0..36).map(|i| card(CardKind::Album, i)).collect(),
            ..Default::default()
        },
        PageKey::Artist(_) => PageData {
            title: Some(words(4, 2)),
            subtitle: Some("Artist".into()),
            img_l: Some("demo://cover/7".into()),
            cards: (0..18).map(|i| card(CardKind::Album, i)).collect(),
            ..Default::default()
        },
        PageKey::Search(_) => {
            let (tracks, _) = batch(10);
            PageData {
                tracks,
                cards: (0..6)
                    .map(|i| card(CardKind::Artist, i))
                    .chain((0..6).map(|i| card(CardKind::Album, i + 6)))
                    .collect(),
                ..Default::default()
            }
        }
        PageKey::Home => {
            let (tracks, _) = batch(50);
            PageData {
                title: Some("Recently played".into()),
                tracks,
                ..Default::default()
            }
        }
        _ => {
            // A long list: 3000 tracks, loaded in batches.
            let (tracks, next) = batch(3000);
            PageData {
                title: (offset == 0).then(|| words(9, 3)),
                subtitle: (offset == 0).then(|| "Demo user · 3000 songs".into()),
                img_l: (offset == 0).then(|| "demo://cover/3".into()),
                tracks,
                next,
                ..Default::default()
            }
        }
    }
}

/// Makes a cover image from the URL text: a two-color gradient.
pub fn image(url: &str, px: u32) -> ([usize; 2], Vec<u8>) {
    let h = url.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let (r, g, b) = ((h & 255) as f32, ((h >> 8) & 255) as f32, ((h >> 16) & 255) as f32);
    let n = px as usize;
    let mut out = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let t = (x + y) as f32 / (2 * n) as f32;
            out.extend_from_slice(&[
                (r * (1.0 - t) + b * t) as u8,
                (g * (1.0 - t) + r * t) as u8,
                (b * (1.0 - t) + g * t) as u8,
                255,
            ]);
        }
    }
    ([n, n], out)
}
