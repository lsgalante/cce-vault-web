#!/usr/bin/env bash
# deploy.sh — build the site and publish it to Cloudflare Pages (project
# cce-vault-web, https://cce-vault-web.pages.dev). Needs `wrangler login`
# done once, and web/config.json holding the real Dropbox app key with no
# "endpoints" (those point at the mock; never ship them).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cfg="$here/config.json"
[[ -f $cfg ]] || { echo "deploy.sh: no web/config.json (copy config.example.json, set app_key)" >&2; exit 2; }
python3 - "$cfg" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
if "endpoints" in c:
    sys.exit("deploy.sh: config.json has \"endpoints\" (the mock's): refusing to deploy it")
if not c.get("app_key") or c["app_key"] in ("mock", "YOUR_DROPBOX_APP_KEY"):
    sys.exit("deploy.sh: config.json has no real app_key")
PY
"$here/build.sh"
wrangler=${WRANGLER:-npx wrangler}
$wrangler pages deploy "$(dirname "$here")/site" --project-name cce-vault-web --branch main --commit-dirty=true
