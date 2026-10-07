#!/usr/bin/env bash
#
# install.sh: install or upgrade sitewatch. Idempotent; run as root from an unpacked bundle:
#
#   sudo ./install.sh --config FILE [options]
#
# Options:
#   --config FILE        install this config.toml (from your repo; authoritative). Required on the
#                        first install. "@@EMAIL_TO@@" / "@@EMAIL_FROM@@" are filled in from
#                        /etc/logscan/config.toml [email], so mail goes the same way as logscan's.
#   --topic-infix STR    ntfy topics alert_<STR>services / notify_<STR>tasks / notify_<STR>digest,
#                        e.g. "km_" on kochmonster (default: none → alert_services, …)
#   --heal-site NAME     set up cache clearing for site NAME (as in config.toml [[sites]] name):
#   --heal-dir DIR         the site's checkout (docker compose project), e.g. /var/opt/services/kochmonster/statamic
#   --heal-user USER       its owner, who may run docker compose there (e.g. kochmonster)
#                        Healing still only happens with [heal] enabled = true in config.toml.
#   --no-timers          don't enable the timers (first install: dry runs first, then
#                        systemctl enable --now sitewatch-quick.timer sitewatch-full.timer)
#   --no-first-run       skip the dry run at the end
#   --bin PATH           binary to install (default: ./sitewatch from the bundle)
#
# Never touches an existing config.toml unless --config is given.
#
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "This script must be run as root: sudo $0" >&2
  exit 1
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
ETC=/etc/sitewatch
STATE=/var/lib/sitewatch
UNITS=/etc/systemd/system
SHARE=/usr/local/share/sitewatch
DISPATCH_CFG=/etc/ntfy-dispatcher/config.toml
BIN="$HERE/sitewatch"
CONFIG=""
INFIX=""
HEAL_SITE=""
HEAL_DIR=""
HEAL_USER=""
TIMERS=1
FIRST_RUN=1

while [ $# -gt 0 ]; do
  case "$1" in
    --config) CONFIG="$2"; shift 2 ;;
    --topic-infix) INFIX="$2"; shift 2 ;;
    --heal-site) HEAL_SITE="$2"; shift 2 ;;
    --heal-dir) HEAL_DIR="$2"; shift 2 ;;
    --heal-user) HEAL_USER="$2"; shift 2 ;;
    --no-timers) TIMERS=0; shift ;;
    --no-first-run) FIRST_RUN=0; shift ;;
    --bin) BIN="$2"; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
  esac
done

