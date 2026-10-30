use rusqlite::{Connection, OptionalExtension};
use std::{path::Path, time::Duration};

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS watchlist (
        title_id TEXT PRIMARY KEY, title_name TEXT NOT NULL, description TEXT NOT NULL,
        added_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%S UTC','now'))
        );
        CREATE TABLE IF NOT EXISTS watchlist_details (
        title_id TEXT PRIMARY KEY, watched INTEGER NOT NULL DEFAULT 0 CHECK(watched IN (0,1)),
        notes TEXT NOT NULL DEFAULT ''
        );",
    )
}

pub fn contains(path: &Path, id: &str) -> rusqlite::Result<bool> {
    Connection::open(path)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM watchlist WHERE title_id=?1)",
        [id],
        |r| r.get(0),
    )
}

fn description(kind: &str, start: Option<i64>, end: Option<i64>) -> String {
    let years = match (start, end) {
        (Some(start), Some(end)) if start != end => format!("{start}–{end}"),
        (Some(start), _) => start.to_string(),
        _ => String::new(),
    };
    format!(
        "{}{}{}",
        years,
        if years.is_empty() { "" } else { " · " },
        crate::database::title_type_label(kind)
    )
}

pub fn update(path: &Path, id: &str, add: bool) -> rusqlite::Result<bool> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    update_connection(&mut connection, id, add)
}

