use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use std::{path::Path, time::Duration};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS poster_cache (
        title_id TEXT PRIMARY KEY, image BLOB NOT NULL, content_type TEXT NOT NULL,
        expires_at INTEGER NOT NULL
        );",
    )
}

#[derive(Debug)]
pub struct Poster {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub ttl: u32,
}

pub fn placeholder() -> Poster {
    Poster {
        bytes: include_bytes!("../static/poster-placeholder.svg").to_vec(),
        content_type: "image/svg+xml".into(),
        ttl: 300,
    }
}

pub fn cached(path: &Path, id: &str) -> rusqlite::Result<Option<Poster>> {
    cached_connection(&Connection::open(path)?, id)
}

fn cached_connection(connection: &Connection, id: &str) -> rusqlite::Result<Option<Poster>> {
    connection
        .query_row(
            "SELECT image,content_type,expires_at-unixepoch() FROM poster_cache
        WHERE title_id=?1 AND expires_at>unixepoch()",
            [id],
            |r| {
                Ok(Poster {
                    bytes: r.get(0)?,
                    content_type: r.get(1)?,
                    ttl: r.get(2)?,
                })
            },
        )
        .optional()
}

pub fn store(path: &Path, id: &str, poster: &Poster) -> rusqlite::Result<()> {
    let connection = Connection::open(path)?;
    // Image caching is optional; don't hold an image response up during a dataset import.
    connection.busy_timeout(Duration::from_millis(100))?;
    store_connection(&connection, id, poster)
}

fn store_connection(connection: &Connection, id: &str, poster: &Poster) -> rusqlite::Result<()> {
    connection.execute("INSERT INTO poster_cache VALUES (?1,?2,?3,unixepoch()+?4)
        ON CONFLICT(title_id) DO UPDATE SET image=excluded.image,content_type=excluded.content_type,expires_at=excluded.expires_at",
        rusqlite::params![id,poster.bytes,poster.content_type,poster.ttl])?;
    Ok(())
}

#[derive(Deserialize)]
struct Suggestions {
    d: Vec<Suggestion>,
}
#[derive(Deserialize)]
struct Suggestion {
    id: String,
    i: Option<Image>,
}
#[derive(Deserialize)]
struct Image {
    #[serde(rename = "imageUrl")]
    url: String,
}

fn image_url(value: &str) -> Result<reqwest::Url, Error> {
    let mut url = reqwest::Url::parse(value)?;
    if url.scheme() != "https"
        || url.host_str() != Some("m.media-amazon.com")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || !url.path().starts_with("/images/")
    {
        return Err("IMDb returned an unsupported poster URL".into());
    }
    let path = url.path().replace("._V1_.", "._V1_QL75_UX480_.");
    url.set_path(&path);
    Ok(url)
}

