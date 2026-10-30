#!/usr/bin/env python3
"""Read-only checks for a running deployment. Never print page bodies or secrets."""
import sys
import time
import urllib.request

base = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:3000"
checks = {"/": ["/options", "/maintenance"],
          "/options": ["Automatic maintenance", "Save schedules", "API Read Access Token"],
          "/maintenance": ["Create backup", "Refresh datasets"],
          "/watchlist": ["Apply filter", "Watched status and notes are shared too."]}
for attempt in range(20):
    try:
        for path, expected in checks.items():
            with urllib.request.urlopen(base + path, timeout=3) as response:
                page = response.read().decode()
            if not all(text in page for text in expected):
                raise ValueError("Unexpected page content")
        print("Live checks passed: navigation, Options, Maintenance, and Watchlist.")
        break
    except Exception:
        if attempt == 19:
            raise SystemExit("Live HTTP checks failed. Inspect the service journal.")
        time.sleep(.5)
