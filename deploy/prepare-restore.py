#!/usr/bin/env python3
"""Prepare a new restore directory without replacing any existing data."""
import os
from pathlib import Path
import sqlite3
import sys


def prepare(source, destination):
    source = Path(source).resolve(strict=True)
    destination = Path(destination).absolute()
    if destination.exists():
        raise ValueError("Destination already exists; choose a new empty path.")
    if not source.is_file():
        raise ValueError("Source must be a verified SQLite backup file.")
    wal = Path(str(source) + '-wal')
    if wal.exists() and wal.stat().st_size:
        raise ValueError("Source has active WAL data. Use the application's backup command first.")
    original = sqlite3.connect(source.as_uri() + '?mode=ro', uri=True)
    try:
        if original.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
            raise ValueError("Source backup failed SQLite quick_check.")
        tables = {row[0] for row in original.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not {'watchlist', 'imdb_titles', 'imdb_cache'} <= tables:
            raise ValueError("Source does not contain the expected RustyMDb tables.")
        destination.mkdir(mode=0o750)
        target_path = destination / 'rustymdb.sqlite3'
        target = sqlite3.connect(target_path)
        try:
            original.backup(target)
            if 'maintenance_schedules' in tables:
                target.execute('UPDATE maintenance_schedules SET days=0,next_run=NULL')
            if 'maintenance_jobs' in tables:
                target.execute("UPDATE maintenance_jobs SET state='interrupted',finished_at=strftime('%Y-%m-%d %H:%M:%S UTC','now'),detail='Prepared from a backup; previous running job was not resumed.' WHERE state='running'")
            target.commit()
            target.execute('PRAGMA journal_mode=DELETE')
            if target.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
                raise ValueError("Prepared copy failed SQLite quick_check.")
        finally:
            target.close()
        target_path.chmod(0o600)
    finally:
        original.close()
    print('Prepared restore directory: ' + str(destination))
    print('Original backup and live data were not changed. Schedules are disabled in this copy.')
    print('TMDB credentials were not copied; configure them through Options after restoring.')


if __name__ == '__main__':
    if len(sys.argv) != 3:
        raise SystemExit('Usage: prepare-restore.py BACKUP.sqlite3 NEW_DIRECTORY')
    os.umask(0o077)
    try:
        prepare(sys.argv[1], sys.argv[2])
    except (OSError, sqlite3.Error, ValueError) as error:
        raise SystemExit('Restore preparation failed: ' + str(error))