fn update_connection(connection: &mut Connection, id: &str, add: bool) -> rusqlite::Result<bool> {
    let transaction = connection.transaction()?;
    if !add {
        transaction.execute("DELETE FROM watchlist_details WHERE title_id=?1", [id])?;
        transaction.execute("DELETE FROM watchlist WHERE title_id=?1", [id])?;
        transaction.commit()?;
        return Ok(true);
    }
    let existing: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM watchlist WHERE title_id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if existing {
        return Ok(true);
    }
    let basics=transaction.query_row("SELECT primary_title,title_type,start_year,end_year FROM imdb_titles WHERE title_id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,Option<i64>>(3)?))).optional()?;
    let metadata = if let Some((title, kind, start, end)) = basics {
        Some((title, description(&kind, start, end)))
    } else {
        // A title not yet in the bulk dataset may still have an IMDb summary cached.
        let payload = transaction
            .query_row(
                "SELECT payload FROM imdb_cache WHERE cache_key=?1",
                [format!("title-summary:{id}")],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        payload
            .and_then(|p| serde_json::from_str::<crate::imdb::SearchTitle>(&p).ok())
            .filter(|title| title.id == id && !title.title.is_empty())
            .map(|title| (title.title, title.description))
    };
    let Some((title, description)) = metadata else {
        return Ok(false);
    };
    transaction.execute(
        "INSERT INTO watchlist(title_id,title_name,description) VALUES (?1,?2,?3)",
        rusqlite::params![id, title, description],
    )?;
    transaction.commit()?;
    Ok(true)
}

pub fn save_details(path: &Path, id: &str, watched: bool, notes: &str) -> rusqlite::Result<bool> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    save_details_connection(&mut connection, id, watched, notes)
}
fn save_details_connection(
    connection: &mut Connection,
    id: &str,
    watched: bool,
    notes: &str,
) -> rusqlite::Result<bool> {
    let transaction = connection.transaction()?;
    let changed = transaction.execute(
        "INSERT INTO watchlist_details(title_id,watched,notes)
        SELECT title_id,?2,?3 FROM watchlist WHERE title_id=?1
        ON CONFLICT(title_id) DO UPDATE SET watched=excluded.watched,notes=excluded.notes",
        rusqlite::params![id, watched, notes],
    )?;
    transaction.commit()?;
    Ok(changed > 0)
}

pub struct Item {
    pub id: String,
    pub title: String,
    pub description: String,
    pub added_at: String,
    pub watched: bool,
    pub notes: String,
}

pub fn list(path: &Path) -> rusqlite::Result<Vec<Item>> {
    list_connection(&Connection::open(path)?)
}

fn list_connection(connection: &Connection) -> rusqlite::Result<Vec<Item>> {
    let mut statement=connection.prepare("SELECT w.title_id,COALESCE(t.primary_title,w.title_name),w.description,w.added_at,
        t.title_type,t.start_year,t.end_year,COALESCE(d.watched,0),COALESCE(d.notes,'') FROM watchlist w LEFT JOIN imdb_titles t ON t.title_id=w.title_id
        LEFT JOIN watchlist_details d ON d.title_id=w.title_id
        ORDER BY w.added_at DESC,w.title_id")?;
    statement
        .query_map([], |r| {
            let kind: Option<String> = r.get(4)?;
            let description = match kind {
                Some(kind) => description(&kind, r.get(5)?, r.get(6)?),
                None => r.get(2)?,
            };
            Ok(Item {
                id: r.get(0)?,
                title: r.get(1)?,
                description,
                added_at: r.get(3)?,
                watched: r.get(7)?,
                notes: r.get(8)?,
            })
        })?
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db.execute_batch("CREATE TABLE imdb_titles(title_id TEXT PRIMARY KEY,primary_title TEXT,title_type TEXT,start_year INTEGER,end_year INTEGER);
            CREATE TABLE imdb_cache(cache_key TEXT PRIMARY KEY,payload TEXT);
            INSERT INTO imdb_titles VALUES ('tt1375666','Inception','movie',2010,NULL);").unwrap();
        db
    }
    #[test]
    fn watched_status_and_notes_preserve_saved_titles_and_do_not_create_unknown_entries() {
        let mut db = database();
        update_connection(&mut db, "tt1375666", true).unwrap();
        db.execute("UPDATE watchlist SET added_at='original timestamp'", [])
            .unwrap();
        initialize(&db).unwrap();
        assert!(!list_connection(&db).unwrap()[0].watched);
        assert!(list_connection(&db).unwrap()[0].notes.is_empty());
        assert!(!save_details_connection(&mut db, "tt9999999", true, "unknown").unwrap());
        assert!(
            save_details_connection(&mut db, "tt1375666", true, "A note\nwith Unicode: René 🎬")
                .unwrap()
        );
        update_connection(&mut db, "tt1375666", true).unwrap();
        db.execute("DELETE FROM imdb_titles", []).unwrap();
        let items = list_connection(&db).unwrap();
        assert!(items[0].watched);
        assert_eq!(items[0].notes, "A note\nwith Unicode: René 🎬");
        assert_eq!(items[0].added_at, "original timestamp");
        assert!(save_details_connection(&mut db, "tt1375666", false, "").unwrap());
        assert!(!list_connection(&db).unwrap()[0].watched);
        assert!(list_connection(&db).unwrap()[0].notes.is_empty());
        update_connection(&mut db, "tt1375666", false).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM watchlist_details", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(!save_details_connection(&mut db, "tt1375666", true, "stale form").unwrap());
    }
    #[test]
    fn existing_watchlist_schema_is_preserved_when_details_storage_is_added() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE watchlist(title_id TEXT PRIMARY KEY,title_name TEXT NOT NULL,description TEXT NOT NULL,added_at TEXT NOT NULL); INSERT INTO watchlist VALUES ('tt1375666','Inception','2010','original');").unwrap();
        initialize(&db).unwrap();
        initialize(&db).unwrap();
        assert_eq!(
            db.query_row("SELECT added_at FROM watchlist", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "original"
        );
    }
    #[test]
    fn saves_are_idempotent_and_survive_dataset_replacement() {
        let mut db = database();
        assert!(update_connection(&mut db, "tt1375666", true).unwrap());
        db.execute("UPDATE watchlist SET added_at='original timestamp'", [])
            .unwrap();
        assert!(update_connection(&mut db, "tt1375666", true).unwrap());
        let list = list_connection(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].added_at, "original timestamp");
        db.execute(
            "UPDATE imdb_titles SET primary_title='Updated IMDb title'",
            [],
        )
        .unwrap();
        assert_eq!(list_connection(&db).unwrap()[0].title, "Updated IMDb title");
        db.execute("DELETE FROM imdb_titles", []).unwrap();
        let list = list_connection(&db).unwrap();
        assert_eq!(list[0].title, "Inception");
        assert_eq!(list[0].description, "2010 · Movie");
        assert!(update_connection(&mut db, "tt1375666", false).unwrap());
        assert!(update_connection(&mut db, "tt1375666", false).unwrap());
        assert!(list_connection(&db).unwrap().is_empty());
    }
    #[test]
    fn summary_only_titles_can_be_saved_but_unknown_or_mismatched_ids_cannot() {
        let mut db = database();
        assert!(!update_connection(&mut db, "tt9999999", true).unwrap());
        let summary = r#"{"id":"tt9999999","l":"New IMDb title","tl":"2026"}"#;
        db.execute(
            "INSERT INTO imdb_cache VALUES ('title-summary:tt9999999',?1)",
            [summary],
        )
        .unwrap();
        assert!(update_connection(&mut db, "tt9999999", true).unwrap());
        assert_eq!(list_connection(&db).unwrap()[0].title, "New IMDb title");
        db.execute(
            "INSERT INTO imdb_cache VALUES ('title-summary:tt9999998',?1)",
            [summary],
        )
        .unwrap();
        assert!(!update_connection(&mut db, "tt9999998", true).unwrap());
    }
}
