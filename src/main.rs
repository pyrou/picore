use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, Response},
    routing::get,
};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore};
use url::Url;

#[derive(Clone)]
struct App {
    db: Arc<Mutex<Connection>>,
    client: reqwest::Client,
    upstream: Arc<Semaphore>,
    conversions: Arc<Semaphore>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Artwork {
    url: Option<String>,
    url_tall: Option<String>,
    artist: String,
    album: String,
    #[serde(rename = "isCached")]
    is_cached: bool,
}
#[derive(Deserialize)]
struct UrlQuery {
    url: String,
}
#[derive(Deserialize)]
struct DownloadQuery {
    url: String,
    format: String,
}
#[derive(Deserialize)]
struct SearchQuery {
    artist: String,
    album: String,
    title: Option<String>,
}
type ApiError = (StatusCode, Json<Value>);
fn err(status: StatusCode, message: &str) -> ApiError {
    (status, Json(json!({"message":message})))
}

fn target(input: &str) -> Option<(String, String, String)> {
    let u = Url::parse(input).ok()?;
    if u.scheme() != "https"
        || u.host_str()? != "music.apple.com"
        || u.port().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        return None;
    }
    let parts: Vec<_> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let pos = parts
        .iter()
        .position(|p| *p == "album" || *p == "playlist")?;
    if pos > 1
        || (pos == 1 && (parts[0].len() != 2 || !parts[0].bytes().all(|b| b.is_ascii_alphabetic())))
    {
        return None;
    }
    let id = *parts.last()?;
    if parts.len() < pos + 2 || parts.len() > pos + 3 {
        return None;
    }
    let kind = parts[pos];
    if !(if kind == "album" {
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
    } else {
        id.strip_prefix("pl.")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
    }) {
        return None;
    }
    Some((
        if pos == 1 {
            parts[0].to_ascii_lowercase()
        } else {
            "us".into()
        },
        kind.into(),
        id.into(),
    ))
}
fn cache_get(app: &App, key: &str) -> Result<Option<Artwork>, ApiError> {
    let db = app
        .db
        .lock()
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache unavailable"))?;
    let s: Option<String> = db
        .query_row(
            "SELECT value FROM cache WHERE key=?1 AND expires > unixepoch()",
            [key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache read failed"))?;
    s.map(|s| {
        serde_json::from_str(&s)
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Invalid cache entry"))
    })
    .transpose()
}
fn cache_put(app: &App, key: &str, value: &Artwork) -> Result<(), ApiError> {
    let db = app
        .db
        .lock()
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache unavailable"))?;
    let ttl = if value.url.is_some() || value.url_tall.is_some() {
        7 * 86400
    } else {
        86400
    };
    db.execute("INSERT OR REPLACE INTO cache(key,value,expires,updated) VALUES (?1,?2,unixepoch()+?3,unixepoch())", params![key, serde_json::to_string(value).unwrap(), ttl]).map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR,"Cache write failed"))?;
    Ok(())
}
async fn fetch(app: &App, url: &str) -> Result<String, ApiError> {
    let response = app
        .client
        .get(url)
        .send()
        .await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Apple Music request failed"))?;
    if !response.status().is_success() {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            "Apple Music returned an error",
        ));
    }
    response
        .text()
        .await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Invalid Apple Music response"))
}
async fn artwork(app: &App, input: &str) -> Result<Artwork, ApiError> {
    let (store, kind, id) = target(input).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "Provide a valid Apple Music album or playlist URL",
        )
    })?;
    let key = format!("{store}:{kind}:{id}");
    if let Some(mut a) = cache_get(app, &key)? {
        a.is_cached = true;
        return Ok(a);
    }
    let _permit = app
        .upstream
        .acquire()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "Service unavailable"))?;
    if let Some(mut a) = cache_get(app, &key)? {
        a.is_cached = true;
        return Ok(a);
    }
    let page = fetch(app, "https://music.apple.com/us/new").await?;
    let assets = Regex::new(r#"/assets/[^\"']+\.js"#).unwrap();
    let jwt = Regex::new(r#"eyJ[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+"#).unwrap();
    let mut token = None;
    for asset in assets
        .find_iter(&page)
        .map(|m| m.as_str())
        .filter(|p| p.contains("index") || p.contains("web-client") || p.contains("apple-music"))
        .take(10)
    {
        if let Ok(js) = fetch(app, &format!("https://music.apple.com{asset}")).await
            && let Some(m) = jwt.find(&js)
        {
            token = Some(m.as_str().to_owned());
            break;
        }
    }
    let token = token.ok_or_else(|| {
        err(
            StatusCode::BAD_GATEWAY,
            "Could not find Apple's public catalog token",
        )
    })?;
    let endpoint = format!(
        "https://amp-api.music.apple.com/v1/catalog/{store}/{kind}s/{id}?extend=editorialVideo&platform=web"
    );
    let response = app
        .client
        .get(endpoint)
        .bearer_auth(token)
        .header("Origin", "https://music.apple.com")
        .send()
        .await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Apple catalog request failed"))?;
    if !response.status().is_success() {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            "Apple catalog returned an error",
        ));
    }
    let data: Value = response
        .json()
        .await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Invalid Apple catalog response"))?;
    let attrs = data.pointer("/data/0/attributes").ok_or_else(|| {
        err(
            StatusCode::BAD_GATEWAY,
            "Apple catalog returned no attributes",
        )
    })?;
    let string = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
    let a = Artwork {
        url: string(attrs.pointer("/editorialVideo/motionDetailSquare/video")),
        url_tall: string(attrs.pointer("/editorialVideo/motionDetailTall/video")),
        artist: string(attrs.get("artistName").or_else(|| attrs.get("curatorName")))
            .unwrap_or_else(|| "Unknown Artist".into()),
        album: string(attrs.get("name")).unwrap_or_else(|| "Unknown Album".into()),
        is_cached: false,
    };
    cache_put(app, &key, &a)?;
    Ok(a)
}
fn result(a: Artwork) -> Result<Json<Artwork>, ApiError> {
    if a.url.is_none() && a.url_tall.is_none() {
        Err(err(StatusCode::NOT_FOUND, "No animated artwork found"))
    } else {
        Ok(Json(a))
    }
}
async fn by_url(
    State(app): State<App>,
    Query(q): Query<UrlQuery>,
) -> Result<Json<Artwork>, ApiError> {
    result(artwork(&app, &q.url).await?)
}
fn normalized_name(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn album_score(found: &str, requested: &str) -> u8 {
    let found = normalized_name(found);
    let requested = normalized_name(requested);
    if found.is_empty() || requested.is_empty() {
        return 0;
    }
    if found == requested {
        return 2;
    }
    let edition = |extra: &str| {
        let words = [
            "deluxe",
            "edition",
            "version",
            "remastered",
            "remaster",
            "anniversary",
            "expanded",
            "bonus",
            "track",
            "tracks",
            "explicit",
            "clean",
            "special",
            "single",
            "ep",
        ];
        extra
            .split_whitespace()
            .all(|w| words.contains(&w) || w.chars().all(|c| c.is_ascii_digit()))
    };
    if found
        .strip_prefix(&format!("{requested} "))
        .is_some_and(edition)
        || requested
            .strip_prefix(&format!("{found} "))
            .is_some_and(edition)
    {
        1
    } else {
        0
    }
}

fn find_album<'a>(data: &'a Value, artist: &str, album: &str) -> Option<&'a str> {
    let artist = normalized_name(artist);
    data.get("results")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let found_artist = normalized_name(item.get("artistName")?.as_str()?);
            if artist.is_empty() || found_artist != artist {
                return None;
            }
            let score = album_score(item.get("collectionName")?.as_str()?, album);
            if score == 0 {
                return None;
            }
            let url = item.get("collectionViewUrl")?.as_str()?;
            target(url)?;
            Some((score, url))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, url)| url)
}

