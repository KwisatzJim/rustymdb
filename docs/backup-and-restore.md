# Backup, retention and restore

## Backups

Use Maintenance → Create backup, or:

```sh
RUSTYMDB_DATA_DIR=/path/to/data ./target/release/rustymdb backup /new/path/backup.sqlite3
```

The destination must not exist. The command uses SQLite's backup API, verifies
the copy and publishes one standalone database file. It includes the watchlist,
notes, statuses, provider caches, maintenance records and schedules. It excludes
TMDB credentials, job log files and previous backup files. Keep separate copies
outside the server's storage for protection against disk loss.

Options provides disabled-by-default schedules. Retention defaults to Keep all;
limits of 3, 5, 10 or 20 apply only to newly recorded scheduled backups after a
new verified scheduled backup succeeds. Manual, pre-upgrade, untracked and
changed backups are preserved. Choosing a limit authorizes permanent deletion
of eligible older managed backups. Saving a limit does not delete immediately.

## Prepare a restore separately

Use a verified backup, not a live database with active WAL data. Choose a fresh
folder owned by the account that will run the application:

```sh
python3 deploy/prepare-restore.py /path/to/backup.sqlite3 /path/to/new-restored-data
RUSTYMDB_DATA_DIR=/path/to/new-restored-data RUSTYMDB_BIND=127.0.0.1:3001 ./target/release/rustymdb
```

The helper verifies the source and copy, preserves the source, refuses existing
destinations, disables schedules in the restored copy and marks old running jobs
interrupted. It does not copy credentials. If preparation fails, preserve the
partial folder for inspection and use a fresh destination when retrying.

Preview at `http://127.0.0.1:3001`, checking saved titles, notes, statuses, search
and a title page. Stop that preview with Ctrl-C when finished.

Only after inspection should you stop production, preserve its entire data
folder under a new name, and put the prepared directory at the configured data
path with the correct service-account ownership. Keep SQLite files together;
never overlay one database onto another or mix old WAL/SHM files. Restart and
check the site. If startup fails, keep the prepared folder separately and put
the original directory back. Never delete the original as a troubleshooting step.

Configure TMDB again through Options and explicitly re-enable wanted schedules.
For a systemd service, the data path must agree with the unit's write permissions.
