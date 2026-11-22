#!/usr/bin/env bash
# llmux2-prune.service: regenerate the gateway config without swept dead
# routes and deploy it through the normal slot-swap path.
#
# The site's importer arguments live in ~/llmux2/import.env as a bash array —
# the same supplemental flags the original import used (e.g.
# --omniroute-fallback, --nous-state, --kilo-free-models), so a prune
# regenerates the same catalog, not a smaller one:
#   IMPORT_FLAGS=(--omniroute-fallback http://127.0.0.1:8410/v1 ...)
#
# ~/llmux2/site.toml carries the site's overlay (extra [server]/[routing]
# settings and site-added credentials/models the export does not carry);
# without it a prune would silently drop them. Create an empty file when
# the site has no overlay.
set -euo pipefail

dir="$HOME/llmux2"
# The export carries plaintext credentials: shred it on every exit —
# success, a failed or aborted import, or an early bail — so it never
# outlives the run that consumed it.
trap 'if command -v shred >/dev/null; then shred -u "$dir/export.json" 2>/dev/null || rm -f "$dir/export.json"; else rm -f "$dir/export.json"; fi' EXIT
IMPORT_FLAGS=()
# shellcheck source=/dev/null
[[ -f "$dir/import.env" ]] && source "$dir/import.env"

[[ -f "$dir/dead-models.json" ]] || { echo "llmux2-prune: no sweep output yet; nothing to do"; exit 0; }

[[ -f "$dir/site.toml" ]] || { echo "llmux2-prune: no site overlay at $dir/site.toml" >&2; exit 1; }

# The deploy gate rejects a config whose listen differs from the installed
# one; read the deployed address back from active.json instead of hardcoding.
listen="$(grep -o '"listen": *"[^"]*"' "$dir/active.json" | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
[[ -n "$listen" ]] || { echo "llmux2-prune: no deployed listen in $dir/active.json" >&2; exit 1; }

# Only the candidate config is removed: deploy atomically copies it to the
# idle slot, and the shared secrets directory stays live — the importer
# reuses identical secret files and aborts on any that differ.
rm -f "$dir/llmux-next.toml"
"$dir/bin/llmux-import" \
    --input "$dir/export.json" \
    --output "$dir/llmux-next.toml" \
    --secrets-dir "$dir/secrets" \
    --exclude-models "$dir/dead-models.json" \
    --listen "$listen" \
    --overlay "$dir/site.toml" \
    "${IMPORT_FLAGS[@]}"
# A short canary window keeps the old slot sharing the port while the new
# slot proves stable; a bad regenerated config never takes the port alone.
"$dir/bin/llmux2-deploy" deploy --binary "$dir/bin/llmux-server" --config "$dir/llmux-next.toml" --canary 300
