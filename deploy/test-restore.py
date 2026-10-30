#!/usr/bin/env python3
import hashlib
import importlib.util
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('restore', Path(__file__).with_name('prepare-restore.py'))
restore = importlib.util.module_from_spec(spec)
spec.loader.exec_module(restore)

class RestoreTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.source = self.root / 'backup.sqlite3'
        self.destination = self.root / 'restored'
        c = sqlite3.connect(self.source)
        c.executescript("""CREATE TABLE watchlist(title_id TEXT,notes TEXT);
            INSERT INTO watchlist VALUES ('tt1375666','Keep this note');
            CREATE TABLE imdb_titles(title_id TEXT);CREATE TABLE imdb_cache(cache_key TEXT);
            CREATE TABLE maintenance_schedules(days INTEGER,next_run INTEGER);
            INSERT INTO maintenance_schedules VALUES (7,1);
            CREATE TABLE maintenance_jobs(state TEXT,finished_at TEXT,detail TEXT);
            INSERT INTO maintenance_jobs VALUES ('running',NULL,'Old job');""")
        c.close()
    def tearDown(self):
        self.temporary.cleanup()
    def test_preserves_source_and_notes_but_disables_schedules_and_old_jobs(self):
        before = hashlib.sha256(self.source.read_bytes()).digest()
        restore.prepare(self.source,self.destination)
        self.assertEqual(hashlib.sha256(self.source.read_bytes()).digest(),before)
        c = sqlite3.connect(self.destination / 'rustymdb.sqlite3')
        self.assertEqual(c.execute('SELECT notes FROM watchlist').fetchone(),('Keep this note',))
        self.assertEqual(c.execute('SELECT days,next_run FROM maintenance_schedules').fetchone(),(0,None))
        self.assertEqual(c.execute('SELECT state FROM maintenance_jobs').fetchone(),('interrupted',))
        self.assertEqual(c.execute('PRAGMA quick_check').fetchone(),('ok',))
        c.close()
        self.assertEqual((self.destination/'rustymdb.sqlite3').stat().st_mode & 0o777,0o600)
        self.assertFalse((self.destination/'tmdb-credentials.json').exists())
    def test_never_overwrites_existing_destination(self):
        self.destination.mkdir()
        marker=self.destination/'keep';marker.write_text('original')
        with self.assertRaises(ValueError):restore.prepare(self.source,self.destination)
        self.assertEqual(marker.read_text(),'original')
    def test_rejects_active_wal_without_creating_destination(self):
        Path(str(self.source)+'-wal').write_bytes(b'active')
        with self.assertRaises(ValueError):restore.prepare(self.source,self.destination)
        self.assertFalse(self.destination.exists())
    def test_rejects_non_rustymdb_database(self):
        other=self.root/'other.sqlite3'
        sqlite3.connect(other).close()
        with self.assertRaises(ValueError):restore.prepare(other,self.destination)
        self.assertFalse(self.destination.exists())

if __name__ == '__main__':unittest.main()
