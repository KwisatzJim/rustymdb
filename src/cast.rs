use rusqlite::{Connection, OptionalExtension};
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
const HEADER: &str = "tconst\tordering\tnconst\tcategory\tjob\tcharacters";

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS imdb_cast (
        title_id TEXT NOT NULL, ordering INTEGER NOT NULL CHECK(ordering > 0),
        person_id TEXT NOT NULL, category TEXT NOT NULL,
        characters TEXT NOT NULL, PRIMARY KEY (title_id, ordering)
        ) WITHOUT ROWID;",
    )
}

pub fn download_and_import(path: &Path) -> Result<usize, Error> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    let people: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM dataset_imports WHERE name='people')",
        [],
        |r| r.get(0),
    )?;
    if !people {
        return Err("Import credits first so IMDb person names are available".into());
    }
    let source = "https://datasets.imdbws.com/title.principals.tsv.gz";
    println!("Downloading principal credits from {source}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(3600))
        .build()?;
    let response = client.get(source).send()?.error_for_status()?;
    let modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Unknown")
        .to_owned();
    let reader = BufReader::new(flate2::read::MultiGzDecoder::new(
        response.take(2 * 1024 * 1024 * 1024),
    ));
    import(&mut connection, reader, &modified)
}

fn import(
    connection: &mut Connection,
    mut reader: impl BufRead,
    modified: &str,
) -> Result<usize, Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end_matches(['\r', '\n']) != HEADER {
        return Err("Unexpected principal credits header; previous cast preserved".into());
    }
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM imdb_cast", [])?;
    let mut scanned = 0;
    let mut retained = 0;
    {
        let mut insert =
            transaction.prepare("INSERT INTO imdb_cast VALUES (?1, ?2, ?3, ?4, ?5)")?;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let fields: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
            let parsed = (|| -> Result<(), Error> {
                if fields.len() != 6
                    || !crate::imdb::valid_title_id(fields[0])
                    || !crate::credits::person_id(fields[2])
                {
                    return Err("Invalid principal-credit columns or IMDb IDs".into());
                }
                let order: i64 = fields[1].parse()?;
                if order <= 0 {
                    return Err("Invalid credit ordering".into());
                }
                if matches!(fields[3], "actor" | "actress" | "self") {
                    let characters = if fields[5] == "\\N" { "[]" } else { fields[5] };
                    let _: Vec<String> = serde_json::from_str(characters)?;
                    insert.execute(rusqlite::params![
                        fields[0], order, fields[2], fields[3], characters
                    ])?;
                    retained += 1;
                }
                Ok(())
            })();
            parsed.map_err(|error| format!("Principal credits row {}: {error}", scanned + 2))?;
            scanned += 1;
            if scanned % 5_000_000 == 0 {
                println!(
                    "Read {scanned} principal credits; retained {retained} cast entries (not yet committed)…"
                );
            }
        }
    }
    if retained == 0 {
        return Err("No principal cast entries found; previous cast preserved".into());
    }
    transaction.execute(
        "INSERT INTO dataset_imports (name, imported_at, source_modified, row_count)
        VALUES ('cast', strftime('%Y-%m-%d %H:%M:%S UTC', 'now'), ?1, ?2)
        ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,
        source_modified=excluded.source_modified, row_count=excluded.row_count",
        rusqlite::params![modified, retained as i64],
    )?;
    transaction.commit()?;
    println!("Read {scanned} principal credits in total.");
    Ok(retained)
}

#[derive(Debug)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub characters: String,
    pub is_self: bool,
}

#[derive(Debug)]
pub struct Cast {
    pub members: Vec<Member>,
    pub imported_at: String,
}

pub fn lookup(path: &Path, title_id: &str) -> rusqlite::Result<Option<Cast>> {
    lookup_connection(&Connection::open(path)?, title_id)
}

