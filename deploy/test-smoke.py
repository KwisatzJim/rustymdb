#!/usr/bin/env python3
"""Smoke-test a compiled executable with temporary data and loopback-only HTTP."""
import os
import json
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
import urllib.error

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
        c.execute("INSERT INTO dataset_imports(name,imported_at,source_modified,row_count) VALUES ('cast','fixture','fixture',2)")
        c.execute("INSERT INTO imdb_people(person_id,primary_name) VALUES ('nm0000001','Fixture Actor')")
        c.execute("INSERT INTO imdb_titles(title_id,primary_title,original_title,title_type,start_year,is_adult) VALUES ('tt1375666','Synthetic test title','Synthetic test title','movie',2010,0)")
        c.executemany("INSERT INTO imdb_cast VALUES ('tt1375666',?,'nm0000001','actor',?)", [(1, '["First role"]'), (2, '["First role","Second role"]')])
        c.commit()
        page = get('/title/tt1375666')
        assert '<summary><h2 id="cast-heading">Principal cast</h2></summary>' in page
        assert 'class="cast-list principal-cast"' in page
        assert page.count('src="/person-photo/nm0000001"') == 1
        assert 'as First role, Second role' in page
        assert page.count('href="/person/nm0000001"') == 1
        page = get('/person/nm0000001')
        assert 'Refresh the credits dataset' in page
        c.execute("INSERT INTO imdb_person_details VALUES ('nm0000001',1940,2020,'actor,voice_actor','tt1375666,tt9999999')")
        c.commit()
        page = get('/person/nm0000001')
        assert 'IMDb person information' in page
        assert '<dd>1940</dd>' in page and '<dd>2020</dd>' in page
        assert 'actor, voice actor' in page
        assert 'href="/title/tt1375666">Synthetic test title</a>' in page
        assert 'IMDb title tt9999999' in page
        assert 'Refresh the credits dataset' not in page
        supplement = {key: '' for key in ['source_url', 'poster_path', 'overview', 'tagline', 'release_date', 'runtime', 'genres', 'countries', 'languages', 'companies', 'status', 'budget', 'revenue']}
        supplement.update({key: [] for key in ['collections', 'recommendations', 'videos', 'cast', 'crew']})
        supplement['alternative_titles'] = [dict(country='FR', titles=[dict(title='Un autre titre', kind='Working title'), dict(title='<script>unsafe title</script>', kind='')]), dict(country='JP', titles=[dict(title='夢の映画', kind='')])]
        supplement['gallery'] = [dict(kind='posters', label='Posters', images=[dict(path='/synthetic.jpg', language='en')])]
        supplement['keywords'] = ['time travel', '<script>unsafe keyword</script>']
        supplement['watch_regions'] = [dict(country='US', groups=[dict(label='Subscription', providers=['Example service'])]), dict(country='GB', groups=[dict(label='Rent', providers=['UK service'])])]
        supplement['source_url'] = 'https://www.themoviedb.org/movie/42'
        supplement['regional_releases'] = [dict(country='US', entries=[dict(date='2010-07-16', kind='Theatrical', certification='PG-13'), dict(date='2010-10-01', kind='Digital', certification='')])]
        c.execute("INSERT INTO tmdb_cache VALUES ('tt1375666',?,unixepoch()+86400)", (json.dumps(supplement),))
        c.commit()
        assert 'id="regional-release-heading"' not in get('/title/tt1375666')
        assert 'id="keywords-heading"' not in get('/title/tt1375666')
        assert 'id="gallery-heading"' not in get('/title/tt1375666')
        # Load synthetic credentials on restart; cached fixtures avoid provider calls.
        process.send_signal(signal.SIGINT)
        process.wait(timeout=10)
        process.stdout.close()
        credential_file = Path(directory) / 'tmdb-credentials.json'
        credential_file.write_text(json.dumps(dict(kind='token', value='synthetic_test_token_12345')))
        credential_file.chmod(0o600)
        process = subprocess.Popen([str(binary)], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        line = process.stdout.readline()
        match = re.search(r'http://127\.0\.0\.1:(\d+)', line)
        assert match, line
        base = 'http://127.0.0.1:' + match.group(1)
        page = get('/title/tt1375666')
        assert '<summary><h2 id="regional-release-heading">Age ratings and release dates</h2></summary>' in page
        assert '<h3>US</h3>' in page and 'PG-13' in page
        assert 'datetime="2010-07-16"' in page
        assert 'Age rating: Not supplied' in page
        assert 'Example service' in page and 'UK service' not in page
        assert '<summary><h2 id="keywords-heading">Plot keywords</h2></summary>' in page
        assert '<li>time travel</li>' in page
        assert '<script>unsafe keyword</script>' not in page
        assert 'unsafe keyword' in page
        assert '<summary><h2 id="alternative-titles-heading">Alternative titles</h2></summary>' in page
        assert '<h3>FR</h3>' in page and '<bdi>Un autre titre</bdi>' in page
        assert 'Working title' in page and '夢の映画' in page
        assert '<script>unsafe title</script>' not in page and 'unsafe title' in page
        assert '<summary><h2 id="gallery-heading">Image gallery</h2></summary>' in page
        assert 'src="/gallery-image/tt1375666/posters/synthetic.jpg"' in page
        assert 'posters/synthetic.jpg?large=true' in page
        image_bytes = bytes([255,216,255,0])
        for size in ['small', 'large']:
            c.execute("INSERT INTO poster_cache VALUES (?,?, 'image/jpeg',unixepoch()+86400)", ('gallery:posters:' + size + ':/synthetic.jpg', image_bytes))
        c.commit()
        for suffix in ['', '?large=true']:
            with urllib.request.urlopen(base + '/gallery-image/tt1375666/posters/synthetic.jpg' + suffix, timeout=10) as response:
                assert response.read() == image_bytes
                assert response.headers['Content-Type'] == 'image/jpeg'
                assert response.headers['X-Content-Type-Options'] == 'nosniff'
        for path in ['/gallery-image/tt1375666/posters/missing.jpg', '/gallery-image/tt1375666/invalid/synthetic.jpg', '/gallery-image/invalid/posters/synthetic.jpg']:
            try:
                get(path)
                raise AssertionError('Invalid gallery request was accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 404
        # Reordering artwork changes neither the old filename URL nor its bytes.
        supplement['gallery'][0]['images'].insert(0, dict(path='/other.jpg', language='en'))
        c.execute("UPDATE tmdb_cache SET payload=? WHERE imdb_id='tt1375666'", (json.dumps(supplement),))
        c.commit()
        with urllib.request.urlopen(base + '/gallery-image/tt1375666/posters/synthetic.jpg', timeout=10) as response:
            assert response.read() == image_bytes
        assert 'JustWatch' in page
        options = get('/options')
        token = re.search(r'name="token" value="([^"]+)"', options).group(1)
        def country_post(country, supplied_token=token):
            fields = urllib.parse.urlencode(dict(token=supplied_token, country=country)).encode()
            return urllib.request.urlopen(base + '/options/country', fields, timeout=10)
        for country, supplied_token, expected in [('GB', 'bad-token', 403), ('USA', token, 400)]:
            try:
                country_post(country, supplied_token)
                raise AssertionError('Invalid country form was accepted')
            except urllib.error.HTTPError as error:
                assert error.code == expected
        with country_post('gb') as response:
            assert 'value="GB"' in response.read().decode()
        page = get('/title/tt1375666')
        assert 'UK service' in page and 'Example service' not in page
        assert 'watch?locale=GB' in page
        assert c.execute("SELECT value FROM tmdb_preferences WHERE name='watch_country'").fetchone()[0] == 'GB'
        with country_post('CA') as response:
            response.read()
        assert 'No providers are listed' in get('/title/tt1375666')
        supplement['alternative_titles'] = []
        supplement['gallery'] = []
        supplement['keywords'] = []
        supplement['regional_releases'] = []
        c.execute("UPDATE tmdb_cache SET payload=? WHERE imdb_id='tt1375666'", (json.dumps(supplement),))
        c.commit()
        assert 'id="regional-release-heading"' not in get('/title/tt1375666')
        assert 'id="alternative-titles-heading"' not in get('/title/tt1375666')
        assert 'id="keywords-heading"' not in get('/title/tt1375666')
        assert 'id="gallery-heading"' not in get('/title/tt1375666')
        c.close()
        print('Clean-data HTTP smoke test passed, including grouped principal cast.')
    finally:
        process.send_signal(signal.SIGINT)
        process.wait(timeout=10)
        process.stdout.close()
