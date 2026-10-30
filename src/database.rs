use rusqlite::Connection;
use std::path::Path;

pub fn cached(path: &Path, key: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Connection::open(path)?
        .query_row(
            "SELECT payload FROM imdb_cache WHERE cache_key = ?1 AND expires_at > unixepoch()",
            [key],
            |row| row.get(0),
        )
        .optional()
}

pub fn store(path: &Path, key: &str, payload: &str) -> rusqlite::Result<()> {
    Connection::open(path)?.execute(
        "INSERT INTO imdb_cache (cache_key, payload, fetched_at, expires_at)
         VALUES (?1, ?2, unixepoch(), unixepoch() + 86400)
         ON CONFLICT(cache_key) DO UPDATE SET payload=excluded.payload,
         fetched_at=excluded.fetched_at, expires_at=excluded.expires_at",
        [key, payload],
    )?;
    Ok(())
}

pub fn initialize(path: &Path) -> rusqlite::Result<()> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS imdb_cache (
             cache_key TEXT PRIMARY KEY,
             payload TEXT NOT NULL,
             fetched_at INTEGER NOT NULL,
             expires_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS imdb_ratings (
             title_id TEXT PRIMARY KEY,
             rating REAL NOT NULL CHECK(rating BETWEEN 1 AND 10),
             votes INTEGER NOT NULL CHECK(votes > 0)
         );
         CREATE TABLE IF NOT EXISTS dataset_imports (
             name TEXT PRIMARY KEY,
             imported_at TEXT NOT NULL,
             source_modified TEXT NOT NULL,
             row_count INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS imdb_titles (
             title_id TEXT PRIMARY KEY,
             title_type TEXT NOT NULL,
             primary_title TEXT NOT NULL,
             original_title TEXT NOT NULL,
             is_adult INTEGER NOT NULL CHECK(is_adult IN (0, 1)),
             start_year INTEGER,
             end_year INTEGER,
             runtime_minutes INTEGER,
             genres TEXT
         ) WITHOUT ROWID;",
    )?;
    crate::catalog::initialize(&connection)?;
    crate::credits::initialize(&connection)?;
    crate::cast::initialize(&connection)?;
    crate::episodes::initialize(&connection)?;
    crate::watchlist::initialize(&connection)?;
    crate::posters::initialize(&connection)?;
    crate::maintenance::initialize(&connection)?;
    crate::retention::initialize(&connection)?;
    crate::tmdb::initialize(&connection)
}

#[derive(Debug)]
pub struct TitleBasics {
    pub primary_title: String,
    pub original_title: String,
    pub title_type: String,
    pub start_year: Option<i64>,
    pub end_year: Option<i64>,
    pub runtime_minutes: Option<i64>,
    pub genres: Option<String>,
    pub imported_at: String,
}

impl TitleBasics {
    pub fn years(&self) -> String {
        match (self.start_year, self.end_year) {
            (Some(start), Some(end)) if start != end => format!("{start}–{end}"),
            (Some(start), _) => start.to_string(),
            _ => String::new(),
        }
    }

    pub fn type_label(&self) -> &str {
        title_type_label(&self.title_type)
    }

    pub fn genres_label(&self) -> String {
        self.genres
            .as_deref()
            .unwrap_or_default()
            .replace(',', ", ")
    }
}

pub fn title_type_label(value: &str) -> &str {
    match value {
        "movie" => "Movie",
        "tvSeries" => "TV series",
        "tvMiniSeries" => "TV mini-series",
        "tvEpisode" => "TV episode",
        "tvMovie" => "TV movie",
        "short" => "Short",
        "tvShort" => "TV short",
        "tvSpecial" => "TV special",
        "video" => "Video",
        "videoGame" => "Video game",
        _ => value,
    }
}

pub fn title_basics(path: &Path, id: &str) -> rusqlite::Result<Option<TitleBasics>> {
    use rusqlite::OptionalExtension;
    Connection::open(path)?
        .query_row(
            "SELECT t.primary_title, t.original_title, t.title_type, t.start_year, t.end_year,
         t.runtime_minutes, t.genres, d.imported_at FROM imdb_titles t
         JOIN dataset_imports d ON d.name = 'titles' WHERE t.title_id = ?1",
            [id],
            |row| {
                Ok(TitleBasics {
                    primary_title: row.get(0)?,
                    original_title: row.get(1)?,
                    title_type: row.get(2)?,
                    start_year: row.get(3)?,
                    end_year: row.get(4)?,
                    runtime_minutes: row.get(5)?,
                    genres: row.get(6)?,
                    imported_at: row.get(7)?,
                })
            },
        )
        .optional()
}

#[derive(Debug)]
pub struct Rating {
    pub score: f64,
    pub votes: i64,
    pub imported_at: String,
}

pub fn rating(path: &Path, id: &str) -> rusqlite::Result<Option<Rating>> {
    use rusqlite::OptionalExtension;
    Connection::open(path)?
        .query_row(
            "SELECT r.rating, r.votes, d.imported_at FROM imdb_ratings r
         JOIN dataset_imports d ON d.name = 'ratings' WHERE r.title_id = ?1",
            [id],
            |row| {
                Ok(Rating {
                    score: row.get(0)?,
                    votes: row.get(1)?,
                    imported_at: row.get(2)?,
                })
            },
        )
        .optional()
}
