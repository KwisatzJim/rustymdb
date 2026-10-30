use rusqlite::Connection;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{fs, path::Path, time::Duration};
type Error = Box<dyn std::error::Error + Send + Sync>;
pub struct View {
    pub keep: u32,
    pub managed: i64,
}
pub fn initialize(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS backup_retention(singleton INTEGER PRIMARY KEY CHECK(singleton=1),keep_count INTEGER NOT NULL CHECK(keep_count IN (0,3,5,10,20))); INSERT OR IGNORE INTO backup_retention VALUES(1,0);
    CREATE TABLE IF NOT EXISTS managed_backups(job_id INTEGER PRIMARY KEY,filename TEXT NOT NULL UNIQUE,size INTEGER NOT NULL,modified TEXT NOT NULL,identity TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'retained',recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%S UTC','now')));")
}
pub fn view(path: &Path) -> rusqlite::Result<View> {
    let c = Connection::open(path)?;
    Ok(View {
        keep: c.query_row(
            "SELECT keep_count FROM backup_retention WHERE singleton=1",
            [],
            |r| r.get(0),
        )?,
        managed: c.query_row(
            "SELECT COUNT(*) FROM managed_backups WHERE state='retained'",
            [],
            |r| r.get(0),
        )?,
    })
}
pub fn save(path: &Path, keep: u32) -> rusqlite::Result<()> {
    if ![0, 3, 5, 10, 20].contains(&keep) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(5))?;
    c.execute(
        "UPDATE backup_retention SET keep_count=?1 WHERE singleton=1",
        [keep],
    )?;
    Ok(())
}
fn fingerprint(path: &Path) -> Result<(i64, String, String), Error> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() {
        return Err("Backup is not a regular file".into());
    }
    let modified = m
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    #[cfg(unix)]
    let identity = format!("{}:{}", m.dev(), m.ino());
    #[cfg(not(unix))]
    let identity = String::new();
    Ok((i64::try_from(m.len())?, modified, identity))
}
fn safe_name(name: &str, job: i64) -> bool {
    let Some(seconds) = name
        .strip_prefix("rustymdb-backup-")
        .and_then(|v| v.strip_suffix(&format!("-job-{job}.sqlite3")))
    else {
        return false;
    };
    job > 0 && !seconds.is_empty() && seconds.bytes().all(|b| b.is_ascii_digit())
}
// Called only after the backup subprocess has returned success from its verification.
pub fn complete(database: &Path, data: &Path, job: i64, file: &Path) -> Result<usize, Error> {
    let directory = data.join("backups");
    if !fs::symlink_metadata(&directory)?.is_dir() {
        return Err("Backup directory is not a regular directory".into());
    }
    let filename = file
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("Invalid backup filename")?;
    if file.parent() != Some(directory.as_path()) || !safe_name(filename, job) {
        return Err("Backup path is outside the managed backup directory".into());
    }
    let (size, modified, identity) = fingerprint(file)?;
    let c = Connection::open(database)?;
    c.busy_timeout(Duration::from_secs(5))?;
    let scheduled:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM maintenance_jobs WHERE id=?1 AND label LIKE 'Scheduled:%' AND state='running')",[job],|r|r.get(0))?;
    if !scheduled {
        return Err("Only a successful scheduled backup can trigger retention".into());
    }
    c.execute("INSERT INTO managed_backups(job_id,filename,size,modified,identity) VALUES (?1,?2,?3,?4,?5)",rusqlite::params![job,filename,size,modified,identity])?;
    let keep: u32 = c.query_row(
        "SELECT keep_count FROM backup_retention WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if keep == 0 {
        return Ok(0);
    }
    let mut statement=c.prepare("SELECT b.job_id,b.filename,b.size,b.modified,b.identity FROM managed_backups b JOIN maintenance_jobs j ON j.id=b.job_id WHERE b.state='retained' AND j.label LIKE 'Scheduled:%' AND (j.state='succeeded' OR j.id=?1) ORDER BY b.job_id DESC")?;
    let rows = statement
        .query_map([job], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut valid = Vec::new();
    for (id, name, size, modified, identity) in rows {
        if !safe_name(&name, id) {
            continue;
        }
        let path = directory.join(&name);
        match fingerprint(&path) {
            Ok(actual) if actual == (size, modified.clone(), identity.clone()) => {
                valid.push((id, path, (size, modified, identity)))
            }
            _ => {
                c.execute(
                    "UPDATE managed_backups SET state='preserved' WHERE job_id=?1",
                    [id],
                )?;
            }
        }
    }
    // Never prune without a confirmed new backup still present and unchanged.
    if valid.first().map(|v| v.0) != Some(job) {
        return Err("New backup changed; retention skipped".into());
    }
    let mut removed = 0;
    for (id, path, expected) in valid.into_iter().skip(keep as usize) {
        let current: u32 = c.query_row(
            "SELECT keep_count FROM backup_retention WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if current != keep {
            break;
        }
        if fingerprint(&path).ok() != Some(expected) {
            continue;
        }
        c.execute(
            "UPDATE managed_backups SET state='deleting' WHERE job_id=?1",
            [id],
        )?;
        fs::remove_file(&path)?;
        c.execute(
            "UPDATE managed_backups SET state='deleted' WHERE job_id=?1",
            [id],
        )?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut random = [0; 8];
            getrandom::fill(&mut random).unwrap();
            let suffix = random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let path = std::env::temp_dir().join(format!("rustymdb-retention-{suffix}"));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("backups")).unwrap();
            let c = Connection::open(path.join("rustymdb.sqlite3")).unwrap();
            crate::maintenance::initialize(&c).unwrap();
            initialize(&c).unwrap();
            Self(path)
        }
        fn db(&self) -> std::path::PathBuf {
            self.0.join("rustymdb.sqlite3")
        }
        fn file(&self, id: i64) -> std::path::PathBuf {
            self.0
                .join("backups")
                .join(format!("rustymdb-backup-123-job-{id}.sqlite3"))
        }
        fn backup(&self, id: i64) -> usize {
            let c = Connection::open(self.db()).unwrap();
            c.execute("INSERT INTO maintenance_jobs(id,label,state,detail) VALUES (?1,'Scheduled: Create verified backup','running','test')",[id]).unwrap();
            let p = self.file(id);
            let b = Connection::open(&p).unwrap();
            b.execute_batch(
                "CREATE TABLE fixture(value TEXT); INSERT INTO fixture VALUES ('verified backup');",
            )
            .unwrap();
            drop(b);
            let count = complete(&self.db(), &self.0, id, &p).unwrap();
            c.execute(
                "UPDATE maintenance_jobs SET state='succeeded' WHERE id=?1",
                [id],
            )
            .unwrap();
            count
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn defaults_preserve_all_and_enabling_prunes_only_after_a_new_backup() {
        let d = Directory::new();
        assert_eq!(view(&d.db()).unwrap().keep, 0);
        for id in 1..=5 {
            assert_eq!(d.backup(id), 0);
        }
        let manual = d.0.join("backups/manual.sqlite3");
        fs::write(&manual, b"manual").unwrap();
        let untracked = d.file(999);
        fs::write(&untracked, b"untracked").unwrap();
        save(&d.db(), 3).unwrap();
        assert!(d.file(1).exists());
        assert_eq!(d.backup(6), 3);
        for id in 1..=3 {
            assert!(!d.file(id).exists());
        }
        for id in 4..=6 {
            assert!(d.file(id).exists());
        }
        assert!(manual.exists() && untracked.exists());
        assert_eq!(view(&d.db()).unwrap().managed, 3);
        save(&d.db(), 0).unwrap();
        assert_eq!(d.backup(7), 0);
        assert!(d.file(4).exists());
        assert!(save(&d.db(), 1).is_err());
    }
    #[test]
    fn changed_files_and_symlinks_are_preserved() {
        let d = Directory::new();
        for id in 1..=3 {
            d.backup(id);
        }
        fs::write(d.file(1), b"replaced by the owner").unwrap();
        #[cfg(unix)]
        {
            fs::remove_file(d.file(2)).unwrap();
            std::os::unix::fs::symlink(d.file(1), d.file(2)).unwrap();
        }
        save(&d.db(), 3).unwrap();
        assert_eq!(d.backup(4), 0);
        assert_eq!(d.backup(5), 0);
        assert!(d.file(1).exists());
        assert_eq!(fs::read(d.file(1)).unwrap(), b"replaced by the owner");
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(d.file(2))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(d.file(3).exists());
    }
    #[test]
    fn missing_new_backups_or_non_scheduled_jobs_cannot_trigger_cleanup() {
        let d = Directory::new();
        for id in 1..=3 {
            d.backup(id);
        }
        save(&d.db(), 3).unwrap();
        let c = Connection::open(d.db()).unwrap();
        c.execute("INSERT INTO maintenance_jobs(id,label,state,detail) VALUES (4,'Create verified backup','running','manual')",[]).unwrap();
        fs::write(d.file(4), b"manual file").unwrap();
        assert!(complete(&d.db(), &d.0, 4, &d.file(4)).is_err());
        assert!(complete(&d.db(), &d.0, 5, &d.file(5)).is_err());
        for id in 1..=4 {
            assert!(d.file(id).exists());
        }
        assert!(!safe_name("../rustymdb-backup-123-job-4.sqlite3", 4));
    }
}
