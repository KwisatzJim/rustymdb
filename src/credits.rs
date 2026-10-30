use rusqlite::{Connection, OptionalExtension};
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
struct Dataset<R> {
    reader: R,
    modified: String,
}
type Download =
    Dataset<BufReader<flate2::read::MultiGzDecoder<std::io::Take<reqwest::blocking::Response>>>>;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS imdb_crew (
        title_id TEXT PRIMARY KEY, directors TEXT NOT NULL, writers TEXT NOT NULL
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS imdb_people (
        person_id TEXT PRIMARY KEY, primary_name TEXT
        ) WITHOUT ROWID;",
    )?;
    crate::people::initialize(connection)
}

fn download(client: &reqwest::blocking::Client, dataset: &str) -> Result<Download, Error> {
    let url = format!("https://datasets.imdbws.com/{dataset}.tsv.gz");
    println!("Downloading {url}");
    let response = client.get(url).send()?.error_for_status()?;
    let modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Unknown")
        .to_owned();
    Ok(Dataset {
        reader: BufReader::new(flate2::read::MultiGzDecoder::new(
            response.take(1024 * 1024 * 1024),
        )),
        modified,
    })
}

pub fn download_and_import(path: &Path) -> Result<(usize, usize), Error> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .build()?;
    let crew = download(&client, "title.crew")?;
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    refresh(&mut connection, crew, || download(&client, "name.basics"))
}

pub(crate) fn person_id(id: &str) -> bool {
    id.strip_prefix("nm")
        .is_some_and(|n| (7..=12).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
}

fn ids(value: &str) -> Result<&str, Error> {
    if value == "\\N" {
        return Ok("");
    }
    if !value.split(',').all(person_id) {
        return Err("Invalid IMDb person ID list".into());
    }
    Ok(value)
}

fn read_header(reader: &mut impl BufRead, expected: &str) -> Result<(), Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end_matches(['\r', '\n']) != expected {
        return Err("Unexpected IMDb dataset header; previous credits preserved".into());
    }
    Ok(())
}

fn metadata(
    connection: &Connection,
    name: &str,
    modified: &str,
    rows: usize,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO dataset_imports (name, imported_at, source_modified, row_count)
        VALUES (?1, strftime('%Y-%m-%d %H:%M:%S UTC', 'now'), ?2, ?3)
        ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,
        source_modified=excluded.source_modified, row_count=excluded.row_count",
        rusqlite::params![name, modified, rows as i64],
    )?;
    Ok(())
}

fn refresh<C: BufRead, P: BufRead>(
    connection: &mut Connection,
    mut crew: Dataset<C>,
    people: impl FnOnce() -> Result<Dataset<P>, Error>,
) -> Result<(usize, usize), Error> {
    read_header(&mut crew.reader, "tconst\tdirectors\twriters")?;
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM imdb_crew", [])?;
    let mut crew_count = 0;
    let mut line = String::new();
    {
        let mut insert = transaction.prepare("INSERT INTO imdb_crew VALUES (?1, ?2, ?3)")?;
        loop {
            line.clear();
            if crew.reader.read_line(&mut line)? == 0 {
                break;
            }
            let fields: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
            if fields.len() != 3 || !crate::imdb::valid_title_id(fields[0]) {
                return Err(format!("Invalid crew row {}", crew_count + 2).into());
            }
            insert.execute(rusqlite::params![
                fields[0],
                ids(fields[1])?,
                ids(fields[2])?
            ])?;
            crew_count += 1;
            if crew_count % 1_000_000 == 0 {
                println!("Read {crew_count} crew rows (not yet committed)…");
            }
        }
    }
    if crew_count == 0 {
        return Err("Empty crew dataset; previous credits preserved".into());
    }
    metadata(&transaction, "crew", &crew.modified, crew_count)?;

    let mut people = people()?;
    read_header(
        &mut people.reader,
        "nconst\tprimaryName\tbirthYear\tdeathYear\tprimaryProfession\tknownForTitles",
    )?;
    transaction.execute("DELETE FROM imdb_people", [])?;
    let mut people_count = 0;
    {
        let mut insert = transaction.prepare("INSERT INTO imdb_people VALUES (?1, ?2)")?;
        loop {
            line.clear();
            if people.reader.read_line(&mut line)? == 0 {
                break;
            }
            let fields: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
            if fields.len() != 6 || !person_id(fields[0]) || fields[1].is_empty() {
                return Err(format!("Invalid people row {}", people_count + 2).into());
            }
            let name = (fields[1] != "\\N").then_some(fields[1]);
            insert.execute(rusqlite::params![fields[0], name])?;
            people_count += 1;
            if people_count % 1_000_000 == 0 {
                println!("Read {people_count} people rows (not yet committed)…");
            }
        }
    }
    if people_count == 0 {
        return Err("Empty people dataset; previous credits preserved".into());
    }
    metadata(&transaction, "people", &people.modified, people_count)?;
    crate::people::rebuild(&transaction)?;
    transaction.commit()?;
    Ok((crew_count, people_count))
}

#[derive(Debug)]
pub struct PersonLink {
    pub id: String,
    pub name: String,
}
#[derive(Debug)]
pub struct Credits {
    pub directors: String,
    pub writers: String,
    pub imported_at: String,
    pub director_links: Vec<PersonLink>,
    pub writer_links: Vec<PersonLink>,
}

