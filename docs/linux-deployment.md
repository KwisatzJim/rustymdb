# Example Linux service

The example uses `/srv/rustymdb` and a dedicated `rustymdb` account. Adjust the
paths consistently for your host. Build natively on Linux; a macOS executable
cannot run on Linux. These are initial-install steps, not commands to overwrite
an existing installation.

```sh
cargo build --release --locked
sudo useradd --system --user-group --home-dir /srv/rustymdb --no-create-home --shell /usr/sbin/nologin rustymdb
sudo install -d -m 0755 /srv/rustymdb/bin
sudo install -d -o rustymdb -g rustymdb -m 0750 /srv/rustymdb/data
sudo install -m 0755 target/release/rustymdb /srv/rustymdb/bin/rustymdb
sudo install -o root -g rustymdb -m 0640 deploy/linux/rustymdb.env.example /srv/rustymdb/rustymdb.env
sudo install -m 0644 deploy/linux/rustymdb.service /etc/systemd/system/rustymdb.service
sudo systemd-analyze verify /etc/systemd/system/rustymdb.service
sudo systemctl daemon-reload
sudo systemctl enable --now rustymdb
```

Skip account creation if that account already exists. Review the environment
file before starting; the default binds only to loopback. The service uses
SIGINT for graceful shutdown and can write only to its data directory. Do not
change shared web-server or Tailscale HTTPS routes to deploy this application.

Inspect service health with `sudo systemctl status rustymdb --no-pager` and logs
with `sudo journalctl -u rustymdb -n 50 --no-pager`.

For upgrades: complete the documented checks, create a verified backup, wait for
maintenance to finish, stop the service, preserve the old executable, install the
new native executable and restart. Keep the old executable and database backup
until the browsing/watchlist workflow has been checked. Do not automatically
restore a database just because a binary upgrade fails.
