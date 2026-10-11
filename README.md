# sitewatch

Website watchdog: looks at a site the way a first-time visitor does (no browser cache) and reports
problems on state change — through `ntfy-dispatcher` and by mail, like logscan and vulnwatch.

Built after 2026-10-07: a manual `npm run build` on kochmonster renamed the hashed CSS while
statically cached pages still referenced the deleted file. Visitors without a cached copy got
unstyled pages, and nobody noticed until someone reported it.

## Checks

| id | severity | what |
|---|---|---|
| `site.down` | critical | Vite manifest or the fresh reference page unreachable (pages are then not checked) |
| `asset.missing` | high | a same-origin CSS/JS/image the page references is not 200 or has the wrong type |
| `page.status` | high | page (or sitemap) not 200; redirects are reported, not followed |
| `page.incomplete` | high | required selector/text missing, forbidden text (`{{`, error pages), cut off, too little text |
| `cache.stale_build` | medium | page references CSS/JS names that are not in the current manifest |
| `cache.stale_layout` | medium | header menu differs from a fresh render (`fresh_reference`, bypasses the cache) |
| `page.shrunk` | medium | a quick page is far smaller than its usual size |
| `site.slow` | low | page slower than `slow_ms` |

Findings fold into one issue per (site, check), e.g. "cache.stale_build (37 of 1,106 pages)".
Pages of open issues are checked again on every run, so issues found on random pages resolve
reliably. `cache.stale_*` and `asset.missing` of a stale build file are *healable*: a static cache
clear fixes them.

## Runs

- `sitewatch-quick.timer`, every 15 min: `quick_pages`, the pages of open issues, `quick_sample`
  random sitemap URLs (sitemap cached for 24 h), images included (max. 8 per page).
- `sitewatch-full.timer`, 05:00 Europe/Berlin: every sitemap URL at `full_rate`, CSS/JS only
  unless `image_check = "all"`. Sends the digest; mails the report if issues are open.

Pushes: `sitewatch.alert` (new/escalated issue ≥ `notify.min_severity`, heal did not help),
`sitewatch.resolved` (a pushed issue is gone), `sitewatch.heal`, `sitewatch.digest`,
`sitewatch.error` (the run itself failed). Topics via `dist/dispatcher-rules.toml`
(`install.sh --topic-infix km_` → `alert_km_services`, `notify_km_tasks`, `notify_km_digest`).

Report: `/var/lib/sitewatch/reports/latest.md` (and `latest-full.md`), group `adm`.

## Healing

Off by default (`[heal] enabled = false`). When on, and *every* open issue of a site is healable,
sitewatch writes `/var/lib/sitewatch/heal/<site>.request` (at most once per `cooldown_hours`).
`sitewatch-heal@<site>.path` starts `sitewatch-heal@<site>.service`, which runs `heal.sh` as the
site owner (`--heal-user`, in the docker group; sitewatch itself is not): `statamic:static:clear`,
then the warm-up in the background. 15 minutes later, a still-stale site raises
"cache clear did not help".

## Heartbeat

Optional dead man's switch (`[heartbeat] enabled = true`), e.g. a Cronitor heartbeat monitor.
After a run (quick or full) that finished without errors and leaves no open issue above
`max_severity` (default `low`), sitewatch GETs the URL in `url_file`. A failed or missing run, or a
serious open issue, means no ping, and the monitor alerts after its grace period. The URL carries
the account key, so it lives in its own file and never in the config or log output:

```bash
printf '%s\n' 'https://cronitor.link/p/<key>/<monitor>' |
  sudo install -m 0640 -o root -g sitewatch /dev/stdin /etc/sitewatch/heartbeat-url
```

Expected interval: every 15 min, except around the nightly full run (it holds the lock while
it walks the whole sitemap). Set the grace period to cover that run.

## Usage

```bash
sitewatch config check
sitewatch check https://www.kochmonster.de/rezepte/casanova-sandwich      # one page, all findings
sitewatch run --mode quick --dry-run --print --state-dir /tmp/sw          # nothing written or sent
cat /var/lib/sitewatch/reports/latest.md
```

## Build + install

```bash
bash dist/package.sh                       # tests, musl build, target/bundle/sitewatch-<v>-x86_64-linux.tar.gz
# on the host, from the unpacked bundle:
sudo ./install.sh --config config.toml --topic-infix km_ \
     --heal-site kochmonster --heal-dir /var/opt/services/kochmonster/statamic --heal-user kochmonster \
     --no-timers
sudo systemctl enable --now sitewatch-quick.timer sitewatch-full.timer
```

Requests carry `User-Agent: sitewatch/<version>`. With `connect_to = "127.0.0.1"` the host's own
nginx is reached over loopback (same vhost and cache as visitors, no hairpin NAT, and CrowdSec's
default whitelist covers 127.0.0.1, so the nightly 7,500-page sweep can't get the host banned).