async fn search(
    State(app): State<App>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Artwork>, ApiError> {
    if q.artist.trim().is_empty()
        || q.album.trim().is_empty()
        || q.artist.len() > 300
        || q.album.len() > 300
        || q.title.as_ref().is_some_and(|v| v.len() > 300)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Artist and album are required and must be at most 300 bytes",
        ));
    }
    let title = q.title.unwrap_or_default();
    let term = format!("{} {} {}", q.artist, q.album, title);
    let key = format!(
        "search:{}",
        serde_json::to_string(&(
            normalized_name(&q.artist),
            normalized_name(&q.album),
            normalized_name(&title)
        ))
        .unwrap()
    );
    if let Some(mut a) = cache_get(&app, &key)? {
        a.is_cached = true;
        return result(a);
    }
    // Version negative entries when matching rules change so older misses are retried.
    let negative_key = format!("v2:{key}");
    {
        let db = app
            .db
            .lock()
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache unavailable"))?;
        let absent: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM negative_search WHERE key=?1 AND expires > unixepoch())", [&negative_key], |row| row.get(0))
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache read failed"))?;
        if absent {
            return Err(err(StatusCode::NOT_FOUND, "No album found"));
        }
    }
    let mut u = Url::parse("https://itunes.apple.com/search").unwrap();
    u.query_pairs_mut()
        .append_pair("term", &term)
        .append_pair(
            "entity",
            if title.trim().is_empty() {
                "album"
            } else {
                "song"
            },
        )
        .append_pair("limit", "50");
    let body = fetch(&app, u.as_str()).await?;
    let data: Value = serde_json::from_str(&body)
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Invalid search response"))?;
    // Only an explicit results array is a confirmed miss. Upstream errors must never be cached as misses.
    if !data.get("results").is_some_and(Value::is_array) {
        return Err(err(StatusCode::BAD_GATEWAY, "Invalid search response"));
    }
    let Some(link) = find_album(&data, &q.artist, &q.album) else {
        let db = app
            .db
            .lock()
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache unavailable"))?;
        db.execute(
            "INSERT OR REPLACE INTO negative_search(key,expires) VALUES (?1,unixepoch()+86400)",
            [&negative_key],
        )
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache write failed"))?;
        return Err(err(StatusCode::NOT_FOUND, "No album found"));
    };
    let a = artwork(&app, link).await?;
    cache_put(&app, &key, &a)?;
    result(a)
}

