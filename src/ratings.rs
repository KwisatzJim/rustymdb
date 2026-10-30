use rusqlite::Connection;
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
const SOURCE: &str = "https://datasets.imdbws.com/title.ratings.tsv.gz";

pub fn download_and_import(path: &Path) -> Result<usize, Error> {
    println!("Downloading IMDb ratings from {SOURCE}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()?;
    let response = client.get(SOURCE).send()?.error_for_status()?;
    let modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("Unknown")
        .to_owned();
    let decoder = flate2::read::MultiGzDecoder::new(response.take(32 * 1024 * 1024));
    let reader = BufReader::new(decoder);
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    import(&mut connection, reader, &modified)
}

fn import(
    connection: &mut Connection,
    mut reader: impl BufRead,
    modified: &str,
) -> Result<usize, Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end() != "tconst\taverageRating\tnumVotes" {
        return Err("Unexpected IMDb ratings header; existing ratings preserved".into());
    }
    let transaction = connection.transaction()?;
    // Readers keep seeing the previous dataset until the complete import commits.
    transaction.execute("DELETE FROM imdb_ratings", [])?;
    let mut count = 0;
    {
        let mut insert = transaction
            .prepare("INSERT INTO imdb_ratings (title_id, rating, votes) VALUES (?1, ?2, ?3)")?;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let fields: Vec<_> = line.trim_end().split('\t').collect();
            if fields.len() != 3 || !crate::imdb::valid_title_id(fields[0]) {
                return Err(format!("Invalid ratings row {}", count + 2).into());
            }
            let score: f64 = fields[1].parse()?;
            let votes: i64 = fields[2].parse()?;
            if !score.is_finite() || !(1.0..=10.0).contains(&score) || votes <= 0 {
                return Err(format!("Invalid rating or vote count on row {}", count + 2).into());
            }
            insert.execute(rusqlite::params![fields[0], score, votes])?;
            count += 1;
        }
    }
    if count == 0 {
        return Err("Empty ratings dataset; existing ratings preserved".into());
    }
    transaction.execute(
        "INSERT INTO dataset_imports (name, imported_at, source_modified, row_count)
         VALUES ('ratings', strftime('%Y-%m-%d %H:%M:%S UTC', 'now'), ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,
         source_modified=excluded.source_modified, row_count=excluded.row_count",
        rusqlite::params![modified, count as i64],
    )?;
    transaction.commit()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE imdb_ratings (title_id TEXT PRIMARY KEY, rating REAL, votes INTEGER);
        CREATE TABLE dataset_imports (name TEXT PRIMARY KEY, imported_at TEXT, source_modified TEXT, row_count INTEGER);").unwrap();
        connection
    }

    #[test]
    fn refresh_replaces_ratings_and_failed_refresh_preserves_previous_data() {
        let mut db = database();
        let first = "tconst\taverageRating\tnumVotes\ntt1375666\t8.8\t100\n";
        assert_eq!(import(&mut db, Cursor::new(first), "first").unwrap(), 1);
        let bad = "tconst\taverageRating\tnumVotes\ntt1375666\t9.0\t200\ntt1234567\tNaN\t3\n";
        assert!(import(&mut db, Cursor::new(bad), "bad").is_err());
        assert_eq!(
            db.query_row("SELECT rating FROM imdb_ratings", [], |r| r
                .get::<_, f64>(0))
                .unwrap(),
            8.8
        );
        assert_eq!(
            db.query_row("SELECT source_modified FROM dataset_imports", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "first"
        );
        assert!(
            import(
                &mut db,
                Cursor::new("tconst\taverageRating\tnumVotes\n"),
                "empty"
            )
            .is_err()
        );
        assert!(import(&mut db, Cursor::new("wrong header\n"), "wrong").is_err());
        assert_eq!(
            import(&mut db, Cursor::new(first.replace("8.8", "8.9")), "second").unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM imdb_ratings", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT rating FROM imdb_ratings", [], |r| r
                .get::<_, f64>(0))
                .unwrap(),
            8.9
        );
    }

    #[test]
    fn truncated_gzip_does_not_replace_previous_ratings() {
        use std::io::Write;
        let mut db = database();
        let text = "tconst\taverageRating\tnumVotes\ntt1375666\t8.8\t100\n";
        import(&mut db, Cursor::new(text), "original").unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        let mut bytes = encoder.finish().unwrap();
        bytes.truncate(bytes.len() - 5);
        let reader = BufReader::new(flate2::read::MultiGzDecoder::new(Cursor::new(bytes)));
        assert!(import(&mut db, reader, "truncated").is_err());
        assert_eq!(
            db.query_row("SELECT source_modified FROM dataset_imports", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "original"
        );
    }
}