fn lookup_connection(connection: &Connection, title_id: &str) -> rusqlite::Result<Option<Cast>> {
    connection.execute_batch("BEGIN")?;
    let result = (|| {
        let imported_at = connection
            .query_row(
                "SELECT imported_at FROM dataset_imports WHERE name='cast'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let Some(imported_at) = imported_at else {
            return Ok(None);
        };
        let mut statement = connection.prepare("SELECT c.person_id,p.primary_name,c.characters,c.category FROM imdb_cast c
            LEFT JOIN imdb_people p ON p.person_id=c.person_id WHERE c.title_id=?1 ORDER BY c.ordering")?;
        let members = statement
            .query_map([title_id], |row| {
                let id: String = row.get(0)?;
                let name: Option<String> = row.get(1)?;
                let json: String = row.get(2)?;
                let characters: Vec<String> = serde_json::from_str(&json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(Member {
                    name: name.unwrap_or_else(|| format!("Name unavailable ({id})")),
                    id,
                    characters: characters.join(", "),
                    is_self: row.get::<_, String>(3)? == "self",
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(Cast {
            members,
            imported_at,
        }))
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
        db.execute_batch("CREATE TABLE dataset_imports (name TEXT PRIMARY KEY, imported_at TEXT, source_modified TEXT, row_count INTEGER);
        CREATE TABLE imdb_people (person_id TEXT PRIMARY KEY, primary_name TEXT);
        INSERT INTO imdb_people VALUES ('nm0000001','René Actor'),('nm0000002','Another Actor');").unwrap();
        db
    }
    fn data() -> String {
        format!(
            "{HEADER}\ntt1375666\t3\tnm0000001\tactor\t\\N\t[\"A & B\",\"Second role\"]\ntt1375666\t1\tnm0000002\tactress\t\\N\t\\N\ntt1375666\t2\tnm9999999\tself\t\\N\t[]\ntt1375666\t4\tnm0000001\tdirector\t\\N\t\\N\n"
        )
    }
    #[test]
    fn imports_principal_cast_in_source_order_with_roles_and_explicit_missing_names() {
        let mut db = database();
        assert!(lookup_connection(&db, "tt1375666").unwrap().is_none());
        assert_eq!(import(&mut db, Cursor::new(data()), "first").unwrap(), 3);
        let cast = lookup_connection(&db, "tt1375666").unwrap().unwrap();
        assert_eq!(cast.members[0].name, "Another Actor");
        assert_eq!(cast.members[0].id, "nm0000002");
        assert!(cast.members[0].characters.is_empty());
        assert_eq!(cast.members[1].name, "Name unavailable (nm9999999)");
        assert!(cast.members[1].is_self);
        assert_eq!(cast.members[2].name, "René Actor");
        assert_eq!(cast.members[2].characters, "A & B, Second role");
        assert!(
            lookup_connection(&db, "tt9999999")
                .unwrap()
                .unwrap()
                .members
                .is_empty()
        );
        import(
            &mut db,
            Cursor::new(data().replace("Second role", "Updated role")),
            "second",
        )
        .unwrap();
        assert!(
            lookup_connection(&db, "tt1375666")
                .unwrap()
                .unwrap()
                .members[2]
                .characters
                .contains("Updated role")
        );
    }
    #[test]
    fn bad_rows_and_truncated_downloads_preserve_existing_cast() {
        let mut db = database();
        import(&mut db, Cursor::new(data()), "original").unwrap();
        for bad in [
            data().replace("[\"A & B\",\"Second role\"]", "[123]"),
            data().replace("nm0000001", "invalid"),
            format!("{HEADER}\n"),
            "wrong header\n".into(),
        ] {
            assert!(import(&mut db, Cursor::new(bad), "bad").is_err());
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
            lookup_connection(&db, "tt1375666")
                .unwrap()
                .unwrap()
                .members
                .len(),
            3
        );
        assert_eq!(
            db.query_row(
                "SELECT source_modified FROM dataset_imports WHERE name='cast'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "original"
        );
    }
}