[ -x "$BIN" ] || { echo "binary not found: $BIN" >&2; exit 1; }
[ -d "$HERE/systemd" ] && [ -f "$HERE/heal.sh" ] || { echo "bundle incomplete" >&2; exit 1; }
[ -n "$CONFIG" ] || [ -f "$ETC/config.toml" ] || { echo "first install needs --config FILE" >&2; exit 1; }
[[ "$INFIX" =~ ^[a-z0-9_]*$ ]] || { echo "--topic-infix must be [a-z0-9_]" >&2; exit 1; }
if [ -n "$HEAL_SITE$HEAL_DIR$HEAL_USER" ]; then
  [ -n "$HEAL_SITE" ] && [ -n "$HEAL_DIR" ] && [ -n "$HEAL_USER" ] || { echo "--heal-site, --heal-dir and --heal-user go together" >&2; exit 1; }
  [[ "$HEAL_SITE" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "--heal-site must be [a-z0-9-]" >&2; exit 1; }
  [ -d "$HEAL_DIR" ] || { echo "--heal-dir $HEAL_DIR does not exist" >&2; exit 1; }
  id "$HEAL_USER" >/dev/null 2>&1 || { echo "--heal-user $HEAL_USER does not exist" >&2; exit 1; }
fi

BACKUPS="/var/backups/sitewatch-$STAMP"
stash() { install -d -m 0700 "$BACKUPS"; cp -a "$1" "$BACKUPS/"; echo "  backup: $BACKUPS/$(basename "$1")"; }
install_file() {  # SRC DST MODE [OWNER:GROUP]
  if [ -e "$2" ] && cmp -s "$1" "$2"; then echo "  unchanged $2"; return; fi
  [ -e "$2" ] && stash "$2"
  install -o "${4%%:*}" -g "${4##*:}" -m "$3" "$1" "$2"; echo "  installed $2"
}

echo "=== sitewatch install on $(hostname) ($("$BIN" --version)) ==="

# --- binary, docs, heal script -------------------------------------------------------------------
install_file "$BIN" /usr/local/bin/sitewatch 0755 root:root
install -d -m 0755 /usr/local/share/doc/sitewatch "$SHARE"
[ -f "$HERE/README.md" ] && install_file "$HERE/README.md" /usr/local/share/doc/sitewatch/README.md 0644 root:root
install_file "$HERE/heal.sh" "$SHARE/heal.sh" 0755 root:root

# --- user + directories --------------------------------------------------------------------------
if ! id sitewatch >/dev/null 2>&1; then
  useradd --system --user-group --home-dir "$STATE" --no-create-home \
          --shell /usr/sbin/nologin --comment "sitewatch website watchdog" sitewatch
  echo "  created system user sitewatch"
fi
EXTRA_GROUPS=()
for g in ntfy-senders msmtp; do getent group "$g" >/dev/null && EXTRA_GROUPS+=("$g"); done
[ ${#EXTRA_GROUPS[@]} -gt 0 ] && usermod -aG "$(IFS=,; echo "${EXTRA_GROUPS[*]}")" sitewatch
echo "  sitewatch groups: $(id -nG sitewatch)"
# adm members may read reports; o+x (no read) lets the heal user reach heal/. Everything inside
# keeps sitewatch's own modes (UMask 0027).
install -d -o sitewatch -g adm -m 2751 "$STATE"
install -d -m 0755 "$ETC"

# --- config ------------------------------------------------------------------------------------------
if [ -n "$CONFIG" ]; then
  CFG="$(mktemp)"
  python3 - "$CONFIG" "$CFG" <<'PY'
import sys, tomllib
src, dst = sys.argv[1:3]
s = open(src).read()
if "@@EMAIL_" in s:
    try:
        ls = tomllib.load(open("/etc/logscan/config.toml", "rb")).get("email", {})
    except FileNotFoundError:
        sys.exit("config uses @@EMAIL_TO@@/@@EMAIL_FROM@@ but /etc/logscan/config.toml is missing")
    to, frm = ls.get("to") or [], ls.get("from") or ""
    if not to or not frm:
        sys.exit("no [email] to/from in /etc/logscan/config.toml")
    s = s.replace('to = ["@@EMAIL_TO@@"]', "to = [" + ", ".join(f'"{t}"' for t in to) + "]")
    s = s.replace('from = "@@EMAIL_FROM@@"', f'from = "{frm}"')
    print(f"  mail as logscan: from {frm} to {', '.join(to)}")
open(dst, "w").write(s)
PY
  install_file "$CFG" "$ETC/config.toml" 0644 root:root
  rm -f "$CFG"
fi
runuser -u sitewatch -- /usr/local/bin/sitewatch --config "$ETC/config.toml" config check | sed 's/^/  /'

# --- ntfy-dispatcher rules -------------------------------------------------------------------------
if [ -f "$DISPATCH_CFG" ] && command -v ntfy-dispatcher >/dev/null; then
  if grep -q 'id = "sitewatch_alert"' "$DISPATCH_CFG"; then
    echo "  dispatcher rules already present (topics: $(grep -A3 'id = "sitewatch_alert"' "$DISPATCH_CFG" | grep -oE 'alert_[a-z_]*services'))"
  else
    stash "$DISPATCH_CFG"
    sed "s/@@INFIX@@/$INFIX/g" "$HERE/dispatcher-rules.toml" >> "$DISPATCH_CFG"
    if ! ntfy-dispatcher config check >/dev/null; then
      echo "dispatcher config check failed; restoring $DISPATCH_CFG" >&2
      cp -a "$BACKUPS/$(basename "$DISPATCH_CFG")" "$DISPATCH_CFG"; exit 1
    fi
    echo "  added dispatcher rules sitewatch_* → alert_${INFIX}services / notify_${INFIX}tasks / notify_${INFIX}digest"
  fi
else
  echo "  WARNING: no ntfy-dispatcher; pushes will fail until it is installed"
fi

# --- units + host drop-ins ---------------------------------------------------------------------------
UNIT_LIST=(sitewatch-quick.service sitewatch-quick.timer sitewatch-full.service sitewatch-full.timer sitewatch-heal@.path sitewatch-heal@.service)
for u in "${UNIT_LIST[@]}"; do
  install_file "$HERE/systemd/$u" "$UNITS/$u" 0644 root:root
done
HANDLER=""
for h in ntfy-job-failed@.service ntfy-unit-failed@.service; do
  if [ -e "$UNITS/$h" ] || [ -e "/usr/lib/systemd/system/$h" ] || [ -e "/lib/systemd/system/$h" ]; then HANDLER="${h%@.service}@%N.service"; break; fi
done
for svc in sitewatch-quick sitewatch-full; do
  install -d -m 0755 "$UNITS/$svc.service.d"
  {
    echo "# Written by sitewatch install.sh for $(hostname) on $STAMP."
    echo "[Unit]"
    [ -n "$HANDLER" ] && echo "OnFailure=$HANDLER"
    if [ ${#EXTRA_GROUPS[@]} -gt 0 ]; then
      echo "[Service]"
      echo "SupplementaryGroups=${EXTRA_GROUPS[*]}"
    fi
  } > "$UNITS/$svc.service.d/10-host.conf"
done

# --- healing (cache clear as the site owner) ---------------------------------------------------------
if [ -n "$HEAL_SITE" ]; then
  # sitewatch writes the request, the site owner's heal unit removes it: shared group, setgid dir.
  HEAL_GROUP="$(id -gn "$HEAL_USER")"
  install -d -o sitewatch -g "$HEAL_GROUP" -m 2770 "$STATE/heal"
  install -d -m 0755 "$ETC/heal"
  ENV="$(mktemp)"
  printf '# Written by sitewatch install.sh on %s.\nSITE_DIR=%s\n' "$STAMP" "$HEAL_DIR" > "$ENV"
  install_file "$ENV" "$ETC/heal/$HEAL_SITE.env" 0644 root:root
  rm -f "$ENV"
  D="$UNITS/sitewatch-heal@$HEAL_SITE.service.d"
  install -d -m 0755 "$D"
  {
    echo "# Written by sitewatch install.sh for $(hostname) on $STAMP."
    echo "[Unit]"
    [ -n "$HANDLER" ] && echo "OnFailure=$HANDLER"
    echo "[Service]"
    echo "User=$HEAL_USER"
    echo "WorkingDirectory=$HEAL_DIR"
  } > "$D/10-host.conf"
  echo "  heal for $HEAL_SITE: as $HEAL_USER in $HEAL_DIR (active only with [heal] enabled = true)"
fi

systemctl daemon-reload
systemd-analyze verify --man=no sitewatch-quick.service sitewatch-full.service 2>&1 | grep -v '^$' || true
[ -n "$HEAL_SITE" ] && systemctl enable --now "sitewatch-heal@$HEAL_SITE.path"

# --- first run + timers ------------------------------------------------------------------------------
if [ "$FIRST_RUN" -eq 1 ]; then
  echo "--- dry run (quick, nothing written or sent) ---"
  runuser -u sitewatch -- /usr/local/bin/sitewatch --config "$ETC/config.toml" run --mode quick --dry-run --print || echo "  dry run failed"
fi
if [ "$TIMERS" -eq 1 ]; then
  systemctl enable --now sitewatch-quick.timer sitewatch-full.timer
  echo "  timers enabled: $(systemctl list-timers 'sitewatch-*' --no-pager --no-legend | awk '{print $NF}' | xargs)"
else
  echo "  timers NOT enabled; when the dry runs look right:"
  echo "    systemctl enable --now sitewatch-quick.timer sitewatch-full.timer"
fi
echo "=== done. Report: $STATE/reports/latest.md; journal: journalctl -u sitewatch-quick -u sitewatch-full"
