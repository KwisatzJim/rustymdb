use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS tmdb_collection_cache(collection_id INTEGER PRIMARY KEY,payload TEXT NOT NULL,expires_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS tmdb_title_links(media TEXT NOT NULL,tmdb_id INTEGER NOT NULL,imdb_id TEXT,expires_at INTEGER NOT NULL,PRIMARY KEY(media,tmdb_id)); CREATE TABLE IF NOT EXISTS tmdb_person_cache(imdb_id TEXT PRIMARY KEY,payload TEXT NOT NULL,expires_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS tmdb_cache(imdb_id TEXT PRIMARY KEY,payload TEXT NOT NULL,expires_at INTEGER NOT NULL);")
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AuthKind {
    Key,
    Token,
}
#[derive(Clone, Serialize, Deserialize)]
struct Credentials {
    kind: AuthKind,
    value: String,
}
impl Credentials {
    fn new(key: &str, token: &str) -> Result<Self, String> {
        let key = key.trim();
        let token = token.trim().strip_prefix("Bearer ").unwrap_or(token.trim());
        if !key.is_empty() && !token.is_empty() {
            return Err("Enter either an API key or a read-access token, not both.".into());
        }
        if !token.is_empty() {
            if !(20..=4096).contains(&token.len())
                || !token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err("The read-access token has an unexpected format.".into());
            }
            Ok(Self {
                kind: AuthKind::Token,
                value: token.into(),
            })
        } else if key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self {
                kind: AuthKind::Key,
                value: key.into(),
            })
        } else {
            Err("Enter your API key (32 hexadecimal characters) or API Read Access Token.".into())
        }
    }
}

