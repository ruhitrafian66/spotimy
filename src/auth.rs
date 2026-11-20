//! Login and token storage.
//!
//! The login is the OAuth 2.0 authorization code flow with PKCE (RFC 7636), which is the
//! flow Spotify specifies for apps that cannot keep a client secret. The app has no secret.
//! It makes a random `code_verifier` for each login, sends only its SHA-256 hash
//! (`code_challenge`) to the browser, and proves that it has the verifier when it exchanges
//! the code. An authorization code that a different program intercepts is thus of no use.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

/// The address of the local login callback. Spotify permits `http` only for a loopback IP
/// literal, and does not permit `localhost`.
const CALLBACK_ADDR: &str = "127.0.0.1:8898";
const CALLBACK_PATH: &str = "/login";
/// The redirect URI. The client ID must have exactly this URI in the Spotify dashboard.
pub const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// The time that the user has to approve the login in the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// The scopes that the app uses. Each one is necessary for a feature.
const SCOPES: &[&str] = &[
    "user-read-playback-state",   // find the Soloist device
    "user-modify-playback-state", // start playback on it
    "playlist-read-private",
    "playlist-read-collaborative",
    "user-library-read",
    "user-library-modify", // the heart button
    "user-follow-read",
    "user-read-recently-played", // the Home page
    "user-top-read",
    "user-read-private",
];

fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Spotimy</title></head>\
         <body style=\"font-family:system-ui;background:#111;color:#eee;display:flex;\
         align-items:center;justify-content:center;height:100vh;margin:0\">\
         <div><h2>{title}</h2><p>{text}</p></div></body></html>"
    )
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub client_id: String,
    /// The path of the Soloist executable. Empty: the app searches for it.
    #[serde(default)]
    pub soloist_path: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    refresh_token: String,
}

pub fn data_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spotimy");
    let _ = fs::create_dir_all(&dir);
    dir
}

impl Config {
    pub fn load() -> Self {
        fs::read(data_dir().join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(data_dir().join("config.json"), b);
        }
    }
}

fn token_path() -> PathBuf {
    data_dir().join("session.json")
}

