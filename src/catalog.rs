use crate::{database, imdb};
use rusqlite::{Connection, OptionalExtension};
use std::{path::Path, time::Duration};

#[derive(Clone, Copy, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleFilter {
    #[default]
    All,
    Movies,
    Series,
    Episodes,
    People,
}

impl TitleFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Movies => "movies",
            Self::Series => "series",
            Self::Episodes => "episodes",
            Self::People => "people",
        }
    }

    fn predicate(self) -> &'static str {
        match self {
            Self::All => "1 = 1",
            Self::Movies => "t.title_type IN ('movie', 'tvMovie')",
            Self::Series => "t.title_type IN ('tvSeries', 'tvMiniSeries')",
            Self::Episodes => "t.title_type = 'tvEpisode'",
            Self::People => "0 = 1",
        }
    }
}

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS title_search USING fts5(
         title_id UNINDEXED, primary_title, original_title,
         tokenize = 'unicode61 remove_diacritics 2');",
    )
}

pub fn rebuild(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM title_search", [])?;
    connection.execute(
        "INSERT INTO title_search (title_id, primary_title, original_title)
         SELECT title_id, primary_title, original_title FROM imdb_titles",
        [],
    )?;
    connection.execute(
        "INSERT INTO dataset_imports (name, imported_at, source_modified, row_count)
         SELECT 'title-search', imported_at, source_modified, row_count
         FROM dataset_imports WHERE name = 'titles'
         ON CONFLICT(name) DO UPDATE SET imported_at=excluded.imported_at,
         source_modified=excluded.source_modified, row_count=excluded.row_count",
        [],
    )?;
    Ok(())
}

pub fn build_index(path: &Path) -> rusqlite::Result<()> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(10))?;
    let imported: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM dataset_imports WHERE name='titles')",
        [],
        |r| r.get(0),
    )?;
    if !imported {
        return Err(rusqlite::Error::InvalidParameterName(
            "Import titles before indexing them".into(),
        ));
    }
    let transaction = connection.transaction()?;
    rebuild(&transaction)?;
    transaction.commit()
}

fn expression(query: &str) -> Option<String> {
    let words: Vec<_> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\"*"))
        .collect();
    (!words.is_empty()).then(|| words.join(" AND "))
}

// None means no local index is ready. Some(empty) means a valid local search had no matches.
pub fn search(
    path: &Path,
    query: &str,
    filter: TitleFilter,
    page: u32,
) -> rusqlite::Result<Option<Vec<imdb::SearchTitle>>> {
    let connection = Connection::open(path)?;
    if filter == TitleFilter::People {
        return crate::people::search(&connection, query, page);
    }
    paged_search(&connection, query, filter, page)
}