#[derive(Default, Serialize, Deserialize, Clone, Debug)]
pub struct Supplement {
    pub collections: Vec<CollectionReference>,
    pub recommendations: Vec<RelatedTitle>,
    pub videos: Vec<Video>,
    pub cast: Vec<ExpandedCredit>,
    pub crew: Vec<ExpandedCredit>,
    pub source_url: String,
    pub poster_path: String,
    pub overview: String,
    pub tagline: String,
    pub release_date: String,
    pub runtime: String,
    pub genres: String,
    pub countries: String,
    pub languages: String,
    pub companies: String,
    pub status: String,
    pub budget: String,
    pub revenue: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectionReference {
    pub id: u64,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub id: u64,
    pub name: String,
    pub overview: String,
    pub movies: Vec<RelatedTitle>,
}
fn collection_references(value: &Value) -> Vec<CollectionReference> {
    let Some(id) = value["id"]
        .as_u64()
        .filter(|id| valid_media_id("movie", *id))
    else {
        return vec![];
    };
    let name = text(value, "name");
    vec![CollectionReference {
        id,
        name: if name.is_empty() {
            "Movie collection".into()
        } else {
            name
        },
    }]
}
fn parse_collection(value: &Value, id: u64) -> Result<Collection, String> {
    if value["id"].as_u64() != Some(id) {
        return Err("TMDB did not confirm the selected collection ID.".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut movies = Vec::new();
    for item in value["parts"].as_array().into_iter().flatten() {
        let Some(id) = item["id"]
            .as_u64()
            .filter(|id| valid_media_id("movie", *id))
        else {
            continue;
        };
        if item["adult"].as_bool() == Some(true)
            || item["media_type"].as_str().is_some_and(|s| s != "movie")
        {
            continue;
        }
        let title = text(item, "title");
        if title.is_empty() || !seen.insert(id) {
            continue;
        }
        let date = text(item, "release_date");
        let year = date
            .get(..4)
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or_default()
            .to_owned();
        let date = if year.is_empty() { String::new() } else { date };
        movies.push((
            date,
            RelatedTitle {
                id,
                media: "movie".into(),
                title,
                year,
            },
        ));
    }
    movies.sort_by(|a, b| {
        a.0.is_empty()
            .cmp(&b.0.is_empty())
            .then(a.0.cmp(&b.0))
            .then(a.1.title.cmp(&b.1.title))
            .then(a.1.id.cmp(&b.1.id))
    });
    let name = text(value, "name");
    Ok(Collection {
        id,
        name: if name.is_empty() {
            "Movie collection".into()
        } else {
            name
        },
        overview: text(value, "overview"),
        movies: movies.into_iter().map(|(_, m)| m).collect(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelatedTitle {
    pub id: u64,
    pub media: String,
    pub title: String,
    pub year: String,
}
impl RelatedTitle {
    pub fn local_url(&self) -> String {
        format!("/tmdb-title/{}/{}", self.media, self.id)
    }
    pub fn type_label(&self) -> &str {
        if self.media == "movie" {
            "Movie"
        } else {
            "TV series"
        }
    }
}
fn source_title_reference(url: &str) -> Option<(&str, u64)> {
    let (media, id) = url
        .strip_prefix("https://www.themoviedb.org/")?
        .split_once('/')?;
    let id = id.parse::<u64>().ok()?;
    valid_media_id(media, id).then_some((media, id))
}
fn recommendation_list(value: &Value, media: &str, current: u64) -> Vec<RelatedTitle> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in value["results"].as_array().into_iter().flatten() {
        let Some(id) = item["id"].as_u64() else {
            continue;
        };
        let kind = item["media_type"].as_str().unwrap_or(media);
        if !valid_media_id(kind, id)
            || item["adult"].as_bool() == Some(true)
            || (kind == media && id == current)
        {
            continue;
        }
        let title = text(item, if kind == "movie" { "title" } else { "name" });
        if title.is_empty() {
            continue;
        }
        if !seen.insert((kind.to_owned(), id)) {
            continue;
        }
        let date = text(
            item,
            if kind == "movie" {
                "release_date"
            } else {
                "first_air_date"
            },
        );
        let year = date
            .get(..4)
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or_default()
            .to_owned();
        result.push(RelatedTitle {
            id,
            media: kind.into(),
            title,
            year,
        });
        if result.len() == 8 {
            break;
        }
    }
    result
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Video {
    pub name: String,
    pub url: String,
    pub site: String,
    pub kind: String,
    pub official: bool,
}
fn trailers(value: &Value) -> Vec<Video> {
    let mut videos = Vec::new();
    for item in value["results"].as_array().into_iter().flatten() {
        let kind = text(item, "type");
        if !matches!(kind.as_str(), "Trailer" | "Teaser") {
            continue;
        }
        let site = text(item, "site");
        let key = text(item, "key");
        let url = match site.as_str() {
            "YouTube"
                if key.len() == 11
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) =>
            {
                format!("https://www.youtube.com/watch?v={key}")
            }
            "Vimeo" if (1..=12).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_digit()) => {
                format!("https://vimeo.com/{key}")
            }
            _ => continue,
        };
        let name = text(item, "name");
        videos.push(Video {
            name: if name.is_empty() { kind.clone() } else { name },
            url,
            site,
            kind,
            official: item["official"].as_bool() == Some(true),
        });
    }
    videos.sort_by_key(|v| (v.kind != "Trailer", !v.official));
    let mut seen = std::collections::HashSet::new();
    videos.retain(|v| seen.insert(v.url.clone()));
    videos.truncate(6);
    videos
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ExpandedCredit {
    pub id: u64,
    pub name: String,
    pub detail: String,
}
fn expanded_credits(value: &Value) -> (Vec<ExpandedCredit>, Vec<ExpandedCredit>) {
    let mut cast: Vec<ExpandedCredit> = vec![];
    let mut positions = std::collections::HashMap::new();
    for group in ["cast", "guest_stars"] {
        for item in value[group].as_array().into_iter().flatten() {
            let Some(id) = item["id"]
                .as_u64()
                .filter(|id| (1..=i32::MAX as u64).contains(id))
            else {
                continue;
            };
            let name = text(item, "name");
            if name.is_empty() {
                continue;
            }
            let character = text(item, "character");
            let detail = if group == "guest_stars" {
                format!(
                    "Guest{}{}",
                    if character.is_empty() { "" } else { " · as " },
                    character
                )
            } else if character.is_empty() {
                "Cast".into()
            } else {
                format!("as {character}")
            };
            let index = *positions.entry(id).or_insert_with(|| {
                cast.push(ExpandedCredit {
                    id,
                    name,
                    detail: String::new(),
                });
                cast.len() - 1
            });
            let entry = &mut cast[index];
            if !entry.detail.split(" / ").any(|s| s == detail) {
                if !entry.detail.is_empty() {
                    entry.detail.push_str(" / ");
                }
                entry.detail.push_str(&detail);
            }
        }
    }
    let mut crew = std::collections::BTreeMap::<(String, u64), ExpandedCredit>::new();
    for item in value["crew"].as_array().into_iter().flatten() {
        let Some(id) = item["id"]
            .as_u64()
            .filter(|id| (1..=i32::MAX as u64).contains(id))
        else {
            continue;
        };
        let name = text(item, "name");
        if name.is_empty() {
            continue;
        }
        let department = text(item, "department");
        let job = text(item, "job");
        let detail = if department.is_empty() {
            job
        } else if job.is_empty() {
            department.clone()
        } else {
            format!("{department} · {job}")
        };
        let entry = crew
            .entry((department, id))
            .or_insert_with(|| ExpandedCredit {
                id,
                name,
                detail: String::new(),
            });
        if !detail.is_empty() && !entry.detail.split(" / ").any(|s| s == detail) {
            if !entry.detail.is_empty() {
                entry.detail.push_str(" / ");
            }
            entry.detail.push_str(&detail);
        }
    }
    (cast, crew.into_values().collect())
}

impl Supplement {
    pub fn has_information(&self) -> bool {
        [
            &self.overview,
            &self.tagline,
            &self.release_date,
            &self.runtime,
            &self.genres,
            &self.countries,
            &self.languages,
            &self.companies,
            &self.status,
            &self.budget,
            &self.revenue,
        ]
        .iter()
        .any(|s| !s.is_empty())
    }
    pub fn preserve_imdb(&mut self, plot: bool, released: bool, runtime: bool, genres: bool) {
        if plot {
            self.overview.clear();
        }
        if released {
            self.release_date.clear();
        }
        if runtime {
            self.runtime.clear();
        }
        if genres {
            self.genres.clear();
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Person {
    pub profile_path: String,
    pub name: String,
    pub biography: String,
    pub birthday: String,
    pub deathday: String,
    pub birthplace: String,
    pub department: String,
    pub source_url: String,
    pub filmography: Vec<PersonCredit>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PersonCredit {
    pub title: String,
    pub date: String,
    pub media: String,
    pub roles: String,
    pub source_url: String,
}
pub fn valid_media_id(media: &str, id: u64) -> bool {
    matches!(media, "movie" | "tv") && (1..=i32::MAX as u64).contains(&id)
}
impl PersonCredit {
    pub fn local_url(&self) -> String {
        let Some((media, id)) = self
            .source_url
            .strip_prefix("https://www.themoviedb.org/")
            .and_then(|p| p.split_once('/'))
        else {
            return "/".into();
        };
        match id.parse::<u64>() {
            Ok(id) if valid_media_id(media, id) => format!("/tmdb-title/{media}/{id}"),
            _ => "/".into(),
        }
    }
}
fn external_person_id(value: &Value, id: u64) -> Result<Option<String>, String> {
    if value["id"].as_u64() != Some(id) {
        return Err("TMDB did not confirm the selected person ID.".into());
    }
    match value.get("imdb_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if crate::credits::person_id(s) => Ok(Some(s.clone())),
        _ => Err("TMDB returned an invalid IMDb person ID; no local person was opened.".into()),
    }
}

fn external_title_id(value: &Value, id: u64) -> Result<Option<String>, String> {
    if value["id"].as_u64() != Some(id) {
        return Err("TMDB did not confirm the selected title ID.".into());
    }
    match value.get("imdb_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if crate::imdb::valid_title_id(s) => Ok(Some(s.clone())),
        _ => Err("TMDB returned an invalid IMDb title ID; no local title was opened.".into()),
    }
}

fn parse_person(value: &Value, imdb_id: &str, id: u64) -> Result<Person, String> {
    if value["imdb_id"].as_str() != Some(imdb_id) || value["id"].as_u64() != Some(id) {
        return Err("TMDB did not confirm the requested IMDb person ID.".into());
    }
    let mut credits = std::collections::BTreeMap::<(String, u64), PersonCredit>::new();
    for group in ["cast", "crew"] {
        for item in value["combined_credits"][group]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(id) = item["id"].as_u64().filter(|id| *id > 0) else {
                continue;
            };
            let media = text(item, "media_type");
            if !matches!(media.as_str(), "movie" | "tv") || item["adult"].as_bool() == Some(true) {
                continue;
            }
            let title = if media == "movie" {
                text(item, "title")
            } else {
                text(item, "name")
            };
            if title.is_empty() {
                continue;
            }
            let date = if media == "movie" {
                text(item, "release_date")
            } else {
                text(item, "first_air_date")
            };
            let role = if group == "cast" {
                let c = text(item, "character");
                if c.is_empty() {
                    "Cast".into()
                } else {
                    format!("as {c}")
                }
            } else {
                text(item, "job")
            };
            let credit = credits
                .entry((media.clone(), id))
                .or_insert_with(|| PersonCredit {
                    title,
                    date,
                    media: if media == "movie" {
                        "Movie".into()
                    } else {
                        "TV".into()
                    },
                    roles: String::new(),
                    source_url: format!("https://www.themoviedb.org/{media}/{id}"),
                });
            if !role.is_empty() && !credit.roles.split(" · ").any(|r| r == role) {
                if !credit.roles.is_empty() {
                    credit.roles.push_str(" · ");
                }
                credit.roles.push_str(&role);
            }
        }
    }
    let mut filmography: Vec<_> = credits.into_values().collect();
    filmography.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then(a.title.cmp(&b.title))
            .then(a.source_url.cmp(&b.source_url))
    });
    Ok(Person {
        profile_path: text(value, "profile_path"),
        name: text(value, "name"),
        biography: text(value, "biography"),
        birthday: text(value, "birthday"),
        deathday: text(value, "deathday"),
        birthplace: text(value, "place_of_birth"),
        department: text(value, "known_for_department"),
        source_url: format!("https://www.themoviedb.org/person/{id}"),
        filmography,
    })
}

#[derive(Clone)]
struct Candidate {
    path: String,
    url: String,
}
fn candidate(value: &Value, kind: Option<&str>) -> Option<Candidate> {
    let groups = [
        ("movie_results", "movie"),
        ("tv_results", "tv"),
        ("tv_episode_results", "episode"),
    ];
    let mut matches = Vec::new();
    for (field, media) in groups {
        if kind.is_some_and(|kind| kind != media) {
            continue;
        }
        for item in value[field].as_array().into_iter().flatten() {
            let Some(id) = item["id"].as_u64().filter(|id| *id > 0) else {
                continue;
            };
            let (path, url) = if media == "episode" {
                let show = item["show_id"].as_u64().filter(|id| *id > 0)?;
                let season = item["season_number"].as_u64()?;
                let episode = item["episode_number"].as_u64()?;
                (
                    format!("tv/{show}/season/{season}/episode/{episode}"),
                    format!(
                        "https://www.themoviedb.org/tv/{show}/season/{season}/episode/{episode}"
                    ),
                )
            } else {
                (
                    format!("{media}/{id}"),
                    format!("https://www.themoviedb.org/{media}/{id}"),
                )
            };
            matches.push(Candidate { path, url });
        }
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().trim().to_owned()
}
fn names(value: &Value, key: &str, field: &str) -> String {
    value[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v[field].as_str())
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}
fn usd(value: &Value, key: &str) -> String {
    let Some(number) = value[key].as_u64().filter(|v| *v > 0) else {
        return String::new();
    };
    let digits = number.to_string();
    let mut output = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    format!("USD {output}")
}
fn parse_details(value: &Value, imdb_id: &str, source_url: String) -> Result<Supplement, String> {
    let returned = value["external_ids"]["imdb_id"]
        .as_str()
        .or_else(|| value["imdb_id"].as_str());
    if returned != Some(imdb_id) {
        return Err(
            "TMDB did not confirm the requested IMDb ID; no supplemental data was used.".into(),
        );
    }
    let runtime = if let Some(minutes) = value["runtime"].as_u64().filter(|v| *v > 0) {
        format!("{minutes} minutes")
    } else {
        let values = value["episode_run_time"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_u64())
            .filter(|v| *v > 0)
            .map(|v| v.to_string())
            .collect::<Vec<_>>();
        if values.is_empty() {
            String::new()
        } else {
            format!("{} minutes", values.join(", "))
        }
    };
    let countries = names(value, "production_countries", "name");
    let countries = if countries.is_empty() {
        value["origin_country"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        countries
    };
    let release_date = ["release_date", "first_air_date", "air_date"]
        .iter()
        .map(|key| text(value, key))
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let (cast, crew) = expanded_credits(&value["credits"]);
    let recommendations = source_title_reference(&source_url)
        .map(|(media, current)| recommendation_list(&value["recommendations"], media, current))
        .unwrap_or_default();
    let collections =
        if source_title_reference(&source_url).is_some_and(|(media, _)| media == "movie") {
            collection_references(&value["belongs_to_collection"])
        } else {
            vec![]
        };
    Ok(Supplement {
        collections,
        recommendations,
        videos: trailers(&value["videos"]),
        cast,
        crew,
        source_url,
        poster_path: text(value, "poster_path"),
        overview: text(value, "overview"),
        tagline: text(value, "tagline"),
        release_date,
        runtime,
        genres: names(value, "genres", "name"),
        countries,
        languages: names(value, "spoken_languages", "english_name"),
        companies: names(value, "production_companies", "name"),
        status: text(value, "status"),
        budget: usd(value, "budget"),
        revenue: usd(value, "revenue"),
    })
}

pub struct Service {
    config: PathBuf,
    database: PathBuf,
    client: reqwest::Client,
    base: String,
    credentials: RwLock<Option<Credentials>>,
    retry_after: Mutex<Option<Instant>>,
}
impl Service {
    pub fn open(data: &Path) -> Result<Arc<Self>, Error> {
        Self::with_base(data, "https://api.themoviedb.org/3/".into())
    }
    fn with_base(data: &Path, base: String) -> Result<Arc<Self>, Error> {
        let config = data.join("tmdb-credentials.json");
        let credentials = match fs::read(&config) {
            Ok(bytes) => match serde_json::from_slice::<Credentials>(&bytes) {
                Ok(c) => match c.kind {
                    AuthKind::Key => Credentials::new(&c.value, "").ok(),
                    AuthKind::Token => Credentials::new("", &c.value).ok(),
                },
                Err(_) => {
                    eprintln!(
                        "TMDB configuration is invalid; supplementation is disabled until Options is updated."
                    );
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                eprintln!("TMDB configuration could not be read; supplementation is disabled.");
                None
            }
        };
        let builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5));
        #[cfg(test)]
        let builder = builder.no_proxy();
        Ok(Arc::new(Self {
            config,
            database: data.join("rustymdb.sqlite3"),
            client: builder.build()?,
            base,
            credentials: RwLock::new(credentials),
            retry_after: Mutex::new(None),
        }))
    }
    fn credentials(&self) -> Option<Credentials> {
        self.credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn method(&self) -> String {
        self.credentials()
            .map(|c| {
                match c.kind {
                    AuthKind::Key => "API key (v3)",
                    AuthKind::Token => "API Read Access Token",
                }
                .to_string()
            })
            .unwrap_or_default()
    }
    async fn request(
        &self,
        path: &str,
        parameters: &[(&str, &str)],
        credentials: &Credentials,
    ) -> Result<Value, String> {
        let url = format!("{}{path}", self.base);
        let mut request = self.client.get(url).query(parameters);
        request = match credentials.kind {
            AuthKind::Key => request.query(&[("api_key", credentials.value.as_str())]),
            AuthKind::Token => request.bearer_auth(&credentials.value),
        };
        // Never return reqwest errors or provider response bodies: they may contain a key-bearing URL.
        let mut response = request.send().await.map_err(|_| {
            "TMDB could not be reached. Check the connection and try again.".to_string()
        })?;
        if !response.status().is_success() {
            return Err(match response.status().as_u16() {
                401 | 403 => "TMDB rejected the credentials. Check Options.",
                429 => "TMDB is rate limiting requests. Try again later.",
                _ => "TMDB is temporarily unavailable.",
            }
            .into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "TMDB response could not be read.")?
        {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err("TMDB response exceeded the size limit.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "TMDB returned an unexpected response.".into())
    }
    async fn verify(&self, credentials: &Credentials) -> Result<(), String> {
        let result = self.request("authentication", &[], credentials).await?;
        if result["success"].as_bool() != Some(true) {
            return Err("TMDB did not validate these credentials.".into());
        }
        Ok(())
    }
    pub async fn save(&self, key: &str, token: &str) -> Result<(), String> {
        if key.trim().is_empty() && token.trim().is_empty() && self.credentials().is_some() {
            return self.test().await;
        }
        let credentials = Credentials::new(key, token)?;
        self.verify(&credentials).await?;
        let bytes =
            serde_json::to_vec(&credentials).map_err(|_| "Could not prepare TMDB settings.")?;
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|_| "Could not save TMDB settings.")?;
        let suffix = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let temporary = self
            .config
            .with_file_name(format!(".tmdb-{suffix}.partial"));
        let saved = (|| -> Result<(), Error> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.config)?;
            Ok(())
        })();
        if saved.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err("TMDB settings could not be saved on the server.".into());
        }
        *self.credentials.write().unwrap_or_else(|e| e.into_inner()) = Some(credentials);
        *self.retry_after.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
    pub async fn test(&self) -> Result<(), String> {
        let credentials = self
            .credentials()
            .ok_or("Save an API key or read-access token first.")?;
        self.verify(&credentials).await
    }
    pub fn remove(&self) -> Result<(), String> {
        match fs::remove_file(&self.config) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("TMDB credentials could not be removed.".into()),
        }
        *self.credentials.write().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
    pub async fn collection(&self, id: u64) -> Result<Option<Collection>, String> {
        if !valid_media_id("movie", id) {
            return Err("Invalid collection ID.".into());
        }
        let Some(credentials) = self.credentials() else {
            return Ok(None);
        };
        let database = self.database.clone();
        if let Ok(Ok(Some(payload)))=tokio::task::spawn_blocking(move||Connection::open(database)?.query_row("SELECT payload FROM tmdb_collection_cache WHERE collection_id=?1 AND expires_at>unixepoch()",[id as i64],|r|r.get::<_,String>(0)).optional()).await
            && let Ok(collection)=serde_json::from_str::<Collection>(&payload) && collection.id==id {return Ok(Some(collection));}
        if self
            .retry_after
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > Instant::now())
        {
            return Err(
                "TMDB is temporarily unavailable. Try the collection again shortly.".into(),
            );
        }
        let result = async {
            let value = self
                .request(
                    &format!("collection/{id}"),
                    &[("language", "en-US")],
                    &credentials,
                )
                .await?;
            parse_collection(&value, id)
        }
        .await;
        if result.is_err() {
            *self.retry_after.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + Duration::from_secs(60));
        }
        if let Ok(collection) = &result
            && let Ok(payload) = serde_json::to_string(collection)
        {
            let database = self.database.clone();
            let _=tokio::task::spawn_blocking(move||->rusqlite::Result<()> {
                let c=Connection::open(database)?;c.busy_timeout(Duration::from_millis(100))?;
                c.execute("INSERT INTO tmdb_collection_cache VALUES (?1,?2,unixepoch()+86400) ON CONFLICT(collection_id) DO UPDATE SET payload=excluded.payload,expires_at=excluded.expires_at",rusqlite::params![id as i64,payload])?;Ok(())
            }).await;
        }
        result.map(Some)
    }

    pub async fn resolve_title(&self, media: &str, id: u64) -> Result<Option<String>, String> {
        if !valid_media_id(media, id) {
            return Err("Invalid title reference.".into());
        }
        self.resolve_link(media, id).await
    }
    pub async fn resolve_person(&self, id: u64) -> Result<Option<String>, String> {
        if !(1..=i32::MAX as u64).contains(&id) {
            return Err("Invalid person reference.".into());
        }
        self.resolve_link("person", id).await
    }
    async fn resolve_link(&self, media: &str, id: u64) -> Result<Option<String>, String> {
        let Some(credentials) = self.credentials() else {
            return Ok(None);
        };
        let valid_id = if media == "person" {
            crate::credits::person_id
        } else {
            crate::imdb::valid_title_id
        };
        let database = self.database.clone();
        let cache_media = media.to_owned();
        if let Ok(Ok(Some(cached)))=tokio::task::spawn_blocking(move||Connection::open(database)?.query_row(
            "SELECT imdb_id FROM tmdb_title_links WHERE media=?1 AND tmdb_id=?2 AND expires_at>unixepoch()",rusqlite::params![cache_media,id as i64],|r|r.get::<_,Option<String>>(0)).optional()).await
            && cached.as_deref().is_none_or(valid_id) {
            return Ok(cached);
        }
        if self
            .retry_after
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > Instant::now())
        {
            return Err("TMDB is temporarily unavailable. Try this link again shortly.".into());
        }
        let result = async {
            let value = self
                .request(&format!("{media}/{id}/external_ids"), &[], &credentials)
                .await?;
            if media == "person" {
                external_person_id(&value, id)
            } else {
                external_title_id(&value, id)
            }
        }
        .await;
        if result.is_err() {
            *self.retry_after.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + Duration::from_secs(60));
        }
        if let Ok(imdb_id) = &result {
            let imdb_id = imdb_id.clone();
            let media = media.to_owned();
            let database = self.database.clone();
            let ttl = if imdb_id.is_some() { 86400 } else { 3600 };
            let _=tokio::task::spawn_blocking(move||->rusqlite::Result<()> {
                let c=Connection::open(database)?;c.busy_timeout(Duration::from_millis(100))?;
                c.execute("INSERT INTO tmdb_title_links VALUES (?1,?2,?3,unixepoch()+?4) ON CONFLICT(media,tmdb_id) DO UPDATE SET imdb_id=excluded.imdb_id,expires_at=excluded.expires_at",rusqlite::params![media,id as i64,imdb_id,ttl])?;Ok(())
            }).await;
        }
        result
    }

    pub async fn person(&self, imdb_id: &str) -> Result<Option<Person>, String> {
        let Some(credentials) = self.credentials() else {
            return Ok(None);
        };
        if !crate::credits::person_id(imdb_id) {
            return Ok(None);
        }
        let database = self.database.clone();
        let key = imdb_id.to_owned();
        if let Ok(Ok(Some(payload))) = tokio::task::spawn_blocking(move || {
            Connection::open(database)?.query_row("SELECT payload FROM tmdb_person_cache WHERE imdb_id=?1 AND expires_at>unixepoch()", [key], |r| r.get::<_,String>(0)).optional()
        }).await && let Ok(person) = serde_json::from_str::<Option<Person>>(&payload) {return Ok(person);}
        if self
            .retry_after
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > Instant::now())
        {
            return Err(
                "TMDB is temporarily unavailable. IMDb information remains available.".into(),
            );
        }
        let result = async {
            let found = self
                .request(
                    &format!("find/{imdb_id}"),
                    &[("external_source", "imdb_id")],
                    &credentials,
                )
                .await?;
            let ids: Vec<_> = found["person_results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["id"].as_u64().filter(|id| *id > 0))
                .collect();
            if ids.len() != 1 {
                return Ok(None);
            }
            let id = ids[0];
            let details = self
                .request(
                    &format!("person/{id}"),
                    &[
                        ("append_to_response", "combined_credits"),
                        ("language", "en-US"),
                    ],
                    &credentials,
                )
                .await?;
            parse_person(&details, imdb_id, id).map(Some)
        }
        .await;
        if result.is_err() {
            *self.retry_after.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + Duration::from_secs(60));
        }
        if let Ok(person) = &result
            && let Ok(payload) = serde_json::to_string(person)
        {
            let database = self.database.clone();
            let key = imdb_id.to_owned();
            let ttl = if person.is_some() { 86400 } else { 3600 };
            let _=tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
                let c=Connection::open(database)?;c.busy_timeout(Duration::from_millis(100))?;
                c.execute("INSERT INTO tmdb_person_cache VALUES (?1,?2,unixepoch()+?3) ON CONFLICT(imdb_id) DO UPDATE SET payload=excluded.payload,expires_at=excluded.expires_at",rusqlite::params![key,payload,ttl])?;Ok(())
            }).await;
        }
        result
    }

    pub async fn supplement(
        &self,
        id: &str,
        kind: Option<&str>,
    ) -> Result<Option<Supplement>, String> {
        let Some(credentials) = self.credentials() else {
            return Ok(None);
        };
        if !crate::imdb::valid_title_id(id) {
            return Ok(None);
        };
        let database = self.database.clone();
        let cache_id = id.to_string();
        if let Ok(Ok(Some(payload))) = tokio::task::spawn_blocking(move || {
            Connection::open(database)?
                .query_row(
                    "SELECT payload FROM tmdb_cache WHERE imdb_id=?1 AND expires_at>unixepoch()",
                    [cache_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
        })
        .await
            && let Ok(value) = serde_json::from_str::<Option<Supplement>>(&payload)
        {
            return Ok(value);
        }
        if self
            .retry_after
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > Instant::now())
        {
            return Err("TMDB is temporarily unavailable; IMDb information is still shown.".into());
        }
        let result = async {
            let found = self
                .request(
                    &format!("find/{id}"),
                    &[("external_source", "imdb_id"), ("language", "en-US")],
                    &credentials,
                )
                .await?;
            let Some(candidate) = candidate(&found, kind) else {
                return Ok(None);
            };
            let details = self
                .request(
                    &candidate.path,
                    &[
                        (
                            "append_to_response",
                            if candidate.path.split('/').count() == 2 {
                                "external_ids,credits,videos,recommendations"
                            } else {
                                "external_ids,credits,videos"
                            },
                        ),
                        ("language", "en-US"),
                    ],
                    &credentials,
                )
                .await?;
            parse_details(&details, id, candidate.url).map(Some)
        }
        .await;
        if result.is_err() {
            *self.retry_after.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + Duration::from_secs(60));
        }
        if let Ok(value) = &result
            && let Ok(payload) = serde_json::to_string(value)
        {
            let database = self.database.clone();
            let id = id.to_owned();
            let ttl = if value.is_some() { 86400 } else { 3600 };
            let _=tokio::task::spawn_blocking(move||->rusqlite::Result<()>{let c=Connection::open(database)?;c.busy_timeout(Duration::from_millis(100))?;
                    c.execute("INSERT INTO tmdb_cache VALUES (?1,?2,unixepoch()+?3) ON CONFLICT(imdb_id) DO UPDATE SET payload=excluded.payload,expires_at=excluded.expires_at",rusqlite::params![id,payload,ttl])?;Ok(())}).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let path = std::env::temp_dir().join(format!("rustymdb-tmdb-{suffix}"));
            fs::create_dir(&path).unwrap();
            crate::database::initialize(&path.join("rustymdb.sqlite3")).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    async fn mock(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = vec![];
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![];
                loop {
                    let mut chunk = [0u8; 2048];
                    let n = stream.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&bytes).to_string());
                let response = format!(
                    "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}/3/"), task)
    }
    #[test]
    fn collections_sort_release_dates_and_reject_invalid_or_duplicate_parts() {
        let value = serde_json::json!({"id":42,"name":"Example Collection","parts":[
            {"id":20,"title":"Later","release_date":"2020-02-01"},
            {"id":10,"title":"Earlier","release_date":"2000-01-01"},
            {"id":10,"title":"Duplicate"},
            {"id":30,"title":"Undated"},
            {"id":40,"title":"Adult","adult":true},
            {"id":0,"title":"Invalid"},
            {"id":50,"media_type":"tv","title":"Not a movie"}]});
        let collection = parse_collection(&value, 42).unwrap();
        assert_eq!(collection.movies.len(), 3);
        assert_eq!(collection.movies[0].title, "Earlier");
        assert_eq!(collection.movies[2].title, "Undated");
        assert_eq!(collection.movies[0].local_url(), "/tmdb-title/movie/10");
        assert!(parse_collection(&value, 43).is_err());
        let details = serde_json::json!({"imdb_id":"tt1375666","belongs_to_collection":{"id":42,"name":"Example Collection"}});
        assert_eq!(
            parse_details(
                &details,
                "tt1375666",
                "https://www.themoviedb.org/movie/27205".into()
            )
            .unwrap()
            .collections
            .len(),
            1
        );
        assert!(
            parse_details(
                &details,
                "tt1375666",
                "https://www.themoviedb.org/tv/42".into()
            )
            .unwrap()
            .collections
            .is_empty()
        );
    }
    #[tokio::test]
    async fn collection_requests_are_explicit_cached_and_credential_gated() {
        let directory = Directory::new();
        let disabled = Service::with_base(&directory.0, "http://127.0.0.1:9/3/".into()).unwrap();
        assert!(disabled.collection(42).await.unwrap().is_none());
        drop(disabled);
        let (base,server)=mock(vec![(200,r#"{"success":true}"#),(200,r#"{"id":42,"name":"Example Collection","parts":[{"id":10,"title":"Example film","release_date":"2000-01-01"}]}"#)]).await;
        let service = Service::with_base(&directory.0, base).unwrap();
        service
            .save("", "test_read_access_token_12345")
            .await
            .unwrap();
        assert_eq!(
            service.collection(42).await.unwrap().unwrap().movies.len(),
            1
        );
        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /3/collection/42?"));
        assert_eq!(
            service.collection(42).await.unwrap().unwrap().name,
            "Example Collection"
        );
        service.remove().unwrap();
        assert!(service.collection(42).await.unwrap().is_none());
    }
    #[test]
    fn recommendations_preserve_order_and_reject_bad_or_duplicate_entries() {
        let value = serde_json::json!({"results":[
            {"id":42,"title":"Self"},
            {"id":10,"title":"First movie","release_date":"2014-11-07"},
            {"id":10,"title":"Duplicate"},
            {"id":10,"media_type":"tv","name":"A series","first_air_date":"2020-01-01"},
            {"id":11,"title":"Adult","adult":true},
            {"id":0,"title":"Invalid ID"},
            {"id":12,"media_type":"person","name":"Not a title"},
            {"id":13,"title":""},
            {"id":14,"title":"Undated","release_date":"unknown"}]});
        let titles = recommendation_list(&value, "movie", 42);
        assert_eq!(titles.len(), 3);
        assert_eq!(titles[0].title, "First movie");
        assert_eq!(titles[0].year, "2014");
        assert_eq!(titles[0].local_url(), "/tmdb-title/movie/10");
        assert_eq!(titles[1].local_url(), "/tmdb-title/tv/10");
        assert_eq!(titles[1].type_label(), "TV series");
        assert!(titles[2].year.is_empty());
        let rows: Vec<_> = (100..120)
            .map(|id| serde_json::json!({"id":id,"name":"Series"}))
            .collect();
        assert_eq!(
            recommendation_list(&serde_json::json!({"results":rows}), "tv", 42).len(),
            8
        );
        assert!(
            source_title_reference("https://www.themoviedb.org/tv/42/season/1/episode/1").is_none()
        );
    }
    #[tokio::test]
    async fn recommendation_supplements_use_verified_titles_and_are_cached() {
        let directory = Directory::new();
        let (base,server)=mock(vec![(200,r#"{"success":true}"#),(200,r#"{"movie_results":[{"id":27205}]}"#),(200,r#"{"id":27205,"imdb_id":"tt1375666","recommendations":{"results":[{"id":157336,"title":"Interstellar","release_date":"2014-11-07"}]}}"#)]).await;
        let service = Service::with_base(&directory.0, base).unwrap();
        service
            .save("", "test_read_access_token_12345")
            .await
            .unwrap();
        let supplement = service
            .supplement("tt1375666", Some("movie"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            supplement.recommendations[0].local_url(),
            "/tmdb-title/movie/157336"
        );
        let requests = server.await.unwrap();
        assert!(requests[2].contains("recommendations"));
        assert_eq!(
            service
                .supplement("tt1375666", Some("movie"))
                .await
                .unwrap()
                .unwrap()
                .recommendations
                .len(),
            1
        );
        service.remove().unwrap();
        assert!(
            service
                .supplement("tt1375666", Some("movie"))
                .await
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn trailers_validate_hosts_keys_types_and_prefer_official_trailers() {
        let value = serde_json::json!({"results":[
            {"name":"Teaser","site":"YouTube","key":"abcdefghij1","type":"Teaser","official":true},
            {"name":"Unofficial","site":"YouTube","key":"abcdefghij2","type":"Trailer"},
            {"name":"Official","site":"YouTube","key":"abcdefghij3","type":"Trailer","official":true},
            {"name":"Duplicate","site":"YouTube","key":"abcdefghij3","type":"Trailer"},
            {"name":"Vimeo trailer","site":"Vimeo","key":"123456789","type":"Trailer"},
            {"site":"YouTube","key":"https://evil.example","type":"Trailer"},
            {"site":"Vimeo","key":"123?evil=1","type":"Trailer"},
            {"site":"Other","key":"abcdefghij4","type":"Trailer"},
            {"site":"YouTube","key":"abcdefghij5","type":"Clip"}]});
        let videos = trailers(&value);
        assert_eq!(videos.len(), 4);
        assert_eq!(videos[0].name, "Official");
        assert_eq!(videos[0].url, "https://www.youtube.com/watch?v=abcdefghij3");
        assert_eq!(videos[3].kind, "Teaser");
        assert!(
            videos
                .iter()
                .any(|v| v.url == "https://vimeo.com/123456789")
        );
        assert!(trailers(&Value::Null).is_empty());
    }
    #[test]
    fn trailer_lists_are_bounded_and_missing_names_have_a_label() {
        let rows: Vec<_> = (0..20)
            .map(|i| serde_json::json!({"site":"Vimeo","key":format!("{}",100+i),"type":"Trailer"}))
            .collect();
        let videos = trailers(&serde_json::json!({"results":rows}));
        assert_eq!(videos.len(), 6);
        assert_eq!(videos[0].name, "Trailer");
    }
    #[test]
    fn expanded_cast_and_crew_merge_roles_and_keep_provider_order() {
        let value = serde_json::json!({"cast":[
            {"id":10,"name":"Actor One","character":"First"},
            {"id":11,"name":"Actor Two","character":""},
            {"id":10,"name":"Actor One","character":"Second"},
            {"id":0,"name":"Invalid"}],
            "guest_stars":[{"id":12,"name":"Guest","character":"Visitor"}],
            "crew":[{"id":20,"name":"Crew Member","department":"Writing","job":"Screenplay"},
            {"id":20,"name":"Crew Member","department":"Writing","job":"Story"},
            {"id":20,"name":"Crew Member","department":"Writing","job":"Story"},
            {"id":21,"name":"Director","department":"Directing","job":"Director"}]});
        let (cast, crew) = expanded_credits(&value);
        assert_eq!(cast.len(), 3);
        assert_eq!(cast[0].id, 10);
        assert_eq!(cast[0].detail, "as First / as Second");
        assert_eq!(cast[2].detail, "Guest · as Visitor");
        assert_eq!(crew.len(), 2);
        assert_eq!(crew[0].name, "Director");
        assert_eq!(crew[1].detail, "Writing · Screenplay / Writing · Story");
        assert!(
            external_person_id(&serde_json::json!({"id":42,"imdb_id":"tt1375666"}), 42).is_err()
        );
        assert!(
            external_person_id(&serde_json::json!({"id":43,"imdb_id":"nm0000138"}), 42).is_err()
        );
    }
    #[tokio::test]
    async fn expanded_credit_people_resolve_exact_ids_and_cache_links() {
        let directory = Directory::new();
        let disabled = Service::with_base(&directory.0, "http://127.0.0.1:9/3/".into()).unwrap();
        assert!(disabled.resolve_person(6193).await.unwrap().is_none());
        drop(disabled);
        let (base, server) = mock(vec![
            (200, r#"{"success":true}"#),
            (200, r#"{"id":6193,"imdb_id":"nm0000138"}"#),
            (200, r#"{"id":42,"imdb_id":null}"#),
        ])
        .await;
        let service = Service::with_base(&directory.0, base).unwrap();
        service
            .save("", "test_read_access_token_12345")
            .await
            .unwrap();
        assert_eq!(
            service.resolve_person(6193).await.unwrap().as_deref(),
            Some("nm0000138")
        );
        assert!(service.resolve_person(42).await.unwrap().is_none());
        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /3/person/6193/external_ids"));
        assert_eq!(
            service.resolve_person(6193).await.unwrap().as_deref(),
            Some("nm0000138")
        );
        assert!(service.resolve_person(42).await.unwrap().is_none());
        service.remove().unwrap();
        assert!(service.resolve_person(6193).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn title_links_validate_identity_cache_both_media_types_and_require_credentials() {
        let directory = Directory::new();
        let disabled = Service::with_base(&directory.0, "http://127.0.0.1:9/3/".into()).unwrap();
        assert!(disabled.resolve_title("movie", 42).await.unwrap().is_none());
        drop(disabled);
        let (base, server) = mock(vec![
            (200, r#"{"success":true}"#),
            (200, r#"{"id":42,"imdb_id":"tt1375666"}"#),
            (200, r#"{"id":42,"imdb_id":"tt0903747"}"#),
            (200, r#"{"id":43,"imdb_id":null}"#),
            (200, r#"{"id":45,"imdb_id":"tt1375666"}"#),
        ])
        .await;
        let service = Service::with_base(&directory.0, base).unwrap();
        service
            .save("", "test_read_access_token_12345")
            .await
            .unwrap();
        assert_eq!(
            service.resolve_title("movie", 42).await.unwrap().as_deref(),
            Some("tt1375666")
        );
        assert_eq!(
            service.resolve_title("tv", 42).await.unwrap().as_deref(),
            Some("tt0903747")
        );
        assert!(service.resolve_title("movie", 43).await.unwrap().is_none());
        assert!(service.resolve_title("movie", 44).await.is_err());
        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /3/movie/42/external_ids"));
        assert!(requests[2].starts_with("GET /3/tv/42/external_ids"));
        assert_eq!(
            service.resolve_title("movie", 42).await.unwrap().as_deref(),
            Some("tt1375666")
        );
        assert!(service.resolve_title("movie", 43).await.unwrap().is_none());
        service.remove().unwrap();
        assert!(service.resolve_title("movie", 42).await.unwrap().is_none());
    }
    #[test]
    fn filmography_targets_never_guess_or_accept_arbitrary_urls() {
        let credit = |url: &str| PersonCredit {
            title: "Example".into(),
            date: String::new(),
            media: "Movie".into(),
            roles: String::new(),
            source_url: url.into(),
        };
        assert_eq!(
            credit("https://www.themoviedb.org/movie/27205").local_url(),
            "/tmdb-title/movie/27205"
        );
        assert_eq!(
            credit("https://www.themoviedb.org/tv/1396").local_url(),
            "/tmdb-title/tv/1396"
        );
        for bad in [
            "https://evil.example/movie/42",
            "https://www.themoviedb.org/person/42",
            "https://www.themoviedb.org/movie/0",
            "https://www.themoviedb.org/movie/42/extra",
            "https://www.themoviedb.org/movie/42?redirect=evil",
        ] {
            assert_eq!(credit(bad).local_url(), "/");
        }
        assert!(
            external_title_id(&serde_json::json!({"id":42,"imdb_id":"tt1375666"}), 42)
                .unwrap()
                .is_some()
        );
        assert!(
            external_title_id(&serde_json::json!({"id":42,"imdb_id":"../../evil"}), 42).is_err()
        );
        assert!(
            external_title_id(&serde_json::json!({"id":43,"imdb_id":"tt1375666"}), 42).is_err()
        );
    }
    #[test]
    fn person_identity_roles_and_filmography_order_are_preserved() {
        let value = serde_json::json!({"id":42,"imdb_id":"nm0000138","name":"Provider Name","biography":"A biography",
            "combined_credits":{"cast":[
                {"id":10,"media_type":"movie","title":"Older film","release_date":"2001-01-01","character":"Lead"},
                {"id":10,"media_type":"movie","title":"Older film","release_date":"2001-01-01","character":"Lead"},
                {"id":10,"media_type":"tv","name":"New series","first_air_date":"2020-02-01","character":"Self"},
                {"id":11,"media_type":"movie","title":"Undated"},
                {"id":12,"media_type":"movie","title":"Adult entry","adult":true},
                {"id":13,"media_type":"unknown","title":"Invalid media"}],
                "crew":[{"id":10,"media_type":"movie","title":"Older film","release_date":"2001-01-01","job":"Producer"}]}});
        let person = parse_person(&value, "nm0000138", 42).unwrap();
        assert_eq!(person.filmography.len(), 3);
        assert_eq!(person.filmography[0].title, "New series");
        assert_eq!(person.filmography[1].roles, "as Lead · Producer");
        assert_eq!(person.filmography[2].title, "Undated");
        assert!(parse_person(&value, "nm0000001", 42).is_err());
        assert!(parse_person(&value, "nm0000138", 43).is_err());
    }
    #[tokio::test]
    async fn person_requests_use_exact_ids_cache_and_require_configuration() {
        let directory = Directory::new();
        let disabled = Service::with_base(&directory.0, "http://127.0.0.1:9/3/".into()).unwrap();
        assert!(disabled.person("nm0000138").await.unwrap().is_none());
        drop(disabled);
        let (base,server)=mock(vec![
            (200,r#"{"success":true}"#),
            (200,r#"{"person_results":[{"id":6193}]}"#),
            (200,r#"{"id":6193,"imdb_id":"nm0000138","name":"Leonardo DiCaprio","profile_path":"/profile.jpg","biography":"Fixture biography","combined_credits":{"cast":[],"crew":[]}}"#)
        ]).await;
        let service = Service::with_base(&directory.0, base).unwrap();
        service
            .save("", "test_read_access_token_12345")
            .await
            .unwrap();
        assert_eq!(
            service
                .person("nm0000138")
                .await
                .unwrap()
                .unwrap()
                .biography,
            "Fixture biography"
        );
        assert_eq!(
            service
                .person("nm0000138")
                .await
                .unwrap()
                .unwrap()
                .profile_path,
            "/profile.jpg"
        );
        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /3/find/nm0000138?external_source=imdb_id"));
        assert!(requests[2].contains("/3/person/6193?append_to_response=combined_credits"));
        assert!(service.person("nm0000138").await.unwrap().is_some());
        service.remove().unwrap();
        assert!(service.person("nm0000138").await.unwrap().is_none());
    }
    #[tokio::test]
    async fn disabled_provider_never_requests_data_and_tokens_are_saved_privately() {
        let directory = Directory::new();
        let disabled = Service::with_base(&directory.0, "http://127.0.0.1:9/3/".into()).unwrap();
        assert!(
            disabled
                .supplement("tt1375666", Some("movie"))
                .await
                .unwrap()
                .is_none()
        );
        drop(disabled);
        let(base,server)=mock(vec![(200,r#"{"success":true}"#),(200,r#"{"movie_results":[{"id":27205}]}"#),(200,r#"{"imdb_id":"tt1375666","overview":"A dream story","runtime":148,"budget":160000000}"#)]).await;
        let service = Service::with_base(&directory.0, base).unwrap();
        service
            .save("", "test_read_access_token_12345")
            .await
            .unwrap();
        let data = service
            .supplement("tt1375666", Some("movie"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data.overview, "A dream story");
        assert_eq!(data.budget, "USD 160,000,000");
        let requests = server.await.unwrap();
        assert!(requests.iter().all(|r| {
            r.to_ascii_lowercase()
                .contains("authorization: bearer test_read_access_token_12345")
        }));
        assert!(requests[1].starts_with("GET /3/find/tt1375666?"));
        assert!(
            service
                .supplement("tt1375666", Some("movie"))
                .await
                .unwrap()
                .is_some()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&service.config).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        service.remove().unwrap();
        assert!(service.method().is_empty());
        assert!(
            service
                .supplement("tt1375666", Some("movie"))
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn invalid_credentials_do_not_persist_or_leak_into_errors() {
        let directory = Directory::new();
        let (base, server) = mock(vec![(401, r#"{"status_message":"bad key"}"#)]).await;
        let service = Service::with_base(&directory.0, base).unwrap();
        let key = "1234567890abcdef1234567890abcdef";
        let error = service.save(key, "").await.unwrap_err();
        assert!(!error.contains(key));
        assert!(service.method().is_empty());
        assert!(!service.config.exists());
        assert!(server.await.unwrap()[0].contains("api_key="));
    }
    #[test]
    fn exact_identity_and_imdb_precedence_are_preserved() {
        let data = serde_json::json!({"movie_results":[{"id":1}],"tv_results":[{"id":2}]});
        assert!(candidate(&data, None).is_none());
        assert!(candidate(&data, Some("movie")).unwrap().path == "movie/1");
        let episode = serde_json::json!({"tv_episode_results":[{"id":9,"show_id":2,"season_number":4,"episode_number":7}]});
        assert_eq!(
            candidate(&episode, Some("episode")).unwrap().path,
            "tv/2/season/4/episode/7"
        );
        assert!(
            parse_details(
                &serde_json::json!({"imdb_id":"tt9999999"}),
                "tt1375666",
                "url".into()
            )
            .is_err()
        );
        let mut details=parse_details(&serde_json::json!({"imdb_id":"tt1375666","overview":"story","runtime":148,"genres":[{"name":"Drama"}],"release_date":"2010-07-16"}),"tt1375666","url".into()).unwrap();
        details.preserve_imdb(true, true, true, true);
        assert!(
            details.overview.is_empty()
                && details.runtime.is_empty()
                && details.genres.is_empty()
                && details.release_date.is_empty()
        );
    }
}
