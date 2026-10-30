use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

type Error = Box<dyn std::error::Error + Send + Sync>;

struct PendingBackup(PathBuf);
impl Drop for PendingBackup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn unique_name() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Unable to create temporary backup name: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Build and verify a standalone SQLite snapshot before publishing its requested name.
pub fn create(source: &Path, destination: &Path) -> Result<u64, Error> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Backup destination already exists; choose a new filename",
            )
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::from_secs(10))?;
    // Pin one committed snapshot, including rows still in the source WAL file.
    source.execute_batch("BEGIN; SELECT name FROM sqlite_schema LIMIT 1;")?;

    let temporary = parent.join(format!(".rustymdb-backup-{}.partial", unique_name()?));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(&temporary)?);
    let pending = PendingBackup(temporary);
    let mut target = Connection::open(&pending.0)?;
    {
        let backup = Backup::new(&source, &mut target)?;
        let deadline = Instant::now() + Duration::from_secs(3600);
        let mut last_bucket = -1;
        loop {
            let result = backup.step(4096)?;
            let progress = backup.progress();
            if progress.pagecount > 0 {
                let percent = (i64::from(progress.pagecount - progress.remaining) * 100
                    / i64::from(progress.pagecount)) as i32;
                if percent / 10 != last_bucket {
                    println!("Backup copy: {percent}%");
                    last_bucket = percent / 10;
                }
            }
            if matches!(result, StepResult::Done) {
                break;
            }
            if Instant::now() >= deadline {
                return Err("Backup timed out; no completed backup was published".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    source.execute_batch("ROLLBACK")?;
    // A completed backup is one file: no separate WAL file is needed for transfer.
    target.execute_batch("PRAGMA journal_mode=DELETE;")?;
    println!("Verifying backup with SQLite quick_check…");
    let checks = target
        .prepare("PRAGMA quick_check")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if checks != ["ok"] {
        return Err(format!("Backup verification failed: {}", checks.join("; ")).into());
    }
    drop(target);
    fs::File::open(&pending.0)?.sync_all()?;
    let bytes = fs::metadata(&pending.0)?.len();
    // Same-directory hard linking publishes atomically and never overwrites another file.
    fs::hard_link(&pending.0, destination)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rustymdb-backup-test-{}", unique_name().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn online_backup_includes_committed_wal_rows_and_is_a_standalone_file() {
        let directory = Directory::new();
        let source = directory.0.join("source.sqlite3");
        let destination = directory.0.join("backup.sqlite3");
        let live = Connection::open(&source).unwrap();
        live.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
            CREATE TABLE watchlist(title_id TEXT PRIMARY KEY,title_name TEXT);
            INSERT INTO watchlist VALUES ('tt1375666','Inception');",
        )
        .unwrap();
        assert!(directory.0.join("source.sqlite3-wal").exists());
        assert!(create(&source, &destination).unwrap() > 0);
        let restored =
            Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            restored
                .query_row("SELECT title_name FROM watchlist", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "Inception"
        );
        assert_eq!(
            restored
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        assert!(!directory.0.join("backup.sqlite3-wal").exists());
        live.execute(
            "INSERT INTO watchlist VALUES ('tt0133093','The Matrix')",
            [],
        )
        .unwrap();
        assert_eq!(
            restored
                .query_row("SELECT COUNT(*) FROM watchlist", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(!fs::read_dir(&directory.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial")
        }));
    }
    #[test]
    fn existing_destinations_and_the_original_database_are_never_overwritten() {
        let directory = Directory::new();
        let source = directory.0.join("source.sqlite3");
        let destination = directory.0.join("existing.sqlite3");
        Connection::open(&source)
            .unwrap()
            .execute_batch(
                "CREATE TABLE example(value TEXT); INSERT INTO example VALUES ('preserved');",
            )
            .unwrap();
        fs::write(&destination, b"existing backup bytes").unwrap();
        assert!(create(&source, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"existing backup bytes");
        assert!(create(&source, &source).is_err());
        assert_eq!(
            Connection::open(&source)
                .unwrap()
                .query_row("SELECT value FROM example", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }
    #[test]
    fn invalid_or_missing_sources_publish_nothing_and_leave_no_partial_files() {
        let directory = Directory::new();
        let source = directory.0.join("bad.sqlite3");
        let destination = directory.0.join("backup.sqlite3");
        fs::write(&source, b"not a SQLite database").unwrap();
        assert!(create(&source, &destination).is_err());
        assert!(!destination.exists());
        let missing = directory.0.join("missing.sqlite3");
        assert!(create(&missing, &destination).is_err());
        assert!(!missing.exists());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
}