fn content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, Error> {
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("IMDb image request returned {}", response.status()).into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            return Err("IMDb image response exceeded its size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub async fn fetch(client: &reqwest::Client, id: &str) -> Result<Poster, Error> {
    if !crate::imdb::valid_title_id(id) {
        return Err("Invalid title ID".into());
    }
    let response = client
        .get(format!(
            "https://v3.sg.media-imdb.com/suggestion/t/{id}.json"
        ))
        .send()
        .await?;
    let suggestions: Suggestions =
        serde_json::from_slice(&read_limited(response, 2 * 1024 * 1024).await?)?;
    let image = suggestions
        .d
        .into_iter()
        .find(|item| item.id == id)
        .and_then(|item| item.i)
        .ok_or("IMDb has no poster for this title")?;
    let url = image_url(&image.url)?;
    let response = client.get(url).send().await?;
    let bytes = read_limited(response, 5 * 1024 * 1024).await?;
    let content_type = content_type(&bytes)
        .ok_or("IMDb returned an unsupported image format")?
        .to_owned();
    Ok(Poster {
        bytes,
        content_type,
        ttl: 7 * 24 * 60 * 60,
    })
}

// TMDB paths are filenames, never arbitrary URLs. Keep downloads on its image CDN.
fn tmdb_image_url(path: &str) -> Result<reqwest::Url, Error> {
    let filename = path.strip_prefix('/').ok_or("Invalid TMDB poster path")?;
    if filename.is_empty()
        || filename.len() > 200
        || !filename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || filename.contains("..")
        || ![".jpg", ".png", ".webp"]
            .iter()
            .any(|suffix| filename.ends_with(suffix))
    {
        return Err("Invalid TMDB poster path".into());
    }
    Ok(reqwest::Url::parse(&format!(
        "https://image.tmdb.org/t/p/w500/{filename}"
    ))?)
}

pub async fn fetch_tmdb(client: &reqwest::Client, path: &str) -> Result<Poster, Error> {
    let response = client.get(tmdb_image_url(path)?).send().await?;
    let bytes = read_limited(response, 5 * 1024 * 1024).await?;
    let content_type = content_type(&bytes).ok_or("TMDB returned an unsupported image format")?;
    Ok(Poster {
        bytes,
        content_type: content_type.into(),
        ttl: 24 * 60 * 60,
    })
}

pub fn person_placeholder() -> Poster {
    Poster {
        bytes: include_bytes!("../static/person-placeholder.svg").to_vec(),
        content_type: "image/svg+xml".into(),
        ttl: 300,
    }
}
pub async fn fetch_profile(client: &reqwest::Client, path: &str) -> Result<Poster, Error> {
    let mut url = tmdb_image_url(path)?;
    url.set_path(&url.path().replace("/t/p/w500/", "/t/p/w185/"));
    let bytes = read_limited(client.get(url).send().await?, 5 * 1024 * 1024).await?;
    let content_type =
        content_type(&bytes).ok_or("TMDB returned an unsupported profile image format")?;
    Ok(Poster {
        bytes,
        content_type: content_type.into(),
        ttl: 24 * 60 * 60,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tmdb_paths_cannot_redirect_downloads_to_other_hosts_or_paths() {
        assert_eq!(
            tmdb_image_url("/abc123.jpg").unwrap().as_str(),
            "https://image.tmdb.org/t/p/w500/abc123.jpg"
        );
        for path in [
            "",
            "//evil.example/a.jpg",
            "https://evil.example/a.jpg",
            "/../a.jpg",
            "/a/b.jpg",
            "/a.jpg?x=1",
            "/a.svg",
            "/a%2f.jpg",
            "/a.jpg#x",
        ] {
            assert!(tmdb_image_url(path).is_err(), "{path}");
        }
    }
    #[test]
    fn only_imdb_image_host_is_allowed_and_images_are_sized_for_the_page() {
        let url = image_url("https://m.media-amazon.com/images/M/example._V1_.jpg").unwrap();
        assert!(url.path().ends_with("._V1_QL75_UX480_.jpg"));
        for invalid in [
            "http://m.media-amazon.com/images/a.jpg",
            "https://example.com/images/a.jpg",
            "https://m.media-amazon.com.example.com/images/a.jpg",
            "https://m.media-amazon.com@127.0.0.1/images/a.jpg",
            "https://m.media-amazon.com:8443/images/a.jpg",
            "https://user@m.media-amazon.com/images/a.jpg",
            "https://m.media-amazon.com/other/a.jpg",
        ] {
            assert!(image_url(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn upstream_html_or_svg_is_not_served_as_a_poster() {
        assert_eq!(content_type(&[0xff, 0xd8, 0xff]), Some("image/jpeg"));
        assert_eq!(content_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(content_type(b"RIFF1234WEBP"), Some("image/webp"));
        assert!(content_type(b"<html>Access challenge</html>").is_none());
        assert!(content_type(b"<svg onload='alert(1)'></svg>").is_none());
    }
    #[test]
    fn cache_round_trips_images_and_expires_without_touching_saved_titles() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        crate::watchlist::initialize(&db).unwrap();
        db.execute("INSERT INTO watchlist(title_id,title_name,description) VALUES ('tt1375666','Inception','2010')",[]).unwrap();
        let poster = Poster {
            bytes: vec![0xff, 0xd8, 0xff],
            content_type: "image/jpeg".into(),
            ttl: 600,
        };
        store_connection(&db, "tt1375666", &poster).unwrap();
        assert_eq!(
            cached_connection(&db, "tt1375666").unwrap().unwrap().bytes,
            poster.bytes
        );
        store_connection(&db, "tt1375666", &placeholder()).unwrap();
        assert_eq!(
            cached_connection(&db, "tt1375666")
                .unwrap()
                .unwrap()
                .content_type,
            "image/svg+xml"
        );
        db.execute("UPDATE poster_cache SET expires_at=unixepoch()-1", [])
            .unwrap();
        assert!(cached_connection(&db, "tt1375666").unwrap().is_none());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM watchlist", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