async fn download(
    State(app): State<App>,
    Query(q): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let (mime, extension) = match q.format.as_str() {
        "mp4" => ("video/mp4", "mp4"),
        "webp" => ("image/webp", "webp"),
        _ => return Err(err(StatusCode::BAD_REQUEST, "Format must be mp4 or webp")),
    };
    let trusted = Url::parse(&q.url).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("mvod.itunes.apple.com")
            && u.port().is_none()
            && u.username().is_empty()
            && u.password().is_none()
    });
    if !trusted {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid Apple artwork stream"));
    }
    let known = {
        let db = app
            .db
            .lock()
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache unavailable"))?;
        let mut stmt = db
            .prepare(
                "SELECT value FROM cache WHERE expires > unixepoch() AND key NOT LIKE 'search:%'",
            )
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache read failed"))?;
        let values = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache read failed"))?;
        values
            .filter_map(|row| {
                row.ok()
                    .and_then(|v| serde_json::from_str::<Artwork>(&v).ok())
            })
            .any(|a| a.url.as_deref() == Some(&q.url) || a.url_tall.as_deref() == Some(&q.url))
    };
    if !known {
        return Err(err(
            StatusCode::NOT_FOUND,
            "Look up this artwork before downloading it",
        ));
    }
    let _permit = app.conversions.try_acquire().map_err(|_| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many conversions; try again shortly",
        )
    })?;
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "https,tls,tcp,crypto",
        "-i",
        &q.url,
        "-t",
        "60",
        "-map",
        "0:v:0",
        "-an",
    ]);
    if extension == "mp4" {
        command.args([
            "-c:v",
            "copy",
            "-movflags",
            "frag_keyframe+empty_moov",
            "-f",
            "mp4",
        ]);
    } else {
        command.args([
            "-c:v", "libwebp", "-loop", "0", "-quality", "80", "-f", "webp",
        ]);
    }
    command
        .arg("pipe:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "FFmpeg is not available"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "Conversion failed"))?;
    let conversion = async {
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| ())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(());
        }
        let status = child.wait().await.map_err(|_| ())?;
        if !status.success() || bytes.is_empty() {
            return Err(());
        }
        Ok(bytes)
    };
    let bytes = tokio::time::timeout(Duration::from_secs(120), conversion)
        .await
        .map_err(|_| err(StatusCode::GATEWAY_TIMEOUT, "Conversion timed out"))?
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Could not convert artwork"))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=artwork.{extension}"),
        )
        .body(bytes.into())
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Conversion failed"))
}