fn save_refresh_token(token: &str) {
    let path = token_path();
    if let Ok(b) = serde_json::to_vec(&Saved {
        refresh_token: token.to_owned(),
    }) {
        let _ = fs::write(&path, b);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

fn load_refresh_token() -> Option<String> {
    let b = fs::read(token_path()).ok()?;
    let s: Saved = serde_json::from_slice(&b).ok()?;
    (!s.refresh_token.is_empty()).then_some(s.refresh_token)
}

pub fn forget() {
    let _ = fs::remove_file(token_path());
}

/// Checks a client ID that the user typed or pasted. A Spotify client ID is 32 hex characters.
/// Returns the clean ID.
pub fn clean_client_id(input: &str) -> Option<String> {
    let id = input.trim().trim_matches(|c| c == '"' || c == '\'');
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

// ---------------------------------------------------------------- PKCE

fn random_token(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("No random numbers are available: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// The S256 code challenge of a code verifier (RFC 7636, section 4.2).
fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize_url(client_id: &str, state: &str, challenge: &str) -> Result<String, String> {
    reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("state", state),
            ("scope", &SCOPES.join(" ")),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge),
        ],
    )
    .map(String::from)
    .map_err(|e| e.to_string())
}

#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    /// Spotify sent an error, for example `access_denied`.
    Denied(String),
    /// Not the login callback, or a callback with an incorrect `state`.
    Ignore,
}

/// Reads the first line of an HTTP request to the callback address.
fn parse_callback(request_line: &str, expected_state: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Ignore;
    };
    let Ok(url) = reqwest::Url::parse(&format!("http://{CALLBACK_ADDR}{target}")) else {
        return Callback::Ignore;
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Ignore;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    // The `state` check comes first. A request without the correct state is not from the
    // login that this app started, thus it must not end or complete the login.
    if param("state").as_deref() != Some(expected_state) {
        return Callback::Ignore;
    }
    if let Some(error) = param("error") {
        return Callback::Denied(error);
    }
    match param("code") {
        Some(code) if !code.is_empty() => Callback::Code(code),
        _ => Callback::Ignore,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Waits for the browser to open the callback URL. Returns the authorization code.
async fn wait_for_code(listener: TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut line = String::new();
        {
            // 8 kB and 5 seconds maximum for the request line.
            let mut reader = BufReader::new((&mut stream).take(8192));
            let read = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line));
            if !matches!(read.await, Ok(Ok(_))) {
                continue;
            }
        }
        match parse_callback(&line, state) {
            Callback::Code(code) => {
                let body = page("Spotimy is connected.", "You can close this tab.");
                respond(&mut stream, "200 OK", &body).await;
                return Ok(code);
            }
            Callback::Denied(error) => {
                let body = page("The login did not complete.", "Go back to Spotimy and try again.");
                respond(&mut stream, "200 OK", &body).await;
                return Err(format!("Spotify refused the login ({error})"));
            }
            Callback::Ignore => {
                respond(&mut stream, "400 Bad Request", &page("Not a valid login callback.", "")).await;
            }
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Deserialize, Default)]
struct TokenError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Sends a request to the token endpoint. On an error, returns the OAuth error code first.
async fn token_request(
    http: &reqwest::Client,
    form: &[(&str, &str)],
) -> Result<TokenResponse, (String, String)> {
    let net = |e: reqwest::Error| (String::new(), format!("Network error: {e}"));
    let resp = http.post(TOKEN_URL).form(form).send().await.map_err(net)?;
    let status = resp.status();
    let text = resp.text().await.map_err(net)?;
    if status.is_success() {
        return serde_json::from_str(&text)
            .map_err(|e| (String::new(), format!("Bad token response: {e}")));
    }
    let err: TokenError = serde_json::from_str(&text).unwrap_or_default();
    let detail = if err.error_description.is_empty() { &err.error } else { &err.error_description };
    Err((err.error.clone(), format!("Spotify login error {}: {detail}", status.as_u16())))
}

/// Holds the current tokens and refreshes them.
pub struct Auth {
    http: reqwest::Client,
    client_id: String,
    access: String,
    refresh: String,
    expires_at: Instant,
}

impl Auth {
    fn from_response(http: reqwest::Client, client_id: &str, t: TokenResponse, old_refresh: &str) -> Self {
        // Spotify does not always send a new refresh token. Keep the old one in that case.
        let refresh = if t.refresh_token.is_empty() { old_refresh.to_owned() } else { t.refresh_token };
        save_refresh_token(&refresh);
        let life = if t.expires_in == 0 { 3600 } else { t.expires_in };
        Self {
            http,
            client_id: client_id.to_owned(),
            access: t.access_token,
            refresh,
            expires_at: Instant::now() + Duration::from_secs(life),
        }
    }

    /// Restores a saved login without the browser.
    pub async fn restore(http: reqwest::Client, client_id: &str) -> Result<Option<Self>, String> {
        let Some(refresh) = load_refresh_token() else {
            return Ok(None);
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id),
        ];
        match token_request(&http, &form).await {
            Ok(t) => Ok(Some(Self::from_response(http, client_id, t, &refresh))),
            Err((code, msg)) => {
                if code == "invalid_grant" {
                    // The refresh token is expired (6 months) or revoked. It has no more use.
                    forget();
                }
                Err(msg)
            }
        }
    }

    /// Opens the browser and waits for the user to approve the login.
    pub async fn interactive(http: reqwest::Client, client_id: &str) -> Result<Self, String> {
        let verifier = random_token(48)?; // 64 characters; RFC 7636 permits 43 to 128
        let state = random_token(16)?;
        let url = authorize_url(client_id, &state, &code_challenge(&verifier))?;

        // Bind before the browser opens, thus the callback cannot arrive too early.
        let listener = TcpListener::bind(CALLBACK_ADDR).await.map_err(|e| {
            format!("Port 8898 is not available ({e}). Close the program that uses it.")
        })?;
        open::that_detached(&url).map_err(|e| format!("The browser did not open: {e}"))?;

        let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(listener, &state))
            .await
            .map_err(|_| "The login was not approved in 5 minutes".to_owned())??;

        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
        ];
        let t = token_request(&http, &form).await.map_err(|(_, msg)| msg)?;
        if t.refresh_token.is_empty() {
            return Err("Spotify sent no refresh token".into());
        }
        Ok(Self::from_response(http, client_id, t, ""))
    }

    /// Returns a valid access token. Refreshes it when it is near its expiry time.
    pub async fn token(&mut self) -> Result<String, String> {
        if Instant::now() + Duration::from_secs(60) >= self.expires_at {
            let form = [
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh.as_str()),
                ("client_id", self.client_id.as_str()),
            ];
            let t = token_request(&self.http, &form)
                .await
                .map_err(|(_, msg)| format!("Token refresh failed. Log out and log in again. {msg}"))?;
            if !t.refresh_token.is_empty() && t.refresh_token != self.refresh {
                self.refresh = t.refresh_token;
                save_refresh_token(&self.refresh);
            }
            self.access = t.access_token;
            let life = if t.expires_in == 0 { 3600 } else { t.expires_in };
            self.expires_at = Instant::now() + Duration::from_secs(life);
        }
        Ok(self.access.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_must_be_32_hex_characters() {
        let id = "0123456789abcdef0123456789ABCDEF";
        assert_eq!(clean_client_id(&format!("  {id}\n")), Some(id.to_ascii_lowercase()));
        assert_eq!(clean_client_id(&format!("\"{id}\"")), Some(id.to_ascii_lowercase()));
        assert_eq!(clean_client_id("0123456789abcdef"), None);
        assert_eq!(clean_client_id("0123456789abcdef0123456789abcdeg"), None);
        assert_eq!(clean_client_id(""), None);
    }

    #[test]
    fn code_challenge_matches_the_rfc_7636_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_state_are_random_and_have_a_valid_length() {
        let a = random_token(48).unwrap();
        let b = random_token(48).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }

    #[test]
    fn authorize_url_has_the_pkce_parameters() {
        let url = authorize_url("cid", "st", "chal").unwrap();
        assert!(url.starts_with("https://accounts.spotify.com/authorize?"));
        for part in [
            "client_id=cid",
            "response_type=code",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A8898%2Flogin",
            "state=st",
            "code_challenge_method=S256",
            "code_challenge=chal",
        ] {
            assert!(url.contains(part), "{part} is not in {url}");
        }
        assert!(!url.contains("secret"));
    }

    #[test]
    fn callback_needs_the_correct_path_and_state() {
        let ok = "GET /login?code=abc&state=s1 HTTP/1.1";
        assert_eq!(parse_callback(ok, "s1"), Callback::Code("abc".into()));
        // An incorrect or absent state is ignored: this is the CSRF protection.
        assert_eq!(parse_callback(ok, "other"), Callback::Ignore);
        assert_eq!(parse_callback("GET /login?code=abc HTTP/1.1", "s1"), Callback::Ignore);
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1", "s1"), Callback::Ignore);
        assert_eq!(parse_callback("GET /other?code=abc&state=s1 HTTP/1.1", "s1"), Callback::Ignore);
        assert_eq!(parse_callback("POST /login?code=abc&state=s1 HTTP/1.1", "s1"), Callback::Ignore);
        assert_eq!(parse_callback("", "s1"), Callback::Ignore);
        assert_eq!(
            parse_callback("GET /login?error=access_denied&state=s1 HTTP/1.1", "s1"),
            Callback::Denied("access_denied".into())
        );
        // An error without the correct state cannot stop the login.
        assert_eq!(
            parse_callback("GET /login?error=access_denied&state=x HTTP/1.1", "s1"),
            Callback::Ignore
        );
    }

    async fn get(addr: std::net::SocketAddr, target: &str) -> String {

        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out).await;
        out
    }

    #[tokio::test]
    async fn listener_ignores_bad_requests_and_returns_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { wait_for_code(listener, "good").await });

        let forged = get(addr, "/login?code=evil&state=bad").await;
        assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
        assert!(!task.is_finished());

        let real = get(addr, "/login?code=real&state=good").await;
        assert!(real.starts_with("HTTP/1.1 200"), "{real}");
        assert_eq!(task.await.unwrap(), Ok("real".to_owned()));
    }
}
