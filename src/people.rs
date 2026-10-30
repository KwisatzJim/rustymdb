use rusqlite::Connection;
use std::{path::Path, time::Duration};

pub fn initialize(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS people_search USING fts5(person_id UNINDEXED,primary_name,tokenize='unicode61 remove_diacritics 2');")
}
pub fn rebuild(c: &Connection) -> rusqlite::Result<()> {
    c.execute("DELETE FROM people_search", [])?;
    c.execute("INSERT INTO people_search SELECT person_id,primary_name FROM imdb_people WHERE primary_name IS NOT NULL",[])?;
    c.execute("INSERT INTO dataset_imports(name,imported_at,source_modified,row_count) SELECT 'people-search',imported_at,source_modified,row_count FROM dataset_imports WHERE name='people' ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,source_modified=excluded.source_modified,row_count=excluded.row_count",[])?;
    Ok(())
}
pub fn build_index(path: &Path) -> rusqlite::Result<()> {
    let mut c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(5))?;
    if !c.query_row(
        "SELECT EXISTS(SELECT 1 FROM dataset_imports WHERE name='people')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let t = c.transaction()?;
    rebuild(&t)?;
    t.commit()
}
pub fn search(
    c: &Connection,
    query: &str,
    page: u32,
) -> rusqlite::Result<Option<Vec<crate::imdb::SearchTitle>>> {
    c.execute_batch("BEGIN")?;
    let result = (|| {
        if !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM dataset_imports WHERE name='people-search')",
            [],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(None);
        }
        let offset = i64::from(page.saturating_sub(1)) * 30;
        let map = |r: &rusqlite::Row<'_>| {
            Ok(crate::imdb::SearchTitle {
                id: r.get(0)?,
                title: r.get(1)?,
                description: format!("IMDb person · {}", r.get::<_, String>(0)?),
                cast: String::new(),
            })
        };
        if crate::credits::person_id(query) {
            let mut s=c.prepare("SELECT person_id,primary_name FROM imdb_people WHERE person_id=?1 AND primary_name IS NOT NULL LIMIT 31 OFFSET ?2")?;
            return s
                .query_map(rusqlite::params![query, offset], map)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(Some);
        }
        let words: Vec<_> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| format!("\"{w}\"*"))
            .collect();
        if words.is_empty() {
            return Ok(Some(vec![]));
        }
        let mut s=c.prepare("SELECT person_id,primary_name FROM people_search WHERE people_search MATCH ?1 ORDER BY CASE WHEN lower(primary_name)=lower(?2) THEN 0 ELSE 1 END, primary_name COLLATE NOCASE,person_id LIMIT 31 OFFSET ?3")?;
        s.query_map(rusqlite::params![words.join(" AND "), query, offset], map)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map(Some)
    })();
    c.execute_batch("ROLLBACK")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        initialize(&c).unwrap();
        c.execute_batch("CREATE TABLE imdb_people(person_id TEXT PRIMARY KEY,primary_name TEXT); CREATE TABLE dataset_imports(name TEXT PRIMARY KEY,imported_at TEXT,source_modified TEXT,row_count INTEGER); INSERT INTO dataset_imports VALUES ('people','now','test',3); INSERT INTO imdb_people VALUES ('nm0000138','Leonardo DiCaprio'),('nm0000001','René Actor'),('nm0000002','Christopher Nolan');").unwrap();
        c
    }
    #[test]
    fn searches_prefixes_accents_and_ids_without_executing_query_syntax() {
        let c = database();
        assert!(search(&c, "Leonardo", 1).unwrap().is_none());
        rebuild(&c).unwrap();
        assert_eq!(
            search(&c, "leo dica", 1).unwrap().unwrap()[0].id,
            "nm0000138"
        );
        assert_eq!(search(&c, "rene", 1).unwrap().unwrap()[0].id, "nm0000001");
        assert_eq!(
            search(&c, "nm0000002", 1).unwrap().unwrap()[0].title,
            "Christopher Nolan"
        );
        assert!(search(&c, "nm9999999", 1).unwrap().unwrap().is_empty());
        assert!(search(&c, r#"" OR *"#, 1).unwrap().unwrap().is_empty());
        assert!(search(&c, "---", 1).unwrap().unwrap().is_empty());
        c.execute(
            "UPDATE imdb_people SET primary_name='Updated Name' WHERE person_id='nm0000138'",
            [],
        )
        .unwrap();
        rebuild(&c).unwrap();
        assert!(search(&c, "Leonardo", 1).unwrap().unwrap().is_empty());
        assert_eq!(search(&c, "Updated", 1).unwrap().unwrap().len(), 1);
    }
    #[test]
    fn paginates_names_in_stable_order_without_duplicates() {
        let c = database();
        for i in 0..65 {
            c.execute(
                "INSERT INTO imdb_people VALUES (?1,'Example Person')",
                [format!("nm{:07}", 1000 + i)],
            )
            .unwrap();
        }
        rebuild(&c).unwrap();
        let first = search(&c, "Example", 1).unwrap().unwrap();
        let second = search(&c, "Example", 2).unwrap().unwrap();
        assert_eq!(first.len(), 31);
        assert_eq!(second.len(), 31);
        assert!(
            !first[..30]
                .iter()
                .any(|a| second[..30].iter().any(|b| a.id == b.id))
        );
        assert_eq!(search(&c, "Example", 3).unwrap().unwrap().len(), 5);
        assert!(search(&c, "Example", u32::MAX).unwrap().unwrap().is_empty());
    }
}
