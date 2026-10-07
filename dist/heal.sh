#!/usr/bin/env bash
# sitewatch-heal@<site>.service: clear + re-warm the static cache of a Statamic site, the same
# steps as km_stat's scripts/rebuild-frontend.sh. Runs as the site owner (docker group).
# Env (from /etc/sitewatch/heal/<site>.env): SITE_DIR, optional CLEAR_CMD / WARM_CMD.
set -euo pipefail
site="$1"
request="/var/lib/sitewatch/heal/$site.request"
# Remove first: the path unit re-triggers as long as the file exists.
echo "request: $(tr '\n' ' ' < "$request" 2>/dev/null || true)"
rm -f "$request"
cd "${SITE_DIR:?SITE_DIR missing in /etc/sitewatch/heal/$site.env}"
if [ -n "${CLEAR_CMD:-}" ]; then
    bash -c "$CLEAR_CMD" </dev/null
else
    docker compose exec -T app php artisan statamic:static:clear </dev/null
fi
if [ -n "${WARM_CMD:-}" ]; then
    bash -c "$WARM_CMD" </dev/null
else
    docker compose exec -d app sh -c 'php -d memory_limit=512M artisan statamic:static:warm > storage/logs/static-warm.log 2>&1' </dev/null
fi
echo "static cache cleared, warm-up started for $site"
