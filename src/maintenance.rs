use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS maintenance_jobs (
        id INTEGER PRIMARY KEY, label TEXT NOT NULL, state TEXT NOT NULL,
        started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%S UTC','now')),
        finished_at TEXT, detail TEXT NOT NULL);
        CREATE UNIQUE INDEX IF NOT EXISTS one_maintenance_job ON maintenance_jobs((1)) WHERE state='running';
        CREATE TABLE IF NOT EXISTS maintenance_schedules (
        kind TEXT PRIMARY KEY CHECK(kind IN ('backup','refresh')), days INTEGER NOT NULL DEFAULT 0 CHECK(days IN (0,1,7,30)),
        next_run INTEGER, last_job_id INTEGER);
        INSERT OR IGNORE INTO maintenance_schedules(kind) VALUES ('backup'),('refresh');")
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dataset {
    #[default]
    All,
    Ratings,
    Titles,
    Credits,
    Cast,
    Episodes,
    People,
}

impl Dataset {
    fn commands(self) -> &'static [&'static str] {
        match self {
            Self::All => &[
                "import-ratings",
                "import-titles",
                "import-credits",
                "import-cast",
                "import-episodes",
            ],
            Self::Ratings => &["import-ratings"],
            Self::Titles => &["import-titles"],
            Self::Credits => &["import-credits"],
            Self::Cast => &["import-cast"],
            Self::Episodes => &["import-episodes"],
            Self::People => &["index-people"],
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::All => "Refresh all datasets",
            Self::Ratings => "Refresh ratings",
            Self::Titles => "Refresh titles and search index",
            Self::Credits => "Refresh directors, writers, and people",
            Self::Cast => "Refresh principal cast",
            Self::Episodes => "Refresh episode relationships",
            Self::People => "Rebuild People search index",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Request {
    Refresh(Dataset),
    Backup,
}
impl Request {
    fn label(self) -> &'static str {
        match self {
            Self::Refresh(dataset) => dataset.label(),
            Self::Backup => "Create verified backup",
        }
    }
}

pub struct Manager {
    data: PathBuf,
    database: PathBuf,
    executable: PathBuf,
    directory: PathBuf,
    _lock: File,
}
pub struct Task {
    scheduled: bool,
    id: i64,
    request: Request,
}
#[derive(Debug)]
pub enum StartError {
    Busy,
    Database(rusqlite::Error),
}
impl From<rusqlite::Error> for StartError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct Job {
    pub id: i64,
    pub label: String,
    pub state: String,
    pub started_at: String,
    pub finished_at: String,
    pub detail: String,
}
impl Job {
    pub fn state_label(&self) -> &str {
        match self.state.as_str() {
            "running" => "Running",
            "succeeded" => "Completed",
            "failed" => "Failed",
            "interrupted" => "Interrupted",
            _ => "Unknown",
        }
    }
}

pub struct Schedule {
    pub kind: String,
    pub label: String,
    pub days: u32,
    pub next_run: String,
    pub last_result: String,
}

impl Manager {
    pub fn open(data: PathBuf, executable: PathBuf) -> Result<Arc<Self>, Error> {
        let data = fs::canonicalize(data)?;
        let directory = data.join("maintenance");
        fs::create_dir_all(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("runner.lock"))?;
        lock.try_lock().map_err(|error|format!("Another web server is already using this data directory, or maintenance locking is unavailable: {error}"))?;
        let database = data.join("rustymdb.sqlite3");
        let manager = Arc::new(Self {
            data,
            database,
            executable,
            directory,
            _lock: lock,
        });
        manager.connection()?.execute("UPDATE maintenance_jobs SET state='interrupted', finished_at=strftime('%Y-%m-%d %H:%M:%S UTC','now'), detail='The server stopped before this job finished. Review its output before retrying.' WHERE state='running'",[])?;
        Ok(manager)
    }
    fn connection(&self) -> rusqlite::Result<Connection> {
        let connection = Connection::open(&self.database)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(connection)
    }
    pub fn reserve(&self, request: Request) -> Result<Task, StartError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let busy: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM maintenance_jobs WHERE state='running')",
            [],
            |row| row.get(0),
        )?;
        if busy {
            return Err(StartError::Busy);
        }
        transaction.execute(
            "INSERT INTO maintenance_jobs(label,state,detail) VALUES (?1,'running','Queued')",
            [request.label()],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(Task {
            id,
            request,
            scheduled: false,
        })
    }
    pub fn schedules(&self) -> rusqlite::Result<Vec<Schedule>> {
        let connection = self.connection()?;
        let mut statement=connection.prepare("SELECT s.kind,s.days,COALESCE(strftime('%Y-%m-%d %H:%M:%S UTC',s.next_run,'unixepoch'),''),COALESCE(j.state,'never run') FROM maintenance_schedules s LEFT JOIN maintenance_jobs j ON j.id=s.last_job_id ORDER BY s.kind")?;
        statement
            .query_map([], |r| {
                let kind: String = r.get(0)?;
                Ok(Schedule {
                    label: if kind == "backup" {
                        "Verified backups".into()
                    } else {
                        "Refresh all IMDb datasets".into()
                    },
                    kind,
                    days: r.get(1)?,
                    next_run: r.get(2)?,
                    last_result: r.get(3)?,
                })
            })?
            .collect()
    }
    pub fn save_schedules(&self, backup_days: u32, refresh_days: u32) -> rusqlite::Result<()> {
        if ![0, 1, 7, 30].contains(&backup_days) || ![0, 1, 7, 30].contains(&refresh_days) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (kind, days) in [("backup", backup_days), ("refresh", refresh_days)] {
            transaction.execute("UPDATE maintenance_schedules SET days=?2,next_run=CASE WHEN ?2=0 THEN NULL WHEN days=?2 AND next_run IS NOT NULL THEN next_run ELSE unixepoch()+?2*86400 END WHERE kind=?1",rusqlite::params![kind,days])?;
        }
        transaction.commit()
    }
    fn reserve_scheduled(&self, now: i64) -> rusqlite::Result<Option<Task>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM maintenance_jobs WHERE state='running')",
            [],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(None);
        }
        let due=transaction.query_row("SELECT kind FROM maintenance_schedules WHERE days>0 AND next_run<=?1 ORDER BY kind LIMIT 1",[now],|r|r.get::<_,String>(0)).optional()?;
        let Some(kind) = due else { return Ok(None) };
        let request = if kind == "backup" {
            Request::Backup
        } else {
            Request::Refresh(Dataset::All)
        };
        transaction.execute("INSERT INTO maintenance_jobs(label,state,detail) VALUES (?1,'running','Scheduled job queued')",[format!("Scheduled: {}",request.label())])?;
        let id = transaction.last_insert_rowid();
        transaction.execute(
            "UPDATE maintenance_schedules SET next_run=?2+days*86400,last_job_id=?3 WHERE kind=?1",
            rusqlite::params![kind, now, id],
        )?;
        transaction.commit()?;
        Ok(Some(Task {
            id,
            request,
            scheduled: true,
        }))
    }
    pub async fn run_scheduler(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let manager = self.clone();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            match tokio::task::spawn_blocking(move || manager.reserve_scheduled(now)).await {
                Ok(Ok(Some(task))) => self.clone().run(task).await,
                Ok(Ok(None)) => {}
                Ok(Err(error)) => eprintln!("Maintenance schedule database check failed: {error}"),
                Err(error) => eprintln!("Maintenance schedule task failed: {error}"),
            }
        }
    }

    fn update(&self, id: i64, state: &str, detail: &str) -> rusqlite::Result<()> {
        self.connection()?.execute("UPDATE maintenance_jobs SET state=?1, detail=?2, finished_at=CASE WHEN ?1='running' THEN NULL ELSE strftime('%Y-%m-%d %H:%M:%S UTC','now') END WHERE id=?3",rusqlite::params![state,detail,id])?;
        Ok(())
    }
    pub fn latest(&self) -> rusqlite::Result<Option<Job>> {
        self.connection()?.query_row("SELECT id,label,state,started_at,COALESCE(finished_at,''),detail FROM maintenance_jobs ORDER BY id DESC LIMIT 1",[],|r|Ok(Job{id:r.get(0)?,label:r.get(1)?,state:r.get(2)?,started_at:r.get(3)?,finished_at:r.get(4)?,detail:r.get(5)?})).optional()
    }
    fn log_path(&self, id: i64) -> PathBuf {
        self.directory.join(format!("job-{id}.log"))
    }
    pub fn output(&self, id: i64) -> std::io::Result<String> {
        let mut file = match File::open(self.log_path(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(error) => return Err(error),
        };
        let start = file.metadata()?.len().saturating_sub(16384);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(16384).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(format!(
            "{}{}",
            if start > 0 {
                "… showing the latest output …\n"
            } else {
                ""
            },
            text
        ))
    }
    pub fn backup_directory(&self) -> PathBuf {
        self.data.join("backups")
    }
    async fn execute(&self, task: &Task) -> Result<String, Error> {
        let mut log = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(self.log_path(task.id))?;
        let mut backup_file = None;
        let (commands,mut success): (Vec<Vec<std::ffi::OsString>>,String)=match task.request {
            Request::Refresh(dataset)=>(dataset.commands().iter().map(|command|vec![(*command).into()]).collect(),"Selected datasets refreshed successfully. Each dataset was committed separately; saved titles are preserved.".into()),
            Request::Backup=>{
                let directory=self.backup_directory();fs::create_dir_all(&directory)?;
                let seconds=SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                let destination=directory.join(format!("rustymdb-backup-{seconds}-job-{}.sqlite3",task.id));
                backup_file=Some(destination.clone());
                (vec![vec!["backup".into(),destination.clone().into_os_string()]],format!("Verified backup saved on this server: {}",destination.display()))
            }
        };
        for arguments in commands {
            let command = arguments[0].to_string_lossy();
            self.update(task.id, "running", &format!("Running {command}"))?;
            writeln!(log, "Running {command}")?;
            log.flush()?;
            let status = tokio::process::Command::new(&self.executable)
                .args(&arguments)
                .current_dir(&self.data)
                .env("RUSTYMDB_DATA_DIR", &self.data)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log.try_clone()?))
                .kill_on_drop(true)
                .status()
                .await?;
            if !status.success() {
                return Err(format!("{command} exited with {status}. Earlier successful refresh steps remain applied. See the output below.").into());
            }
        }
        if task.scheduled
            && let Some(file) = backup_file
        {
            let database = self.database.clone();
            let data = self.data.clone();
            let id = task.id;
            let result = tokio::task::spawn_blocking(move || {
                crate::retention::complete(&database, &data, id, &file)
            })
            .await;
            match result {
                Ok(Ok(count)) => success.push_str(&format!(
                    " Retention removed {count} older managed scheduled backup(s)."
                )),
                error => {
                    writeln!(log, "Retention cleanup skipped or incomplete: {error:?}")?;
                    success.push_str(" Backup is verified, but retention cleanup was skipped or incomplete. Older unprocessed files were preserved; inspect the job output.");
                }
            }
        }
        Ok(success)
    }
    pub async fn run(self: Arc<Self>, task: Task) {
        let (state, detail) = match self.execute(&task).await {
            Ok(detail) => ("succeeded", detail),
            Err(error) => ("failed", error.to_string()),
        };
        if let Err(error) = self.update(task.id, state, &detail) {
            eprintln!(
                "Unable to record maintenance result for job {}: {error}",
                task.id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_executable(name: &str) -> PathBuf {
        for directory in ["/usr/bin", "/bin"] {
            let candidate = PathBuf::from(directory).join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
        panic!("Required test utility is unavailable: {name}");
    }
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let name = bytes.iter().map(|v| format!("{v:02x}")).collect::<String>();
            let path = std::env::temp_dir().join(format!("rustymdb-maintenance-{name}"));
            fs::create_dir(&path).unwrap();
            crate::database::initialize(&path.join("rustymdb.sqlite3")).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn schedules_start_disabled_preserve_due_times_and_serialize_with_manual_jobs() {
        let directory = Directory::new();
        let manager = Manager::open(directory.0.clone(), fixture_executable("true")).unwrap();
        assert!(
            manager
                .schedules()
                .unwrap()
                .iter()
                .all(|s| s.days == 0 && s.next_run.is_empty())
        );
        assert!(manager.save_schedules(2, 0).is_err());
        manager.save_schedules(1, 7).unwrap();
        let before = manager.schedules().unwrap()[0].next_run.clone();
        manager.save_schedules(1, 7).unwrap();
        assert_eq!(manager.schedules().unwrap()[0].next_run, before);
        assert!(manager.reserve_scheduled(0).unwrap().is_none());
        manager
            .connection()
            .unwrap()
            .execute("UPDATE maintenance_schedules SET next_run=100", [])
            .unwrap();
        let manual = manager.reserve(Request::Backup).unwrap();
        assert!(manager.reserve_scheduled(1000).unwrap().is_none());
        assert_eq!(
            manager
                .connection()
                .unwrap()
                .query_row(
                    "SELECT next_run FROM maintenance_schedules WHERE kind='backup'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            100
        );
        manager.clone().run(manual).await;
        let backup = manager.reserve_scheduled(1000).unwrap().unwrap();
        assert!(matches!(backup.request, Request::Backup));
        assert!(manager.reserve_scheduled(1000).unwrap().is_none());
        manager.clone().run(backup).await;
        let refresh = manager.reserve_scheduled(1000).unwrap().unwrap();
        assert!(matches!(refresh.request, Request::Refresh(Dataset::All)));
        manager.clone().run(refresh).await;
        assert!(manager.reserve_scheduled(1000).unwrap().is_none());
        assert_eq!(
            manager
                .connection()
                .unwrap()
                .query_row(
                    "SELECT next_run FROM maintenance_schedules WHERE kind='backup'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            87400
        );
        assert!(
            manager
                .schedules()
                .unwrap()
                .iter()
                .all(|s| s.last_result == "succeeded")
        );
        manager.save_schedules(0, 0).unwrap();
        assert!(
            manager
                .schedules()
                .unwrap()
                .iter()
                .all(|s| s.next_run.is_empty())
        );
    }
    #[test]
    fn interrupted_scheduled_jobs_do_not_repeat_after_restart() {
        let directory = Directory::new();
        let manager = Manager::open(directory.0.clone(), fixture_executable("true")).unwrap();
        manager.save_schedules(1, 0).unwrap();
        manager
            .connection()
            .unwrap()
            .execute(
                "UPDATE maintenance_schedules SET next_run=100 WHERE kind='backup'",
                [],
            )
            .unwrap();
        assert!(manager.reserve_scheduled(1000).unwrap().is_some());
        drop(manager);
        let manager = Manager::open(directory.0.clone(), fixture_executable("true")).unwrap();
        assert_eq!(manager.schedules().unwrap()[0].last_result, "interrupted");
        assert!(manager.reserve_scheduled(1000).unwrap().is_none());
    }
    #[test]
    fn serializes_jobs_locks_other_servers_and_recovers_interrupted_work() {
        let directory = Directory::new();
        let manager = Manager::open(directory.0.clone(), fixture_executable("true")).unwrap();
        let task = manager.reserve(Request::Refresh(Dataset::Ratings)).unwrap();
        assert!(matches!(
            manager.reserve(Request::Backup),
            Err(StartError::Busy)
        ));
        assert!(Manager::open(directory.0.clone(), fixture_executable("true")).is_err());
        assert_eq!(manager.latest().unwrap().unwrap().id, task.id);
        drop(manager);
        let recovered = Manager::open(directory.0.clone(), fixture_executable("true")).unwrap();
        assert_eq!(recovered.latest().unwrap().unwrap().state, "interrupted");
        assert!(recovered.reserve(Request::Backup).is_ok());
    }
    #[tokio::test]
    async fn records_child_success_failure_and_keeps_log_tail_bounded() {
        let directory = Directory::new();
        let manager = Manager::open(directory.0.clone(), fixture_executable("true")).unwrap();
        let task = manager.reserve(Request::Refresh(Dataset::Ratings)).unwrap();
        let id = task.id;
        manager.clone().run(task).await;
        assert_eq!(manager.latest().unwrap().unwrap().state, "succeeded");
        fs::write(manager.log_path(id), vec![b'x'; 20000]).unwrap();
        assert!(manager.output(id).unwrap().len() < 16500);
        drop(manager);
        let manager = Manager::open(directory.0.clone(), fixture_executable("false")).unwrap();
        let task = manager.reserve(Request::Refresh(Dataset::Ratings)).unwrap();
        manager.clone().run(task).await;
        assert_eq!(manager.latest().unwrap().unwrap().state, "failed");
        assert!(
            manager
                .output(manager.latest().unwrap().unwrap().id)
                .unwrap()
                .contains("import-ratings")
        );
    }
    #[test]
    fn refresh_all_is_a_fixed_ordered_command_list() {
        assert_eq!(
            Dataset::All.commands(),
            [
                "import-ratings",
                "import-titles",
                "import-credits",
                "import-cast",
                "import-episodes"
            ]
        );
        assert_eq!(Dataset::Titles.commands(), ["import-titles"]);
    }
}
