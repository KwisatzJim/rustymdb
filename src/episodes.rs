use rusqlite::{Connection, OptionalExtension};
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
const HEADER: &str = "tconst\tparentTconst\tseasonNumber\tepisodeNumber";

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS imdb_episodes (
        title_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL,
        season_number INTEGER, episode_number INTEGER
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS episodes_by_series ON imdb_episodes
        (parent_id, season_number, episode_number, title_id);",
    )
}

pub fn download_and_import(path: &Path) -> Result<usize, Error> {
    let source = "https://datasets.imdbws.com/title.episode.tsv.gz";
    println!("Downloading IMDb episode relationships from {source}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .build()?;
    let response = client.get(source).send()?.error_for_status()?;
    let modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Unknown")
        .to_owned();
    let reader = BufReader::new(flate2::read::MultiGzDecoder::new(
        response.take(512 * 1024 * 1024),
    ));
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    import(&mut connection, reader, &modified)
}

fn number(value: &str) -> Result<Option<i64>, Error> {
    if value == "\\N" {
        return Ok(None);
    }
    let value: u32 = value.parse()?;
    Ok(Some(i64::from(value)))
}

fn import(
    connection: &mut Connection,
    mut reader: impl BufRead,
    modified: &str,
) -> Result<usize, Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end_matches(['\r', '\n']) != HEADER {
        return Err("Unexpected episode header; previous episodes preserved".into());
    }
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM imdb_episodes", [])?;
    let mut count = 0;
    {
        let mut insert = transaction.prepare("INSERT INTO imdb_episodes VALUES (?1,?2,?3,?4)")?;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let fields: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
            let row = (|| -> Result<(), Error> {
                if fields.len() != 4
                    || !crate::imdb::valid_title_id(fields[0])
                    || !crate::imdb::valid_title_id(fields[1])
                    || fields[0] == fields[1]
                {
                    return Err("Invalid episode/series identifiers or columns".into());
                }
                insert.execute(rusqlite::params![
                    fields[0],
                    fields[1],
                    number(fields[2])?,
                    number(fields[3])?
                ])?;
                Ok(())
            })();
            row.map_err(|error| format!("Episode row {}: {error}", count + 2))?;
            count += 1;
            if count % 1_000_000 == 0 {
                println!("Read {count} episode relationships (not yet committed)…");
            }
        }
    }
    if count == 0 {
        return Err("Empty episode dataset; previous episodes preserved".into());
    }
    transaction.execute(
        "INSERT INTO dataset_imports (name,imported_at,source_modified,row_count)
        VALUES ('episodes',strftime('%Y-%m-%d %H:%M:%S UTC','now'),?1,?2)
        ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,
        source_modified=excluded.source_modified,row_count=excluded.row_count",
        rusqlite::params![modified, count as i64],
    )?;
    transaction.commit()?;
    Ok(count)
}

pub fn season_value(value: &str) -> Result<String, std::num::ParseIntError> {
    match value {
        "" | "all" => Ok("all".into()),
        "unknown" => Ok("unknown".into()),
        _ => value.parse::<u32>().map(|v| v.to_string()),
    }
}

fn position(season: Option<i64>, episode: Option<i64>) -> String {
    let season = match season {
        Some(0) => "Specials".into(),
        Some(n) => format!("Season {n}"),
        None => "Season not listed".into(),
    };
    let episode = episode
        .map(|n| format!("Episode {n}"))
        .unwrap_or_else(|| "Episode number not listed".into());
    format!("{season} · {episode}")
}

fn series_url(id: &str, season: &str, page: u32) -> String {
    // id, season, and page are validated identifiers/numbers, never raw query text.
    format!("/title/{id}?season={season}&page={page}#episodes")
}

#[derive(Debug)]
pub struct Parent {
    pub title: String,
    pub url: String,
    pub position: String,
}
#[derive(Debug)]
pub struct Season {
    pub value: String,
    pub label: String,
    pub selected: bool,
}
#[derive(Debug)]
pub struct Episode {
    pub id: String,
    pub title: String,
    pub position: String,
    pub rating: String,
}
#[derive(Debug)]
pub struct Series {
    pub seasons: Vec<Season>,
    pub episodes: Vec<Episode>,
    pub number: u32,
    pub previous_url: String,
    pub next_url: String,
    pub imported_at: String,
}
#[derive(Debug, Default)]
pub struct Navigation {
    pub parent: Option<Parent>,
    pub series: Option<Series>,
}

