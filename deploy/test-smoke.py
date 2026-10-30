#!/usr/bin/env python3
"""Smoke-test a compiled executable with temporary data and loopback-only HTTP."""
import os
from pathlib import Path
import queue
import re
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import urllib.request

binary = Path(sys.argv[1]).resolve(strict=True)
with tempfile.TemporaryDirectory(prefix='rustymdb-smoke-') as directory:
    env = dict(os.environ, RUSTYMDB_DATA_DIR=directory, RUSTYMDB_BIND='127.0.0.1:0')
    process = subprocess.Popen([str(binary)], env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
    try:
        lines = queue.Queue()
        threading.Thread(target=lambda: lines.put(process.stdout.readline()), daemon=True).start()
        line = lines.get(timeout=20)
        match = re.search(r'http://127\.0\.0\.1:(\d+)', line)
        if not match:
            raise RuntimeError('Server did not announce its bound loopback port')
        base = 'http://127.0.0.1:' + match.group(1)
        def get(path):
            with urllib.request.urlopen(base + path, timeout=10) as response:
                return response.read().decode()
        assert 'People' in get('/')
        assert 'Status: Not configured' in get('/options')
        assert 'Save backup retention' in get('/options')
        assert 'Create backup' in get('/maintenance')
        assert 'Your watchlist is empty' in get('/watchlist')
        assert 'season-section' in get('/static/seasons.js')
        c = sqlite3.connect(Path(directory) / 'rustymdb.sqlite3')
        c.execute("INSERT INTO watchlist(title_id,title_name,description) VALUES ('tt1375666','Synthetic test title','Fixture')")
        c.commit()
        page = get('/watchlist')
        token = re.search(r'name="token" value="([^"]+)"', page).group(1)
        fields = urllib.parse.urlencode(dict(token=token, watched='yes', notes='Synthetic note', filter='all')).encode()
        with urllib.request.urlopen(base + '/watchlist/tt1375666/details', fields, timeout=10) as response:
            assert 'Synthetic note' in response.read().decode()
        assert c.execute("SELECT watched,notes FROM watchlist_details WHERE title_id='tt1375666'").fetchone() == (1, 'Synthetic note')
        c.close()
        print('Clean-data HTTP smoke test passed.')
    finally:
        process.send_signal(signal.SIGINT)
        process.wait(timeout=10)
        process.stdout.close()
