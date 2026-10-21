# projectionlab-sync-server

Automatically syncs account balances from [SimpleFIN Bridge](https://www.simplefin.org/) into
[ProjectionLab](https://projectionlab.com) on a cron schedule.

ProjectionLab has no server-side API for updating balances, only a client-side Plugin API inside a
logged-in browser tab. This tool drives headless Chrome (via
[chromiumoxide](https://github.com/mattsse/chromiumoxide)) to log in, handle OTP, and call
`updateAccount` for each mapped account. Each sync cycle launches a fresh browser and closes it when
done; the login session is persisted to a profile directory so it's reused across runs.

## Quick start (Docker Compose)

```sh
cp .env.example .env   # fill in credentials and account mappings
docker compose up -d
```

> **Note:** Compose interpolates `$` in `.env` values. Escape a literal `$` in a password as `$$`,
> or it will be silently truncated.

## Configuration

Configure with either `config.toml` (copy from `config.example.toml`; path overridable via
`SYNC_CONFIG`) or environment variables. Setting `SIMPLEFIN_ACCESS_URL` switches to env mode.

| Env var | `config.toml` key | Required |
|---|---|---|
| `SIMPLEFIN_ACCESS_URL` | `simplefin.access_url` | yes |
| `PROJECTIONLAB_EMAIL` | `projectionlab.email` | yes |
| `PROJECTIONLAB_PASSWORD` | `projectionlab.password` | yes |
| `PROJECTIONLAB_API_KEY` | `projectionlab.api_key` (Settings → Plugins) | yes |
| `PROJECTIONLAB_TOTP_SECRET` | `projectionlab.totp_secret` (base32, only if 2FA is on) | no |
| `PROJECTIONLAB_PROFILE_DIR` | `projectionlab.profile_dir` | no (default `./.pl-profile`) |
| `SCHEDULE_CRON` | `schedule.cron` | no (default `0 0 * * *`) |
| `MAPPINGS` | `[[mapping]]` entries | no |
| `TZ` | n/a | no (default `UTC` in Docker) |

`MAPPINGS` is a comma-separated list of `simplefin_id:projectionlab_id` pairs, e.g.
`ACT-abc:pl-123,ACT-def:pl-456`.

The cron expression is evaluated in local time (`TZ`). SimpleFIN's free tier allows **24
requests/day**, so don't sync more than hourly.

`config.toml` and the profile directory contain secrets and are gitignored; never commit them.

## Commands

| Command | Description |
|---|---|
| `projectionlab-sync-server` | Run continuously, syncing on the configured schedule |
| `projectionlab-sync-server run-once` | Run a single sync and exit (for cron, systemd timers, k8s CronJobs) |
| `projectionlab-sync-server list-accounts` | Print SimpleFIN and ProjectionLab account IDs to build mappings |
| `projectionlab-sync-server claim <setup-token>` | Exchange a SimpleFIN setup token for an access URL (tokens are single-use) |

## Running locally

Requires Rust and a local Chrome/Chromium install (set `CHROME_EXECUTABLE` to override the binary).

```sh
cargo run -- claim <base64-setup-token>   # once, if you only have a setup token
cargo run -- list-accounts                # find IDs for your mappings
cargo run                                 # start the scheduler
```

## Deployment notes

- **Run with an init process** (`docker run --init`, or `init: true` in Compose) so Chromium's child
  processes are reaped.
- **Persist the profile directory** (`/app/.pl-profile`) on a volume, otherwise every restart redoes
  the full login/OTP flow.
- **Run a single instance.** Two processes sharing one profile directory will collide on Chrome's
  profile lock. On Kubernetes use `replicas: 1` with the `Recreate` strategy.
- **File ownership:** the image runs as uid/gid `1000`. For a bind-mounted profile directory on
  Linux, `chown -R 1000:1000 .pl-profile` or build with
  `--build-arg UID=$(id -u) --build-arg GID=$(id -g)`. On Kubernetes set `fsGroup: 1000`.

Example `docker run`:

```sh
docker build -t projectionlab-sync-server .
docker run -d --init \
  -v "$(pwd)/config.toml:/app/config.toml" \
  -v "$(pwd)/.pl-profile:/app/.pl-profile" \
  projectionlab-sync-server
```

## Limitations

ProjectionLab's login markup isn't a stable public interface. If it changes, the selectors in
`src/projectionlab.rs` may need updating.

## License

[MIT](LICENSE)
