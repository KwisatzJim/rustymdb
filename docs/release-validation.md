# Initial source release validation

Validated on 2026-10-30 with Rust/Cargo 1.99.1.

| Check | Linux x86_64 | macOS Apple Silicon |
| --- | --- | --- |
| Formatting | Passed | Passed |
| Application tests | 50 passed | 50 passed |
| Clippy, all targets, warnings denied | Passed | Passed |
| Native release build | Passed | Passed |
| Restore helper tests | 4 passed | 4 passed |
| Clean-data HTTP smoke test | Passed | Passed |

The Mac test run exposed Linux-only test utility paths. The test fixtures now
locate `true` and `false` under `/usr/bin` or `/bin`, and both platform runs pass.
No application runtime behavior was changed by that test-only correction.

## Source-distribution checks

- Application source license and Cargo metadata identify MIT.
- Dependency license expressions are recorded from the locked dependency graph.
- The bundled TMDB attribution logo matches the approved upstream asset unchanged.
- Tracked source excludes runtime databases, backups, logs, credentials and host-specific deployment files.
- Text files were checked for the private deployment's host/path markers and common private-key/token patterns.
- README screenshots use the application home page and synthetic season data.
- The source archive is generated from the Git index, so ignored build/test files are excluded.

## GitHub status

The CI workflow is prepared for Linux and macOS with read-only repository
permissions and pinned action revisions. GitHub-hosted CI has not run yet;
that requires publishing the prepared repository. Local validation does not
substitute for checking the first remote CI run after publication.
