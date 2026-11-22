#!/usr/bin/env bash
# llmux2-prune.service: regenerate the gateway config without swept dead
# routes and deploy it through the normal slot-swap path.
#
# The site's importer arguments live in ~/llmux2/import.env as a bash array —
# the same supplemental flags the original import used (e.g.
# --omniroute-fallback, --nous-state, --kilo-free-models), so a prune
# regenerates the same catalog, not a smaller one:
#   IMPORT_FLAGS=(--omniroute-fallback http://127.0.0.1:8410/v1 ...)
set -euo pipefail

dir="$HOME/llmux2"
IMPORT_FLAGS=()
# shellcheck source=/dev/null
[[ -f "$dir/import.env" ]] && source "$dir/import.env"

# The deploy gate rejects a config whose listen differs from the installed
# one; read the deployed address back from active.json instead of hardcoding.
listen="$(grep -o '"listen": *"[^"]*"' "$dir/active.json" | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
[[ -n "$listen" ]] || { echo "llmux2-prune: no deployed listen in $dir/active.json" >&2; exit 1; }

[[ -f "$dir/dead-models.json" ]] || { echo "llmux2-prune: no sweep output yet; nothing to do"; exit 0; }

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
    "${IMPORT_FLAGS[@]}"
# A short canary window keeps the old slot sharing the port while the new
# slot proves stable; a bad regenerated config never takes the port alone.
"$dir/bin/llmux2-deploy" deploy --config "$dir/llmux-next.toml" --canary 300