async fn history(State(app): State<App>) -> Result<Json<Vec<Artwork>>, ApiError> {
    let db = app
        .db
        .lock()
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache unavailable"))?;
    let mut stmt = db.prepare("SELECT value FROM cache WHERE key NOT LIKE 'search:%' AND expires > unixepoch() ORDER BY updated DESC LIMIT 30").map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR,"Cache read failed"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cache read failed"))?;
    Ok(Json(
        rows.filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok()))
            .collect(),
    ))
}
#[tokio::main]
async fn main() {
    let path = env::var("DATABASE_PATH").unwrap_or_else(|_| "cache.db".into());
    let db = Connection::open(path).expect("open cache database");
    db.execute_batch("CREATE TABLE IF NOT EXISTS cache (key TEXT PRIMARY KEY, value TEXT NOT NULL, expires INTEGER NOT NULL, updated INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS negative_search (key TEXT PRIMARY KEY, expires INTEGER NOT NULL);").expect("initialize cache");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/17.0 Safari/605.1.15").build().unwrap();
    let app = App {
        db: Arc::new(Mutex::new(db)),
        client,
        upstream: Arc::new(Semaphore::new(2)),
        conversions: Arc::new(Semaphore::new(2)),
    };
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route("/api/v1/artwork/url", get(by_url))
        .route("/api/v1/artwork/search", get(search))
        .route("/api/v1/artwork/history", get(history))
        .route("/api/v1/artwork/download", get(download))
        .route(
            "/api/v1/status",
            get(|| async { Json(json!({"status":"operational"})) }),
        )
        .with_state(app);
    let addr = env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind server");
    println!("Listening on {addr}");
    axum::serve(listener, router).await.unwrap();
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_apple_targets() {
        assert_eq!(
            target("https://music.apple.com/fr/album/test/123?i=456"),
            Some(("fr".into(), "album".into(), "123".into()))
        );
        assert!(target("https://music.apple.com/us/playlist/test/pl.abc123").is_some());
        for u in [
            "https://music.apple.com.evil.test/us/album/a/123",
            "http://music.apple.com/us/album/a/123",
            "https://evil.test/?u=music.apple.com/us/album/a/123",
            "https://music.apple.com/us/album/a/nope",
        ] {
            assert!(target(u).is_none());
        }
    }
    #[test]
    fn search_requires_artist_and_album_match_and_prefers_exact() {
        let data = json!({"results": [
            {"artistName":"Other", "collectionName":"Living Things", "collectionViewUrl":"https://music.apple.com/us/album/1"},
            {"artistName":"Linkin Park", "collectionName":"Living Things (Deluxe Edition)", "collectionViewUrl":"https://music.apple.com/us/album/2"},
            {"artistName":"LINKIN PARK", "collectionName":"LIVING THINGS", "collectionViewUrl":"https://music.apple.com/us/album/3"},
            {"artistName":"Linkin Park", "collectionName":"Living Things Live", "collectionViewUrl":"https://music.apple.com/us/album/4"}
        ]});
        assert_eq!(
            find_album(&data, "Linkin Park", "Living Things"),
            Some("https://music.apple.com/us/album/3")
        );
        assert_eq!(find_album(&data, "Linkin Park", "Living"), None);
        assert_eq!(find_album(&data, "Nobody", "Living Things"), None);
        assert_eq!(
            album_score("Living Things (Deluxe Edition)", "Living Things"),
            1
        );
        assert_eq!(album_score("Living Things Live", "Living Things"), 0);
    }
    #[test]
    fn search_matches_single_and_ep_without_accepting_unrelated_titles() {
        let data = json!({"results": [
            {"artistName":"BLACKPINK", "collectionName":"JUMP (Remix) - Single", "collectionViewUrl":"https://music.apple.com/us/album/1"},
            {"artistName":"BLACKPINK", "collectionName":"JUMP - Single", "collectionViewUrl":"https://music.apple.com/us/album/2"}
        ]});
        assert_eq!(
            find_album(&data, "BLACKPINK", "JUMP"),
            Some("https://music.apple.com/us/album/2")
        );
        assert_eq!(album_score("JUMP - Single", "JUMP"), 1);
        assert_eq!(album_score("JUMP", "JUMP - Single"), 1);
        assert_eq!(album_score("SQUARE TWO - EP", "SQUARE TWO"), 1);
        assert_eq!(album_score("JUMP (Remix) - Single", "JUMP"), 0);
    }
    #[test]
    fn cache_stores_and_expires_positive_and_negative_artwork() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE cache (key TEXT PRIMARY KEY, value TEXT NOT NULL, expires INTEGER NOT NULL, updated INTEGER NOT NULL)").unwrap();
        let app = App {
            db: Arc::new(Mutex::new(db)),
            client: reqwest::Client::new(),
            upstream: Arc::new(Semaphore::new(2)),
            conversions: Arc::new(Semaphore::new(2)),
        };
        let mut art = Artwork {
            url: Some("https://example.com/test.m3u8".into()),
            url_tall: None,
            artist: "Artist".into(),
            album: "Album".into(),
            is_cached: false,
        };
        cache_put(&app, "positive", &art).unwrap();
        art.url = None;
        cache_put(&app, "negative", &art).unwrap();
        assert!(cache_get(&app, "positive").unwrap().unwrap().url.is_some());
        assert!(cache_get(&app, "negative").unwrap().unwrap().url.is_none());
        app.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE cache SET expires=unixepoch()-1 WHERE key='negative'",
                [],
            )
            .unwrap();
        assert!(cache_get(&app, "negative").unwrap().is_none());
        assert!(cache_get(&app, "positive").unwrap().is_some());
    }
}