fn paged_search(
    connection: &Connection,
    query: &str,
    filter: TitleFilter,
    page: u32,
) -> rusqlite::Result<Option<Vec<imdb::SearchTitle>>> {
    // Keep index-readiness and the search on the same committed snapshot during refreshes.
    connection.execute_batch("BEGIN")?;
    let result = (|| {
        let ready = connection
            .query_row(
                "SELECT row_count FROM dataset_imports WHERE name='title-search'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        if ready.is_none() {
            return Ok(None);
        }
        let columns = "SELECT t.title_id, t.primary_title, t.title_type, t.start_year, t.end_year
                       FROM imdb_titles t";
        let is_id = imdb::valid_title_id(query);
        // Only these fixed enum predicates enter SQL; query text stays parameterized.
        let predicate = filter.predicate();
        let offset = i64::from(page.saturating_sub(1)) * 30;
        let sql = if is_id {
            format!("{columns} WHERE t.title_id = ?1 AND {predicate} LIMIT 31 OFFSET ?2")
        } else {
            format!("{columns} JOIN title_search ON title_search.title_id = t.title_id
                     LEFT JOIN imdb_ratings r ON r.title_id = t.title_id
                     WHERE title_search MATCH ?1 AND {predicate}
                     ORDER BY CASE WHEN lower(t.primary_title) = lower(?2) OR lower(t.original_title) = lower(?2) THEN 0 ELSE 1 END,
                     COALESCE(r.votes, 0) DESC, title_search.rank, t.title_id LIMIT 31 OFFSET ?3")
        };
        let Some(term) = (if is_id {
            Some(query.to_owned())
        } else {
            expression(query)
        }) else {
            return Ok(Some(vec![]));
        };
        let mut statement = connection.prepare(&sql)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            let start: Option<i64> = row.get(3)?;
            let end: Option<i64> = row.get(4)?;
            let years = match (start, end) {
                (Some(start), Some(end)) if start != end => format!("{start}–{end}"),
                (Some(start), _) => start.to_string(),
                _ => String::new(),
            };
            let kind: String = row.get(2)?;
            Ok(imdb::SearchTitle {
                id: row.get(0)?,
                title: row.get(1)?,
                description: format!(
                    "{}{}{}",
                    years,
                    if years.is_empty() { "" } else { " · " },
                    database::title_type_label(&kind)
                ),
                cast: String::new(),
            })
        };
        let rows = if is_id {
            statement.query_map(rusqlite::params![term, offset], map_row)?
        } else {
            statement.query_map(rusqlite::params![term, query, offset], map_row)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>().map(Some)
    })();
    connection.execute_batch("ROLLBACK")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered_search(
        db: &Connection,
        query: &str,
        filter: TitleFilter,
    ) -> rusqlite::Result<Option<Vec<imdb::SearchTitle>>> {
        paged_search(db, query, filter, 1)
    }

    #[test]
    fn pages_have_stable_order_without_duplicate_results_and_respect_filters() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE imdb_titles (title_id TEXT PRIMARY KEY, primary_title TEXT, original_title TEXT, title_type TEXT, start_year INTEGER, end_year INTEGER);
        CREATE TABLE imdb_ratings (title_id TEXT PRIMARY KEY, votes INTEGER);
        CREATE TABLE dataset_imports (name TEXT PRIMARY KEY, imported_at TEXT, source_modified TEXT, row_count INTEGER);
        INSERT INTO dataset_imports VALUES ('titles','now','today',65);").unwrap();
        initialize(&db).unwrap();
        for index in 1..=65 {
            db.execute(
                "INSERT INTO imdb_titles VALUES (?1,'Shared','Shared',?2,2000,NULL)",
                rusqlite::params![
                    format!("tt{index:07}"),
                    if index <= 62 { "movie" } else { "tvEpisode" }
                ],
            )
            .unwrap();
        }
        rebuild(&db).unwrap();
        let mut ids = vec![];
        for (page, expected) in [(1, 31), (2, 31), (3, 2)] {
            let mut results = paged_search(&db, "Shared", TitleFilter::Movies, page)
                .unwrap()
                .unwrap();
            assert_eq!(results.len(), expected);
            results.truncate(30);
            ids.extend(results.into_iter().map(|r| r.id));
        }
        assert_eq!(
            ids,
            (1..=62).map(|i| format!("tt{i:07}")).collect::<Vec<_>>()
        );
        assert!(
            paged_search(&db, "Shared", TitleFilter::Movies, 4)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(
            paged_search(&db, "tt0000001", TitleFilter::All, 2)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            paged_search(&db, "Shared", TitleFilter::Episodes, 1)
                .unwrap()
                .unwrap()
                .len(),
            3
        );
    }

    fn search_connection(
        db: &Connection,
        query: &str,
    ) -> rusqlite::Result<Option<Vec<imdb::SearchTitle>>> {
        filtered_search(db, query, TitleFilter::All)
    }
    #[test]
    fn searches_words_original_titles_accents_and_ids_without_executing_fts_syntax() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE imdb_titles (title_id TEXT PRIMARY KEY, primary_title TEXT, original_title TEXT, title_type TEXT, start_year INTEGER, end_year INTEGER);
        CREATE TABLE imdb_ratings (title_id TEXT PRIMARY KEY, votes INTEGER);
        CREATE TABLE dataset_imports (name TEXT PRIMARY KEY, imported_at TEXT, source_modified TEXT, row_count INTEGER);
        INSERT INTO dataset_imports VALUES ('titles','now','today',3);
        INSERT INTO imdb_titles VALUES ('tt0000001','Amélie','Le fabuleux destin','movie',2001,NULL),
        ('tt0000002','Star Wars','Star Wars','movie',1977,NULL),
        ('tt0000003','Star Wars Documentary','Star Wars Documentary','tvSeries',2000,2005);
        INSERT INTO imdb_ratings VALUES ('tt0000002',100),('tt0000003',200);").unwrap();
        initialize(&db).unwrap();
        assert!(search_connection(&db, "Star").unwrap().is_none());
        rebuild(&db).unwrap();
        assert_eq!(
            search_connection(&db, "amelie").unwrap().unwrap()[0].title,
            "Amélie"
        );
        assert_eq!(
            search_connection(&db, "fabuleux").unwrap().unwrap()[0].id,
            "tt0000001"
        );
        let matches = search_connection(&db, "star wars").unwrap().unwrap();
        assert_eq!(matches[0].id, "tt0000002");
        assert!(matches[1].description.contains("2000–2005"));
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::Movies)
                .unwrap()
                .unwrap()[0]
                .id,
            "tt0000002"
        );
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::Movies)
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::Series)
                .unwrap()
                .unwrap()[0]
                .id,
            "tt0000003"
        );
        assert!(
            filtered_search(&db, "tt0000002", TitleFilter::Series)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        db.execute_batch(
            "INSERT INTO imdb_titles VALUES
            ('tt0000004','Star Test','Star Test','tvEpisode',2000,NULL),
            ('tt0000005','Star Test Movie','Star Test Movie','tvMovie',2000,NULL),
            ('tt0000006','Star Test Mini','Star Test Mini','tvMiniSeries',2000,2001),
            ('tt0000007','Star Test Short','Star Test Short','short',2000,NULL);",
        )
        .unwrap();
        rebuild(&db).unwrap();
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::Episodes)
                .unwrap()
                .unwrap()[0]
                .id,
            "tt0000004"
        );
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::Movies)
                .unwrap()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::Series)
                .unwrap()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            filtered_search(&db, "star", TitleFilter::All)
                .unwrap()
                .unwrap()
                .len(),
            6
        );
        assert_eq!(
            search_connection(&db, "tt0000001").unwrap().unwrap().len(),
            1
        );
        assert!(
            search_connection(&db, "\"* - ()")
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(
            search_connection(&db, "Star OR Missing")
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(
            search_connection(&db, "Unknown")
                .unwrap()
                .unwrap()
                .is_empty()
        );
        db.execute("DELETE FROM imdb_titles WHERE title_id='tt0000001'", [])
            .unwrap();
        rebuild(&db).unwrap();
        assert!(
            search_connection(&db, "amelie")
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }
}
