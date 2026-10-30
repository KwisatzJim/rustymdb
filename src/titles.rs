use rusqlite::Connection;
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
const SOURCE: &str = "https://datasets.imdbws.com/title.basics.tsv.gz";
const HEADER: &str = "tconst\ttitleType\tprimaryTitle\toriginalTitle\tisAdult\tstartYear\tendYear\truntimeMinutes\tgenres";

pub fn download_and_import(path: &Path) -> Result<usize, Error> {
    println!("Downloading IMDb title basics from {SOURCE}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .build()?;
    let response = client.get(SOURCE).send()?.error_for_status()?;
    let modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Unknown")
        .to_owned();
    let reader = BufReader::new(flate2::read::MultiGzDecoder::new(
        response.take(1024 * 1024 * 1024),
    ));
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    import(&mut connection, reader, &modified)
}

fn optional_number(value: &str) -> Result<Option<i64>, Error> {
    if value == "\\N" {
        return Ok(None);
    }
    let number: i64 = value.parse()?;
    if number < 0 {
        return Err("Negative year or runtime in IMDb dataset".into());
    }
    Ok(Some(number))
}

fn import(
    connection: &mut Connection,
    mut reader: impl BufRead,
    modified: &str,
) -> Result<usize, Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end_matches(['\r', '\n']) != HEADER {
        return Err("Unexpected title basics header; existing data preserved".into());
    }
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM imdb_titles", [])?;
    let mut count = 0;
    {
        let mut insert = transaction.prepare("INSERT INTO imdb_titles
            (title_id, title_type, primary_title, original_title, is_adult, start_year, end_year, runtime_minutes, genres)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)")?;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let fields: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
            let result = (|| -> Result<(), Error> {
                if fields.len() != 9 || !crate::imdb::valid_title_id(fields[0]) {
                    return Err("Unexpected title columns or invalid ID".into());
                }
                let adult: i64 = match fields[4] {
                    "0" => 0,
                    "1" => 1,
                    _ => return Err("Invalid isAdult value".into()),
                };
                let genres = (fields[8] != "\\N").then_some(fields[8]);
                insert.execute(rusqlite::params![
                    fields[0],
                    fields[1],
                    fields[2],
                    fields[3],
                    adult,
                    optional_number(fields[5])?,
                    optional_number(fields[6])?,
                    optional_number(fields[7])?,
                    genres
                ])?;
                Ok(())
            })();
            result.map_err(|error| format!("Title basics row {}: {error}", count + 2))?;
            count += 1;
            if count % 1_000_000 == 0 {
                println!("Imported {count} titles so far (not yet committed)…");
            }
        }
    }
    if count == 0 {
        return Err("Empty title basics dataset; existing data preserved".into());
    }
    transaction.execute(
        "INSERT INTO dataset_imports (name, imported_at, source_modified, row_count)
         VALUES ('titles', strftime('%Y-%m-%d %H:%M:%S UTC', 'now'), ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,
         source_modified=excluded.source_modified, row_count=excluded.row_count",
        rusqlite::params![modified, count as i64],
    )?;
    println!("Building local title search index…");
    crate::catalog::rebuild(&transaction)?;
    transaction.commit()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE imdb_titles (title_id TEXT PRIMARY KEY, title_type TEXT,
            primary_title TEXT, original_title TEXT, is_adult INTEGER, start_year INTEGER,
            end_year INTEGER, runtime_minutes INTEGER, genres TEXT) WITHOUT ROWID;
            CREATE TABLE dataset_imports (name TEXT PRIMARY KEY, imported_at TEXT, source_modified TEXT, row_count INTEGER);").unwrap();
        crate::catalog::initialize(&db).unwrap();
        db
    }

    #[test]
    fn imports_nulls_unicode_series_years_and_replaces_atomically() {
        let mut db = database();
        let data = format!(
            "{HEADER}\ntt1375666\tmovie\tInception\tInception\t0\t2010\t\\N\t148\tAction,Adventure,Sci-Fi\ntt1234567\ttvSeries\tCafé\tCafé\t0\t2000\t2005\t\\N\t\\N\n"
        );
        assert_eq!(import(&mut db, Cursor::new(&data), "first").unwrap(), 2);
        let series: (String, Option<i64>, Option<String>, i64) = db.query_row("SELECT primary_title, runtime_minutes, genres, end_year FROM imdb_titles WHERE title_id='tt1234567'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(series, ("Café".into(), None, None, 2005));
        let bad = format!("{data}tt9999999\tmovie\tBad\tBad\t0\t2010\t\\N\tbroken\tDrama\n");
        assert!(import(&mut db, Cursor::new(bad), "bad").is_err());
        assert!(import(&mut db, Cursor::new(format!("{HEADER}\n")), "empty").is_err());
        assert!(import(&mut db, Cursor::new("wrong header\n"), "wrong").is_err());
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM title_search WHERE title_search MATCH 'Inception'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM imdb_titles", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT source_modified FROM dataset_imports WHERE name='titles'",
                [],
                |r| { r.get::<_, String>(0) }
            )
            .unwrap(),
            "first"
        );
        assert_eq!(
            import(
                &mut db,
                Cursor::new(data.replace("148", "149").replace("Inception", "Changed")),
                "second"
            )
            .unwrap(),
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT runtime_minutes FROM imdb_titles WHERE title_id='tt1375666'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            149
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM title_search WHERE title_search MATCH 'Inception'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM title_search WHERE title_search MATCH 'Changed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}