pub fn lookup(path: &Path, title_id: &str) -> rusqlite::Result<Option<Credits>> {
    let connection = Connection::open(path)?;
    lookup_connection(&connection, title_id)
}

fn lookup_connection(connection: &Connection, title_id: &str) -> rusqlite::Result<Option<Credits>> {
    connection.execute_batch("BEGIN")?;
    let result = (|| {
        let row = connection
            .query_row(
                "SELECT c.directors,c.writers,d.imported_at FROM imdb_crew c
            JOIN dataset_imports d ON d.name='crew'
            WHERE c.title_id=?1 AND EXISTS (SELECT 1 FROM dataset_imports WHERE name='people')",
                [title_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((directors, writers, imported_at)) = row else {
            return Ok(None);
        };
        let mut statement =
            connection.prepare("SELECT primary_name FROM imdb_people WHERE person_id=?1")?;
        let mut names = |list: &str| -> rusqlite::Result<Vec<PersonLink>> {
            let mut result = vec![];
            for id in list.split(',').filter(|id| !id.is_empty()) {
                let name = statement
                    .query_row([id], |row| row.get::<_, Option<String>>(0))
                    .optional()?
                    .flatten();
                result.push(PersonLink {
                    id: id.into(),
                    name: name.unwrap_or_else(|| format!("Name unavailable ({id})")),
                });
            }
            Ok(result)
        };
        let director_links = names(&directors)?;
        let writer_links = names(&writers)?;
        Ok(Some(Credits {
            directors: director_links
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            writers: writer_links
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            director_links,
            writer_links,
            imported_at,
        }))
    })();
    connection.execute_batch("ROLLBACK")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    const CREW: &str = "tconst\tdirectors\twriters\ntt1375666\tnm0000002,nm0000001\tnm0000001,nm9999999\ntt1234567\t\\N\t\\N\n";
    const PEOPLE: &str = "nconst\tprimaryName\tbirthYear\tdeathYear\tprimaryProfession\tknownForTitles\nnm0000001\tRené Writer\t\\N\t\\N\twriter\t\\N\nnm0000002\tA Director\t\\N\t\\N\tdirector\t\\N\n";
    fn dataset(text: &str) -> Dataset<Cursor<Vec<u8>>> {
        Dataset {
            reader: Cursor::new(text.as_bytes().to_vec()),
            modified: "test".into(),
        }
    }
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db.execute_batch("CREATE TABLE dataset_imports (name TEXT PRIMARY KEY, imported_at TEXT, source_modified TEXT, row_count INTEGER)").unwrap();
        db
    }

    #[test]
    fn credits_preserve_source_order_unicode_and_missing_names() {
        let mut db = database();
        assert_eq!(
            refresh(&mut db, dataset(CREW), || Ok(dataset(PEOPLE))).unwrap(),
            (2, 2)
        );
        let credits = lookup_connection(&db, "tt1375666").unwrap().unwrap();
        assert_eq!(credits.directors, "A Director, René Writer");
        assert_eq!(credits.director_links[0].id, "nm0000002");
        assert_eq!(credits.writer_links[1].id, "nm9999999");
        assert_eq!(credits.writers, "René Writer, Name unavailable (nm9999999)");
        assert!(
            lookup_connection(&db, "tt1234567")
                .unwrap()
                .unwrap()
                .directors
                .is_empty()
        );
        assert!(lookup_connection(&db, "tt9999999").unwrap().is_none());
        refresh(&mut db, dataset(CREW), || {
            Ok(dataset(&PEOPLE.replace("René Writer", "Updated Writer")))
        })
        .unwrap();
        assert!(
            lookup_connection(&db, "tt1375666")
                .unwrap()
                .unwrap()
                .writers
                .starts_with("Updated Writer")
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM imdb_people", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn failed_imports_restore_both_datasets_and_their_metadata() {
        let mut db = database();
        refresh(&mut db, dataset(CREW), || Ok(dataset(PEOPLE))).unwrap();
        let changed = CREW.replace("nm0000002,nm0000001", "nm0000001");
        assert!(refresh(&mut db, dataset(&changed), || Ok(dataset("bad header\n"))).is_err());
        assert!(
            refresh(&mut db, dataset(&changed), || Err::<
                Dataset<Cursor<Vec<u8>>>,
                Error,
            >(
                "download failed".into()
            ))
            .is_err()
        );
        assert!(
            refresh(
                &mut db,
                dataset(&CREW.replace("nm0000002", "bad-id")),
                || Ok(dataset(PEOPLE))
            )
            .is_err()
        );
        assert!(
            refresh(&mut db, dataset("tconst\tdirectors\twriters\n"), || Ok(
                dataset(PEOPLE)
            ))
            .is_err()
        );
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(PEOPLE.as_bytes()).unwrap();
        let mut bytes = encoder.finish().unwrap();
        bytes.truncate(bytes.len() - 5);
        assert!(
            refresh(&mut db, dataset(&changed), || Ok(Dataset {
                reader: BufReader::new(flate2::read::MultiGzDecoder::new(Cursor::new(bytes))),
                modified: "broken".into()
            }))
            .is_err()
        );
        assert_eq!(
            lookup_connection(&db, "tt1375666")
                .unwrap()
                .unwrap()
                .directors,
            "A Director, René Writer"
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM dataset_imports WHERE source_modified='test'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            3
        );
        assert_eq!(
            crate::people::search(&db, "René", 1).unwrap().unwrap()[0].title,
            "René Writer"
        );
    }
}