pub fn lookup(path: &Path, id: &str, season: &str, page: u32) -> rusqlite::Result<Navigation> {
    lookup_connection(&Connection::open(path)?, id, season, page)
}

fn lookup_connection(
    connection: &Connection,
    id: &str,
    season: &str,
    page: u32,
) -> rusqlite::Result<Navigation> {
    connection.execute_batch("BEGIN")?;
    let result = (|| {
        let imported = connection
            .query_row(
                "SELECT imported_at FROM dataset_imports WHERE name='episodes'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let Some(imported_at) = imported else {
            return Ok(Navigation::default());
        };
        let parent = connection.query_row("SELECT e.parent_id,COALESCE(t.primary_title,e.parent_id),e.season_number,e.episode_number
            FROM imdb_episodes e LEFT JOIN imdb_titles t ON t.title_id=e.parent_id WHERE e.title_id=?1",[id],|r| {
            let id:String=r.get(0)?;let title=r.get(1)?;let season:Option<i64>=r.get(2)?;let episode=r.get(3)?;
            Ok(Parent{title,url:series_url(&id,&season.map(|v|v.to_string()).unwrap_or_else(||"unknown".into()),1),position:position(season,episode)})
        }).optional()?;
        let mut statement=connection.prepare("SELECT DISTINCT season_number FROM imdb_episodes WHERE parent_id=?1 ORDER BY season_number IS NULL,season_number")?;
        let values = statement
            .query_map([id], |r| r.get::<_, Option<i64>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let is_series = !values.is_empty() || connection.query_row("SELECT EXISTS(SELECT 1 FROM imdb_titles WHERE title_id=?1 AND title_type IN ('tvSeries','tvMiniSeries'))",[id],|r|r.get::<_,bool>(0))?;
        if !is_series {
            return Ok(Navigation {
                parent,
                series: None,
            });
        }
        let mut seasons = vec![Season {
            value: "all".into(),
            label: "All seasons".into(),
            selected: season == "all",
        }];
        for value in values {
            let (value, label) = match value {
                Some(0) => ("0".into(), "Specials".into()),
                Some(n) => (n.to_string(), format!("Season {n}")),
                None => ("unknown".into(), "Season not listed".into()),
            };
            let selected = value == season;
            seasons.push(Season {
                value,
                label,
                selected,
            });
        }
        if !seasons.iter().any(|s| s.selected) {
            seasons.push(Season {
                value: season.into(),
                label: format!("Season {season} (not listed)"),
                selected: true,
            });
        }
        let mut statement=connection.prepare("SELECT e.title_id,COALESCE(t.primary_title,e.title_id),e.season_number,e.episode_number,r.rating
            FROM imdb_episodes e LEFT JOIN imdb_titles t ON t.title_id=e.title_id LEFT JOIN imdb_ratings r ON r.title_id=e.title_id
            WHERE e.parent_id=?1 AND (?2='all' OR (?2='unknown' AND e.season_number IS NULL) OR e.season_number=?3)
            ORDER BY e.season_number IS NULL,e.season_number,e.episode_number IS NULL,e.episode_number,e.title_id LIMIT 51 OFFSET ?4")?;
        let mut episodes = statement
            .query_map(
                rusqlite::params![
                    id,
                    season,
                    season.parse::<u32>().ok(),
                    i64::from(page.saturating_sub(1)) * 50
                ],
                |r| {
                    let score: Option<f64> = r.get(4)?;
                    Ok(Episode {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        position: position(r.get(2)?, r.get(3)?),
                        rating: score.map(|v| format!("{v} / 10")).unwrap_or_default(),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next_url = if episodes.len() > 50 {
            page.checked_add(1)
                .map(|p| series_url(id, season, p))
                .unwrap_or_default()
        } else {
            String::new()
        };
        episodes.truncate(50);
        let previous_url = if page > 1 {
            series_url(id, season, page - 1)
        } else {
            String::new()
        };
        Ok(Navigation {
            parent,
            series: Some(Series {
                seasons,
                episodes,
                number: page,
                previous_url,
                next_url,
                imported_at,
            }),
        })
    })();
    connection.execute_batch("ROLLBACK")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db.execute_batch("CREATE TABLE dataset_imports(name TEXT PRIMARY KEY,imported_at TEXT,source_modified TEXT,row_count INTEGER);
            CREATE TABLE imdb_titles(title_id TEXT PRIMARY KEY,primary_title TEXT,title_type TEXT);
            CREATE TABLE imdb_ratings(title_id TEXT PRIMARY KEY,rating REAL);
            INSERT INTO imdb_titles VALUES ('tt0000001','Series','tvSeries'),('tt0000002','Pilot','tvEpisode'),('tt9999999','Movie','movie');
            INSERT INTO imdb_ratings VALUES ('tt0000002',8.5);").unwrap();
        db
    }
    fn data() -> String {
        format!(
            "{HEADER}\ntt0000002\ttt0000001\t1\t1\ntt0000003\ttt0000001\t0\t1\ntt0000004\ttt0000001\t\\N\t\\N\n"
        )
    }
    #[test]
    fn series_seasons_parent_links_unknown_numbers_and_missing_titles() {
        let mut db = database();
        assert!(
            lookup_connection(&db, "tt0000001", "all", 1)
                .unwrap()
                .series
                .is_none()
        );
        import(&mut db, Cursor::new(data()), "first").unwrap();
        let nav = lookup_connection(&db, "tt0000001", "all", 1).unwrap();
        let series = nav.series.unwrap();
        assert_eq!(series.episodes.len(), 3);
        assert!(series.episodes[0].position.starts_with("Specials"));
        assert_eq!(series.episodes[1].title, "Pilot");
        assert_eq!(series.episodes[1].rating, "8.5 / 10");
        assert_eq!(series.episodes[2].title, "tt0000004");
        assert!(series.episodes[2].position.contains("not listed"));
        assert_eq!(
            lookup_connection(&db, "tt0000001", "unknown", 1)
                .unwrap()
                .series
                .unwrap()
                .episodes
                .len(),
            1
        );
        let parent = lookup_connection(&db, "tt0000002", "all", 1)
            .unwrap()
            .parent
            .unwrap();
        assert_eq!(parent.title, "Series");
        assert_eq!(parent.url, "/title/tt0000001?season=1&page=1#episodes");
        assert!(
            lookup_connection(&db, "tt9999999", "all", 1)
                .unwrap()
                .series
                .is_none()
        );
        assert!(
            lookup_connection(&db, "tt0000001", "99", 1)
                .unwrap()
                .series
                .unwrap()
                .episodes
                .is_empty()
        );
        assert_eq!(season_value("0002").unwrap(), "2");
        assert!(season_value("-1").is_err());
    }
    #[test]
    fn invalid_and_truncated_refreshes_preserve_episode_relationships() {
        let mut db = database();
        import(&mut db, Cursor::new(data()), "original").unwrap();
        for invalid in [
            data().replace("\t1\t1", "\t-1\t1"),
            data().replace("tt0000002", "invalid"),
            format!("{HEADER}\n"),
            "wrong\n".into(),
        ] {
            assert!(import(&mut db, Cursor::new(invalid), "bad").is_err());
        }
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(data().as_bytes()).unwrap();
        let mut bytes = gzip.finish().unwrap();
        bytes.truncate(bytes.len() - 5);
        assert!(
            import(
                &mut db,
                BufReader::new(flate2::read::MultiGzDecoder::new(Cursor::new(bytes))),
                "broken"
            )
            .is_err()
        );
        assert_eq!(
            lookup_connection(&db, "tt0000001", "all", 1)
                .unwrap()
                .series
                .unwrap()
                .episodes
                .len(),
            3
        );
        assert_eq!(
            db.query_row(
                "SELECT source_modified FROM dataset_imports WHERE name='episodes'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "original"
        );
        let mut refreshed = format!("{HEADER}\n");
        for i in 2..=53 {
            refreshed.push_str(&format!("tt{i:07}\ttt0000001\t1\t{}\n", i - 1));
        }
        import(&mut db, Cursor::new(refreshed), "second").unwrap();
        let first = lookup_connection(&db, "tt0000001", "1", 1)
            .unwrap()
            .series
            .unwrap();
        let second = lookup_connection(&db, "tt0000001", "1", 2)
            .unwrap()
            .series
            .unwrap();
        assert_eq!(first.episodes.len(), 50);
        assert_eq!(second.episodes.len(), 2);
        assert!(!first.next_url.is_empty());
        assert!(!second.previous_url.is_empty());
        assert!(second.next_url.is_empty());
        assert!(
            !first
                .episodes
                .iter()
                .any(|a| second.episodes.iter().any(|b| a.id == b.id))
        );
    }
}
